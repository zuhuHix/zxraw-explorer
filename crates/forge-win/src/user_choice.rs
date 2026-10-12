//! Per-user Windows file associations using the legacy `UserChoice` hash.
//!
//! Windows does not document or support third-party writes to `UserChoice`. This uses the
//! hash format currently observed on Windows 11 25H2 and is deliberately guarded against
//! `UserChoiceLatest`; an OS change that prevents a write is reported for that extension.

use md5::{Digest, Md5};

use crate::{FileType, Result, WinError};

const PROG_ID_PREFIX: &str = "ZXrawExplorer.";
const BACKUP_KEY: &str = "Software\\ZXrawExplorerDefaultState\\FileTypes";
const USER_CHOICE_ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts";
const EXPERIENCE_SALT: &str =
    "User Choice set via Windows User Experience {D18B6DD5-6124-4341-9318-804003BAFA0B}";
const TICKS_PER_MINUTE: u64 = 60 * 10_000_000;

/// Result for one offered extension after attempting to set or restore its default app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTypeDefaultResult {
    /// The extension without the leading dot.
    pub extension: String,
    /// Whether ZXraw or the previously saved app is now the active choice.
    pub succeeded: bool,
    /// Empty on success; otherwise a per-extension reason suitable for diagnostics.
    pub message: Option<String>,
}

/// Per-extension results from a one-click set or restore operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileTypeDefaultReport {
    pub results: Vec<FileTypeDefaultResult>,
}

impl FileTypeDefaultReport {
    pub fn succeeded(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.succeeded)
            .count()
    }

    pub fn failed(&self) -> impl Iterator<Item = &FileTypeDefaultResult> {
        self.results.iter().filter(|result| !result.succeeded)
    }

    pub fn all_succeeded(&self) -> bool {
        self.failed().next().is_none()
    }

    pub fn failure_summary(&self) -> String {
        self.failed()
            .map(|result| {
                format!(
                    ".{}: {}",
                    result.extension,
                    result
                        .message
                        .as_deref()
                        .unwrap_or("association was not changed")
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Sets the per-user `UserChoice` for each registered photo type and records its prior app.
pub fn set_file_type_defaults(types: &[FileType]) -> Result<FileTypeDefaultReport> {
    #[cfg(windows)]
    {
        let report = set_defaults(types);
        super::file_types::notify_association_change();
        Ok(report)
    }
    #[cfg(not(windows))]
    {
        let _ = types;
        Err(WinError::Unsupported)
    }
}

/// Restores every saved `UserChoice` snapshot. A choice changed to another app since setup is
/// left alone; only choices that still point to ZXraw are replaced.
pub fn restore_file_type_defaults() -> Result<FileTypeDefaultReport> {
    #[cfg(windows)]
    {
        let report = restore_saved_defaults()?;
        super::file_types::notify_association_change();
        Ok(report)
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

#[cfg(windows)]
fn set_defaults(types: &[FileType]) -> FileTypeDefaultReport {
    let sid = match current_user_sid() {
        Ok(sid) => sid,
        Err(error) => {
            return FileTypeDefaultReport {
                results: types
                    .iter()
                    .map(|file_type| failed(&file_type.extension, error.to_string()))
                    .collect(),
            };
        }
    };

    let mut report = FileTypeDefaultReport::default();
    for file_type in types {
        let extension = file_type.extension.to_ascii_lowercase();
        let result = set_default_one(&extension, &sid);
        report.results.push(match result {
            Ok(()) => FileTypeDefaultResult {
                extension,
                succeeded: true,
                message: None,
            },
            Err(error) => failed(&extension, error.to_string()),
        });
    }
    report
}

#[cfg(windows)]
fn set_default_one(extension: &str, sid: &str) -> Result<()> {
    let extension = checked_extension(extension)?;
    ensure_legacy_choice_supported(&extension, sid)?;
    capture_previous_choice(&extension)?;
    set_user_choice(&extension, &format!("{PROG_ID_PREFIX}{extension}"), sid)
}

#[cfg(windows)]
pub(crate) fn restore_saved_defaults() -> Result<FileTypeDefaultReport> {
    use super::integration::registry;

    let sid = current_user_sid()?;
    let mut report = FileTypeDefaultReport::default();
    let mut names = registry::value_names(BACKUP_KEY)?;
    names.sort_unstable();
    for name in names {
        let Some(extension) = name.strip_prefix('.') else {
            continue;
        };
        let extension = match checked_extension(extension) {
            Ok(extension) => extension,
            Err(error) => {
                report.results.push(failed(extension, error.to_string()));
                continue;
            }
        };
        let snapshot = match registry::string_value(BACKUP_KEY, Some(&name))? {
            Some(snapshot) => snapshot,
            None => continue,
        };
        match restore_one(&extension, &snapshot, &sid) {
            Ok(()) => {
                registry::delete_value(BACKUP_KEY, Some(&name))?;
                report.results.push(FileTypeDefaultResult {
                    extension,
                    succeeded: true,
                    message: None,
                });
            }
            Err(error) => report.results.push(failed(&extension, error.to_string())),
        }
    }
    if report.all_succeeded()
        && registry::value_names(BACKUP_KEY)?.is_empty()
        && registry::subkey_names(BACKUP_KEY)?.is_empty()
    {
        registry::delete_key(BACKUP_KEY)?;
    }
    Ok(report)
}

#[cfg(windows)]
fn restore_one(extension: &str, snapshot: &str, sid: &str) -> Result<()> {
    use super::integration::registry;

    let previous = if snapshot == "N:" {
        None
    } else if let Some(previous) = snapshot.strip_prefix("P:") {
        Some(previous)
    } else {
        return Err(WinError::Invalid(
            "saved previous-app entry is malformed".into(),
        ));
    };

    let current = registry::string_value(&user_choice_key(extension), Some("ProgId"))?;
    if !current.as_deref().is_some_and(is_zxraw_prog_id) {
        // The user chose another app after ZXraw was enabled; keep that more recent choice.
        return Ok(());
    }

    ensure_legacy_choice_supported(extension, sid)?;
    match previous {
        Some(previous) if !previous.is_empty() => set_user_choice(extension, previous, sid),
        _ => registry::delete_key(&user_choice_key(extension)),
    }
}

#[cfg(windows)]
fn capture_previous_choice(extension: &str) -> Result<()> {
    use super::integration::registry;

    let dotted = format!(".{extension}");
    if registry::string_value(BACKUP_KEY, Some(&dotted))?.is_some() {
        return Ok(());
    }
    let previous = registry::string_value(&user_choice_key(extension), Some("ProgId"))?;
    let snapshot = previous_choice_snapshot(previous.as_deref());
    registry::set_string(BACKUP_KEY, Some(&dotted), &snapshot)
}

#[cfg(windows)]
fn previous_choice_snapshot(previous: Option<&str>) -> String {
    previous
        .filter(|value| !value.is_empty() && !is_zxraw_prog_id(value))
        .map(|value| format!("P:{value}"))
        .unwrap_or_else(|| "N:".into())
}

#[cfg(windows)]
fn set_user_choice(extension: &str, prog_id: &str, sid: &str) -> Result<()> {
    use super::integration::registry;

    let key = user_choice_key(extension);
    let dotted_extension = format!(".{extension}");
    // A key's ACL denies the current user SetValue. Delete and recreate it so the new key
    // receives the normal inherited permissions, then derive the hash from its real timestamp.
    for _attempt in 0..3 {
        registry::delete_key(&key)?;
        if let Err(error) = registry::set_string(&key, Some("ProgId"), prog_id) {
            let _ = registry::delete_key(&key);
            return Err(error);
        }
        let Some(first_write) = registry::last_write_filetime(&key)? else {
            return Err(WinError::Invalid(
                "Windows did not create the UserChoice key".into(),
            ));
        };
        let hash = generate_hash(&dotted_extension, sid, prog_id, first_write);
        if let Err(error) = registry::set_string(&key, Some("Hash"), &hash) {
            let _ = registry::delete_key(&key);
            return Err(error);
        }
        let Some(final_write) = registry::last_write_filetime(&key)? else {
            let _ = registry::delete_key(&key);
            return Err(WinError::Invalid(
                "Windows removed the UserChoice key during the write".into(),
            ));
        };
        if first_write / TICKS_PER_MINUTE == final_write / TICKS_PER_MINUTE
            && generate_hash(&dotted_extension, sid, prog_id, final_write) == hash
        {
            return Ok(());
        }
    }

    let _ = registry::delete_key(&key);
    Err(WinError::Invalid(
        "the UserChoice key crossed a minute boundary while writing; try again".into(),
    ))
}

#[cfg(windows)]
fn ensure_legacy_choice_supported(extension: &str, sid: &str) -> Result<()> {
    use super::integration::registry;

    let latest = format!("{}\\.{}\\UserChoiceLatest", USER_CHOICE_ROOT, extension);
    if registry::key_exists(&latest)? {
        return Err(WinError::Invalid(
            "Windows is using UserChoiceLatest for this type; the current hash format is not supported".into(),
        ));
    }
    let hash_version = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\SystemProtectedUserData\\{sid}\\AnyoneRead\\AppDefaults"
    );
    if registry::machine_dword_value(&hash_version, "HashVersion")?.unwrap_or(0) != 0 {
        return Err(WinError::Invalid(
            "Windows has enabled a newer UserChoice hash version; the current hash format is not supported".into(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn current_user_sid() -> Result<String> {
    use windows::Win32::Foundation::{CloseHandle, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, PSID, TOKEN_QUERY, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PWSTR;

    let mut token = windows::Win32::Foundation::HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| crate::api_err("OpenProcessToken", error))?;
    let result = (|| {
        let mut length = 0_u32;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut length) };
        if length == 0 {
            return Err(WinError::Invalid(
                "Windows did not return the current user's SID".into(),
            ));
        }
        let mut buffer = vec![0_u8; length as usize];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                length,
                &mut length,
            )
        }
        .map_err(|error| crate::api_err("GetTokenInformation", error))?;

        #[repr(C)]
        struct SidAndAttributes {
            sid: *mut std::ffi::c_void,
            attributes: u32,
        }
        let user = unsafe { &*(buffer.as_ptr().cast::<SidAndAttributes>()) };
        let mut sid = PWSTR::null();
        unsafe { ConvertSidToStringSidW(PSID(user.sid), &mut sid) }
            .map_err(|error| crate::api_err("ConvertSidToStringSidW", error))?;
        let mut sid_len = 0;
        unsafe {
            while *sid.0.add(sid_len) != 0 {
                sid_len += 1;
            }
        }
        let value = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid.0, sid_len)) };
        let _ = unsafe { LocalFree(Some(windows::Win32::Foundation::HLOCAL(sid.0.cast()))) };
        Ok(value)
    })();
    let _ = unsafe { CloseHandle(token) };
    result
}

#[cfg(windows)]
fn user_choice_key(extension: &str) -> String {
    format!("{USER_CHOICE_ROOT}\\.{extension}\\UserChoice")
}

#[cfg(windows)]
fn checked_extension(extension: &str) -> Result<String> {
    if (1..=16).contains(&extension.len())
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        Ok(extension.to_ascii_lowercase())
    } else {
        Err(WinError::Invalid(format!(
            "not a file extension: {extension:?}"
        )))
    }
}

#[cfg(windows)]
fn is_zxraw_prog_id(prog_id: &str) -> bool {
    prog_id
        .get(..PROG_ID_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PROG_ID_PREFIX))
        || prog_id.eq_ignore_ascii_case("Applications\\zxraw.exe")
}

fn failed(extension: &str, message: String) -> FileTypeDefaultResult {
    FileTypeDefaultResult {
        extension: extension.trim_start_matches('.').to_ascii_lowercase(),
        succeeded: false,
        message: Some(message),
    }
}

/// Hash the UTF-16LE input using the currently observed Windows `UserChoice` v1 scheme.
/// `last_write_filetime` is the raw FILETIME value reported by RegQueryInfoKey.
fn generate_hash(extension: &str, sid: &str, prog_id: &str, last_write_filetime: u64) -> String {
    let rounded = last_write_filetime - last_write_filetime % TICKS_PER_MINUTE;
    let high = (rounded >> 32) as u32;
    let low = rounded as u32;
    let canonical =
        format!("{extension}{sid}{prog_id}{high:08x}{low:08x}{EXPERIENCE_SALT}").to_lowercase();

    let mut bytes = Vec::with_capacity((canonical.len() + 1) * 2);
    for unit in canonical.encode_utf16().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let digest = Md5::digest(&bytes);
    let multipliers = [
        u32::from_le_bytes(digest[0..4].try_into().expect("MD5 has four bytes")),
        u32::from_le_bytes(digest[4..8].try_into().expect("MD5 has four bytes")),
    ];
    let c0 = [
        [
            multipliers[0] | 1,
            0xCF98_B111,
            0x8708_5B9F,
            0x12CE_B96D,
            0x257E_1D83,
        ],
        [
            multipliers[1] | 1,
            0xA274_16F5,
            0xD383_96FF,
            0x7C93_2B89,
            0xBFA4_9F69,
        ],
    ];
    let c1 = [
        [
            multipliers[0] | 1,
            0xEF05_69FB,
            0x689B_6B9F,
            0x79F8_A395,
            0xC3EF_EA97,
        ],
        [
            multipliers[1] | 1,
            0xC317_13DB,
            0xDDCD_1F0F,
            0x59C3_AF2D,
            0x35BD_1EC9,
        ],
    ];

    let mut h0 = 0_u32;
    let mut h1 = 0_u32;
    let mut h0_acc = 0_u32;
    let mut h1_acc = 0_u32;
    for block in bytes.as_chunks::<8>().0 {
        for word_index in 0..2 {
            let start = word_index * 4;
            let input = u32::from_le_bytes(
                block[start..start + 4]
                    .try_into()
                    .expect("four bytes in each hash word"),
            );
            h0 = h0.wrapping_add(input);
            h0 = h0.wrapping_mul(c0[word_index][0]);
            h0 = word_swap(h0).wrapping_mul(c0[word_index][1]);
            h0 = word_swap(h0).wrapping_mul(c0[word_index][2]);
            h0 = word_swap(h0).wrapping_mul(c0[word_index][3]);
            h0 = word_swap(h0).wrapping_mul(c0[word_index][4]);
            h0_acc = h0_acc.wrapping_add(h0);

            h1 = h1.wrapping_add(input);
            h1 = word_swap(h1)
                .wrapping_mul(c1[word_index][1])
                .wrapping_add(h1.wrapping_mul(c1[word_index][0]));
            h1 = (h1 >> 16)
                .wrapping_mul(c1[word_index][2])
                .wrapping_add(h1.wrapping_mul(c1[word_index][3]));
            h1 = word_swap(h1)
                .wrapping_mul(c1[word_index][4])
                .wrapping_add(h1);
            h1_acc = h1_acc.wrapping_add(h1);
        }
    }

    let mut result = [0_u8; 8];
    result[..4].copy_from_slice(&(h0 ^ h1).to_le_bytes());
    result[4..].copy_from_slice(&(h0_acc ^ h1_acc).to_le_bytes());
    base64_eight_bytes(&result)
}

fn word_swap(value: u32) -> u32 {
    value.rotate_left(16)
}

fn base64_eight_bytes(bytes: &[u8; 8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(12);
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as usize;
        let b = chunk.get(1).copied().unwrap_or(0) as usize;
        let c = chunk.get(2).copied().unwrap_or(0) as usize;
        encoded.push(ALPHABET[a >> 2] as char);
        encoded.push(ALPHABET[((a & 0x03) << 4) | (b >> 4)] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((b & 0x0F) << 2) | (c >> 6)] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[c & 0x3F] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    // These are read-only vectors captured on Windows 11 Pro build 26200. The timestamps are
    // UTC key last-write times rounded down to the minute; each output matched the real Hash.
    const SID: &str = "S-1-5-21-2071675659-896857157-2987233961-1001";

    #[test]
    fn matches_real_windows_26200_user_choice_vectors() {
        let vectors = [
            (
                ".jpg",
                "ZXrawExplorer.jpg",
                0x01dd_5a3e_f8d2_8000,
                "BuIaVj+OTeo=",
            ),
            (
                ".cr3",
                "ZXrawExplorer.cr3",
                0x01dd_5a3e_d50f_3a00,
                "t8+WrS8n/aQ=",
            ),
            (
                ".png",
                "ZXrawExplorer.png",
                0x01dd_5a3e_f8d2_8000,
                "R+Jj23aU/JI=",
            ),
            (
                ".txt",
                "AppX4ztfk9wxr86nxmzzq47px0nh0e58b8fw",
                0x01dd_1912_185f_7e00,
                "DjaT0y47csM=",
            ),
        ];

        for (extension, prog_id, timestamp, expected) in vectors {
            assert_eq!(generate_hash(extension, SID, prog_id, timestamp), expected);
        }
    }

    #[test]
    fn canonicalization_is_case_insensitive_and_uses_minute_precision() {
        let expected = generate_hash(".jpg", SID, "ZXrawExplorer.jpg", 0x01dd_5a3e_f8d2_8000);
        assert_eq!(
            generate_hash(
                ".JPG",
                SID,
                "ZXRAWEXPLORER.JPG",
                0x01dd_5a3e_f8d2_8000 + 50 * 10_000_000,
            ),
            expected
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_existing_zxraw_userchoice_is_removed_on_restore_not_saved_as_previous() {
        assert_eq!(previous_choice_snapshot(Some("ZXrawExplorer.jpg")), "N:");
        assert_eq!(
            previous_choice_snapshot(Some("Applications\\zxraw.exe")),
            "N:"
        );
        assert_eq!(
            previous_choice_snapshot(Some("Photos.Jpeg")),
            "P:Photos.Jpeg"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "writes only a unique throwaway extension under the current user's registry"]
    fn dummy_extension_hash_is_honored_and_invalid_hash_falls_back() {
        use super::super::file_types::notify_association_change;
        use super::super::integration::registry;
        use std::time::{SystemTime, UNIX_EPOCH};
        use windows::Win32::UI::Shell::{ASSOCSTR_COMMAND, ASSOCSTR_EXECUTABLE};

        let suffix = format!(
            "zx{:04x}{:04x}",
            std::process::id() as u16,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u16
        );
        let extension = suffix;
        let fallback = format!("ZXrawChoiceTest.Fallback.{extension}");
        let preferred = format!("ZXrawChoiceTest.Preferred.{extension}");
        let classes = "Software\\Classes";
        let extension_class = format!("{classes}\\.{extension}");
        let fallback_command = r#""C:\Windows\System32\notepad.exe" "%1""#.to_string();
        let preferred_command = r#""C:\Windows\System32\cmd.exe" "%1""#.to_string();
        let fallback_key = format!("{classes}\\{fallback}\\shell\\open\\command");
        let preferred_key = format!("{classes}\\{preferred}\\shell\\open\\command");
        let choice_key = user_choice_key(&extension);

        struct Cleanup(Vec<String>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                use super::super::file_types::notify_association_change;
                use super::super::integration::registry;
                for key in &self.0 {
                    let _ = registry::delete_tree(key);
                    let _ = registry::delete_key(key);
                }
                notify_association_change();
            }
        }
        let _cleanup = Cleanup(vec![
            format!("{USER_CHOICE_ROOT}\\.{extension}"),
            extension_class.clone(),
            format!("{classes}\\{fallback}"),
            format!("{classes}\\{preferred}"),
        ]);

        assert!(!registry::key_exists(&format!("{USER_CHOICE_ROOT}\\.{extension}")).unwrap());
        registry::set_default_string(&extension_class, &fallback).unwrap();
        registry::set_default_string(&fallback_key, &fallback_command).unwrap();
        registry::set_default_string(&preferred_key, &preferred_command).unwrap();

        let sid = current_user_sid().unwrap();
        set_user_choice(&extension, &preferred, &sid).unwrap();
        notify_association_change();
        assert_eq!(
            query_assoc(&format!(".{extension}"), ASSOCSTR_EXECUTABLE)
                .as_deref()
                .map(|value| value.to_ascii_lowercase()),
            Some(r"c:\windows\system32\cmd.exe".into())
        );
        let command = query_assoc(&format!(".{extension}"), ASSOCSTR_COMMAND)
            .expect("valid UserChoice exposes a command");
        assert!(command.to_ascii_lowercase().contains("cmd.exe"));

        registry::set_string(&choice_key, Some("Hash"), "invalid").unwrap();
        notify_association_change();
        assert_eq!(
            query_assoc(&format!(".{extension}"), ASSOCSTR_EXECUTABLE)
                .as_deref()
                .map(|value| value.to_ascii_lowercase()),
            Some(r"c:\windows\system32\notepad.exe".into())
        );
    }

    #[cfg(windows)]
    fn query_assoc(association: &str, what: windows::Win32::UI::Shell::ASSOCSTR) -> Option<String> {
        use windows::Win32::UI::Shell::{
            ASSOCF_INIT_IGNOREUNKNOWN, ASSOCF_NOTRUNCATE, AssocQueryStringW,
        };
        use windows::core::{PCWSTR, PWSTR};

        let association = crate::to_wide(association);
        let mut length = 0_u32;
        unsafe {
            AssocQueryStringW(
                ASSOCF_NOTRUNCATE | ASSOCF_INIT_IGNOREUNKNOWN,
                what,
                PCWSTR(association.as_ptr()),
                PCWSTR::null(),
                None,
                &mut length,
            )
        }
        .ok()
        .ok()?;
        let mut buffer = vec![0_u16; length as usize];
        unsafe {
            AssocQueryStringW(
                ASSOCF_NOTRUNCATE | ASSOCF_INIT_IGNOREUNKNOWN,
                what,
                PCWSTR(association.as_ptr()),
                PCWSTR::null(),
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut length,
            )
        }
        .ok()
        .ok()?;
        let end = buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(buffer.len());
        let result = String::from_utf16_lossy(&buffer[..end]);
        (!result.is_empty()).then_some(result)
    }
}

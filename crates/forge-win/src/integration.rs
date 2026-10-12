//! Reversible, per-user Explorer context menu entries.

use std::path::Path;

use crate::{Result, WinError};

#[cfg(windows)]
const ROOT: &str = "Software\\Classes";
#[cfg(windows)]
const VERB: &str = "ZXrawExplorer";

/// Register "Open in ZXraw Explorer" for folders, drives, and directory backgrounds.
///
/// Only the current user's `HKCU\Software\Classes` tree is modified.
pub fn register_context_menu(exe: impl AsRef<Path>) -> Result<()> {
    #[cfg(windows)]
    {
        register_context_menu_at(ROOT, exe.as_ref())
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Err(WinError::Unsupported)
    }
}

/// Remove ZXraw Explorer's folder, drive, and directory-background context menu entries.
pub fn unregister_context_menu() -> Result<()> {
    #[cfg(windows)]
    {
        unregister_context_menu_at(ROOT)
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

/// Return whether all three current-user context menu entries point at the executable.
pub fn is_registered(exe: impl AsRef<Path>) -> Result<bool> {
    #[cfg(windows)]
    {
        is_registered_at(ROOT, exe.as_ref())
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Err(WinError::Unsupported)
    }
}

/// Refreshes the registered command if the user opted in and this executable moved.
///
/// Returns true only when a stale command was rewritten.
pub fn refresh_if_registered(current_exe: impl AsRef<Path>) -> Result<bool> {
    #[cfg(windows)]
    {
        refresh_if_registered_at(ROOT, current_exe.as_ref())
    }
    #[cfg(not(windows))]
    {
        let _ = current_exe;
        Err(WinError::Unsupported)
    }
}

#[cfg(windows)]
const LOCATIONS: [&str; 3] = [
    "Directory\\shell",
    "Drive\\shell",
    "Directory\\Background\\shell",
];

#[cfg(windows)]
fn menu_key(root: &str, location: &str) -> String {
    format!("{root}\\{location}\\{VERB}")
}

#[cfg(windows)]
fn command_key(root: &str, location: &str) -> String {
    format!("{}\\command", menu_key(root, location))
}

#[cfg(windows)]
fn command_line(exe: &Path) -> Result<Vec<u16>> {
    shell_command(exe, "%V")
}

/// `"<exe>" "<placeholder>"`, NUL-terminated, for a `shell\<verb>\command` default value.
#[cfg(windows)]
pub(crate) fn shell_command(exe: &Path, placeholder: &str) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    // A quote cannot occur in a valid Windows file name. Keeping the path as UTF-16 avoids
    // losing non-Unicode Windows paths while producing the required quoted shell command.
    let executable: Vec<u16> = exe.as_os_str().encode_wide().collect();
    if executable.contains(&(b'"' as u16)) {
        return Err(WinError::Invalid("executable path contains a quote".into()));
    }
    let mut result = vec![b'"' as u16];
    result.extend(executable);
    result.extend(format!("\" \"{placeholder}\"").encode_utf16());
    result.push(0);
    Ok(result)
}

#[cfg(windows)]
fn register_context_menu_at(root: &str, exe: &Path) -> Result<()> {
    let command = command_line(exe)?;
    for location in LOCATIONS {
        let key_path = menu_key(root, location);
        if let Err(error) = registry::set_default_string(&key_path, "Open in ZXraw Explorer")
            .and_then(|_| registry::set_default_wide(&command_key(root, location), &command))
        {
            let _ = unregister_context_menu_at(root);
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn unregister_context_menu_at(root: &str) -> Result<()> {
    for location in LOCATIONS {
        registry::delete_tree(&menu_key(root, location))?;
    }
    Ok(())
}

#[cfg(windows)]
fn is_registered_at(root: &str, exe: &Path) -> Result<bool> {
    let expected = command_line(exe)?;
    for location in LOCATIONS {
        let Some(actual) = registry::default_wide(&command_key(root, location))? else {
            return Ok(false);
        };
        if actual != expected[..expected.len().saturating_sub(1)] {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(windows)]
fn refresh_if_registered_at(root: &str, current_exe: &Path) -> Result<bool> {
    if !LOCATIONS.iter().try_fold(true, |all, location| {
        registry::key_exists(&command_key(root, location)).map(|exists| all && exists)
    })? {
        return Ok(false);
    }
    if is_registered_at(root, current_exe)? {
        return Ok(false);
    }
    register_context_menu_at(root, current_exe)?;
    Ok(true)
}

#[cfg(windows)]
pub(crate) mod registry {
    //! Minimal `HKEY_CURRENT_USER` access; every path is relative to HKCU.

    use std::ffi::c_void;

    use crate::{Result, WinError};

    type HKey = *mut c_void;
    // Predefined Win32 HKEYs are sign-extended from LONG through ULONG_PTR.
    const HKEY_CURRENT_USER: HKey = -2_147_483_647isize as HKey;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const KEY_CREATE_SUB_KEY: u32 = 0x0004;
    const REG_SZ: u32 = 1;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PATH_NOT_FOUND: i32 = 3;
    const ERROR_MORE_DATA: i32 = 234;
    const ERROR_NO_MORE_ITEMS: i32 = 259;
    /// Registry value names are at most 16,383 characters.
    const MAX_VALUE_NAME: usize = 16_384;

    #[link(name = "Advapi32")]
    unsafe extern "system" {
        fn RegCreateKeyExW(
            hkey: HKey,
            sub_key: *const u16,
            reserved: u32,
            class: *mut u16,
            options: u32,
            desired: u32,
            security_attributes: *const c_void,
            result: *mut HKey,
            disposition: *mut u32,
        ) -> i32;
        fn RegSetValueExW(
            hkey: HKey,
            value_name: *const u16,
            reserved: u32,
            value_type: u32,
            data: *const u8,
            data_size: u32,
        ) -> i32;
        fn RegOpenKeyExW(
            hkey: HKey,
            sub_key: *const u16,
            options: u32,
            desired: u32,
            result: *mut HKey,
        ) -> i32;
        fn RegQueryValueExW(
            hkey: HKey,
            value_name: *const u16,
            reserved: *mut u32,
            value_type: *mut u32,
            data: *mut u8,
            data_size: *mut u32,
        ) -> i32;
        fn RegEnumValueW(
            hkey: HKey,
            index: u32,
            value_name: *mut u16,
            value_name_len: *mut u32,
            reserved: *mut u32,
            value_type: *mut u32,
            data: *mut u8,
            data_size: *mut u32,
        ) -> i32;
        fn RegDeleteValueW(hkey: HKey, value_name: *const u16) -> i32;
        fn RegCloseKey(hkey: HKey) -> i32;
        fn RegDeleteTreeW(hkey: HKey, sub_key: *const u16) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `None` names the key's default ("(Default)") value.
    fn name_ptr(name: &Option<Vec<u16>>) -> *const u16 {
        name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr())
    }

    fn not_found(status: i32) -> bool {
        status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND
    }

    fn api_error(op: &'static str, code: i32) -> WinError {
        WinError::Api {
            op,
            code: code as u32,
            message: std::io::Error::from_raw_os_error(code).to_string(),
        }
    }

    fn create_key(path: &str) -> Result<HKey> {
        let path_wide = wide(path);
        let mut key = std::ptr::null_mut();
        let mut disposition = 0;
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path_wide.as_ptr(),
                0,
                std::ptr::null_mut(),
                0,
                KEY_SET_VALUE | KEY_CREATE_SUB_KEY,
                std::ptr::null(),
                &mut key,
                &mut disposition,
            )
        };
        if status != 0 {
            return Err(api_error("RegCreateKeyExW", status));
        }
        Ok(key)
    }

    /// Opens an existing key; `None` when it does not exist.
    fn open_key(path: &str, access: u32) -> Result<Option<HKey>> {
        let path_wide = wide(path);
        let mut key = std::ptr::null_mut();
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path_wide.as_ptr(), 0, access, &mut key) };
        if not_found(status) {
            Ok(None)
        } else if status != 0 {
            Err(api_error("RegOpenKeyExW", status))
        } else {
            Ok(Some(key))
        }
    }

    fn set_bytes(path: &str, name: Option<&str>, bytes: &[u8]) -> Result<()> {
        let name = name.map(wide);
        let key = create_key(path)?;
        let status = unsafe {
            RegSetValueExW(
                key,
                name_ptr(&name),
                0,
                REG_SZ,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        };
        unsafe { RegCloseKey(key) };
        if status != 0 {
            return Err(api_error("RegSetValueExW", status));
        }
        Ok(())
    }

    pub(crate) fn set_default_string(path: &str, value: &str) -> Result<()> {
        set_string(path, None, value)
    }

    /// Sets a `REG_SZ` value, creating the key if needed.
    pub(crate) fn set_string(path: &str, name: Option<&str>, value: &str) -> Result<()> {
        set_bytes(path, name, bytemuck_u16_as_bytes(&wide(value)))
    }

    /// Sets the default value from NUL-terminated UTF-16.
    pub(crate) fn set_default_wide(path: &str, value: &[u16]) -> Result<()> {
        set_wide(path, None, value)
    }

    /// Sets a `REG_SZ` value from NUL-terminated UTF-16.
    pub(crate) fn set_wide(path: &str, name: Option<&str>, value: &[u16]) -> Result<()> {
        set_bytes(path, name, bytemuck_u16_as_bytes(value))
    }

    fn bytemuck_u16_as_bytes(value: &[u16]) -> &[u8] {
        // u16 has no padding, and the byte slice is used only during the synchronous call.
        unsafe { std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2) }
    }

    pub(crate) fn key_exists(path: &str) -> Result<bool> {
        Ok(open_key(path, KEY_QUERY_VALUE)?
            .map(|key| unsafe { RegCloseKey(key) })
            .is_some())
    }

    pub(crate) fn default_wide(path: &str) -> Result<Option<Vec<u16>>> {
        wide_value(path, None)
    }

    /// A string value without its terminating NULs. A value of another type reads as empty.
    pub(crate) fn wide_value(path: &str, name: Option<&str>) -> Result<Option<Vec<u16>>> {
        let Some(key) = open_key(path, KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        let name = name.map(wide);

        let result = (|| {
            let mut value_type = 0;
            let mut byte_count = 0;
            let status = unsafe {
                RegQueryValueExW(
                    key,
                    name_ptr(&name),
                    std::ptr::null_mut(),
                    &mut value_type,
                    std::ptr::null_mut(),
                    &mut byte_count,
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if status != 0 {
                return Err(api_error("RegQueryValueExW", status));
            }
            if value_type != REG_SZ || byte_count % 2 != 0 {
                return Ok(Some(Vec::new()));
            }
            let mut value = vec![0_u16; byte_count as usize / 2];
            let status = unsafe {
                RegQueryValueExW(
                    key,
                    name_ptr(&name),
                    std::ptr::null_mut(),
                    &mut value_type,
                    value.as_mut_ptr().cast(),
                    &mut byte_count,
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if status == ERROR_MORE_DATA {
                return Err(api_error("RegQueryValueExW", status));
            }
            if status != 0 {
                return Err(api_error("RegQueryValueExW", status));
            }
            value.truncate(byte_count as usize / 2);
            while value.last() == Some(&0) {
                value.pop();
            }
            Ok(Some(value))
        })();
        unsafe { RegCloseKey(key) };
        result
    }

    /// Like [`wide_value`], decoded lossily.
    pub(crate) fn string_value(path: &str, name: Option<&str>) -> Result<Option<String>> {
        Ok(wide_value(path, name)?.map(|value| String::from_utf16_lossy(&value)))
    }

    /// Deletes one value. A missing key or value is not an error.
    pub(crate) fn delete_value(path: &str, name: Option<&str>) -> Result<()> {
        let Some(key) = open_key(path, KEY_SET_VALUE)? else {
            return Ok(());
        };
        let name = name.map(wide);
        let status = unsafe { RegDeleteValueW(key, name_ptr(&name)) };
        unsafe { RegCloseKey(key) };
        if status == 0 || not_found(status) {
            Ok(())
        } else {
            Err(api_error("RegDeleteValueW", status))
        }
    }

    /// Names of the key's named values (not its default value); empty if the key is missing.
    pub(crate) fn value_names(path: &str) -> Result<Vec<String>> {
        let Some(key) = open_key(path, KEY_QUERY_VALUE)? else {
            return Ok(Vec::new());
        };
        let mut names = Vec::new();
        let mut buffer = vec![0_u16; MAX_VALUE_NAME];
        let result = (|| {
            for index in 0.. {
                let mut length = buffer.len() as u32;
                let status = unsafe {
                    RegEnumValueW(
                        key,
                        index,
                        buffer.as_mut_ptr(),
                        &mut length,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if status == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if status != 0 {
                    return Err(api_error("RegEnumValueW", status));
                }
                if length > 0 {
                    names.push(String::from_utf16_lossy(&buffer[..length as usize]));
                }
            }
            Ok(())
        })();
        unsafe { RegCloseKey(key) };
        result.map(|()| names)
    }

    pub(crate) fn delete_tree(path: &str) -> Result<()> {
        let path_wide = wide(path);
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path_wide.as_ptr()) };
        if status == 0 || not_found(status) {
            Ok(())
        } else {
            Err(api_error("RegDeleteTreeW", status))
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn command_line_quotes_the_executable_and_uses_shell_directory_value() {
        let command =
            super::command_line(std::path::Path::new(r"C:\Test Folder\zxraw.exe")).unwrap();
        let command = String::from_utf16(&command[..command.len() - 1]).unwrap();
        assert_eq!(command, r#""C:\Test Folder\zxraw.exe" "%V""#);
    }

    #[cfg(windows)]
    #[test]
    fn registration_lifecycle_uses_an_isolated_hkcu_test_root() {
        use std::{path::Path, time::SystemTime};

        let root = format!(
            "Software\\Classes\\ZXrawExplorer-Test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = super::unregister_context_menu_at(&self.0);
                let _ = super::registry::delete_tree(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());

        let original = Path::new(r"C:\Test Folder\zxraw.exe");
        let moved = Path::new(r"D:\Moved Folder\zxraw.exe");
        super::register_context_menu_at(&root, original).unwrap();
        assert!(super::is_registered_at(&root, original).unwrap());
        assert!(!super::is_registered_at(&root, moved).unwrap());
        assert!(super::refresh_if_registered_at(&root, moved).unwrap());
        assert!(super::is_registered_at(&root, moved).unwrap());
        assert!(!super::refresh_if_registered_at(&root, moved).unwrap());
        super::unregister_context_menu_at(&root).unwrap();
        assert!(!super::is_registered_at(&root, moved).unwrap());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_registration_is_explicitly_unsupported() {
        assert!(super::register_context_menu("zxraw.exe").is_err());
    }
}

//! Network paths: UNC shares (`\\server\share\…`) and mapped network drives.
//!
//! Everything here except [`is_network_path`] is lexical. [`is_network_path`] asks Windows
//! for the type of a drive letter (`GetDriveTypeW`). In practice that answers from the local
//! device table without contacting the server, but Windows does not document that it never
//! touches the network (see docs/LIMITATIONS.md). UNC paths are decided lexically and never
//! reach that call. Nothing in this
//! module opens a path on a share: callers must do that on a worker thread with a timeout,
//! because an offline server can block a file call for tens of seconds.

use std::path::{Path, PathBuf};

/// Characters that cannot appear in a server or share name (or any path component).
const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*', '/'];

/// The `server` and `share` of a UNC path, or `None` when `path` is not a UNC path.
/// Accepts `\\server\share\…`, `//server/share/…` and `\\?\UNC\server\share\…`.
/// Device paths (`\\?\C:\…`, `\\.\…`) are not UNC paths.
pub fn unc_parts(path: &Path) -> Option<(String, String)> {
    let text = path.to_string_lossy().replace('/', "\\");
    let rest = if let Some(r) = strip_prefix_ci(&text, r"\\?\UNC\") {
        r
    } else if text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
        return None;
    } else {
        text.strip_prefix(r"\\")?
    };
    let mut parts = rest.split('\\');
    // Kept exactly as written: `\\nas\ photos` is a different share from `\\nas\photos`.
    let server = parts.next()?;
    let share = parts.next()?;
    if server.is_empty() || share.is_empty() {
        return None;
    }
    Some((server.to_string(), share.to_string()))
}

/// Is `path` a UNC path (`\\server\share\…`)? Lexical.
pub fn is_unc(path: &Path) -> bool {
    unc_parts(path).is_some()
}

/// `\\server\share` for a UNC path: the unit Windows connects to and authenticates.
pub fn share_root(path: &Path) -> Option<PathBuf> {
    unc_parts(path).map(|(server, share)| PathBuf::from(format!(r"\\{server}\{share}")))
}

/// The drive letter of `C:…` paths (including `\\?\C:\…`), uppercased.
fn drive_letter(path: &Path) -> Option<char> {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let b = text.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        .then(|| (b[0] as char).to_ascii_uppercase())
}

/// Is `path` on a network location: a UNC path or a drive letter mapped to a share?
///
/// Background work uses this to pick gentler concurrency, and the UI to avoid file calls
/// on its own thread. UNC paths are recognised lexically; only a drive letter asks
/// `GetDriveTypeW`, which in practice answers locally.
pub fn is_network_path(path: &Path) -> bool {
    if is_unc(path) {
        return true;
    }
    drive_letter(path).is_some_and(is_remote_drive)
}

#[cfg(windows)]
fn is_remote_drive(letter: char) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::PCWSTR;
    // DRIVE_REMOTE (4). Not re-exported as a constant by every `windows` feature set.
    const DRIVE_REMOTE: u32 = 4;
    let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain(Some(0)).collect();
    // SAFETY: `root` is NUL-terminated and outlives the call.
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_REMOTE }
}

#[cfg(not(windows))]
fn is_remote_drive(_letter: char) -> bool {
    false
}

/// Win32 error codes that mean the server or the network went away (as opposed to a
/// missing file or a permission problem on a reachable server).
pub fn is_network_error_code(code: i32) -> bool {
    matches!(
        code,
        51      // ERROR_REM_NOT_LIST: remote computer not available
        | 53    // ERROR_BAD_NETPATH
        | 54    // ERROR_NETWORK_BUSY
        | 59    // ERROR_UNEXP_NET_ERR
        | 64    // ERROR_NETNAME_DELETED: connection dropped mid-operation
        | 67    // ERROR_BAD_NET_NAME: share not found on the server
        | 121   // ERROR_SEM_TIMEOUT
        | 1203  // ERROR_NO_NET_OR_BAD_PATH
        | 1222  // ERROR_NO_NETWORK
        | 1231  // ERROR_NETWORK_UNREACHABLE
        | 1232  // ERROR_HOST_UNREACHABLE
        | 1311  // ERROR_NO_LOGON_SERVERS
        | 2250 // ERROR_NOT_CONNECTED
    )
}

/// Does `e` mean the network location is unreachable?
pub fn is_network_error(e: &std::io::Error) -> bool {
    e.raw_os_error().is_some_and(is_network_error_code)
}

/// Win32 error codes that mean the server rejected the user (wrong or missing credentials).
pub fn is_login_error_code(code: i32) -> bool {
    matches!(
        code,
        5       // ERROR_ACCESS_DENIED
        | 86    // ERROR_INVALID_PASSWORD
        | 1326  // ERROR_LOGON_FAILURE
        | 1327  // ERROR_ACCOUNT_RESTRICTION
        | 1328  // ERROR_INVALID_LOGON_HOURS
        | 1329  // ERROR_INVALID_WORKSTATION
        | 1330  // ERROR_PASSWORD_EXPIRED
        | 1331  // ERROR_ACCOUNT_DISABLED
        | 1909  // ERROR_ACCOUNT_LOCKED_OUT
        | 1244 // ERROR_NOT_AUTHENTICATED
    )
}

/// Validates a network location typed by the user and returns it normalized as
/// `\\server\share[\sub\folders]`. Accepts `\\server\share`, `//server/share`,
/// `\\?\UNC\server\share`, `smb://server/share` and `file://server/share`, with or without a
/// trailing separator. Lexical: nothing is contacted.
pub fn validate_share_path(input: &str) -> Result<PathBuf, String> {
    let mut s = input.trim().trim_matches('"').trim().to_string();
    if s.is_empty() {
        return Err("Enter a share path such as \\\\nas\\photos.".into());
    }
    for scheme in ["smb://", "file://"] {
        if let Some(rest) = strip_prefix_ci(&s, scheme) {
            s = format!("//{rest}");
            break;
        }
    }
    let mut s = s.replace('/', "\\");
    // The long form of a UNC path names the same share.
    if let Some(rest) = strip_prefix_ci(&s, r"\\?\UNC\") {
        s = format!(r"\\{rest}");
    }
    if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") {
        return Err("Device paths (\\\\?\\ and \\\\.\\) are not network shares.".into());
    }
    let Some(rest) = s.strip_prefix(r"\\") else {
        return Err(r"A network share starts with \\, for example \\nas\photos.".into());
    };
    if rest.starts_with('\\') {
        return Err("Use exactly two backslashes before the server name.".into());
    }
    let comps: Vec<&str> = rest.split('\\').filter(|c| !c.is_empty()).collect();
    let server = match comps.as_slice() {
        [] => return Err(r"Enter the server name after \\, for example \\nas\photos.".into()),
        [_] => {
            return Err(r"Add the share name after the server, for example \\nas\photos.".into());
        }
        [server, ..] => unc_server(server)?,
    };
    let share = comps[1];
    if share.trim() != share {
        return Err(format!("The share name \"{share}\" cannot start or end with a space."));
    }
    for c in &comps[1..] {
        if *c == "." || *c == ".." {
            return Err("The path cannot contain . or .. folders.".into());
        }
        if c.chars().any(|ch| INVALID_CHARS.contains(&ch) || ch.is_control()) {
            return Err(format!("\"{c}\" contains a character Windows does not allow."));
        }
        if c.ends_with(' ') || c.ends_with('.') {
            return Err(format!("\"{c}\" cannot end with a space or a dot."));
        }
    }
    Ok(PathBuf::from(format!(r"\\{server}\{}", comps[1..].join("\\"))))
}

/// Checks a server name and returns it as it must appear in a UNC path. A host name or an
/// IPv4 address is kept as typed. Windows does not accept a bracketed IPv6 address in a UNC
/// path, so `[fe80::1%4]` becomes its `fe80--1s4.ipv6-literal.net` form.
fn unc_server(server: &str) -> Result<String, String> {
    if let Some(inner) = server.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let (addr, zone) = inner.split_once('%').unwrap_or((inner, ""));
        let zone_ok = zone.chars().all(|c| c.is_ascii_alphanumeric());
        if addr.parse::<std::net::Ipv6Addr>().is_err() || !zone_ok {
            return Err(format!("\"{server}\" is not a valid IPv6 address."));
        }
        let zone = if zone.is_empty() { String::new() } else { format!("s{zone}") };
        return Ok(format!("{}{zone}.ipv6-literal.net", addr.replace(':', "-")));
    }
    validate_server(server)?;
    Ok(server.to_string())
}

/// A server is a host name (`nas`, `nas.local`), a dotted IPv4 address or the
/// `fe80--1.ipv6-literal.net` form Windows uses for IPv6 in UNC paths.
fn validate_server(server: &str) -> Result<(), String> {
    if server.len() > 253 {
        return Err("The server name is too long.".into());
    }
    let ok = server
        .split('.')
        .all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "\"{server}\" is not a valid server name. Use a name like nas or nas.local, or an IP address."
        ))
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_paths_are_recognised_lexically() {
        assert!(is_unc(Path::new(r"\\nas\photos")));
        assert!(is_unc(Path::new(r"\\nas\photos\2024\a.cr3")));
        assert!(is_unc(Path::new("//nas/photos")));
        assert!(is_unc(Path::new(r"\\?\UNC\nas\photos\a.nef")));
        assert!(is_unc(Path::new(r"\\?\unc\nas\photos")));
        assert!(!is_unc(Path::new(r"\\?\C:\photos")));
        assert!(!is_unc(Path::new(r"\\.\PhysicalDrive0")));
        assert!(!is_unc(Path::new(r"\\nas")));
        assert!(!is_unc(Path::new(r"\\nas\")));
        assert!(!is_unc(Path::new(r"C:\photos")));
        assert_eq!(
            share_root(Path::new(r"\\?\UNC\nas\photos\2024\a.nef")),
            Some(PathBuf::from(r"\\nas\photos"))
        );
        assert_eq!(
            share_root(Path::new(r"\\nas\photos\2024")),
            Some(PathBuf::from(r"\\nas\photos"))
        );
        // Spaces belong to the name: they are not trimmed into another share.
        assert_eq!(
            unc_parts(Path::new(r"\\nas\ photos \x")),
            Some(("nas".to_string(), " photos ".to_string()))
        );
        assert_eq!(
            share_root(Path::new(r"\\nas\ photos")),
            Some(PathBuf::from(r"\\nas\ photos"))
        );
    }

    #[test]
    fn local_paths_are_not_network_paths() {
        let dir = std::env::temp_dir();
        assert!(!is_network_path(&dir));
        assert!(is_network_path(Path::new(r"\\nas\photos\a.jpg")));
        assert!(!is_network_path(Path::new("relative")));
    }

    #[test]
    fn share_paths_are_validated_and_normalized() {
        let ok = |s: &str| validate_share_path(s).unwrap();
        assert_eq!(ok(r"\\nas\photos"), PathBuf::from(r"\\nas\photos"));
        assert_eq!(ok(r" \\nas\photos\ "), PathBuf::from(r"\\nas\photos"));
        assert_eq!(ok(r#""\\nas\photos""#), PathBuf::from(r"\\nas\photos"));
        assert_eq!(ok("//nas/photos/2024"), PathBuf::from(r"\\nas\photos\2024"));
        assert_eq!(ok("smb://nas.local/photos"), PathBuf::from(r"\\nas.local\photos"));
        assert_eq!(ok("SMB://nas/photos"), PathBuf::from(r"\\nas\photos"));
        assert_eq!(ok("file://nas/photos"), PathBuf::from(r"\\nas\photos"));
        assert_eq!(ok(r"\\192.168.1.20\photos$"), PathBuf::from(r"\\192.168.1.20\photos$"));
        assert_eq!(
            ok(r"\\[fe80::1]\photos"),
            PathBuf::from(r"\\fe80--1.ipv6-literal.net\photos")
        );
        assert_eq!(
            ok(r"\\[fe80::1%4]\photos"),
            PathBuf::from(r"\\fe80--1s4.ipv6-literal.net\photos")
        );
        assert_eq!(
            ok(r"\\fe80--1.ipv6-literal.net\photos"),
            PathBuf::from(r"\\fe80--1.ipv6-literal.net\photos")
        );
        assert_eq!(ok(r"\\192.0.2.1\x\sub dir\"), PathBuf::from(r"\\192.0.2.1\x\sub dir"));
        assert_eq!(ok(r"\\?\UNC\nas\photos\2024"), PathBuf::from(r"\\nas\photos\2024"));
        assert_eq!(ok(r"\\?\unc\nas\photos"), PathBuf::from(r"\\nas\photos"));
        assert_eq!(ok(r"\\my_nas\Photo Archive"), PathBuf::from(r"\\my_nas\Photo Archive"));

        for bad in [
            "",
            "   ",
            r"C:\photos",
            "photos",
            r"\\",
            r"\\nas",
            r"\\nas\",
            r"\\\nas\photos",
            r"\\?\C:\photos",
            r"\\?\UNC\nas",
            r"\\nas\ photos",
            r"\\nas\photos \x",
            r"\\.\pipe\x",
            r"\\na s\photos",
            r"\\-nas\photos",
            r"\\nas..local\photos",
            r"\\nas\pho*tos",
            r"\\nas\photos\a|b",
            r"\\nas\photos\..\x",
            r"\\nas\photos.",
            r"\\[zz]\photos",
            r"\\[fe80::1%a b]\photos",
            r"\\nas\photos\sub.",
            r"\\nas\photos\sub \x",
            "smb://",
            "smb://nas",
            r"\\nas:445\photos",
        ] {
            assert!(validate_share_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn error_codes_are_classified() {
        assert!(is_network_error_code(53));
        assert!(is_network_error_code(64));
        assert!(!is_network_error_code(2));
        assert!(is_login_error_code(1326));
        assert!(!is_login_error_code(53));
        assert!(is_network_error(&std::io::Error::from_raw_os_error(67)));
    }
}

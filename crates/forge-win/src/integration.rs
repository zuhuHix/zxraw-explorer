//! Reversible, per-user Explorer context menu entries.

use std::path::Path;

use crate::{Result, WinError};

#[cfg(windows)]
const ROOT: &str = "Software\\Classes";
#[cfg(windows)]
const VERB: &str = "ZXrawExplorer";

/// Register "Open in ZXraw Explorer" for folders, drives, and directory backgrounds.
///
/// Only the current user's `HKCU\\Software\\Classes` tree is modified.
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

/// Return whether all three current-user context menu entries are installed.
pub fn is_registered() -> Result<bool> {
    #[cfg(windows)]
    {
        is_registered_at(ROOT)
    }
    #[cfg(not(windows))]
    {
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
    use std::os::windows::ffi::OsStrExt;

    // A quote cannot occur in a valid Windows file name. Keeping the path as UTF-16 avoids
    // losing non-Unicode Windows paths while producing the required quoted shell command.
    let executable: Vec<u16> = exe.as_os_str().encode_wide().collect();
    if executable.contains(&(b'"' as u16)) {
        return Err(WinError::Invalid("executable path contains a quote".into()));
    }
    let mut result = vec![b'"' as u16];
    result.extend(executable);
    result.extend("\" \"%V\"".encode_utf16());
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
fn is_registered_at(root: &str) -> Result<bool> {
    for location in LOCATIONS {
        if !registry::key_exists(&command_key(root, location))? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(windows)]
mod registry {
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
        fn RegCloseKey(hkey: HKey) -> i32;
        fn RegDeleteTreeW(hkey: HKey, sub_key: *const u16) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
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

    fn set_bytes(path: &str, bytes: &[u8]) -> Result<()> {
        let key = create_key(path)?;
        let status = unsafe {
            RegSetValueExW(
                key,
                std::ptr::null(),
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

    pub(super) fn set_default_string(path: &str, value: &str) -> Result<()> {
        let mut encoded: Vec<u16> = value.encode_utf16().collect();
        encoded.push(0);
        set_bytes(path, bytemuck_u16_as_bytes(&encoded))
    }

    pub(super) fn set_default_wide(path: &str, value: &[u16]) -> Result<()> {
        set_bytes(path, bytemuck_u16_as_bytes(value))
    }

    fn bytemuck_u16_as_bytes(value: &[u16]) -> &[u8] {
        // u16 has no padding, and the byte slice is used only during the synchronous call.
        unsafe { std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2) }
    }

    pub(super) fn key_exists(path: &str) -> Result<bool> {
        let path_wide = wide(path);
        let mut key = std::ptr::null_mut();
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path_wide.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };
        if status == 0 {
            unsafe { RegCloseKey(key) };
            Ok(true)
        } else if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
            Ok(false)
        } else {
            Err(api_error("RegOpenKeyExW", status))
        }
    }

    pub(super) fn delete_tree(path: &str) -> Result<()> {
        let path_wide = wide(path);
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path_wide.as_ptr()) };
        if status == 0 || status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
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

        super::register_context_menu_at(&root, Path::new(r"C:\Test Folder\zxraw.exe")).unwrap();
        assert!(super::is_registered_at(&root).unwrap());
        super::unregister_context_menu_at(&root).unwrap();
        assert!(!super::is_registered_at(&root).unwrap());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_registration_is_explicitly_unsupported() {
        assert!(super::register_context_menu("zxraw.exe").is_err());
    }
}

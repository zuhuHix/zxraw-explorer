//! Reversible, per-user Explorer context menu entries.

use std::path::Path;

use crate::{Result, WinError};

#[cfg(windows)]
const ROOT: &str = "Software\\Classes";
#[cfg(windows)]
const VERB: &str = "ZXrawExplorer";
#[cfg(windows)]
const FOLDER_STATE: &str = "Software\\ZXrawExplorerDefaultState\\Folder";

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
fn command_line(exe: &Path, location: &str) -> Result<Vec<u16>> {
    let placeholder = if location == "Directory\\Background\\shell" {
        "%V"
    } else {
        "%1"
    };
    shell_command(exe, placeholder)
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
    for location in LOCATIONS {
        let command = command_line(exe, location)?;
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
    for location in LOCATIONS {
        let expected = command_line(exe, location)?;
        let expected = &expected[..expected.len().saturating_sub(1)];
        let Some(actual) = registry::default_wide(&command_key(root, location))? else {
            return Ok(false);
        };
        if actual != expected {
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

/// Set the default verbs for filesystem directories and drives to ZXraw Explorer.
/// `Folder\shell` is deliberately untouched because it also covers virtual shell folders.
pub fn register_default_folder_handler(exe: impl AsRef<Path>) -> Result<()> {
    #[cfg(windows)]
    {
        let result = register_default_folder_handler_at(ROOT, exe.as_ref());
        notify_association_change();
        result
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Err(WinError::Unsupported)
    }
}

/// Restore the shell defaults that were in place before ZXraw was made the folder handler.
pub fn unregister_default_folder_handler() -> Result<()> {
    #[cfg(windows)]
    {
        let result = unregister_default_folder_handler_at(ROOT);
        notify_association_change();
        result
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

/// Whether both filesystem directory and drive defaults currently resolve to ZXraw Explorer.
pub fn default_folder_handler_enabled() -> Result<bool> {
    #[cfg(windows)]
    {
        default_folder_handler_enabled_at(ROOT)
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

/// Refresh the shared directory/drive verb after a move, if folder handling was enabled.
pub fn refresh_default_folder_handler(current_exe: impl AsRef<Path>) -> Result<bool> {
    #[cfg(windows)]
    {
        refresh_default_folder_handler_at(ROOT, current_exe.as_ref())
    }
    #[cfg(not(windows))]
    {
        let _ = current_exe;
        Err(WinError::Unsupported)
    }
}

#[cfg(windows)]
const DEFAULT_FOLDER_LOCATIONS: [&str; 2] = ["Directory\\shell", "Drive\\shell"];

#[cfg(windows)]
fn default_folder_shell(root: &str, location: &str) -> String {
    format!("{root}\\{location}")
}

#[cfg(windows)]
fn folder_state_key(root: &str) -> String {
    if root.eq_ignore_ascii_case(ROOT) {
        FOLDER_STATE.to_string()
    } else {
        format!("{root}\\DefaultFolderState")
    }
}

#[cfg(windows)]
fn save_folder_value(state: &str, name: &str, path: &str, value_name: Option<&str>) -> Result<()> {
    let value = registry::string_value(path, value_name)?;
    registry::set_string(
        state,
        Some(&format!("{name}Present")),
        if value.is_some() { "1" } else { "0" },
    )?;
    match value {
        Some(value) => registry::set_string(state, Some(&format!("{name}Value")), &value),
        None => registry::delete_value(state, Some(&format!("{name}Value"))),
    }
}

#[cfg(windows)]
fn save_folder_key_exists(state: &str, name: &str, path: &str) -> Result<()> {
    registry::set_string(
        state,
        Some(name),
        if registry::key_exists(path)? {
            "1"
        } else {
            "0"
        },
    )
}

#[cfg(windows)]
fn capture_folder_state(root: &str) -> Result<()> {
    let state = folder_state_key(root);
    if registry::string_value(&state, Some("Captured"))?.as_deref() == Some("1") {
        return Ok(());
    }
    for (index, location) in DEFAULT_FOLDER_LOCATIONS.iter().enumerate() {
        let shell = default_folder_shell(root, location);
        let label = menu_key(root, location);
        let command = command_key(root, location);
        let prefix = if index == 0 { "Directory" } else { "Drive" };
        save_folder_key_exists(&state, &format!("{prefix}ShellExisted"), &shell)?;
        save_folder_value(&state, &format!("{prefix}Default"), &shell, None)?;
        save_folder_key_exists(&state, &format!("{prefix}VerbExisted"), &label)?;
        save_folder_value(&state, &format!("{prefix}VerbLabel"), &label, None)?;
        save_folder_value(&state, &format!("{prefix}VerbCommand"), &command, None)?;
    }
    // Set this last so a partially captured snapshot is never mistaken for complete.
    registry::set_string(&state, Some("Captured"), "1")
}

#[cfg(windows)]
fn register_default_folder_handler_at(root: &str, exe: &Path) -> Result<()> {
    capture_folder_state(root)?;
    let result = (|| {
        for location in DEFAULT_FOLDER_LOCATIONS {
            let command = command_line(exe, location)?;
            let key_path = menu_key(root, location);
            registry::set_default_string(&key_path, "Open in ZXraw Explorer")?;
            registry::set_default_wide(&command_key(root, location), &command)?;
            registry::set_default_string(&default_folder_shell(root, location), VERB)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = unregister_default_folder_handler_at(root);
    }
    result
}

#[cfg(windows)]
fn restore_folder_value(
    state: &str,
    name: &str,
    path: &str,
    value_name: Option<&str>,
) -> Result<()> {
    let present = registry::string_value(state, Some(&format!("{name}Present")))?;
    if present.as_deref() == Some("1") {
        let value = registry::string_value(state, Some(&format!("{name}Value")))?
            .ok_or_else(|| WinError::Invalid(format!("saved folder value {name} is missing")))?;
        registry::set_string(path, value_name, &value)
    } else {
        registry::delete_value(path, value_name)
    }
}

#[cfg(windows)]
fn restore_folder_verb(root: &str, state: &str, prefix: &str, location: &str) -> Result<()> {
    let was_present = registry::string_value(state, Some(&format!("{prefix}VerbExisted")))?
        .as_deref()
        == Some("1");
    let menu = menu_key(root, location);
    let command = command_key(root, location);
    if was_present {
        restore_folder_value(state, &format!("{prefix}VerbLabel"), &menu, None)?;
        restore_folder_value(state, &format!("{prefix}VerbCommand"), &command, None)?;
    } else {
        // If the Explorer context-menu task was enabled after the default-folder action,
        // preserve its shared verb; the uninstaller removes it in the following step.
        let background = menu_key(root, "Directory\\Background\\shell");
        if !registry::key_exists(&background)? {
            registry::delete_value(&menu, None)?;
            registry::delete_tree(&menu)?;
            registry::delete_key(&menu)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn unregister_default_folder_handler_at(root: &str) -> Result<()> {
    let state = folder_state_key(root);
    let captured = registry::string_value(&state, Some("Captured"))?.as_deref() == Some("1");
    for (index, location) in DEFAULT_FOLDER_LOCATIONS.iter().enumerate() {
        let shell = default_folder_shell(root, location);
        if registry::string_value(&shell, None)?.as_deref() == Some(VERB) {
            let prefix = if index == 0 { "Directory" } else { "Drive" };
            if captured {
                restore_folder_value(&state, &format!("{prefix}Default"), &shell, None)?;
            } else {
                registry::delete_value(&shell, None)?;
            }
        }
    }
    if captured {
        restore_folder_verb(root, &state, "Directory", DEFAULT_FOLDER_LOCATIONS[0])?;
        restore_folder_verb(root, &state, "Drive", DEFAULT_FOLDER_LOCATIONS[1])?;
        for (index, location) in DEFAULT_FOLDER_LOCATIONS.iter().enumerate() {
            let prefix = if index == 0 { "Directory" } else { "Drive" };
            let existed = registry::string_value(&state, Some(&format!("{prefix}ShellExisted")))?
                .as_deref()
                == Some("1");
            if !existed {
                let shell = default_folder_shell(root, location);
                if registry::default_wide(&shell)?.is_none()
                    && registry::value_names(&shell)?.is_empty()
                    && registry::subkey_names(&shell)?.is_empty()
                {
                    registry::delete_key(&shell)?;
                }
            }
        }
        registry::delete_tree(&state)?;
        registry::delete_key(&state)?;
    }
    Ok(())
}

#[cfg(windows)]
fn default_folder_handler_enabled_at(root: &str) -> Result<bool> {
    for location in DEFAULT_FOLDER_LOCATIONS {
        if registry::string_value(&default_folder_shell(root, location), None)?.as_deref()
            != Some(VERB)
        {
            return Ok(false);
        }
    }
    let Some(exe) = std::env::current_exe().ok() else {
        return Ok(false);
    };
    let expected = exe.to_string_lossy().to_string();
    for assoc in ["Directory", "Drive"] {
        if assoc_query_executable(assoc)
            .is_none_or(|actual| !actual.eq_ignore_ascii_case(&expected))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(windows)]
fn refresh_default_folder_handler_at(root: &str, current_exe: &Path) -> Result<bool> {
    let state = folder_state_key(root);
    let captured = registry::string_value(&state, Some("Captured"))?.as_deref() == Some("1");
    if !captured {
        return Ok(false);
    }
    let command = command_line(current_exe, DEFAULT_FOLDER_LOCATIONS[0])?;
    let command = &command[..command.len().saturating_sub(1)];
    let mut changed = false;
    for location in DEFAULT_FOLDER_LOCATIONS {
        let shell = default_folder_shell(root, location);
        if registry::string_value(&shell, None)?.as_deref() != Some(VERB) {
            continue;
        }
        let key = command_key(root, location);
        if registry::default_wide(&key)?.as_deref() != Some(command) {
            registry::set_default_wide(&key, command)?;
            changed = true;
        }
    }
    if changed {
        notify_association_change();
    }
    Ok(changed)
}

#[cfg(windows)]
fn notify_association_change() {
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

#[cfg(windows)]
fn assoc_query_executable(assoc: &str) -> Option<String> {
    use windows::Win32::UI::Shell::{
        ASSOCF_INIT_IGNOREUNKNOWN, ASSOCF_NOTRUNCATE, ASSOCSTR_EXECUTABLE, AssocQueryStringW,
    };
    use windows::core::{PCWSTR, PWSTR};

    let assoc = crate::to_wide(assoc);
    let flags = ASSOCF_NOTRUNCATE | ASSOCF_INIT_IGNOREUNKNOWN;
    let mut length = 0_u32;
    unsafe {
        AssocQueryStringW(
            flags,
            ASSOCSTR_EXECUTABLE,
            PCWSTR(assoc.as_ptr()),
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
            flags,
            ASSOCSTR_EXECUTABLE,
            PCWSTR(assoc.as_ptr()),
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
    let value = String::from_utf16_lossy(&buffer[..end]);
    (!value.is_empty()).then_some(value)
}

#[cfg(windows)]
pub(crate) mod registry {
    //! Minimal `HKEY_CURRENT_USER` access; every path is relative to HKCU.

    use std::ffi::c_void;

    use crate::{Result, WinError};

    type HKey = *mut c_void;
    // Predefined Win32 HKEYs are sign-extended from LONG through ULONG_PTR.
    const HKEY_CURRENT_USER: HKey = -2_147_483_647isize as HKey;
    const HKEY_LOCAL_MACHINE: HKey = -2_147_483_646isize as HKey;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const KEY_CREATE_SUB_KEY: u32 = 0x0004;
    const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
    const REG_SZ: u32 = 1;
    const REG_DWORD: u32 = 4;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PATH_NOT_FOUND: i32 = 3;
    const ERROR_MORE_DATA: i32 = 234;
    const ERROR_NO_MORE_ITEMS: i32 = 259;
    /// Registry value names are at most 16,383 characters.
    const MAX_VALUE_NAME: usize = 16_384;
    /// Registry key names are limited to 255 characters, but use the value-size buffer here.
    const MAX_KEY_NAME: usize = 16_384;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

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
        fn RegQueryInfoKeyW(
            hkey: HKey,
            class: *mut u16,
            class_len: *mut u32,
            reserved: *mut u32,
            subkeys: *mut u32,
            max_subkey_len: *mut u32,
            max_class_len: *mut u32,
            values: *mut u32,
            max_value_name_len: *mut u32,
            max_value_len: *mut u32,
            security_descriptor_len: *mut u32,
            last_write: *mut FileTime,
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
        fn RegEnumKeyExW(
            hkey: HKey,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            class: *mut u16,
            class_len: *mut u32,
            last_write: *mut FileTime,
        ) -> i32;
        fn RegDeleteValueW(hkey: HKey, value_name: *const u16) -> i32;
        fn RegCloseKey(hkey: HKey) -> i32;
        fn RegDeleteKeyW(hkey: HKey, sub_key: *const u16) -> i32;
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
        open_key_at(HKEY_CURRENT_USER, path, access)
    }

    fn open_key_at(hive: HKey, path: &str, access: u32) -> Result<Option<HKey>> {
        let path_wide = wide(path);
        let mut key = std::ptr::null_mut();
        let status = unsafe { RegOpenKeyExW(hive, path_wide.as_ptr(), 0, access, &mut key) };
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

    /// Last-write FILETIME from `RegQueryInfoKeyW`, preserving its 100 ns precision.
    pub(crate) fn last_write_filetime(path: &str) -> Result<Option<u64>> {
        let Some(key) = open_key(path, KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        let mut last_write = FileTime::default();
        let status = unsafe {
            RegQueryInfoKeyW(
                key,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut last_write,
            )
        };
        unsafe { RegCloseKey(key) };
        if status != 0 {
            return Err(api_error("RegQueryInfoKeyW", status));
        }
        Ok(Some(
            ((last_write.high as u64) << 32) | last_write.low as u64,
        ))
    }

    /// Read a `REG_DWORD` from HKLM, used to detect the newer UserChoice hash migration.
    pub(crate) fn machine_dword_value(path: &str, name: &str) -> Result<Option<u32>> {
        let Some(key) = open_key_at(HKEY_LOCAL_MACHINE, path, KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        let name = wide(name);
        let mut value_type = 0;
        let mut value = 0_u32;
        let mut byte_count = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut value_type,
                (&mut value as *mut u32).cast(),
                &mut byte_count,
            )
        };
        unsafe { RegCloseKey(key) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != 0 {
            return Err(api_error("RegQueryValueExW", status));
        }
        if value_type != REG_DWORD || byte_count != std::mem::size_of::<u32>() as u32 {
            return Ok(None);
        }
        Ok(Some(value))
    }

    /// Removes the named key itself. Unlike `RegDeleteTreeW`, this does not leave an empty key.
    pub(crate) fn delete_key(path: &str) -> Result<()> {
        let path_wide = wide(path);
        let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path_wide.as_ptr()) };
        if status == 0 || not_found(status) {
            Ok(())
        } else {
            Err(api_error("RegDeleteKeyW", status))
        }
    }

    /// Names of immediate subkeys; empty when the parent key is missing.
    pub(crate) fn subkey_names(path: &str) -> Result<Vec<String>> {
        let Some(key) = open_key(path, KEY_ENUMERATE_SUB_KEYS)? else {
            return Ok(Vec::new());
        };
        let mut names = Vec::new();
        let mut buffer = vec![0_u16; MAX_KEY_NAME];
        let result = (|| {
            for index in 0.. {
                let mut length = buffer.len() as u32;
                let status = unsafe {
                    RegEnumKeyExW(
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
                    return Err(api_error("RegEnumKeyExW", status));
                }
                names.push(String::from_utf16_lossy(&buffer[..length as usize]));
            }
            Ok(())
        })();
        unsafe { RegCloseKey(key) };
        result.map(|()| names)
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
    fn default_folder_registration_restores_directory_and_drive_verbs_in_an_isolated_key() {
        use std::time::SystemTime;

        let root = format!(
            "Software\\ZXrawExplorer-FolderTest-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = super::unregister_default_folder_handler_at(&self.0);
                let _ = super::registry::delete_tree(&self.0);
                let _ = super::registry::delete_key(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let dir_shell = super::default_folder_shell(&root, "Directory\\shell");
        let drive_shell = super::default_folder_shell(&root, "Drive\\shell");
        let virtual_shell = format!("{root}\\Folder\\shell");
        super::registry::set_default_string(&dir_shell, "OtherDirectoryVerb").unwrap();
        super::registry::set_default_string(&virtual_shell, "VirtualFolderOpen").unwrap();

        let exe = std::path::Path::new(r"C:\Test Folder\zxraw.exe");
        super::register_default_folder_handler_at(&root, exe).unwrap();
        assert_eq!(
            super::registry::string_value(&dir_shell, None)
                .unwrap()
                .as_deref(),
            Some(super::VERB)
        );
        assert_eq!(
            super::registry::string_value(&drive_shell, None)
                .unwrap()
                .as_deref(),
            Some(super::VERB)
        );
        assert_eq!(
            super::registry::string_value(&virtual_shell, None)
                .unwrap()
                .as_deref(),
            Some("VirtualFolderOpen")
        );
        let dir_command =
            super::registry::string_value(&super::command_key(&root, "Directory\\shell"), None)
                .unwrap()
                .unwrap();
        assert_eq!(dir_command, r#""C:\Test Folder\zxraw.exe" "%1""#);

        super::unregister_default_folder_handler_at(&root).unwrap();
        assert_eq!(
            super::registry::string_value(&dir_shell, None)
                .unwrap()
                .as_deref(),
            Some("OtherDirectoryVerb")
        );
        assert_eq!(
            super::registry::string_value(&drive_shell, None).unwrap(),
            None
        );
        assert!(!super::registry::key_exists(&drive_shell).unwrap());
        assert_eq!(
            super::registry::string_value(&virtual_shell, None)
                .unwrap()
                .as_deref(),
            Some("VirtualFolderOpen")
        );
        assert!(!super::registry::key_exists(&super::menu_key(&root, "Directory\\shell")).unwrap());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "reversible real-HKCU folder verb and AssocQueryStringW verification"]
    fn real_hkcu_folder_default_resolves_and_restores_exactly() {
        let state = super::folder_state_key(super::ROOT);
        assert_ne!(
            super::registry::string_value(&state, Some("Captured"))
                .unwrap()
                .as_deref(),
            Some("1"),
            "refusing to overwrite an existing real-HKCU folder backup"
        );
        let dir_shell = super::default_folder_shell(super::ROOT, "Directory\\shell");
        let drive_shell = super::default_folder_shell(super::ROOT, "Drive\\shell");
        assert_ne!(
            super::registry::string_value(&dir_shell, None)
                .unwrap()
                .as_deref(),
            Some(super::VERB),
            "refusing to overwrite an already active Directory default"
        );
        assert_ne!(
            super::registry::string_value(&drive_shell, None)
                .unwrap()
                .as_deref(),
            Some(super::VERB),
            "refusing to overwrite an already active Drive default"
        );

        let before = (
            super::registry::key_exists(&dir_shell).unwrap(),
            super::registry::string_value(&dir_shell, None).unwrap(),
            super::registry::key_exists(&drive_shell).unwrap(),
            super::registry::string_value(&drive_shell, None).unwrap(),
            super::registry::key_exists(&super::menu_key(super::ROOT, "Directory\\shell")).unwrap(),
            super::registry::string_value(&super::menu_key(super::ROOT, "Directory\\shell"), None)
                .unwrap(),
            super::registry::string_value(
                &super::command_key(super::ROOT, "Directory\\shell"),
                None,
            )
            .unwrap(),
            super::registry::key_exists(&super::menu_key(super::ROOT, "Drive\\shell")).unwrap(),
            super::registry::string_value(&super::menu_key(super::ROOT, "Drive\\shell"), None)
                .unwrap(),
            super::registry::string_value(&super::command_key(super::ROOT, "Drive\\shell"), None)
                .unwrap(),
        );
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = super::unregister_default_folder_handler();
            }
        }
        let _restore = Restore;

        let current_test = std::env::current_exe().unwrap();
        let app_exe = current_test
            .parent()
            .and_then(std::path::Path::parent)
            .unwrap()
            .join("zxraw.exe");
        assert!(app_exe.exists(), "built ZXraw executable is available");
        super::register_default_folder_handler(&app_exe).unwrap();
        let expected = app_exe.to_string_lossy().to_string();
        for association in ["Directory", "Drive"] {
            assert_eq!(
                super::assoc_query_executable(association)
                    .as_deref()
                    .map(str::to_ascii_lowercase),
                Some(expected.to_ascii_lowercase()),
                "AssocQueryStringW executable for {association}"
            );
        }

        super::unregister_default_folder_handler().unwrap();
        let after = (
            super::registry::key_exists(&dir_shell).unwrap(),
            super::registry::string_value(&dir_shell, None).unwrap(),
            super::registry::key_exists(&drive_shell).unwrap(),
            super::registry::string_value(&drive_shell, None).unwrap(),
            super::registry::key_exists(&super::menu_key(super::ROOT, "Directory\\shell")).unwrap(),
            super::registry::string_value(&super::menu_key(super::ROOT, "Directory\\shell"), None)
                .unwrap(),
            super::registry::string_value(
                &super::command_key(super::ROOT, "Directory\\shell"),
                None,
            )
            .unwrap(),
            super::registry::key_exists(&super::menu_key(super::ROOT, "Drive\\shell")).unwrap(),
            super::registry::string_value(&super::menu_key(super::ROOT, "Drive\\shell"), None)
                .unwrap(),
            super::registry::string_value(&super::command_key(super::ROOT, "Drive\\shell"), None)
                .unwrap(),
        );
        assert_eq!(after, before, "real HKCU folder registry state restored");
        assert!(!super::registry::key_exists(&state).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn command_line_quotes_the_executable_and_uses_shell_directory_value() {
        let exe = std::path::Path::new(r"C:\Test Folder\zxraw.exe");
        let command = super::command_line(exe, "Directory\\Background\\shell").unwrap();
        let command = String::from_utf16(&command[..command.len() - 1]).unwrap();
        assert_eq!(command, r#""C:\Test Folder\zxraw.exe" "%V""#);
        let command = super::command_line(exe, "Directory\\shell").unwrap();
        let command = String::from_utf16(&command[..command.len() - 1]).unwrap();
        assert_eq!(command, r#""C:\Test Folder\zxraw.exe" "%1""#);
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

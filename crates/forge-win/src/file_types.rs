//! Opt-in, reversible, per-user registration of ZXraw Explorer as a handler for photo types.
//!
//! Windows 10 and 11 do not let a program make itself the default app: the user's choice
//! (`...\Explorer\FileExts\.<ext>\UserChoice`) is hash-protected and only Settings writes
//! it. What a program may do, and all this module does, is under `HKCU` only:
//!
//! * a ProgID per extension (`ZXrawExplorer.jpg`) whose open command passes the file;
//! * `Software\Classes\.<ext>\OpenWithProgids`, so it is offered in "Open with";
//! * `Capabilities` + `RegisteredApplications`, so Settings > Default apps lists it and
//!   [`open_default_apps_settings`] can deep-link to its page there;
//! * the per-user class default (`Software\Classes\.<ext>`, "(Default)"). Programs that
//!   read the class default directly instead of asking the shell (common in older
//!   software, and some file pickers) ignore the Settings choice and kept opening the
//!   previous viewer; this makes them open ZXraw Explorer too. A value it replaces is saved
//!   and put back by [`unregister_file_types`].

use std::path::Path;

#[cfg(windows)]
use crate::integration::{registry, shell_command};
use crate::{Result, WinError};

/// The name shown in Settings > Default apps (and the `RegisteredApplications` value name).
#[cfg(windows)]
const APP_NAME: &str = "ZXraw Explorer";
#[cfg(windows)]
const APP_DESCRIPTION: &str = "A fast file manager and viewer for photos, RAW files and video.";
const PROG_ID_PREFIX: &str = "ZXrawExplorer.";

/// One extension to register, e.g. `jpg` described as "JPEG image".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileType {
    /// Without the dot; ASCII letters and digits only.
    pub extension: String,
    /// Shown in Explorer's Type column while ZXraw Explorer is the class default.
    pub description: String,
}

/// What the shell opens a type with, honouring the user's choice in Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultApp {
    /// The app's display name, e.g. "Photos".
    pub name: String,
    /// Whether that is ZXraw Explorer.
    pub is_zxraw: bool,
}

/// Registers `exe` for `types`, replacing an earlier registration (types no longer listed
/// are removed). Only the current user's `HKCU` tree is modified.
pub fn register_file_types(exe: impl AsRef<Path>, types: &[FileType]) -> Result<()> {
    #[cfg(windows)]
    {
        let result = register_at(&Layout::user(), exe.as_ref(), types);
        notify_association_change();
        result
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, types);
        Err(WinError::Unsupported)
    }
}

/// Removes every registration made by [`register_file_types`] and restores the class
/// defaults it replaced.
pub fn unregister_file_types() -> Result<()> {
    #[cfg(windows)]
    {
        let result = unregister_at(&Layout::user());
        notify_association_change();
        result
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

/// Whether the user opted in (some type is registered).
pub fn file_types_registered() -> Result<bool> {
    #[cfg(windows)]
    {
        is_registered_at(&Layout::user())
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

/// Rewrites the registration if the user opted in and this executable moved or the list of
/// types changed (an update). Returns true only when something was rewritten.
pub fn refresh_file_types_if_registered(
    current_exe: impl AsRef<Path>,
    types: &[FileType],
) -> Result<bool> {
    #[cfg(windows)]
    {
        let layout = Layout::user();
        if !is_registered_at(&layout)? || matches_at(&layout, current_exe.as_ref(), types)? {
            return Ok(false);
        }
        register_file_types(current_exe, types)?;
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        let _ = (current_exe, types);
        Err(WinError::Unsupported)
    }
}

/// The app Windows Explorer opens `extension` with (no dot), or `None` if there is none.
pub fn default_app(extension: &str) -> Option<DefaultApp> {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::{ASSOCSTR_FRIENDLYAPPNAME, ASSOCSTR_PROGID};

        let _com = crate::shell::com::ComGuard::apartment_threaded().ok()?;
        let assoc = format!(".{extension}");
        let name = assoc_query(&assoc, ASSOCSTR_FRIENDLYAPPNAME)?;
        let prog_id = assoc_query(&assoc, ASSOCSTR_PROGID).unwrap_or_default();
        Some(DefaultApp {
            name,
            is_zxraw: is_own_prog_id(&prog_id),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = extension;
        None
    }
}

/// Opens Settings > Default apps, on ZXraw Explorer's own page where Windows supports that
/// (Windows 11); there the user confirms with "Set default".
pub fn open_default_apps_settings() -> Result<()> {
    #[cfg(windows)]
    {
        let app_page = format!(
            "ms-settings:defaultapps?registeredAppUser={}",
            APP_NAME.replace(' ', "%20")
        );
        crate::shell::open_default(Path::new(&app_page))
            .or_else(|_| crate::shell::open_default(Path::new("ms-settings:defaultapps")))
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

fn prog_id(extension: &str) -> String {
    format!("{PROG_ID_PREFIX}{extension}")
}

/// Our ProgIDs, or the key Windows creates when the user picked this exe through
/// "Open with > Choose another app".
fn is_own_prog_id(prog_id: &str) -> bool {
    prog_id
        .get(..PROG_ID_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PROG_ID_PREFIX))
        || prog_id.eq_ignore_ascii_case("Applications\\zxraw.exe")
}

/// Lower-cased extension, or an error if it could escape its registry key.
fn checked_extension(extension: &str) -> Result<String> {
    let valid = (1..=16).contains(&extension.len())
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric());
    if valid {
        Ok(extension.to_ascii_lowercase())
    } else {
        Err(WinError::Invalid(format!(
            "not a file extension: {extension:?}"
        )))
    }
}

/// Registry locations, relative to `HKCU`. Tests use an isolated root.
#[cfg(windows)]
struct Layout {
    classes: String,
    app: String,
    registered_apps: String,
}

#[cfg(windows)]
impl Layout {
    fn user() -> Self {
        Self {
            classes: "Software\\Classes".into(),
            app: "Software\\ZXrawExplorer".into(),
            registered_apps: "Software\\RegisteredApplications".into(),
        }
    }

    fn capabilities(&self) -> String {
        format!("{}\\Capabilities", self.app)
    }

    fn associations(&self) -> String {
        format!("{}\\Capabilities\\FileAssociations", self.app)
    }

    /// Class defaults replaced at registration, by `.ext`.
    fn previous_defaults(&self) -> String {
        format!("{}\\PreviousClassDefaults", self.app)
    }

    fn extension_key(&self, extension: &str) -> String {
        format!("{}\\.{extension}", self.classes)
    }

    fn prog_id_key(&self, extension: &str) -> String {
        format!("{}\\{}", self.classes, prog_id(extension))
    }

    fn registered_extensions(&self) -> Result<Vec<String>> {
        Ok(registry::value_names(&self.associations())?
            .iter()
            .filter_map(|name| name.strip_prefix('.'))
            .filter_map(|extension| checked_extension(extension).ok())
            .collect())
    }
}

/// `<exe>,0`, NUL-terminated: the executable's first icon.
#[cfg(windows)]
fn icon_location(exe: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    exe.as_os_str()
        .encode_wide()
        .chain(",0".encode_utf16())
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn register_at(layout: &Layout, exe: &Path, types: &[FileType]) -> Result<()> {
    let command = shell_command(exe, "%1")?;
    let icon = icon_location(exe);
    let wanted = types
        .iter()
        .map(|file_type| checked_extension(&file_type.extension))
        .collect::<Result<Vec<_>>>()?;

    let result = (|| {
        for extension in layout.registered_extensions()? {
            if !wanted.contains(&extension) {
                unregister_extension_at(layout, &extension)?;
            }
        }
        let capabilities = layout.capabilities();
        registry::set_string(&capabilities, Some("ApplicationName"), APP_NAME)?;
        registry::set_string(
            &capabilities,
            Some("ApplicationDescription"),
            APP_DESCRIPTION,
        )?;
        registry::set_wide(&capabilities, Some("ApplicationIcon"), &icon)?;
        for (extension, file_type) in wanted.iter().zip(types) {
            register_extension_at(layout, extension, &file_type.description, &command, &icon)?;
        }
        registry::set_string(&layout.registered_apps, Some(APP_NAME), &capabilities)
    })();
    if result.is_err() {
        // Leave nothing half-registered; replaced class defaults are restored.
        let _ = unregister_at(layout);
    }
    result
}

#[cfg(windows)]
fn register_extension_at(
    layout: &Layout,
    extension: &str,
    description: &str,
    command: &[u16],
    icon: &[u16],
) -> Result<()> {
    let prog_id = prog_id(extension);
    let prog_id_key = layout.prog_id_key(extension);
    registry::set_default_string(&prog_id_key, description)?;
    registry::set_default_wide(&format!("{prog_id_key}\\DefaultIcon"), icon)?;
    registry::set_default_wide(&format!("{prog_id_key}\\shell\\open\\command"), command)?;

    let extension_key = layout.extension_key(extension);
    registry::set_string(
        &format!("{extension_key}\\OpenWithProgids"),
        Some(&prog_id),
        "",
    )?;
    let dotted = format!(".{extension}");
    registry::set_string(&layout.associations(), Some(&dotted), &prog_id)?;

    let current = registry::string_value(&extension_key, None)?;
    if !current
        .as_deref()
        .is_some_and(|current| current.eq_ignore_ascii_case(&prog_id))
    {
        match current.filter(|current| !current.is_empty()) {
            Some(previous) => {
                registry::set_string(&layout.previous_defaults(), Some(&dotted), &previous)?
            }
            None => registry::delete_value(&layout.previous_defaults(), Some(&dotted))?,
        }
        registry::set_default_string(&extension_key, &prog_id)?;
    }
    Ok(())
}

#[cfg(windows)]
fn unregister_at(layout: &Layout) -> Result<()> {
    for extension in layout.registered_extensions()? {
        unregister_extension_at(layout, &extension)?;
    }
    registry::delete_value(&layout.registered_apps, Some(APP_NAME))?;
    registry::delete_tree(&layout.app)
}

#[cfg(windows)]
fn unregister_extension_at(layout: &Layout, extension: &str) -> Result<()> {
    let prog_id = prog_id(extension);
    let dotted = format!(".{extension}");
    let extension_key = layout.extension_key(extension);
    registry::delete_value(&format!("{extension_key}\\OpenWithProgids"), Some(&prog_id))?;
    // Restore the class default only if it is still ours: another app may have taken it.
    let current = registry::string_value(&extension_key, None)?;
    if current
        .as_deref()
        .is_some_and(|current| current.eq_ignore_ascii_case(&prog_id))
    {
        match registry::string_value(&layout.previous_defaults(), Some(&dotted))? {
            Some(previous) if !previous.is_empty() => {
                registry::set_default_string(&extension_key, &previous)?
            }
            _ => registry::delete_value(&extension_key, None)?,
        }
    }
    registry::delete_value(&layout.previous_defaults(), Some(&dotted))?;
    registry::delete_value(&layout.associations(), Some(&dotted))?;
    registry::delete_tree(&layout.prog_id_key(extension))
}

#[cfg(windows)]
fn is_registered_at(layout: &Layout) -> Result<bool> {
    Ok(!layout.registered_extensions()?.is_empty())
}

/// Whether exactly `types` are registered, each opening `exe`.
#[cfg(windows)]
fn matches_at(layout: &Layout, exe: &Path, types: &[FileType]) -> Result<bool> {
    let command = shell_command(exe, "%1")?;
    let command = &command[..command.len() - 1];
    let mut registered = layout.registered_extensions()?;
    let mut wanted = types
        .iter()
        .map(|file_type| checked_extension(&file_type.extension))
        .collect::<Result<Vec<_>>>()?;
    registered.sort();
    wanted.sort();
    wanted.dedup();
    if registered != wanted {
        return Ok(false);
    }
    for extension in &wanted {
        let key = format!("{}\\shell\\open\\command", layout.prog_id_key(extension));
        if registry::default_wide(&key)?.as_deref() != Some(command) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(windows)]
fn notify_association_change() {
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

    // Explorer caches associations and icons; this tells it to re-read them.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

#[cfg(windows)]
fn assoc_query(assoc: &str, what: windows::Win32::UI::Shell::ASSOCSTR) -> Option<String> {
    use windows::Win32::UI::Shell::{
        ASSOCF_INIT_IGNOREUNKNOWN, ASSOCF_NOTRUNCATE, AssocQueryStringW,
    };
    use windows::core::{PCWSTR, PWSTR};

    // Without IGNOREUNKNOWN an unregistered type reports the "pick an app" fallback.
    let flags = ASSOCF_NOTRUNCATE | ASSOCF_INIT_IGNOREUNKNOWN;
    let assoc = crate::to_wide(assoc);
    let mut length = 0_u32;
    // The first call reports the length (including the NUL) with S_FALSE.
    unsafe {
        AssocQueryStringW(
            flags,
            what,
            PCWSTR(assoc.as_ptr()),
            PCWSTR::null(),
            None,
            &mut length,
        )
    }
    .ok()
    .ok()?;
    if length == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; length as usize];
    unsafe {
        AssocQueryStringW(
            flags,
            what,
            PCWSTR(assoc.as_ptr()),
            PCWSTR::null(),
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut length,
        )
    }
    .ok()
    .ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let value = String::from_utf16_lossy(&buffer[..end]);
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_lowercased_and_must_be_alphanumeric() {
        assert_eq!(checked_extension("JPG").unwrap(), "jpg");
        assert_eq!(checked_extension("3fr").unwrap(), "3fr");
        for bad in ["", ".jpg", "a\\b", "jp g", "x".repeat(17).as_str()] {
            assert!(checked_extension(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn own_prog_ids_are_recognised() {
        assert!(is_own_prog_id("ZXrawExplorer.jpg"));
        assert!(is_own_prog_id("zxrawexplorer.CR3"));
        assert!(is_own_prog_id("Applications\\zxraw.exe"));
        assert!(!is_own_prog_id("AppX43hnxtbyyps62jhe9sqpdzxn1790zetc"));
        assert!(!is_own_prog_id("jpegfile"));
    }

    #[cfg(windows)]
    #[test]
    fn default_app_names_the_handler_of_a_stock_type() {
        // Every Windows install opens .txt with something (Notepad unless changed).
        let app = default_app("txt").expect("a .txt handler");
        assert!(!app.name.is_empty());
        assert!(default_app("zxrawnosuchtype").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn registration_lifecycle_restores_previous_class_defaults() {
        use std::time::SystemTime;

        let root = format!(
            "Software\\ZXrawExplorer-FileTypes-Test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = registry::delete_tree(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let layout = Layout {
            classes: format!("{root}\\Classes"),
            app: format!("{root}\\App"),
            registered_apps: format!("{root}\\RegisteredApplications"),
        };
        let types = |extensions: &[&str]| {
            extensions
                .iter()
                .map(|extension| FileType {
                    extension: extension.to_string(),
                    description: format!("{} image", extension.to_uppercase()),
                })
                .collect::<Vec<_>>()
        };
        // .jpg already belongs to another viewer; .png has no class default.
        registry::set_default_string(&layout.extension_key("jpg"), "OtherViewer.jpg").unwrap();
        registry::set_string(
            &format!("{}\\OpenWithProgids", layout.extension_key("jpg")),
            Some("OtherViewer.jpg"),
            "",
        )
        .unwrap();

        let exe = Path::new(r"C:\Test Folder\zxraw.exe");
        assert!(!is_registered_at(&layout).unwrap());
        register_at(&layout, exe, &types(&["jpg", "png"])).unwrap();
        assert!(is_registered_at(&layout).unwrap());
        assert!(matches_at(&layout, exe, &types(&["png", "jpg"])).unwrap());
        assert!(!matches_at(&layout, Path::new(r"D:\zxraw.exe"), &types(&["jpg", "png"])).unwrap());
        assert!(!matches_at(&layout, exe, &types(&["jpg"])).unwrap());

        let default = |extension: &str| {
            registry::string_value(&layout.extension_key(extension), None).unwrap()
        };
        assert_eq!(default("jpg").as_deref(), Some("ZXrawExplorer.jpg"));
        assert_eq!(default("png").as_deref(), Some("ZXrawExplorer.png"));
        let command = registry::string_value(
            &format!("{}\\shell\\open\\command", layout.prog_id_key("jpg")),
            None,
        )
        .unwrap();
        assert_eq!(
            command.as_deref(),
            Some(r#""C:\Test Folder\zxraw.exe" "%1""#)
        );
        assert_eq!(
            registry::string_value(&layout.registered_apps, Some(APP_NAME))
                .unwrap()
                .as_deref(),
            Some(layout.capabilities().as_str())
        );

        // Re-registering with fewer types drops the rest and restores their defaults.
        register_at(&layout, exe, &types(&["jpg"])).unwrap();
        assert_eq!(default("png"), None);
        assert!(!registry::key_exists(&layout.prog_id_key("png")).unwrap());
        assert_eq!(
            layout.registered_extensions().unwrap(),
            vec!["jpg".to_string()]
        );

        unregister_at(&layout).unwrap();
        assert!(!is_registered_at(&layout).unwrap());
        assert_eq!(default("jpg").as_deref(), Some("OtherViewer.jpg"));
        assert_eq!(
            registry::value_names(&format!("{}\\OpenWithProgids", layout.extension_key("jpg")))
                .unwrap(),
            vec!["OtherViewer.jpg".to_string()]
        );
        assert!(!registry::key_exists(&layout.app).unwrap());
        assert_eq!(
            registry::string_value(&layout.registered_apps, Some(APP_NAME)).unwrap(),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn unregistering_keeps_a_class_default_another_app_took_since() {
        use std::time::SystemTime;

        let root = format!(
            "Software\\ZXrawExplorer-FileTypes-Test-{}-{}-b",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = registry::delete_tree(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let layout = Layout {
            classes: format!("{root}\\Classes"),
            app: format!("{root}\\App"),
            registered_apps: format!("{root}\\RegisteredApplications"),
        };
        let types = [FileType {
            extension: "tif".into(),
            description: "TIFF image".into(),
        }];
        register_at(&layout, Path::new(r"C:\zxraw.exe"), &types).unwrap();
        registry::set_default_string(&layout.extension_key("tif"), "LaterApp.tif").unwrap();
        unregister_at(&layout).unwrap();
        assert_eq!(
            registry::string_value(&layout.extension_key("tif"), None)
                .unwrap()
                .as_deref(),
            Some("LaterApp.tif")
        );
    }
}

//! Opt-in, reversible, per-user "ZXraw" cascading menu in Explorer's right-click menu:
//! extract commands on archives, "Add to archive" commands on any file or folder.
//!
//! Classic static verbs (no shell extension DLL), all under `HKCU\Software\Classes`:
//!
//! * `SystemFileAssociations\.<ext>\shell\ZXrawExplorer.Menu` for each archive extension: the
//!   full menu (open, extract, add);
//! * `*\shell\ZXrawExplorer.Menu` (every file, except the archive extensions through
//!   `AppliesTo`, so archives show one ZXraw menu, not two) and `Directory\shell\...`: the
//!   "Add to" commands.
//!
//! Each command is a `DropTarget` verb backed by its own per-user COM class whose
//! `LocalServer32` is `"zxraw.exe" --shell-verb <name>`. With `MultiSelectModel=Player`,
//! Explorer starts one process for the whole selection and drops every selected item on it
//! at once ([`crate::shell_verb_server`]), so 50 selected files make one archive, not 50.
//! A plain `command` verb would start one process per item instead (and hide the verb past
//! 15 items).
//!
//! Windows 11 lists these under "Show more options": a top-level entry in the new menu needs
//! an `IExplorerCommand` with package identity (a sparse MSIX package), which this does not
//! use.

use std::path::Path;

use crate::{Result, WinError};

/// The switch COM starts the menu's local server with (`--shell-verb <name>`, followed by
/// `-Embedding`).
pub const SHELL_VERB_SWITCH: &str = "--shell-verb";

/// Name of the cascading verb key; only ZXraw Explorer uses it.
#[cfg(any(windows, test))]
const MENU_KEY: &str = "ZXrawExplorer.Menu";
#[cfg(any(windows, test))]
const MENU_TITLE: &str = "ZXraw";

/// A command of the ZXraw menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuVerb {
    /// Opens each archive as a folder in a tab.
    Open,
    /// Extracts each archive next to it with the smart-folder rule.
    ExtractHere,
    /// Extracts each archive into a folder named after it.
    ExtractToFolder,
    /// Asks for a folder, then extracts each archive into it.
    ExtractDialog,
    /// Opens the create-archive dialog with the items preselected.
    AddDialog,
    /// Puts the items into one new ZIP next to them.
    AddZip,
    /// Puts the items into one new 7z next to them.
    Add7z,
}

impl MenuVerb {
    pub const ALL: [MenuVerb; 7] = [
        Self::Open,
        Self::ExtractHere,
        Self::ExtractToFolder,
        Self::ExtractDialog,
        Self::AddDialog,
        Self::AddZip,
        Self::Add7z,
    ];

    /// The name used on the command line (`--<name>`), on the instance pipe, and after
    /// [`SHELL_VERB_SWITCH`].
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open-archive",
            Self::ExtractHere => "extract-here",
            Self::ExtractToFolder => "extract-to",
            Self::ExtractDialog => "extract-dialog",
            Self::AddDialog => "add-dialog",
            Self::AddZip => "add-zip",
            Self::Add7z => "add-7z",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|verb| verb.name() == name)
    }

    /// The menu text. Static verbs cannot show the selected item's name, so the ZIP and 7z
    /// commands say what the name will be instead.
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open in ZXraw",
            Self::ExtractHere => "Extract here",
            Self::ExtractToFolder => "Extract to a folder named after the archive",
            Self::ExtractDialog => "Extract to…",
            Self::AddDialog => "Add to archive…",
            Self::AddZip => "Add to \"<name>.zip\"",
            Self::Add7z => "Add to \"<name>.7z\"",
        }
    }

    /// Whether the command is offered on every file and folder, not only on archives.
    pub fn on_everything(self) -> bool {
        matches!(self, Self::AddDialog | Self::AddZip | Self::Add7z)
    }

    /// The COM class whose local server runs this command.
    pub fn clsid(self) -> &'static str {
        match self {
            Self::Open => "{AB314DBF-17E2-4FA6-87C2-7C905D9E48B9}",
            Self::ExtractHere => "{FBFABA5C-357F-438E-8F19-238FE96632A4}",
            Self::ExtractToFolder => "{2B87099A-A007-428D-8635-B4C0591DFBAB}",
            Self::ExtractDialog => "{2890F3EA-705E-445E-8E2D-07C2B4D14EBA}",
            Self::AddDialog => "{066E9532-19F2-45BA-A429-196D0B24F2FC}",
            Self::AddZip => "{C452E1E2-0C6C-4DE3-A91A-A597D5D00FEA}",
            Self::Add7z => "{38CE9404-C93C-4E5D-BCB9-DFB508C222FA}",
        }
    }

    /// Subkey name in the cascade; Explorer sorts the commands by it.
    #[cfg(any(windows, test))]
    fn key(self) -> String {
        let position = Self::ALL.iter().position(|verb| *verb == self).unwrap_or(0);
        format!("{}-{}", position + 1, self.name())
    }
}

/// Registers the ZXraw menu for `exe`: the archive commands on `archive_extensions`
/// (without dots, e.g. `zip`, `gz`), the "Add to" commands on every file and folder.
/// Replaces an earlier registration. Only the current user's `HKCU` tree is modified.
pub fn register_archive_menu(exe: impl AsRef<Path>, archive_extensions: &[&str]) -> Result<()> {
    #[cfg(windows)]
    {
        let result = register_at(ROOT, exe.as_ref(), archive_extensions);
        crate::file_types::notify_association_change();
        result
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, archive_extensions);
        Err(WinError::Unsupported)
    }
}

/// Removes every key [`register_archive_menu`] wrote, for any extension it was written for.
pub fn unregister_archive_menu() -> Result<()> {
    #[cfg(windows)]
    {
        let result = unregister_at(ROOT);
        crate::file_types::notify_association_change();
        result
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

/// Whether the menu is registered (for any executable).
pub fn archive_menu_registered() -> Result<bool> {
    #[cfg(windows)]
    {
        is_registered_at(ROOT)
    }
    #[cfg(not(windows))]
    {
        Err(WinError::Unsupported)
    }
}

/// Rewrites the menu if it is registered but points at another executable or misses an
/// extension (the app moved or was updated). Returns true only when something was rewritten.
pub fn refresh_archive_menu_if_registered(
    current_exe: impl AsRef<Path>,
    archive_extensions: &[&str],
) -> Result<bool> {
    #[cfg(windows)]
    {
        if !is_registered_at(ROOT)? || matches_at(ROOT, current_exe.as_ref(), archive_extensions)? {
            return Ok(false);
        }
        register_archive_menu(current_exe, archive_extensions)?;
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        let _ = (current_exe, archive_extensions);
        Err(WinError::Unsupported)
    }
}

#[cfg(windows)]
use crate::integration::registry;

#[cfg(windows)]
const ROOT: &str = "Software\\Classes";

/// The `shell` keys the cascade is written under, relative to the classes root.
#[cfg(any(windows, test))]
fn everything_locations() -> [&'static str; 2] {
    ["*\\shell", "Directory\\shell"]
}

#[cfg(any(windows, test))]
fn archive_location(extension: &str) -> String {
    format!("SystemFileAssociations\\.{extension}\\shell")
}

/// AQS for `*`: every file except the archive types, which have the full menu.
#[cfg(any(windows, test))]
fn applies_to_non_archives(archive_extensions: &[String]) -> String {
    let terms = archive_extensions
        .iter()
        .map(|extension| format!("System.FileExtension:=.{extension}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!("NOT ({terms})")
}

/// Lower-cased, checked extensions (ASCII letters and digits), deduplicated.
#[cfg(any(windows, test))]
fn checked_extensions(archive_extensions: &[&str]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for extension in archive_extensions {
        let valid = (1..=16).contains(&extension.len())
            && extension.bytes().all(|byte| byte.is_ascii_alphanumeric());
        if !valid {
            return Err(WinError::Invalid(format!("not a file extension: {extension:?}")));
        }
        let lower = extension.to_ascii_lowercase();
        if !out.contains(&lower) {
            out.push(lower);
        }
    }
    Ok(out)
}

/// `"<exe>" --shell-verb <name>`, the local server command of `verb`'s class.
#[cfg(windows)]
fn server_command(exe: &Path, verb: MenuVerb) -> Result<String> {
    let exe = exe.to_str().ok_or_else(|| {
        WinError::Invalid("the executable path is not valid Unicode".into())
    })?;
    if exe.contains('"') {
        return Err(WinError::Invalid("executable path contains a quote".into()));
    }
    Ok(format!("\"{exe}\" {SHELL_VERB_SWITCH} {}", verb.name()))
}

/// One registry string to write: key, value name (`None` = default), data.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Value {
    key: String,
    name: Option<&'static str>,
    data: String,
}

#[cfg(any(windows, test))]
fn value(key: String, name: Option<&'static str>, data: impl Into<String>) -> Value {
    Value {
        key,
        name,
        data: data.into(),
    }
}

/// Everything the menu writes under `root`. `commands` gives each verb's server command.
#[cfg(any(windows, test))]
fn plan(
    root: &str,
    icon: &str,
    commands: &dyn Fn(MenuVerb) -> String,
    archive_extensions: &[String],
) -> Vec<Value> {
    let mut values = Vec::new();
    for verb in MenuVerb::ALL {
        let class = format!("{root}\\CLSID\\{}", verb.clsid());
        values.push(value(
            class.clone(),
            None,
            format!("ZXraw Explorer: {}", verb.label()),
        ));
        values.push(value(format!("{class}\\LocalServer32"), None, commands(verb)));
    }
    let mut cascade = |location: String, verbs: &[MenuVerb], applies_to: Option<String>| {
        let menu = format!("{root}\\{location}\\{MENU_KEY}");
        values.push(value(menu.clone(), Some("MUIVerb"), MENU_TITLE));
        // An empty SubCommands with a `shell` subkey makes the verb a cascading menu.
        values.push(value(menu.clone(), Some("SubCommands"), ""));
        values.push(value(menu.clone(), Some("Icon"), icon));
        if let Some(applies_to) = applies_to {
            values.push(value(menu.clone(), Some("AppliesTo"), applies_to));
        }
        for verb in verbs {
            let command = format!("{menu}\\shell\\{}", verb.key());
            values.push(value(command.clone(), None, verb.label()));
            values.push(value(command.clone(), Some("MUIVerb"), verb.label()));
            // One process for the whole selection, and no 15-item limit.
            values.push(value(command.clone(), Some("MultiSelectModel"), "Player"));
            values.push(value(format!("{command}\\DropTarget"), Some("CLSID"), verb.clsid()));
        }
    };
    let everything: Vec<MenuVerb> = MenuVerb::ALL
        .into_iter()
        .filter(|verb| verb.on_everything())
        .collect();
    for extension in archive_extensions {
        cascade(archive_location(extension), &MenuVerb::ALL, None);
    }
    let [files, folders] = everything_locations();
    cascade(
        files.to_string(),
        &everything,
        (!archive_extensions.is_empty()).then(|| applies_to_non_archives(archive_extensions)),
    );
    cascade(folders.to_string(), &everything, None);
    values
}

#[cfg(windows)]
fn icon_of(exe: &Path) -> String {
    format!("{},0", exe.display())
}

#[cfg(windows)]
fn register_at(root: &str, exe: &Path, archive_extensions: &[&str]) -> Result<()> {
    let extensions = checked_extensions(archive_extensions)?;
    let mut commands = Vec::new();
    for verb in MenuVerb::ALL {
        commands.push((verb, server_command(exe, verb)?));
    }
    let command_of = |verb: MenuVerb| {
        commands
            .iter()
            .find(|(v, _)| *v == verb)
            .map(|(_, command)| command.clone())
            .unwrap_or_default()
    };
    let values = plan(root, &icon_of(exe), &command_of, &extensions);
    // Start clean, so extensions dropped from the list lose their menu.
    unregister_at(root)?;
    let written = values
        .iter()
        .try_for_each(|v| registry::set_string(&v.key, v.name, &v.data));
    if let Err(error) = written {
        let _ = unregister_at(root);
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
fn unregister_at(root: &str) -> Result<()> {
    let associations = format!("{root}\\SystemFileAssociations");
    for name in registry::subkey_names(&associations)? {
        if name.starts_with('.') {
            registry::delete_tree(&format!("{associations}\\{name}\\shell\\{MENU_KEY}"))?;
        }
    }
    for location in everything_locations() {
        registry::delete_tree(&format!("{root}\\{location}\\{MENU_KEY}"))?;
    }
    for verb in MenuVerb::ALL {
        registry::delete_tree(&format!("{root}\\CLSID\\{}", verb.clsid()))?;
    }
    Ok(())
}

#[cfg(windows)]
fn is_registered_at(root: &str) -> Result<bool> {
    registry::key_exists(&format!("{root}\\{}\\{MENU_KEY}", everything_locations()[0]))
}

/// Whether every key of the plan for `exe` and `archive_extensions` holds its value.
#[cfg(windows)]
fn matches_at(root: &str, exe: &Path, archive_extensions: &[&str]) -> Result<bool> {
    let extensions = checked_extensions(archive_extensions)?;
    let mut commands = Vec::new();
    for verb in MenuVerb::ALL {
        commands.push((verb, server_command(exe, verb)?));
    }
    let command_of = |verb: MenuVerb| {
        commands
            .iter()
            .find(|(v, _)| *v == verb)
            .map(|(_, command)| command.clone())
            .unwrap_or_default()
    };
    for v in plan(root, &icon_of(exe), &command_of, &extensions) {
        if registry::string_value(&v.key, v.name)?.as_deref() != Some(v.data.as_str()) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_names_round_trip_and_classes_are_distinct() {
        for verb in MenuVerb::ALL {
            assert_eq!(MenuVerb::from_name(verb.name()), Some(verb));
        }
        assert_eq!(MenuVerb::from_name("format-c"), None);
        let mut classes: Vec<&str> = MenuVerb::ALL.iter().map(|v| v.clsid()).collect();
        classes.sort();
        classes.dedup();
        assert_eq!(classes.len(), MenuVerb::ALL.len());
        assert_eq!(MenuVerb::ExtractHere.key(), "2-extract-here");
    }

    #[test]
    fn applies_to_excludes_every_archive_extension() {
        let aqs = applies_to_non_archives(&["zip".into(), "7z".into()]);
        assert_eq!(
            aqs,
            "NOT (System.FileExtension:=.zip OR System.FileExtension:=.7z)"
        );
        assert!(checked_extensions(&["ZIP", "zip", "gz"]).unwrap() == ["zip", "gz"]);
        assert!(checked_extensions(&[".zip"]).is_err());
        assert!(checked_extensions(&["a\\b"]).is_err());
    }

    #[test]
    fn plan_puts_the_full_menu_on_archives_and_add_commands_everywhere() {
        let values = plan(
            "R",
            "C:\\zxraw.exe,0",
            &|verb| format!("\"C:\\zxraw.exe\" --shell-verb {}", verb.name()),
            &["zip".into(), "gz".into()],
        );
        let get = |key: &str, name: Option<&str>| {
            values
                .iter()
                .find(|v| v.key == key && v.name == name)
                .map(|v| v.data.clone())
        };
        let zip = "R\\SystemFileAssociations\\.zip\\shell\\ZXrawExplorer.Menu";
        assert_eq!(get(zip, Some("MUIVerb")).as_deref(), Some("ZXraw"));
        assert_eq!(get(zip, Some("SubCommands")).as_deref(), Some(""));
        assert_eq!(get(zip, Some("AppliesTo")), None);
        assert_eq!(
            get(&format!("{zip}\\shell\\2-extract-here\\DropTarget"), Some("CLSID")).as_deref(),
            Some(MenuVerb::ExtractHere.clsid())
        );
        assert_eq!(
            get(&format!("{zip}\\shell\\6-add-zip"), Some("MultiSelectModel")).as_deref(),
            Some("Player")
        );
        let files = "R\\*\\shell\\ZXrawExplorer.Menu";
        assert_eq!(
            get(files, Some("AppliesTo")).as_deref(),
            Some("NOT (System.FileExtension:=.zip OR System.FileExtension:=.gz)")
        );
        assert!(get(&format!("{files}\\shell\\2-extract-here"), None).is_none());
        assert_eq!(
            get(&format!("{files}\\shell\\5-add-dialog"), None).as_deref(),
            Some("Add to archive…")
        );
        let folders = "R\\Directory\\shell\\ZXrawExplorer.Menu";
        assert!(get(&format!("{folders}\\shell\\7-add-7z\\DropTarget"), Some("CLSID")).is_some());
        assert!(get(&format!("{folders}\\shell\\1-open-archive"), None).is_none());
        assert_eq!(
            get(
                &format!("R\\CLSID\\{}\\LocalServer32", MenuVerb::AddZip.clsid()),
                None
            )
            .as_deref(),
            Some("\"C:\\zxraw.exe\" --shell-verb add-zip")
        );
    }

    #[cfg(windows)]
    #[test]
    fn registration_lifecycle_in_an_isolated_root() {
        use std::time::SystemTime;

        let root = format!(
            "Software\\ZXrawExplorer-ArchiveMenu-Test-{}-{}",
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
        // Another app's verb on .zip must survive.
        let other = format!("{root}\\SystemFileAssociations\\.zip\\shell\\OtherApp");
        registry::set_default_string(&other, "Other app").unwrap();

        let exe = Path::new(r"C:\Apps\zxraw.exe");
        assert!(!is_registered_at(&root).unwrap());
        register_at(&root, exe, &["zip", "7z"]).unwrap();
        assert!(is_registered_at(&root).unwrap());
        assert!(matches_at(&root, exe, &["zip", "7z"]).unwrap());
        assert!(!matches_at(&root, Path::new(r"D:\zxraw.exe"), &["zip", "7z"]).unwrap());
        assert!(!matches_at(&root, exe, &["zip", "7z", "rar"]).unwrap());
        assert_eq!(
            registry::string_value(
                &format!("{root}\\CLSID\\{}\\LocalServer32", MenuVerb::ExtractHere.clsid()),
                None
            )
            .unwrap()
            .as_deref(),
            Some(r#""C:\Apps\zxraw.exe" --shell-verb extract-here"#)
        );

        // Fewer extensions: the dropped one loses its menu.
        register_at(&root, exe, &["zip"]).unwrap();
        assert!(
            !registry::key_exists(&format!(
                "{root}\\SystemFileAssociations\\.7z\\shell\\{MENU_KEY}"
            ))
            .unwrap()
        );

        unregister_at(&root).unwrap();
        assert!(!is_registered_at(&root).unwrap());
        assert!(
            !registry::key_exists(&format!(
                "{root}\\SystemFileAssociations\\.zip\\shell\\{MENU_KEY}"
            ))
            .unwrap()
        );
        assert!(
            !registry::key_exists(&format!("{root}\\CLSID\\{}", MenuVerb::Open.clsid())).unwrap()
        );
        assert_eq!(
            registry::string_value(&other, None).unwrap().as_deref(),
            Some("Other app")
        );
    }
}

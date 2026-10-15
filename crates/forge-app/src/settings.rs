//! User settings and session state, stored as JSON in the per-user config directory with
//! atomic write-then-rename so a crash mid-save never corrupts the file.

use std::path::{Path, PathBuf};

use forge_core::listing::SortSpec;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Fields of a settings object that this version doesn't know. They are kept and written back,
/// so a setting added by a newer version survives being saved by this one.
pub type Extra = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

/// A setting that falls back to its default when the stored value is not one this version knows
/// (e.g. written by a newer version), instead of failing the whole settings file. The stored
/// value is kept and written back unchanged until the setting is changed here, so saving
/// doesn't overwrite the newer version's choice; any change (even back to the default) drops
/// it, and the user's choice is saved. Reads go through `Deref`; changes go through
/// [`Lenient::set`] or [`Lenient::edit`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lenient<T> {
    value: T,
    unknown: Option<serde_json::Value>,
}

impl<T> Lenient<T> {
    /// Changes the value; an unknown stored value is dropped.
    pub fn set(&mut self, value: T) {
        self.value = value;
        self.unknown = None;
    }

    /// Lets `f` change the value in place (e.g. an egui widget); an unknown stored value is
    /// dropped only when `f` actually changes it.
    pub fn edit<R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R
    where
        T: Clone + PartialEq,
    {
        let mut value = self.value.clone();
        let result = f(&mut value);
        if value != self.value {
            self.set(value);
        }
        result
    }

    /// Is a stored value this version doesn't know still kept?
    pub fn has_unknown(&self) -> bool {
        self.unknown.is_some()
    }

    /// Takes over `other`'s unknown stored value, whatever the values.
    pub fn adopt_unknown(&mut self, other: &Self) {
        self.unknown.clone_from(&other.unknown);
    }

    /// Takes over `old`'s unknown stored value when both hold the same value, i.e. this one
    /// was not changed from it.
    pub fn keep_unknown_from(&mut self, old: &Self)
    where
        T: PartialEq,
    {
        if self.unknown.is_none() && self.value == old.value {
            self.adopt_unknown(old);
        }
    }

    /// Drops the unknown stored value once the value has changed to `now`.
    pub fn forget_unknown_unless(&mut self, now: &T)
    where
        T: PartialEq,
    {
        if self.value != *now {
            self.unknown = None;
        }
    }
}

impl<T> From<T> for Lenient<T> {
    fn from(value: T) -> Self {
        Self {
            value,
            unknown: None,
        }
    }
}

impl<T> std::ops::Deref for Lenient<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: Serialize> Serialize for Lenient<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.unknown {
            Some(raw) => raw.serialize(serializer),
            None => self.value.serialize(serializer),
        }
    }
}

impl<'de, T: DeserializeOwned + Default> Deserialize<'de> for Lenient<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        Ok(match T::deserialize(&raw) {
            Ok(value) => value.into(),
            Err(_) => Self {
                value: T::default(),
                unknown: Some(raw),
            },
        })
    }
}

/// The colour of the app icon's folder-camera mark (see `app_icon`). Independent of the UI
/// accent. Settings files from before the colour choice ("Square" or "Round") load as
/// Graphite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AppIcon {
    #[default]
    #[serde(alias = "Square", alias = "Round")]
    Graphite,
    Blue,
    Steel,
    Sky,
    Teal,
    Green,
    Purple,
    Pink,
    Red,
    Orange,
}

/// The accent colour preset (see `theme::ACCENTS`): selection, focus, indicators and links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Accent {
    #[default]
    Blue,
    Purple,
    Pink,
    Red,
    Orange,
    Green,
    Teal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ViewMode {
    #[default]
    Details,
    Grid,
}

/// What typing a name in the file list does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ListTyping {
    /// Jump to the next name that starts with the typed text.
    #[default]
    Jump,
    /// Add the typed text to the folder filter, so only matching names stay listed.
    Filter,
}

/// An external program the user can send files to ("Open in Lightroom", "Resolve"…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalTool {
    pub name: String,
    pub exe: PathBuf,
    /// Arguments; `{files}` expands to one argument per selected file, `{dir}` to the
    /// current folder. Never passed through a shell.
    pub args: Vec<String>,
    /// Lowercase extensions this tool is offered for; empty = all files.
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Lenient<ThemeChoice>,
    pub ui_scale: f32,
    pub app_icon: Lenient<AppIcon>,
    pub accent: Lenient<Accent>,
    pub show_hidden: bool,
    pub show_system: bool,
    pub confirm_recycle: bool,
    pub default_sort: Lenient<SortSpec>,
    pub default_view: Lenient<ViewMode>,
    pub thumb_size: f32,
    pub thumb_mem_cache_mb: u64,
    pub thumb_disk_cache_mb: u64,
    pub verify_copies: bool,
    /// Run copies and moves one at a time in a queue (`true`, default) or all at once.
    /// Transfers between unrelated physical disks may still run side by side in queue mode.
    pub transfer_queue: bool,
    pub ffmpeg_dir: Option<PathBuf>,
    pub bookmarks: Vec<PathBuf>,
    pub external_tools: Vec<ExternalTool>,
    pub index_roots: Vec<PathBuf>,
    pub index_exclusions: Vec<String>,
    /// Mirror ratings, labels and keywords into `.xmp` sidecars next to the photos.
    pub xmp_sidecar_sync: bool,
    /// URL opened by "Open in maps"; `{lat}` and `{lon}` are replaced. Never fetched by
    /// the app itself: the link is handed to the default browser on an explicit click.
    pub map_url_template: String,
    /// Filename rules associating exports with their originals (see `export_assoc`).
    pub export_rules: Vec<String>,
    /// Ask GitHub once a day whether a newer release exists (see `updater`).
    pub check_updates: bool,
    /// When the last update check started (Unix seconds).
    pub last_update_check: Option<i64>,
    /// A version the user chose to skip: its notice is not shown again.
    pub skipped_update: Option<String>,
    /// Keyboard shortcut overrides by command id (see `commands::Cmd::id`). Absent = the
    /// built-in default; `null` = unbound.
    pub keybindings: crate::commands::Keybindings,
    /// Where the search window looks: "index" (the library index, default), "everything"
    /// (voidtools Everything) or "windows" (Windows Search). Unknown values mean "index".
    pub search_backend: String,
    /// Files and folders pinned to the sidebar's Quick access section, in display order.
    pub pins: Vec<Pin>,
    /// Saved tab layouts ("Save layout…"), in the order they were saved.
    pub layouts: Vec<Layout>,
    /// Reopen the previous session's tabs at startup (the behaviour before this was a
    /// setting, so it defaults to on).
    pub restore_session: bool,
    /// What typing in the file list does: jump to a name (default) or filter the list.
    pub list_typing: Lenient<ListTyping>,
    /// The terminal "Open terminal here" starts (see `terminal`).
    pub terminal: Lenient<crate::terminal::TerminalChoice>,
    /// Show Git status badges and the branch for folders inside a Git repository. Off by
    /// default: a malicious repository can still steer the scan to a network share.
    pub git_badges: bool,
    /// Settings this version doesn't know (written by a newer one), kept so saving doesn't
    /// drop them.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark.into(),
            ui_scale: 1.0,
            app_icon: AppIcon::Graphite.into(),
            accent: Accent::Blue.into(),
            show_hidden: false,
            show_system: false,
            confirm_recycle: false,
            default_sort: SortSpec::default().into(),
            default_view: ViewMode::Details.into(),
            thumb_size: 160.0,
            thumb_mem_cache_mb: 512,
            thumb_disk_cache_mb: 4096,
            verify_copies: false,
            transfer_queue: true,
            ffmpeg_dir: None,
            bookmarks: Vec::new(),
            external_tools: Vec::new(),
            index_roots: Vec::new(),
            index_exclusions: vec!["$RECYCLE.BIN".into(), "System Volume Information".into()],
            xmp_sidecar_sync: false,
            map_url_template: "https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=15/{lat}/{lon}".into(),
            export_rules: crate::export_assoc::default_rules(),
            check_updates: true,
            last_update_check: None,
            skipped_update: None,
            keybindings: Default::default(),
            search_backend: "index".into(),
            pins: Vec::new(),
            layouts: Vec::new(),
            restore_session: true,
            list_typing: ListTyping::Jump.into(),
            terminal: Default::default(),
            git_badges: false,
            extra: Extra::new(),
        }
    }
}

impl Settings {
    /// Upgrades a settings file written by an older version. Folder bookmarks became Quick
    /// access pins: they are moved over once, after any pins that already exist.
    pub fn migrate(&mut self) {
        for path in std::mem::take(&mut self.bookmarks) {
            if !self.pins.iter().any(|p| p.path == path) {
                self.pins.push(Pin {
                    path,
                    ..Default::default()
                });
            }
        }
    }
}

/// A file or folder pinned to the sidebar's Quick access section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Pin {
    pub path: PathBuf,
    /// Shown instead of the file or folder name when set.
    pub label: Option<String>,
    /// Pinned as a file: a click opens it with its default app instead of navigating.
    pub file: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A saved set of tabs, restored by "Load layout".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Layout {
    pub name: String,
    pub tabs: Vec<TabSession>,
    pub active_tab: usize,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One tab of a session or a saved layout. The split is on when `right` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TabSession {
    pub left: PathBuf,
    pub right: Option<PathBuf>,
    pub view: Lenient<ViewMode>,
    /// Absent in files from before layouts: the default sort applies.
    pub sort: Lenient<Option<SortSpec>>,
    /// The right pane's view and sort, when they differ from the left pane's.
    pub right_view: Lenient<Option<ViewMode>>,
    pub right_sort: Lenient<Option<SortSpec>>,
    /// Folders opened in place in this tab's details view.
    pub expanded: Vec<PathBuf>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl TabSession {
    /// Does it hold anything this version doesn't know (an unknown value or field)?
    pub fn has_unknown(&self) -> bool {
        self.has_unknown_values() || !self.extra.is_empty()
    }

    /// Does it hold a value this version doesn't know?
    pub fn has_unknown_values(&self) -> bool {
        self.view.has_unknown()
            || self.sort.has_unknown()
            || self.right_view.has_unknown()
            || self.right_sort.has_unknown()
    }

    /// Takes over everything `loaded` holds that this version doesn't know, whatever the
    /// values: `self` is the same tab as opened here.
    pub fn adopt_unknown(&mut self, loaded: &TabSession) {
        self.view.adopt_unknown(&loaded.view);
        self.sort.adopt_unknown(&loaded.sort);
        self.right_view.adopt_unknown(&loaded.right_view);
        self.right_sort.adopt_unknown(&loaded.right_sort);
        self.keep_extra_from(loaded);
    }

    /// Takes over `old`'s unknown fields, and its unknown values for the fields that still
    /// hold the same value.
    pub fn keep_unknown_from(&mut self, old: &TabSession) {
        self.view.keep_unknown_from(&old.view);
        self.sort.keep_unknown_from(&old.sort);
        self.right_view.keep_unknown_from(&old.right_view);
        self.right_sort.keep_unknown_from(&old.right_sort);
        self.keep_extra_from(old);
    }

    /// Drops the unknown values of the fields that changed to `now`'s.
    pub fn forget_changed(&mut self, now: &TabSession) {
        if !self.has_unknown_values() {
            return;
        }
        self.view.forget_unknown_unless(&now.view);
        self.sort.forget_unknown_unless(&now.sort);
        self.right_view.forget_unknown_unless(&now.right_view);
        self.right_sort.forget_unknown_unless(&now.right_sort);
    }

    fn keep_extra_from(&mut self, old: &TabSession) {
        for (key, value) in &old.extra {
            self.extra.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Session {
    pub tabs: Vec<TabSession>,
    pub active_tab: usize,
    pub show_preview: bool,
    pub show_sidebar: bool,
}

pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        let pd = directories::ProjectDirs::from("", "", "ZXraw Explorer");
        // Automated UI runs (screenshot hook) must never touch the user's real settings,
        // session or library: they get a throwaway profile unless one is given explicitly.
        #[cfg(any(debug_assertions, feature = "automation"))]
        let profile = std::env::var_os("ZXRAW_PROFILE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                crate::testhook::env_requests_automation().then(|| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos();
                    std::env::temp_dir()
                        .join(format!("zxraw-test-profile-{}-{now}", std::process::id()))
                })
            });
        #[cfg(not(any(debug_assertions, feature = "automation")))]
        let profile: Option<PathBuf> = None;
        let (config, data, cache) = match (&profile, &pd) {
            (Some(root), _) => (root.join("config"), root.join("data"), root.join("cache")),
            (None, pd) => match pd {
                Some(p) => (
                    p.config_dir().to_path_buf(),
                    p.data_local_dir().to_path_buf(),
                    p.cache_dir().to_path_buf(),
                ),
                None => {
                    let t = std::env::temp_dir().join("ZXraw Explorer");
                    (t.join("config"), t.join("data"), t.join("cache"))
                }
            },
        };
        let logs = data.join("logs");
        for d in [&config, &data, &cache, &logs] {
            let _ = std::fs::create_dir_all(d);
        }
        Self {
            config,
            data,
            cache,
            logs,
        }
    }
}

pub fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable {}: {e}", path.display());
            // Keep the broken file for inspection instead of overwriting it silently.
            let _ = std::fs::rename(path, path.with_extension("json.bad"));
            T::default()
        }),
        Err(_) => T::default(),
    }
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_square_and_round_icon_settings_load_as_graphite() {
        for old in ["\"Square\"", "\"Round\""] {
            let icon: AppIcon = serde_json::from_str(old).unwrap();
            assert_eq!(icon, AppIcon::Graphite, "{old}");
        }
        let settings: Settings = serde_json::from_str(r#"{"app_icon":"Round"}"#).unwrap();
        assert_eq!(*settings.app_icon, AppIcon::Graphite);
        let settings: Settings = serde_json::from_str(r#"{"app_icon":"Steel"}"#).unwrap();
        assert_eq!(*settings.app_icon, AppIcon::Steel);
    }

    #[test]
    fn unknown_app_icon_or_accent_keeps_the_rest_of_the_settings() {
        let json = r#"{
            "app_icon": "Holographic",
            "accent": "Chartreuse",
            "bookmarks": ["C:\\Photos"],
            "keybindings": {"NewTab": "Ctrl+Shift+T"}
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(*settings.app_icon, AppIcon::Graphite);
        assert_eq!(*settings.accent, Accent::Blue);
        assert_eq!(settings.bookmarks, vec![PathBuf::from(r"C:\Photos")]);
        assert_eq!(settings.keybindings.get("NewTab"), Some(&Some("Ctrl+Shift+T".to_string())));
    }

    #[test]
    fn settings_without_transfer_queue_default_to_queue() {
        let settings: Settings = serde_json::from_str(r#"{"verify_copies": true}"#).unwrap();
        assert!(settings.transfer_queue);
        assert!(settings.verify_copies);
        let settings: Settings = serde_json::from_str(r#"{"transfer_queue": false}"#).unwrap();
        assert!(!settings.transfer_queue);
    }

    #[test]
    fn pins_layouts_and_restore_flag_round_trip() {
        let mut settings = Settings {
            restore_session: false,
            ..Default::default()
        };
        settings.pins = vec![
            Pin { path: PathBuf::from(r"C:\Shoots"), label: None, file: false, ..Default::default() },
            Pin { path: PathBuf::from(r"C:\Shoots\cover.cr3"), label: Some("Cover".into()), file: true, ..Default::default() },
        ];
        settings.layouts = vec![Layout {
            name: "Culling".into(),
            tabs: vec![TabSession {
                left: PathBuf::from(r"C:\Shoots"),
                right: Some(PathBuf::from(r"D:\Backup")),
                view: ViewMode::Grid.into(),
                sort: Some(SortSpec { descending: true, ..Default::default() }).into(),
                right_view: Some(ViewMode::Details).into(),
                right_sort: None.into(),
                expanded: Vec::new(),
                extra: Default::default(),
            }],
            active_tab: 0,
            extra: Default::default(),
        }];
        let json = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pins, settings.pins);
        assert_eq!(back.layouts, settings.layouts);
        assert!(!back.restore_session);
    }

    #[test]
    fn old_settings_without_pins_or_layouts_load_with_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"theme":"Light","show_hidden":true}"#).unwrap();
        assert!(settings.pins.is_empty());
        assert!(settings.layouts.is_empty());
        assert!(settings.restore_session, "session restore stays on for existing users");
    }

    #[test]
    fn bookmarks_migrate_into_pins_once_after_existing_pins() {
        let json = r#"{
            "bookmarks": ["C:\\Photos", "D:\\Card"],
            "pins": [{"path": "D:\\Card", "label": "SD", "file": false}]
        }"#;
        let mut settings: Settings = serde_json::from_str(json).unwrap();
        settings.migrate();
        let paths: Vec<_> = settings.pins.iter().map(|p| p.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from(r"D:\Card"), PathBuf::from(r"C:\Photos")]);
        assert_eq!(settings.pins[0].label.as_deref(), Some("SD"));
        assert!(!settings.pins[1].file);
        assert!(settings.bookmarks.is_empty());
        let before = settings.pins.clone();
        settings.migrate();
        assert_eq!(settings.pins, before);
    }

    #[test]
    fn old_session_tabs_load_without_sort_or_right_view() {
        let json = r#"{"tabs":[{"left":"C:\\A","right":"C:\\B","view":"Grid"}],"active_tab":0}"#;
        let session: Session = serde_json::from_str(json).unwrap();
        let tab = &session.tabs[0];
        assert_eq!(tab.right.as_deref(), Some(Path::new(r"C:\B")));
        assert_eq!(*tab.view, ViewMode::Grid);
        assert_eq!((*tab.sort, *tab.right_view, *tab.right_sort), (None, None, None));
    }

    #[test]
    fn a_pin_with_missing_fields_still_loads() {
        let settings: Settings =
            serde_json::from_str(r#"{"pins":[{"path":"C:\\X"},{"label":"no path"}]}"#).unwrap();
        assert_eq!(settings.pins[0], Pin { path: PathBuf::from(r"C:\X"), label: None, file: false, ..Default::default() });
        assert_eq!(settings.pins.len(), 2);
    }

    #[test]
    fn unknown_fields_from_a_newer_version_survive_a_save() {
        let json = r#"{
            "theme": "Light",
            "network_locations": [{"name": "NAS", "path": "//nas/photos"}],
            "pins": [{"path": "C:/Shoots", "colour": "red"}],
            "layouts": [{"name": "Cull", "pinned": true,
                         "tabs": [{"left": "C:/Shoots", "zoom": 2}]}]
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(*settings.theme, ThemeChoice::Light);
        assert_eq!(settings.pins[0].path, PathBuf::from("C:/Shoots"));
        let saved: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(saved["network_locations"][0]["name"], "NAS");
        assert_eq!(saved["pins"][0]["colour"], "red");
        assert_eq!(saved["layouts"][0]["pinned"], true);
        assert_eq!(saved["layouts"][0]["tabs"][0]["zoom"], 2);
        assert_eq!(saved["theme"], "Light");
    }

    #[test]
    fn unknown_enum_values_from_a_newer_version_keep_the_rest_of_the_file() {
        let json = r#"{
            "theme": "Sepia",
            "default_view": "Filmstrip",
            "default_sort": {"key": "Colour", "descending": true, "dirs_first": true},
            "pins": [{"path": "C:/Shoots"}],
            "layouts": [{"name": "Cull", "tabs": [{
                "left": "C:/Shoots", "right": "D:/Backup", "view": "Filmstrip",
                "right_view": "Timeline", "sort": {"key": "Colour"}
            }]}]
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(*settings.theme, ThemeChoice::Dark);
        assert_eq!(*settings.default_view, ViewMode::Details);
        assert_eq!(*settings.default_sort, SortSpec::default());
        assert_eq!(settings.pins.len(), 1);
        let tab = &settings.layouts[0].tabs[0];
        assert_eq!(tab.left, PathBuf::from("C:/Shoots"));
        assert_eq!(tab.right, Some(PathBuf::from("D:/Backup")));
        assert_eq!((*tab.view, *tab.right_view, *tab.sort), (ViewMode::Details, None, None));
    }

    #[test]
    fn unknown_enum_values_are_saved_back_unchanged_until_the_user_changes_them() {
        let json = r#"{
            "theme": "Sepia",
            "accent": "Chartreuse",
            "default_view": "Filmstrip",
            "default_sort": {"key": "Colour", "descending": true, "dirs_first": true},
            "layouts": [{"name": "Cull", "tabs": [{
                "left": "C:/Shoots", "view": "Filmstrip", "right_view": "Timeline",
                "sort": {"key": "Colour"}
            }]}]
        }"#;
        let mut settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(*settings.theme, ThemeChoice::Dark);
        let saved: serde_json::Value = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["theme"], "Sepia");
        assert_eq!(saved["accent"], "Chartreuse");
        assert_eq!(saved["default_view"], "Filmstrip");
        assert_eq!(saved["default_sort"]["key"], "Colour");
        let tab = &saved["layouts"][0]["tabs"][0];
        assert_eq!((&tab["view"], &tab["right_view"]), (&"Filmstrip".into(), &"Timeline".into()));
        assert_eq!(tab["sort"]["key"], "Colour");
        assert_eq!(tab["right_sort"], serde_json::Value::Null);
        // Saving and loading again changes nothing.
        let again: Settings = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(serde_json::to_value(&again).unwrap(), saved);

        // A setting the user changes in this version is saved as they chose it.
        settings.theme.set(ThemeChoice::Light);
        settings.accent.set(Accent::Teal);
        let saved: serde_json::Value = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["theme"], "Light");
        assert_eq!(saved["accent"], "Teal");
        assert_eq!(saved["default_view"], "Filmstrip");
    }

    #[test]
    fn git_badges_are_off_unless_turned_on() {
        assert!(!Settings::default().git_badges);
        let settings: Settings = serde_json::from_str(r#"{"verify_copies": true}"#).unwrap();
        assert!(!settings.git_badges);
        let settings: Settings = serde_json::from_str(r#"{"git_badges": true}"#).unwrap();
        assert!(settings.git_badges);
    }

    #[test]
    fn changing_an_unknown_setting_and_back_to_the_default_saves_the_default() {
        let mut settings: Settings =
            serde_json::from_str(r#"{"theme": "Sepia", "default_view": "Filmstrip"}"#).unwrap();
        assert_eq!(*settings.theme, ThemeChoice::Dark);
        settings.theme.set(ThemeChoice::Light);
        settings.theme.set(ThemeChoice::Dark);
        // An edit that changes nothing (a widget drawn each frame) keeps the stored value.
        settings.default_view.edit(|_| ());
        let saved: serde_json::Value = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["theme"], "Dark");
        assert_eq!(saved["default_view"], "Filmstrip");
        settings.default_view.edit(|v| *v = ViewMode::Grid);
        settings.default_view.edit(|v| *v = ViewMode::Details);
        let saved: serde_json::Value = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["default_view"], "Details");
    }

    #[test]
    fn non_string_app_icon_falls_back_to_default() {
        let settings: Settings = serde_json::from_str(r#"{"app_icon": 7, "accent": null}"#).unwrap();
        assert_eq!(*settings.app_icon, AppIcon::Graphite);
        assert_eq!(*settings.accent, Accent::Blue);
    }
}

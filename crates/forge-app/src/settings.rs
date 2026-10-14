//! User settings and session state, stored as JSON in the per-user config directory with
//! atomic write-then-rename so a crash mid-save never corrupts the file.

use std::path::{Path, PathBuf};

use forge_core::listing::SortSpec;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

/// Deserializes a setting that falls back to its default when the stored value is not one this
/// version knows (e.g. written by a newer version), instead of failing the whole settings file.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
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
    pub theme: ThemeChoice,
    pub ui_scale: f32,
    #[serde(deserialize_with = "lenient")]
    pub app_icon: AppIcon,
    #[serde(deserialize_with = "lenient")]
    pub accent: Accent,
    pub show_hidden: bool,
    pub show_system: bool,
    pub confirm_recycle: bool,
    pub default_sort: SortSpec,
    pub default_view: ViewMode,
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            ui_scale: 1.0,
            app_icon: AppIcon::Graphite,
            accent: Accent::Blue,
            show_hidden: false,
            show_system: false,
            confirm_recycle: false,
            default_sort: SortSpec::default(),
            default_view: ViewMode::Details,
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
                    label: None,
                    file: false,
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
}

/// A saved set of tabs, restored by "Load layout".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Layout {
    pub name: String,
    pub tabs: Vec<TabSession>,
    pub active_tab: usize,
}

/// One tab of a session or a saved layout. The split is on when `right` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TabSession {
    pub left: PathBuf,
    pub right: Option<PathBuf>,
    pub view: ViewMode,
    /// Absent in files from before layouts: the default sort applies.
    pub sort: Option<SortSpec>,
    /// The right pane's view and sort, when they differ from the left pane's.
    pub right_view: Option<ViewMode>,
    pub right_sort: Option<SortSpec>,
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
        assert_eq!(settings.app_icon, AppIcon::Graphite);
        let settings: Settings = serde_json::from_str(r#"{"app_icon":"Steel"}"#).unwrap();
        assert_eq!(settings.app_icon, AppIcon::Steel);
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
        assert_eq!(settings.app_icon, AppIcon::Graphite);
        assert_eq!(settings.accent, Accent::Blue);
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
            Pin { path: PathBuf::from(r"C:\Shoots"), label: None, file: false },
            Pin { path: PathBuf::from(r"C:\Shoots\cover.cr3"), label: Some("Cover".into()), file: true },
        ];
        settings.layouts = vec![Layout {
            name: "Culling".into(),
            tabs: vec![TabSession {
                left: PathBuf::from(r"C:\Shoots"),
                right: Some(PathBuf::from(r"D:\Backup")),
                view: ViewMode::Grid,
                sort: Some(SortSpec { descending: true, ..Default::default() }),
                right_view: Some(ViewMode::Details),
                right_sort: None,
            }],
            active_tab: 0,
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
        assert_eq!(tab.view, ViewMode::Grid);
        assert_eq!((tab.sort, tab.right_view, tab.right_sort), (None, None, None));
    }

    #[test]
    fn a_pin_with_missing_fields_still_loads() {
        let settings: Settings =
            serde_json::from_str(r#"{"pins":[{"path":"C:\\X"},{"label":"no path"}]}"#).unwrap();
        assert_eq!(settings.pins[0], Pin { path: PathBuf::from(r"C:\X"), label: None, file: false });
        assert_eq!(settings.pins.len(), 2);
    }

    #[test]
    fn non_string_app_icon_falls_back_to_default() {
        let settings: Settings = serde_json::from_str(r#"{"app_icon": 7, "accent": null}"#).unwrap();
        assert_eq!(settings.app_icon, AppIcon::Graphite);
        assert_eq!(settings.accent, Accent::Blue);
    }
}

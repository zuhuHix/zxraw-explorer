//! User settings and session state, stored as JSON in the per-user config directory with
//! atomic write-then-rename so a crash mid-save never corrupts the file.

use std::path::{Path, PathBuf};

use forge_core::listing::SortSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
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
    pub app_icon: AppIcon,
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TabSession {
    pub left: PathBuf,
    pub right: Option<PathBuf>,
    pub view: ViewMode,
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
}

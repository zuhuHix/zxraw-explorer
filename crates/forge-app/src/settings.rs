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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            ui_scale: 1.0,
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
        let profile = std::env::var_os("ZXRAW_PROFILE_DIR").map(PathBuf::from).or_else(|| {
            crate::testhook::env_requests_automation()
                .then(|| std::env::temp_dir().join("zxraw-test-profile"))
        });
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

//! Default apps through the freedesktop.org MIME Apps mechanism (`mimeapps.list`), and the
//! user's applications menu entry for runs that are not installed (AppImage, a build folder).
//!
//! Everything here is plain file work, written only to the user's own directories
//! (`$XDG_CONFIG_HOME`, `$XDG_DATA_HOME`); nothing under `/usr` or `/etc` is touched. Every
//! function takes an [`XdgDirs`], so the tests run against temp folders. [`XdgDirs::from_env`]
//! returns `None` off Linux, which is what keeps the feature Linux-only.
//!
//! * Lookup follows the MIME Apps spec: `$desktop-mimeapps.list` before `mimeapps.list`, in
//!   `$XDG_CONFIG_HOME`, `$XDG_CONFIG_DIRS`, `$XDG_DATA_HOME/applications`, then
//!   `$XDG_DATA_DIRS/applications`; within a `[Default Applications]` value the first desktop
//!   id that is installed wins, else the next file is tried. The legacy `defaults.list` of the
//!   data dirs comes last, as GIO reads it.
//! * [`set_defaults`] writes `$XDG_CONFIG_HOME/mimeapps.list` (and any user-level
//!   `$desktop-mimeapps.list` that already sets the type, since that one would win). Before a
//!   type is taken over, what each file said for it is recorded in a small file the caller
//!   names, and [`restore_defaults`] puts exactly that back. Files are replaced atomically
//!   (temp file + rename) and every other line is kept as it was.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::{Result, WinError};

/// The desktop file id of ZXraw Explorer, from the .deb or [`install_menu_entry`].
pub const DESKTOP_ID: &str = "zxraw.desktop";

/// Marks a menu entry this module wrote, so only that one is ever replaced or removed.
const GENERATED_KEY: &str = "X-ZXraw-Generated";

const DEFAULTS: &str = "Default Applications";
#[cfg(test)]
const ADDED: &str = "Added Associations";
const REMOVED: &str = "Removed Associations";

/// The XDG base directories and the current desktops, as the MIME Apps spec uses them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XdgDirs {
    pub config_home: PathBuf,
    pub config_dirs: Vec<PathBuf>,
    pub data_home: PathBuf,
    pub data_dirs: Vec<PathBuf>,
    /// `XDG_CURRENT_DESKTOP` entries, lowercased (`x-cinnamon`, `gnome`).
    pub desktops: Vec<String>,
}

impl XdgDirs {
    /// The directories from the environment, with the spec's defaults. `None` off Linux and
    /// when there is no home folder.
    pub fn from_env() -> Option<Self> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)?;
        let absolute = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let list = |name: &str, default: &str| -> Vec<PathBuf> {
            let value = std::env::var(name).ok().filter(|v| !v.is_empty());
            value
                .as_deref()
                .unwrap_or(default)
                .split(':')
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .collect()
        };
        Some(Self {
            config_home: absolute("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            config_dirs: list("XDG_CONFIG_DIRS", "/etc/xdg"),
            data_home: absolute("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
            data_dirs: list("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
            desktops: std::env::var("XDG_CURRENT_DESKTOP")
                .unwrap_or_default()
                .split(':')
                .filter(|d| !d.is_empty())
                .map(str::to_ascii_lowercase)
                .collect(),
        })
    }

    /// The user's own `mimeapps.list`, the one [`set_defaults`] writes.
    pub fn user_mimeapps(&self) -> PathBuf {
        self.config_home.join("mimeapps.list")
    }

    /// The user's applications folder, where [`install_menu_entry`] writes.
    pub fn user_applications(&self) -> PathBuf {
        self.data_home.join("applications")
    }

    fn application_dirs(&self) -> impl Iterator<Item = PathBuf> + '_ {
        std::iter::once(&self.data_home)
            .chain(&self.data_dirs)
            .map(|dir| dir.join("applications"))
    }

    /// Every `mimeapps.list`, highest precedence first.
    fn mimeapps_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let config = std::iter::once(&self.config_home)
            .chain(&self.config_dirs)
            .cloned();
        for dir in config.chain(self.application_dirs()) {
            for desktop in &self.desktops {
                files.push(dir.join(format!("{desktop}-mimeapps.list")));
            }
            files.push(dir.join("mimeapps.list"));
        }
        files
    }

    /// The installed desktop file for `id`, if any. A `Hidden=true` entry counts as removed.
    /// `vendor-app.desktop` may also be `vendor/app.desktop`.
    pub fn find_desktop_file(&self, id: &str) -> Option<PathBuf> {
        if !is_desktop_id(id) {
            return None;
        }
        let mut names = vec![PathBuf::from(id)];
        for (i, _) in id.match_indices('-') {
            names.push(PathBuf::from(&id[..i]).join(&id[i + 1..]));
        }
        for dir in self.application_dirs() {
            for name in &names {
                let path = dir.join(name);
                if path.is_file() {
                    let hidden = fs::read_to_string(&path).is_ok_and(|text| {
                        entry_value(&text, "Hidden").is_some_and(|v| v.trim() == "true")
                    });
                    // The first match shadows the rest, so a hidden one hides the id.
                    return (!hidden).then_some(path);
                }
            }
        }
        None
    }
}

fn is_desktop_id(id: &str) -> bool {
    id.ends_with(".desktop") && !id.contains('/') && !id.starts_with('.') && id.len() > ".desktop".len()
}

/// The app that opens `mime`: its desktop id and its `Name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultHandler {
    pub desktop_id: String,
    pub name: String,
}

impl DefaultHandler {
    pub fn is_zxraw(&self) -> bool {
        self.desktop_id == DESKTOP_ID
    }
}

/// The default app for `mime`, or `None` when no list names an installed one (the desktop
/// then picks one itself from the apps that support the type).
pub fn default_for(dirs: &XdgDirs, mime: &str) -> Option<DefaultHandler> {
    defaults_for(dirs, &[mime]).pop().flatten()
}

/// [`default_for`] for several types, reading each list once.
pub fn defaults_for(dirs: &XdgDirs, mimes: &[&str]) -> Vec<Option<DefaultHandler>> {
    let legacy = std::iter::once(&dirs.data_home)
        .chain(&dirs.data_dirs)
        .map(|dir| dir.join("applications/defaults.list"));
    let lists: Vec<KeyFile> = dirs
        .mimeapps_files()
        .into_iter()
        .chain(legacy)
        .filter_map(|file| fs::read_to_string(file).ok())
        .map(|text| KeyFile::parse(&text))
        .collect();
    let mut found: BTreeMap<String, Option<DefaultHandler>> = BTreeMap::new();
    let mut handler = |id: &str| -> Option<DefaultHandler> {
        found
            .entry(id.to_string())
            .or_insert_with(|| {
                let path = dirs.find_desktop_file(id)?;
                let name = fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| entry_value(&text, "Name"))
                    .map(|name| unescape(&name))
                    .unwrap_or_else(|| id.trim_end_matches(".desktop").to_string());
                Some(DefaultHandler {
                    desktop_id: id.to_string(),
                    name,
                })
            })
            .clone()
    };
    mimes
        .iter()
        .map(|mime| {
            lists.iter().find_map(|list| {
                let value = list.get(DEFAULTS, mime)?;
                split_ids(&value).find_map(&mut handler)
            })
        })
        .collect()
}

/// Makes ZXraw Explorer the default for every type in `mimes`. Refuses while no
/// `zxraw.desktop` is installed, so no type is ever pointed at a missing entry. What each
/// changed file said before is added to `record` unless it already holds the value from
/// before ZXraw took the type over.
pub fn set_defaults(dirs: &XdgDirs, mimes: &[&str], record: &Path) -> Result<()> {
    if dirs.find_desktop_file(DESKTOP_ID).is_none() {
        return Err(WinError::Invalid(
            "zxraw.desktop is not installed; add ZXraw Explorer to the applications menu first".into(),
        ));
    }
    let mut records = Records::load(record)?;
    let mut targets = vec![dirs.user_mimeapps()];
    for desktop in &dirs.desktops {
        targets.push(dirs.config_home.join(format!("{desktop}-mimeapps.list")));
    }
    for (index, file) in targets.iter().enumerate() {
        let main = index == 0;
        let mut keys = match fs::read_to_string(file) {
            Ok(text) => KeyFile::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !main {
                    continue;
                }
                KeyFile::default()
            }
            Err(source) => return Err(io_err("read mimeapps.list", file, source)),
        };
        let mut changed = false;
        for &mime in mimes {
            let current = keys.get(DEFAULTS, mime);
            // A desktop-specific file is changed only where it would override ours.
            if !main && current.is_none() {
                continue;
            }
            let ours = current
                .as_deref()
                .is_some_and(|value| split_ids(value).next() == Some(DESKTOP_ID));
            if !(ours && records.contains(mime, file)) {
                records.insert(mime, file, current.clone());
            }
            let others = current.as_deref().map(split_ids).into_iter().flatten();
            let value: String = std::iter::once(DESKTOP_ID)
                .chain(others.filter(|id| *id != DESKTOP_ID))
                .map(|id| format!("{id};"))
                .collect();
            if current.as_deref() != Some(value.as_str()) {
                keys.set(DEFAULTS, mime, Some(&value));
                changed = true;
            }
            // An explicit "not this app" for the type would hide the default in some desktops.
            if let Some(removed) = keys.get(REMOVED, mime)
                && split_ids(&removed).any(|id| id == DESKTOP_ID)
            {
                let rest = join_ids(split_ids(&removed).filter(|id| *id != DESKTOP_ID));
                keys.set(REMOVED, mime, rest.as_deref());
                changed = true;
            }
        }
        // The record is saved first: a crash in between leaves a record for a value that was
        // not written yet, which restore then ignores (the type is not ours).
        records.save(record)?;
        if changed {
            write_atomic(file, keys.render().as_bytes())?;
        }
    }
    Ok(())
}

/// Turns ZXraw Explorer off for every type in `mimes`: puts back what [`set_defaults`]
/// recorded, leaving out apps that are no longer installed, or removes the entry when there
/// was none (so the system default applies). A type the user has since given to another app
/// is left alone. Types ZXraw opens without a record lose ZXraw from their entry.
pub fn restore_defaults(dirs: &XdgDirs, mimes: &[&str], record: &Path) -> Result<()> {
    let mut records = Records::load(record)?;
    let mut files: BTreeMap<PathBuf, Vec<(String, Option<Option<String>>)>> = BTreeMap::new();
    for &mime in mimes {
        let recorded = records.take(mime);
        let main = dirs.user_mimeapps();
        if !recorded.iter().any(|(file, _)| *file == main) {
            files.entry(main).or_default().push((mime.to_string(), None));
        }
        for (file, previous) in recorded {
            files.entry(file).or_default().push((mime.to_string(), Some(previous)));
        }
    }
    for (file, entries) in files {
        let mut keys = match fs::read_to_string(&file) {
            Ok(text) => KeyFile::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_err("read mimeapps.list", &file, source)),
        };
        let mut changed = false;
        for (mime, previous) in entries {
            let Some(current) = keys.get(DEFAULTS, &mime) else {
                continue;
            };
            if split_ids(&current).next() != Some(DESKTOP_ID) {
                continue;
            }
            // Recorded: exactly what was there. Unrecorded: the entry without ZXraw.
            let base = match &previous {
                Some(previous) => previous.clone(),
                None => Some(current.clone()),
            };
            let usable = |id: &&str| *id != DESKTOP_ID && dirs.find_desktop_file(id).is_some();
            let restored = base.as_deref().and_then(|value| {
                if previous.is_some() && split_ids(value).all(|id| usable(&id)) {
                    // Everything recorded is still there: put the line back as it was.
                    Some(value.to_string()).filter(|v| split_ids(v).next().is_some())
                } else {
                    join_ids(split_ids(value).filter(usable))
                }
            });
            keys.set(DEFAULTS, &mime, restored.as_deref());
            changed = true;
        }
        if changed {
            write_atomic(&file, keys.render().as_bytes())?;
        }
    }
    records.save(record)
}

/// What the applications menu has for ZXraw Explorer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// No `zxraw.desktop` is installed.
    Missing,
    /// Installed by a package or by the user themselves: never changed here.
    Installed(PathBuf),
    /// Written by [`install_menu_entry`]. `current` is false when its `Exec` no longer
    /// starts the running copy (the AppImage moved).
    Ours { path: PathBuf, current: bool },
}

/// The menu entry for ZXraw Explorer, compared against `exe` (see [`launcher_path`]).
pub fn menu_entry(dirs: &XdgDirs, exe: &Path) -> MenuEntry {
    let Some(path) = dirs.find_desktop_file(DESKTOP_ID) else {
        return MenuEntry::Missing;
    };
    let text = fs::read_to_string(&path).unwrap_or_default();
    let generated = entry_value(&text, GENERATED_KEY).is_some_and(|v| v.trim() == "true");
    if !generated || path != dirs.user_applications().join(DESKTOP_ID) {
        return MenuEntry::Installed(path);
    }
    let current = entry_value(&text, "Exec").is_some_and(|exec| exec == exec_line(exe));
    MenuEntry::Ours { path, current }
}

/// The program a menu entry should start: the AppImage file itself when running from one
/// (`$APPIMAGE`, never the temporary mount the process runs from), else this executable.
pub fn launcher_path() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_file())
        .or_else(|| std::env::current_exe().ok())
}

/// The `zxraw.desktop` text for a copy that is started as `exe`: the packaged entry's
/// fields, plus the marker that lets [`menu_entry`] recognise it.
pub fn desktop_entry_text(exe: &Path, mime_types: &[&str]) -> String {
    let mimes: String = mime_types.iter().map(|m| format!("{m};")).collect();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.5\n\
         Name=ZXraw Explorer\n\
         GenericName=Photo and RAW browser\n\
         Comment=Browse, view and organise photos and RAW files\n\
         Exec={}\n\
         Icon=zxraw\n\
         Terminal=false\n\
         StartupNotify=true\n\
         StartupWMClass=zxraw\n\
         Categories=Graphics;Photography;\n\
         Keywords=photo;raw;image;viewer;camera;\n\
         MimeType={mimes}\n\
         {GENERATED_KEY}=true\n",
        exec_line(exe)
    )
}

/// `Exec` for `exe`: the path quoted per the Desktop Entry spec, then `%f`.
fn exec_line(exe: &Path) -> String {
    format!("{} %f", quote_exec_arg(&exe.to_string_lossy()))
}

/// One `Exec` argument, already escaped for the key file: always in double quotes, with `"`,
/// `` ` ``, `$` and `\` backslash-escaped inside them, `%` doubled, and then each backslash
/// doubled again as a string value needs.
pub fn quote_exec_arg(arg: &str) -> String {
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(c);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Writes the menu entry for `exe` and the icons (`(size, png)`, into
/// `$XDG_DATA_HOME/icons/hicolor/<size>x<size>/apps/zxraw.png`), then refreshes the desktop
/// and icon caches when their tools are there. Refuses to replace an entry it did not write.
pub fn install_menu_entry(
    dirs: &XdgDirs,
    exe: &Path,
    mime_types: &[&str],
    icons: &[(u32, &[u8])],
) -> Result<PathBuf> {
    let path = dirs.user_applications().join(DESKTOP_ID);
    if path.exists() && !matches!(menu_entry(dirs, exe), MenuEntry::Ours { .. }) {
        let generated = fs::read_to_string(&path)
            .is_ok_and(|text| entry_value(&text, GENERATED_KEY).is_some_and(|v| v.trim() == "true"));
        if !generated {
            return Err(WinError::Invalid(format!(
                "{} exists and was not written by ZXraw Explorer",
                path.display()
            )));
        }
    }
    for (size, png) in icons {
        let icon = icon_path(dirs, *size);
        write_atomic(&icon, png)?;
    }
    write_atomic(&path, desktop_entry_text(exe, mime_types).as_bytes())?;
    refresh_caches(dirs);
    Ok(path)
}

/// Removes the entry and icons [`install_menu_entry`] wrote. The caller restores the
/// defaults first: a type left pointing at `zxraw.desktop` would point at nothing.
pub fn remove_menu_entry(dirs: &XdgDirs, icon_sizes: &[u32]) -> Result<()> {
    let path = dirs.user_applications().join(DESKTOP_ID);
    let generated = fs::read_to_string(&path)
        .is_ok_and(|text| entry_value(&text, GENERATED_KEY).is_some_and(|v| v.trim() == "true"));
    if !generated {
        return Ok(());
    }
    fs::remove_file(&path).map_err(|source| io_err("remove the menu entry", &path, source))?;
    for size in icon_sizes {
        let icon = icon_path(dirs, *size);
        match fs::remove_file(&icon) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_err("remove the icon", &icon, source)),
        }
    }
    refresh_caches(dirs);
    Ok(())
}

fn icon_path(dirs: &XdgDirs, size: u32) -> PathBuf {
    dirs.data_home
        .join(format!("icons/hicolor/{size}x{size}/apps/zxraw.png"))
}

/// Best effort: menus pick the change up without these too, only later.
fn refresh_caches(dirs: &XdgDirs) {
    let run = |program: &str, args: &[&std::ffi::OsStr]| {
        match std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if !status.success() => tracing::debug!("{program} exited with {status}"),
            Ok(_) => {}
            Err(error) => tracing::debug!("{program} not run: {error}"),
        }
    };
    let applications = dirs.user_applications();
    run("update-desktop-database", &[applications.as_os_str()]);
    let icons = dirs.data_home.join("icons/hicolor");
    if icons.is_dir() {
        run(
            "gtk-update-icon-cache",
            &["-f".as_ref(), "-t".as_ref(), "-q".as_ref(), icons.as_os_str()],
        );
    }
}

fn split_ids(value: &str) -> impl Iterator<Item = &str> {
    value.split(';').map(str::trim).filter(|id| !id.is_empty())
}

fn join_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Option<String> {
    let value: String = ids.map(|id| format!("{id};")).collect();
    (!value.is_empty()).then_some(value)
}

/// A `[Desktop Entry]` key's value as written (escapes kept, so `Exec` compares exactly).
fn entry_value(text: &str, key: &str) -> Option<String> {
    KeyFile::parse(text).get("Desktop Entry", key)
}

/// A string value with its escapes (`\s`, `\n`, `\t`, `\r`, `\\`) resolved, for display.
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A key file as lines, so a rewrite keeps comments, order and unknown keys as they were.
#[derive(Debug, Default, Clone)]
struct KeyFile {
    lines: Vec<Line>,
}

#[derive(Debug, Clone)]
enum Line {
    Group(String),
    Entry { key: String, value: String },
    Other(String),
}

impl KeyFile {
    fn parse(text: &str) -> Self {
        let lines = text
            .lines()
            .map(|line| {
                let trimmed = line.trim();
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    Line::Group(trimmed[1..trimmed.len() - 1].to_string())
                } else if !trimmed.starts_with('#')
                    && let Some((key, value)) = line.split_once('=')
                {
                    Line::Entry {
                        key: key.trim().to_string(),
                        value: value.trim_start().to_string(),
                    }
                } else {
                    Line::Other(line.to_string())
                }
            })
            .collect();
        Self { lines }
    }

    /// The first value of `key` in the first `[group]` that has it (a duplicate group's keys
    /// count as part of the group, as GLib reads them).
    fn get(&self, group: &str, key: &str) -> Option<String> {
        let mut inside = false;
        for line in &self.lines {
            match line {
                Line::Group(name) => inside = name == group,
                Line::Entry { key: k, value } if inside && k == key => return Some(value.clone()),
                _ => {}
            }
        }
        None
    }

    /// Sets (or with `None` removes) `key` in `group`, adding the group at the end when it is
    /// missing. Every other copy of the key in that group is removed, so the file is not
    /// left with two values for one type.
    fn set(&mut self, group: &str, key: &str, value: Option<&str>) {
        let mut inside = false;
        let mut first_group_end = None;
        let mut placed = false;
        let mut keep = Vec::with_capacity(self.lines.len() + 2);
        for line in self.lines.drain(..) {
            match &line {
                Line::Group(name) => {
                    if inside && first_group_end.is_none() {
                        first_group_end = Some(keep.len());
                    }
                    inside = name == group;
                }
                Line::Entry { key: k, .. } if inside && k == key => {
                    if let (false, Some(value)) = (placed, value) {
                        keep.push(Line::Entry {
                            key: key.to_string(),
                            value: value.to_string(),
                        });
                    }
                    placed = true;
                    continue;
                }
                _ => {}
            }
            keep.push(line);
        }
        if inside && first_group_end.is_none() {
            first_group_end = Some(keep.len());
        }
        self.lines = keep;
        let (false, Some(value)) = (placed, value) else {
            return;
        };
        let entry = Line::Entry {
            key: key.to_string(),
            value: value.to_string(),
        };
        match first_group_end {
            Some(mut at) => {
                // After the group's last entry, before trailing blank lines.
                while at > 0 && matches!(&self.lines[at - 1], Line::Other(t) if t.trim().is_empty()) {
                    at -= 1;
                }
                self.lines.insert(at, entry);
            }
            None => {
                if self.lines.last().is_some_and(|l| !matches!(l, Line::Other(t) if t.trim().is_empty())) {
                    self.lines.push(Line::Other(String::new()));
                }
                self.lines.push(Line::Group(group.to_string()));
                self.lines.push(entry);
            }
        }
    }

    fn render(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            match line {
                Line::Group(name) => text.push_str(&format!("[{name}]")),
                Line::Entry { key, value } => text.push_str(&format!("{key}={value}")),
                Line::Other(raw) => text.push_str(raw),
            }
            text.push('\n');
        }
        text
    }
}

/// What each file said for a type before ZXraw took it over. One line per (type, file):
/// `type<TAB>file<TAB>=value`, or `!` for "no entry".
#[derive(Debug, Default)]
struct Records {
    entries: Vec<(String, PathBuf, Option<String>)>,
}

impl Records {
    fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(io_err("read the default-app record", path, source)),
        };
        let entries = text
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let mime = parts.next()?.to_string();
                let file = PathBuf::from(parts.next()?);
                let previous = match parts.next()? {
                    "!" => None,
                    value => Some(value.strip_prefix('=')?.to_string()),
                };
                Some((mime, file, previous))
            })
            .collect();
        Ok(Self { entries })
    }

    fn contains(&self, mime: &str, file: &Path) -> bool {
        self.entries.iter().any(|(m, f, _)| m == mime && f == file)
    }

    fn insert(&mut self, mime: &str, file: &Path, previous: Option<String>) {
        self.entries.retain(|(m, f, _)| !(m == mime && f == file));
        self.entries.push((mime.to_string(), file.to_path_buf(), previous));
    }

    fn take(&mut self, mime: &str) -> Vec<(PathBuf, Option<String>)> {
        let (taken, kept) = self.entries.drain(..).partition(|(m, _, _)| m == mime);
        self.entries = kept;
        taken.into_iter().map(|(_, file, previous)| (file, previous)).collect()
    }

    fn save(&self, path: &Path) -> Result<()> {
        if self.entries.is_empty() {
            return match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(io_err("remove the default-app record", path, source)),
            };
        }
        let mut text = String::new();
        for (mime, file, previous) in &self.entries {
            let value = match previous {
                Some(value) => format!("={value}"),
                None => "!".to_string(),
            };
            text.push_str(&format!("{mime}\t{}\t{value}\n", file.display()));
        }
        write_atomic(path, text.as_bytes())
    }
}

/// Replaces `path` with `bytes` through a temp file in the same folder and a rename, so a
/// reader never sees half a file. Follows a symlinked `mimeapps.list` to its target.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .ok_or_else(|| WinError::Invalid(format!("no folder for {}", target.display())))?;
    fs::create_dir_all(dir).map_err(|source| io_err("create folder", dir, source))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(".{name}.zxraw-{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&temp, meta.permissions())?;
        }
        fs::rename(&temp, &target)
    })();
    result.map_err(|source| {
        let _ = fs::remove_file(&temp);
        io_err("write", &target, source)
    })
}

fn io_err(op: &'static str, path: &Path, source: std::io::Error) -> WinError {
    WinError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Env {
        _root: tempfile::TempDir,
        dirs: XdgDirs,
        record: PathBuf,
    }

    fn env(desktops: &[&str]) -> Env {
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        let dirs = XdgDirs {
            config_home: p.join("config"),
            config_dirs: vec![p.join("etc-xdg")],
            data_home: p.join("data"),
            data_dirs: vec![p.join("usr-local-share"), p.join("usr-share")],
            desktops: desktops.iter().map(|d| d.to_string()).collect(),
        };
        let record = p.join("profile/linux-default-apps.tsv");
        Env { _root: root, dirs, record }
    }

    fn app(dir: &Path, id: &str, name: &str) {
        let path = dir.join("applications").join(id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("[Desktop Entry]\nType=Application\nName={name}\nExec={name}\n")).unwrap();
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    fn id(dirs: &XdgDirs, mime: &str) -> Option<String> {
        default_for(dirs, mime).map(|h| h.desktop_id)
    }

    #[test]
    fn precedence_follows_the_mime_apps_spec() {
        let e = env(&["x-cinnamon"]);
        let d = &e.dirs;
        app(&d.data_dirs[1], "nemo.desktop", "Files");
        app(&d.data_dirs[1], "xviewer.desktop", "Image Viewer");
        app(&d.data_home, "gimp.desktop", "GIMP");
        // System defaults, then a desktop-specific system file that wins over them.
        write(&d.data_dirs[1].join("applications/mimeapps.list"), "[Default Applications]\nimage/jpeg=gimp.desktop\n");
        write(&d.data_dirs[1].join("applications/x-cinnamon-mimeapps.list"), "[Default Applications]\nimage/jpeg=xviewer.desktop\n");
        assert_eq!(id(d, "image/jpeg").as_deref(), Some("xviewer.desktop"));
        assert_eq!(default_for(d, "image/jpeg").unwrap().name, "Image Viewer");
        // /etc/xdg beats every data dir; a missing app falls through to the next id.
        write(&d.config_dirs[0].join("mimeapps.list"), "[Default Applications]\nimage/jpeg=gone.desktop;gimp.desktop;\n");
        assert_eq!(id(d, "image/jpeg").as_deref(), Some("gimp.desktop"));
        // A list naming only missing apps falls through to the next file.
        write(&d.user_mimeapps(), "[Default Applications]\nimage/jpeg=gone.desktop\n");
        assert_eq!(id(d, "image/jpeg").as_deref(), Some("gimp.desktop"));
        // Legacy defaults.list comes last.
        write(&d.data_dirs[1].join("applications/defaults.list"), "[Default Applications]\ninode/directory=nemo.desktop\n");
        assert_eq!(id(d, "inode/directory").as_deref(), Some("nemo.desktop"));
        assert_eq!(id(d, "image/png"), None);
        // Hidden=true hides an id; vendor-app.desktop may live in vendor/app.desktop.
        write(&d.data_home.join("applications/gimp.desktop"), "[Desktop Entry]\nName=GIMP\nHidden=true\n");
        assert_eq!(id(d, "image/jpeg").as_deref(), Some("xviewer.desktop"));
        write(&d.data_dirs[0].join("applications/kde/okular.desktop"), "[Desktop Entry]\nName=Okular\n");
        assert!(d.find_desktop_file("kde-okular.desktop").is_some());
    }

    #[test]
    fn set_and_restore_round_trip_keeps_other_entries() {
        let e = env(&[]);
        let d = &e.dirs;
        app(&d.data_dirs[1], "xviewer.desktop", "Image Viewer");
        app(&d.data_dirs[1], "nemo.desktop", "Files");
        assert!(set_defaults(d, &["image/jpeg"], &e.record).is_err(), "refused before zxraw.desktop exists");
        app(&d.data_home, DESKTOP_ID, "ZXraw");
        let original = "# my comment\n[Default Applications]\nimage/jpeg=xviewer.desktop;\ntext/plain=gedit.desktop\n\n[Added Associations]\nimage/jpeg=xviewer.desktop;gimp.desktop;\n\n[Removed Associations]\nimage/png=zxraw.desktop;\n";
        write(&d.user_mimeapps(), original);
        set_defaults(d, &["image/jpeg", "image/png", "inode/directory"], &e.record).unwrap();
        for mime in ["image/jpeg", "image/png", "inode/directory"] {
            assert_eq!(id(d, mime).as_deref(), Some(DESKTOP_ID), "{mime}");
        }
        let text = read(&d.user_mimeapps());
        assert!(text.starts_with("# my comment\n[Default Applications]\nimage/jpeg=zxraw.desktop;xviewer.desktop;\n"), "{text}");
        assert!(text.contains("text/plain=gedit.desktop\n"));
        assert!(text.contains("[Added Associations]\nimage/jpeg=xviewer.desktop;gimp.desktop;\n"));
        assert!(!text.contains("image/png=zxraw.desktop;\n\n") && !text.contains("[Removed Associations]\nimage/png"), "{text}");
        // Setting again keeps the original record.
        set_defaults(d, &["image/jpeg"], &e.record).unwrap();
        restore_defaults(d, &["image/jpeg", "image/png", "inode/directory"], &e.record).unwrap();
        assert_eq!(id(d, "image/jpeg").as_deref(), Some("xviewer.desktop"));
        assert_eq!(id(d, "image/png"), None);
        assert_eq!(id(d, "inode/directory"), None);
        let text = read(&d.user_mimeapps());
        assert!(text.contains("[Default Applications]\nimage/jpeg=xviewer.desktop;\ntext/plain=gedit.desktop\n"), "{text}");
        assert!(!text.contains("zxraw"), "{text}");
        assert!(!e.record.exists(), "record removed once empty");
    }

    #[test]
    fn missing_file_is_created_and_removed_entries_restore_to_system_default() {
        let e = env(&[]);
        let d = &e.dirs;
        app(&d.data_home, DESKTOP_ID, "ZXraw");
        app(&d.data_dirs[1], "nemo.desktop", "Files");
        write(&d.data_dirs[1].join("applications/mimeapps.list"), "[Default Applications]\ninode/directory=nemo.desktop\n");
        set_defaults(d, &["inode/directory"], &e.record).unwrap();
        assert_eq!(read(&d.user_mimeapps()), "[Default Applications]\ninode/directory=zxraw.desktop;\n");
        restore_defaults(d, &["inode/directory"], &e.record).unwrap();
        assert_eq!(read(&d.user_mimeapps()), "[Default Applications]\n");
        assert_eq!(id(d, "inode/directory").as_deref(), Some("nemo.desktop"));
    }

    #[test]
    fn restore_never_points_at_an_uninstalled_app_or_undoes_a_later_choice() {
        let e = env(&[]);
        let d = &e.dirs;
        app(&d.data_home, DESKTOP_ID, "ZXraw");
        app(&d.data_dirs[1], "old.desktop", "Old");
        app(&d.data_dirs[1], "gimp.desktop", "GIMP");
        write(&d.user_mimeapps(), "[Default Applications]\nimage/jpeg=old.desktop;\nimage/png=old.desktop;gimp.desktop;\n");
        set_defaults(d, &["image/jpeg", "image/png", "image/gif"], &e.record).unwrap();
        fs::remove_file(d.data_dirs[1].join("applications/old.desktop")).unwrap();
        // The user picks another app for gif elsewhere in the meantime.
        let text = read(&d.user_mimeapps()).replace("image/gif=zxraw.desktop;", "image/gif=gimp.desktop;");
        write(&d.user_mimeapps(), &text);
        restore_defaults(d, &["image/jpeg", "image/png", "image/gif"], &e.record).unwrap();
        let text = read(&d.user_mimeapps());
        assert!(!text.contains("image/jpeg="), "{text}");
        assert!(text.contains("image/png=gimp.desktop;\n"), "{text}");
        assert!(text.contains("image/gif=gimp.desktop;\n"), "{text}");
    }

    #[test]
    fn a_desktop_specific_override_is_taken_over_and_restored() {
        let e = env(&["x-cinnamon"]);
        let d = &e.dirs;
        app(&d.data_home, DESKTOP_ID, "ZXraw");
        app(&d.data_dirs[1], "xviewer.desktop", "Image Viewer");
        let cinnamon = d.config_home.join("x-cinnamon-mimeapps.list");
        write(&cinnamon, "[Default Applications]\nimage/jpeg=xviewer.desktop\nimage/png=xviewer.desktop\n");
        set_defaults(d, &["image/jpeg"], &e.record).unwrap();
        assert_eq!(id(d, "image/jpeg").as_deref(), Some(DESKTOP_ID));
        assert_eq!(id(d, "image/png").as_deref(), Some("xviewer.desktop"));
        restore_defaults(d, &["image/jpeg"], &e.record).unwrap();
        assert_eq!(read(&cinnamon), "[Default Applications]\nimage/jpeg=xviewer.desktop\nimage/png=xviewer.desktop\n");
        assert_eq!(id(d, "image/jpeg").as_deref(), Some("xviewer.desktop"));
    }

    #[test]
    fn unrecorded_type_loses_only_zxraw() {
        let e = env(&[]);
        let d = &e.dirs;
        app(&d.data_home, DESKTOP_ID, "ZXraw");
        app(&d.data_dirs[1], "gimp.desktop", "GIMP");
        write(&d.user_mimeapps(), "[Default Applications]\nimage/tiff=zxraw.desktop;gimp.desktop;\n");
        restore_defaults(d, &["image/tiff"], &e.record).unwrap();
        assert_eq!(read(&d.user_mimeapps()), "[Default Applications]\nimage/tiff=gimp.desktop;\n");
    }

    #[test]
    fn exec_quoting_follows_the_desktop_entry_spec() {
        assert_eq!(quote_exec_arg("/opt/ZXraw.AppImage"), "\"/opt/ZXraw.AppImage\"");
        assert_eq!(quote_exec_arg("/home/a b/100%/$x\"`"), "\"/home/a b/100%%/\\\\$x\\\\\"\\\\`\"");
        assert_eq!(quote_exec_arg("/a\\b"), "\"/a\\\\\\\\b\"");
    }

    #[test]
    fn menu_entry_install_detects_moves_and_removes_only_its_own() {
        let e = env(&[]);
        let d = &e.dirs;
        let exe = Path::new("/home/me/Apps/ZXraw Explorer.AppImage");
        assert_eq!(menu_entry(d, exe), MenuEntry::Missing);
        let path = install_menu_entry(d, exe, &["image/jpeg", "inode/directory"], &[(48, b"png48"), (256, b"png256")]).unwrap();
        assert_eq!(path, d.user_applications().join(DESKTOP_ID));
        let text = read(&path);
        assert!(text.contains("Exec=\"/home/me/Apps/ZXraw Explorer.AppImage\" %f\n"), "{text}");
        assert!(text.contains("MimeType=image/jpeg;inode/directory;\n"));
        assert!(text.contains("Icon=zxraw\n"));
        assert_eq!(fs::read(d.data_home.join("icons/hicolor/48x48/apps/zxraw.png")).unwrap(), b"png48");
        assert!(matches!(menu_entry(d, exe), MenuEntry::Ours { current: true, .. }));
        assert!(matches!(menu_entry(d, Path::new("/elsewhere/ZXraw.AppImage")), MenuEntry::Ours { current: false, .. }));
        // Update after a move.
        install_menu_entry(d, Path::new("/elsewhere/ZXraw.AppImage"), &["image/jpeg"], &[]).unwrap();
        assert!(matches!(menu_entry(d, Path::new("/elsewhere/ZXraw.AppImage")), MenuEntry::Ours { current: true, .. }));
        remove_menu_entry(d, &[48, 256]).unwrap();
        assert!(!path.exists());
        assert!(!d.data_home.join("icons/hicolor/256x256/apps/zxraw.png").exists());
        assert_eq!(menu_entry(d, exe), MenuEntry::Missing);
        // A user's own entry, or a package's, is never replaced or removed.
        write(&path, "[Desktop Entry]\nName=Mine\nExec=zxraw %f\n");
        assert!(matches!(menu_entry(d, exe), MenuEntry::Installed(_)));
        assert!(install_menu_entry(d, exe, &[], &[]).is_err());
        remove_menu_entry(d, &[]).unwrap();
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
        app(&d.data_dirs[1], DESKTOP_ID, "ZXraw");
        assert!(matches!(menu_entry(d, exe), MenuEntry::Installed(p) if p.starts_with(&d.data_dirs[1])));
    }

    #[test]
    fn key_file_set_handles_duplicates_and_missing_groups() {
        let mut k = KeyFile::parse("[Default Applications]\na=1\na=2\n\n[Other]\nb=3\n");
        k.set(DEFAULTS, "a", Some("x"));
        k.set(DEFAULTS, "c", Some("y"));
        k.set(ADDED, "d", Some("z"));
        assert_eq!(k.render(), "[Default Applications]\na=x\nc=y\n\n[Other]\nb=3\n\n[Added Associations]\nd=z\n");
        k.set(DEFAULTS, "a", None);
        assert_eq!(k.get(DEFAULTS, "a"), None);
    }
}

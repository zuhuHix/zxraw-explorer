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
//!   names, and [`restore_defaults`] puts exactly that back. A record file that is not
//!   exactly as written is refused whole. Files are replaced atomically (temp file + rename),
//!   every other line is kept as it was, and a file another program saved in between is
//!   edited again from its new content.
//! * The generated menu entry uses its own icon name ([`ICON_NAME`]), so no `zxraw.png` of a
//!   package or the user is replaced.

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
    /// The app supports the type: its `MimeType` lists it, or an `[Added Associations]` entry
    /// does. Some desktops skip a default that does not.
    pub supports: bool,
}

impl DefaultHandler {
    /// ZXraw Explorer is the default and supports the type.
    pub fn is_zxraw(&self) -> bool {
        self.desktop_id == DESKTOP_ID && self.supports
    }
}

/// The default app for `mime`, or `None` when no list names an installed one (the desktop
/// then picks one itself from the apps that support the type).
pub fn default_for(dirs: &XdgDirs, mime: &str) -> Option<DefaultHandler> {
    defaults_for(dirs, &[mime]).pop().flatten()
}

/// The `Name` and `MimeType` list of an installed desktop file.
fn app_info(dirs: &XdgDirs, id: &str) -> Option<(String, Vec<String>)> {
    let path = dirs.find_desktop_file(id)?;
    let text = fs::read_to_string(&path).unwrap_or_default();
    let name = entry_value(&text, "Name")
        .map(|name| unescape(&name))
        .unwrap_or_else(|| id.trim_end_matches(".desktop").to_string());
    Some((name, listed_types(&text)))
}

fn listed_types(desktop_entry: &str) -> Vec<String> {
    entry_value(desktop_entry, "MimeType")
        .map(|value| split_ids(&value).map(str::to_string).collect())
        .unwrap_or_default()
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
    let mut apps: BTreeMap<String, Option<(String, Vec<String>)>> = BTreeMap::new();
    mimes
        .iter()
        .map(|&mime| {
            lists.iter().find_map(|list| {
                let value = list.get(DEFAULTS, mime)?;
                split_ids(&value).find_map(|id| {
                    let (name, types) = apps
                        .entry(id.to_string())
                        .or_insert_with(|| app_info(dirs, id))
                        .clone()?;
                    let added = lists.iter().any(|list| {
                        list.get(ADDED, mime)
                            .is_some_and(|value| split_ids(&value).any(|a| a == id))
                    });
                    Some(DefaultHandler {
                        desktop_id: id.to_string(),
                        name,
                        supports: added || types.iter().any(|t| t == mime),
                    })
                })
            })
        })
        .collect()
}

/// Makes ZXraw Explorer the default for every type in `mimes`. Refuses while no
/// `zxraw.desktop` is installed, so no type is ever pointed at a missing entry.
///
/// In each changed file, and recorded first so [`restore_defaults`] can undo exactly that:
/// * the `[Default Applications]` value becomes ZXraw first, then what was there;
/// * a type the installed `zxraw.desktop` does not list (an older or a custom entry) gets
///   ZXraw under `[Added Associations]`, so the default counts as supported;
/// * ZXraw is taken out of the type's `[Removed Associations]`.
///
/// A value already recorded from before ZXraw took the type over is kept.
pub fn set_defaults(dirs: &XdgDirs, mimes: &[&str], record: &Path) -> Result<()> {
    let Some(entry) = dirs.find_desktop_file(DESKTOP_ID) else {
        return Err(WinError::Invalid(
            "zxraw.desktop is not installed; add ZXraw Explorer to the applications menu first".into(),
        ));
    };
    let listed = listed_types(&fs::read_to_string(&entry).unwrap_or_default());
    let mut records = Records::load(record, dirs)?;
    let mut targets = vec![dirs.user_mimeapps()];
    for desktop in &dirs.desktops {
        targets.push(dirs.config_home.join(format!("{desktop}-mimeapps.list")));
    }
    for (index, file) in targets.iter().enumerate() {
        let main = index == 0;
        // A desktop-specific file is changed only where it would override ours.
        if !main && !file.exists() {
            continue;
        }
        let edited = edit_file(
            file,
            |keys| {
                let mut recs = records.clone();
                let mut changed = false;
                for &mime in mimes {
                    let current = keys.get(DEFAULTS, mime);
                    if !main && current.is_none() {
                        continue;
                    }
                    let ours = current
                        .as_deref()
                        .is_some_and(|value| split_ids(value).next() == Some(DESKTOP_ID));
                    if !(ours && recs.get(Kind::Default, mime, file).is_some()) {
                        recs.insert(Kind::Default, mime, file, current.clone());
                    }
                    let others = current.as_deref().map(split_ids).into_iter().flatten();
                    let value = join_ids(
                        std::iter::once(DESKTOP_ID).chain(others.filter(|id| *id != DESKTOP_ID)),
                    );
                    if current != value {
                        keys.set(DEFAULTS, mime, value.as_deref());
                        changed = true;
                    }
                    if !listed.iter().any(|t| t == mime) {
                        let added = keys.get(ADDED, mime);
                        if !added.as_deref().is_some_and(|v| split_ids(v).any(|id| id == DESKTOP_ID)) {
                            if recs.get(Kind::Added, mime, file).is_none() {
                                recs.insert(Kind::Added, mime, file, added.clone());
                            }
                            let ids = added.as_deref().map(split_ids).into_iter().flatten();
                            keys.set(ADDED, mime, join_ids(ids.chain([DESKTOP_ID])).as_deref());
                            changed = true;
                        }
                    }
                    // An explicit "not this app" would hide the default in some desktops.
                    if let Some(removed) = keys.get(REMOVED, mime)
                        && split_ids(&removed).any(|id| id == DESKTOP_ID)
                    {
                        if recs.get(Kind::Removed, mime, file).is_none() {
                            recs.insert(Kind::Removed, mime, file, Some(removed.clone()));
                        }
                        let rest = join_ids(split_ids(&removed).filter(|id| *id != DESKTOP_ID));
                        keys.set(REMOVED, mime, rest.as_deref());
                        changed = true;
                    }
                }
                Ok(changed.then_some(recs))
            },
            // Recorded before the file is replaced: a crash in between leaves a record of a
            // change not made, which restore ignores (the type is not ZXraw's).
            |recs| recs.save(record),
        )?;
        if let Some(recs) = edited {
            records = recs;
        }
    }
    Ok(())
}

/// Turns ZXraw Explorer off for every type in `mimes`, undoing what [`set_defaults`]
/// recorded: the previous default (leaving out apps that are no longer installed, or no entry
/// at all so the system default applies), ZXraw's `[Added Associations]` addition, and its
/// `[Removed Associations]` entry. A type the user has since given to another app keeps that
/// app. A type ZXraw opens without a record loses ZXraw from its entry. A file's records are
/// dropped only once that file was written. A damaged record file refuses the whole restore
/// ([`is_damaged_record`]).
pub fn restore_defaults(dirs: &XdgDirs, mimes: &[&str], record: &Path) -> Result<()> {
    let mut records = Records::load(record, dirs)?;
    let main = dirs.user_mimeapps();
    let mut files = vec![main.clone()];
    for r in &records.entries {
        if mimes.contains(&r.mime.as_str()) && !files.contains(&r.file) {
            files.push(r.file.clone());
        }
    }
    for file in files {
        if file.exists() {
            edit_file(
                &file,
                |keys| {
                    let mut changed = false;
                    for &mime in mimes {
                        changed |= restore_type(dirs, keys, &records, mime, &file, file == main);
                    }
                    Ok(changed.then_some(()))
                },
                |_| Ok(()),
            )?;
        }
        records
            .entries
            .retain(|r| !(r.file == file && mimes.contains(&r.mime.as_str())));
        records.save(record)?;
    }
    Ok(())
}

/// One type in one file for [`restore_defaults`]. Returns whether `keys` changed.
fn restore_type(dirs: &XdgDirs, keys: &mut KeyFile, records: &Records, mime: &str, file: &Path, main: bool) -> bool {
    let mut changed = false;
    let recorded = records.get(Kind::Default, mime, file).map(|r| r.previous.clone());
    if let Some(current) = keys.get(DEFAULTS, mime)
        && split_ids(&current).next() == Some(DESKTOP_ID)
        && (recorded.is_some() || main)
    {
        // Recorded: what was there. Unrecorded: the entry without ZXraw.
        let base = recorded.clone().unwrap_or_else(|| Some(current.clone()));
        let usable = |id: &&str| *id != DESKTOP_ID && dirs.find_desktop_file(id).is_some();
        let restored = base.as_deref().and_then(|value| {
            if recorded.is_some() && split_ids(value).all(|id| usable(&id)) {
                // Everything recorded is still there: the line goes back as it was.
                Some(value.to_string()).filter(|v| split_ids(v).next().is_some())
            } else {
                join_ids(split_ids(value).filter(usable))
            }
        });
        keys.set(DEFAULTS, mime, restored.as_deref());
        changed = true;
    }
    if records.get(Kind::Added, mime, file).is_some()
        && let Some(added) = keys.get(ADDED, mime)
        && split_ids(&added).any(|id| id == DESKTOP_ID)
    {
        let rest = join_ids(split_ids(&added).filter(|id| *id != DESKTOP_ID));
        keys.set(ADDED, mime, rest.as_deref());
        changed = true;
    }
    if let Some(r) = records.get(Kind::Removed, mime, file) {
        let current = keys.get(REMOVED, mime);
        if !current.as_deref().is_some_and(|v| split_ids(v).any(|id| id == DESKTOP_ID)) {
            let previous = r.previous.as_deref().unwrap_or_default();
            let written = join_ids(split_ids(previous).filter(|id| *id != DESKTOP_ID));
            let now = current.as_deref().and_then(|v| join_ids(split_ids(v)));
            let value = if now == written {
                // Untouched since: the line goes back as it was.
                r.previous.clone()
            } else {
                let ids = current.as_deref().map(split_ids).into_iter().flatten();
                join_ids(ids.chain([DESKTOP_ID]))
            };
            keys.set(REMOVED, mime, value.as_deref());
            changed = true;
        }
    }
    changed
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

/// The menu entry for ZXraw Explorer, compared against `exe` (see [`launcher`]).
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

/// The program a menu entry should start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launcher {
    /// The AppImage file (`$APPIMAGE`), else this executable.
    Path(PathBuf),
    /// `$APPIMAGE` names a file that is no longer there: the AppImage was moved or deleted
    /// while running. This executable is inside its temporary mount, so it is no answer.
    AppImageGone(PathBuf),
    /// No path at all.
    Unknown,
}

/// The [`Launcher`] for this process.
pub fn launcher() -> Launcher {
    launcher_from(std::env::var_os("APPIMAGE"), std::env::current_exe().ok())
}

fn launcher_from(appimage: Option<std::ffi::OsString>, exe: Option<PathBuf>) -> Launcher {
    match appimage.filter(|value| !value.is_empty()).map(PathBuf::from) {
        Some(path) if path.is_absolute() && path.is_file() => Launcher::Path(path),
        Some(path) => Launcher::AppImageGone(path),
        None => exe.map_or(Launcher::Unknown, Launcher::Path),
    }
}

/// The icon name of the generated entry. Not `zxraw`: the user's or a package's
/// `zxraw.png` is never replaced or removed.
pub const ICON_NAME: &str = "zxraw-appimage";

/// The `zxraw.desktop` text for a copy that is started as `exe`: the packaged entry's
/// fields (with [`ICON_NAME`] as its icon), plus the marker that lets [`menu_entry`]
/// recognise it.
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
         Icon={ICON_NAME}\n\
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
/// `$XDG_DATA_HOME/icons/hicolor/<size>x<size>/apps/zxraw-appimage.png`), then refreshes the
/// desktop and icon caches when their tools are there. Refuses to replace an entry it did
/// not write.
pub fn install_menu_entry(
    dirs: &XdgDirs,
    exe: &Path,
    mime_types: &[&str],
    icons: &[(u32, &[u8])],
) -> Result<PathBuf> {
    let path = dirs.user_applications().join(DESKTOP_ID);
    if path.exists() && !is_generated(&path) {
        return Err(WinError::Invalid(format!(
            "{} exists and was not written by ZXraw Explorer",
            path.display()
        )));
    }
    for (size, png) in icons {
        write_atomic(&icon_path(dirs, *size), png)?;
    }
    write_atomic(&path, desktop_entry_text(exe, mime_types).as_bytes())?;
    refresh_caches(dirs);
    Ok(path)
}

fn is_generated(path: &Path) -> bool {
    fs::read_to_string(path)
        .is_ok_and(|text| entry_value(&text, GENERATED_KEY).is_some_and(|v| v.trim() == "true"))
}

/// Removes the entry [`install_menu_entry`] wrote, and each icon whose content is still the
/// one written (`icons`). The caller restores the defaults first: a type left pointing at
/// `zxraw.desktop` would point at nothing.
pub fn remove_menu_entry(dirs: &XdgDirs, icons: &[(u32, &[u8])]) -> Result<()> {
    let path = dirs.user_applications().join(DESKTOP_ID);
    if !is_generated(&path) {
        return Ok(());
    }
    fs::remove_file(&path).map_err(|source| io_err("remove the menu entry", &path, source))?;
    for (size, png) in icons {
        let icon = icon_path(dirs, *size);
        if fs::read(&icon).is_ok_and(|bytes| bytes == *png) {
            fs::remove_file(&icon).map_err(|source| io_err("remove the icon", &icon, source))?;
        }
    }
    refresh_caches(dirs);
    Ok(())
}

fn icon_path(dirs: &XdgDirs, size: u32) -> PathBuf {
    dirs.data_home
        .join(format!("icons/hicolor/{size}x{size}/apps/{ICON_NAME}.png"))
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

/// Which part of a `mimeapps.list` a record undoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The `[Default Applications]` value before ZXraw took the type over.
    Default,
    /// ZXraw was added under `[Added Associations]` (the previous value is kept for reference).
    Added,
    /// ZXraw was taken out of `[Removed Associations]`; the value it had then.
    Removed,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Kind::Default => "default",
            Kind::Added => "added",
            Kind::Removed => "removed",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        [Kind::Default, Kind::Added, Kind::Removed]
            .into_iter()
            .find(|kind| kind.tag() == tag)
    }
}

#[derive(Debug, Clone)]
struct Record {
    kind: Kind,
    mime: String,
    file: PathBuf,
    previous: Option<String>,
}

/// The first line of a record file.
const RECORD_HEADER: &str = "# ZXraw Explorer: default apps to restore, version 2";

/// Starts the message of a refused record file (see [`is_damaged_record`]).
const DAMAGED: &str = "the record of the previous default apps is damaged";

/// Starts the message of an edit given up because another program kept changing the file.
const CHANGED: &str = "the default apps were changed by another program";

/// The record file could not be trusted, so nothing was restored or changed.
pub fn is_damaged_record(error: &WinError) -> bool {
    matches!(error, WinError::Invalid(message) if message.starts_with(DAMAGED))
}

/// Another program changed a `mimeapps.list` twice while ZXraw was writing it.
pub fn is_changed_by_another_program(error: &WinError) -> bool {
    matches!(error, WinError::Invalid(message) if message.starts_with(CHANGED))
}

/// What ZXraw changed, one line per (kind, type, file): `kind<TAB>type<TAB>file<TAB>=value`,
/// or `!` for "no entry", under [`RECORD_HEADER`].
#[derive(Debug, Default, Clone)]
struct Records {
    entries: Vec<Record>,
}

impl Records {
    /// Reads the record. Any line that is not exactly as written, a type that is not
    /// `type/subtype`, or a file other than the user's own `mimeapps.list` or
    /// `<desktop>-mimeapps.list` refuses the whole file: a partly trusted record could put
    /// back the wrong app or write somewhere else.
    fn load(path: &Path, dirs: &XdgDirs) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(damaged(path, 0, "not UTF-8"));
            }
            Err(source) => return Err(io_err("read the default-app record", path, source)),
        };
        let mut lines = text.lines();
        if lines.next() != Some(RECORD_HEADER) {
            return Err(damaged(path, 1, "unknown header"));
        }
        let mut entries = Vec::new();
        for (index, line) in lines.enumerate() {
            let number = index + 2;
            let fields: Vec<&str> = line.split('\t').collect();
            let [kind, mime, file, previous] = fields[..] else {
                return Err(damaged(path, number, "not four fields"));
            };
            let kind = Kind::from_tag(kind).ok_or_else(|| damaged(path, number, "unknown kind"))?;
            if !is_mime_type(mime) {
                return Err(damaged(path, number, "not a MIME type"));
            }
            let file = PathBuf::from(file);
            if !is_user_mimeapps(dirs, &file) {
                return Err(damaged(path, number, "not the user's mimeapps.list"));
            }
            let previous = match previous {
                "!" => None,
                value => Some(
                    value
                        .strip_prefix('=')
                        .ok_or_else(|| damaged(path, number, "bad value"))?
                        .to_string(),
                ),
            };
            entries.push(Record {
                kind,
                mime: mime.to_string(),
                file,
                previous,
            });
        }
        Ok(Self { entries })
    }

    fn get(&self, kind: Kind, mime: &str, file: &Path) -> Option<&Record> {
        self.entries
            .iter()
            .find(|r| r.kind == kind && r.mime == mime && r.file == file)
    }

    fn insert(&mut self, kind: Kind, mime: &str, file: &Path, previous: Option<String>) {
        self.entries
            .retain(|r| !(r.kind == kind && r.mime == mime && r.file == file));
        self.entries.push(Record {
            kind,
            mime: mime.to_string(),
            file: file.to_path_buf(),
            previous,
        });
    }

    fn save(&self, path: &Path) -> Result<()> {
        if self.entries.is_empty() {
            return match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(io_err("remove the default-app record", path, source)),
            };
        }
        let mut text = format!("{RECORD_HEADER}\n");
        for r in &self.entries {
            let value = match &r.previous {
                Some(value) => format!("={value}"),
                None => "!".to_string(),
            };
            text.push_str(&format!("{}\t{}\t{}\t{value}\n", r.kind.tag(), r.mime, r.file.display()));
        }
        write_atomic(path, text.as_bytes())
    }
}

fn damaged(path: &Path, line: usize, why: &str) -> WinError {
    WinError::Invalid(format!("{DAMAGED} ({}, line {line}: {why})", path.display()))
}

/// `type/subtype`, both RFC 6838 restricted names.
fn is_mime_type(text: &str) -> bool {
    let token = |part: &str| {
        !part.is_empty()
            && part.len() <= 127
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c))
    };
    matches!(text.split_once('/'), Some((kind, sub)) if token(kind) && token(sub))
}

/// `$XDG_CONFIG_HOME/mimeapps.list` or `$XDG_CONFIG_HOME/<desktop>-mimeapps.list`, compared
/// after resolving links and `..`.
fn is_user_mimeapps(dirs: &XdgDirs, file: &Path) -> bool {
    let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let named = name == "mimeapps.list"
        || name.strip_suffix("-mimeapps.list").is_some_and(|desktop| {
            !desktop.is_empty()
                && desktop
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                && !desktop.starts_with('.')
        });
    let resolve = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    named
        && file.is_absolute()
        && file
            .parent()
            .is_some_and(|parent| resolve(parent) == resolve(&dirs.config_home))
}

/// What tells one version of a file from the next: device, inode, size, modified time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp(u64, u64, u64, i64, i64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(Stamp(meta.dev(), meta.ino(), meta.len(), meta.mtime(), meta.mtime_nsec()))
    }
    #[cfg(not(unix))]
    {
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .unwrap_or_default();
        Some(Stamp(0, 0, meta.len(), modified.as_secs() as i64, modified.subsec_nanos() as i64))
    }
}

/// Reads the key file at `path` (empty when missing), lets `edit` change it, and replaces
/// the file, unless it changed in between (another program saved its own default): then
/// the edit runs again on the new content, and if the file changes once more it gives up
/// with [`is_changed_by_another_program`]. `edit` returns `None` for "nothing to write";
/// `commit` runs with its result right before the file is replaced.
fn edit_file<R>(
    path: &Path,
    mut edit: impl FnMut(&mut KeyFile) -> Result<Option<R>>,
    mut commit: impl FnMut(&R) -> Result<()>,
) -> Result<Option<R>> {
    for _attempt in 0..2 {
        let before = stamp(path);
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => return Err(io_err("read mimeapps.list", path, source)),
        };
        let mut keys = KeyFile::parse(&text);
        let Some(result) = edit(&mut keys)? else {
            return Ok(None);
        };
        commit(&result)?;
        if write_atomic_if_unchanged(path, keys.render().as_bytes(), before.as_ref())? {
            return Ok(Some(result));
        }
        tracing::info!("{} changed while it was being edited; reading it again", path.display());
    }
    Err(WinError::Invalid(format!(
        "{CHANGED} while ZXraw was saving {}; try again",
        path.display()
    )))
}

#[cfg(test)]
thread_local! {
    /// Tests: runs right before the rename, to change the file behind the writer's back.
    static BEFORE_RENAME: std::cell::RefCell<Option<Box<dyn FnMut(&Path)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Replaces `path` with `bytes` through a temp file in the same folder and a rename, so a
/// reader never sees half a file. Follows a symlinked `mimeapps.list` to its target.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_checked(path, bytes, None).map(|_| ())
}

/// [`write_atomic`], but only when the file still has the stamp `before` (`None`: missing).
/// Returns false, writing nothing, when it changed.
fn write_atomic_if_unchanged(path: &Path, bytes: &[u8], before: Option<&Stamp>) -> Result<bool> {
    write_atomic_checked(path, bytes, Some(before))
}

fn write_atomic_checked(path: &Path, bytes: &[u8], expect: Option<Option<&Stamp>>) -> Result<bool> {
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
        #[cfg(test)]
        BEFORE_RENAME.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(&target);
            }
        });
        if let Some(expected) = expect
            && stamp(&target).as_ref() != expected
        {
            fs::remove_file(&temp)?;
            return Ok(false);
        }
        fs::rename(&temp, &target)?;
        Ok(true)
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

    /// A `zxraw.desktop` that lists the types the tests use.
    fn zxraw(dir: &Path) {
        write(
            &dir.join("applications").join(DESKTOP_ID),
            "[Desktop Entry]\nName=ZXraw\nExec=zxraw %f\nMimeType=image/jpeg;image/png;image/gif;image/tiff;inode/directory;\n",
        );
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
        zxraw(&d.data_home);
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
        // The removed association ZXraw took out is back; nothing else names it.
        assert!(text.ends_with("[Removed Associations]\nimage/png=zxraw.desktop;\n"), "{text}");
        assert_eq!(text.matches("zxraw").count(), 1, "{text}");
        assert!(!e.record.exists(), "record removed once empty");
    }

    #[test]
    fn a_damaged_record_refuses_restore_and_set() {
        let e = env(&[]);
        let d = &e.dirs;
        zxraw(&d.data_home);
        app(&d.data_dirs[1], "xviewer.desktop", "Image Viewer");
        write(&d.user_mimeapps(), "[Default Applications]\nimage/jpeg=xviewer.desktop;\n");
        set_defaults(d, &["image/jpeg"], &e.record).unwrap();
        let good = read(&e.record);
        assert!(good.starts_with(RECORD_HEADER), "{good}");
        let main = d.user_mimeapps().display().to_string();
        let elsewhere = d.data_home.join("mimeapps.list").display().to_string();
        let damaged_records = [
            good.replace(RECORD_HEADER, "# something else"),
            format!("{good}garbage line\n"),
            format!("{good}default\timage/png\t{main}\n"),
            format!("{good}default\tnot-a-type\t{main}\t!\n"),
            format!("{good}default\timage/png\t{elsewhere}\t!\n"),
            format!("{good}default\timage/png\t/etc/xdg/mimeapps.list\t!\n"),
            format!("{good}default\timage/png\t{main}/../../x/mimeapps.list\t!\n"),
            format!("{good}default\timage/png\t{main}\tvalue-without-equals\n"),
            format!("{good}other\timage/png\t{main}\t!\n"),
        ];
        let before = read(&d.user_mimeapps());
        for bad in damaged_records {
            write(&e.record, &bad);
            let error = restore_defaults(d, &["image/jpeg"], &e.record).unwrap_err();
            assert!(is_damaged_record(&error), "{bad}: {error}");
            assert!(is_damaged_record(&set_defaults(d, &["image/png"], &e.record).unwrap_err()));
            assert_eq!(read(&d.user_mimeapps()), before, "nothing written for {bad}");
            assert_eq!(read(&e.record), bad, "the record is kept for a look");
        }
        // A desktop-specific user file is a valid target.
        write(&e.record, &format!("{good}default\timage/png\t{}\t!\n", d.config_home.join("kde-mimeapps.list").display()));
        restore_defaults(d, &["image/jpeg"], &e.record).unwrap();
        assert_eq!(id(d, "image/jpeg").as_deref(), Some("xviewer.desktop"));
    }

    #[test]
    fn a_type_the_entry_does_not_list_is_associated_and_unassociated() {
        let e = env(&[]);
        let d = &e.dirs;
        // An older entry that does not list HEIF.
        zxraw(&d.data_home);
        write(&d.user_mimeapps(), "[Added Associations]\nimage/heif=gimp.desktop;\n");
        set_defaults(d, &["image/heif", "image/jpeg"], &e.record).unwrap();
        let text = read(&d.user_mimeapps());
        assert!(text.contains("[Added Associations]\nimage/heif=gimp.desktop;zxraw.desktop;\n"), "{text}");
        assert!(!text.contains("[Added Associations]\nimage/jpeg"), "a listed type gets no association: {text}");
        assert!(default_for(d, "image/heif").unwrap().is_zxraw());
        // Without the association the default alone does not count.
        let unassociated = text.replace("gimp.desktop;zxraw.desktop;", "gimp.desktop;");
        write(&d.user_mimeapps(), &unassociated);
        let handler = default_for(d, "image/heif").unwrap();
        assert_eq!(handler.desktop_id, DESKTOP_ID);
        assert!(!handler.is_zxraw());
        write(&d.user_mimeapps(), &text);
        restore_defaults(d, &["image/heif", "image/jpeg"], &e.record).unwrap();
        assert_eq!(read(&d.user_mimeapps()), "[Added Associations]\nimage/heif=gimp.desktop;\n\n[Default Applications]\n");
    }

    #[test]
    fn a_removed_association_is_recorded_and_put_back() {
        let e = env(&[]);
        let d = &e.dirs;
        zxraw(&d.data_home);
        write(&d.user_mimeapps(), "[Removed Associations]\nimage/gif=zxraw.desktop;gimp.desktop;\nimage/tiff=zxraw.desktop;\n");
        set_defaults(d, &["image/gif", "image/tiff"], &e.record).unwrap();
        let text = read(&d.user_mimeapps());
        assert!(text.starts_with("[Removed Associations]\nimage/gif=gimp.desktop;\n\n[Default"), "tiff's line is gone: {text}");
        // The user edits the gif line meanwhile; ZXraw is added back to it, not over it.
        write(&d.user_mimeapps(), &text.replace("image/gif=gimp.desktop;", "image/gif=gimp.desktop;eog.desktop;"));
        restore_defaults(d, &["image/gif", "image/tiff"], &e.record).unwrap();
        let text = read(&d.user_mimeapps());
        assert!(text.contains("[Removed Associations]\nimage/gif=gimp.desktop;eog.desktop;zxraw.desktop;\nimage/tiff=zxraw.desktop;\n"), "{text}");
    }

    #[test]
    fn a_stale_appimage_path_never_falls_back_to_the_mount() {
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("ZXraw.AppImage");
        let mount = PathBuf::from("/tmp/.mount_ZXrawXYZ/usr/bin/zxraw");
        assert_eq!(launcher_from(None, Some(mount.clone())), Launcher::Path(mount.clone()));
        assert_eq!(launcher_from(Some(image.clone().into()), Some(mount.clone())), Launcher::AppImageGone(image.clone()));
        assert_eq!(launcher_from(Some("relative.AppImage".into()), Some(mount.clone())), Launcher::AppImageGone("relative.AppImage".into()));
        fs::write(&image, b"x").unwrap();
        assert_eq!(launcher_from(Some(image.clone().into()), Some(mount)), Launcher::Path(image));
        assert_eq!(launcher_from(Some("".into()), None), Launcher::Unknown);
    }

    #[test]
    fn icons_never_replace_or_remove_someone_elses() {
        let e = env(&[]);
        let d = &e.dirs;
        let theirs = d.data_home.join("icons/hicolor/48x48/apps/zxraw.png");
        write(&theirs, "their icon");
        let exe = Path::new("/opt/ZXraw.AppImage");
        install_menu_entry(d, exe, &[], &[(48, b"ours48"), (64, b"ours64")]).unwrap();
        assert_eq!(read(&theirs), "their icon");
        // Someone replaced our 64 px icon with their own: it stays.
        let ours64 = d.data_home.join("icons/hicolor/64x64/apps/zxraw-appimage.png");
        write(&ours64, "edited");
        remove_menu_entry(d, &[(48, b"ours48"), (64, b"ours64")]).unwrap();
        assert!(!d.data_home.join("icons/hicolor/48x48/apps/zxraw-appimage.png").exists());
        assert_eq!(read(&ours64), "edited");
        assert_eq!(read(&theirs), "their icon");
    }

    /// Changes the file `times` times right before ZXraw's rename, as another program would.
    fn interfere(times: usize, line: &'static str) {
        let mut left = times;
        BEFORE_RENAME.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
                if left > 0 && path.ends_with("mimeapps.list") {
                    left -= 1;
                    let mut text = fs::read_to_string(path).unwrap_or_default();
                    text.push_str(line);
                    fs::write(path, text).unwrap();
                }
            }));
        });
    }

    #[test]
    fn a_file_changed_between_read_and_write_is_edited_again_or_refused() {
        let e = env(&[]);
        let d = &e.dirs;
        zxraw(&d.data_home);
        write(&d.user_mimeapps(), "[Default Applications]\ntext/plain=gedit.desktop\n");
        interfere(1, "text/html=firefox.desktop\n");
        set_defaults(d, &["image/jpeg"], &e.record).unwrap();
        let text = read(&d.user_mimeapps());
        assert!(text.contains("text/html=firefox.desktop\n"), "the other program's change is kept: {text}");
        assert!(text.contains("image/jpeg=zxraw.desktop;\n"), "{text}");
        // Changed on every try: refused, and the other program's file is left as it saved it.
        interfere(2, "text/css=gedit.desktop\n");
        let error = restore_defaults(d, &["image/jpeg"], &e.record).unwrap_err();
        assert!(is_changed_by_another_program(&error), "{error}");
        let text = read(&d.user_mimeapps());
        assert!(text.contains("image/jpeg=zxraw.desktop;\n") && text.matches("text/css").count() == 2, "{text}");
        assert!(e.record.exists(), "a failed restore keeps its record");
        BEFORE_RENAME.with(|hook| *hook.borrow_mut() = None);
        restore_defaults(d, &["image/jpeg"], &e.record).unwrap();
        assert!(!read(&d.user_mimeapps()).contains("zxraw"));
        assert!(!e.record.exists());
    }

    #[test]
    fn missing_file_is_created_and_removed_entries_restore_to_system_default() {
        let e = env(&[]);
        let d = &e.dirs;
        zxraw(&d.data_home);
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
        zxraw(&d.data_home);
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
        zxraw(&d.data_home);
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
        zxraw(&d.data_home);
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
        assert!(text.contains("Icon=zxraw-appimage\n"));
        assert_eq!(fs::read(d.data_home.join("icons/hicolor/48x48/apps/zxraw-appimage.png")).unwrap(), b"png48");
        assert!(matches!(menu_entry(d, exe), MenuEntry::Ours { current: true, .. }));
        assert!(matches!(menu_entry(d, Path::new("/elsewhere/ZXraw.AppImage")), MenuEntry::Ours { current: false, .. }));
        // Update after a move.
        install_menu_entry(d, Path::new("/elsewhere/ZXraw.AppImage"), &["image/jpeg"], &[]).unwrap();
        assert!(matches!(menu_entry(d, Path::new("/elsewhere/ZXraw.AppImage")), MenuEntry::Ours { current: true, .. }));
        remove_menu_entry(d, &[(48, b"png48"), (256, b"png256")]).unwrap();
        assert!(!path.exists());
        assert!(!d.data_home.join("icons/hicolor/256x256/apps/zxraw-appimage.png").exists());
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

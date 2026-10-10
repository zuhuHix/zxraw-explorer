//! Path safety and convenience helpers.
//!
//! Everything here is *lexical* (no filesystem access) unless stated otherwise, so it can be
//! used on paths that do not exist yet and is cheap enough for the UI thread.
//!
//! Validation rules are Windows rules on every platform: Forge is a Windows file manager and
//! names it creates should stay valid when the data is later copied to an NTFS/exFAT volume.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf, Prefix};

use crate::error::{CoreError, Result};

/// Classic Win32 `MAX_PATH`.
pub const MAX_PATH: usize = 260;
/// Directories are limited to `MAX_PATH - 12` by `CreateDirectoryW` without the `\\?\` prefix.
pub const MAX_DIR_PATH: usize = 248;
/// Maximum length of one path component, in UTF-16 code units (NTFS/exFAT).
pub const MAX_COMPONENT: usize = 255;

const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

// ---------------------------------------------------------------------------
// User input
// ---------------------------------------------------------------------------

/// Normalize a path typed or pasted by the user (address bar, dialogs):
/// trims whitespace and surrounding quotes, accepts `file:///` URLs, expands `%VAR%`
/// environment variables and a leading `~`, unifies separators (on Windows) and resolves
/// `.`/`..` lexically. Does not touch the filesystem.
pub fn normalize_user_path(input: &str) -> PathBuf {
    let mut s = input.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            s = s[1..s.len() - 1].trim();
        }
    }
    let mut owned = s.to_string();
    if let Some(rest) = strip_prefix_ci(&owned, "file:///") {
        owned = percent_decode(rest);
    } else if let Some(rest) = strip_prefix_ci(&owned, "file://") {
        // file://server/share -> \\server\share
        owned = format!("//{}", percent_decode(rest));
    }
    let mut expanded = expand_env_vars(&owned);
    if (expanded == "~" || expanded.starts_with("~/") || expanded.starts_with("~\\"))
        && let Some(home) = home_dir()
    {
        expanded = format!("{}{}", home.display(), &expanded[1..]);
    }
    if cfg!(windows) {
        expanded = expanded.replace('/', "\\");
    }
    if expanded.is_empty() {
        return PathBuf::new();
    }
    lexical_normalize(Path::new(&expanded))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Expand Windows-style `%VAR%` references. Unknown variables and lone `%` are left as-is.
/// Lookup is case-insensitive on Windows (as the OS does).
pub fn expand_env_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 && !after[..end].contains(['\\', '/', ' ']) => {
                let name = &after[..end];
                match std::env::var_os(name) {
                    Some(v) => out.push_str(&v.to_string_lossy()),
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resolve `.` and `..` lexically (does not consult the filesystem, so symlinks are not
/// considered). `..` never climbs above a root/prefix.
pub fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(c),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.iter().collect()
}

// ---------------------------------------------------------------------------
// Containment
// ---------------------------------------------------------------------------

/// Comparison key of a component: case-insensitive on Windows, prefix forms unified
/// (`C:` == `\\?\C:`, `\\srv\share` == `\\?\UNC\srv\share`).
fn component_key(c: &Component<'_>) -> String {
    let raw = match c {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => format!("{}:", d as char),
            Prefix::UNC(s, sh) | Prefix::VerbatimUNC(s, sh) => {
                format!(r"\\{}\{}", s.to_string_lossy(), sh.to_string_lossy())
            }
            Prefix::Verbatim(v) => format!(r"\\?\{}", v.to_string_lossy()),
            Prefix::DeviceNS(d) => format!(r"\\.\{}", d.to_string_lossy()),
        },
        Component::RootDir => "/".to_string(),
        Component::CurDir => ".".to_string(),
        Component::ParentDir => "..".to_string(),
        Component::Normal(n) => n.to_string_lossy().into_owned(),
    };
    if cfg!(windows) {
        raw.to_lowercase()
    } else {
        raw
    }
}

fn keys(p: &Path) -> Vec<String> {
    lexical_normalize(p)
        .components()
        .map(|c| component_key(&c))
        .collect()
}

/// Are two paths the same location (lexically; case-insensitive on Windows)?
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    keys(a) == keys(b)
}

/// Is `candidate` equal to `base` or located below it? Lexical, component-wise
/// (so `C:\foo` does not contain `C:\foobar`), case-insensitive on Windows.
pub fn is_within(base: &Path, candidate: &Path) -> bool {
    let b = keys(base);
    let c = keys(candidate);
    c.len() >= b.len() && b.iter().zip(&c).all(|(x, y)| x == y)
}

/// Like [`is_within`] but false when the paths are equal.
pub fn is_strictly_within(base: &Path, candidate: &Path) -> bool {
    is_within(base, candidate) && keys(base).len() < keys(candidate).len()
}

/// Validate an untrusted relative path (archive entry, manifest line, template output) and
/// return it as a clean relative `PathBuf`. Both `/` and `\` are treated as separators.
///
/// Rejects: empty paths, absolute paths, drive/UNC/device prefixes, `..`, any `:` (drive
/// letters and NTFS alternate data streams), characters invalid on Windows, components with
/// trailing dots/spaces, and reserved device names (`CON`, `NUL`, `COM1`…).
pub fn validate_relative(untrusted: &str) -> Result<PathBuf> {
    let bad = |why: &str| CoreError::UnsafePath(format!("{untrusted:?}: {why}"));
    if untrusted.is_empty() {
        return Err(bad("empty path"));
    }
    if untrusted.starts_with(['/', '\\']) {
        return Err(bad("absolute path"));
    }
    if untrusted.contains(':') {
        return Err(bad("drive prefix or alternate data stream"));
    }
    if untrusted.contains('\0') {
        return Err(bad("NUL byte"));
    }
    let mut out = PathBuf::new();
    for comp in untrusted.split(['/', '\\']) {
        match comp {
            "" | "." => continue,
            ".." => return Err(bad("parent directory reference")),
            c => {
                validate_file_name(c).map_err(|e| bad(&e.to_string()))?;
                out.push(c);
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(bad("empty path"));
    }
    Ok(out)
}

/// Join an untrusted relative path onto `base`, guaranteeing the result stays inside `base`.
/// See [`validate_relative`] for the rules.
pub fn safe_join(base: &Path, untrusted: &str) -> Result<PathBuf> {
    let rel = validate_relative(untrusted)?;
    let joined = base.join(&rel);
    if !is_within(base, &joined) {
        return Err(CoreError::UnsafePath(format!(
            "{untrusted:?}: escapes base"
        )));
    }
    Ok(joined)
}

// ---------------------------------------------------------------------------
// File names
// ---------------------------------------------------------------------------

/// Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM0-9`, `LPT0-9`, incl.
/// superscript digits, `CONIN$`, `CONOUT$`). They are reserved with any extension
/// (`nul.txt`) and with trailing spaces before the dot.
pub fn is_reserved_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let upper = base.to_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        s => {
            let mut chars = s.chars();
            let head: String = chars.by_ref().take(3).collect();
            let tail: Vec<char> = chars.collect();
            (head == "COM" || head == "LPT")
                && tail.len() == 1
                && (tail[0].is_ascii_digit() || matches!(tail[0], '¹' | '²' | '³'))
        }
    }
}

/// Validate a single file/folder name for creation or rename.
pub fn validate_file_name(name: &str) -> Result<()> {
    let bad = |why: &str| Err(CoreError::Invalid(format!("invalid name {name:?}: {why}")));
    if name.is_empty() || name.trim().is_empty() {
        return bad("empty");
    }
    if name == "." || name == ".." {
        return bad("reserved");
    }
    if let Some(c) = name
        .chars()
        .find(|c| INVALID_CHARS.contains(c) || (*c as u32) < 32)
    {
        return bad(&format!("contains invalid character {c:?}"));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return bad("ends with a dot or space");
    }
    if is_reserved_name(name) {
        return bad("reserved device name");
    }
    if name.encode_utf16().count() > MAX_COMPONENT {
        return bad("longer than 255 characters");
    }
    Ok(())
}

/// Turn arbitrary text into a valid file name: invalid characters become `_`, leading
/// spaces and trailing dots/spaces are trimmed, reserved names get a `_` prefix, and the
/// result is truncated to 255 UTF-16 units (keeping the extension where possible).
pub fn sanitize_file_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if INVALID_CHARS.contains(&c) || (c as u32) < 32 {
                '_'
            } else {
                c
            }
        })
        .collect();
    s = s
        .trim_start_matches(' ')
        .trim_end_matches(['.', ' '])
        .to_string();
    if s.is_empty() || s == "." || s == ".." {
        s = "_".to_string();
    }
    if is_reserved_name(&s) {
        s.insert(0, '_');
    }
    if s.encode_utf16().count() > MAX_COMPONENT {
        let (stem, ext) = split_name_ext(&s);
        let ext = if ext.encode_utf16().count() < 32 {
            ext
        } else {
            ""
        };
        let budget = MAX_COMPONENT - ext.encode_utf16().count();
        let mut out = String::new();
        let mut used = 0;
        for c in stem.chars() {
            if used + c.len_utf16() > budget {
                break;
            }
            used += c.len_utf16();
            out.push(c);
        }
        let out = out.trim_end_matches(['.', ' ']).to_string();
        s = format!("{out}{ext}");
    }
    s
}

/// Split `name` into (stem, extension-with-dot). Dotfiles (`.gitignore`) have no extension.
pub fn split_name_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Does anything (file, dir, or even a broken link) exist at `p`?
pub fn exists_no_follow(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// First free name in `dir` following the Explorer convention `name (2).ext`,
/// `name (3).ext`… Returns `dir/name` unchanged when free. Touches the filesystem.
pub fn unique_name(dir: &Path, name: &str) -> PathBuf {
    unique_name_with(dir, name, true, exists_no_follow)
}

/// [`unique_name`] with control over extension splitting (pass `false` for folders) and a
/// custom "is taken" predicate (e.g. to also avoid names reserved by a pending plan).
pub fn unique_name_with(
    dir: &Path,
    name: &str,
    split_ext: bool,
    taken: impl Fn(&Path) -> bool,
) -> PathBuf {
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = if split_ext {
        split_name_ext(name)
    } else {
        (name, "")
    };
    // "photo (2)" -> continue at 3 rather than producing "photo (2) (2)".
    let (base, start) = match parse_counter_suffix(stem) {
        Some((b, n)) => (b, n + 1),
        None => (stem, 2),
    };
    let mut n = start;
    loop {
        let candidate = dir.join(format!("{base} ({n}){ext}"));
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn parse_counter_suffix(stem: &str) -> Option<(&str, u64)> {
    let inner = stem.strip_suffix(')')?;
    let open = inner.rfind(" (")?;
    let n: u64 = inner[open + 2..].parse().ok()?;
    Some((&stem[..open], n))
}

// ---------------------------------------------------------------------------
// Long paths
// ---------------------------------------------------------------------------

/// Convert an absolute Windows path to the extended-length form (`\\?\C:\…`,
/// `\\?\UNC\server\share\…`). Relative paths and already-prefixed paths are returned
/// unchanged. The path is lexically normalized first because the OS does not normalize
/// verbatim paths.
///
/// Rust's `std::fs` already does this internally on Windows, so this is only needed when
/// handing paths to raw Win32/COM APIs (forge-win) or external tools.
pub fn to_extended_length(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") || s.starts_with("//?/") {
        return p.to_path_buf();
    }
    let unified = s.replace('/', "\\");
    let is_unc = unified.starts_with(r"\\");
    let b = unified.as_bytes();
    let is_drive_abs = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    if !is_unc && !is_drive_abs {
        return p.to_path_buf();
    }
    // Normalize "." and ".." manually (string based so this works on every platform).
    let (head, rest) = if is_unc {
        ("UNC\\".to_string(), &unified[2..])
    } else {
        (unified[..3].to_string(), &unified[3..])
    };
    // The server\share of a UNC path cannot be popped by "..".
    let keep = if is_unc { 2 } else { 0 };
    let mut parts: Vec<&str> = Vec::new();
    for comp in rest.split('\\') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.len() > keep {
                    parts.pop();
                }
            }
            c => parts.push(c),
        }
    }
    PathBuf::from(format!(r"\\?\{head}{}", parts.join("\\")))
}

/// Apply [`to_extended_length`] only when the path is long enough to need it.
pub fn long_path_if_needed(p: &Path) -> PathBuf {
    if p.as_os_str().len() >= MAX_DIR_PATH {
        to_extended_length(p)
    } else {
        p.to_path_buf()
    }
}

/// Strip a `\\?\` / `\\?\UNC\` prefix for display.
pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.into_owned()
    }
}

// ---------------------------------------------------------------------------
// Breadcrumbs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crumb {
    /// Text shown in the breadcrumb bar (`C:`, `\\server\share`, `Users`).
    pub label: String,
    /// Full path the crumb navigates to.
    pub path: PathBuf,
}

/// Breadcrumb chain from the root down to `p` (inclusive).
pub fn parent_chain(p: &Path) -> Vec<Crumb> {
    let mut out: Vec<Crumb> = Vec::new();
    let mut acc = PathBuf::new();
    for c in lexical_normalize(p).components() {
        acc.push(c.as_os_str());
        match c {
            Component::Prefix(pre) => {
                let label = match pre.kind() {
                    Prefix::Disk(d) | Prefix::VerbatimDisk(d) => format!("{}:", d as char),
                    Prefix::UNC(s, sh) | Prefix::VerbatimUNC(s, sh) => {
                        format!(r"\\{}\{}", s.to_string_lossy(), sh.to_string_lossy())
                    }
                    _ => pre.as_os_str().to_string_lossy().into_owned(),
                };
                out.push(Crumb {
                    label,
                    path: acc.clone(),
                });
            }
            Component::RootDir => {
                if let Some(last) = out.last_mut() {
                    // "C:" + "\" -> the crumb should navigate to "C:\".
                    last.path = acc.clone();
                } else {
                    out.push(Crumb {
                        label: std::path::MAIN_SEPARATOR.to_string(),
                        path: acc.clone(),
                    });
                }
            }
            Component::Normal(n) => out.push(Crumb {
                label: n.to_string_lossy().into_owned(),
                path: acc.clone(),
            }),
            Component::CurDir | Component::ParentDir => out.push(Crumb {
                label: c.as_os_str().to_string_lossy().into_owned(),
                path: acc.clone(),
            }),
        }
    }
    out
}

/// File name of `p` as a lossy `String` ("" when none).
pub fn name_of(p: &Path) -> String {
    p.file_name()
        .map(OsStr::to_string_lossy)
        .map(|c| c.into_owned())
        .unwrap_or_default()
}

/// Relative path with `/` separators (for manifests and archives).
pub fn to_slash(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_path_normalization() {
        // SAFETY: tests in this module are the only users of this variable.
        unsafe { std::env::set_var("FORGE_TEST_DIR", "base") };
        let p = normalize_user_path("  \"%FORGE_TEST_DIR%/a/./b/../c\"  ");
        assert_eq!(p, Path::new("base").join("a").join("c"));
        assert_eq!(expand_env_vars("100% sure"), "100% sure");
        assert_eq!(
            expand_env_vars("%NOPE_NOT_SET_X%\\x"),
            "%NOPE_NOT_SET_X%\\x"
        );
        assert_eq!(normalize_user_path("''"), PathBuf::new());
        let f = normalize_user_path("file:///tmp/my%20dir");
        assert!(f.to_string_lossy().contains("my dir"));
    }

    #[test]
    fn containment() {
        let base = Path::new("/data/photos");
        assert!(is_within(base, Path::new("/data/photos")));
        assert!(is_within(base, Path::new("/data/photos/2024/a.jpg")));
        assert!(!is_within(base, Path::new("/data/photos2")));
        assert!(!is_within(base, Path::new("/data/photos/../secret")));
        assert!(is_strictly_within(base, Path::new("/data/photos/x")));
        assert!(!is_strictly_within(base, Path::new("/data/photos")));
        #[cfg(windows)]
        {
            assert!(is_within(
                Path::new(r"C:\Users"),
                Path::new(r"c:\users\Bob")
            ));
            assert!(is_within(
                Path::new(r"C:\Users"),
                Path::new(r"\\?\C:\Users\Bob")
            ));
        }
    }

    #[test]
    fn safe_join_rejects_escapes() {
        let base = Path::new("base");
        assert_eq!(
            safe_join(base, "a/b\\c.txt").unwrap(),
            base.join("a").join("b").join("c.txt")
        );
        assert_eq!(safe_join(base, "./a//b").unwrap(), base.join("a").join("b"));
        for bad in [
            "",
            "../x",
            "a/../../x",
            "/etc/passwd",
            "\\x",
            "C:\\x",
            "C:x",
            "\\\\server\\share\\x",
            "\\\\?\\C:\\x",
            "file.txt:stream",
            "CON",
            "a/nul.txt",
            "com1",
            "LPT9.log",
            "a/b./c",
            "a/b /c",
            "a/b?c",
            ".",
        ] {
            assert!(safe_join(base, bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn names() {
        assert!(is_reserved_name("CON"));
        assert!(is_reserved_name("con.txt"));
        assert!(is_reserved_name("Com1 .txt"));
        assert!(is_reserved_name("COM²"));
        assert!(!is_reserved_name("CONSOLE"));
        assert!(!is_reserved_name("COM10"));
        assert!(validate_file_name("ok name.txt").is_ok());
        assert!(validate_file_name("日本語 файл.jpg").is_ok());
        assert!(validate_file_name("bad.").is_err());
        assert!(validate_file_name("a|b").is_err());
        assert!(validate_file_name("..").is_err());
        assert_eq!(sanitize_file_name("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_file_name("  name. . "), "name");
        assert_eq!(sanitize_file_name("nul"), "_nul");
        assert_eq!(sanitize_file_name(""), "_");
        let long = format!("{}.jpg", "x".repeat(400));
        let s = sanitize_file_name(&long);
        assert_eq!(s.encode_utf16().count(), 255);
        assert!(s.ends_with(".jpg"));
        assert_eq!(split_name_ext(".gitignore"), (".gitignore", ""));
        assert_eq!(split_name_ext("a.tar.gz"), ("a.tar", ".gz"));
    }

    #[test]
    fn unique_names() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(unique_name(d.path(), "a.txt"), d.path().join("a.txt"));
        fs::write(d.path().join("a.txt"), "").unwrap();
        assert_eq!(unique_name(d.path(), "a.txt"), d.path().join("a (2).txt"));
        fs::write(d.path().join("a (2).txt"), "").unwrap();
        assert_eq!(unique_name(d.path(), "a.txt"), d.path().join("a (3).txt"));
        assert_eq!(
            unique_name(d.path(), "a (2).txt"),
            d.path().join("a (3).txt")
        );
        fs::create_dir(d.path().join("my.folder")).unwrap();
        assert_eq!(
            unique_name_with(d.path(), "my.folder", false, exists_no_follow),
            d.path().join("my.folder (2)")
        );
    }

    #[test]
    fn extended_length() {
        assert_eq!(
            to_extended_length(Path::new(r"C:\a\.\b\..\c")),
            PathBuf::from(r"\\?\C:\a\c")
        );
        assert_eq!(
            to_extended_length(Path::new(r"\\srv\share\x")),
            PathBuf::from(r"\\?\UNC\srv\share\x")
        );
        assert_eq!(
            to_extended_length(Path::new(r"\\?\C:\x")),
            PathBuf::from(r"\\?\C:\x")
        );
        assert_eq!(to_extended_length(Path::new("rel")), PathBuf::from("rel"));
        let short = Path::new(r"C:\short");
        assert_eq!(long_path_if_needed(short), short);
        let long = PathBuf::from(format!(r"C:\{}", "d".repeat(300)));
        assert!(
            long_path_if_needed(&long)
                .to_string_lossy()
                .starts_with(r"\\?\")
        );
        assert_eq!(display_path(Path::new(r"\\?\UNC\s\sh\x")), r"\\s\sh\x");
    }

    #[test]
    fn breadcrumbs() {
        #[cfg(windows)]
        {
            let c = parent_chain(Path::new(r"C:\Users\Bob"));
            let labels: Vec<_> = c.iter().map(|c| c.label.as_str()).collect();
            assert_eq!(labels, ["C:", "Users", "Bob"]);
            assert_eq!(c[0].path, PathBuf::from(r"C:\"));
            assert_eq!(c[2].path, PathBuf::from(r"C:\Users\Bob"));
            let u = parent_chain(Path::new(r"\\srv\share\x"));
            assert_eq!(u[0].label, r"\\srv\share");
        }
        #[cfg(not(windows))]
        {
            let c = parent_chain(Path::new("/home/bob"));
            assert_eq!(c.len(), 3);
            assert_eq!(c[2].path, PathBuf::from("/home/bob"));
        }
    }
}

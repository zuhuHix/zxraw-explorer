//! Batch rename engine.
//!
//! The pipeline is split in two:
//!
//! * [`preview`] / [`preview_with_existing`] — pure logic. Applies an ordered list of
//!   [`RenameRule`]s to every item, validates the resulting Windows file names and reports
//!   a [`RenameProblem`] per item. No filesystem access unless the caller passes in the
//!   directory listings through [`ExistingNames`].
//! * [`apply`] / [`undo`] — the only functions that touch the filesystem. Renames run in
//!   two phases through unique temporary names, so chains (A→B, B→C) and cycles (A→B,
//!   B→A) work regardless of order. If any step fails, every completed step is reverted.
//!
//! Scope of rules: `Template`, `Replace`, `Regex` and `Insert`/`RemoveRange` operate on
//! the name as a whole unless documented otherwise; `ChangeCase` and `Trim` act on the
//! stem (the part before the last dot) and `ExtensionCase` on the extension only.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::NaiveDate;
use regex::{Regex, RegexBuilder};
use thiserror::Error;

use crate::paths::rename_no_replace;

/// Characters Windows does not allow in a file name component.
const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Device names Windows reserves regardless of extension or case.
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Maximum length of one file name component, in UTF-16 code units.
const MAX_NAME_UNITS: usize = 255;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One item to rename, plus the per-file data that template tokens may refer to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenameInput {
    /// Full path of the item. Its parent directory is where the new name lands.
    pub path: PathBuf,
    /// Value for `{date:FORMAT}`. Items without a date fail a template that uses it.
    pub date: Option<NaiveDate>,
    /// Values for `{meta:KEY}`.
    pub meta: HashMap<String, String>,
}

impl RenameInput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            date: None,
            meta: HashMap::new(),
        }
    }
}

/// Case transformation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
    Lower,
    Upper,
    /// Capitalises the first letter of each word; words are separated by any
    /// non-alphanumeric character other than an apostrophe.
    Title,
}

/// A rename rule. Rules are applied in order; each sees the output of the previous one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameRule {
    /// Replaces the whole name with `pattern`, expanding tokens:
    ///
    /// * `{name}` — original stem (name without extension), not the intermediate value
    /// * `{ext}` — original extension without the dot (empty if none)
    /// * `{n}` / `{n:W}` — counter `start + index * step`, zero-padded to width `W`
    /// * `{date:FORMAT}` — per-item date formatted with chrono/strftime syntax
    /// * `{parent}` — name of the containing folder
    /// * `{meta:KEY}` — per-item metadata value
    ///
    /// `{{` and `}}` produce literal braces.
    Template {
        pattern: String,
        start: u64,
        step: u64,
    },
    /// Literal find/replace on the whole name. Case-insensitive matching when
    /// `case_sensitive` is false.
    Replace {
        find: String,
        replace: String,
        case_sensitive: bool,
    },
    /// Regex replace on the whole name. Replacement uses the `regex` crate syntax
    /// (`$1`, `${name}`; write `$$` for a literal dollar sign).
    Regex {
        pattern: String,
        replacement: String,
    },
    /// Changes the case of the stem.
    ChangeCase(CaseMode),
    /// Changes the case of the extension.
    ExtensionCase(CaseMode),
    /// Removes leading and trailing whitespace from the stem.
    Trim,
    /// Inserts `text` into the stem at character index `at` (clamped to the stem length).
    Insert { at: usize, text: String },
    /// Removes `len` characters from the stem starting at character index `start`
    /// (clamped).
    RemoveRange { start: usize, len: usize },
}

/// Why a preview cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RenameProblem {
    #[error("the new name is empty")]
    Empty,
    #[error("the new name contains the invalid character {0:?}")]
    InvalidChars(char),
    #[error("{0} is a reserved device name on Windows")]
    ReservedName(String),
    #[error("the new name ends with a dot or a space")]
    TrailingDotOrSpace,
    #[error("the new name is {0} UTF-16 units long; the limit is 255")]
    TooLong(usize),
    #[error("another item in this batch gets the same name")]
    DuplicateInBatch,
    #[error("a file with this name already exists in the folder")]
    CollidesWithExisting,
    #[error("the current name is not valid Unicode and cannot be renamed here")]
    NotUnicode,
    #[error("rule error: {0}")]
    Rule(String),
}

/// Result of previewing one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamePreview {
    pub old_path: PathBuf,
    /// `parent(old_path).join(new_name)`.
    pub new_path: PathBuf,
    pub old_name: String,
    pub new_name: String,
    pub problem: Option<RenameProblem>,
}

impl RenamePreview {
    /// True if the name would actually change (case-only changes count).
    pub fn is_change(&self) -> bool {
        self.old_name != self.new_name
    }
}

/// Names that already exist in each directory, used for collision detection.
///
/// The caller fills this from directory listings. Matching is case-insensitive, as on
/// Windows. Names of items that are being renamed in the same batch are handled by the
/// preview logic (they are vacated), so the caller does not need to remove them.
#[derive(Debug, Clone, Default)]
pub struct ExistingNames {
    by_dir: HashMap<String, HashSet<String>>,
}

impl ExistingNames {
    /// Adds the names found in `dir`. Call once per directory involved in the batch.
    pub fn add_dir<I, S>(&mut self, dir: &Path, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let set = self.by_dir.entry(dir_key(dir)).or_default();
        set.extend(names.into_iter().map(|n| n.as_ref().to_lowercase()));
    }

    fn contains(&self, dir: &str, name_lc: &str) -> bool {
        self.by_dir
            .get(dir)
            .is_some_and(|set| set.contains(name_lc))
    }
}

/// Outcome of [`apply`] or [`undo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameStatus {
    /// Nothing was touched; these items have problems (or would collide with each other).
    Refused { blocked: Vec<PathBuf> },
    /// Every item keeps its name.
    NothingToDo,
    /// All renames succeeded.
    Completed,
    /// A step failed and all completed steps were reverted. Filesystem state is as before.
    RolledBack { failed: PathBuf, error: String },
    /// A step failed and at least one revert failed too. See `stranded`.
    RollbackIncomplete { failed: PathBuf, error: String },
}

/// A file that could not be returned to its original name after a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrandedFile {
    /// Where the file is now (the target name, or a temporary name).
    pub current: PathBuf,
    /// Where it should have been.
    pub original: PathBuf,
    pub error: String,
}

/// Report of what [`apply`] or [`undo`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameReport {
    pub status: RenameStatus,
    /// `(from, to)` pairs left in place. Non-empty only when `status` is `Completed`.
    pub renamed: Vec<(PathBuf, PathBuf)>,
    /// `(from, to)` pairs that succeeded and were then reverted after a failure.
    pub reverted: Vec<(PathBuf, PathBuf)>,
    /// Files that could not be restored. Empty unless `RollbackIncomplete`.
    pub stranded: Vec<StrandedFile>,
    /// `(new_path, old_path)` pairs; pass them to [`undo`] to revert. Non-empty only when
    /// `status` is `Completed`.
    pub undo: Vec<(PathBuf, PathBuf)>,
}

impl RenameReport {
    fn with_status(status: RenameStatus) -> Self {
        Self {
            status,
            renamed: Vec::new(),
            reverted: Vec::new(),
            stranded: Vec::new(),
            undo: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Preview (pure)
// ---------------------------------------------------------------------------

/// Previews the rename of every item. Collision detection against the filesystem is not
/// performed; use [`preview_with_existing`] for that.
pub fn preview(items: &[RenameInput], rules: &[RenameRule]) -> Vec<RenamePreview> {
    preview_with_existing(items, rules, &ExistingNames::default())
}

/// Previews the rename of every item, also checking new names against `existing`.
pub fn preview_with_existing(
    items: &[RenameInput],
    rules: &[RenameRule],
    existing: &ExistingNames,
) -> Vec<RenamePreview> {
    let compiled = compile(rules);
    let mut out: Vec<RenamePreview> = items
        .iter()
        .enumerate()
        .map(|(i, item)| match &compiled {
            Ok(c) => preview_one(i, item, c),
            Err(msg) => preview_failed(item, msg),
        })
        .collect();

    // Duplicates among final names, across the whole batch (unchanged items included,
    // because their name is also a final name).
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for p in &out {
        *counts.entry(final_key(p)).or_default() += 1;
    }
    for p in &mut out {
        if p.problem.is_none() && counts.get(&final_key(p)).copied().unwrap_or(0) > 1 {
            p.problem = Some(RenameProblem::DuplicateInBatch);
        }
    }

    // Collisions with names already in the folder. A target is fine if the name it
    // takes is itself being vacated by a rename in this batch.
    let vacated: HashSet<(String, String)> = out
        .iter()
        .filter(|p| p.is_change())
        .map(|p| (parent_key(&p.old_path), p.old_name.to_lowercase()))
        .collect();
    for p in &mut out {
        if p.problem.is_none()
            && p.is_change()
            && existing.contains(&parent_key(&p.old_path), &p.new_name.to_lowercase())
            && !vacated.contains(&final_key(p))
        {
            p.problem = Some(RenameProblem::CollidesWithExisting);
        }
    }
    out
}

fn preview_failed(item: &RenameInput, msg: &str) -> RenamePreview {
    let old_name = name_lossy(&item.path);
    RenamePreview {
        new_path: item.path.clone(),
        new_name: old_name.clone(),
        old_name,
        old_path: item.path.clone(),
        problem: Some(RenameProblem::Rule(msg.to_string())),
    }
}

fn preview_one(index: usize, item: &RenameInput, rules: &[Compiled]) -> RenamePreview {
    let old_path = item.path.clone();
    let Some(old_name) = item.path.file_name().and_then(|n| n.to_str()) else {
        let lossy = name_lossy(&item.path);
        return RenamePreview {
            new_path: old_path.clone(),
            old_path,
            new_name: lossy.clone(),
            old_name: lossy,
            problem: Some(RenameProblem::NotUnicode),
        };
    };
    let old_name = old_name.to_string();
    let parent = item.path.parent().unwrap_or(Path::new(""));
    let parent_name = parent
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let (new_name, problem) = match run_rules(&old_name, index, item, &parent_name, rules) {
        Err(msg) => (old_name.clone(), Some(RenameProblem::Rule(msg))),
        Ok(new_name) => {
            let problem = if new_name != old_name {
                validate_name(&new_name)
            } else {
                None
            };
            (new_name, problem)
        }
    };
    RenamePreview {
        new_path: parent.join(&new_name),
        old_path,
        old_name,
        new_name,
        problem,
    }
}

/// Checks a single Windows file name component.
fn validate_name(name: &str) -> Option<RenameProblem> {
    if name.is_empty() {
        return Some(RenameProblem::Empty);
    }
    if let Some(c) = name
        .chars()
        .find(|c| INVALID_CHARS.contains(c) || (*c as u32) < 0x20)
    {
        return Some(RenameProblem::InvalidChars(c));
    }
    // Windows ignores trailing spaces and everything after the first dot when checking
    // device names: "con .txt" and "NUL.tar.gz" are reserved.
    let base = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(base)) {
        return Some(RenameProblem::ReservedName(base.to_string()));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some(RenameProblem::TrailingDotOrSpace);
    }
    let units = name.encode_utf16().count();
    if units > MAX_NAME_UNITS {
        return Some(RenameProblem::TooLong(units));
    }
    None
}

// ---------------------------------------------------------------------------
// Rule compilation and evaluation
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Compiled {
    Template {
        pieces: Vec<Piece>,
        start: u64,
        step: u64,
    },
    /// `None` when `find` is empty (no-op).
    Replace(Option<Regex>, String),
    Regex(Regex, String),
    ChangeCase(CaseMode),
    ExtensionCase(CaseMode),
    Trim,
    Insert {
        at: usize,
        text: String,
    },
    RemoveRange {
        start: usize,
        len: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Lit(String),
    Name,
    Ext,
    Parent,
    Counter { width: usize },
    Date(String),
    Meta(String),
}

fn compile(rules: &[RenameRule]) -> Result<Vec<Compiled>, String> {
    rules
        .iter()
        .map(|rule| -> Result<Compiled, String> {
            Ok(match rule {
                RenameRule::Template {
                    pattern,
                    start,
                    step,
                } => Compiled::Template {
                    pieces: parse_template(pattern)?,
                    start: *start,
                    step: *step,
                },
                RenameRule::Replace {
                    find,
                    replace,
                    case_sensitive,
                } => {
                    let re = if find.is_empty() {
                        None
                    } else {
                        Some(
                            RegexBuilder::new(&regex::escape(find))
                                .case_insensitive(!case_sensitive)
                                .build()
                                .map_err(|e| format!("invalid find text: {e}"))?,
                        )
                    };
                    // Escape '$' so the replacement is taken literally.
                    Compiled::Replace(re, replace.replace('$', "$$"))
                }
                RenameRule::Regex {
                    pattern,
                    replacement,
                } => Compiled::Regex(
                    Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?,
                    replacement.clone(),
                ),
                RenameRule::ChangeCase(mode) => Compiled::ChangeCase(*mode),
                RenameRule::ExtensionCase(mode) => Compiled::ExtensionCase(*mode),
                RenameRule::Trim => Compiled::Trim,
                RenameRule::Insert { at, text } => Compiled::Insert {
                    at: *at,
                    text: text.clone(),
                },
                RenameRule::RemoveRange { start, len } => Compiled::RemoveRange {
                    start: *start,
                    len: *len,
                },
            })
        })
        .collect()
}

fn parse_template(p: &str) -> Result<Vec<Piece>, String> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push('}');
            }
            '{' => {
                let mut tok = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(ch) => tok.push(ch),
                        None => return Err("unclosed '{' in template".to_string()),
                    }
                }
                if !lit.is_empty() {
                    out.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                out.push(parse_token(&tok)?);
            }
            '}' => return Err("unmatched '}' in template".to_string()),
            _ => lit.push(c),
        }
    }
    if !lit.is_empty() {
        out.push(Piece::Lit(lit));
    }
    Ok(out)
}

fn parse_token(tok: &str) -> Result<Piece, String> {
    match tok {
        "name" => Ok(Piece::Name),
        "ext" => Ok(Piece::Ext),
        "parent" => Ok(Piece::Parent),
        "n" => Ok(Piece::Counter { width: 0 }),
        _ => {
            if let Some(w) = tok.strip_prefix("n:") {
                let width = w
                    .parse::<usize>()
                    .map_err(|_| format!("bad counter width in {{{tok}}}"))?;
                Ok(Piece::Counter { width })
            } else if let Some(fmt) = tok.strip_prefix("date:") {
                if fmt.is_empty() {
                    Err("empty date format in template".to_string())
                } else {
                    Ok(Piece::Date(fmt.to_string()))
                }
            } else if let Some(key) = tok.strip_prefix("meta:") {
                Ok(Piece::Meta(key.to_string()))
            } else {
                Err(format!("unknown template token {{{tok}}}"))
            }
        }
    }
}

struct Ctx<'a> {
    stem: &'a str,
    ext: &'a str,
    parent: &'a str,
    index: usize,
    input: &'a RenameInput,
}

fn render(pieces: &[Piece], start: u64, step: u64, ctx: &Ctx<'_>) -> Result<String, String> {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Lit(s) => out.push_str(s),
            Piece::Name => out.push_str(ctx.stem),
            Piece::Ext => out.push_str(ctx.ext),
            Piece::Parent => out.push_str(ctx.parent),
            Piece::Counter { width } => {
                let value = start.saturating_add(step.saturating_mul(ctx.index as u64));
                let _ = write!(out, "{value:0width$}", width = *width);
            }
            Piece::Date(fmt) => {
                let date = ctx
                    .input
                    .date
                    .ok_or_else(|| "this item has no date for {date:...}".to_string())?;
                write!(out, "{}", date.format(fmt))
                    .map_err(|_| format!("invalid date format '{fmt}'"))?;
            }
            Piece::Meta(key) => {
                let value = ctx
                    .input
                    .meta
                    .get(key)
                    .ok_or_else(|| format!("no metadata value for '{key}'"))?;
                out.push_str(value);
            }
        }
    }
    Ok(out)
}

fn run_rules(
    name: &str,
    index: usize,
    item: &RenameInput,
    parent_name: &str,
    rules: &[Compiled],
) -> Result<String, String> {
    let (orig_stem, orig_ext) = split_name(name);
    let ctx = Ctx {
        stem: orig_stem,
        ext: orig_ext.unwrap_or(""),
        parent: parent_name,
        index,
        input: item,
    };
    let mut cur = name.to_string();
    for rule in rules {
        cur = match rule {
            Compiled::Template {
                pieces,
                start,
                step,
            } => render(pieces, *start, *step, &ctx)?,
            Compiled::Replace(re, replacement) => match re {
                Some(re) => re.replace_all(&cur, replacement.as_str()).into_owned(),
                None => cur,
            },
            Compiled::Regex(re, replacement) => {
                re.replace_all(&cur, replacement.as_str()).into_owned()
            }
            Compiled::ChangeCase(mode) => {
                let (stem, ext) = split_name(&cur);
                join_name(&apply_case(stem, *mode), ext)
            }
            Compiled::ExtensionCase(mode) => {
                let (stem, ext) = split_name(&cur);
                match ext {
                    Some(e) => join_name(stem, Some(&apply_case(e, *mode))),
                    None => cur,
                }
            }
            Compiled::Trim => {
                let (stem, ext) = split_name(&cur);
                join_name(stem.trim(), ext)
            }
            Compiled::Insert { at, text } => {
                let (stem, ext) = split_name(&cur);
                let b = char_byte_index(stem, *at);
                let new_stem = format!("{}{}{}", &stem[..b], text, &stem[b..]);
                join_name(&new_stem, ext)
            }
            Compiled::RemoveRange { start, len } => {
                let (stem, ext) = split_name(&cur);
                let b0 = char_byte_index(stem, *start);
                let b1 = char_byte_index(stem, start.saturating_add(*len));
                let new_stem = if b0 < b1 {
                    format!("{}{}", &stem[..b0], &stem[b1..])
                } else {
                    stem.to_string()
                };
                join_name(&new_stem, ext)
            }
        };
    }
    Ok(cur)
}

/// Splits a name at its last dot. A leading dot (".bashrc") does not start an extension.
fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i + 1..])),
        _ => (name, None),
    }
}

fn join_name(stem: &str, ext: Option<&str>) -> String {
    match ext {
        Some(e) => format!("{stem}.{e}"),
        None => stem.to_string(),
    }
}

fn char_byte_index(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(b, _)| b)
}

fn apply_case(s: &str, mode: CaseMode) -> String {
    match mode {
        CaseMode::Lower => s.to_lowercase(),
        CaseMode::Upper => s.to_uppercase(),
        CaseMode::Title => {
            let mut out = String::with_capacity(s.len());
            let mut boundary = true;
            for c in s.chars() {
                if boundary {
                    out.extend(c.to_uppercase());
                } else {
                    out.extend(c.to_lowercase());
                }
                boundary = !(c.is_alphanumeric() || c == '\'');
            }
            out
        }
    }
}

fn name_lossy(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Case-folded directory key. Windows paths are case-insensitive.
fn dir_key(dir: &Path) -> String {
    dir.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn parent_key(path: &Path) -> String {
    path.parent().map(dir_key).unwrap_or_default()
}

fn final_key(p: &RenamePreview) -> (String, String) {
    (parent_key(&p.old_path), p.new_name.to_lowercase())
}

// ---------------------------------------------------------------------------
// Apply / undo (filesystem)
// ---------------------------------------------------------------------------

/// Applies the previews. Refuses (touching nothing) if any preview has a problem.
///
/// Unchanged items are skipped. Changed items are moved to unique temporary names in
/// their own folder first, then to their final names, so chains and cycles are safe. A
/// target that exists at the moment of the second phase is treated as a failure, never
/// overwritten. On failure, completed steps are reverted in reverse order.
pub fn apply(previews: &[RenamePreview]) -> RenameReport {
    let blocked: Vec<PathBuf> = previews
        .iter()
        .filter(|p| p.problem.is_some())
        .map(|p| p.old_path.clone())
        .collect();
    if !blocked.is_empty() {
        return RenameReport::with_status(RenameStatus::Refused { blocked });
    }

    let pairs: Vec<(PathBuf, PathBuf)> = previews
        .iter()
        .filter(|p| p.is_change())
        .map(|p| (p.old_path.clone(), p.new_path.clone()))
        .collect();

    // Defensive: two previews must not share a target (case-insensitive). The preview
    // functions already flag this; this guards hand-built previews.
    let mut seen = HashSet::new();
    let dup_sources: Vec<PathBuf> = pairs
        .iter()
        .filter(|(_, to)| !seen.insert(to.to_string_lossy().to_lowercase()))
        .map(|(from, _)| from.clone())
        .collect();
    if !dup_sources.is_empty() {
        return RenameReport::with_status(RenameStatus::Refused {
            blocked: dup_sources,
        });
    }

    execute(&pairs)
}

/// Reverts a previous successful [`apply`]. `pairs` is the report's `undo` list, i.e.
/// `(new_path, old_path)`. Uses the same two-phase, all-or-nothing procedure.
pub fn undo(pairs: &[(PathBuf, PathBuf)]) -> RenameReport {
    execute(pairs)
}

/// Moves each `(from, to)` pair with the same two-phase, all-or-nothing procedure as
/// [`apply`], without previews: a single rename typed by the user, for example. Every step
/// refuses to replace an existing entry, and a case-only rename (`a.jpg` -> `A.jpg`) goes
/// through a temporary name, so it never meets itself.
pub fn rename_paths(pairs: &[(PathBuf, PathBuf)]) -> RenameReport {
    execute(pairs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
    Original,
    Temp,
    Target,
}

fn execute(pairs: &[(PathBuf, PathBuf)]) -> RenameReport {
    let pairs: Vec<(PathBuf, PathBuf)> = pairs.iter().filter(|(a, b)| a != b).cloned().collect();
    if pairs.is_empty() {
        return RenameReport::with_status(RenameStatus::NothingToDo);
    }
    let n = pairs.len();
    let temps: Vec<PathBuf> = pairs
        .iter()
        .enumerate()
        .map(|(i, (from, _))| temp_path_for(from, i))
        .collect();
    let mut loc = vec![Loc::Original; n];

    // Phase 1: every source to its temporary name. This breaks all chains and cycles.
    let mut failure: Option<(usize, io::Error)> = None;
    for i in 0..n {
        match rename_no_replace(&pairs[i].0, &temps[i]) {
            Ok(()) => loc[i] = Loc::Temp,
            Err(e) => {
                failure = Some((i, e));
                break;
            }
        }
    }

    // Phase 2: temporary names to final names. Never overwrite an existing entry: the move
    // itself refuses an existing target (no check-then-rename window), so a concurrent rename
    // or a file created meanwhile is never replaced.
    if failure.is_none() {
        for i in 0..n {
            match rename_no_replace(&temps[i], &pairs[i].1) {
                Ok(()) => loc[i] = Loc::Target,
                Err(e) => {
                    failure = Some((i, e));
                    break;
                }
            }
        }
    }

    let Some((failed_idx, err)) = failure else {
        let undo = pairs.iter().map(|(a, b)| (b.clone(), a.clone())).collect();
        return RenameReport {
            status: RenameStatus::Completed,
            renamed: pairs,
            reverted: Vec::new(),
            stranded: Vec::new(),
            undo,
        };
    };

    let was_target: Vec<bool> = loc.iter().map(|l| *l == Loc::Target).collect();
    let stranded = revert(&pairs, &temps, &mut loc);
    let reverted: Vec<(PathBuf, PathBuf)> = (0..n)
        .filter(|&i| was_target[i] && loc[i] == Loc::Original)
        .map(|i| pairs[i].clone())
        .collect();
    let failed = pairs[failed_idx].0.clone();
    let error = err.to_string();
    let status = if stranded.is_empty() {
        RenameStatus::RolledBack { failed, error }
    } else {
        RenameStatus::RollbackIncomplete { failed, error }
    };
    RenameReport {
        status,
        renamed: Vec::new(),
        reverted,
        stranded,
        undo: Vec::new(),
    }
}

/// Moves every item back to its original path. Targets go back to temporary names first,
/// then temporaries go back to their originals, so no step can collide with another.
fn revert(pairs: &[(PathBuf, PathBuf)], temps: &[PathBuf], loc: &mut [Loc]) -> Vec<StrandedFile> {
    let n = pairs.len();
    let mut stranded = Vec::new();
    for i in (0..n).rev() {
        if loc[i] == Loc::Target {
            match rename_no_replace(&pairs[i].1, &temps[i]) {
                Ok(()) => loc[i] = Loc::Temp,
                Err(e) => stranded.push(StrandedFile {
                    current: pairs[i].1.clone(),
                    original: pairs[i].0.clone(),
                    error: e.to_string(),
                }),
            }
        }
    }
    for i in (0..n).rev() {
        if loc[i] == Loc::Temp {
            match rename_no_replace(&temps[i], &pairs[i].0) {
                Ok(()) => loc[i] = Loc::Original,
                Err(e) => stranded.push(StrandedFile {
                    current: temps[i].clone(),
                    original: pairs[i].0.clone(),
                    error: e.to_string(),
                }),
            }
        }
    }
    stranded
}

/// A free temporary name in the same folder as `from`.
fn temp_path_for(from: &Path, index: usize) -> PathBuf {
    // A per-process sequence number keeps two renames running at once from picking the same
    // name (the move into it refuses an existing entry anyway, so a clash fails, never
    // overwrites).
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let dir = from.parent().unwrap_or(Path::new(""));
    let pid = std::process::id();
    let run = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut k = 0usize;
    loop {
        let cand = dir.join(format!(".forge-rename-{pid}-{run}-{index}-{k}.tmp"));
        if fs::symlink_metadata(&cand).is_err() {
            return cand;
        }
        k += 1;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str) -> RenameInput {
        RenameInput::new(PathBuf::from("dir").join(path))
    }

    fn names(previews: &[RenamePreview]) -> Vec<String> {
        previews.iter().map(|p| p.new_name.clone()).collect()
    }

    fn one(name: &str, rules: &[RenameRule]) -> RenamePreview {
        preview(&[item(name)], rules).remove(0)
    }

    fn template(p: &str) -> RenameRule {
        RenameRule::Template {
            pattern: p.to_string(),
            start: 1,
            step: 1,
        }
    }

    fn replace(find: &str, rep: &str, cs: bool) -> RenameRule {
        RenameRule::Replace {
            find: find.to_string(),
            replace: rep.to_string(),
            case_sensitive: cs,
        }
    }

    // --- templates ---------------------------------------------------------

    #[test]
    fn template_name_ext_and_counter_padding() {
        let items = [item("a.jpg"), item("holiday.PNG")];
        let rules = [template("{name}_{n:3}.{ext}")];
        let p = preview(&items, &rules);
        assert_eq!(names(&p), ["a_001.jpg", "holiday_002.PNG"]);
        assert!(p.iter().all(|x| x.problem.is_none()));

        // A file without an extension gets a trailing dot, which Windows rejects.
        let p = preview(&[item("noext")], &rules).remove(0);
        assert_eq!(p.new_name, "noext_001.");
        assert_eq!(p.problem, Some(RenameProblem::TrailingDotOrSpace));
    }

    #[test]
    fn counter_start_step_and_plain_n() {
        let items = [item("a.txt"), item("b.txt"), item("c.txt")];
        let rules = [RenameRule::Template {
            pattern: "img{n}".into(),
            start: 10,
            step: 5,
        }];
        assert_eq!(names(&preview(&items, &rules)), ["img10", "img15", "img20"]);
    }

    #[test]
    fn counter_wider_than_padding_is_not_truncated() {
        let items = [item("a.txt")];
        let rules = [RenameRule::Template {
            pattern: "{n:2}".into(),
            start: 1234,
            step: 0,
        }];
        assert_eq!(names(&preview(&items, &rules)), ["1234"]);
    }

    #[test]
    fn template_date_parent_and_meta() {
        let mut meta = HashMap::new();
        meta.insert("camera".to_string(), "X100".to_string());
        let input = RenameInput {
            path: PathBuf::from("Trips").join("beach.jpg"),
            date: NaiveDate::from_ymd_opt(2024, 5, 6),
            meta,
        };
        let rules = [template(
            "{date:%Y%m%d}_{parent}_{meta:camera}_{name}.{ext}",
        )];
        let p = preview(&[input], &rules).remove(0);
        assert_eq!(p.problem, None);
        assert_eq!(p.new_name, "20240506_Trips_X100_beach.jpg");
    }

    #[test]
    fn template_missing_date_or_meta_is_a_rule_problem() {
        let rules = [template("{date:%Y}")];
        let p = preview(&[item("a.jpg")], &rules).remove(0);
        assert!(matches!(p.problem, Some(RenameProblem::Rule(_))));

        let rules = [template("{meta:nope}")];
        let p = preview(&[item("a.jpg")], &rules).remove(0);
        assert!(matches!(p.problem, Some(RenameProblem::Rule(_))));
    }

    #[test]
    fn template_bad_syntax_is_reported_for_every_item() {
        for pat in [
            "{unknown}",
            "{n:x}",
            "open {name",
            "close }",
            "{}",
            "{date:}",
        ] {
            let p = preview(&[item("a.txt"), item("b.txt")], &[template(pat)]);
            assert!(
                p.iter()
                    .all(|x| matches!(x.problem, Some(RenameProblem::Rule(_)))),
                "pattern {pat:?} should fail"
            );
            assert!(p.iter().all(|x| x.new_name == x.old_name));
        }
    }

    #[test]
    fn template_escaped_braces() {
        assert_eq!(one("a.txt", &[template("{{{name}}}")]).new_name, "{a}");
    }

    #[test]
    fn template_name_and_ext_use_original_name_not_intermediate() {
        let rules = [replace("a", "XYZ", true), template("{name}")];
        assert_eq!(one("abc.txt", &rules).new_name, "abc");
    }

    #[test]
    fn template_unicode_stem() {
        let p = one("日本語 写真.jpg", &[template("{name}_{n:2}.{ext}")]);
        assert_eq!(p.new_name, "日本語 写真_01.jpg");
        assert_eq!(p.problem, None);
    }

    // --- replace / regex -----------------------------------------------------

    #[test]
    fn replace_case_sensitive_and_insensitive() {
        assert_eq!(
            one("Photo photo.txt", &[replace("photo", "img", true)]).new_name,
            "Photo img.txt"
        );
        assert_eq!(
            one("Photo photo.txt", &[replace("PHOTO", "img", false)]).new_name,
            "img img.txt"
        );
    }

    #[test]
    fn replace_treats_find_text_literally_and_dollar_is_literal() {
        assert_eq!(
            one("a.b+c.txt", &[replace(".b+", "$1", true)]).new_name,
            "a$1c.txt"
        );
    }

    #[test]
    fn replace_with_empty_find_is_noop() {
        assert_eq!(
            one("abc.txt", &[replace("", "x", true)]).new_name,
            "abc.txt"
        );
    }

    #[test]
    fn regex_groups_expand() {
        let rules = [RenameRule::Regex {
            pattern: r"^(\w+)_(\d+)".into(),
            replacement: "${2}_${1}".into(),
        }];
        assert_eq!(one("IMG_12.jpg", &rules).new_name, "12_IMG.jpg");

        let rules = [RenameRule::Regex {
            pattern: r"(?P<y>\d{4})-(?P<m>\d{2})".into(),
            replacement: "${m}/${y}".into(),
        }];
        assert_eq!(
            one("2024-05 notes.txt", &rules).new_name,
            "05/2024 notes.txt"
        );
    }

    #[test]
    fn regex_invalid_pattern_is_rule_problem() {
        let rules = [RenameRule::Regex {
            pattern: "(unclosed".into(),
            replacement: "x".into(),
        }];
        let p = one("a.txt", &rules);
        assert!(matches!(p.problem, Some(RenameProblem::Rule(_))));
    }

    // --- case, trim, insert, remove -----------------------------------------

    #[test]
    fn change_case_affects_stem_only() {
        assert_eq!(
            one("MyFile.JPG", &[RenameRule::ChangeCase(CaseMode::Lower)]).new_name,
            "myfile.JPG"
        );
        assert_eq!(
            one("myFile.jpg", &[RenameRule::ChangeCase(CaseMode::Upper)]).new_name,
            "MYFILE.jpg"
        );
    }

    #[test]
    fn title_case_words() {
        assert_eq!(
            one(
                "hello big-world_x don't.txt",
                &[RenameRule::ChangeCase(CaseMode::Title)]
            )
            .new_name,
            "Hello Big-World_X Don't.txt"
        );
    }

    #[test]
    fn extension_case() {
        assert_eq!(
            one("a.JPG", &[RenameRule::ExtensionCase(CaseMode::Lower)]).new_name,
            "a.jpg"
        );
        assert_eq!(
            one("Ünïcode.TXT", &[RenameRule::ExtensionCase(CaseMode::Lower)]).new_name,
            "Ünïcode.txt"
        );
        assert_eq!(
            one("README", &[RenameRule::ExtensionCase(CaseMode::Lower)]).new_name,
            "README"
        );
    }

    #[test]
    fn case_only_change_is_a_change() {
        let p = one("a.JPG", &[RenameRule::ExtensionCase(CaseMode::Lower)]);
        assert!(p.is_change());
        assert_eq!(p.problem, None);
    }

    #[test]
    fn trim_stem_whitespace() {
        assert_eq!(one("  a  .txt", &[RenameRule::Trim]).new_name, "a.txt");
        assert_eq!(one("  .bashrc", &[RenameRule::Trim]).new_name, ".bashrc");
    }

    #[test]
    fn insert_and_remove_unicode_aware_and_clamped() {
        let ins = RenameRule::Insert {
            at: 1,
            text: "_".into(),
        };
        assert_eq!(one("日本語.txt", &[ins]).new_name, "日_本語.txt");

        let ins_past_end = RenameRule::Insert {
            at: 99,
            text: "!".into(),
        };
        assert_eq!(one("ab.txt", &[ins_past_end]).new_name, "ab!.txt");

        let rm = RenameRule::RemoveRange { start: 1, len: 1 };
        assert_eq!(one("日本語.txt", &[rm]).new_name, "日語.txt");

        let rm_all = RenameRule::RemoveRange {
            start: 0,
            len: usize::MAX,
        };
        assert_eq!(one("abc.txt", &[rm_all]).new_name, ".txt");

        let rm_none = RenameRule::RemoveRange { start: 50, len: 2 };
        assert_eq!(one("abc.txt", &[rm_none]).new_name, "abc.txt");
    }

    #[test]
    fn rules_apply_in_order() {
        let rules = [
            RenameRule::Trim,
            RenameRule::ChangeCase(CaseMode::Title),
            replace(" ", "_", true),
            RenameRule::ExtensionCase(CaseMode::Lower),
        ];
        assert_eq!(one(" my holiday .JPG", &rules).new_name, "My_Holiday.jpg");
    }

    #[test]
    fn unicode_names_pass_through_validation() {
        let p = one("ファイル 1.txt", &[template("{name}_{n}.{ext}")]);
        assert_eq!(p.new_name, "ファイル 1_1.txt");
        assert_eq!(p.problem, None);
    }

    // --- validation ----------------------------------------------------------

    fn problem_for(new: &str) -> Option<RenameProblem> {
        one("source.txt", &[template(new)]).problem
    }

    #[test]
    fn validation_empty_and_invalid_chars() {
        assert_eq!(problem_for(""), Some(RenameProblem::Empty));
        assert_eq!(problem_for("a:b"), Some(RenameProblem::InvalidChars(':')));
        assert_eq!(problem_for("a/b"), Some(RenameProblem::InvalidChars('/')));
        assert_eq!(problem_for("a?b"), Some(RenameProblem::InvalidChars('?')));
        assert_eq!(
            problem_for("a\u{1}b"),
            Some(RenameProblem::InvalidChars('\u{1}'))
        );
    }

    #[test]
    fn validation_reserved_names() {
        for n in [
            "CON",
            "con.txt",
            "Prn",
            "aux.tar.gz",
            "NUL",
            "COM1.log",
            "lpt9",
            "con .txt",
        ] {
            assert!(
                matches!(problem_for(n), Some(RenameProblem::ReservedName(_))),
                "{n} should be reserved"
            );
        }
        assert_eq!(problem_for("CONSOLE.txt"), None);
        assert_eq!(problem_for("COM10.txt"), None);
        assert_eq!(problem_for("my_con.txt"), None);
    }

    #[test]
    fn validation_trailing_dot_or_space() {
        assert_eq!(problem_for("a."), Some(RenameProblem::TrailingDotOrSpace));
        assert_eq!(problem_for("a "), Some(RenameProblem::TrailingDotOrSpace));
        assert_eq!(problem_for("."), Some(RenameProblem::TrailingDotOrSpace));
    }

    #[test]
    fn validation_too_long_counts_utf16() {
        let ok = "a".repeat(255);
        assert_eq!(problem_for(&ok), None);
        let too_long = "a".repeat(256);
        assert_eq!(problem_for(&too_long), Some(RenameProblem::TooLong(256)));
        // 130 two-unit-free chars would be 130 units; 128 U+1F600 emoji are 256 units.
        // U+1F600 is two UTF-16 units, so 128 of them already exceed the limit.
        let emoji = "\u{1F600}".repeat(128);
        assert_eq!(problem_for(&emoji), Some(RenameProblem::TooLong(256)));
        let emoji_ok = "\u{1F600}".repeat(127) + "a";
        assert_eq!(problem_for(&emoji_ok), None);
    }

    #[test]
    fn unchanged_names_are_not_validated() {
        // An existing odd name that the rules leave alone must not block the batch.
        let p = preview(&[item("CON.txt")], &[RenameRule::Trim]).remove(0);
        assert_eq!(p.problem, None);
        assert!(!p.is_change());
    }

    // --- duplicates and collisions ---------------------------------------------

    #[test]
    fn duplicate_in_batch_is_case_insensitive_and_per_folder() {
        let items = [item("A.txt"), item("a.txt")];
        let rules = [RenameRule::ChangeCase(CaseMode::Lower)];
        let p = preview(&items, &rules);
        assert!(
            p.iter()
                .all(|x| x.problem == Some(RenameProblem::DuplicateInBatch))
        );

        // Same name in different folders is fine.
        let items = [
            RenameInput::new(PathBuf::from("d1").join("x.txt")),
            RenameInput::new(PathBuf::from("d2").join("x.txt")),
        ];
        let p = preview(&items, &[template("same")]);
        assert!(p.iter().all(|x| x.problem.is_none()), "{p:?}");
    }

    #[test]
    fn duplicate_with_unchanged_item_is_flagged() {
        // "b.txt" stays put; "a.txt" is renamed to "b.txt" in the same folder.
        let items = [item("a.txt"), item("b.txt")];
        let rules = [RenameRule::Regex {
            pattern: "^a".into(),
            replacement: "b".into(),
        }];
        let p = preview(&items, &rules);
        assert_eq!(p[0].problem, Some(RenameProblem::DuplicateInBatch));
        assert_eq!(p[1].problem, Some(RenameProblem::DuplicateInBatch));
    }

    #[test]
    fn collision_with_existing_file_outside_batch() {
        let items = [item("a.txt")];
        let rules = [replace("a", "b", true)];
        let mut existing = ExistingNames::default();
        existing.add_dir(Path::new("dir"), ["a.txt", "B.TXT"]);
        let p = preview_with_existing(&items, &rules, &existing).remove(0);
        assert_eq!(p.problem, Some(RenameProblem::CollidesWithExisting));

        // Without the existing listing there is no collision to report.
        let p = preview(&items, &rules).remove(0);
        assert_eq!(p.problem, None);
    }

    /// Items whose target name comes from their own `meta["to"]`, so a batch can express
    /// an arbitrary mapping such as a -> b and b -> c.
    fn mapped(dir: &Path, pairs: &[(&str, &str)]) -> Vec<RenameInput> {
        pairs
            .iter()
            .map(|(from, to)| {
                let mut input = RenameInput::new(dir.join(from));
                input.meta.insert("to".to_string(), to.to_string());
                input
            })
            .collect()
    }

    #[test]
    fn collision_not_reported_when_target_is_vacated_in_batch() {
        // a -> b and b -> c: b is vacated by the second rename, so a -> b is fine.
        let dir = Path::new("dir");
        let items = mapped(dir, &[("a.txt", "b.txt"), ("b.txt", "c.txt")]);
        let mut existing = ExistingNames::default();
        existing.add_dir(dir, ["a.txt", "b.txt"]);
        let p = preview_with_existing(&items, &[template("{meta:to}")], &existing);
        assert!(p.iter().all(|x| x.problem.is_none()), "{p:?}");
        assert_eq!(names(&p), ["b.txt", "c.txt"]);
    }

    #[test]
    fn collision_reported_when_target_is_not_vacated() {
        // b.txt exists and is not part of the batch, so a -> b must be refused.
        let dir = Path::new("dir");
        let items = mapped(dir, &[("a.txt", "b.txt")]);
        let mut existing = ExistingNames::default();
        existing.add_dir(dir, ["a.txt", "b.txt"]);
        let p = preview_with_existing(&items, &[template("{meta:to}")], &existing);
        assert_eq!(p[0].problem, Some(RenameProblem::CollidesWithExisting));
    }

    #[test]
    fn case_only_rename_does_not_collide_with_itself() {
        let mut existing = ExistingNames::default();
        existing.add_dir(Path::new("dir"), ["a.JPG"]);
        let p = preview_with_existing(
            &[item("a.JPG")],
            &[RenameRule::ExtensionCase(CaseMode::Lower)],
            &existing,
        )
        .remove(0);
        assert_eq!(p.new_name, "a.jpg");
        assert_eq!(p.problem, None);
    }

    #[test]
    fn unchanged_item_does_not_collide_with_its_own_name() {
        let mut existing = ExistingNames::default();
        existing.add_dir(Path::new("dir"), ["keep.txt"]);
        let p =
            preview_with_existing(&[item("keep.txt")], &[RenameRule::Trim], &existing).remove(0);
        assert_eq!(p.problem, None);
    }

    #[test]
    fn unchanged_name_blocks_rename_onto_it() {
        let mut existing = ExistingNames::default();
        existing.add_dir(Path::new("dir"), ["b.txt"]);
        let p = preview_with_existing(&[item("a.txt")], &[replace("a", "b", true)], &existing);
        assert_eq!(p[0].problem, Some(RenameProblem::CollidesWithExisting));
    }

    // --- apply / undo on a real folder -----------------------------------------

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn rename_previews(dir: &Path, names: &[&str], rules: &[RenameRule]) -> Vec<RenamePreview> {
        let items: Vec<RenameInput> = names
            .iter()
            .map(|n| RenameInput::new(dir.join(n)))
            .collect();
        let mut existing = ExistingNames::default();
        existing.add_dir(dir, names.iter().copied());
        preview_with_existing(&items, rules, &existing)
    }

    /// Previews for a mapping in `dir`, with existing names of the folder filled in.
    fn previews_for(dir: &Path, pairs: &[(&str, &str)]) -> Vec<RenamePreview> {
        let items = mapped(dir, pairs);
        let mut existing = ExistingNames::default();
        let all: Vec<&str> = pairs.iter().map(|(f, _)| *f).collect();
        existing.add_dir(dir, all);
        preview_with_existing(&items, &[template("{meta:to}")], &existing)
    }

    #[test]
    fn apply_swaps_two_files() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.txt", "A");
        write(d.path(), "b.txt", "B");
        let previews = previews_for(d.path(), &[("a.txt", "b.txt"), ("b.txt", "a.txt")]);
        assert!(previews.iter().all(|p| p.problem.is_none()), "{previews:?}");

        let report = apply(&previews);
        assert_eq!(report.status, RenameStatus::Completed);
        assert_eq!(read(&d.path().join("a.txt")), "B");
        assert_eq!(read(&d.path().join("b.txt")), "A");
        assert_eq!(report.renamed.len(), 2);
        assert!(report.reverted.is_empty() && report.stranded.is_empty());
    }

    #[test]
    fn rename_paths_does_a_case_only_rename() {
        let d = tempfile::tempdir().unwrap();
        let lower = d.path().join("a.jpg");
        fs::write(&lower, b"photo").unwrap();
        let upper = d.path().join("A.jpg");
        let r = rename_paths(&[(lower.clone(), upper.clone())]);
        assert_eq!(r.status, RenameStatus::Completed);
        let names: Vec<String> = fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["A.jpg"]);
        assert_eq!(fs::read(&upper).unwrap(), b"photo");
    }

    #[test]
    fn rename_paths_never_replaces_an_existing_target() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.txt"), d.path().join("b.txt"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let r = rename_paths(&[(a.clone(), b.clone())]);
        assert!(matches!(r.status, RenameStatus::RolledBack { .. }), "{:?}", r.status);
        assert_eq!(fs::read(&a).unwrap(), b"a");
        assert_eq!(fs::read(&b).unwrap(), b"b");
    }

    #[test]
    fn two_concurrent_renames_to_one_name_lose_no_data() {
        for _ in 0..20 {
            let d = tempfile::tempdir().unwrap();
            let (a, b) = (d.path().join("a.txt"), d.path().join("b.txt"));
            let target = d.path().join("final.txt");
            fs::write(&a, b"from a").unwrap();
            fs::write(&b, b"from b").unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let run = |from: PathBuf| {
                let (barrier, target) = (barrier.clone(), target.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    rename_paths(&[(from, target)]).status
                })
            };
            let (first, second) = (run(a.clone()), run(b.clone()));
            let statuses = [first.join().unwrap(), second.join().unwrap()];
            let done = statuses.iter().filter(|s| **s == RenameStatus::Completed).count();
            assert_eq!(done, 1, "{statuses:?}");
            // One file has the final name, the other is back under its own name.
            let mut contents: Vec<Vec<u8>> = fs::read_dir(d.path())
                .unwrap()
                .map(|e| fs::read(e.unwrap().path()).unwrap())
                .collect();
            contents.sort();
            assert_eq!(contents, [b"from a".to_vec(), b"from b".to_vec()]);
            assert!(target.exists());
        }
    }

    #[test]
    fn apply_three_cycle_and_undo() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.txt", "A");
        write(d.path(), "b.txt", "B");
        write(d.path(), "c.txt", "C");
        let previews = previews_for(
            d.path(),
            &[("a.txt", "b.txt"), ("b.txt", "c.txt"), ("c.txt", "a.txt")],
        );
        assert!(previews.iter().all(|p| p.problem.is_none()), "{previews:?}");
        let report = apply(&previews);
        assert_eq!(report.status, RenameStatus::Completed);
        assert_eq!(read(&d.path().join("b.txt")), "A");
        assert_eq!(read(&d.path().join("c.txt")), "B");
        assert_eq!(read(&d.path().join("a.txt")), "C");

        let undone = undo(&report.undo);
        assert_eq!(undone.status, RenameStatus::Completed);
        assert_eq!(read(&d.path().join("a.txt")), "A");
        assert_eq!(read(&d.path().join("b.txt")), "B");
        assert_eq!(read(&d.path().join("c.txt")), "C");
        assert_eq!(
            fs::read_dir(d.path()).unwrap().count(),
            3,
            "no temporary files may remain"
        );
    }

    #[test]
    fn apply_refuses_when_target_is_an_unchanged_file() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.txt", "A");
        write(d.path(), "b.txt", "B");
        // a -> b while b stays put: both end up named b.txt.
        let previews = previews_for(d.path(), &[("a.txt", "b.txt"), ("b.txt", "b.txt")]);
        assert!(previews.iter().all(|p| p.problem.is_some()));
        let report = apply(&previews);
        assert!(matches!(report.status, RenameStatus::Refused { .. }));
        assert_eq!(read(&d.path().join("a.txt")), "A");
        assert_eq!(read(&d.path().join("b.txt")), "B");
    }

    #[test]
    fn apply_chain_moves_each_file_once() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.txt", "A");
        write(d.path(), "b.txt", "B");
        // a -> b and b -> c, where c is free.
        let previews = previews_for(d.path(), &[("a.txt", "b.txt"), ("b.txt", "c.txt")]);
        assert!(previews.iter().all(|p| p.problem.is_none()), "{previews:?}");
        let report = apply(&previews);
        assert_eq!(report.status, RenameStatus::Completed);
        assert_eq!(read(&d.path().join("c.txt")), "B");
        assert_eq!(read(&d.path().join("b.txt")), "A");
        assert!(!d.path().join("a.txt").exists());
    }

    #[test]
    fn apply_case_only_rename_on_disk() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.JPG", "pic");
        let previews = rename_previews(
            d.path(),
            &["a.JPG"],
            &[RenameRule::ExtensionCase(CaseMode::Lower)],
        );
        assert_eq!(previews[0].problem, None);
        let report = apply(&previews);
        assert_eq!(report.status, RenameStatus::Completed);
        let names: Vec<String> = fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.jpg"]);
        assert_eq!(read(&d.path().join("a.jpg")), "pic");
    }

    #[test]
    fn apply_refuses_when_any_preview_has_problem() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.txt", "A");
        write(d.path(), "b.txt", "B");
        let previews = rename_previews(d.path(), &["a.txt", "b.txt"], &[template("{name}:x")]);
        let report = apply(&previews);
        match &report.status {
            RenameStatus::Refused { blocked } => assert_eq!(blocked.len(), 2),
            other => panic!("expected refusal, got {other:?}"),
        }
        assert_eq!(read(&d.path().join("a.txt")), "A");
        assert_eq!(read(&d.path().join("b.txt")), "B");
        assert!(report.renamed.is_empty() && report.undo.is_empty());
    }

    #[test]
    fn apply_rolls_back_when_target_appears_mid_batch() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.txt", "A");
        write(d.path(), "c.txt", "C");
        let previews = rename_previews(
            d.path(),
            &["a.txt", "c.txt"],
            &[RenameRule::Regex {
                pattern: r"^([ac])\.txt$".into(),
                replacement: "${1}2.txt".into(),
            }],
        );
        assert!(previews.iter().all(|p| p.problem.is_none()), "{previews:?}");
        // A file appears at c2.txt after the preview was built (race with another app).
        write(d.path(), "c2.txt", "intruder");

        let report = apply(&previews);
        match &report.status {
            RenameStatus::RolledBack { failed, .. } => {
                assert_eq!(failed, &d.path().join("c.txt"));
            }
            other => panic!("expected rollback, got {other:?}"),
        }
        assert_eq!(
            report.reverted.len(),
            1,
            "a.txt -> a2.txt was completed then reverted"
        );
        assert!(report.stranded.is_empty());
        assert_eq!(read(&d.path().join("a.txt")), "A");
        assert_eq!(read(&d.path().join("c.txt")), "C");
        assert_eq!(read(&d.path().join("c2.txt")), "intruder");
        assert!(!d.path().join("a2.txt").exists());
    }

    #[test]
    fn apply_never_overwrites_existing_target() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.txt", "A");
        write(d.path(), "b.txt", "B");
        let previews = vec![RenamePreview {
            old_path: d.path().join("a.txt"),
            new_path: d.path().join("b.txt"),
            old_name: "a.txt".into(),
            new_name: "b.txt".into(),
            problem: None,
        }];
        let report = apply(&previews);
        assert!(matches!(report.status, RenameStatus::RolledBack { .. }));
        assert_eq!(read(&d.path().join("a.txt")), "A");
        assert_eq!(read(&d.path().join("b.txt")), "B");
    }

    #[test]
    fn nothing_to_do_when_names_unchanged() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.txt", "A");
        let previews = rename_previews(d.path(), &["a.txt"], &[RenameRule::Trim]);
        let report = apply(&previews);
        assert_eq!(report.status, RenameStatus::NothingToDo);
        assert_eq!(read(&d.path().join("a.txt")), "A");
    }

    #[test]
    fn unicode_rename_on_disk() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "写真 一.JPG", "photo");
        let previews = rename_previews(
            d.path(),
            &["写真 一.JPG"],
            &[template("{name}_{n:3}.{ext}")],
        );
        assert_eq!(previews[0].new_name, "写真 一_001.JPG");
        let report = apply(&previews);
        assert_eq!(report.status, RenameStatus::Completed);
        assert_eq!(read(&d.path().join("写真 一_001.JPG")), "photo");
    }

    #[test]
    fn preview_paths_are_consistent_with_names() {
        let p = one("x.txt", &[template("y{n}")]);
        assert_eq!(p.new_path, PathBuf::from("dir").join("y1"));
        assert_eq!(p.old_path, PathBuf::from("dir").join("x.txt"));
    }
}

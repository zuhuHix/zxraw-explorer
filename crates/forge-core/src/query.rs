//! Search query language: tokenizer, recursive-descent parser, typed AST and an in-memory
//! evaluator for live directory listings.
//!
//! This module is platform independent and knows nothing about SQL; `forge-db` compiles
//! the [`Query`] AST to parameterized SQL.
//!
//! # Syntax
//!
//! | Input | Meaning |
//! |---|---|
//! | `sunset beach` | free words: case-insensitive substring of the full path (name included); all must match |
//! | `"big sur"` | quoted phrase (spaces kept, no wildcards) |
//! | `IMG_*.cr3`, `a?c` | a free word with `*`/`?` is a glob on the *file name* (whole-name match) |
//! | `-word`, `-field:value`, `NOT x` | negation |
//! | `a OR b`, `(a OR b) c` | `OR` (uppercase) binds looser than the implicit AND; `AND` is optional |
//! | `field:"quoted value"` | values with spaces |
//! | `ext:cr3,nef` | comma = any of (works for every field except `re:`) |
//! | `>`, `>=`, `<`, `<=`, `=`, `a..b`, `a..`, `..b` | comparisons / inclusive ranges |
//!
//! Fields (aliases in parentheses):
//!
//! * text: `name:` (`filename`), `path:`, `camera:` (`cam`), `lens:`, `codec:`, `notes:`
//!   (`note`, `comment`), `tag:` (`tags`; trailing `*` = prefix), `collection:` (`in`)
//! * `ext:` (`extension`), `kind:` (`type`): `file dir link image raw video audio sidecar
//!   document archive project other` (`image` also matches RAW)
//! * `re:` (`regex`): case-insensitive regex on the file name
//! * `size:` with binary units `b kb mb gb tb` (`100mb`, `1.5gb`, `>=10k`)
//! * dates: `modified:` (`mtime`), `created:` (`ctime`), `date:` (capture date, falling back
//!   to modified), `taken:` (capture date only). Values: `2024`, `2024-06`, `2024-06-01`,
//!   ranges `2024-01..2024-06`, keywords `today yesterday thisweek lastweek thismonth
//!   lastmonth thisyear lastyear`, and ages `12h 7d 2w 3mo 1y` (`7d` = within the last 7
//!   days; with comparators an age compares *age*: `>30d` = older than 30 days).
//! * numbers: `rating:` (`stars`, 0–5), `iso:`, `aperture:` (`f/2.8` ok), `focal:` (`35mm`
//!   ok), `width:`, `height:`, `bitdepth:`, `fps:`, `duration:` (`dur`, `length`; units
//!   `ms s m h` or `h:mm:ss`)
//! * `label:` (`color`): `red yellow green blue purple none`; `pick:` (`flag`): `pick reject none`
//! * `missing:` / `has:`: `gps rating label tags meta hash date camera notes`
//! * `is:`: `dup offline online pick reject rated labeled tagged vfr hdr file dir link`
//!
//! Single-letter "fields" such as `C:\photos` are treated as free text so Windows paths
//! can be typed directly. Backslashes are literal everywhere (Windows paths); use quotes
//! for values containing spaces.

use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Local, Months, NaiveDate, TimeZone};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::model::{EntryKind, FileEntry, MediaKind};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

// ---------------------------------------------------------------------------
// Shared annotation vocabulary (also used by forge-db)
// ---------------------------------------------------------------------------

/// Color label, Lightroom/Bridge style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorLabel {
    Red,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl ColorLabel {
    pub const ALL: [ColorLabel; 5] = [
        ColorLabel::Red,
        ColorLabel::Yellow,
        ColorLabel::Green,
        ColorLabel::Blue,
        ColorLabel::Purple,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ColorLabel::Red => "red",
            ColorLabel::Yellow => "yellow",
            ColorLabel::Green => "green",
            ColorLabel::Blue => "blue",
            ColorLabel::Purple => "purple",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

impl fmt::Display for ColorLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pick flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PickState {
    #[default]
    None,
    Pick,
    Reject,
}

impl PickState {
    /// Storage encoding: 1 = pick, 0 = none, -1 = reject.
    pub fn as_i64(self) -> i64 {
        match self {
            PickState::None => 0,
            PickState::Pick => 1,
            PickState::Reject => -1,
        }
    }
    pub fn from_i64(v: i64) -> Self {
        match v {
            1 => PickState::Pick,
            -1 => PickState::Reject,
            _ => PickState::None,
        }
    }
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// A parsed search query. An empty input parses to `Expr::And(vec![])`, which matches
/// everything.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// All must match. Empty = true.
    And(Vec<Expr>),
    /// Any must match. Empty = false.
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Term(Filter),
}

/// Numeric comparison. `Between` is inclusive on both ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmp<T> {
    Eq(T),
    Gt(T),
    Ge(T),
    Lt(T),
    Le(T),
    Between(T, T),
}

impl<T: PartialOrd + Copy> Cmp<T> {
    pub fn test(&self, v: T) -> bool {
        match *self {
            Cmp::Eq(x) => v == x,
            Cmp::Gt(x) => v > x,
            Cmp::Ge(x) => v >= x,
            Cmp::Lt(x) => v < x,
            Cmp::Le(x) => v <= x,
            Cmp::Between(a, b) => v >= a && v <= b,
        }
    }
}

impl Cmp<f64> {
    /// Like [`Cmp::test`] but `Eq` tolerates `eps` (frame rates, durations, apertures).
    pub fn test_approx(&self, v: f64, eps: f64) -> bool {
        match *self {
            Cmp::Eq(x) => (v - x).abs() <= eps,
            _ => self.test(v),
        }
    }
}

/// Half-open time interval `[start, end)` in Unix epoch **milliseconds**; `None` = unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl DateRange {
    pub fn contains(&self, ms: i64) -> bool {
        self.start.is_none_or(|s| ms >= s) && self.end.is_none_or(|e| ms < e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilter {
    File,
    Dir,
    Link,
    /// Regular file with this media classification.
    Media(MediaKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingField {
    Gps,
    Rating,
    Label,
    Tags,
    /// Neither image nor video metadata has been extracted.
    Meta,
    /// No full content hash.
    Hash,
    /// No capture date.
    Date,
    Camera,
    Notes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsFlag {
    /// Shares its full content hash with another present file.
    Dup,
    /// Known to the index but not found on disk (or on a disconnected volume).
    Offline,
    Online,
    Pick,
    Reject,
    Rated,
    Labeled,
    Tagged,
    Vfr,
    Hdr,
    File,
    Dir,
    Link,
}

/// Case-insensitive regex on the file name. Equality compares the pattern text.
#[derive(Debug, Clone)]
pub struct NameRegex {
    pattern: String,
    re: Regex,
}

impl NameRegex {
    pub fn new(pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            pattern: pattern.to_string(),
            re: build_name_regex(pattern)?,
        })
    }
    /// The pattern exactly as typed (compile it with [`build_name_regex`]).
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
    pub fn is_match(&self, s: &str) -> bool {
        self.re.is_match(s)
    }
}

impl PartialEq for NameRegex {
    fn eq(&self, o: &Self) -> bool {
        self.pattern == o.pattern
    }
}

/// The one place user regexes are compiled: case-insensitive, size-limited (user input
/// must not be able to exhaust memory).
pub fn build_name_regex(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 22)
        .build()
}

/// One search predicate. All text values are stored **lowercased** (comparisons are
/// case-insensitive).
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Free word / phrase: substring of the full path (which includes the name).
    Text(String),
    /// Substring of the file name.
    Name(String),
    /// Whole-name glob with `*` and `?`.
    Glob(String),
    /// Substring of the full path.
    Path(String),
    /// Extension without dot.
    Ext(String),
    Kind(KindFilter),
    Regex(NameRegex),
    Size(Cmp<u64>),
    Modified(DateRange),
    Created(DateRange),
    /// Capture date, falling back to the modification time.
    Date(DateRange),
    /// Capture date only (unknown capture date never matches).
    Taken(DateRange),
    Rating(Cmp<i64>),
    /// `None` = no label.
    Label(Option<ColorLabel>),
    Pick(PickState),
    /// Exact tag name; a trailing `*` means prefix.
    Tag(String),
    Collection(String),
    /// Substring of "make model" (images) or camera (videos).
    Camera(String),
    Lens(String),
    Codec(String),
    Notes(String),
    Iso(Cmp<i64>),
    Aperture(Cmp<f64>),
    FocalLength(Cmp<f64>),
    Width(Cmp<i64>),
    Height(Cmp<i64>),
    /// Seconds.
    Duration(Cmp<f64>),
    Fps(Cmp<f64>),
    BitDepth(Cmp<i64>),
    Missing(MissingField),
    Is(IsFlag),
}

/// Tolerances for `Eq` on float fields.
pub const FPS_EPS: f64 = 0.01;
pub const DURATION_EPS: f64 = 0.5;
pub const APERTURE_EPS: f64 = 0.05;
pub const FOCAL_EPS: f64 = 0.5;

impl Filter {
    /// Can this predicate be decided from a [`FileEntry`] alone (no database)?
    pub fn entry_evaluable(&self) -> bool {
        match self {
            Filter::Text(_)
            | Filter::Name(_)
            | Filter::Glob(_)
            | Filter::Path(_)
            | Filter::Ext(_)
            | Filter::Kind(_)
            | Filter::Regex(_)
            | Filter::Size(_)
            | Filter::Modified(_)
            | Filter::Created(_)
            | Filter::Date(_) => true,
            Filter::Is(f) => matches!(
                f,
                IsFlag::File | IsFlag::Dir | IsFlag::Link | IsFlag::Online | IsFlag::Offline
            ),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} (at position {pos})")]
pub struct ParseError {
    /// Byte offset into the input.
    pub pos: usize,
    pub message: String,
}

fn perr(pos: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        pos,
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------
// Query API
// ---------------------------------------------------------------------------

impl Query {
    /// Matches everything.
    pub fn all() -> Self {
        Self {
            expr: Expr::And(Vec::new()),
        }
    }

    /// Parse using the current local time for relative dates.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Self::parse_with_now(input, Local::now())
    }

    /// Parse with an explicit "now" (relative dates such as `today`, `7d`).
    pub fn parse_with_now(input: &str, now: DateTime<Local>) -> Result<Self, ParseError> {
        let toks = lex(input)?;
        if toks.is_empty() {
            return Ok(Self::all());
        }
        let mut p = Parser {
            toks,
            i: 0,
            now,
            end: input.len(),
        };
        let expr = p.parse_or()?;
        if let Some(t) = p.toks.get(p.i) {
            return Err(perr(t.pos, "unmatched ')'"));
        }
        Ok(Self { expr })
    }

    pub fn is_match_all(&self) -> bool {
        matches!(&self.expr, Expr::And(v) if v.is_empty())
    }

    /// All leaf filters (depth-first).
    pub fn filters(&self) -> Vec<&Filter> {
        let mut out = Vec::new();
        fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a Filter>) {
            match e {
                Expr::And(v) | Expr::Or(v) => v.iter().for_each(|x| walk(x, out)),
                Expr::Not(x) => walk(x, out),
                Expr::Term(f) => out.push(f),
            }
        }
        walk(&self.expr, &mut out);
        out
    }

    /// True if some predicate needs indexed data (ratings, tags, metadata…), i.e. the
    /// query cannot be fully answered by [`Query::matches_entry`].
    pub fn needs_db(&self) -> bool {
        self.filters().iter().any(|f| !f.entry_evaluable())
    }

    /// Filter a live listing entry. Predicates that need the database evaluate to
    /// "unknown" (three-valued logic); unknown is treated as no match.
    pub fn matches_entry(&self, e: &FileEntry) -> bool {
        self.eval_entry(e) == Some(true)
    }

    /// Three-valued evaluation: `None` = cannot be decided without the database.
    pub fn eval_entry(&self, e: &FileEntry) -> Option<bool> {
        let v = EntryView {
            e,
            name_lc: e.name.to_lowercase(),
            path_lc: e.path.to_string_lossy().to_lowercase(),
        };
        eval(&self.expr, &v)
    }
}

impl FromStr for Query {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

// ---------------------------------------------------------------------------
// Evaluation against FileEntry
// ---------------------------------------------------------------------------

struct EntryView<'a> {
    e: &'a FileEntry,
    name_lc: String,
    path_lc: String,
}

fn eval(expr: &Expr, v: &EntryView<'_>) -> Option<bool> {
    match expr {
        Expr::And(items) => {
            let mut unknown = false;
            for x in items {
                match eval(x, v) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        Expr::Or(items) => {
            let mut unknown = false;
            for x in items {
                match eval(x, v) {
                    Some(true) => return Some(true),
                    None => unknown = true,
                    Some(false) => {}
                }
            }
            if unknown { None } else { Some(false) }
        }
        Expr::Not(x) => eval(x, v).map(|b| !b),
        Expr::Term(f) => eval_filter(f, v),
    }
}

fn system_time_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

fn eval_filter(f: &Filter, v: &EntryView<'_>) -> Option<bool> {
    let e = v.e;
    let is_file = e.kind == EntryKind::File;
    Some(match f {
        Filter::Text(t) => v.path_lc.contains(t.as_str()) || v.name_lc.contains(t.as_str()),
        Filter::Name(t) => v.name_lc.contains(t.as_str()),
        Filter::Glob(g) => glob_match(g, &v.name_lc),
        Filter::Path(p) => v.path_lc.contains(p.as_str()),
        Filter::Ext(x) => is_file && e.ext == *x,
        Filter::Kind(k) => match k {
            KindFilter::File => is_file,
            KindFilter::Dir => e.kind == EntryKind::Dir,
            KindFilter::Link => e.kind == EntryKind::Link,
            KindFilter::Media(m) => is_file && e.media_kind() == *m,
        },
        Filter::Regex(r) => r.is_match(&e.name),
        Filter::Size(c) => {
            if !is_file {
                return None;
            }
            c.test(e.size)
        }
        Filter::Modified(r) | Filter::Date(r) => r.contains(system_time_ms(e.modified?)),
        Filter::Created(r) => r.contains(system_time_ms(e.created?)),
        Filter::Is(flag) => match flag {
            IsFlag::File => is_file,
            IsFlag::Dir => e.kind == EntryKind::Dir,
            IsFlag::Link => e.kind == EntryKind::Link,
            IsFlag::Online => true,
            IsFlag::Offline => false,
            _ => return None,
        },
        _ => return None,
    })
}

/// Case-sensitive glob (`*`, `?`) over chars; callers lowercase both sides.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum TokKind {
    LParen,
    RParen,
    Or,
    And,
    Not,
    Neg,
    Word {
        text: String,
        quoted: bool,
    },
    Field {
        name: String,
        value: String,
        value_pos: usize,
        quoted: bool,
    },
}

#[derive(Debug, Clone)]
struct Tok {
    kind: TokKind,
    pos: usize,
}

fn lex(input: &str) -> Result<Vec<Tok>, ParseError> {
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let n = chars.len();
    let byte_at = |i: usize| if i < n { chars[i].0 } else { input.len() };
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let (pos, c) = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                out.push(Tok {
                    kind: TokKind::LParen,
                    pos,
                });
                i += 1;
            }
            ')' => {
                out.push(Tok {
                    kind: TokKind::RParen,
                    pos,
                });
                i += 1;
            }
            '-' if i + 1 < n && !chars[i + 1].1.is_whitespace() && chars[i + 1].1 != ')' => {
                out.push(Tok {
                    kind: TokKind::Neg,
                    pos,
                });
                i += 1;
            }
            '"' => {
                let (text, ni) = read_quoted(&chars, i, input)?;
                out.push(Tok {
                    kind: TokKind::Word { text, quoted: true },
                    pos,
                });
                i = ni;
            }
            _ => {
                let mut j = i;
                while j < n && chars[j].1.is_ascii_alphabetic() {
                    j += 1;
                }
                if j - i >= 2 && j < n && chars[j].1 == ':' {
                    let name = input[pos..chars[j].0].to_ascii_lowercase();
                    let vstart = j + 1;
                    let value_pos = byte_at(vstart);
                    let (value, quoted, ni) = if vstart < n && chars[vstart].1 == '"' {
                        let (s, ni) = read_quoted(&chars, vstart, input)?;
                        (s, true, ni)
                    } else {
                        let (s, ni) = read_bare(&chars, vstart, input);
                        (s, false, ni)
                    };
                    out.push(Tok {
                        kind: TokKind::Field {
                            name,
                            value,
                            value_pos,
                            quoted,
                        },
                        pos,
                    });
                    i = ni;
                } else {
                    let (text, ni) = read_bare(&chars, i, input);
                    let kind = match text.as_str() {
                        "OR" | "||" => TokKind::Or,
                        "AND" | "&&" => TokKind::And,
                        "NOT" => TokKind::Not,
                        _ => TokKind::Word {
                            text,
                            quoted: false,
                        },
                    };
                    out.push(Tok { kind, pos });
                    i = ni;
                }
            }
        }
    }
    Ok(out)
}

/// `chars[start]` is the opening quote. Returns (content, index after closing quote).
fn read_quoted(
    chars: &[(usize, char)],
    start: usize,
    input: &str,
) -> Result<(String, usize), ParseError> {
    let open = chars[start].0;
    let mut j = start + 1;
    while j < chars.len() {
        if chars[j].1 == '"' {
            let s = input[chars[start].0 + 1..chars[j].0].to_string();
            return Ok((s, j + 1));
        }
        j += 1;
    }
    Err(perr(open, "unterminated quote"))
}

/// Bare token: up to whitespace, or a `)` that does not close a `(` opened inside the
/// token (so `name:file(1)` and `re:^(a|b)$` work, while `(a b)` still closes).
fn read_bare(chars: &[(usize, char)], start: usize, input: &str) -> (String, usize) {
    let mut j = start;
    let mut depth = 0usize;
    while j < chars.len() {
        let c = chars[j].1;
        if c.is_whitespace() {
            break;
        }
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            if depth == 0 {
                break;
            }
            depth -= 1;
        }
        j += 1;
    }
    let s = if start < chars.len() {
        let end = if j < chars.len() {
            chars[j].0
        } else {
            input.len()
        };
        input[chars[start].0..end].to_string()
    } else {
        String::new()
    };
    (s, j)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    i: usize,
    now: DateTime<Local>,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&TokKind> {
        self.toks.get(self.i).map(|t| &t.kind)
    }
    fn pos(&self) -> usize {
        self.toks.get(self.i).map_or(self.end, |t| t.pos)
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut alts = vec![self.parse_and()?];
        while matches!(self.peek(), Some(TokKind::Or)) {
            self.i += 1;
            if matches!(
                self.peek(),
                None | Some(TokKind::RParen) | Some(TokKind::Or)
            ) {
                return Err(perr(self.pos(), "expected a search term after OR"));
            }
            alts.push(self.parse_and()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().expect("one alternative")
        } else {
            let mut flat = Vec::with_capacity(alts.len());
            for a in alts {
                match a {
                    Expr::Or(v) => flat.extend(v),
                    other => flat.push(other),
                }
            }
            Expr::Or(flat)
        })
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None | Some(TokKind::RParen) | Some(TokKind::Or) => break,
                Some(TokKind::And) => self.i += 1,
                _ => items.push(self.parse_unary()?),
            }
        }
        if items.is_empty() {
            let msg = if matches!(self.peek(), Some(TokKind::Or)) {
                "OR needs a search term on its left"
            } else {
                "expected a search term"
            };
            return Err(perr(self.pos(), msg));
        }
        Ok(if items.len() == 1 {
            items.pop().expect("one item")
        } else {
            let mut flat = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Expr::And(v) => flat.extend(v),
                    other => flat.push(other),
                }
            }
            Expr::And(flat)
        })
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek() {
            Some(TokKind::Not) | Some(TokKind::Neg) => {
                let p = self.pos();
                self.i += 1;
                if matches!(
                    self.peek(),
                    None | Some(TokKind::RParen) | Some(TokKind::Or) | Some(TokKind::And)
                ) {
                    return Err(perr(p, "negation needs a search term"));
                }
                Ok(Expr::Not(Box::new(self.parse_unary()?)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let Some(tok) = self.toks.get(self.i).cloned() else {
            return Err(perr(self.end, "expected a search term"));
        };
        self.i += 1;
        match tok.kind {
            TokKind::LParen => {
                if matches!(self.peek(), Some(TokKind::RParen)) {
                    return Err(perr(tok.pos, "empty parentheses"));
                }
                let e = self.parse_or()?;
                if !matches!(self.peek(), Some(TokKind::RParen)) {
                    return Err(perr(tok.pos, "missing ')' for this '('"));
                }
                self.i += 1;
                Ok(e)
            }
            TokKind::RParen => Err(perr(tok.pos, "unmatched ')'")),
            TokKind::Word { text, quoted } => Ok(word_term(&text, quoted)),
            TokKind::Field {
                name,
                value,
                value_pos,
                quoted,
            } => self.field_expr(&name, tok.pos, &value, value_pos, quoted),
            TokKind::Or | TokKind::And | TokKind::Not | TokKind::Neg => {
                Err(perr(tok.pos, "expected a search term"))
            }
        }
    }

    fn field_expr(
        &self,
        name: &str,
        name_pos: usize,
        value: &str,
        vpos: usize,
        quoted: bool,
    ) -> Result<Expr, ParseError> {
        let field = FieldName::from_name(name).ok_or_else(|| {
            perr(
                name_pos,
                format!(
                    "unknown field '{name}:' (put the text in quotes to search for it literally)"
                ),
            )
        })?;
        if value.trim().is_empty() {
            return Err(perr(vpos, format!("missing value after '{name}:'")));
        }
        let pieces: Vec<(usize, &str)> = if quoted || field == FieldName::Regex {
            vec![(vpos, value)]
        } else {
            let mut v = Vec::new();
            let mut off = 0;
            for piece in value.split(',') {
                if !piece.trim().is_empty() {
                    v.push((vpos + off, piece.trim()));
                }
                off += piece.len() + 1;
            }
            v
        };
        if pieces.is_empty() {
            return Err(perr(vpos, format!("missing value after '{name}:'")));
        }
        let mut alts = Vec::with_capacity(pieces.len());
        for (p, v) in pieces {
            alts.push(self.field_value(field, name, v, p, quoted)?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().expect("one")
        } else {
            Expr::Or(alts)
        })
    }

    fn field_value(
        &self,
        field: FieldName,
        name: &str,
        v: &str,
        pos: usize,
        quoted: bool,
    ) -> Result<Expr, ParseError> {
        use FieldName as F;
        let bad = |hint: &str| perr(pos, format!("invalid value '{v}' for '{name}:' ({hint})"));
        let lc = v.to_lowercase();
        let t = |f: Filter| Ok(Expr::Term(f));
        match field {
            F::Name => {
                if !quoted && (lc.contains('*') || lc.contains('?')) {
                    t(Filter::Glob(lc))
                } else {
                    t(Filter::Name(lc))
                }
            }
            F::Path => t(Filter::Path(lc)),
            F::Ext => {
                let e = lc
                    .trim_start_matches('*')
                    .trim_start_matches('.')
                    .to_string();
                if e.is_empty() {
                    return Err(bad("expected an extension such as jpg"));
                }
                t(Filter::Ext(e))
            }
            F::Kind => {
                let k = |k: KindFilter| Expr::Term(Filter::Kind(k));
                Ok(match lc.as_str() {
                    "file" | "files" => k(KindFilter::File),
                    "dir" | "dirs" | "folder" | "folders" | "directory" => k(KindFilter::Dir),
                    "link" | "links" | "symlink" | "junction" => k(KindFilter::Link),
                    "image" | "images" | "photo" | "photos" | "picture" | "pictures" => {
                        Expr::Or(vec![
                            k(KindFilter::Media(MediaKind::Image)),
                            k(KindFilter::Media(MediaKind::Raw)),
                        ])
                    }
                    "raw" => k(KindFilter::Media(MediaKind::Raw)),
                    "video" | "videos" | "movie" | "movies" => {
                        k(KindFilter::Media(MediaKind::Video))
                    }
                    "audio" | "sound" | "music" => k(KindFilter::Media(MediaKind::Audio)),
                    "sidecar" | "sidecars" => k(KindFilter::Media(MediaKind::Sidecar)),
                    "doc" | "docs" | "document" | "documents" => {
                        k(KindFilter::Media(MediaKind::Document))
                    }
                    "archive" | "archives" => k(KindFilter::Media(MediaKind::Archive)),
                    "project" | "projects" => k(KindFilter::Media(MediaKind::Project)),
                    "other" => k(KindFilter::Media(MediaKind::Other)),
                    _ => {
                        return Err(bad(
                            "expected file, dir, link, image, raw, video, audio, sidecar, document, archive, project or other",
                        ));
                    }
                })
            }
            F::Regex => match NameRegex::new(v) {
                Ok(r) => t(Filter::Regex(r)),
                Err(e) => Err(perr(pos, format!("invalid regular expression: {e}"))),
            },
            F::Size => parse_cmp(&lc, parse_size)
                .map(|c| Expr::Term(Filter::Size(c)))
                .ok_or_else(|| bad("expected e.g. 100mb, >1.5gb, 10kb..2mb")),
            F::Modified | F::Created | F::Date | F::Taken => {
                let r = parse_date_range(&lc, &self.now).ok_or_else(|| {
                    bad("expected e.g. 2024, 2024-06, 2024-06-01, 2024-01..2024-06, today, thisweek, 7d, >30d")
                })?;
                t(match field {
                    F::Modified => Filter::Modified(r),
                    F::Created => Filter::Created(r),
                    F::Date => Filter::Date(r),
                    _ => Filter::Taken(r),
                })
            }
            F::Rating => {
                if matches!(lc.as_str(), "none" | "unrated") {
                    return t(Filter::Rating(Cmp::Eq(0)));
                }
                let c = parse_cmp(&lc, parse_int).ok_or_else(|| bad("expected 0-5, e.g. >=4"))?;
                t(Filter::Rating(c))
            }
            F::Label => {
                if matches!(lc.as_str(), "none" | "nolabel") {
                    return t(Filter::Label(None));
                }
                ColorLabel::parse(&lc)
                    .map(|c| Expr::Term(Filter::Label(Some(c))))
                    .ok_or_else(|| bad("expected red, yellow, green, blue, purple or none"))
            }
            F::Pick => Ok(Expr::Term(Filter::Pick(match lc.as_str() {
                "pick" | "picked" | "flagged" | "yes" | "true" => PickState::Pick,
                "reject" | "rejected" => PickState::Reject,
                "none" | "unflagged" | "no" | "false" => PickState::None,
                _ => return Err(bad("expected pick, reject or none")),
            }))),
            F::Tag => t(Filter::Tag(lc)),
            F::Collection => t(Filter::Collection(lc)),
            F::Camera => t(Filter::Camera(lc)),
            F::Lens => t(Filter::Lens(lc)),
            F::Codec => t(Filter::Codec(lc)),
            F::Notes => t(Filter::Notes(lc)),
            F::Iso => parse_cmp(&lc, parse_int)
                .map(|c| Expr::Term(Filter::Iso(c)))
                .ok_or_else(|| bad("expected a number, e.g. >=1600")),
            F::Width | F::Height | F::BitDepth => {
                let c = parse_cmp(&lc, parse_int).ok_or_else(|| bad("expected a whole number"))?;
                t(match field {
                    F::Width => Filter::Width(c),
                    F::Height => Filter::Height(c),
                    _ => Filter::BitDepth(c),
                })
            }
            F::Aperture => parse_cmp(&lc, |s| {
                parse_float(s.trim_start_matches("f/").trim_start_matches('f'))
            })
            .map(|c| Expr::Term(Filter::Aperture(c)))
            .ok_or_else(|| bad("expected an f-number, e.g. <=2.8")),
            F::Focal => parse_cmp(&lc, |s| parse_float(s.trim_end_matches("mm")))
                .map(|c| Expr::Term(Filter::FocalLength(c)))
                .ok_or_else(|| bad("expected millimetres, e.g. 35 or 24..70")),
            F::Fps => parse_cmp(&lc, parse_float)
                .map(|c| Expr::Term(Filter::Fps(c)))
                .ok_or_else(|| bad("expected a frame rate, e.g. 59.94")),
            F::Duration => parse_cmp(&lc, parse_duration)
                .map(|c| Expr::Term(Filter::Duration(c)))
                .ok_or_else(|| bad("expected e.g. 30s, 2m, 1h, 1:30, <30s")),
            F::Missing | F::Has => {
                let m = match lc.as_str() {
                    "gps" | "location" => MissingField::Gps,
                    "rating" => MissingField::Rating,
                    "label" => MissingField::Label,
                    "tag" | "tags" => MissingField::Tags,
                    "meta" | "metadata" => MissingField::Meta,
                    "hash" => MissingField::Hash,
                    "date" => MissingField::Date,
                    "camera" => MissingField::Camera,
                    "notes" | "note" => MissingField::Notes,
                    _ => {
                        return Err(bad(
                            "expected gps, rating, label, tags, meta, hash, date, camera or notes",
                        ));
                    }
                };
                let e = Expr::Term(Filter::Missing(m));
                Ok(if field == F::Has {
                    Expr::Not(Box::new(e))
                } else {
                    e
                })
            }
            F::Is => Ok(Expr::Term(Filter::Is(match lc.as_str() {
                "dup" | "dupe" | "duplicate" => IsFlag::Dup,
                "offline" | "missing" => IsFlag::Offline,
                "online" | "present" => IsFlag::Online,
                "pick" | "picked" | "flagged" => IsFlag::Pick,
                "reject" | "rejected" => IsFlag::Reject,
                "rated" => IsFlag::Rated,
                "labeled" | "labelled" => IsFlag::Labeled,
                "tagged" => IsFlag::Tagged,
                "vfr" => IsFlag::Vfr,
                "hdr" => IsFlag::Hdr,
                "file" => IsFlag::File,
                "dir" | "folder" => IsFlag::Dir,
                "link" => IsFlag::Link,
                _ => {
                    return Err(bad(
                        "expected dup, offline, online, pick, reject, rated, labeled, tagged, vfr, hdr, file, dir or link",
                    ));
                }
            }))),
        }
    }
}

fn word_term(text: &str, quoted: bool) -> Expr {
    let lc = text.to_lowercase();
    if !quoted && (lc.contains('*') || lc.contains('?')) {
        Expr::Term(Filter::Glob(lc))
    } else {
        Expr::Term(Filter::Text(lc))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldName {
    Name,
    Path,
    Ext,
    Kind,
    Regex,
    Size,
    Modified,
    Created,
    Date,
    Taken,
    Rating,
    Label,
    Pick,
    Tag,
    Collection,
    Camera,
    Lens,
    Codec,
    Notes,
    Iso,
    Aperture,
    Focal,
    Width,
    Height,
    Duration,
    Fps,
    BitDepth,
    Missing,
    Has,
    Is,
}

impl FieldName {
    fn from_name(n: &str) -> Option<Self> {
        use FieldName as F;
        Some(match n {
            "name" | "filename" => F::Name,
            "path" | "folder" => F::Path,
            "ext" | "extension" => F::Ext,
            "kind" | "type" => F::Kind,
            "re" | "regex" => F::Regex,
            "size" => F::Size,
            "modified" | "mtime" | "mod" => F::Modified,
            "created" | "ctime" => F::Created,
            "date" => F::Date,
            "taken" | "shot" | "recorded" => F::Taken,
            "rating" | "stars" => F::Rating,
            "label" | "color" | "colour" => F::Label,
            "pick" | "flag" => F::Pick,
            "tag" | "tags" => F::Tag,
            "collection" | "in" => F::Collection,
            "camera" | "cam" => F::Camera,
            "lens" => F::Lens,
            "codec" => F::Codec,
            "notes" | "note" | "comment" => F::Notes,
            "iso" => F::Iso,
            "aperture" | "fnumber" => F::Aperture,
            "focal" | "focallength" => F::Focal,
            "width" => F::Width,
            "height" => F::Height,
            "duration" | "dur" | "length" => F::Duration,
            "fps" | "framerate" => F::Fps,
            "bitdepth" | "bits" | "depth" => F::BitDepth,
            "missing" | "no" => F::Missing,
            "has" => F::Has,
            "is" => F::Is,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Value parsers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Gt,
    Ge,
    Lt,
    Le,
}

fn split_op(s: &str) -> (Option<Op>, &str) {
    let s = s.trim();
    for (p, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
    ] {
        if let Some(r) = s.strip_prefix(p) {
            return (Some(op), r.trim());
        }
    }
    (None, s)
}

fn parse_cmp<T: PartialOrd + Copy>(s: &str, one: impl Fn(&str) -> Option<T>) -> Option<Cmp<T>> {
    let (op, rest) = split_op(s);
    if op.is_none()
        && let Some((a, b)) = rest.split_once("..")
    {
        let (a, b) = (a.trim(), b.trim());
        return match (a.is_empty(), b.is_empty()) {
            (true, true) => None,
            (true, false) => Some(Cmp::Le(one(b)?)),
            (false, true) => Some(Cmp::Ge(one(a)?)),
            (false, false) => {
                let (x, y) = (one(a)?, one(b)?);
                Some(if x <= y {
                    Cmp::Between(x, y)
                } else {
                    Cmp::Between(y, x)
                })
            }
        };
    }
    let v = one(rest)?;
    Some(match op.unwrap_or(Op::Eq) {
        Op::Eq => Cmp::Eq(v),
        Op::Gt => Cmp::Gt(v),
        Op::Ge => Cmp::Ge(v),
        Op::Lt => Cmp::Lt(v),
        Op::Le => Cmp::Le(v),
    })
}

fn parse_int(s: &str) -> Option<i64> {
    s.trim().parse().ok()
}

fn parse_float(s: &str) -> Option<f64> {
    let v: f64 = s.trim().parse().ok()?;
    v.is_finite().then_some(v)
}

/// Splits "1.5gb" into (1.5, "gb").
fn split_number_unit(s: &str) -> Option<(f64, &str)> {
    let s = s.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let num = parse_float(&s[..end])?;
    Some((num, s[end..].trim()))
}

/// Binary size units: `512`, `10k`, `10kb`, `10kib`, `1.5gb`, `2t`.
pub fn parse_size(s: &str) -> Option<u64> {
    let (n, unit) = split_number_unit(s)?;
    let mult: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0f64.powi(4),
        _ => return None,
    };
    let v = n * mult;
    (v >= 0.0 && v < u64::MAX as f64).then(|| v.round() as u64)
}

/// Duration in seconds: `30`, `30s`, `500ms`, `1.5m`, `2h`, `1:30`, `1:02:03`.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() > 3 {
            return None;
        }
        let mut total = 0.0;
        for p in parts {
            total = total * 60.0 + parse_float(p)?;
        }
        return Some(total);
    }
    let (n, unit) = split_number_unit(s)?;
    let mult = match unit.to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" => 1.0,
        "ms" => 0.001,
        "m" | "min" | "mins" => 60.0,
        "h" | "hr" | "hrs" => 3600.0,
        _ => return None,
    };
    Some(n * mult)
}

enum DateAtom {
    /// `[start, end)` ms.
    Period(i64, i64),
    /// A point in time computed from an age ("7d" ago).
    Ago(i64),
}

fn local_midnight_ms(d: NaiveDate) -> i64 {
    // DST transitions at midnight exist in some zones; fall back to 01:00 in that case.
    for h in 0..3 {
        if let Some(ndt) = d.and_hms_opt(h, 0, 0)
            && let Some(t) = Local.from_local_datetime(&ndt).earliest()
        {
            return t.timestamp_millis();
        }
    }
    d.and_hms_opt(0, 0, 0)
        .map(|n| n.and_utc().timestamp_millis())
        .unwrap_or(0)
}

fn period(a: NaiveDate, b: NaiveDate) -> DateAtom {
    DateAtom::Period(local_midnight_ms(a), local_midnight_ms(b))
}

fn first_of_month(y: i32, m: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(y, m, 1)
}

fn next_month(y: i32, m: u32) -> Option<NaiveDate> {
    if m == 12 {
        first_of_month(y + 1, 1)
    } else {
        first_of_month(y, m + 1)
    }
}

fn parse_date_atom(s: &str, now: &DateTime<Local>) -> Option<DateAtom> {
    let today = now.date_naive();
    let day = chrono::Days::new(1);
    let week_start = today - chrono::Days::new(today.weekday().num_days_from_monday() as u64);
    let (y, m) = (today.year(), today.month());
    match s {
        "today" => return Some(period(today, today + day)),
        "yesterday" => return Some(period(today - day, today)),
        "thisweek" | "week" => return Some(period(week_start, week_start + chrono::Days::new(7))),
        "lastweek" => return Some(period(week_start - chrono::Days::new(7), week_start)),
        "thismonth" | "month" => return Some(period(first_of_month(y, m)?, next_month(y, m)?)),
        "lastmonth" => {
            let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
            return Some(period(first_of_month(py, pm)?, first_of_month(y, m)?));
        }
        "thisyear" | "year" => {
            return Some(period(first_of_month(y, 1)?, first_of_month(y + 1, 1)?));
        }
        "lastyear" => return Some(period(first_of_month(y - 1, 1)?, first_of_month(y, 1)?)),
        "now" => return Some(DateAtom::Ago(now.timestamp_millis())),
        _ => {}
    }
    // Age: 12h, 7d, 2w, 3mo / 3m, 1y.
    if let Some(split) = s.find(|c: char| !c.is_ascii_digit())
        && split > 0
    {
        let n: u32 = s[..split].parse().ok()?;
        let unit = &s[split..];
        let now_ms = now.timestamp_millis();
        let ago = match unit {
            "h" | "hr" | "hrs" | "hour" | "hours" => Some(now_ms - n as i64 * HOUR_MS),
            "d" | "day" | "days" => Some(now_ms - n as i64 * DAY_MS),
            "w" | "wk" | "week" | "weeks" => Some(now_ms - n as i64 * 7 * DAY_MS),
            "m" | "mo" | "mon" | "month" | "months" => now
                .checked_sub_months(Months::new(n))
                .map(|t| t.timestamp_millis()),
            "y" | "yr" | "year" | "years" => now
                .checked_sub_months(Months::new(n.checked_mul(12)?))
                .map(|t| t.timestamp_millis()),
            _ => None,
        };
        if let Some(t) = ago {
            return Some(DateAtom::Ago(t));
        }
    }
    // Absolute: YYYY, YYYY-MM, YYYY-MM-DD ('/' also accepted).
    let parts: Vec<&str> = s.split(['-', '/']).collect();
    if parts[0].len() != 4
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    match parts.len() {
        1 => Some(period(first_of_month(y, 1)?, first_of_month(y + 1, 1)?)),
        2 => {
            let m: u32 = parts[1].parse().ok()?;
            Some(period(first_of_month(y, m)?, next_month(y, m)?))
        }
        3 => {
            let d = NaiveDate::from_ymd_opt(y, parts[1].parse().ok()?, parts[2].parse().ok()?)?;
            Some(period(d, d + day))
        }
        _ => None,
    }
}

fn atom_bounds(a: &DateAtom) -> (i64, i64) {
    match *a {
        DateAtom::Period(s, e) => (s, e),
        DateAtom::Ago(t) => (t, t),
    }
}

fn parse_date_range(s: &str, now: &DateTime<Local>) -> Option<DateRange> {
    let (op, rest) = split_op(s);
    if op.is_none()
        && let Some((a, b)) = rest.split_once("..")
    {
        let (a, b) = (a.trim(), b.trim());
        if a.is_empty() && b.is_empty() {
            return None;
        }
        let ab = if a.is_empty() {
            None
        } else {
            Some(atom_bounds(&parse_date_atom(a, now)?))
        };
        let bb = if b.is_empty() {
            None
        } else {
            Some(atom_bounds(&parse_date_atom(b, now)?))
        };
        return Some(match (ab, bb) {
            (Some((s1, e1)), Some((s2, e2))) => DateRange {
                start: Some(s1.min(s2)),
                end: Some(e1.max(e2)),
            },
            (Some((s1, _)), None) => DateRange {
                start: Some(s1),
                end: None,
            },
            (None, Some((_, e2))) => DateRange {
                start: None,
                end: Some(e2),
            },
            (None, None) => return None,
        });
    }
    let atom = parse_date_atom(rest, now)?;
    let is_age = matches!(atom, DateAtom::Ago(_));
    let (start, end) = atom_bounds(&atom);
    let op = match (is_age, op) {
        // An age compares *age*: "<7d" = younger than 7 days = after (now - 7d).
        (true, None | Some(Op::Eq) | Some(Op::Le)) => Op::Ge,
        (true, Some(Op::Lt)) => Op::Gt,
        (true, Some(Op::Gt)) => Op::Lt,
        (true, Some(Op::Ge)) => Op::Le,
        (false, o) => o.unwrap_or(Op::Eq),
    };
    Some(match op {
        Op::Eq => DateRange {
            start: Some(start),
            end: Some(end),
        },
        Op::Gt => DateRange {
            start: Some(end),
            end: None,
        },
        Op::Ge => DateRange {
            start: Some(start),
            end: None,
        },
        Op::Lt => DateRange {
            start: None,
            end: Some(start),
        },
        Op::Le => DateRange {
            start: None,
            end: Some(end),
        },
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn now() -> DateTime<Local> {
        // Wednesday 2024-07-10 15:30 local.
        Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2024, 7, 10)
                    .unwrap()
                    .and_hms_opt(15, 30, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap()
    }

    fn q(s: &str) -> Query {
        Query::parse_with_now(s, now()).unwrap_or_else(|e| panic!("{s}: {e}"))
    }
    fn e(s: &str) -> ParseError {
        Query::parse_with_now(s, now()).unwrap_err()
    }
    fn term(f: Filter) -> Expr {
        Expr::Term(f)
    }
    fn ms(y: i32, m: u32, d: u32) -> i64 {
        local_midnight_ms(NaiveDate::from_ymd_opt(y, m, d).unwrap())
    }

    fn entry(path: &str, size: u64, kind: EntryKind, mtime_ms: i64) -> FileEntry {
        let p = PathBuf::from(path);
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let ext = if kind == EntryKind::Dir {
            String::new()
        } else {
            crate::model::ext_of(&p)
        };
        FileEntry {
            path: p,
            name,
            kind,
            size,
            modified: Some(UNIX_EPOCH + Duration::from_millis(mtime_ms as u64)),
            created: None,
            hidden: false,
            readonly: false,
            system: false,
            ext,
        }
    }

    #[test]
    fn empty_query_matches_all() {
        let qq = q("   ");
        assert!(qq.is_match_all());
        assert!(qq.matches_entry(&entry("C:\\a.txt", 1, EntryKind::File, 0)));
    }

    #[test]
    fn free_words_and_phrases() {
        assert_eq!(
            q("Sunset \"Big Sur\"").expr,
            Expr::And(vec![
                term(Filter::Text("sunset".into())),
                term(Filter::Text("big sur".into()))
            ])
        );
        assert_eq!(q("IMG_*.CR3").expr, term(Filter::Glob("img_*.cr3".into())));
        assert_eq!(q("\"a*b\"").expr, term(Filter::Text("a*b".into())));
    }

    #[test]
    fn negation_or_and_parentheses() {
        assert_eq!(
            q("a b OR c").expr,
            Expr::Or(vec![
                Expr::And(vec![
                    term(Filter::Text("a".into())),
                    term(Filter::Text("b".into()))
                ]),
                term(Filter::Text("c".into())),
            ])
        );
        assert_eq!(
            q("x (a OR b) -c").expr,
            Expr::And(vec![
                term(Filter::Text("x".into())),
                Expr::Or(vec![
                    term(Filter::Text("a".into())),
                    term(Filter::Text("b".into()))
                ]),
                Expr::Not(Box::new(term(Filter::Text("c".into())))),
            ])
        );
        assert_eq!(
            q("-(a OR b)").expr,
            Expr::Not(Box::new(Expr::Or(vec![
                term(Filter::Text("a".into())),
                term(Filter::Text("b".into()))
            ])))
        );
        assert_eq!(
            q("NOT a AND b").expr,
            Expr::And(vec![
                Expr::Not(Box::new(term(Filter::Text("a".into())))),
                term(Filter::Text("b".into()))
            ])
        );
        // lowercase "or" is a word; a lone '-' is a word
        assert_eq!(
            q("this or that").expr,
            Expr::And(vec![
                term(Filter::Text("this".into())),
                term(Filter::Text("or".into())),
                term(Filter::Text("that".into()))
            ])
        );
        assert_eq!(q("a - b").filters().len(), 3);
    }

    #[test]
    fn full_example_parses() {
        let s = r#"sunset ext:cr3,nef iso:>=1600 rating:>=4 camera:"Canon EOS R5" -label:red date:2024-01..2024-06 size:>100mb duration:<30s fps:59.94 width:>=3840 tag:wedding path:clients\ acme OR beach (a OR b) kind:video missing:gps is:dup"#;
        let qq = q(s);
        let fs = qq.filters();
        assert!(fs.contains(&&Filter::Ext("cr3".into())));
        assert!(fs.contains(&&Filter::Ext("nef".into())));
        assert!(fs.contains(&&Filter::Iso(Cmp::Ge(1600))));
        assert!(fs.contains(&&Filter::Rating(Cmp::Ge(4))));
        assert!(fs.contains(&&Filter::Camera("canon eos r5".into())));
        assert!(fs.contains(&&Filter::Label(Some(ColorLabel::Red))));
        assert!(fs.contains(&&Filter::Date(DateRange {
            start: Some(ms(2024, 1, 1)),
            end: Some(ms(2024, 7, 1))
        })));
        assert!(fs.contains(&&Filter::Size(Cmp::Gt(100 * 1024 * 1024))));
        assert!(fs.contains(&&Filter::Duration(Cmp::Lt(30.0))));
        assert!(fs.contains(&&Filter::Fps(Cmp::Eq(59.94))));
        assert!(fs.contains(&&Filter::Width(Cmp::Ge(3840))));
        assert!(fs.contains(&&Filter::Tag("wedding".into())));
        assert!(fs.contains(&&Filter::Path("clients\\".into())));
        assert!(fs.contains(&&Filter::Kind(KindFilter::Media(MediaKind::Video))));
        assert!(fs.contains(&&Filter::Missing(MissingField::Gps)));
        assert!(fs.contains(&&Filter::Is(IsFlag::Dup)));
        // Top level is an OR (… acme) OR (beach (a OR b) …)
        let Expr::Or(alts) = &qq.expr else {
            panic!("{:?}", qq.expr)
        };
        assert_eq!(alts.len(), 2);
        assert!(qq.needs_db());
    }

    #[test]
    fn comparisons_and_ranges() {
        assert_eq!(
            q("iso:100..800").expr,
            term(Filter::Iso(Cmp::Between(100, 800)))
        );
        assert_eq!(
            q("iso:800..100").expr,
            term(Filter::Iso(Cmp::Between(100, 800)))
        );
        assert_eq!(q("iso:..400").expr, term(Filter::Iso(Cmp::Le(400))));
        assert_eq!(q("iso:400..").expr, term(Filter::Iso(Cmp::Ge(400))));
        assert_eq!(q("rating:=5").expr, term(Filter::Rating(Cmp::Eq(5))));
        assert_eq!(q("rating:<2").expr, term(Filter::Rating(Cmp::Lt(2))));
        assert_eq!(q("rating:none").expr, term(Filter::Rating(Cmp::Eq(0))));
        assert_eq!(
            q("aperture:<=f/2.8").expr,
            term(Filter::Aperture(Cmp::Le(2.8)))
        );
        assert_eq!(
            q("focal:24mm..70mm").expr,
            term(Filter::FocalLength(Cmp::Between(24.0, 70.0)))
        );
        assert_eq!(
            q("fps:23.976..30").expr,
            term(Filter::Fps(Cmp::Between(23.976, 30.0)))
        );
        assert_eq!(q("bitdepth:>8").expr, term(Filter::BitDepth(Cmp::Gt(8))));
        assert_eq!(q("height:2160").expr, term(Filter::Height(Cmp::Eq(2160))));
        assert!(Cmp::Eq(59.94).test_approx(59.94006, FPS_EPS));
        assert!(!Cmp::Eq(59.94).test_approx(60.0, FPS_EPS));
        assert!(Cmp::Between(1, 3).test(3));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("100mb"), Some(100 * 1024 * 1024));
        assert_eq!(parse_size("1.5GB"), Some(1536 * 1024 * 1024));
        assert_eq!(parse_size("10k"), Some(10240));
        assert_eq!(parse_size("2kib"), Some(2048));
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("1tb"), Some(1u64 << 40));
        assert_eq!(parse_size("5zb"), None);
        assert_eq!(
            q("size:1mb..2mb").expr,
            term(Filter::Size(Cmp::Between(1 << 20, 2 << 20)))
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s"), Some(30.0));
        assert_eq!(parse_duration("1.5m"), Some(90.0));
        assert_eq!(parse_duration("2h"), Some(7200.0));
        assert_eq!(parse_duration("500ms"), Some(0.5));
        assert_eq!(parse_duration("1:30"), Some(90.0));
        assert_eq!(parse_duration("1:02:03"), Some(3723.0));
        assert_eq!(parse_duration("45"), Some(45.0));
        assert_eq!(parse_duration("3x"), None);
        assert_eq!(
            q("dur:1m..2m").expr,
            term(Filter::Duration(Cmp::Between(60.0, 120.0)))
        );
    }

    #[test]
    fn absolute_dates() {
        let r = |s: &str| match q(s).expr {
            Expr::Term(Filter::Modified(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            r("modified:2024"),
            DateRange {
                start: Some(ms(2024, 1, 1)),
                end: Some(ms(2025, 1, 1))
            }
        );
        assert_eq!(
            r("modified:2024-12"),
            DateRange {
                start: Some(ms(2024, 12, 1)),
                end: Some(ms(2025, 1, 1))
            }
        );
        assert_eq!(
            r("modified:2024/02/29"),
            DateRange {
                start: Some(ms(2024, 2, 29)),
                end: Some(ms(2024, 3, 1))
            }
        );
        assert_eq!(
            r("modified:>2024-03"),
            DateRange {
                start: Some(ms(2024, 4, 1)),
                end: None
            }
        );
        assert_eq!(
            r("modified:>=2024-03"),
            DateRange {
                start: Some(ms(2024, 3, 1)),
                end: None
            }
        );
        assert_eq!(
            r("modified:<2024-03"),
            DateRange {
                start: None,
                end: Some(ms(2024, 3, 1))
            }
        );
        assert_eq!(
            r("modified:<=2024-03"),
            DateRange {
                start: None,
                end: Some(ms(2024, 4, 1))
            }
        );
        assert_eq!(
            r("modified:2024-03.."),
            DateRange {
                start: Some(ms(2024, 3, 1)),
                end: None
            }
        );
        assert_eq!(
            r("modified:..2024-03"),
            DateRange {
                start: None,
                end: Some(ms(2024, 4, 1))
            }
        );
        assert!(Query::parse_with_now("modified:2024-13", now()).is_err());
        assert!(Query::parse_with_now("modified:24-01", now()).is_err());
    }

    #[test]
    fn relative_dates() {
        let r = |s: &str| match q(s).expr {
            Expr::Term(Filter::Modified(r)) => r,
            other => panic!("{other:?}"),
        };
        let n = now().timestamp_millis();
        assert_eq!(
            r("modified:today"),
            DateRange {
                start: Some(ms(2024, 7, 10)),
                end: Some(ms(2024, 7, 11))
            }
        );
        assert_eq!(
            r("modified:yesterday"),
            DateRange {
                start: Some(ms(2024, 7, 9)),
                end: Some(ms(2024, 7, 10))
            }
        );
        // 2024-07-10 is a Wednesday; ISO week starts Monday 07-08.
        assert_eq!(
            r("modified:thisweek"),
            DateRange {
                start: Some(ms(2024, 7, 8)),
                end: Some(ms(2024, 7, 15))
            }
        );
        assert_eq!(
            r("modified:lastweek"),
            DateRange {
                start: Some(ms(2024, 7, 1)),
                end: Some(ms(2024, 7, 8))
            }
        );
        assert_eq!(
            r("modified:thismonth"),
            DateRange {
                start: Some(ms(2024, 7, 1)),
                end: Some(ms(2024, 8, 1))
            }
        );
        assert_eq!(
            r("modified:lastmonth"),
            DateRange {
                start: Some(ms(2024, 6, 1)),
                end: Some(ms(2024, 7, 1))
            }
        );
        assert_eq!(
            r("modified:lastyear"),
            DateRange {
                start: Some(ms(2023, 1, 1)),
                end: Some(ms(2024, 1, 1))
            }
        );
        assert_eq!(
            r("modified:7d"),
            DateRange {
                start: Some(n - 7 * DAY_MS),
                end: None
            }
        );
        assert_eq!(
            r("modified:<7d"),
            DateRange {
                start: Some(n - 7 * DAY_MS),
                end: None
            }
        );
        assert_eq!(
            r("modified:>30d"),
            DateRange {
                start: None,
                end: Some(n - 30 * DAY_MS)
            }
        );
        assert_eq!(
            r("modified:12h"),
            DateRange {
                start: Some(n - 12 * HOUR_MS),
                end: None
            }
        );
        assert_eq!(
            r("modified:2w"),
            DateRange {
                start: Some(n - 14 * DAY_MS),
                end: None
            }
        );
        assert_eq!(
            r("modified:7d..30d"),
            DateRange {
                start: Some(n - 30 * DAY_MS),
                end: Some(n - 7 * DAY_MS)
            }
        );
        let three_mo = now()
            .checked_sub_months(Months::new(3))
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            r("modified:3mo"),
            DateRange {
                start: Some(three_mo),
                end: None
            }
        );
    }

    #[test]
    fn text_fields_lists_and_misc() {
        assert_eq!(q("name:IMG").expr, term(Filter::Name("img".into())));
        assert_eq!(
            q("name:img_00?.jpg").expr,
            term(Filter::Glob("img_00?.jpg".into()))
        );
        assert_eq!(q("ext:.JPG").expr, term(Filter::Ext("jpg".into())));
        assert_eq!(q("ext:*.png").expr, term(Filter::Ext("png".into())));
        assert_eq!(
            q("label:red,blue").expr,
            Expr::Or(vec![
                term(Filter::Label(Some(ColorLabel::Red))),
                term(Filter::Label(Some(ColorLabel::Blue)))
            ])
        );
        assert_eq!(q("label:none").expr, term(Filter::Label(None)));
        assert_eq!(q("pick:reject").expr, term(Filter::Pick(PickState::Reject)));
        assert_eq!(
            q("has:gps").expr,
            Expr::Not(Box::new(term(Filter::Missing(MissingField::Gps))))
        );
        assert_eq!(
            q("kind:image").expr,
            Expr::Or(vec![
                term(Filter::Kind(KindFilter::Media(MediaKind::Image))),
                term(Filter::Kind(KindFilter::Media(MediaKind::Raw)))
            ])
        );
        assert_eq!(
            q("camera:\"a, b\"").expr,
            term(Filter::Camera("a, b".into()))
        );
        assert_eq!(q("tag:wed*").expr, term(Filter::Tag("wed*".into())));
        assert_eq!(q("in:Best").expr, term(Filter::Collection("best".into())));
        // drive-letter paths are free text, not fields
        assert_eq!(
            q(r"C:\Photos").expr,
            term(Filter::Text(r"c:\photos".into()))
        );
        // Unicode text is lowercased properly
        assert_eq!(q("ÄRGER").expr, term(Filter::Text("ärger".into())));
    }

    #[test]
    fn regex_values_keep_parentheses_and_commas() {
        let qq = q(r"re:^(img|dsc)_\d{1,4}$ x");
        let Expr::And(v) = &qq.expr else { panic!() };
        let Expr::Term(Filter::Regex(r)) = &v[0] else {
            panic!("{v:?}")
        };
        assert_eq!(r.pattern(), r"^(img|dsc)_\d{1,4}$");
        assert!(r.is_match("IMG_0001"));
        assert!(!r.is_match("IMG_00001"));
        // closing paren of a group still works after a field value
        assert_eq!(
            q("(name:file(1))").expr,
            term(Filter::Name("file(1)".into()))
        );
    }

    #[test]
    fn errors_have_positions() {
        let x = e("a \"unterminated");
        assert_eq!(x.pos, 2);
        assert!(x.message.contains("unterminated"));
        let x = e("a foo:bar");
        assert_eq!(x.pos, 2);
        assert!(x.message.contains("unknown field"));
        let x = e("a (b c");
        assert_eq!(x.pos, 2);
        let x = e("a b)");
        assert_eq!(x.pos, 3);
        let x = e("size:>12zb");
        assert_eq!(x.pos, 5);
        let x = e("x ext:");
        assert_eq!(x.pos, 6);
        assert!(e("re:(unclosed").message.contains("regular expression"));
        assert!(e("a OR").message.contains("after OR"));
        assert!(e("OR a").message.contains("left"));
        assert!(e("()").message.contains("empty"));
        assert!(e("rating:lots").message.contains("rating"));
        assert!(e("kind:spaceship").message.contains("expected"));
        assert!(e("label:pink").message.contains("red"));
        let x = e("iso:100,abc");
        assert_eq!(x.pos, 8);
        // Display includes the position
        assert!(e("a b)").to_string().contains("position 3"));
    }

    #[test]
    fn matches_entries() {
        let t = ms(2024, 3, 15) + 3 * HOUR_MS;
        let f = entry(
            r"D:\Clients\Acme\IMG_0001.CR3",
            30 << 20,
            EntryKind::File,
            t,
        );
        let v = entry(r"D:\Clients\Acme\clip.mp4", 200 << 20, EntryKind::File, t);
        let d = entry(r"D:\Clients\Acme\Selects", 0, EntryKind::Dir, t);
        let m = |s: &str, x: &FileEntry| q(s).matches_entry(x);

        assert!(m("acme", &f)); // path substring
        assert!(m("img_0001", &f));
        assert!(m("IMG_*.cr3", &f));
        assert!(!m("IMG_*.jpg", &f));
        assert!(m("ext:cr3,nef", &f));
        assert!(!m("ext:cr3,nef", &v));
        assert!(m("kind:image", &f));
        assert!(m("kind:raw", &f));
        assert!(m("kind:video", &v));
        assert!(m("kind:dir", &d));
        assert!(!m("kind:file", &d));
        assert!(m("size:>100mb", &v));
        assert!(!m("size:>100mb", &f));
        assert!(m("size:10mb..50mb", &f));
        assert!(!m("size:>1b", &d)); // dirs have unknown size
        assert!(!m("-size:>1b", &d));
        assert!(m("modified:2024-03", &f));
        assert!(m("modified:2024-03-15", &f));
        assert!(!m("modified:2024-04..", &f));
        assert!(m("date:2024", &f));
        assert!(m(r"re:^img_\d+\.cr3$", &f));
        assert!(m(r"path:clients\acme", &f));
        assert!(m("-name:clip", &f));
        assert!(!m("-name:clip", &v));
        assert!(m("clip OR img", &v));
        assert!(m("(clip OR img) ext:cr3", &f));
        assert!(!m("(clip OR img) ext:cr3", &v));
        assert!(m("is:file", &f));
        assert!(m("is:dir", &d));
        assert!(!m("is:offline", &f));
        assert!(m("selects", &d));
        assert!(!m("created:2024", &f)); // created unknown
    }

    #[test]
    fn three_valued_logic_for_db_only_fields() {
        let f = entry(r"C:\a\b.jpg", 10, EntryKind::File, 0);
        let qq = q("rating:>=4");
        assert_eq!(qq.eval_entry(&f), None);
        assert!(!qq.matches_entry(&f));
        assert!(!q("-rating:>=4").matches_entry(&f));
        // OR with a known-true side is decidable
        assert!(q("rating:>=4 OR ext:jpg").matches_entry(&f));
        // AND with a known-false side is decidable
        assert_eq!(q("rating:>=4 ext:png").eval_entry(&f), Some(false));
        assert!(q("rating:>=4").needs_db());
        assert!(!q("ext:jpg size:>1mb modified:7d").needs_db());
    }

    #[test]
    fn unicode_paths() {
        let f = entry("C:\\Фото\\日本\\Ärger Ü.jpg", 10, EntryKind::File, 0);
        assert!(q("фото").matches_entry(&f));
        assert!(q("日本").matches_entry(&f));
        assert!(q("\"ärger ü\"").matches_entry(&f));
        assert!(q("name:ÄRGER").matches_entry(&f));
        assert!(q("ärger*").matches_entry(&f));
    }

    #[test]
    fn glob() {
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("?b", "ab"));
        assert!(!glob_match("?b", "b"));
        assert!(glob_match("*.jpg", "x.y.jpg"));
    }

    #[test]
    fn labels_and_picks_roundtrip() {
        for c in ColorLabel::ALL {
            assert_eq!(ColorLabel::parse(c.as_str()), Some(c));
        }
        for p in [PickState::None, PickState::Pick, PickState::Reject] {
            assert_eq!(PickState::from_i64(p.as_i64()), p);
        }
    }
}

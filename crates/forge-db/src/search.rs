//! Compiles a [`forge_core::query::Query`] to parameterized SQL.
//!
//! The generated SQL text consists only of fixed fragments written here; every user value
//! is bound as a parameter (`?N`). Table aliases: `f` files, `a` annotations,
//! `im` image_meta, `vm` video_meta (meta tables are joined only when referenced).
//!
//! Predicates over nullable columns are wrapped in `COALESCE(…, 0)` so that "unknown"
//! behaves as *false* and negation stays two-valued (`-label:red` matches unlabeled files).

use std::path::PathBuf;

use forge_core::model::EntryKind;
use forge_core::query::{
    APERTURE_EPS, Cmp, DURATION_EPS, DateRange, Expr, FOCAL_EPS, FPS_EPS, Filter, IsFlag,
    KindFilter, MissingField, Query,
};
use rusqlite::types::Value;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::files::{
    FileRecord, RECORD_COLS, entry_kind_str, media_kind_str, normalize_dir, subtree_bounds,
};
use crate::{Db, path_str};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SortField {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Created,
    /// Capture date, falling back to modified.
    Date,
    Rating,
    Ext,
    Duration,
    Indexed,
}

/// Restrict a search to a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchScope {
    pub path: PathBuf,
    /// Include subfolders (otherwise direct children only).
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOptions {
    pub sort: SortField,
    pub descending: bool,
    /// `None` = no limit.
    pub limit: Option<u32>,
    pub offset: u32,
    /// Include files not currently present. Implied when the query mentions
    /// `is:offline` / `is:online`.
    pub include_offline: bool,
    pub scope: Option<SearchScope>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            sort: SortField::Name,
            descending: false,
            limit: Some(10_000),
            offset: 0,
            include_offline: false,
            scope: None,
        }
    }
}

/// A compiled WHERE-clause fragment and its parameters (numbered `?1..?N`).
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    pub where_sql: String,
    pub params: Vec<Value>,
}

impl CompiledQuery {
    pub(crate) fn uses_image_meta(&self) -> bool {
        self.where_sql.contains("im.")
    }
    pub(crate) fn uses_video_meta(&self) -> bool {
        self.where_sql.contains("vm.")
    }
}

struct Compiler {
    params: Vec<Value>,
}

impl Compiler {
    fn bind(&mut self, v: impl Into<Value>) -> String {
        self.params.push(v.into());
        format!("?{}", self.params.len())
    }

    fn bind_s(&mut self, s: &str) -> String {
        self.bind(s.to_string())
    }

    fn expr(&mut self, e: &Expr) -> String {
        match e {
            Expr::And(v) if v.is_empty() => "1".into(),
            Expr::Or(v) if v.is_empty() => "0".into(),
            Expr::And(v) => self.join(v, " AND "),
            Expr::Or(v) => self.join(v, " OR "),
            Expr::Not(x) => format!("NOT ({})", self.expr(x)),
            Expr::Term(f) => {
                let (sql, nullable) = self.filter(f);
                if nullable {
                    format!("COALESCE(({sql}), 0)")
                } else {
                    format!("({sql})")
                }
            }
        }
    }

    fn join(&mut self, v: &[Expr], sep: &str) -> String {
        let parts: Vec<String> = v.iter().map(|x| self.expr(x)).collect();
        format!("({})", parts.join(sep))
    }

    fn cmp_int(&mut self, col: &str, c: &Cmp<i64>) -> String {
        match *c {
            Cmp::Eq(x) => format!("{col} = {}", self.bind(x)),
            Cmp::Gt(x) => format!("{col} > {}", self.bind(x)),
            Cmp::Ge(x) => format!("{col} >= {}", self.bind(x)),
            Cmp::Lt(x) => format!("{col} < {}", self.bind(x)),
            Cmp::Le(x) => format!("{col} <= {}", self.bind(x)),
            Cmp::Between(a, b) => {
                let (a, b) = (self.bind(a), self.bind(b));
                format!("{col} BETWEEN {a} AND {b}")
            }
        }
    }

    fn cmp_real(&mut self, col: &str, c: &Cmp<f64>, eps: f64) -> String {
        match *c {
            Cmp::Eq(x) => {
                let (x, e) = (self.bind(x), self.bind(eps));
                format!("ABS({col} - {x}) <= {e}")
            }
            Cmp::Gt(x) => format!("{col} > {}", self.bind(x)),
            Cmp::Ge(x) => format!("{col} >= {}", self.bind(x)),
            Cmp::Lt(x) => format!("{col} < {}", self.bind(x)),
            Cmp::Le(x) => format!("{col} <= {}", self.bind(x)),
            Cmp::Between(a, b) => {
                let (a, b) = (self.bind(a), self.bind(b));
                format!("{col} BETWEEN {a} AND {b}")
            }
        }
    }

    fn date(&mut self, col: &str, r: &DateRange) -> String {
        let mut parts = Vec::new();
        if let Some(s) = r.start {
            parts.push(format!("{col} >= {}", self.bind(s)));
        }
        if let Some(e) = r.end {
            parts.push(format!("{col} < {}", self.bind(e)));
        }
        if parts.is_empty() {
            format!("{col} IS NOT NULL")
        } else {
            parts.join(" AND ")
        }
    }

    fn contains(&mut self, haystack: &str, needle: &str) -> String {
        format!("instr({haystack}, {}) > 0", self.bind(needle.to_string()))
    }

    /// Returns (sql, may_be_null).
    fn filter(&mut self, f: &Filter) -> (String, bool) {
        const TAKEN: &str = "COALESCE(im.taken_at, vm.recorded_at)";
        match f {
            Filter::Text(t) | Filter::Path(t) => (self.contains("f.path_lc", t), false),
            Filter::Name(t) => (self.contains("f.name_lc", t), false),
            Filter::Glob(g) => {
                // SQLite GLOB: '*' and '?' as typed; '[' must be escaped as a class.
                let pat = g.replace('[', "[[]");
                (format!("f.name_lc GLOB {}", self.bind(pat)), false)
            }
            Filter::Ext(e) => (format!("f.ext = {}", self.bind(e.clone())), false),
            Filter::Kind(k) => match k {
                KindFilter::File => (format!("f.kind = {}", self.bind_s("file")), false),
                KindFilter::Dir => (format!("f.kind = {}", self.bind_s("dir")), false),
                KindFilter::Link => (format!("f.kind = {}", self.bind_s("link")), false),
                KindFilter::Media(m) => {
                    let (a, b) = (self.bind_s("file"), self.bind_s(media_kind_str(*m)));
                    (format!("f.kind = {a} AND f.media_kind = {b}"), false)
                }
            },
            Filter::Regex(r) => (format!("regexp({}, f.name)", self.bind(r.pattern().to_string())), true),
            Filter::Size(c) => {
                let c = to_i64_cmp(c);
                let k = self.bind_s("file");
                (format!("f.kind = {k} AND {}", self.cmp_int("f.size", &c)), false)
            }
            Filter::Modified(r) => (self.date("f.mtime", r), true),
            Filter::Created(r) => (self.date("f.ctime", r), true),
            Filter::Date(r) => (self.date("COALESCE(im.taken_at, vm.recorded_at, f.mtime)", r), true),
            Filter::Taken(r) => (self.date(TAKEN, r), true),
            Filter::Rating(c) => (self.cmp_int("COALESCE(a.rating, 0)", c), false),
            Filter::Label(Some(l)) => (format!("a.label = {}", self.bind_s(l.as_str())), true),
            Filter::Label(None) => ("a.label IS NULL".into(), false),
            Filter::Pick(p) => (format!("COALESCE(a.pick, 0) = {}", self.bind(p.as_i64())), false),
            Filter::Tag(t) => {
                let cond = if let Some(prefix) = t.strip_suffix('*') {
                    format!("instr(t.name_lc, {}) = 1", self.bind(prefix.to_string()))
                } else {
                    format!("t.name_lc = {}", self.bind(t.clone()))
                };
                (
                    format!(
                        "EXISTS (SELECT 1 FROM file_tags ft JOIN tags t ON t.id = ft.tag_id \
                         WHERE ft.file_id = f.id AND {cond})"
                    ),
                    false,
                )
            }
            Filter::Collection(n) => (
                format!(
                    "EXISTS (SELECT 1 FROM collection_items ci JOIN collections c ON c.id = ci.collection_id \
                     WHERE ci.file_id = f.id AND c.name_lc = {})",
                    self.bind(n.clone())
                ),
                false,
            ),
            Filter::Camera(s) => (
                self.contains(
                    "ulower(COALESCE(im.camera_make, '') || ' ' || COALESCE(im.camera_model, '') || ' ' || COALESCE(vm.camera, ''))",
                    s,
                ),
                false,
            ),
            Filter::Lens(s) => (self.contains("ulower(COALESCE(im.lens, ''))", s), false),
            Filter::Codec(s) => (self.contains("ulower(COALESCE(vm.codec, ''))", s), false),
            Filter::Notes(s) => (self.contains("ulower(COALESCE(a.notes, ''))", s), false),
            Filter::Iso(c) => (self.cmp_int("im.iso", c), true),
            Filter::Aperture(c) => (self.cmp_real("im.aperture", c, APERTURE_EPS), true),
            Filter::FocalLength(c) => (self.cmp_real("im.focal_length", c, FOCAL_EPS), true),
            Filter::Width(c) => (self.cmp_int("COALESCE(im.width, vm.width)", c), true),
            Filter::Height(c) => (self.cmp_int("COALESCE(im.height, vm.height)", c), true),
            Filter::Duration(c) => (self.cmp_real("vm.duration", c, DURATION_EPS), true),
            Filter::Fps(c) => (self.cmp_real("vm.fps", c, FPS_EPS), true),
            Filter::BitDepth(c) => (self.cmp_int("COALESCE(im.bit_depth, vm.bit_depth)", c), true),
            Filter::Missing(m) => (
                match m {
                    MissingField::Gps => "im.gps_lat IS NULL OR im.gps_lon IS NULL".into(),
                    MissingField::Rating => "COALESCE(a.rating, 0) = 0".into(),
                    MissingField::Label => "a.label IS NULL".into(),
                    MissingField::Tags => {
                        "NOT EXISTS (SELECT 1 FROM file_tags ft WHERE ft.file_id = f.id)".into()
                    }
                    MissingField::Meta => "im.file_id IS NULL AND vm.file_id IS NULL".into(),
                    MissingField::Hash => "f.full_hash IS NULL".into(),
                    MissingField::Date => format!("{TAKEN} IS NULL"),
                    MissingField::Camera => {
                        "COALESCE(im.camera_model, im.camera_make, vm.camera) IS NULL".into()
                    }
                    MissingField::Notes => "COALESCE(a.notes, '') = ''".into(),
                },
                false,
            ),
            Filter::Is(flag) => match flag {
                IsFlag::Dup => (
                    "f.full_hash IS NOT NULL AND EXISTS (SELECT 1 FROM files d \
                     WHERE d.full_hash = f.full_hash AND d.id <> f.id AND d.present = 1)"
                        .into(),
                    false,
                ),
                IsFlag::Offline => ("f.present = 0".into(), false),
                IsFlag::Online => ("f.present = 1".into(), false),
                IsFlag::Pick => ("COALESCE(a.pick, 0) = 1".into(), false),
                IsFlag::Reject => ("COALESCE(a.pick, 0) = -1".into(), false),
                IsFlag::Rated => ("COALESCE(a.rating, 0) > 0".into(), false),
                IsFlag::Labeled => ("a.label IS NOT NULL".into(), false),
                IsFlag::Tagged => (
                    "EXISTS (SELECT 1 FROM file_tags ft WHERE ft.file_id = f.id)".into(),
                    false,
                ),
                IsFlag::Vfr => ("COALESCE(vm.vfr, 0) = 1".into(), false),
                IsFlag::Hdr => ("COALESCE(vm.hdr, '') <> ''".into(), false),
                IsFlag::File => (format!("f.kind = {}", self.bind_s(entry_kind_str(EntryKind::File))), false),
                IsFlag::Dir => (format!("f.kind = {}", self.bind_s(entry_kind_str(EntryKind::Dir))), false),
                IsFlag::Link => (format!("f.kind = {}", self.bind_s(entry_kind_str(EntryKind::Link))), false),
            },
        }
    }
}

fn to_i64_cmp(c: &Cmp<u64>) -> Cmp<i64> {
    let f = |v: u64| v.min(i64::MAX as u64) as i64;
    match *c {
        Cmp::Eq(x) => Cmp::Eq(f(x)),
        Cmp::Gt(x) => Cmp::Gt(f(x)),
        Cmp::Ge(x) => Cmp::Ge(f(x)),
        Cmp::Lt(x) => Cmp::Lt(f(x)),
        Cmp::Le(x) => Cmp::Le(f(x)),
        Cmp::Between(a, b) => Cmp::Between(f(a), f(b)),
    }
}

/// Compile the query's predicate (no presence/scope/sort handling).
pub fn compile_query(q: &Query) -> CompiledQuery {
    let mut c = Compiler { params: Vec::new() };
    let where_sql = c.expr(&q.expr);
    CompiledQuery {
        where_sql,
        params: c.params,
    }
}

fn sort_sql(s: SortField) -> &'static str {
    match s {
        SortField::Name => "f.name_lc",
        SortField::Path => "f.path_lc",
        SortField::Size => "f.size",
        SortField::Modified => "f.mtime",
        SortField::Created => "f.ctime",
        SortField::Date => "COALESCE(im.taken_at, vm.recorded_at, f.mtime)",
        SortField::Rating => "COALESCE(a.rating, 0)",
        SortField::Ext => "f.ext",
        SortField::Duration => "vm.duration",
        SortField::Indexed => "f.indexed_at",
    }
}

/// Build the full statement. `count_only` produces `SELECT COUNT(*)`.
pub(crate) fn build_sql(q: &Query, opts: &SearchOptions, count_only: bool) -> (String, Vec<Value>) {
    let mut cq = compile_query(q);
    let mut wheres = vec![cq.where_sql.clone()];
    let mentions_presence = q
        .filters()
        .iter()
        .any(|f| matches!(f, Filter::Is(IsFlag::Offline | IsFlag::Online)));
    if !opts.include_offline && !mentions_presence {
        wheres.push("f.present = 1".into());
    }
    if let Some(scope) = &opts.scope {
        let dir = normalize_dir(&path_str(&scope.path));
        if scope.recursive {
            let (lo, hi) = subtree_bounds(&dir);
            cq.params.push(lo.into());
            let a = cq.params.len();
            cq.params.push(hi.into());
            wheres.push(format!("f.path >= ?{a} AND f.path < ?{}", a + 1));
        } else {
            cq.params.push(dir.into());
            wheres.push(format!("f.parent_path = ?{}", cq.params.len()));
        }
    }
    let sort_col = sort_sql(opts.sort);
    let needs_im = cq.uses_image_meta() || (!count_only && sort_col.contains("im."));
    let needs_vm = cq.uses_video_meta() || (!count_only && sort_col.contains("vm."));
    let mut sql = if count_only {
        String::from("SELECT COUNT(*) FROM files f LEFT JOIN annotations a ON a.file_id = f.id")
    } else {
        format!("SELECT {RECORD_COLS} FROM files f LEFT JOIN annotations a ON a.file_id = f.id")
    };
    if needs_im {
        sql.push_str(" LEFT JOIN image_meta im ON im.file_id = f.id");
    }
    if needs_vm {
        sql.push_str(" LEFT JOIN video_meta vm ON vm.file_id = f.id");
    }
    sql.push_str(" WHERE ");
    sql.push_str(&wheres.join(" AND "));
    if !count_only {
        let dir = if opts.descending { "DESC" } else { "ASC" };
        sql.push_str(&format!(" ORDER BY {sort_col} {dir}, f.id {dir}"));
        cq.params
            .push(Value::Integer(opts.limit.map_or(-1, i64::from)));
        let l = cq.params.len();
        cq.params.push(Value::Integer(i64::from(opts.offset)));
        sql.push_str(&format!(" LIMIT ?{l} OFFSET ?{}", l + 1));
    }
    (sql, cq.params)
}

impl Db {
    pub fn search(&self, q: &Query, opts: &SearchOptions) -> Result<Vec<FileRecord>> {
        let (sql, params) = build_sql(q, opts, false);
        self.with_conn(|c| {
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map(
                    rusqlite::params_from_iter(params.iter()),
                    FileRecord::from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Parse and search. Parse errors are returned as [`crate::DbError::Query`].
    pub fn search_str(&self, query: &str, opts: &SearchOptions) -> Result<Vec<FileRecord>> {
        self.search(&Query::parse(query)?, opts)
    }

    /// Number of matches (ignores limit/offset/sort).
    pub fn count(&self, q: &Query, opts: &SearchOptions) -> Result<u64> {
        let (sql, params) = build_sql(q, opts, true);
        self.with_conn(|c| {
            let n: i64 = c.query_row(&sql, rusqlite::params_from_iter(params.iter()), |r| {
                r.get(0)
            })?;
            Ok(n as u64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{ImageMeta, NewFile, VideoMeta};
    use crate::{AnnotationChange, FileId};
    use chrono::{Local, TimeZone};
    use forge_core::query::{ColorLabel, PickState};
    use std::collections::BTreeSet;

    struct Fixture {
        db: Db,
    }

    const T2024_03: i64 = 1_710_000_000_000; // 2024-03-09
    const T2023_06: i64 = 1_686_000_000_000; // 2023-06-05

    fn file(p: &str, size: u64, mtime: i64) -> NewFile {
        NewFile {
            path: PathBuf::from(p),
            kind: EntryKind::File,
            size,
            mtime: Some(mtime),
            ctime: Some(mtime),
        }
    }

    fn fixture() -> Fixture {
        let db = Db::open_in_memory().unwrap();
        let mut dir = NewFile {
            path: PathBuf::from(r"D:\Clients\Acme"),
            kind: EntryKind::Dir,
            size: 0,
            mtime: None,
            ctime: None,
        };
        let ids = db
            .upsert_files(&[
                file(r"D:\Clients\Acme\sunset_001.CR3", 30 << 20, T2024_03),
                file(r"D:\Clients\Acme\sunset_002.nef", 40 << 20, T2024_03),
                file(r"D:\Clients\Acme\beach.jpg", 5 << 20, T2023_06),
                file(r"D:\Clients\Acme\clip_a.mp4", 500 << 20, T2024_03),
                file(r"D:\Clients\Acme\clip_b.mov", 50 << 20, T2023_06),
                file(r"D:\Other\notes.txt", 1024, T2023_06),
                file(r"D:\Other\Фото [1].jpg", 2 << 20, T2024_03),
            ])
            .unwrap();
        dir.path = PathBuf::from(r"D:\Clients\Acme");
        db.upsert_files(&[dir]).unwrap();
        db.upsert_image_meta(
            ids[0],
            &ImageMeta {
                width: Some(8192),
                height: Some(5464),
                camera_make: Some("Canon".into()),
                camera_model: Some("Canon EOS R5".into()),
                lens: Some("RF 28-70mm F2L".into()),
                focal_length: Some(35.0),
                aperture: Some(2.0),
                iso: Some(3200),
                taken_at: Some(T2024_03 - 1000),
                gps_lat: Some(1.0),
                gps_lon: Some(2.0),
                bit_depth: Some(14),
                ..Default::default()
            },
        )
        .unwrap();
        db.upsert_image_meta(
            ids[1],
            &ImageMeta {
                width: Some(6048),
                camera_make: Some("NIKON CORPORATION".into()),
                camera_model: Some("NIKON Z 6".into()),
                iso: Some(800),
                aperture: Some(5.6),
                taken_at: Some(T2023_06),
                ..Default::default()
            },
        )
        .unwrap();
        db.upsert_video_meta(
            ids[3],
            &VideoMeta {
                duration: Some(25.0),
                width: Some(3840),
                height: Some(2160),
                fps: Some(59.94),
                codec: Some("hevc".into()),
                hdr: Some("HLG".into()),
                camera: Some("Canon EOS R5".into()),
                vfr: true,
                bit_depth: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        db.upsert_video_meta(
            ids[4],
            &VideoMeta {
                duration: Some(95.0),
                width: Some(1920),
                fps: Some(23.976),
                codec: Some("prores".into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.annotate(
            &ids[..2],
            &AnnotationChange {
                rating: Some(5),
                label: Some(Some(ColorLabel::Red)),
                add_tags: vec!["wedding".into()],
                ..Default::default()
            },
        )
        .unwrap();
        db.annotate(
            &ids[2..3],
            &AnnotationChange {
                rating: Some(3),
                label: Some(Some(ColorLabel::Blue)),
                pick: Some(PickState::Pick),
                notes: Some(Some("Golden HOUR".into())),
                add_tags: vec!["Weddings-2023".into()],
                ..Default::default()
            },
        )
        .unwrap();
        db.annotate(
            &ids[4..5],
            &AnnotationChange {
                pick: Some(PickState::Reject),
                ..Default::default()
            },
        )
        .unwrap();
        db.set_hashes(ids[2], None, Some("abc")).unwrap();
        db.set_hashes(ids[6], None, Some("abc")).unwrap();
        db.set_hashes(ids[5], None, Some("zzz")).unwrap();
        let c = db.create_manual_collection("Best Of").unwrap();
        db.add_to_collection(c, &[ids[3]]).unwrap();
        Fixture { db }
    }

    fn names(f: &Fixture, q: &str) -> BTreeSet<String> {
        let opts = SearchOptions::default();
        let res =
            f.db.search_str(q, &opts)
                .unwrap_or_else(|e| panic!("{q}: {e}"));
        let n = f.db.count(&Query::parse(q).unwrap(), &opts).unwrap();
        assert_eq!(n as usize, res.len(), "count mismatch for {q}");
        res.into_iter().map(|r| r.name).collect()
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_filter_type() {
        let f = fixture();
        let n = |q: &str| names(&f, q);
        assert_eq!(n("sunset"), set(&["sunset_001.CR3", "sunset_002.nef"]));
        assert_eq!(n("acme beach"), set(&["beach.jpg"]));
        assert_eq!(n("\"clip_a.mp4\""), set(&["clip_a.mp4"]));
        assert_eq!(
            n("sunset_00?.*"),
            set(&["sunset_001.CR3", "sunset_002.nef"])
        );
        assert_eq!(n("фото*"), set(&["Фото [1].jpg"]));
        assert_eq!(n("name:clip"), set(&["clip_a.mp4", "clip_b.mov"]));
        assert_eq!(n("path:other"), set(&["notes.txt", "Фото [1].jpg"]));
        assert_eq!(n("ext:cr3,nef"), set(&["sunset_001.CR3", "sunset_002.nef"]));
        assert_eq!(n("kind:video"), set(&["clip_a.mp4", "clip_b.mov"]));
        assert_eq!(
            n("kind:image"),
            set(&[
                "sunset_001.CR3",
                "sunset_002.nef",
                "beach.jpg",
                "Фото [1].jpg"
            ])
        );
        assert_eq!(n("kind:dir"), set(&["Acme"]));
        assert_eq!(n(r"re:^clip_[ab]\.m"), set(&["clip_a.mp4", "clip_b.mov"]));
        assert_eq!(n("size:>100mb"), set(&["clip_a.mp4"]));
        assert_eq!(n("size:1kb..3mb"), set(&["notes.txt", "Фото [1].jpg"]));
        assert_eq!(n("modified:2023 ext:jpg"), set(&["beach.jpg"]));
        assert_eq!(n("created:2023-06 kind:video"), set(&["clip_b.mov"]));
        // date: capture date first — sunset_002 was *taken* in 2023 but modified in 2024.
        assert_eq!(
            n("date:2023 kind:image"),
            set(&["sunset_002.nef", "beach.jpg"])
        );
        assert_eq!(n("taken:2023"), set(&["sunset_002.nef"]));
        assert_eq!(n("rating:>=4"), set(&["sunset_001.CR3", "sunset_002.nef"]));
        assert_eq!(n("rating:3 ext:jpg"), set(&["beach.jpg"]));
        assert_eq!(
            n("label:blue,red"),
            set(&["sunset_001.CR3", "sunset_002.nef", "beach.jpg"])
        );
        assert_eq!(n("pick:pick"), set(&["beach.jpg"]));
        assert_eq!(n("pick:reject"), set(&["clip_b.mov"]));
        assert_eq!(n("tag:WEDDING"), set(&["sunset_001.CR3", "sunset_002.nef"]));
        assert_eq!(
            n("tag:wedd*"),
            set(&["sunset_001.CR3", "sunset_002.nef", "beach.jpg"])
        );
        assert_eq!(n("in:\"best of\""), set(&["clip_a.mp4"]));
        assert_eq!(
            n("camera:\"canon eos r5\""),
            set(&["sunset_001.CR3", "clip_a.mp4"])
        );
        assert_eq!(n("camera:nikon"), set(&["sunset_002.nef"]));
        assert_eq!(n("lens:rf"), set(&["sunset_001.CR3"]));
        assert_eq!(n("codec:HEVC"), set(&["clip_a.mp4"]));
        assert_eq!(n("notes:golden"), set(&["beach.jpg"]));
        assert_eq!(n("iso:>=1600"), set(&["sunset_001.CR3"]));
        assert_eq!(n("iso:500..1000"), set(&["sunset_002.nef"]));
        assert_eq!(n("aperture:<=f/2.8"), set(&["sunset_001.CR3"]));
        assert_eq!(n("aperture:5.6"), set(&["sunset_002.nef"]));
        assert_eq!(n("focal:24..50"), set(&["sunset_001.CR3"]));
        assert_eq!(
            n("width:>=3840"),
            set(&["sunset_001.CR3", "sunset_002.nef", "clip_a.mp4"])
        );
        assert_eq!(n("height:2160"), set(&["clip_a.mp4"]));
        assert_eq!(n("duration:<30s"), set(&["clip_a.mp4"]));
        assert_eq!(n("duration:1m..2m"), set(&["clip_b.mov"]));
        assert_eq!(n("fps:59.94"), set(&["clip_a.mp4"]));
        assert_eq!(n("fps:23.976"), set(&["clip_b.mov"]));
        assert_eq!(n("bitdepth:>=10"), set(&["sunset_001.CR3", "clip_a.mp4"]));
        assert_eq!(n("missing:gps kind:raw"), set(&["sunset_002.nef"]));
        assert_eq!(n("has:gps"), set(&["sunset_001.CR3"]));
        assert_eq!(
            n("missing:meta kind:file"),
            set(&["beach.jpg", "notes.txt", "Фото [1].jpg"])
        );
        assert_eq!(n("missing:rating kind:image"), set(&["Фото [1].jpg"]));
        assert_eq!(
            n("missing:label kind:video"),
            set(&["clip_a.mp4", "clip_b.mov"])
        );
        assert_eq!(n("missing:tags ext:jpg"), set(&["Фото [1].jpg"]));
        assert_eq!(
            n("missing:hash kind:image"),
            set(&["sunset_001.CR3", "sunset_002.nef"])
        );
        assert_eq!(
            n("missing:date kind:video"),
            set(&["clip_a.mp4", "clip_b.mov"])
        );
        assert_eq!(n("missing:camera kind:video"), set(&["clip_b.mov"]));
        assert_eq!(n("missing:notes ext:jpg"), set(&["Фото [1].jpg"]));
        assert_eq!(n("is:dup"), set(&["beach.jpg", "Фото [1].jpg"]));
        assert_eq!(n("is:vfr"), set(&["clip_a.mp4"]));
        assert_eq!(n("is:hdr"), set(&["clip_a.mp4"]));
        assert_eq!(n("is:rated kind:video"), BTreeSet::new());
        assert_eq!(
            n("is:labeled"),
            set(&["sunset_001.CR3", "sunset_002.nef", "beach.jpg"])
        );
        assert_eq!(
            n("is:tagged"),
            set(&["sunset_001.CR3", "sunset_002.nef", "beach.jpg"])
        );
        assert_eq!(n("is:pick"), set(&["beach.jpg"]));
        assert_eq!(n("is:reject"), set(&["clip_b.mov"]));
        assert_eq!(n("is:dir"), set(&["Acme"]));
    }

    #[test]
    fn negation_and_or() {
        let f = fixture();
        let n = |q: &str| names(&f, q);
        // Negation over a nullable column must include unlabeled files.
        assert_eq!(
            n("-label:red kind:file"),
            set(&[
                "beach.jpg",
                "clip_a.mp4",
                "clip_b.mov",
                "notes.txt",
                "Фото [1].jpg"
            ])
        );
        assert_eq!(n("-iso:>=1600 kind:raw"), set(&["sunset_002.nef"]));
        assert_eq!(
            n("-kind:image -kind:dir"),
            set(&["clip_a.mp4", "clip_b.mov", "notes.txt"])
        );
        assert_eq!(
            n("ext:cr3 OR ext:mov"),
            set(&["sunset_001.CR3", "clip_b.mov"])
        );
        assert_eq!(
            n("(sunset OR clip) rating:>=4"),
            set(&["sunset_001.CR3", "sunset_002.nef"])
        );
        assert_eq!(n("clip -(codec:hevc OR fps:59.94)"), set(&["clip_b.mov"]));
        assert_eq!(n("NOT has:gps kind:raw"), set(&["sunset_002.nef"]));
        assert_eq!(
            n("-missing:date"),
            set(&["sunset_001.CR3", "sunset_002.nef"])
        );
        assert_eq!(n("-tag:wedding kind:raw"), BTreeSet::new());
        // The big example from the spec compiles and runs.
        let big = r#"sunset ext:cr3,nef iso:>=1600 rating:>=4 camera:"Canon EOS R5" -label:blue date:2024-01..2024-06 size:>10mb width:>=3840 tag:wedding path:clients\ acme OR beach (a OR b) kind:video missing:gps is:dup"#;
        assert_eq!(n(big), set(&["sunset_001.CR3"]));
    }

    #[test]
    fn user_values_are_parameters_not_sql() {
        let f = fixture();
        let evil = r#"name:"x' OR 1=1 --" "'); DROP TABLE files; --""#;
        let q = Query::parse(evil).unwrap();
        let cq = compile_query(&q);
        assert!(!cq.where_sql.contains("DROP"));
        assert!(!cq.where_sql.contains("1=1"));
        assert_eq!(cq.params.len(), 2);
        assert!(
            f.db.search(&q, &SearchOptions::default())
                .unwrap()
                .is_empty()
        );
        assert_eq!(f.db.stats().unwrap().files, 7);
    }

    #[test]
    fn sorting_paging_scope_and_offline() {
        let f = fixture();
        let q = Query::parse("kind:file").unwrap();
        let mut o = SearchOptions {
            sort: SortField::Size,
            descending: true,
            limit: Some(2),
            ..Default::default()
        };
        let r = f.db.search(&q, &o).unwrap();
        assert_eq!(
            r.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["clip_a.mp4", "clip_b.mov"]
        );
        o.offset = 2;
        assert_eq!(f.db.search(&q, &o).unwrap()[0].name, "sunset_002.nef");
        o.sort = SortField::Rating;
        o.offset = 0;
        assert_eq!(f.db.search(&q, &o).unwrap()[0].rating, 5);
        o.sort = SortField::Duration;
        assert_eq!(f.db.search(&q, &o).unwrap()[0].name, "clip_b.mov");
        o.sort = SortField::Date;
        o.limit = None;
        assert_eq!(f.db.search(&q, &o).unwrap().len(), 7);

        let scoped = SearchOptions {
            scope: Some(SearchScope {
                path: PathBuf::from(r"D:\Clients"),
                recursive: true,
            }),
            ..Default::default()
        };
        assert_eq!(f.db.count(&q, &scoped).unwrap(), 5);
        let flat = SearchOptions {
            scope: Some(SearchScope {
                path: PathBuf::from(r"D:\Clients"),
                recursive: false,
            }),
            ..Default::default()
        };
        assert_eq!(f.db.count(&Query::all(), &flat).unwrap(), 1); // just the Acme folder

        f.db.mark_missing(&[PathBuf::from(r"D:\Other")]).unwrap();
        assert_eq!(f.db.count(&q, &SearchOptions::default()).unwrap(), 5);
        assert_eq!(names(&f, "is:offline"), set(&["notes.txt", "Фото [1].jpg"]));
        let all = SearchOptions {
            include_offline: true,
            ..Default::default()
        };
        assert_eq!(f.db.count(&q, &all).unwrap(), 7);
        // is:dup only counts present twins.
        assert_eq!(names(&f, "is:dup"), BTreeSet::new());
    }

    #[test]
    fn relative_dates_compile() {
        let db = Db::open_in_memory().unwrap();
        let now = Local::now();
        let recent = (now - chrono::Duration::days(2)).timestamp_millis();
        let old = Local
            .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        db.upsert_files(&[
            file(r"C:\r\new.txt", 1, recent),
            file(r"C:\r\old.txt", 1, old),
        ])
        .unwrap();
        let n = |q: &str| -> Vec<String> {
            db.search_str(q, &SearchOptions::default())
                .unwrap()
                .into_iter()
                .map(|r| r.name)
                .collect()
        };
        assert_eq!(n("modified:7d"), ["new.txt"]);
        assert_eq!(n("modified:>30d"), ["old.txt"]);
        assert_eq!(
            n("modified:thisyear OR modified:lastyear OR modified:7d"),
            ["new.txt"]
        );
        assert!(matches!(
            db.search_str("size:>12zb", &SearchOptions::default()),
            Err(crate::DbError::Query(_))
        ));
    }

    #[allow(dead_code)]
    fn _ids(_: &[FileId]) {}
}

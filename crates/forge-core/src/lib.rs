//! ZXraw Explorer filesystem engine. Platform-independent; Windows specifics live in `forge-win`.
//!
//! Module ownership (see docs/ARCHITECTURE.md):
//! * `model`, `listing`, `paths`, `error` — shared types & directory enumeration
//! * `ops` — copy/move/delete/create with conflict policies and verification
//! * `rename` — batch rename templates with preview
//! * `hashing`, `duplicates`, `disk_usage`, `compare` — power-user analysis
//! * `query` — search query language (parsed to an AST consumed by `forge-db`)
//! * `ingest` — media import planning & verified copy
//! * `archive` — safe archive browsing/extraction helpers

pub mod archive;
pub mod compare;
pub mod disk_usage;
pub mod duplicates;
pub mod error;
pub mod hashing;
pub mod ingest;
pub mod listing;
pub mod model;
pub mod ops;
pub mod paths;
pub mod query;
pub mod rename;
pub mod split;

pub use error::{CoreError, Result};
pub use model::{EntryKind, FileEntry, MediaKind};

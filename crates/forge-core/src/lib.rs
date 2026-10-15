//! ZXraw Explorer filesystem engine. Platform-independent; Windows specifics live in `forge-win`.
//!
//! Module ownership (see docs/ARCHITECTURE.md):
//! * `model`, `listing`, `paths`, `error` — shared types & directory enumeration
//! * `ops` — copy/move/delete/create with conflict policies and verification
//! * `rename` — batch rename templates with preview
//! * `hashing`, `duplicates`, `disk_usage`, `compare`, `backup_verify` — power-user analysis
//! * `text_replace` — find and replace text in files (preview, atomic write, undo)
//! * `query` — search query language (parsed to an AST consumed by `forge-db`)
//! * `ingest` — media import planning & verified copy
//! * `archive` — safe archive browsing/extraction helpers
//! * `cloud` — cloud placeholder detection: may background work read a file?
//! * `netpath` — UNC shares and mapped network drives (lexical checks, share validation)
//! * `netpolicy` — worker counts for background work on network locations

pub mod archive;
pub mod backup_verify;
pub mod cloud;
pub mod compare;
pub mod disk_usage;
pub mod duplicates;
pub mod error;
pub mod fault;
pub mod hashing;
pub mod ingest;
pub mod listing;
pub mod model;
pub mod netpath;
pub mod netpolicy;
pub mod ops;
pub mod paths;
pub mod query;
pub mod rename;
pub mod split;
pub mod text_replace;

pub use error::{CoreError, Result};
pub use model::{EntryKind, FileEntry, MediaKind};

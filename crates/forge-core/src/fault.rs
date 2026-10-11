//! Test-only fault injection for copy paths (owned by the ingest/copy-robustness work).
//!
//! Lets automated runs simulate a source disappearing mid-copy (a removable card being
//! pulled). Inactive unless explicitly armed; must cost nothing measurable when unarmed.

//! `ui related <verb> [args]` script verbs.
//!
//! Document every verb here (grammar, what it waits for, how it fails). Paths given to
//! verbs that write or delete must be checked with `super::sandboxed`.

use super::{Flow, StepState};
use crate::app::ZxrawApp;

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    let _ = (app, ctx, args, st);
    Err(format!("unknown `ui related` verb `{verb}`"))
}

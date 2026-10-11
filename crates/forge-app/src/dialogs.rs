//! Native file and folder dialogs, with a seam for automated runs.
//!
//! Every native dialog in the app goes through these wrappers. In an automated run
//! (test hook active) no native dialog is ever shown: the wrappers return the answer
//! queued by the script's `dialog <path>` / `dialog cancel` step instead. A dialog opened
//! without a queued answer returns "cancelled" and is reported, so the script fails with
//! a clear message rather than hanging on an invisible window.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static AUTOMATED: AtomicBool = AtomicBool::new(false);
static ANSWERS: Mutex<VecDeque<Option<PathBuf>>> = Mutex::new(VecDeque::new());
static UNEXPECTED: Mutex<Option<String>> = Mutex::new(None);

/// Switches every dialog to scripted answers for the rest of the process.
pub fn set_automated() {
    AUTOMATED.store(true, Ordering::SeqCst);
}

/// Queues the answer for the next dialog (`None` = the user cancelled).
pub fn queue_answer(answer: Option<PathBuf>) {
    ANSWERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push_back(answer);
}

/// The kind of a dialog that was opened without a queued answer, once.
pub fn take_unexpected() -> Option<String> {
    UNEXPECTED.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// `Some(answer)` in automated runs, `None` when the real dialog should be shown.
fn scripted(kind: &str) -> Option<Option<PathBuf>> {
    if !AUTOMATED.load(Ordering::SeqCst) {
        return None;
    }
    let answer = ANSWERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pop_front();
    match answer {
        Some(answer) => Some(answer),
        None => {
            tracing::error!("automated run opened a {kind} dialog with no queued answer");
            *UNEXPECTED.lock().unwrap_or_else(|p| p.into_inner()) = Some(kind.to_string());
            Some(None)
        }
    }
}

pub fn pick_folder(dialog: rfd::FileDialog) -> Option<PathBuf> {
    match scripted("pick folder") {
        Some(answer) => answer,
        None => dialog.pick_folder(),
    }
}

pub fn pick_file(dialog: rfd::FileDialog) -> Option<PathBuf> {
    match scripted("pick file") {
        Some(answer) => answer,
        None => dialog.pick_file(),
    }
}

/// Multi-select file picker. A scripted answer selects one file per queued `dialog` step:
/// a path naming a directory selects every regular file directly inside it (sorted).
#[allow(dead_code)] // for multi-file pickers; covered by unit tests
pub fn pick_files(dialog: rfd::FileDialog) -> Option<Vec<PathBuf>> {
    match scripted("pick files") {
        Some(answer) => answer.map(|p| {
            if p.is_dir() {
                let mut files: Vec<PathBuf> = std::fs::read_dir(&p)
                    .map(|d| {
                        d.filter_map(|e| e.ok().map(|e| e.path()))
                            .filter(|p| p.is_file())
                            .collect()
                    })
                    .unwrap_or_default();
                files.sort();
                files
            } else {
                vec![p]
            }
        }),
        None => dialog.pick_files(),
    }
}

pub fn save_file(dialog: rfd::FileDialog) -> Option<PathBuf> {
    match scripted("save file") {
        Some(answer) => answer,
        None => dialog.save_file(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automated_dialogs_use_queued_answers_and_report_missing_ones() {
        set_automated();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        queue_answer(Some(PathBuf::from(r"C:\picked")));
        queue_answer(None);
        queue_answer(Some(dir.path().to_path_buf()));
        assert_eq!(
            pick_folder(rfd::FileDialog::new()),
            Some(PathBuf::from(r"C:\picked"))
        );
        assert_eq!(save_file(rfd::FileDialog::new()), None);
        assert_eq!(
            pick_files(rfd::FileDialog::new()),
            Some(vec![dir.path().join("a.txt"), dir.path().join("b.txt")])
        );
        assert_eq!(take_unexpected(), None);
        assert_eq!(pick_file(rfd::FileDialog::new()), None);
        assert_eq!(take_unexpected().as_deref(), Some("pick file"));
        assert_eq!(take_unexpected(), None);
    }
}

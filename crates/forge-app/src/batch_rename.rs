//! Batch rename dialog: an ordered list of rules with a live preview. Nothing touches the
//! disk until "Rename" is pressed, and it refuses to run while any row has a problem.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use egui::{Color32, RichText};
use forge_core::rename::{
    self, CaseMode, ExistingNames, RenameInput, RenamePreview, RenameReport, RenameRule,
    RenameStatus,
};
use parking_lot::Mutex;

pub enum BrOutcome {
    Open,
    Closed,
    Applied {
        dirs: Vec<PathBuf>,
        report: Vec<String>,
        undo: Vec<(PathBuf, PathBuf)>,
    },
}

/// What the dialog needs from the disk: names already in each folder and modified dates.
struct Scan {
    existing: ExistingNames,
    dates: Vec<Option<chrono::NaiveDate>>,
}

/// Reads the folders of `paths` and their modified dates. Touches the disk: worker only.
fn scan(paths: &[PathBuf]) -> Scan {
    let mut existing = ExistingNames::default();
    let dirs: BTreeSet<PathBuf> = paths
        .iter()
        .filter_map(|p| p.parent().map(|d| d.to_path_buf()))
        .collect();
    for d in dirs {
        if let Ok(rd) = std::fs::read_dir(&d) {
            let names: Vec<String> = rd
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            existing.add_dir(&d, names);
        }
    }
    // `{date:…}` uses the file's modified date (capture date once indexed).
    let dates = paths
        .iter()
        .map(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).date_naive())
        })
        .collect();
    Scan { existing, dates }
}

pub struct BatchRename {
    inputs: Vec<RenameInput>,
    existing: ExistingNames,
    rules: Vec<RenameRule>,
    previews: Vec<RenamePreview>,
    dirty: bool,
    running: Option<Arc<Mutex<Option<RenameReport>>>>,
    /// The folders' existing names and the items' modified dates, read off the UI thread (a
    /// slow or offline share would freeze the window). `None` once they have arrived.
    scanning: Option<Arc<Mutex<Option<Scan>>>>,
    /// Test hook (`ZXRAW_SCRIPT` `accept`): press the Rename button on the next frame.
    pub(crate) auto_apply: bool,
}

fn rule_label(r: &RenameRule) -> &'static str {
    match r {
        RenameRule::Template { .. } => "Template",
        RenameRule::Replace { .. } => "Find & replace",
        RenameRule::Regex { .. } => "Regex replace",
        RenameRule::ChangeCase(_) => "Change case",
        RenameRule::ExtensionCase(_) => "Extension case",
        RenameRule::Trim => "Trim spaces",
        RenameRule::Insert { .. } => "Insert text",
        RenameRule::RemoveRange { .. } => "Remove characters",
    }
}

fn case_combo(ui: &mut egui::Ui, id: usize, m: &mut CaseMode) -> bool {
    let before = *m;
    egui::ComboBox::from_id_salt(("case", id))
        .selected_text(match m {
            CaseMode::Lower => "lowercase",
            CaseMode::Upper => "UPPERCASE",
            CaseMode::Title => "Title Case",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(m, CaseMode::Lower, "lowercase");
            ui.selectable_value(m, CaseMode::Upper, "UPPERCASE");
            ui.selectable_value(m, CaseMode::Title, "Title Case");
        });
    before != *m
}

impl BatchRename {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let (s2, scan_paths) = (slot.clone(), paths.clone());
        std::thread::spawn(move || *s2.lock() = Some(scan(&scan_paths)));
        Self {
            inputs: paths.into_iter().map(RenameInput::new).collect(),
            existing: ExistingNames::default(),
            rules: vec![RenameRule::Template {
                pattern: "{name}_{n:3}.{ext}".into(),
                start: 1,
                step: 1,
            }],
            previews: Vec::new(),
            dirty: true,
            running: None,
            scanning: Some(slot),
            auto_apply: false,
        }
    }

    /// Takes the folder scan once it has finished; `true` while it is still running.
    fn still_scanning(&mut self) -> bool {
        let Some(slot) = &self.scanning else {
            return false;
        };
        let Some(scan) = slot.lock().take() else {
            return true;
        };
        self.existing = scan.existing;
        for (input, date) in self.inputs.iter_mut().zip(scan.dates) {
            input.date = date;
        }
        self.scanning = None;
        self.dirty = true;
        false
    }

    fn recompute(&mut self) {
        self.previews = rename::preview_with_existing(&self.inputs, &self.rules, &self.existing);
        self.dirty = false;
    }

    pub fn show(&mut self, ctx: &egui::Context) -> BrOutcome {
        let scanning = self.still_scanning();
        if scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        } else if self.dirty {
            self.recompute();
        }
        // Finished?
        let done = self.running.as_ref().and_then(|slot| slot.lock().take());
        if let Some(report) = done {
            return self.finish(report);
        }
        if self.running.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        let mut outcome = BrOutcome::Open;
        let modal = egui::Modal::new(egui::Id::new("batch-rename")).show(ctx, |ui| {
            ui.set_width(820.0);
            ui.heading(format!("Batch rename — {} item(s)", self.inputs.len()));
            ui.add_space(6.0);

            // Rules editor.
            let mut remove = None;
            let mut swap = None;
            for (i, rule) in self.rules.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}. {}", i + 1, rule_label(rule))).strong());
                    let ch = match rule {
                        RenameRule::Template {
                            pattern,
                            start,
                            step,
                        } => {
                            let a = ui
                                .add(egui::TextEdit::singleline(pattern).desired_width(260.0))
                                .changed();
                            ui.label("start");
                            let b = ui.add(egui::DragValue::new(start)).changed();
                            ui.label("step");
                            let c = ui.add(egui::DragValue::new(step).range(1..=1000)).changed();
                            a || b || c
                        }
                        RenameRule::Replace {
                            find,
                            replace,
                            case_sensitive,
                        } => {
                            let a = ui
                                .add(
                                    egui::TextEdit::singleline(find)
                                        .hint_text("find")
                                        .desired_width(180.0),
                                )
                                .changed();
                            let b = ui
                                .add(
                                    egui::TextEdit::singleline(replace)
                                        .hint_text("replace with")
                                        .desired_width(180.0),
                                )
                                .changed();
                            let c = ui.checkbox(case_sensitive, "Match case").changed();
                            a || b || c
                        }
                        RenameRule::Regex {
                            pattern,
                            replacement,
                        } => {
                            let a = ui
                                .add(
                                    egui::TextEdit::singleline(pattern)
                                        .hint_text("regex")
                                        .desired_width(200.0),
                                )
                                .changed();
                            let b = ui
                                .add(
                                    egui::TextEdit::singleline(replacement)
                                        .hint_text("replacement ($1…)")
                                        .desired_width(200.0),
                                )
                                .changed();
                            a || b
                        }
                        RenameRule::ChangeCase(m) | RenameRule::ExtensionCase(m) => {
                            case_combo(ui, i, m)
                        }
                        RenameRule::Trim => false,
                        RenameRule::Insert { at, text } => {
                            ui.label("at");
                            let a = ui.add(egui::DragValue::new(at)).changed();
                            let b = ui
                                .add(egui::TextEdit::singleline(text).desired_width(200.0))
                                .changed();
                            a || b
                        }
                        RenameRule::RemoveRange { start, len } => {
                            ui.label("from");
                            let a = ui.add(egui::DragValue::new(start)).changed();
                            ui.label("count");
                            let b = ui.add(egui::DragValue::new(len)).changed();
                            a || b
                        }
                    };
                    if ch {
                        self.dirty = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✖").on_hover_text("Remove rule").clicked() {
                            remove = Some(i);
                        }
                        if i > 0 && ui.small_button("⏶").on_hover_text("Move up").clicked() {
                            swap = Some(i);
                        }
                    });
                });
            }
            if let Some(i) = remove {
                self.rules.remove(i);
                self.dirty = true;
            }
            if let Some(i) = swap {
                self.rules.swap(i, i - 1);
                self.dirty = true;
            }
            ui.horizontal(|ui| {
                ui.menu_button("+ Add rule", |ui| {
                    let options: [(&str, RenameRule); 8] = [
                        (
                            "Template",
                            RenameRule::Template {
                                pattern: "{name}".into(),
                                start: 1,
                                step: 1,
                            },
                        ),
                        (
                            "Find & replace",
                            RenameRule::Replace {
                                find: String::new(),
                                replace: String::new(),
                                case_sensitive: false,
                            },
                        ),
                        (
                            "Regex replace",
                            RenameRule::Regex {
                                pattern: String::new(),
                                replacement: String::new(),
                            },
                        ),
                        ("Change case", RenameRule::ChangeCase(CaseMode::Lower)),
                        ("Extension case", RenameRule::ExtensionCase(CaseMode::Lower)),
                        ("Trim spaces", RenameRule::Trim),
                        (
                            "Insert text",
                            RenameRule::Insert {
                                at: 0,
                                text: String::new(),
                            },
                        ),
                        (
                            "Remove characters",
                            RenameRule::RemoveRange { start: 0, len: 1 },
                        ),
                    ];
                    for (label, rule) in options {
                        if ui.button(label).clicked() {
                            self.rules.push(rule);
                            self.dirty = true;
                            ui.close();
                        }
                    }
                });
                ui.label(
                    RichText::new("Tokens: {name} {ext} {n} {n:4} {date:%Y-%m-%d} {parent}")
                        .small()
                        .weak(),
                );
            });
            ui.separator();

            // Preview table.
            let problems = self.previews.iter().filter(|p| p.problem.is_some()).count();
            let changes = self.previews.iter().filter(|p| p.is_change()).count();
            if scanning {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading the folder…");
                });
            } else {
                ui.label(format!("{changes} will be renamed, {problems} problem(s)"));
            }
            let row_h = 20.0;
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, true])
                .show_rows(ui, row_h, self.previews.len(), |ui, range| {
                    egui::Grid::new("br-grid")
                        .num_columns(3)
                        .striped(true)
                        .show(ui, |ui| {
                            for p in &self.previews[range] {
                                ui.label(&p.old_name);
                                ui.label("→");
                                match &p.problem {
                                    Some(pr) => ui.label(
                                        RichText::new(format!("{}   ⚠ {pr}", p.new_name))
                                            .color(ui.visuals().error_fg_color),
                                    ),
                                    None if p.is_change() => ui.label(
                                        RichText::new(&p.new_name)
                                            .color(Color32::from_rgb(110, 190, 120)),
                                    ),
                                    None => ui.label(RichText::new(&p.new_name).weak()),
                                };
                                ui.end_row();
                            }
                        });
                });
            ui.separator();
            ui.horizontal(|ui| {
                let can = !scanning && problems == 0 && changes > 0 && self.running.is_none();
                let pressed = ui
                    .add_enabled(can, egui::Button::new(format!("Rename {changes} item(s)")))
                    .clicked();
                if can && (pressed || std::mem::take(&mut self.auto_apply)) {
                    let slot = Arc::new(Mutex::new(None));
                    let previews = self.previews.clone();
                    let s2 = slot.clone();
                    let ctx2 = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let r = rename::apply(&previews);
                        *s2.lock() = Some(r);
                        ctx2.request_repaint();
                    });
                    self.running = Some(slot);
                }
                if self.running.is_some() {
                    ui.spinner();
                } else if ui.button("Cancel").clicked() {
                    outcome = BrOutcome::Closed;
                }
                if problems > 0 {
                    ui.label(
                        RichText::new("Fix the problems above to continue.")
                            .color(ui.visuals().warn_fg_color),
                    );
                }
            });
        });
        if modal.should_close() && self.running.is_none() {
            outcome = BrOutcome::Closed;
        }
        outcome
    }

    fn finish(&mut self, r: RenameReport) -> BrOutcome {
        let dirs: BTreeSet<PathBuf> = self
            .inputs
            .iter()
            .filter_map(|i| i.path.parent().map(|d| d.to_path_buf()))
            .collect();
        let mut lines = Vec::new();
        match &r.status {
            RenameStatus::Completed => lines.push(format!(
                "Renamed {} item(s). Undo with Ctrl+Z.",
                r.renamed.len()
            )),
            RenameStatus::NothingToDo => lines.push("Nothing to rename.".into()),
            RenameStatus::Refused { blocked } => lines.push(format!(
                "Refused: {} item(s) have problems; nothing was changed.",
                blocked.len()
            )),
            RenameStatus::RolledBack { failed, error } => {
                lines.push(format!("Failed on {}: {error}", failed.display()));
                lines.push(format!(
                    "All {} completed rename(s) were reverted; nothing changed.",
                    r.reverted.len()
                ));
            }
            RenameStatus::RollbackIncomplete { failed, error } => {
                lines.push(format!("Failed on {}: {error}", failed.display()));
                lines.push(
                    "WARNING: some files could not be restored to their original names:".into(),
                );
                for s in &r.stranded {
                    lines.push(format!(
                        "  {} (was {}): {}",
                        s.current.display(),
                        s.original.display(),
                        s.error
                    ));
                }
            }
        }
        BrOutcome::Applied {
            dirs: dirs.into_iter().collect(),
            report: lines,
            undo: r.undo,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_scanned(br: &mut BatchRename) {
        let t0 = std::time::Instant::now();
        while br.still_scanning() {
            assert!(
                t0.elapsed().as_secs() < 30,
                "the folder scan never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn opening_on_an_offline_share_returns_at_once() {
        // TEST-NET-1 (RFC 5737) never answers: any disk call on it blocks for a long time.
        let paths = vec![PathBuf::from(r"\\192.0.2.1\x\a.jpg")];
        let t0 = std::time::Instant::now();
        let mut br = BatchRename::new(paths);
        assert!(br.still_scanning());
        assert!(t0.elapsed() < std::time::Duration::from_millis(250));
    }

    #[test]
    fn the_scan_finds_existing_names_and_dates() {
        let d = tempfile::tempdir().unwrap();
        for name in ["a.txt", "taken_001.txt"] {
            std::fs::write(d.path().join(name), b"x").unwrap();
        }
        let mut br = BatchRename::new(vec![d.path().join("a.txt")]);
        br.rules = vec![RenameRule::Template {
            pattern: "taken_{n:3}.{ext}".into(),
            start: 1,
            step: 1,
        }];
        wait_scanned(&mut br);
        assert!(br.inputs[0].date.is_some());
        br.recompute();
        assert!(
            br.previews[0].problem.is_some(),
            "the existing name must be flagged: {:?}",
            br.previews[0]
        );
    }
}

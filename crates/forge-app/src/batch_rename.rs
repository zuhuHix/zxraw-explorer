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

pub struct BatchRename {
    inputs: Vec<RenameInput>,
    existing: ExistingNames,
    rules: Vec<RenameRule>,
    previews: Vec<RenamePreview>,
    dirty: bool,
    running: Option<Arc<Mutex<Option<RenameReport>>>>,
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
        let inputs = paths
            .into_iter()
            .map(|p| {
                let mut i = RenameInput::new(p.clone());
                // `{date:…}` uses the file's modified date (capture date once indexed).
                i.date = std::fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .ok()
                    .map(|t| chrono::DateTime::<chrono::Local>::from(t).date_naive());
                i
            })
            .collect();
        Self {
            inputs,
            existing,
            rules: vec![RenameRule::Template {
                pattern: "{name}_{n:3}.{ext}".into(),
                start: 1,
                step: 1,
            }],
            previews: Vec::new(),
            dirty: true,
            running: None,
            auto_apply: false,
        }
    }

    fn recompute(&mut self) {
        self.previews = rename::preview_with_existing(&self.inputs, &self.rules, &self.existing);
        self.dirty = false;
    }

    pub fn show(&mut self, ctx: &egui::Context) -> BrOutcome {
        if self.dirty {
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
            ui.label(format!("{changes} will be renamed, {problems} problem(s)"));
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
                let can = problems == 0 && changes > 0 && self.running.is_none();
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

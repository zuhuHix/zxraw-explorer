//! Modal dialogs. Each dialog is a state struct; `show` returns an outcome when the user
//! confirms or cancels. Destructive confirmations always state counts and consequences.

use std::path::PathBuf;

use egui::{Key, RichText};
use forge_core::ops::OpKind;

pub enum Modal {
    /// Single-line text prompt (new folder, rename).
    Prompt(Prompt),
    /// Yes/No confirmation for destructive actions.
    Confirm(Confirm),
    /// Informational report (job results, errors).
    Report { title: String, lines: Vec<String> },
}

pub enum PromptKind {
    NewFolder { dir: PathBuf },
    NewFile { dir: PathBuf },
    Rename { path: PathBuf },
    /// Name for "Save layout…" (see `layouts`).
    SaveLayout,
    /// New label for a layout, by index in `Settings::layouts`.
    RenameLayout { index: usize },
    /// New label for a Quick access pin, by its path (see `sidebar`).
    RenamePin { path: PathBuf },
    /// New name for a saved network location, by its path (see `sidebar::network`).
    RenameNetworkLocation { path: PathBuf },
}

impl PromptKind {
    /// A label or name kept in settings rather than a file name on disk.
    pub fn is_label(&self) -> bool {
        matches!(
            self,
            Self::SaveLayout
                | Self::RenameLayout { .. }
                | Self::RenamePin { .. }
                | Self::RenameNetworkLocation { .. }
        )
    }
}

pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    pub text: String,
    pub error: Option<String>,
    focused: bool,
}

impl Prompt {
    pub fn new(kind: PromptKind, title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            text: text.into(),
            error: None,
            focused: false,
        }
    }
}

pub enum ConfirmKind {
    DeletePermanently(Vec<PathBuf>),
    /// Permanent delete through Windows, which can ask for administrator permission.
    DeleteWithWindows(Vec<PathBuf>),
    Recycle(Vec<PathBuf>),
    /// Download and install FFmpeg (see `ffmpeg_setup`).
    DownloadFfmpeg,
    /// A copy or move that would first download online-only files; Accept resumes it.
    /// `clipboard` is `(generation, expected_sequence)` for a clipboard move.
    DownloadCloudFiles {
        sources: Vec<PathBuf>,
        dest: PathBuf,
        kind: OpKind,
        clipboard: Option<(Option<u64>, Option<u32>)>,
    },
    /// The online-only check of a copy or move runs on a background thread. `result` carries
    /// the check's id and warning. Accept starts the transfer without waiting for it.
    CheckingCloudFiles {
        id: u64,
        result: crossbeam_channel::Receiver<(u64, Option<String>)>,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        kind: OpKind,
        clipboard: Option<(Option<u64>, Option<u32>)>,
        /// Stops the scan when the dialog goes away (Cancel, Continue anyway, replaced).
        _scan: crate::ops_ui::CloudScanGuard,
    },
}

pub struct Confirm {
    pub kind: ConfirmKind,
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    pub danger: bool,
}

pub enum Outcome {
    None,
    Cancel,
    Accept,
}

pub fn show(modal: &mut Modal, ctx: &egui::Context) -> Outcome {
    let mut out = Outcome::None;
    let id = egui::Id::new("forge-modal");
    let enter = ctx.input(|i| i.key_pressed(Key::Enter));
    let esc = ctx.input(|i| i.key_pressed(Key::Escape));
    let resp = egui::Modal::new(id).show(ctx, |ui| match modal {
        Modal::Prompt(p) => {
            ui.set_width(420.0);
            ui.heading(&p.title);
            ui.add_space(6.0);
            let r = ui.add(egui::TextEdit::singleline(&mut p.text).desired_width(f32::INFINITY));
            if !p.focused {
                r.request_focus();
                // Pre-select the stem (not the extension) like Explorer does.
                let stem_len = match p.kind {
                    PromptKind::Rename { .. } => p
                        .text
                        .rfind('.')
                        .filter(|&i| i > 0)
                        .map(|i| p.text[..i].chars().count())
                        .unwrap_or_else(|| p.text.chars().count()),
                    _ => p.text.chars().count(),
                };
                if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(stem_len),
                    )));
                    st.store(ui.ctx(), r.id);
                }
                p.focused = true;
            }
            if r.changed() {
                p.error = None;
            }
            if let Some(e) = &p.error {
                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() || (enter && !p.text.trim().is_empty()) {
                    out = Outcome::Accept;
                }
                if ui.button("Cancel").clicked() || esc {
                    out = Outcome::Cancel;
                }
            });
        }
        Modal::Confirm(c) => {
            ui.set_width(460.0);
            ui.heading(&c.title);
            ui.add_space(6.0);
            ui.label(&c.body);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let btn = if c.danger {
                    egui::Button::new(RichText::new(&c.confirm_label).color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(190, 50, 50))
                } else {
                    egui::Button::new(&c.confirm_label)
                };
                // Dangerous confirmations do not accept Enter — require a deliberate click.
                if ui.add(btn).clicked() || (enter && !c.danger) {
                    out = Outcome::Accept;
                }
                if ui.button("Cancel").clicked() || esc {
                    out = Outcome::Cancel;
                }
            });
        }
        Modal::Report { title, lines } => {
            ui.set_width(560.0);
            ui.heading(title.as_str());
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    for l in lines.iter() {
                        ui.label(RichText::new(l).monospace().small());
                    }
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Copy report").clicked() {
                    ui.ctx().copy_text(lines.join("\r\n"));
                }
                if ui.button("Close").clicked() || esc || enter {
                    out = Outcome::Cancel;
                }
            });
        }
    });
    if resp.should_close() && matches!(out, Outcome::None) {
        out = Outcome::Cancel;
    }
    out
}

/// Summarise a list of paths for a confirmation dialog ("3 items: a.jpg, b.jpg, c.jpg").
pub fn describe_items(paths: &[PathBuf]) -> String {
    let names: Vec<String> = paths
        .iter()
        .take(5)
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    let more = if paths.len() > 5 {
        format!(" and {} more", paths.len() - 5)
    } else {
        String::new()
    };
    format!("{}{}", names.join(", "), more)
}

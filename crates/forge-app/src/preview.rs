//! Preview panel: file info for anything, text preview for text-like files, plus media
//! previews/metadata supplied by the media integration.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{RichText, Ui};
use forge_core::FileEntry;
use forge_core::model::{EntryKind, MediaKind, format_size};
use parking_lot::Mutex;

use crate::views;

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "log", "json", "xml", "csv", "ini", "cfg", "toml", "yaml", "yml", "rs", "py",
    "js", "ts", "html", "css", "c", "h", "cpp", "cs", "java", "go", "ps1", "bat", "sh", "xmp",
    "srt", "vtt", "edl", "fcpxml", "otio", "cube", "sql",
];

/// Loads text previews off the UI thread; caches the last result.
#[derive(Default)]
pub struct TextPreview {
    state: Arc<Mutex<Option<(PathBuf, Result<String, String>)>>>,
    requested: Option<PathBuf>,
}

impl TextPreview {
    pub fn is_text(e: &FileEntry) -> bool {
        TEXT_EXTS.contains(&e.ext.as_str())
    }

    pub fn get(&mut self, path: &Path, ctx: &egui::Context) -> Option<Result<String, String>> {
        if let Some((p, r)) = &*self.state.lock() {
            if p == path {
                return Some(r.clone());
            }
        }
        if self.requested.as_deref() != Some(path) {
            self.requested = Some(path.to_path_buf());
            let st = self.state.clone();
            let p = path.to_path_buf();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let r = read_text_head(&p, 64 * 1024);
                *st.lock() = Some((p, r));
                ctx.request_repaint();
            });
        }
        None
    }
}

fn read_text_head(p: &Path, max: usize) -> Result<String, String> {
    let f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(max);
    f.take(max as u64)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.iter().take(4096).filter(|b| **b == 0).count() > 0 {
        return Err("Binary file".into());
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    Ok(s)
}

pub fn info_grid(ui: &mut Ui, e: &FileEntry) {
    egui::Grid::new("info")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            let row = |ui: &mut Ui, k: &str, v: String| {
                ui.label(RichText::new(k).color(ui.visuals().weak_text_color()));
                ui.label(v);
                ui.end_row();
            };
            row(ui, "Type", views::type_label(e));
            if e.kind == EntryKind::File {
                row(
                    ui,
                    "Size",
                    format!("{} ({} bytes)", format_size(e.size), e.size),
                );
            }
            row(ui, "Modified", views::format_time(e.modified));
            row(ui, "Created", views::format_time(e.created));
            let mut attrs = Vec::new();
            if e.readonly {
                attrs.push("read-only");
            }
            if e.hidden {
                attrs.push("hidden");
            }
            if e.system {
                attrs.push("system");
            }
            if e.kind == EntryKind::Link {
                attrs.push("link / junction");
            }
            if !attrs.is_empty() {
                row(ui, "Attributes", attrs.join(", "));
            }
            row(
                ui,
                "Location",
                e.path
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            );
        });
}

pub fn header(ui: &mut Ui, e: &FileEntry) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(views::icon_for(e))
                .size(22.0)
                .color(views::icon_color(e)),
        );
        ui.add(egui::Label::new(RichText::new(&e.name).strong().size(14.0)).wrap());
    });
}

pub fn is_media(e: &FileEntry) -> bool {
    e.kind == EntryKind::File
        && matches!(
            e.media_kind(),
            MediaKind::Image | MediaKind::Raw | MediaKind::Video
        )
}

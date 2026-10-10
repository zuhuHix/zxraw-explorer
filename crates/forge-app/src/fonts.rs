//! Use the system UI font (Segoe UI) and add Windows' CJK / symbol fonts as fallbacks so
//! file names in any script render instead of showing tofu boxes.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

pub fn install(ctx: &egui::Context) {
    let dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let mut defs = FontDefinitions::default();

    // (file, collection index, is primary)
    let candidates: &[(&str, u32, bool)] = &[
        ("segoeui.ttf", 0, true),
        ("seguisym.ttf", 0, false),
        ("msyh.ttc", 0, false),    // Chinese
        ("YuGothM.ttc", 0, false), // Japanese
        ("malgun.ttf", 0, false),  // Korean
        ("Nirmala.ttc", 0, false), // Indic
        ("Nirmala.ttf", 0, false),
        ("seguiemj.ttf", 0, false),
    ];
    let mut primary = Vec::new();
    let mut fallbacks = Vec::new();
    for &(file, index, is_primary) in candidates {
        let Ok(bytes) = std::fs::read(dir.join(file)) else {
            continue;
        };
        let mut fd = FontData::from_owned(bytes);
        fd.index = index;
        defs.font_data.insert(file.to_string(), Arc::new(fd));
        if is_primary {
            primary.push(file.to_string());
        } else {
            fallbacks.push(file.to_string());
        }
    }
    let prop = defs.families.entry(FontFamily::Proportional).or_default();
    for (i, p) in primary.into_iter().enumerate() {
        prop.insert(i, p);
    }
    prop.extend(fallbacks.iter().cloned());
    defs.families
        .entry(FontFamily::Monospace)
        .or_default()
        .extend(fallbacks);
    ctx.set_fonts(defs);
}

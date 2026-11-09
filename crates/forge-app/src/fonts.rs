//! Use the system UI font (Segoe UI) and add Windows' CJK / symbol fonts as fallbacks so
//! file names in any script render instead of showing tofu boxes. On Linux the same roles are
//! filled by the Noto and DejaVu families when they are installed; otherwise egui's built-in
//! font is used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

pub fn install(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();

    // (file, collection index, is primary)
    let candidates: &[(&str, u32, bool)] = if cfg!(windows) {
        &[
            ("segoeui.ttf", 0, true),
            ("seguisym.ttf", 0, false),
            ("msyh.ttc", 0, false),    // Chinese
            ("YuGothM.ttc", 0, false), // Japanese
            ("malgun.ttf", 0, false),  // Korean
            ("Nirmala.ttc", 0, false), // Indic
            ("Nirmala.ttf", 0, false),
            ("seguiemj.ttf", 0, false),
        ]
    } else {
        &[
            ("NotoSans-Regular.ttf", 0, true),
            ("DejaVuSans.ttf", 0, true),
            ("NotoSansCJK-Regular.ttc", 0, false),
            ("NotoSansCJK-VF.ttc", 0, false),
            ("NotoSansSymbols2-Regular.ttf", 0, false),
            ("NotoSansDevanagari-Regular.ttf", 0, false),
            ("NotoColorEmoji.ttf", 0, false),
        ]
    };
    let files = if cfg!(windows) {
        let dir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        candidates
            .iter()
            .filter_map(|&(file, _, _)| Some((file, dir.join(file))))
            .filter(|(_, path)| path.is_file())
            .collect::<HashMap<_, _>>()
    } else {
        let names: Vec<&str> = candidates.iter().map(|c| c.0).collect();
        locate_unix(&names)
    };
    let mut primary = Vec::new();
    let mut fallbacks = Vec::new();
    for &(file, index, is_primary) in candidates {
        let Some(path) = files.get(file) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
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

/// Looks for the named font files under the usual font folders (system, local and the user's),
/// up to a few levels deep, and returns the first copy of each name found.
fn locate_unix<'a>(names: &[&'a str]) -> HashMap<&'a str, PathBuf> {
    let mut roots = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Some(home) = forge_core::paths::home_dir() {
        roots.push(home.join(".local/share/fonts"));
        roots.push(home.join(".fonts"));
    }
    let mut found = HashMap::new();
    for root in roots {
        collect(&root, 4, names, &mut found);
    }
    found
}

fn collect<'a>(dir: &Path, depth: u32, names: &[&'a str], found: &mut HashMap<&'a str, PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth > 0 {
                collect(&path, depth - 1, names, found);
            }
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(&wanted) = names.iter().find(|&&n| n == name) {
            found.entry(wanted).or_insert(path);
        }
    }
}

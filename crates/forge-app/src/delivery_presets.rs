//! Named export settings ("delivery presets") and the rating/pick filter used when
//! delivering a selection to a client. Stored in `delivery_presets.json` in the config folder.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::delivery_meta::MetadataPolicy;
use crate::views::Annotation;
use crate::watermark::Watermark;

pub const FILE_NAME: &str = "delivery_presets.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PresetFormat {
    #[default]
    Jpeg,
    Png,
    Tiff,
    WebP,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum PresetResize {
    #[default]
    Original,
    LongEdge {
        px: u32,
    },
    Within {
        width: u32,
        height: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OutputKind {
    #[default]
    Folder,
    /// One `.zip` in the output folder.
    Zip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SourceFilter {
    #[default]
    All,
    PicksOnly,
    MinRating {
        stars: u8,
    },
}

impl SourceFilter {
    pub fn label(self) -> String {
        match self {
            Self::All => "All images".into(),
            Self::PicksOnly => "Only picks".into(),
            Self::MinRating { stars } => format!("Rating ≥ {stars}★"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeliveryPreset {
    pub name: String,
    pub format: PresetFormat,
    /// JPEG quality 1–100.
    pub quality: u8,
    pub resize: PresetResize,
    /// File naming template (same tokens as batch export).
    pub template: String,
    pub watermark: Option<Watermark>,
    pub output: OutputKind,
    /// Zip file name without `.zip`; tokens `{folder}` and `{date}` (YYYY-MM-DD).
    pub zip_name: String,
    pub filter: SourceFilter,
    pub metadata: MetadataPolicy,
}

impl Default for DeliveryPreset {
    fn default() -> Self {
        Self {
            name: "New preset".into(),
            format: PresetFormat::Jpeg,
            quality: 90,
            resize: PresetResize::LongEdge { px: 2048 },
            template: "{name}".into(),
            watermark: None,
            output: OutputKind::Folder,
            zip_name: "{folder}_{date}".into(),
            filter: SourceFilter::All,
            metadata: MetadataPolicy::Clean,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresetStore {
    pub presets: Vec<DeliveryPreset>,
    /// Name of the preset used most recently.
    pub last_used: Option<String>,
}

impl Default for PresetStore {
    /// The built-in presets.
    fn default() -> Self {
        Self {
            presets: Self::builtins(),
            last_used: None,
        }
    }
}

/// Preset names compare case-insensitively.
fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

impl PresetStore {
    pub fn builtins() -> Vec<DeliveryPreset> {
        vec![
            DeliveryPreset {
                name: "Client web (2048px JPEG 85, watermark, zip)".into(),
                quality: 85,
                resize: PresetResize::LongEdge { px: 2048 },
                watermark: Some(Watermark::default()),
                output: OutputKind::Zip,
                ..DeliveryPreset::default()
            },
            DeliveryPreset {
                name: "Client full resolution (JPEG 95)".into(),
                quality: 95,
                resize: PresetResize::Original,
                ..DeliveryPreset::default()
            },
            DeliveryPreset {
                name: "Social (1080px JPEG 90, picks)".into(),
                quality: 90,
                resize: PresetResize::LongEdge { px: 1080 },
                watermark: Some(Watermark {
                    size_pct: 15.0,
                    ..Watermark::default()
                }),
                filter: SourceFilter::PicksOnly,
                ..DeliveryPreset::default()
            },
        ]
    }

    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    /// Loads the store; a missing file gives the built-ins.
    pub fn load(path: &Path) -> Self {
        crate::settings::load_json(path)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        crate::settings::save_json(path, self)
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.presets.iter().position(|p| same_name(&p.name, name))
    }

    /// The last-used preset, else the first one.
    pub fn last_used_index(&self) -> Option<usize> {
        self.last_used
            .as_deref()
            .and_then(|name| self.index_of(name))
            .or_else(|| (!self.presets.is_empty()).then_some(0))
    }

    /// `base` if no preset has that name (case-insensitive), else `base (2)`, `base (3)`, ….
    pub fn unique_name(&self, base: &str) -> String {
        if self.index_of(base).is_none() {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base} ({n})"))
            .find(|candidate| self.index_of(candidate).is_none())
            .unwrap_or_default()
    }

    /// Inserts a copy named "<name> copy" (made unique) right after `index`; returns the
    /// copy's index.
    pub fn duplicate(&mut self, index: usize) -> usize {
        let mut copy = self.presets[index].clone();
        copy.name = self.unique_name(&format!("{} copy", copy.name));
        self.presets.insert(index + 1, copy);
        index + 1
    }

    /// Removes the preset; clears `last_used` if it pointed at it.
    pub fn delete(&mut self, index: usize) {
        if index >= self.presets.len() {
            return;
        }
        let removed = self.presets.remove(index);
        if self
            .last_used
            .as_deref()
            .is_some_and(|name| same_name(name, &removed.name))
        {
            self.last_used = None;
        }
    }

    /// Renames, rejecting an empty name or one used by another preset. Keeps `last_used`
    /// pointing at the renamed preset.
    pub fn rename(&mut self, index: usize, new_name: &str) -> Result<(), String> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err("Preset name is empty.".into());
        }
        if self.index_of(new_name).is_some_and(|other| other != index) {
            return Err(format!("A preset named \"{new_name}\" already exists."));
        }
        let preset = self
            .presets
            .get_mut(index)
            .ok_or_else(|| "No such preset.".to_string())?;
        let old_name = std::mem::replace(&mut preset.name, new_name.to_string());
        if self
            .last_used
            .as_deref()
            .is_some_and(|name| same_name(name, &old_name))
        {
            self.last_used = Some(new_name.to_string());
        }
        Ok(())
    }

    /// Replaces the preset with the same name, or appends it.
    pub fn upsert(&mut self, preset: DeliveryPreset) -> usize {
        match self.index_of(&preset.name) {
            Some(i) => {
                self.presets[i] = preset;
                i
            }
            None => {
                self.presets.push(preset);
                self.presets.len() - 1
            }
        }
    }

    /// Appends the built-ins that are missing by name.
    pub fn restore_builtins(&mut self) {
        for builtin in Self::builtins() {
            if self.index_of(&builtin.name).is_none() {
                self.presets.push(builtin);
            }
        }
    }
}

/// Keeps the sources that pass `filter`. `annotation` returns the library annotation of a
/// path (None = not annotated: rating 0, no pick). Order is preserved.
pub fn filter_sources(
    sources: &[PathBuf],
    filter: SourceFilter,
    annotation: impl Fn(&Path) -> Option<Annotation>,
) -> Vec<PathBuf> {
    sources
        .iter()
        .filter(|path| {
            let ann = annotation(path.as_path()).unwrap_or_default();
            match filter {
                SourceFilter::All => true,
                SourceFilter::PicksOnly => ann.pick == 1,
                SourceFilter::MinRating { stars } => ann.rating >= stars,
            }
        })
        .cloned()
        .collect()
}

/// Expands `{folder}` and `{date}` in a zip name template and adds `.zip`. The result is a
/// valid Windows file name ("delivery.zip" when empty).
pub fn zip_file_name(template: &str, folder: &str, date: &str) -> String {
    let expanded = template.replace("{folder}", folder).replace("{date}", date);
    let clean: String = expanded
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let base = clean.trim().trim_end_matches(['.', ' ']);
    let mut name = if base.is_empty() {
        "delivery".to_string()
    } else if forge_core::paths::is_reserved_name(base) {
        format!("_{base}")
    } else {
        base.to_string()
    };
    if !name.to_ascii_lowercase().ends_with(".zip") {
        name.push_str(".zip");
    }
    name
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::watermark::{Anchor, WatermarkSource};

    fn store_of(names: &[&str]) -> PresetStore {
        PresetStore {
            presets: names
                .iter()
                .map(|n| DeliveryPreset {
                    name: n.to_string(),
                    ..DeliveryPreset::default()
                })
                .collect(),
            last_used: None,
        }
    }

    fn names(store: &PresetStore) -> Vec<&str> {
        store.presets.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn save_then_load_round_trips_the_whole_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = PresetStore::path(dir.path());

        let mut store = PresetStore::default();
        let mut web = store.presets[0].clone();
        web.format = PresetFormat::Png;
        web.resize = PresetResize::Within {
            width: 1200,
            height: 800,
        };
        web.filter = SourceFilter::MinRating { stars: 3 };
        web.metadata = MetadataPolicy::All;
        web.watermark = Some(Watermark {
            source: WatermarkSource::Image {
                path: PathBuf::from(r"C:\Logos\mark.png"),
            },
            anchor: Anchor::TopLeft,
            ..Watermark::default()
        });
        store.presets[0] = web;
        store.last_used = Some(store.presets[0].name.clone());

        store.save(&path).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"kind\": \"MinRating\""), "{json}");
        assert!(json.contains("\"mode\": \"Within\""), "{json}");

        assert_eq!(PresetStore::load(&path), store);
    }

    #[test]
    fn missing_file_loads_the_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let store = PresetStore::load(&dir.path().join("missing.json"));
        assert_eq!(store, PresetStore::default());
        assert_eq!(
            names(&store),
            vec![
                "Client web (2048px JPEG 85, watermark, zip)",
                "Client full resolution (JPEG 95)",
                "Social (1080px JPEG 90, picks)",
            ]
        );
    }

    #[test]
    fn partial_json_fills_missing_fields_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.json");
        std::fs::write(&path, r#"{"presets":[{"name":"A"}]}"#).unwrap();

        let store = PresetStore::load(&path);
        assert_eq!(store.last_used, None);
        assert_eq!(
            store.presets,
            vec![DeliveryPreset {
                name: "A".into(),
                ..DeliveryPreset::default()
            }]
        );
    }

    #[test]
    fn filter_labels() {
        assert_eq!(SourceFilter::All.label(), "All images");
        assert_eq!(SourceFilter::PicksOnly.label(), "Only picks");
        assert_eq!(SourceFilter::MinRating { stars: 3 }.label(), "Rating ≥ 3★");
    }

    #[test]
    fn unique_name_adds_numbers_case_insensitively() {
        let store = store_of(&["X", "x (2)"]);
        assert_eq!(store.unique_name("Free"), "Free");
        assert_eq!(store.unique_name("X"), "X (3)");
        assert_eq!(store.unique_name("x"), "x (3)");
    }

    #[test]
    fn duplicate_inserts_a_unique_copy_after_the_source() {
        let mut store = store_of(&["A", "B"]);
        assert_eq!(store.duplicate(0), 1);
        assert_eq!(names(&store), vec!["A", "A copy", "B"]);
        assert_eq!(store.duplicate(0), 1);
        assert_eq!(names(&store), vec!["A", "A copy (2)", "A copy", "B"]);

        let mut expected = store_of(&["A"]).presets.remove(0);
        expected.name = "A copy".into();
        assert_eq!(store.presets[2], expected);
    }

    #[test]
    fn delete_removes_and_clears_last_used() {
        let mut store = store_of(&["A", "B", "C"]);
        store.last_used = Some("b".into());
        store.delete(9);
        assert_eq!(names(&store), vec!["A", "B", "C"]);
        assert_eq!(store.last_used.as_deref(), Some("b"));

        store.delete(1);
        assert_eq!(names(&store), vec!["A", "C"]);
        assert_eq!(store.last_used, None);

        store.last_used = Some("A".into());
        store.delete(1);
        assert_eq!(store.last_used.as_deref(), Some("A"));
    }

    #[test]
    fn rename_validates_and_follows_last_used() {
        let mut store = store_of(&["A", "B"]);
        store.last_used = Some("A".into());

        assert_eq!(store.rename(0, "   ").unwrap_err(), "Preset name is empty.");
        assert_eq!(
            store.rename(0, "b").unwrap_err(),
            "A preset named \"b\" already exists."
        );
        assert_eq!(names(&store), vec!["A", "B"]);
        assert_eq!(store.last_used.as_deref(), Some("A"));

        store.rename(0, "a").unwrap();
        assert_eq!(names(&store), vec!["a", "B"]);
        assert_eq!(store.last_used.as_deref(), Some("a"));

        store.rename(0, "  Renamed ").unwrap();
        assert_eq!(names(&store), vec!["Renamed", "B"]);
        assert_eq!(store.last_used.as_deref(), Some("Renamed"));
    }

    #[test]
    fn last_used_index_falls_back_to_first() {
        let mut store = store_of(&["A", "B"]);
        assert_eq!(store.last_used_index(), Some(0));
        store.last_used = Some("b".into());
        assert_eq!(store.last_used_index(), Some(1));
        store.last_used = Some("gone".into());
        assert_eq!(store.last_used_index(), Some(0));
        assert_eq!(store_of(&[]).last_used_index(), None);
    }

    #[test]
    fn upsert_replaces_by_name_or_appends() {
        let mut store = store_of(&["A", "B"]);
        let replacement = DeliveryPreset {
            name: "b".into(),
            quality: 50,
            ..DeliveryPreset::default()
        };
        assert_eq!(store.upsert(replacement.clone()), 1);
        assert_eq!(store.presets[1], replacement);
        assert_eq!(store.presets.len(), 2);

        let added = DeliveryPreset {
            name: "C".into(),
            ..DeliveryPreset::default()
        };
        assert_eq!(store.upsert(added.clone()), 2);
        assert_eq!(store.presets[2], added);
    }

    #[test]
    fn restore_builtins_appends_only_missing_names() {
        let mut store = PresetStore {
            presets: vec![DeliveryPreset {
                name: "client full resolution (jpeg 95)".into(),
                ..DeliveryPreset::default()
            }],
            last_used: None,
        };
        store.restore_builtins();
        assert_eq!(
            names(&store),
            vec![
                "client full resolution (jpeg 95)",
                "Client web (2048px JPEG 85, watermark, zip)",
                "Social (1080px JPEG 90, picks)",
            ]
        );
        store.restore_builtins();
        assert_eq!(store.presets.len(), 3);
    }

    #[test]
    fn filter_sources_by_pick_and_rating() {
        let paths: Vec<PathBuf> = ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let annotations: HashMap<PathBuf, Annotation> = HashMap::from([
            (paths[0].clone(), Annotation { rating: 5, label: None, pick: 1, has_note: false }),
            (paths[1].clone(), Annotation { rating: 3, label: Some(2), pick: 0, has_note: false }),
            (paths[2].clone(), Annotation { rating: 0, label: None, pick: -1, has_note: false }),
            (paths[3].clone(), Annotation { rating: 4, label: None, pick: 1, has_note: false }),
            // e.jpg is unannotated.
        ]);
        let run = |filter| filter_sources(&paths, filter, |p| annotations.get(p).copied());
        let pick = |list: &[&str]| -> Vec<PathBuf> { list.iter().map(PathBuf::from).collect() };

        assert_eq!(run(SourceFilter::All), paths);
        assert_eq!(run(SourceFilter::PicksOnly), pick(&["a.jpg", "d.jpg"]));
        assert_eq!(
            run(SourceFilter::MinRating { stars: 3 }),
            pick(&["a.jpg", "b.jpg", "d.jpg"])
        );
        assert_eq!(run(SourceFilter::MinRating { stars: 0 }), paths);

        // Unannotated files count as rating 0 and no pick.
        assert_eq!(run(SourceFilter::MinRating { stars: 1 }), pick(&["a.jpg", "b.jpg", "d.jpg"]));

        // Order follows the input.
        let reversed: Vec<PathBuf> = paths.iter().rev().cloned().collect();
        let kept = filter_sources(&reversed, SourceFilter::MinRating { stars: 3 }, |p| {
            annotations.get(p).copied()
        });
        assert_eq!(kept, pick(&["d.jpg", "b.jpg", "a.jpg"]));
    }

    #[test]
    fn zip_file_name_expands_tokens() {
        assert_eq!(
            zip_file_name("{folder}_{date}", "Wedding", "2026-10-14"),
            "Wedding_2026-10-14.zip"
        );
        assert_eq!(zip_file_name("{date}-{folder}-{date}", "x", "d"), "d-x-d.zip");
    }

    #[test]
    fn zip_file_name_replaces_invalid_characters() {
        assert_eq!(
            zip_file_name("a<b>c:d\"e/f\\g|h?i*j", "", ""),
            "a_b_c_d_e_f_g_h_i_j.zip"
        );
        assert_eq!(zip_file_name("x\ty", "", ""), "x_y.zip");
        assert_eq!(zip_file_name("{folder}", "a/b", ""), "a_b.zip");
    }

    #[test]
    fn zip_file_name_trims_and_falls_back() {
        assert_eq!(zip_file_name("", "", ""), "delivery.zip");
        assert_eq!(zip_file_name("{folder}", "", ""), "delivery.zip");
        assert_eq!(zip_file_name("  .. ", "", ""), "delivery.zip");
        assert_eq!(zip_file_name("  name. . ", "", ""), "name.zip");
    }

    #[test]
    fn zip_file_name_guards_reserved_names() {
        assert_eq!(zip_file_name("CON", "", ""), "_CON.zip");
        assert_eq!(zip_file_name("{folder}", "lpt1", ""), "_lpt1.zip");
        assert_eq!(zip_file_name("nul.txt", "", ""), "_nul.txt.zip");
        assert_eq!(zip_file_name("CONSOLE", "", ""), "CONSOLE.zip");
    }

    #[test]
    fn zip_file_name_keeps_existing_zip_extension() {
        assert_eq!(zip_file_name("photos.ZIP", "", ""), "photos.ZIP");
        assert_eq!(zip_file_name("photos.zip", "", ""), "photos.zip");
    }
}

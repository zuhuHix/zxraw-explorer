//! Culling model: annotations, the store abstraction, filters, key mapping and
//! navigation helpers. Everything here is pure (no egui painting, no I/O).

use std::path::{Path, PathBuf};

/// Per-file culling state. `rating` 0..=5, `label` 0..5 = red, yellow, green, blue,
/// purple, `pick` 1 = pick, -1 = reject, 0 = none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Annotation {
    pub rating: u8,
    pub label: Option<u8>,
    pub pick: i8,
}

/// A partial edit: `None` fields are left unchanged. `label: Some(None)` clears the label.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct AnnotationChange {
    pub rating: Option<u8>,
    pub label: Option<Option<u8>>,
    pub pick: Option<i8>,
}

impl AnnotationChange {
    /// The annotation after applying this change (used by in-memory stores and tests).
    pub fn apply(&self, mut a: Annotation) -> Annotation {
        if let Some(r) = self.rating {
            a.rating = r.min(5);
        }
        if let Some(l) = self.label {
            a.label = l.filter(|l| *l < 5);
        }
        if let Some(p) = self.pick {
            a.pick = p.signum();
        }
        a
    }
}

/// Where annotations live (the app implements this on top of forge-db). The viewer never
/// writes to image files; every edit goes through [`AnnotationStore::set`].
pub trait AnnotationStore {
    fn get(&self, path: &Path) -> Annotation;
    fn set(&mut self, paths: &[PathBuf], change: AnnotationChange);
}

/// Simple in-memory store (demo / tests / fallback).
#[derive(Debug, Default, Clone)]
pub struct MemoryStore {
    pub map: std::collections::HashMap<PathBuf, Annotation>,
}

impl AnnotationStore for MemoryStore {
    fn get(&self, path: &Path) -> Annotation {
        self.map.get(path).copied().unwrap_or_default()
    }
    fn set(&mut self, paths: &[PathBuf], change: AnnotationChange) {
        for p in paths {
            let a = change.apply(self.get(p));
            self.map.insert(p.clone(), a);
        }
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Filter {
    #[default]
    All,
    Picks,
    Rejects,
    Unrated,
    /// Rating ≥ N (1..=5).
    MinRating(u8),
}

impl Filter {
    pub fn matches(self, a: Annotation) -> bool {
        match self {
            Filter::All => true,
            Filter::Picks => a.pick > 0,
            Filter::Rejects => a.pick < 0,
            Filter::Unrated => a.rating == 0,
            Filter::MinRating(n) => a.rating >= n,
        }
    }

    pub fn label(self) -> String {
        match self {
            Filter::All => "All".into(),
            Filter::Picks => "Picks".into(),
            Filter::Rejects => "Rejects".into(),
            Filter::Unrated => "Unrated".into(),
            Filter::MinRating(n) => format!("≥ {n}★"),
        }
    }
}

/// Indices (into the item list) that pass `filter`.
pub fn filter_items(n: usize, filter: Filter, ann: impl Fn(usize) -> Annotation) -> Vec<usize> {
    if filter == Filter::All {
        return (0..n).collect();
    }
    (0..n).filter(|&i| filter.matches(ann(i))).collect()
}

/// Position in `visible` for item `cur`: the item itself if visible, else the next visible
/// item after it in original order, else the last visible one. `None` when empty.
pub fn reselect(visible: &[usize], cur: usize) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    Some(match visible.binary_search(&cur) {
        Ok(p) => p,
        Err(p) => p.min(visible.len() - 1),
    })
}

/// Next position from `pos` (exclusive) in direction `forward` whose item satisfies `pred`.
pub fn next_matching(
    visible: &[usize],
    pos: usize,
    forward: bool,
    pred: impl Fn(usize) -> bool,
) -> Option<usize> {
    if forward {
        (pos + 1..visible.len()).find(|&p| pred(visible[p]))
    } else {
        (0..pos.min(visible.len()))
            .rev()
            .find(|&p| pred(visible[p]))
    }
}

// ---------------------------------------------------------------------------
// Keyboard culling
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CullAction {
    Rate(u8),
    ToggleLabel(u8),
    Pick,
    Reject,
    Unflag,
}

/// Maps a key to a culling action. Digits should be passed by *physical* key so
/// non-US layouts (AZERTY, PT) and Shift combinations work.
///
/// * `0`–`5` rating, `6` red, `7` yellow, `8` green, `9` blue, `Shift+6` or `V` purple
/// * `P` pick, `X` reject, `U` unflag
pub fn action_for_key(key: egui::Key, shift: bool) -> Option<CullAction> {
    use egui::Key as K;
    Some(match key {
        K::Num0 => CullAction::Rate(0),
        K::Num1 => CullAction::Rate(1),
        K::Num2 => CullAction::Rate(2),
        K::Num3 => CullAction::Rate(3),
        K::Num4 => CullAction::Rate(4),
        K::Num5 => CullAction::Rate(5),
        K::Num6 if shift => CullAction::ToggleLabel(4),
        K::Num6 => CullAction::ToggleLabel(0),
        K::Num7 => CullAction::ToggleLabel(1),
        K::Num8 => CullAction::ToggleLabel(2),
        K::Num9 => CullAction::ToggleLabel(3),
        K::V => CullAction::ToggleLabel(4),
        K::P => CullAction::Pick,
        K::X => CullAction::Reject,
        K::U => CullAction::Unflag,
        _ => return None,
    })
}

/// The edit an action produces given the current annotation (labels toggle).
pub fn change_for(action: CullAction, current: Annotation) -> AnnotationChange {
    let mut c = AnnotationChange::default();
    match action {
        CullAction::Rate(r) => c.rating = Some(r.min(5)),
        CullAction::ToggleLabel(l) => {
            c.label = Some(if current.label == Some(l) {
                None
            } else {
                Some(l)
            })
        }
        CullAction::Pick => c.pick = Some(1),
        CullAction::Reject => c.pick = Some(-1),
        CullAction::Unflag => c.pick = Some(0),
    }
    c
}

/// Auto-advance applies to ratings and pick/reject decisions, not to labels or unflag.
pub fn advances(action: CullAction) -> bool {
    matches!(
        action,
        CullAction::Rate(_) | CullAction::Pick | CullAction::Reject
    )
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// `2431` → `"2,431"`.
pub fn format_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn stars(rating: u8) -> String {
    let r = rating.min(5) as usize;
    format!("{}{}", "★".repeat(r), "☆".repeat(5 - r))
}

pub fn pick_text(pick: i8) -> Option<&'static str> {
    match pick {
        1 => Some("Pick"),
        -1 => Some("Reject"),
        _ => None,
    }
}

/// `"123 / 2,431 · ★★★☆☆ · Pick"` (1-based position).
pub fn header_text(pos: usize, total: usize, a: Annotation) -> String {
    let mut s = format!(
        "{} / {} · {}",
        format_count(pos + 1),
        format_count(total),
        stars(a.rating)
    );
    if let Some(p) = pick_text(a.pick) {
        s.push_str(" · ");
        s.push_str(p);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Key;

    fn ann(rating: u8, pick: i8) -> Annotation {
        Annotation {
            rating,
            label: None,
            pick,
        }
    }

    #[test]
    fn filter_logic() {
        let anns = [ann(0, 0), ann(3, 1), ann(5, -1), ann(1, 0), ann(0, 1)];
        let f = |flt| filter_items(anns.len(), flt, |i| anns[i]);
        assert_eq!(f(Filter::All), vec![0, 1, 2, 3, 4]);
        assert_eq!(f(Filter::Picks), vec![1, 4]);
        assert_eq!(f(Filter::Rejects), vec![2]);
        assert_eq!(f(Filter::Unrated), vec![0, 4]);
        assert_eq!(f(Filter::MinRating(3)), vec![1, 2]);
        assert_eq!(f(Filter::MinRating(1)), vec![1, 2, 3]);
    }

    #[test]
    fn reselect_after_filter() {
        let vis = vec![1, 4, 7];
        assert_eq!(reselect(&vis, 4), Some(1));
        assert_eq!(reselect(&vis, 5), Some(2)); // next after
        assert_eq!(reselect(&vis, 9), Some(2)); // past end → last
        assert_eq!(reselect(&vis, 0), Some(0));
        assert_eq!(reselect(&[], 3), None);
    }

    #[test]
    fn next_unrated_navigation() {
        let anns = [ann(2, 0), ann(0, 0), ann(3, 0), ann(4, 0), ann(0, 0)];
        let vis: Vec<usize> = (0..anns.len()).collect();
        let unrated = |i: usize| anns[i].rating == 0;
        assert_eq!(next_matching(&vis, 0, true, unrated), Some(1));
        assert_eq!(next_matching(&vis, 1, true, unrated), Some(4));
        assert_eq!(next_matching(&vis, 4, true, unrated), None);
        assert_eq!(next_matching(&vis, 4, false, unrated), Some(1));
        assert_eq!(next_matching(&vis, 1, false, unrated), None);
        // Works on a filtered list (positions, not item indices).
        let vis = vec![0, 2, 4];
        assert_eq!(next_matching(&vis, 0, true, unrated), Some(2));
    }

    #[test]
    fn key_to_change_mapping() {
        let none = Annotation::default();
        let c = |k, shift, a| action_for_key(k, shift).map(|act| change_for(act, a));
        assert_eq!(c(Key::Num3, false, none).unwrap().rating, Some(3));
        assert_eq!(c(Key::Num0, false, ann(4, 0)).unwrap().rating, Some(0));
        assert_eq!(c(Key::Num6, false, none).unwrap().label, Some(Some(0)));
        assert_eq!(c(Key::Num9, false, none).unwrap().label, Some(Some(3)));
        assert_eq!(c(Key::Num6, true, none).unwrap().label, Some(Some(4)));
        assert_eq!(c(Key::V, false, none).unwrap().label, Some(Some(4)));
        // Same label again toggles it off.
        let red = Annotation {
            label: Some(0),
            ..none
        };
        assert_eq!(c(Key::Num6, false, red).unwrap().label, Some(None));
        // Different label replaces.
        assert_eq!(c(Key::Num7, false, red).unwrap().label, Some(Some(1)));
        assert_eq!(c(Key::P, false, none).unwrap().pick, Some(1));
        assert_eq!(c(Key::X, false, none).unwrap().pick, Some(-1));
        assert_eq!(c(Key::U, false, ann(0, 1)).unwrap().pick, Some(0));
        assert!(c(Key::Q, false, none).is_none());
        // Only the touched field changes.
        let ch = c(Key::P, false, none).unwrap();
        assert!(ch.rating.is_none() && ch.label.is_none());
        assert!(advances(CullAction::Rate(2)) && !advances(CullAction::ToggleLabel(1)));
    }

    #[test]
    fn memory_store_applies_changes() {
        let mut s = MemoryStore::default();
        let p = PathBuf::from("a.jpg");
        s.set(
            std::slice::from_ref(&p),
            AnnotationChange {
                rating: Some(9),
                label: Some(Some(2)),
                pick: Some(1),
            },
        );
        assert_eq!(
            s.get(&p),
            Annotation {
                rating: 5,
                label: Some(2),
                pick: 1
            }
        );
        s.set(
            std::slice::from_ref(&p),
            AnnotationChange {
                label: Some(None),
                ..Default::default()
            },
        );
        assert_eq!(s.get(&p).label, None);
        assert_eq!(s.get(&p).rating, 5);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(2431), "2,431");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(
            header_text(122, 2431, ann(3, 1)),
            "123 / 2,431 · ★★★☆☆ · Pick"
        );
        assert_eq!(header_text(0, 1, ann(0, 0)), "1 / 1 · ☆☆☆☆☆");
    }
}

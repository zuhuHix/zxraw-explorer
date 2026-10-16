//! The selected paths of a pane, with a revision that moves on every change.

use std::collections::HashSet;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Source of selection revisions, shared by all panes so two selections never report the
/// same one after a change.
static SELECTION_REV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_selection_rev() -> u64 {
    SELECTION_REV.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The selected paths of a pane. Reads go through `Deref` to the set; every change goes
/// through a method here and moves `revision`, so per-frame code can notice any change
/// (not only one in the first few paths) with one comparison.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    paths: HashSet<PathBuf>,
    revision: u64,
}

impl Deref for Selection {
    type Target = HashSet<PathBuf>;

    fn deref(&self) -> &HashSet<PathBuf> {
        &self.paths
    }
}

impl FromIterator<PathBuf> for Selection {
    fn from_iter<I: IntoIterator<Item = PathBuf>>(iter: I) -> Self {
        let mut selection = Selection::default();
        selection.extend(iter);
        selection
    }
}

impl Selection {
    /// Changes whenever the selected paths change.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns whether `path` was added.
    pub fn insert(&mut self, path: PathBuf) -> bool {
        let added = self.paths.insert(path);
        if added {
            self.revision = next_selection_rev();
        }
        added
    }

    /// Returns whether `path` was removed.
    pub fn remove(&mut self, path: &Path) -> bool {
        let removed = self.paths.remove(path);
        if removed {
            self.revision = next_selection_rev();
        }
        removed
    }

    pub fn clear(&mut self) {
        if !self.paths.is_empty() {
            self.paths.clear();
            self.revision = next_selection_rev();
        }
    }

    pub fn extend(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let before = self.paths.len();
        self.paths.extend(paths);
        if self.paths.len() != before {
            self.revision = next_selection_rev();
        }
    }

    pub fn retain(&mut self, keep: impl FnMut(&PathBuf) -> bool) {
        let before = self.paths.len();
        self.paths.retain(keep);
        if self.paths.len() != before {
            self.revision = next_selection_rev();
        }
    }

    /// Replaces the whole selection. Setting the same paths again changes nothing.
    pub fn replace(&mut self, paths: HashSet<PathBuf>) {
        if self.paths != paths {
            self.paths = paths;
            self.revision = next_selection_rev();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> PathBuf {
        PathBuf::from(format!(r"C:\fixture\{name}"))
    }

    #[test]
    fn revision_moves_on_every_change_that_changes_something() {
        let mut s = Selection::default();
        let mut last = s.revision();
        let moved = |s: &Selection, last: &mut u64| {
            assert_ne!(s.revision(), *last);
            *last = s.revision();
        };

        assert!(s.insert(p("a")));
        moved(&s, &mut last);
        assert!(s.remove(&p("a")));
        moved(&s, &mut last);
        s.extend([p("a"), p("b")]);
        moved(&s, &mut last);
        s.retain(|x| x != &p("b"));
        moved(&s, &mut last);
        s.replace(HashSet::from([p("c")]));
        moved(&s, &mut last);
        s.clear();
        moved(&s, &mut last);
    }

    #[test]
    fn changes_that_change_nothing_keep_the_revision() {
        let mut s: Selection = [p("a")].into_iter().collect();
        let rev = s.revision();
        assert!(!s.insert(p("a")));
        assert!(!s.remove(&p("zz")));
        s.extend([p("a")]);
        s.retain(|_| true);
        s.replace(HashSet::from([p("a")]));
        assert_eq!(s.revision(), rev);
        s.clear();
        let cleared = s.revision();
        s.clear();
        assert_eq!(s.revision(), cleared);
    }

    #[test]
    fn different_selections_never_share_a_revision() {
        let mut a = Selection::default();
        let mut b = Selection::default();
        a.insert(p("a"));
        b.insert(p("a"));
        assert_ne!(a.revision(), b.revision());
        b.clear();
        a.clear();
        assert_ne!(a.revision(), b.revision());
        let c: Selection = [p("x")].into_iter().collect();
        assert_ne!(c.revision(), a.revision());
        assert_ne!(c.revision(), b.revision());
    }
}

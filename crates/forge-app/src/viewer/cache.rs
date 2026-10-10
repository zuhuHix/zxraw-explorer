//! Byte-budgeted LRU used for decoded textures.

use std::hash::Hash;

use lru::LruCache;

pub struct ByteLru<K: Hash + Eq, V> {
    map: LruCache<K, (V, usize)>,
    bytes: usize,
    budget: usize,
}

#[allow(dead_code)] // full API exercised by tests
impl<K: Hash + Eq + Clone, V> ByteLru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self {
            map: LruCache::unbounded(),
            bytes: 0,
            budget,
        }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Changes the budget, evicting as needed. Returns evicted entries.
    pub fn set_budget(&mut self, budget: usize) -> Vec<(K, V)> {
        self.budget = budget;
        self.evict()
    }

    /// Looks up and marks as most recently used.
    pub fn get(&mut self, k: &K) -> Option<&V> {
        self.map.get(k).map(|(v, _)| v)
    }

    pub fn peek(&self, k: &K) -> Option<&V> {
        self.map.peek(k).map(|(v, _)| v)
    }

    pub fn contains(&self, k: &K) -> bool {
        self.map.contains(k)
    }

    /// Inserts (replacing any previous value) and evicts least-recently-used entries until
    /// within budget. The newest entry is always kept, even if it alone exceeds the budget.
    pub fn insert(&mut self, k: K, v: V, size: usize) -> Vec<(K, V)> {
        if let Some((_, old)) = self.map.put(k, (v, size)) {
            self.bytes -= old;
        }
        self.bytes += size;
        self.evict()
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let (v, s) = self.map.pop(k)?;
        self.bytes -= s;
        Some(v)
    }

    fn evict(&mut self) -> Vec<(K, V)> {
        let mut out = Vec::new();
        while self.bytes > self.budget && self.map.len() > 1 {
            match self.map.pop_lru() {
                Some((k, (v, s))) => {
                    self.bytes -= s;
                    out.push((k, v));
                }
                None => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_budget_evicts_lru() {
        let mut c: ByteLru<u32, &str> = ByteLru::new(100);
        assert!(c.insert(1, "a", 40).is_empty());
        assert!(c.insert(2, "b", 40).is_empty());
        assert_eq!(c.bytes(), 80);
        // Touch 1 so 2 becomes least recently used.
        assert_eq!(c.get(&1), Some(&"a"));
        let ev = c.insert(3, "c", 40);
        assert_eq!(ev, vec![(2, "b")]);
        assert_eq!(c.bytes(), 80);
        assert!(c.contains(&1) && c.contains(&3) && !c.contains(&2));
    }

    #[test]
    fn replace_updates_bytes_and_oversize_kept() {
        let mut c: ByteLru<u32, u8> = ByteLru::new(100);
        c.insert(1, 0, 60);
        c.insert(1, 1, 30);
        assert_eq!(c.bytes(), 30);
        assert_eq!(c.len(), 1);
        // A single entry over budget is kept (it's what the user is looking at)...
        let ev = c.insert(2, 2, 500);
        assert_eq!(ev, vec![(1, 1)]);
        assert_eq!(c.len(), 1);
        assert_eq!(c.bytes(), 500);
        // ...and evicted by the next insert.
        let ev = c.insert(3, 3, 10);
        assert_eq!(ev, vec![(2, 2)]);
        assert_eq!(c.bytes(), 10);
        assert_eq!(c.remove(&3), Some(3));
        assert_eq!(c.bytes(), 0);
    }

    #[test]
    fn shrinking_budget_evicts() {
        let mut c: ByteLru<u32, ()> = ByteLru::new(1000);
        for i in 0..10 {
            c.insert(i, (), 100);
        }
        assert_eq!(c.bytes(), 1000);
        let ev = c.set_budget(350);
        assert_eq!(ev.len(), 7);
        assert_eq!(ev[0].0, 0);
        assert!(c.bytes() <= 350);
    }
}

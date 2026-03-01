/// Union-Find (Disjoint Set) data structure.
///
/// Port of DisjointSet.ts. K must be Copy + Eq + Hash.
use std::collections::HashMap;
use std::hash::Hash;

pub struct DisjointSet<K: Copy + Eq + Hash> {
    entries: HashMap<K, K>,
}

impl<K: Copy + Eq + Hash> DisjointSet<K> {
    pub fn new() -> Self {
        DisjointSet { entries: HashMap::new() }
    }

    /// Merge all items into one group.
    pub fn union(&mut self, items: &[K]) {
        if items.is_empty() {
            return;
        }
        let first = items[0];
        let root = match self.find(first) {
            Some(r) => r,
            None => {
                self.entries.insert(first, first);
                first
            }
        };
        for &item in &items[1..] {
            if !self.entries.contains_key(&item) {
                self.entries.insert(item, root);
            } else {
                let mut current = item;
                loop {
                    let parent = *self.entries.get(&current).unwrap();
                    if parent == root {
                        break;
                    }
                    if parent == current {
                        // current was a different root; re-point to our root
                        self.entries.insert(current, root);
                        break;
                    }
                    self.entries.insert(current, root);
                    current = parent;
                }
            }
        }
    }

    /// Find the canonical root for `item` with path compression.
    pub fn find(&mut self, item: K) -> Option<K> {
        if !self.entries.contains_key(&item) {
            return None;
        }
        let mut path = vec![item];
        let mut current = item;
        loop {
            let parent = *self.entries.get(&current).unwrap();
            if parent == current {
                break;
            }
            path.push(parent);
            current = parent;
        }
        let root = current;
        for node in path {
            self.entries.insert(node, root);
        }
        Some(root)
    }

    /// Build a canonical map: item → root. Mutates self (path compression).
    pub fn canonicalize(&mut self) -> HashMap<K, K> {
        let keys: Vec<K> = self.entries.keys().copied().collect();
        let mut out = HashMap::new();
        for item in keys {
            let root = self.find(item).unwrap();
            out.insert(item, root);
        }
        out
    }

    pub fn has(&self, item: K) -> bool {
        self.entries.contains_key(&item)
    }
}

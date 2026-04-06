use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Time-bounded set. Entries expire after TTL, bounded by max capacity.
pub struct ExpiringCache<K: Eq + std::hash::Hash + Copy> {
    entries: HashMap<K, Instant>,
    insertion_order: Vec<K>,
    ttl: Duration,
    capacity: usize,
}

impl<K: Eq + std::hash::Hash + Copy> ExpiringCache<K> {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: Vec::new(),
            ttl,
            capacity,
        }
    }

    pub fn insert(&mut self, key: K) {
        if self.entries.contains_key(&key) {
            return;
        }
        while self.entries.len() >= self.capacity {
            if let Some(oldest) = self.insertion_order.first().copied() {
                self.entries.remove(&oldest);
                self.insertion_order.remove(0);
            } else {
                break;
            }
        }
        self.entries.insert(key, Instant::now());
        self.insertion_order.push(key);
    }

    pub fn contains(&self, key: &K) -> bool {
        match self.entries.get(key) {
            Some(inserted) => inserted.elapsed() < self.ttl,
            None => false,
        }
    }

    /// Remove all expired entries.
    pub fn prune(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, inserted| now.duration_since(*inserted) < self.ttl);
        self.insertion_order.retain(|k| self.entries.contains_key(k));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.insertion_order.clear();
    }
}

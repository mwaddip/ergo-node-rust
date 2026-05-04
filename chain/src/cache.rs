//! Lazy header/score caching for [`crate::HeaderChain`].
//!
//! Backs the migration from in-memory `Vec<Header>` / `Vec<BigUint>`
//! to an LRU cache + lazy load from persistent storage. At mainnet
//! scale (1.76M headers) the Vec alternative is ~1.4 GB; the cache at
//! the default 16k capacity is ~10 MB across both caches. The
//! integrator (main crate) wires [`HeaderLoader`] and [`ScoreLoader`]
//! against `enr-store`.
//!
//! Phase 1 role: cache and loaders are installed on
//! [`crate::HeaderChain`] and kept coherent via write-through on
//! push/pop/reorg. No reads are routed through the cache yet — that
//! arrives in Phase 2.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use ergo_chain_types::Header;
use lru::LruCache;
use num_bigint::BigUint;

/// Default LRU capacity.
///
/// Chosen to cover, with generous slack:
/// - the difficulty-recalculation walk (mainnet `use_last_epochs *
///   epoch_length` = 8 × 1024 = 8192 headers per incoming header), and
/// - the deepest finalization-depth reorg (1440 blocks).
///
/// At ~500 B/header + ~80 B/score this is ~10 MB total across both
/// caches.
pub const DEFAULT_CACHE_CAPACITY: usize = 16_384;

/// Callback for loading a header by height from persistent storage.
///
/// Returns `None` if no header is stored at that height. Wired by the
/// integrator (main crate) to bridge `enr-store`.
pub type HeaderLoader =
    Arc<dyn Fn(u32) -> Option<Header> + Send + Sync + 'static>;

/// Callback for loading a cumulative-difficulty score by height.
///
/// Split from [`HeaderLoader`] so consumers that only need the header
/// (NiPoPoW build, difficulty walk) don't pay `BigUint`
/// deserialization on every lookup.
pub type ScoreLoader =
    Arc<dyn Fn(u32) -> Option<BigUint> + Send + Sync + 'static>;

/// Paired LRU caches + loaders for headers and cumulative scores.
///
/// Owned by [`crate::HeaderChain`]. Interior mutability via `Mutex`
/// because `lru::LruCache::get` requires `&mut self` (it updates
/// recency) but the chain's public read API is `&self`. The mutex is
/// not contended in normal use — the integrator already serializes
/// chain access with an outer lock.
pub(crate) struct LazyHeaderStore {
    headers: Mutex<LruCache<u32, Header>>,
    scores: Mutex<LruCache<u32, BigUint>>,
    header_loader: Option<HeaderLoader>,
    score_loader: Option<ScoreLoader>,
}

impl LazyHeaderStore {
    /// Create a cache with the default capacity
    /// ([`DEFAULT_CACHE_CAPACITY`]).
    pub fn with_default_capacity() -> Self {
        Self::with_capacity(
            NonZeroUsize::new(DEFAULT_CACHE_CAPACITY)
                .expect("DEFAULT_CACHE_CAPACITY is nonzero"),
        )
    }

    pub fn with_capacity(capacity: NonZeroUsize) -> Self {
        Self {
            headers: Mutex::new(LruCache::new(capacity)),
            scores: Mutex::new(LruCache::new(capacity)),
            header_loader: None,
            score_loader: None,
        }
    }

    /// Resize both caches in place. Existing entries beyond the new
    /// capacity are evicted (LRU-order) per the underlying crate's
    /// `resize` semantics.
    pub fn resize(&self, capacity: NonZeroUsize) {
        self.headers.lock().unwrap().resize(capacity);
        self.scores.lock().unwrap().resize(capacity);
    }

    pub fn set_header_loader(&mut self, loader: HeaderLoader) {
        self.header_loader = Some(loader);
    }

    pub fn set_score_loader(&mut self, loader: ScoreLoader) {
        self.score_loader = Some(loader);
    }

    pub fn has_header_loader(&self) -> bool {
        self.header_loader.is_some()
    }

    pub fn has_score_loader(&self) -> bool {
        self.score_loader.is_some()
    }

    /// Fetch the header at `height`. Cache hit returns the cloned
    /// header and updates recency; cache miss falls through to the
    /// loader (if wired) and inserts the loaded value before
    /// returning.
    pub fn get_header(&self, height: u32) -> Option<Header> {
        if let Some(h) = self.headers.lock().unwrap().get(&height).cloned() {
            return Some(h);
        }
        let loader = self.header_loader.as_ref()?;
        let header = loader(height)?;
        self.headers.lock().unwrap().put(height, header.clone());
        Some(header)
    }

    /// Fetch the cumulative score at `height`. Same cache-then-loader
    /// pattern as [`Self::get_header`].
    pub fn get_score(&self, height: u32) -> Option<BigUint> {
        if let Some(s) = self.scores.lock().unwrap().get(&height).cloned() {
            return Some(s);
        }
        let loader = self.score_loader.as_ref()?;
        let score = loader(height)?;
        self.scores.lock().unwrap().put(height, score.clone());
        Some(score)
    }

    /// Insert a freshly-appended (header, score) pair into both
    /// caches. Called by [`crate::HeaderChain::push_header`] et al.
    /// for write-through coherence.
    pub fn put(&self, height: u32, header: Header, score: BigUint) {
        self.headers.lock().unwrap().put(height, header);
        self.scores.lock().unwrap().put(height, score);
    }

    /// Evict a specific height from both caches. Called on
    /// pop/reorg-drain so stale entries don't shadow a legitimate
    /// loader miss after the canonical state has moved on.
    pub fn evict(&self, height: u32) {
        self.headers.lock().unwrap().pop(&height);
        self.scores.lock().unwrap().pop(&height);
    }

    /// Clear both caches. Used by `rollback_install` to match the
    /// full-state-clear semantics of that path.
    pub fn clear(&self) {
        self.headers.lock().unwrap().clear();
        self.scores.lock().unwrap().clear();
    }

    // ---- Test-only observers ----

    /// Peek at the cached header for `height` without updating
    /// recency or consulting the loader.
    #[cfg(test)]
    pub fn peek_header(&self, height: u32) -> Option<Header> {
        self.headers.lock().unwrap().peek(&height).cloned()
    }

    /// Peek at the cached score for `height` without updating
    /// recency or consulting the loader.
    #[cfg(test)]
    pub fn peek_score(&self, height: u32) -> Option<BigUint> {
        self.scores.lock().unwrap().peek(&height).cloned()
    }

    /// Current header-cache length (entries resident, not capacity).
    #[cfg(test)]
    pub fn header_cache_len(&self) -> usize {
        self.headers.lock().unwrap().len()
    }

    /// Current score-cache length (entries resident, not capacity).
    #[cfg(test)]
    pub fn score_cache_len(&self) -> usize {
        self.scores.lock().unwrap().len()
    }
}

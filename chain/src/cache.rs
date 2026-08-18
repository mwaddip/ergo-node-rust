//! Lazy header/score caching for [`crate::HeaderChain`].
//!
//! LRU cache + lazy load from persistent storage for headers and
//! cumulative-difficulty scores, bounded by [`DEFAULT_CACHE_CAPACITY`]
//! regardless of chain length. The integrator (main crate) wires
//! [`HeaderLoader`] and [`ScoreLoader`] against `enr-store`.

use std::mem::size_of;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use ergo_chain_types::{EcPoint, Header};
use lru::LruCache;
use num_bigint::BigUint;

/// Default LRU capacity.
///
/// Chosen to cover, with generous slack:
/// - the difficulty-recalculation walk (mainnet `use_last_epochs *
///   epoch_length` = 8 × 1024 = 8192 headers per incoming header), and
/// - the deepest finalization-depth reorg (1440 blocks).
///
/// Full at this capacity the two caches hold roughly 11 MB — but do not
/// hand-carry that number anywhere. `header_entry_bytes` and
/// `score_entry_bytes` below derive it from the actual types, and
/// `LazyHeaderStore::header_cache_bytes` reports it at live occupancy.
pub const DEFAULT_CACHE_CAPACITY: usize = 16_384;

// ---- Memory attribution ----
//
// Formulas over type sizes and live counts, never measurements. They
// live here, beside the caches they model, so a refactor of those
// caches is editing the same file. See `facts/chain.md` § "Memory
// attribution" and [`crate::ChainMemoryEstimate`].

/// Bytes the `lru` crate spends on an entry regardless of its value:
/// the `u32` height key and the two intrusive list pointers inside the
/// boxed `LruEntry` node it allocates per entry.
///
/// `LruEntry` is private to `lru`, so the node is reconstructed from
/// its fields rather than measured; the reconstruction ignores the few
/// bytes of padding the real layout adds.
const LRU_NODE_OVERHEAD_BYTES: u64 = (size_of::<u32>() + 2 * size_of::<usize>()) as u64;

/// Bytes one entry occupies in the LRU's internal index, a
/// `HashMap<KeyRef<u32>, NonNull<LruEntry<..>>>`: the two pointers of
/// the slot plus hashbrown's one control byte per slot.
const LRU_INDEX_SLOT_BYTES: u64 = (2 * size_of::<usize>() + 1) as u64;

/// Payload of `AutolykosSolution::nonce`, a `Vec<u8>` that carries the
/// 8-byte Autolykos nonce.
const AUTOLYKOS_NONCE_BYTES: u64 = 8;

/// Heap digits of one cumulative-difficulty `BigUint`.
///
/// `num-bigint` stores 64-bit digits on a 64-bit target. Mainnet total
/// work sits around 2^71 — two digits, a 16-byte request — and this
/// leaves room for the allocator's rounding of that request plus
/// growth as the chain's total work climbs.
const SCORE_DIGIT_BYTES: u64 = 32;

/// Estimated bytes held by one resident entry of the header LRU.
///
/// Formula, not a measurement:
/// - `size_of::<Header>()` — the value, stored inline in the boxed
///   `LruEntry` node.
/// - `2 × size_of::<EcPoint>()` — `Header::autolykos_solution`'s
///   `miner_pk` and `pow_onetime_pk`, each a `Box<EcPoint>` pointing
///   out of line. Both are populated in practice: parsing an Autolykos
///   v2 solution materializes the group generator into
///   `pow_onetime_pk`, mirroring the JVM's `wForV2`.
/// - the nonce payload, the LRU node overhead, and the entry's slot in
///   the LRU's index.
///
/// `Header::unparsed_bytes` and `AutolykosSolution::pow_distance` are
/// not counted: both are empty / `None` in every header version the
/// node accepts today.
pub(crate) const fn header_entry_bytes() -> u64 {
    size_of::<Header>() as u64
        + 2 * size_of::<EcPoint>() as u64
        + AUTOLYKOS_NONCE_BYTES
        + LRU_NODE_OVERHEAD_BYTES
        + LRU_INDEX_SLOT_BYTES
}

/// Estimated bytes held by one resident entry of the score LRU.
///
/// Formula, not a measurement: `size_of::<BigUint>()` for the value
/// inline in the boxed `LruEntry` node, plus its heap digits, plus the
/// node overhead and the entry's slot in the LRU's index.
pub(crate) const fn score_entry_bytes() -> u64 {
    size_of::<BigUint>() as u64 + SCORE_DIGIT_BYTES + LRU_NODE_OVERHEAD_BYTES + LRU_INDEX_SLOT_BYTES
}

/// Callback for loading a header by height from persistent storage.
///
/// Returns `None` if no header is stored at that height. Wired by the
/// integrator (main crate) to bridge `enr-store`.
pub type HeaderLoader = Arc<dyn Fn(u32) -> Option<Header> + Send + Sync + 'static>;

/// Callback for loading a cumulative-difficulty score by height.
///
/// Split from [`HeaderLoader`] so consumers that only need the header
/// (NiPoPoW build, difficulty walk) don't pay `BigUint`
/// deserialization on every lookup.
pub type ScoreLoader = Arc<dyn Fn(u32) -> Option<BigUint> + Send + Sync + 'static>;

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
            NonZeroUsize::new(DEFAULT_CACHE_CAPACITY).expect("DEFAULT_CACHE_CAPACITY is nonzero"),
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

    /// Estimated bytes held by the header LRU **at its current
    /// occupancy** — `entries × header_entry_bytes()`. A half-full
    /// cache reports half; the capacity ceiling is not the answer.
    ///
    /// Constant time: reads `LruCache::len` and touches no entry.
    ///
    /// Not counted: the LRU's index table, which `LruCache::new`
    /// preallocates at full capacity. Below full occupancy the real
    /// footprint therefore exceeds this by the unused slots — roughly
    /// half a megabyte for an empty default-capacity cache. The
    /// contract asks for occupancy, and each resident entry's share of
    /// that table is already included above.
    pub fn header_cache_bytes(&self) -> u64 {
        self.headers.lock().unwrap().len() as u64 * header_entry_bytes()
    }

    /// Estimated bytes held by the score LRU at its current occupancy —
    /// `entries × score_entry_bytes()`. Same occupancy semantics,
    /// constant-time guarantee, and index-table caveat as
    /// [`Self::header_cache_bytes`].
    pub fn score_cache_bytes(&self) -> u64 {
        self.scores.lock().unwrap().len() as u64 * score_entry_bytes()
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

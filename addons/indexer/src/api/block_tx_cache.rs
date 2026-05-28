//! Single-flight + bounded cache over per-block transaction fetches.
//!
//! Motivation: `GET /api/v1/boxes/{id}/bytes` composes a box's canonical bytes
//! by fetching the *whole* creating block's transactions from the node, then
//! extracting the one output. The validation harness requests boxes 64-wide,
//! and those boxes usually live in the same block (it walks one block's
//! inputs). Without coordination that fires 64 concurrent identical
//! `GET /blocks/{id}/transactions` at the node — each a full fat-block
//! deserialize — and the node melts.
//!
//! This layer collapses that burst: concurrent requests for the same
//! `header_id` share one in-flight fetch (single-flight), and completed
//! fetches are cached so the harness's *sequential* walk across one block's
//! boxes also skips the node. Blocks are immutable once indexed, so caching by
//! `header_id` carries no staleness risk within the cache window.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::OnceCell;

use crate::node_client::BlockTransactionsJson;

/// Max distinct blocks held at once. A deserialized fat block is ~2.6 MB on
/// the wire and several MB as a parsed JSON value; 16 entries bounds the cache
/// at tens of MB while comfortably covering the hot path (a harness walking one
/// block's boxes, occasionally spanning a couple of adjacent blocks). The point
/// is to cap memory, not to maximise hit rate across the whole chain.
const CAPACITY: usize = 16;

type Block = Arc<BlockTransactionsJson>;
type Cell = Arc<OnceCell<Block>>;

pub struct BlockTxCache {
    inner: Mutex<Inner>,
}

struct Inner {
    map: HashMap<String, Entry>,
    /// Monotonic access counter; the entry with the lowest `last_used` is the
    /// LRU eviction victim. u64 never realistically wraps.
    tick: u64,
}

struct Entry {
    cell: Cell,
    last_used: u64,
}

impl BlockTxCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                tick: 0,
            }),
        }
    }

    /// Return the block transactions for `header_id`, fetching via `fetch`
    /// exactly once across all concurrent callers for that key.
    ///
    /// On a cache hit the stored value is returned without calling `fetch`. On
    /// a miss, the first caller runs `fetch` while concurrent callers for the
    /// same key wait and then share its result. If `fetch` errors the entry is
    /// left uninitialized so the next caller retries — a transient node failure
    /// never poisons the cache, and a persistent one degrades to *serialized*
    /// retries rather than the concurrent burst this layer exists to prevent.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        header_id: &str,
        fetch: F,
    ) -> anyhow::Result<Block>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<BlockTransactionsJson>>,
    {
        let cell = self.cell_for(header_id);
        let value = cell
            .get_or_try_init(|| async move { fetch().await.map(Arc::new) })
            .await?;
        Ok(value.clone())
    }

    /// Look up (or create) the cell for `header_id`, refreshing recency and
    /// evicting the LRU entry when at capacity. The lock is held only for this
    /// bookkeeping — never across the fetch await — so concurrent fetches for
    /// distinct blocks never serialize on it.
    fn cell_for(&self, header_id: &str) -> Cell {
        let mut inner = self.inner.lock().unwrap();
        inner.tick += 1;
        let tick = inner.tick;

        if let Some(entry) = inner.map.get_mut(header_id) {
            entry.last_used = tick;
            return entry.cell.clone();
        }

        if inner.map.len() >= CAPACITY {
            if let Some(victim) = inner
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                inner.map.remove(&victim);
            }
        }

        let cell: Cell = Arc::new(OnceCell::new());
        inner.map.insert(
            header_id.to_string(),
            Entry {
                cell: cell.clone(),
                last_used: tick,
            },
        );
        cell
    }
}

impl Default for BlockTxCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    fn block_with(marker: &str) -> BlockTransactionsJson {
        BlockTransactionsJson {
            transactions: vec![json!({ "id": marker })],
        }
    }

    /// N concurrent requests for the SAME block must collapse to ONE fetch, and
    /// every caller must receive the one shared instance. This is the exact
    /// regression for the production gridlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_same_block_one_fetch() {
        const N: usize = 64;
        let cache = Arc::new(BlockTxCache::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let cache = cache.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_fetch("block-A", || {
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            // Widen the overlap window so all N callers are
                            // genuinely in-flight before the first completes.
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            Ok(block_with("A"))
                        }
                    })
                    .await
                    .expect("fetch should succeed")
            }));
        }

        let results: Vec<Block> = futures_join(handles).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "64 concurrent same-block requests must trigger exactly one fetch"
        );
        // Every caller got the one shared allocation, not a private copy.
        for r in &results {
            assert!(Arc::ptr_eq(r, &results[0]), "all callers share one Arc");
            assert_eq!(r.transactions[0]["id"].as_str(), Some("A"));
        }
    }

    /// Sequential requests for the same block stay served from cache: the fetch
    /// count never climbs past the first miss.
    #[tokio::test]
    async fn sequential_same_block_hits_cache() {
        let cache = BlockTxCache::new();
        let calls = Arc::new(AtomicUsize::new(0));

        for _ in 0..10 {
            let calls = calls.clone();
            let block = cache
                .get_or_fetch("block-A", || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(block_with("A"))
                })
                .await
                .unwrap();
            assert_eq!(block.transactions[0]["id"].as_str(), Some("A"));
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "sequential same-block requests must fetch only once"
        );
    }

    /// Distinct blocks are keyed independently: each fetches once, and a second
    /// pass is fully cached. Guards against a key being ignored (which would
    /// silently serve one block's txs for another — a correctness disaster).
    #[tokio::test]
    async fn distinct_blocks_keyed_independently() {
        let cache = BlockTxCache::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let keys = ["block-A", "block-B", "block-C"];

        for pass in 0..2 {
            for k in keys {
                let calls = calls.clone();
                let block = cache
                    .get_or_fetch(k, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(block_with(k))
                    })
                    .await
                    .unwrap();
                assert_eq!(block.transactions[0]["id"].as_str(), Some(k), "pass {pass} key {k}");
            }
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            keys.len(),
            "one fetch per distinct block, second pass cached"
        );
    }

    /// A failed fetch must NOT be cached: the next caller retries. Only once a
    /// fetch succeeds does the result stick.
    #[tokio::test]
    async fn failed_fetch_is_not_cached() {
        let cache = BlockTxCache::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let fetch = |calls: Arc<AtomicUsize>| async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(anyhow::anyhow!("transient node error"))
            } else {
                Ok(block_with("A"))
            }
        };

        // First attempt fails.
        let r1 = cache.get_or_fetch("block-A", || fetch(calls.clone())).await;
        assert!(r1.is_err(), "first fetch errors");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Retry succeeds — proves the error was not cached.
        let r2 = cache.get_or_fetch("block-A", || fetch(calls.clone())).await;
        assert!(r2.is_ok(), "retry after error succeeds");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // Now cached — no further fetch.
        let r3 = cache.get_or_fetch("block-A", || fetch(calls.clone())).await;
        assert!(r3.is_ok());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "successful result is cached; no third fetch"
        );
    }

    /// Minimal join over spawned handles without pulling in `futures`.
    async fn futures_join(handles: Vec<tokio::task::JoinHandle<Block>>) -> Vec<Block> {
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.await.expect("task panicked"));
        }
        out
    }
}

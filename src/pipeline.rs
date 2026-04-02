use std::num::NonZeroUsize;
use std::sync::Arc;

use enr_chain::{BlockId, ChainError, Header, HeaderChain, HeaderTracker, HEADER_TYPE_ID};
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_sync::delivery::DeliveryEvent;
use lru::LruCache;
use sigma_ser::ScorexSerializable;
use tokio::sync::{mpsc, Mutex};

/// LRU buffer capacity for out-of-order headers (JVM: `headersCache` = 8192).
const BUFFER_CAPACITY: usize = 8_192;

/// Async validation pipeline for modifiers.
///
/// Receives raw modifier data from the P2P layer via a channel, validates
/// in batches (sort by height, PoW check, chain-validate), and updates
/// the shared HeaderChain. Runs as a single tokio task.
///
/// Out-of-order headers are buffered in an LRU cache. Evicted headers are
/// reported to the delivery tracker for re-request.
pub struct ValidationPipeline {
    rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
    chain: Arc<Mutex<HeaderChain>>,
    store: Arc<RedbModifierStore>,
    progress_tx: mpsc::Sender<u32>,
    delivery_tx: mpsc::Sender<DeliveryEvent>,
    tracker: HeaderTracker,
    buffer: LruCache<BlockId, (Header, Vec<u8>)>,
}

impl ValidationPipeline {
    pub fn new(
        rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
        chain: Arc<Mutex<HeaderChain>>,
        store: Arc<RedbModifierStore>,
        progress_tx: mpsc::Sender<u32>,
        delivery_tx: mpsc::Sender<DeliveryEvent>,
    ) -> Self {
        Self {
            rx,
            chain,
            store,
            progress_tx,
            delivery_tx,
            tracker: HeaderTracker::new(),
            buffer: LruCache::new(NonZeroUsize::new(BUFFER_CAPACITY).unwrap()),
        }
    }

    /// Run the pipeline loop. Returns when the channel closes.
    pub async fn run(&mut self) {
        tracing::info!("validation pipeline started");
        loop {
            let first = match self.rx.recv().await {
                Some(item) => item,
                None => {
                    tracing::info!("validation pipeline channel closed");
                    return;
                }
            };

            let mut batch = vec![first];
            while let Ok(item) = self.rx.try_recv() {
                batch.push(item);
            }

            self.process_batch(batch).await;
        }
    }

    /// Process a batch of raw modifiers.
    pub(crate) async fn process_batch(&mut self, batch: Vec<(u8, [u8; 32], Vec<u8>)>) {
        // Single pass: collect IDs for delivery tracker, partition headers from sections
        let mut received_ids = Vec::with_capacity(batch.len());
        let mut raw_headers: Vec<&[u8]> = Vec::new();
        let mut section_entries: Vec<(u8, [u8; 32], u32, Vec<u8>)> = Vec::new();

        for (type_id, id, data) in &batch {
            received_ids.push(*id);
            if *type_id == HEADER_TYPE_ID {
                raw_headers.push(data.as_slice());
            } else if !data.is_empty() {
                // Height 0 signals "height unknown" — the store skips the height
                // index write for height=0. The sync machine pre-registers the
                // correct height via SyncStore::put_height before data arrives.
                section_entries.push((*type_id, *id, 0, data.clone()));
            }
        }

        // Notify delivery tracker
        if !received_ids.is_empty() {
            if self.delivery_tx.try_send(DeliveryEvent::Received(received_ids)).is_err() {
                tracing::warn!("delivery channel full, dropped Received notification");
            }
        }

        // Store non-header block sections directly (no validation)
        if !section_entries.is_empty() {
            let count = section_entries.len();
            if let Err(e) = self.store.put_batch(&section_entries) {
                tracing::error!(count, "store write failed for block sections: {e}");
            } else {
                tracing::debug!(count, "stored block sections");
            }
        }

        if raw_headers.is_empty() {
            return;
        }

        // Parse, round-trip check, and PoW-verify
        let mut valid_headers: Vec<(Header, Vec<u8>)> = Vec::with_capacity(raw_headers.len());
        for data in raw_headers {
            let header = match enr_chain::parse_header(data) {
                Ok(h) => h,
                Err(e) => {
                    tracing::debug!("pipeline: rejecting header: parse failed: {e}");
                    continue;
                }
            };

            // Round-trip check: detect headers whose re-serialization produces
            // different bytes. These would break SyncInfo (commonPoint fails).
            if let Ok(reserialized) = header.scorex_serialize_bytes() {
                if data != reserialized.as_slice() {
                    let first_diff = data.iter().zip(reserialized.iter())
                        .position(|(a, b)| a != b);
                    tracing::error!(
                        height = header.height,
                        wire_len = data.len(),
                        reser_len = reserialized.len(),
                        first_diff_at = ?first_diff,
                        wire_prefix = format!("{:02x?}", &data[..data.len().min(20)]),
                        reser_prefix = format!("{:02x?}", &reserialized[..reserialized.len().min(20)]),
                        "ROUND-TRIP MISMATCH"
                    );
                }
            }

            if let Err(e) = enr_chain::verify_pow(&header) {
                tracing::debug!(
                    "pipeline: rejecting header at height {}: {e}",
                    header.height
                );
                continue;
            }
            valid_headers.push((header, data.to_vec()));
        }

        if valid_headers.is_empty() {
            return;
        }

        // Sort by height — within a batch this eliminates most buffering
        valid_headers.sort_by_key(|(h, _)| h.height);
        let batch_size = valid_headers.len();

        // Lock chain once for the whole batch
        let mut chain = self.chain.lock().await;
        let height_before = chain.height();

        let mut chained = 0u32;
        let mut buffered = 0u32;
        let mut rejected = 0u32;
        let mut evicted_ids: Vec<[u8; 32]> = Vec::new();
        let mut store_entries: Vec<(u8, [u8; 32], u32, Vec<u8>)> = Vec::new();

        for (header, raw) in valid_headers {
            // Skip headers strictly below the chain tip (duplicates).
            // Headers AT tip height with a different ID are competing blocks —
            // buffer them for potential 1-deep reorg.
            if header.height < chain.height() {
                rejected += 1;
                continue;
            }
            if header.height == chain.height() && header.id == chain.tip().id {
                rejected += 1;
                continue;
            }
            self.tracker.observe(&header);
            let header_id = header.id;
            let header_height = header.height;
            let parent_id = header.parent_id;
            match chain.try_append(header.clone()) {
                Ok(()) => {
                    chained += 1;
                    store_entries.push((HEADER_TYPE_ID, header_id.0.0, header_height, raw));
                    if header_height % 400 < 2 {
                        tracing::info!(height = header_height, id = %header_id, "chained header ID");
                    }
                    // Drain buffer: follow the chain of buffered children
                    let mut next_parent = header_id;
                    while let Some((buf, buf_raw)) = self.buffer.pop(&next_parent) {
                        let bid = buf.id;
                        let buf_height = buf.height;
                        match chain.try_append(buf.clone()) {
                            Ok(()) => {
                                chained += 1;
                                store_entries.push((HEADER_TYPE_ID, bid.0.0, buf_height, buf_raw));
                                self.tracker.observe(&buf);
                                next_parent = bid;
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(ChainError::ParentNotFound { .. }) => {
                    tracing::debug!(
                        header_height,
                        chain_height = chain.height(),
                        id = %header_id,
                        parent = %parent_id,
                        "ParentNotFound"
                    );
                    // Check for 1-deep reorg: the header's parent might be in the
                    // buffer (a competing block at tip height). If so, try to replace
                    // our tip with the alternative chain.
                    if header_height == chain.height() + 1 {
                        // The alternative block shares our tip's parent. It would be
                        // buffered under key = tip.parent_id (its own parent_id).
                        let tip_parent = chain.tip().parent_id;
                        tracing::info!(
                            header_height,
                            parent = %parent_id,
                            tip_parent = %tip_parent,
                            buffer_len = self.buffer.len(),
                            "reorg check: looking for alternative at tip_parent"
                        );
                        if let Some((alt, alt_raw)) = self.buffer.pop(&tip_parent) {
                            match chain.try_reorg(alt.clone(), header.clone()) {
                                Ok(old_tip_id) => {
                                    tracing::info!(
                                        height = header_height,
                                        old_tip = %old_tip_id,
                                        new_tip = %header_id,
                                        "1-deep reorg: replaced tip"
                                    );
                                    chained += 2; // alternative + continuation
                                    store_entries.push((HEADER_TYPE_ID, alt.id.0.0, alt.height, alt_raw));
                                    store_entries.push((HEADER_TYPE_ID, header_id.0.0, header_height, raw));
                                    // Drain buffer from the new tip
                                    let mut next_parent = header_id;
                                    while let Some((buf, buf_raw)) = self.buffer.pop(&next_parent) {
                                        let bid = buf.id;
                                        let buf_height = buf.height;
                                        match chain.try_append(buf.clone()) {
                                            Ok(()) => {
                                                chained += 1;
                                                store_entries.push((HEADER_TYPE_ID, bid.0.0, buf_height, buf_raw));
                                                self.tracker.observe(&buf);
                                                next_parent = bid;
                                            }
                                            Err(_) => break,
                                        }
                                    }
                                    continue;
                                }
                                Err(e) => {
                                    tracing::debug!(height = header_height, "reorg failed: {e}");
                                    // Put the alternative back in the buffer
                                    self.buffer.push(parent_id, (alt, alt_raw));
                                }
                            }
                        }
                    }
                    buffered += 1;
                    if let Some((_, (evicted, _))) = self.buffer.push(parent_id, (header, raw)) {
                        evicted_ids.push(evicted.id.0.0);
                    }
                }
                Err(ChainError::InvalidGenesisParent { .. })
                | Err(ChainError::InvalidGenesisHeight { .. }) => {
                    buffered += 1;
                    if let Some((_, (evicted, _))) = self.buffer.push(parent_id, (header, raw)) {
                        evicted_ids.push(evicted.id.0.0);
                    }
                }
                Err(_) => {
                    rejected += 1;
                }
            }
        }

        let height_after = chain.height();
        drop(chain);

        // Purge buffer entries at or below the chain tip (stale duplicates)
        let before_purge = self.buffer.len();
        let stale_keys: Vec<BlockId> = self.buffer.iter()
            .filter(|(_, (h, _))| h.height <= height_after)
            .map(|(k, _)| *k)
            .collect();
        for key in &stale_keys {
            self.buffer.pop(key);
        }
        let purged = before_purge - self.buffer.len();

        // Persist chained headers to disk
        if !store_entries.is_empty() {
            if let Err(e) = self.store.put_batch(&store_entries) {
                tracing::error!(count = store_entries.len(), "store write failed: {e}");
            }
        }

        // Notify delivery tracker of evicted modifier IDs for re-request
        if !evicted_ids.is_empty() {
            tracing::debug!(count = evicted_ids.len(), "buffer evictions → re-request");
            if self.delivery_tx.try_send(DeliveryEvent::Evicted(evicted_ids)).is_err() {
                tracing::warn!("delivery channel full, dropped Evicted notification");
            }
        }

        if height_after > height_before {
            tracing::info!(
                chain_height = height_after,
                chained,
                buffer = self.buffer.len(),
                "pipeline: chained headers from height {height_before}"
            );
            if self.progress_tx.try_send(height_after).is_err() {
                tracing::warn!("progress channel full, dropped height {height_after}");
            }
        }

        if buffered > 0 || rejected > 0 || purged > 0 {
            tracing::info!(
                batch_size,
                chained, buffered, rejected, purged,
                chain_tip = height_after,
                "pipeline: batch breakdown"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enr_chain::ChainConfig;

    /// Build a pipeline with a testnet chain, channels, and a temp store.
    fn test_pipeline() -> (
        ValidationPipeline,
        mpsc::Sender<(u8, [u8; 32], Vec<u8>)>,
        mpsc::Receiver<u32>,
        mpsc::Receiver<DeliveryEvent>,
        tempfile::TempDir,
    ) {
        let (tx, rx) = mpsc::channel(256);
        let (progress_tx, progress_rx) = mpsc::channel(4);
        let (delivery_tx, delivery_rx) = mpsc::channel(64);
        let chain = Arc::new(Mutex::new(HeaderChain::new(ChainConfig::testnet())));
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(RedbModifierStore::new(&dir.path().join("test.redb")).unwrap());
        let pipeline = ValidationPipeline::new(rx, chain, store, progress_tx, delivery_tx);
        (pipeline, tx, progress_rx, delivery_rx, dir)
    }

    #[test]
    fn header_scorex_bytes_roundtrip() {
        use sigma_ser::ScorexSerializable;

        let json = r#"{
            "extensionId": "00cce45975d87414e8bdd8146bc88815be59cd9fe37a125b5021101e05675a18",
            "difficulty": "16384",
            "votes": "000000",
            "timestamp": 4928911477310178288,
            "size": 223,
            "stateRoot": "5c8c00b8403d3701557181c8df800001b6d5009e2201c6ff807d71808c00019780",
            "height": 614400,
            "nBits": 37748736,
            "version": 2,
            "id": "5603a937ec1988220fc44fb5022fb82d5565b961f005ebb55d85bd5a9e6f801f",
            "adProofsRoot": "5d3f80dcff7f5e7f59007294c180808d0158d1ff6ba10000f901c7f0ef87dcff",
            "transactionsRoot": "f17fffacb6ff7f7f1180d2ff7f1e24ffffe1ff937f807f0797b9ff6ebdae007e",
            "extensionHash": "1480887f80007f4b01cf7f013ff1ffff564a0000b9a54f00770e807f41ff88c0",
            "powSolutions": {
                "pk": "03bedaee069ff4829500b3c07c4d5fe6b3ea3d3bf76c5c28c1d4dcdb1bed0ade0c",
                "n": "0000000000003105"
            },
            "parentId": "ac2101807f0000ca01ff0119db227f202201007f62000177a080005d440896d0"
        }"#;
        let header: Header = serde_json::from_str(json).unwrap();
        let bytes1 = header.scorex_serialize_bytes().unwrap();
        let header2 = Header::scorex_parse_bytes(&bytes1).unwrap();
        let bytes2 = header2.scorex_serialize_bytes().unwrap();
        assert_eq!(bytes1, bytes2, "scorex serialize round-trip must be byte-identical");
    }

    #[test]
    fn rejects_unparseable_header() {
        let (mut pipeline, _tx, _progress_rx, _delivery_rx, _dir) = test_pipeline();
        let batch = vec![(HEADER_TYPE_ID, [0xaa; 32], vec![0xff, 0x00, 0x01])];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "bad header should not be chained");
    }

    #[test]
    fn ignores_non_header_modifier_types() {
        let (mut pipeline, _tx, _progress_rx, _delivery_rx, _dir) = test_pipeline();
        let batch = vec![(102, [0xaa; 32], vec![0xff; 100])];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "non-header types should be skipped");
    }

    #[test]
    fn accepts_valid_pow_header_into_pending() {
        use sigma_ser::ScorexSerializable;

        let json = r#"{
            "extensionId": "00cce45975d87414e8bdd8146bc88815be59cd9fe37a125b5021101e05675a18",
            "difficulty": "16384",
            "votes": "000000",
            "timestamp": 4928911477310178288,
            "size": 223,
            "stateRoot": "5c8c00b8403d3701557181c8df800001b6d5009e2201c6ff807d71808c00019780",
            "height": 614400,
            "nBits": 37748736,
            "version": 2,
            "id": "5603a937ec1988220fc44fb5022fb82d5565b961f005ebb55d85bd5a9e6f801f",
            "adProofsRoot": "5d3f80dcff7f5e7f59007294c180808d0158d1ff6ba10000f901c7f0ef87dcff",
            "transactionsRoot": "f17fffacb6ff7f7f1180d2ff7f1e24ffffe1ff937f807f0797b9ff6ebdae007e",
            "extensionHash": "1480887f80007f4b01cf7f013ff1ffff564a0000b9a54f00770e807f41ff88c0",
            "powSolutions": {
                "pk": "03bedaee069ff4829500b3c07c4d5fe6b3ea3d3bf76c5c28c1d4dcdb1bed0ade0c",
                "n": "0000000000003105"
            },
            "parentId": "ac2101807f0000ca01ff0119db227f202201007f62000177a080005d440896d0"
        }"#;
        let header: Header = serde_json::from_str(json).unwrap();
        let bytes = header.scorex_serialize_bytes().unwrap();

        let (mut pipeline, _tx, _progress_rx, _delivery_rx, _dir) = test_pipeline();
        let batch = vec![(HEADER_TYPE_ID, [0xaa; 32], bytes)];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));

        // Header has valid PoW but parent is missing, so it goes to the LRU buffer
        assert_eq!(pipeline.buffer.len(), 1, "valid PoW header should be buffered");
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "unchainable header should not increase height");
    }

    /// Test vector from JVM ergo-core HeaderSerializationSpecification.
    /// Mainnet block 418,138 (version 2). Verifies byte-level compatibility
    /// with the JVM serializer — same bytes, same Blake2b256 header ID.
    #[test]
    fn jvm_header_v2_test_vector() {
        use sigma_ser::ScorexSerializable;

        // Real mainnet header, from ergo-core test suite
        let json = r#"{
            "extensionId": "0000000000000000000000000000000000000000000000000000000000000000",
            "difficulty": "107976917",
            "votes": "000000",
            "timestamp": 1612465607426,
            "size": 0,
            "stateRoot": "995c0efe63744c5227e6ae213a2061c60f8db845d47707a6bff53f9ff1936a9e13",
            "height": 418138,
            "nBits": 107976917,
            "version": 2,
            "id": "f46c89e44f13a92d8409341490f97f05c85785fa8d2d2164332cc066eda95c39",
            "adProofsRoot": "a80bbd4d69b4f017da6dd9250448ef1cde492121fc350727e755c7b7ae2988ad",
            "transactionsRoot": "141bf3de015c44995858a435e4d6c50c51622d077760de32977ba5412aaaae03",
            "extensionHash": "b1457df896bba9dc962f8e42187e1ac580842f1282c8c7fb9cf9f4cd520d1c07",
            "powSolutions": {
                "pk": "0315345f1fca9445eee5df74759d4c495094bcfc82a2831b26fca6efa599b509de",
                "n": "1b95db2168f95fda"
            },
            "parentId": "7fbc70ec5913706ddef67bbcdb7700ea5f15dc709012491269c9c7eb545d720c"
        }"#;
        let header: Header = serde_json::from_str(json).unwrap();
        let bytes = header.scorex_serialize_bytes().unwrap();

        // Verify header ID matches JVM (Blake2b256 of serialized bytes)
        let id_hex = format!("{}", header.id);
        assert_eq!(
            id_hex,
            "f46c89e44f13a92d8409341490f97f05c85785fa8d2d2164332cc066eda95c39",
            "header ID must match JVM test vector"
        );

        // Verify round-trip
        let header2 = Header::scorex_parse_bytes(&bytes).unwrap();
        let bytes2 = header2.scorex_serialize_bytes().unwrap();
        assert_eq!(bytes, bytes2, "round-trip must be byte-identical");
    }
}

use std::num::NonZeroUsize;
use std::sync::Arc;

use enr_chain::{
    AppendResult, BlockId, ChainError, Header, HeaderChain, HeaderTracker,
    decode_compact_bits, HEADER_TYPE_ID, TRANSACTION_TYPE_ID,
};
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_sync::delivery::{DeliveryControl, DeliveryData};
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
    rx: mpsc::Receiver<ergo_api::ModifierBatchItem>,
    chain: Arc<Mutex<HeaderChain>>,
    store: Arc<RedbModifierStore>,
    progress_tx: mpsc::Sender<u32>,
    delivery_control_tx: mpsc::UnboundedSender<DeliveryControl>,
    delivery_data_tx: mpsc::Sender<DeliveryData>,
    tracker: HeaderTracker,
    buffer: LruCache<BlockId, (Header, Vec<u8>)>,
    /// Parent IDs we've already requested for fork resolution (avoid flooding).
    reorg_requested: std::collections::HashSet<[u8; 32]>,
    /// Channel for forwarding unconfirmed transactions to the mempool task.
    tx_sender: Option<mpsc::Sender<([u8; 32], Vec<u8>)>>,
}

impl ValidationPipeline {
    pub fn new(
        rx: mpsc::Receiver<ergo_api::ModifierBatchItem>,
        chain: Arc<Mutex<HeaderChain>>,
        store: Arc<RedbModifierStore>,
        progress_tx: mpsc::Sender<u32>,
        delivery_control_tx: mpsc::UnboundedSender<DeliveryControl>,
        delivery_data_tx: mpsc::Sender<DeliveryData>,
    ) -> Self {
        Self {
            rx,
            chain,
            store,
            progress_tx,
            delivery_control_tx,
            delivery_data_tx,
            tracker: HeaderTracker::new(),
            buffer: LruCache::new(NonZeroUsize::new(BUFFER_CAPACITY).unwrap()),
            reorg_requested: std::collections::HashSet::new(),
            tx_sender: None,
        }
    }

    /// Set the channel for forwarding unconfirmed transactions to the mempool.
    pub fn set_tx_sender(&mut self, sender: mpsc::Sender<([u8; 32], Vec<u8>)>) {
        self.tx_sender = Some(sender);
    }

    /// Walk backward from a fork tip through the store to find the fork point
    /// (first ancestor that's in the best chain). Returns (fork_point_height, branch)
    /// where branch is `(header, raw_bytes)` pairs in ascending order. The raw bytes
    /// are needed so the caller can re-emit the new branch via `put_batch` after a
    /// successful reorg, updating BEST_CHAIN to reflect the new chain.
    fn assemble_fork_branch(
        &self,
        chain: &HeaderChain,
        _tip_id: BlockId,
        tip_header: &Header,
        tip_raw: Vec<u8>,
    ) -> Option<(u32, Vec<(Header, Vec<u8>)>)> {
        let mut branch: Vec<(Header, Vec<u8>)> = vec![(tip_header.clone(), tip_raw)];
        let mut current_parent = tip_header.parent_id;

        // Walk backward, max 1000 steps to prevent runaway
        for _ in 0..1000 {
            // Is this parent in the best chain?
            if chain.contains(&current_parent) {
                // Found the fork point
                // The branch is in reverse order (tip first). The last entry is
                // closest to the fork point — its height - 1 is the fork point.
                let (first_fork_header, _) = branch.last().unwrap();
                let fork_point_height = first_fork_header.height - 1;

                // Verify the fork point is actually in the chain at that height
                if chain.header_at(fork_point_height).map(|h| h.id) == Some(current_parent) {
                    branch.reverse(); // ascending order
                    return Some((fork_point_height, branch));
                } else {
                    tracing::warn!(
                        fork_point_height,
                        parent = %current_parent,
                        "fork point parent ID doesn't match chain at expected height"
                    );
                    return None;
                }
            }

            // Parent is another fork header — read it from the store
            let parent_data = match self.store.get(HEADER_TYPE_ID, &current_parent.0.0) {
                Ok(Some(data)) => data,
                _ => {
                    tracing::debug!(parent = %current_parent, "fork chain broken — parent not in store");
                    return None;
                }
            };
            let parent_header = match enr_chain::parse_header(&parent_data) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(parent = %current_parent, "fork parent parse failed: {e}");
                    return None;
                }
            };

            current_parent = parent_header.parent_id;
            branch.push((parent_header, parent_data));
        }

        tracing::warn!("fork chain walk exceeded 1000 steps");
        None
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
    pub(crate) async fn process_batch(&mut self, batch: Vec<ergo_api::ModifierBatchItem>) {
        // Single pass: collect IDs for delivery tracker, partition headers from sections
        let mut received_ids = Vec::with_capacity(batch.len());
        let mut raw_headers: Vec<(&[u8], Option<u64>)> = Vec::new();
        let mut section_entries: Vec<(u8, [u8; 32], u32, Vec<u8>)> = Vec::new();

        {
        let chain_guard = self.chain.lock().await;
        for (type_id, id, data, peer_id) in &batch {
            received_ids.push(*id);
            if *type_id == HEADER_TYPE_ID {
                raw_headers.push((data.as_slice(), *peer_id));
            } else if *type_id == TRANSACTION_TYPE_ID && !data.is_empty() {
                // Unconfirmed transaction — forward to mempool
                if let Some(ref tx_sender) = self.tx_sender {
                    let _ = tx_sender.try_send((*id, data.clone()));
                }
            } else if !data.is_empty() {
                // Block sections (102=BlockTransactions, 104=ADProofs, 108=Extension)
                // have the header ID in the first 32 bytes. Look up the header to
                // derive the height so the store can index by (type_id, height).
                let height = if data.len() >= 32 {
                    let header_id: [u8; 32] = data[..32].try_into().unwrap();
                    let block_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::from(header_id));
                    chain_guard.height_of(&block_id).unwrap_or(0)
                } else {
                    0
                };
                section_entries.push((*type_id, *id, height, data.clone()));
            }
        }
        } // drop chain_guard

        // Notify delivery tracker (data plane — ok to drop)
        if !received_ids.is_empty()
            && self.delivery_data_tx.try_send(DeliveryData::Received(received_ids)).is_err() {
                tracing::debug!("delivery data channel full, dropped Received notification");
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
        for (data, peer_id) in raw_headers {
            let header = match enr_chain::parse_header(data) {
                Ok(h) => h,
                Err(e) => {
                    if let Some(pid) = peer_id {
                        tracing::warn!(peer_id = pid, "PENALTY header parse failed: {e}");
                    } else {
                        tracing::debug!("pipeline: rejecting header: parse failed: {e}");
                    }
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
                if let Some(pid) = peer_id {
                    tracing::warn!(peer_id = pid, height = header.height, "PENALTY invalid PoW: {e}");
                } else {
                    tracing::debug!(
                        "pipeline: rejecting header at height {}: {e}",
                        header.height
                    );
                }
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
        // Pending deep reorg: (fork_point_height, [(header, raw_bytes)] in ascending order)
        let mut pending_reorg: Option<(u32, Vec<(Header, Vec<u8>)>)> = None;

        for (header, raw) in valid_headers {
            // Skip headers already in the chain (duplicates from overlapping peer responses)
            if chain.contains(&header.id) {
                rejected += 1;
                continue;
            }
            self.tracker.observe(&header);
            let header_id = header.id;
            let header_height = header.height;
            let parent_id = header.parent_id;
            match chain.try_append(header.clone()) {
                Ok(AppendResult::Extended) => {
                    chained += 1;
                    store_entries.push((HEADER_TYPE_ID, header_id.0.0, header_height, raw));
                    if header_height % 400 < 2 {
                        tracing::debug!(height = header_height, id = %header_id, "chained header ID");
                    }
                    // Drain buffer: follow the chain of buffered children
                    let mut next_parent = header_id;
                    while let Some((buf, buf_raw)) = self.buffer.pop(&next_parent) {
                        let bid = buf.id;
                        let buf_height = buf.height;
                        match chain.try_append(buf.clone()) {
                            Ok(AppendResult::Extended) => {
                                chained += 1;
                                store_entries.push((HEADER_TYPE_ID, bid.0.0, buf_height, buf_raw));
                                self.tracker.observe(&buf);
                                next_parent = bid;
                            }
                            _ => break,
                        }
                    }
                }
                Ok(AppendResult::Forked { fork_height }) => {
                    // Header is valid but forks from the best chain.
                    // Compute its cumulative score and store immediately
                    // (later headers in this batch may extend this fork).
                    let parent_score = chain.score_at(fork_height)
                        .unwrap_or_default();
                    let difficulty = decode_compact_bits(header.n_bits)
                        .to_biguint()
                        .unwrap_or_default();
                    let fork_score = &parent_score + &difficulty;

                    // Determine fork number at this height
                    let fork_num = self.store.header_ids_at_height(header_height)
                        .map(|forks| forks.last().map(|(_, f)| f + 1).unwrap_or(1))
                        .unwrap_or(1);

                    let score_bytes = fork_score.to_bytes_be();
                    if let Err(e) = self.store.put_header(
                        &header_id.0.0, header_height, fork_num,
                        &score_bytes, &raw,
                    ) {
                        tracing::error!(height = header_height, "store fork header failed: {e}");
                    }

                    let best_score = chain.cumulative_score();
                    tracing::info!(
                        height = header_height,
                        fork_height,
                        fork_better = fork_score > best_score,
                        "fork detected (parent in best chain)"
                    );

                    if fork_score > best_score && pending_reorg.is_none() {
                        pending_reorg = Some((fork_height, vec![(header, raw)]));
                    }
                }
                Err(ChainError::ParentNotFound { .. }) => {
                    // Parent not in best chain. Check if it's a known fork header
                    // in the store — if so, this extends that fork chain.
                    let parent_score_opt = self.store.header_score(&parent_id.0.0)
                        .ok()
                        .flatten()
                        .filter(|s| !s.is_empty());

                    if let Some(parent_score_bytes) = parent_score_opt {
                        // Parent is a stored fork header. Extend the fork chain.
                        use enr_chain::BigUint;
                        let parent_score = BigUint::from_bytes_be(&parent_score_bytes);
                        let difficulty = decode_compact_bits(header.n_bits)
                            .to_biguint()
                            .unwrap_or_default();
                        let fork_score = &parent_score + &difficulty;

                        let fork_num = self.store.header_ids_at_height(header_height)
                            .map(|forks| forks.last().map(|(_, f)| f + 1).unwrap_or(1))
                            .unwrap_or(1);

                        let score_bytes = fork_score.to_bytes_be();
                        if let Err(e) = self.store.put_header(
                            &header_id.0.0, header_height, fork_num,
                            &score_bytes, &raw,
                        ) {
                            tracing::error!(height = header_height, "store fork chain header failed: {e}");
                        }

                        let best_score = chain.cumulative_score();
                        if fork_score > best_score && pending_reorg.is_none() {
                            // Fork chain is better. Assemble the branch by walking
                            // parent_id backward through the store to the fork point.
                            tracing::info!(
                                height = header_height,
                                "fork chain surpasses best chain — assembling reorg branch"
                            );

                            match self.assemble_fork_branch(&chain, header_id, &header, raw) {
                                Some((fork_point, branch)) => {
                                    pending_reorg = Some((fork_point, branch));
                                }
                                None => {
                                    tracing::warn!(
                                        height = header_height,
                                        "fork chain better but couldn't assemble branch"
                                    );
                                }
                            }
                        } else {
                            tracing::debug!(
                                height = header_height,
                                "fork chain extended but not yet better"
                            );
                        }
                    } else {
                        // Truly unknown parent — buffer it and request the parent
                        // so we can build the fork chain back to a known block.
                        tracing::debug!(
                            header_height,
                            chain_height = chain.height(),
                            id = %header_id,
                            parent = %parent_id,
                            "ParentNotFound"
                        );

                        // Request the missing parent — but only the FIRST one per
                        // batch to avoid flooding. The fork chain links backward,
                        // so fetching the lowest missing parent is sufficient: once
                        // it arrives and chains, the rest drain from the buffer.
                        if (self.reorg_requested.is_empty() || self.reorg_requested.len() < 3)
                            && self.reorg_requested.insert(parent_id.0.0) {
                                let _ = self.delivery_control_tx.send(
                                    DeliveryControl::NeedModifier {
                                        type_id: HEADER_TYPE_ID,
                                        id: parent_id.0.0,
                                    },
                                );
                            }

                        buffered += 1;
                        if let Some((_, (evicted, _))) = self.buffer.push(parent_id, (header, raw)) {
                            evicted_ids.push(evicted.id.0.0);
                        }
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

        // Execute pending deep reorg while still holding the chain lock
        if let Some((fork_point, new_branch_with_raw)) = pending_reorg {
            let old_tip = chain.height();
            let headers_only: Vec<Header> = new_branch_with_raw
                .iter()
                .map(|(h, _)| h.clone())
                .collect();
            match chain.try_reorg_deep(fork_point, headers_only) {
                Ok(demoted_ids) => {
                    let new_tip = chain.height();
                    tracing::info!(
                        fork_point,
                        demoted = demoted_ids.len(),
                        old_tip,
                        new_tip,
                        "deep reorg succeeded"
                    );
                    chained += new_branch_with_raw.len() as u32;

                    // Re-emit the new branch headers via put_batch so BEST_CHAIN
                    // reflects the new chain at fork_point+1..new_tip. The new
                    // branch headers are already in PRIMARY + HEADER_FORKS from
                    // their earlier put_header (fork > 0) writes during fork
                    // detection, but put_header with fork > 0 only inserts into
                    // BEST_CHAIN if the slot is empty — so BEST_CHAIN still
                    // points at the OLD chain in that height range. Without this
                    // step, any consumer reading best_header_at(h) after a
                    // reorg (notably the extension_loader used by voting-epoch
                    // parameter recomputation) would see the demoted chain.
                    // put_batch's unconditional BEST_CHAIN insert for type 101
                    // makes the new branch authoritative.
                    let reorg_entries: Vec<(u8, [u8; 32], u32, Vec<u8>)> = new_branch_with_raw
                        .into_iter()
                        .map(|(h, raw)| (HEADER_TYPE_ID, h.id.0.0, h.height, raw))
                        .collect();
                    let reorg_entry_count = reorg_entries.len();
                    if let Err(e) = self.store.put_batch(&reorg_entries) {
                        tracing::error!(
                            count = reorg_entry_count,
                            "reorg: put_batch failed to update BEST_CHAIN: {e}"
                        );
                    }

                    // Notify sync machine — Reorg MUST NOT be dropped.
                    // A missed reorg leaves the validator with a stale state root,
                    // permanently stalling validation. Unbounded channel = infallible.
                    let _ = self.delivery_control_tx.send(DeliveryControl::Reorg {
                        fork_point,
                        old_tip,
                        new_tip,
                    });
                }
                Err(e) => {
                    tracing::warn!(fork_point, "deep reorg failed: {e}");
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

        // Notify delivery tracker of evicted modifier IDs for re-request (data plane — ok to drop)
        if !evicted_ids.is_empty() {
            tracing::debug!(count = evicted_ids.len(), "buffer evictions → re-request");
            if self.delivery_data_tx.try_send(DeliveryData::Evicted(evicted_ids)).is_err() {
                tracing::debug!("delivery data channel full, dropped Evicted notification");
            }
        }

        if height_after > height_before {
            tracing::debug!(
                chain_height = height_after,
                chained,
                buffer = self.buffer.len(),
                "pipeline: chained headers from height {height_before}"
            );
            if self.progress_tx.try_send(height_after).is_err() {
                tracing::warn!("progress channel full, dropped height {height_after}");
            }
        }

        if chained > 0 || buffered > 0 {
            tracing::debug!(
                batch_size,
                chained, buffered, rejected, purged,
                chain_tip = height_after,
                "pipeline: batch breakdown"
            );
        } else if rejected > 0 || purged > 0 {
            tracing::debug!(
                batch_size,
                rejected, purged,
                chain_tip = height_after,
                "pipeline: batch breakdown (all known)"
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
        mpsc::Sender<ergo_api::ModifierBatchItem>,
        mpsc::Receiver<u32>,
        mpsc::UnboundedReceiver<DeliveryControl>,
        mpsc::Receiver<DeliveryData>,
        tempfile::TempDir,
    ) {
        let (tx, rx) = mpsc::channel(256);
        let (progress_tx, progress_rx) = mpsc::channel(4);
        let (delivery_control_tx, delivery_control_rx) = mpsc::unbounded_channel();
        let (delivery_data_tx, delivery_data_rx) = mpsc::channel(64);
        let chain = Arc::new(Mutex::new(HeaderChain::new(ChainConfig::testnet())));
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(RedbModifierStore::new(&dir.path().join("test.redb")).unwrap());
        let pipeline = ValidationPipeline::new(rx, chain, store, progress_tx, delivery_control_tx, delivery_data_tx);
        (pipeline, tx, progress_rx, delivery_control_rx, delivery_data_rx, dir)
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
        let (mut pipeline, _tx, _progress_rx, _ctrl_rx, _data_rx, _dir) = test_pipeline();
        let batch = vec![(HEADER_TYPE_ID, [0xaa; 32], vec![0xff, 0x00, 0x01], None)];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "bad header should not be chained");
    }

    #[test]
    fn ignores_non_header_modifier_types() {
        let (mut pipeline, _tx, _progress_rx, _ctrl_rx, _data_rx, _dir) = test_pipeline();
        let batch = vec![(102, [0xaa; 32], vec![0xff; 100], None)];
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

        let (mut pipeline, _tx, _progress_rx, _ctrl_rx, _data_rx, _dir) = test_pipeline();
        let batch = vec![(HEADER_TYPE_ID, [0xaa; 32], bytes, None)];
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

use std::collections::HashMap;
use std::sync::Arc;

use enr_chain::{BlockId, ChainError, Header, HeaderChain, HeaderTracker};
use sigma_ser::ScorexSerializable;
use tokio::sync::{mpsc, Mutex};

/// Modifier type ID for headers (NetworkObjectTypeId in JVM source).
const HEADER_TYPE_ID: u8 = 101;

/// Max pending headers before we start dropping old entries.
const MAX_PENDING: usize = 10_000;

/// Async validation pipeline for modifiers.
///
/// Receives raw modifier data from the P2P layer via a channel, validates
/// in batches (sort by height, PoW check, chain-validate), and updates
/// the shared HeaderChain. Runs as a single tokio task.
pub struct ValidationPipeline {
    rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
    chain: Arc<Mutex<HeaderChain>>,
    progress_tx: mpsc::Sender<u32>,
    tracker: HeaderTracker,
    pending: HashMap<BlockId, Header>,
}

impl ValidationPipeline {
    pub fn new(
        rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
        chain: Arc<Mutex<HeaderChain>>,
        progress_tx: mpsc::Sender<u32>,
    ) -> Self {
        Self {
            rx,
            chain,
            progress_tx,
            tracker: HeaderTracker::new(),
            pending: HashMap::new(),
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
        // Filter to headers only
        let raw_headers: Vec<&[u8]> = batch
            .iter()
            .filter(|(t, _, _)| *t == HEADER_TYPE_ID)
            .map(|(_, _, data)| data.as_slice())
            .collect();

        if raw_headers.is_empty() {
            return;
        }

        // Parse, round-trip check, and PoW-verify
        let mut valid_headers: Vec<Header> = Vec::with_capacity(raw_headers.len());
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
            valid_headers.push(header);
        }

        if valid_headers.is_empty() {
            return;
        }

        // Sort by height — within a batch this eliminates most buffering
        valid_headers.sort_by_key(|h| h.height);

        // Lock chain once for the whole batch
        let mut chain = self.chain.lock().await;
        let height_before = chain.height();

        let mut chained = 0u32;
        let mut buffered = 0u32;
        let mut rejected = 0u32;

        for header in &valid_headers {
            // Skip headers already at or below the chain tip (duplicates
            // from overlapping SyncInfo responses)
            if header.height <= chain.height() {
                rejected += 1;
                continue;
            }
            self.tracker.observe(header);
            match chain.try_append(header.clone()) {
                Ok(()) => {
                    chained += 1;
                    // Log IDs at SyncInfo offset heights for diagnostic comparison
                    let h = header.height;
                    if h % 400 < 2 || h == 4789 || h == 4773 || h == 4661 || h == 4277 {
                        tracing::info!(height = h, id = %header.id, "chained header ID");
                    }
                    // Drain pending buffer from this header
                    let mut next_parent = header.id;
                    while let Some(buf) = self.pending.remove(&next_parent) {
                        let bid = buf.id;
                        match chain.try_append(buf.clone()) {
                            Ok(()) => {
                                chained += 1;
                                self.tracker.observe(&buf);
                                next_parent = bid;
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(ChainError::ParentNotFound { .. })
                | Err(ChainError::InvalidGenesisParent { .. })
                | Err(ChainError::InvalidGenesisHeight { .. }) => {
                    buffered += 1;
                    if self.pending.len() < MAX_PENDING {
                        self.pending.insert(header.parent_id, header.clone());
                    }
                }
                Err(_) => {
                    rejected += 1;
                }
            }
        }

        let height_after = chain.height();
        drop(chain);

        // Purge pending entries at or below the chain tip (stale duplicates)
        let before_purge = self.pending.len();
        self.pending.retain(|_, h| h.height > height_after);
        let purged = before_purge - self.pending.len();

        if height_after > height_before {
            tracing::info!(
                chain_height = height_after,
                chained,
                pending = self.pending.len(),
                "pipeline: chained headers from height {height_before}"
            );
            let _ = self.progress_tx.try_send(height_after);
        }

        if buffered > 0 || rejected > 0 || purged > 0 {
            tracing::info!(
                batch_size = valid_headers.len(),
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

    /// Build a pipeline with a testnet chain and a channel.
    fn test_pipeline() -> (
        ValidationPipeline,
        mpsc::Sender<(u8, [u8; 32], Vec<u8>)>,
        mpsc::Receiver<u32>,
    ) {
        let (tx, rx) = mpsc::channel(256);
        let (progress_tx, progress_rx) = mpsc::channel(4);
        let chain = Arc::new(Mutex::new(HeaderChain::new(ChainConfig::testnet())));
        let pipeline = ValidationPipeline::new(rx, chain, progress_tx);
        (pipeline, tx, progress_rx)
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
        let (mut pipeline, _tx, _progress_rx) = test_pipeline();
        let batch = vec![(HEADER_TYPE_ID, [0xaa; 32], vec![0xff, 0x00, 0x01])];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "bad header should not be chained");
    }

    #[test]
    fn ignores_non_header_modifier_types() {
        let (mut pipeline, _tx, _progress_rx) = test_pipeline();
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

        let (mut pipeline, _tx, _progress_rx) = test_pipeline();
        let batch = vec![(HEADER_TYPE_ID, [0xaa; 32], bytes)];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));

        // Header has valid PoW but parent is missing, so it goes to pending
        assert_eq!(pipeline.pending.len(), 1, "valid PoW header should be buffered");
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "unchainable header should not increase height");
    }
}

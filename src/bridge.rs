//! Bridge implementations: connect ergo-sync traits to concrete P2P and chain types.

use std::sync::Arc;

use enr_chain::{BlockId, ChainError, Header, HeaderChain, SyncInfo};
use enr_p2p::node::P2pNode;
use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_sync::{SyncChain, SyncStore, SyncTransport};
use sigma_ser::ScorexSerializable;
use tokio::sync::{mpsc, Mutex};

/// Restore the sparse difficulty anchors required by a light chain. A
/// best-chain index starting above genesis is not usable without the matching
/// versioned context record, so absence and corruption are hard errors.
pub fn restore_nipopow_difficulty_context_from_store(
    chain: &mut HeaderChain,
    store: &RedbModifierStore,
) -> Result<(), ChainError> {
    if !chain.light_client_mode() {
        return Ok(());
    }
    let bytes = store
        .chain_meta_get(enr_chain::NIPOPOW_DIFFICULTY_CONTEXT_META_KEY)
        .map_err(|e| ChainError::Nipopow(format!("read NiPoPoW difficulty context: {e}")))?
        .ok_or_else(|| {
            ChainError::Nipopow(
                "restored light chain is missing persisted NiPoPoW difficulty context".into(),
            )
        })?;
    let context = enr_chain::parse_nipopow_difficulty_context(&bytes)?;
    chain.restore_nipopow_difficulty_context(
        context.suffix_head_height,
        context.suffix_head_id,
        context.headers,
    )
}

/// Wraps `P2pNode` + event receiver to implement `SyncTransport`.
pub struct P2pTransport {
    node: Arc<P2pNode>,
    events: mpsc::Receiver<ProtocolEvent>,
}

impl P2pTransport {
    pub fn new(node: Arc<P2pNode>, events: mpsc::Receiver<ProtocolEvent>) -> Self {
        Self { node, events }
    }
}

impl SyncTransport for P2pTransport {
    async fn send_to(
        &self,
        peer: PeerId,
        message: ProtocolMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        self.node
            .send_to(peer, message)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)
    }

    async fn outbound_peers(&self) -> Vec<PeerId> {
        self.node.outbound_peers().await
    }

    async fn next_event(&mut self) -> Option<ProtocolEvent> {
        self.events.recv().await
    }
}

/// Wraps `Arc<Mutex<HeaderChain>>` to implement `SyncChain`.
pub struct SharedChain {
    chain: Arc<Mutex<HeaderChain>>,
    store: Arc<RedbModifierStore>,
}

impl SharedChain {
    pub fn new(chain: Arc<Mutex<HeaderChain>>, store: Arc<RedbModifierStore>) -> Self {
        Self { chain, store }
    }
}

impl SyncChain for SharedChain {
    async fn chain_height(&self) -> u32 {
        self.chain.lock().await.height()
    }

    async fn build_sync_info(&self) -> Vec<u8> {
        let chain = self.chain.lock().await;
        enr_chain::build_sync_info(&chain)
    }

    fn parse_sync_info(&self, body: &[u8]) -> Result<SyncInfo, ChainError> {
        enr_chain::parse_sync_info(body)
    }

    async fn continuation_ids(&self, peer_last_ids: &[BlockId], limit: usize) -> Vec<[u8; 32]> {
        let chain = self.chain.lock().await;
        // Best common point: the highest of the peer's anchors present in
        // our chain. Empty anchor list = fresh peer → serve from height 1;
        // non-empty anchors with NO common point = JVM `Unknown` status →
        // serve nothing (facts/sync.md § Serving sync). We serve the full
        // `limit` (400) — the JVM's V2 non-empty path quirks at size-1=399,
        // but JVM requesters accept 400-id Invs, so the contract cap wins.
        let anchor = if peer_last_ids.is_empty() {
            0
        } else {
            match peer_last_ids
                .iter()
                .filter_map(|id| chain.height_of(id))
                .max()
            {
                Some(h) => h,
                None => return Vec::new(),
            }
        };
        let tip = chain.height();
        if anchor >= tip {
            return Vec::new();
        }
        (anchor + 1..=tip)
            .take(limit)
            .filter_map(|h| chain.header_at(h).map(|hdr| hdr.id.0 .0))
            .collect()
    }

    async fn header_at(&self, height: u32) -> Option<Header> {
        self.chain.lock().await.header_at(height)
    }

    async fn header_state_root(&self, height: u32) -> Option<[u8; 33]> {
        let chain = self.chain.lock().await;
        let header = chain.header_at(height)?;
        Some(header.state_root.0)
    }

    async fn active_parameters(&self) -> ergo_validation::Parameters {
        self.chain.lock().await.active_parameters().clone()
    }

    async fn is_epoch_boundary(&self, height: u32) -> bool {
        self.chain.lock().await.is_epoch_boundary(height)
    }

    async fn compute_expected_parameters(
        &self,
        epoch_boundary_height: u32,
        block_proposed_update: &[u8],
    ) -> Result<ergo_validation::Parameters, ChainError> {
        self.chain
            .lock()
            .await
            .compute_expected_parameters(epoch_boundary_height, block_proposed_update)
    }

    async fn apply_epoch_boundary_parameters(
        &self,
        params: ergo_validation::Parameters,
        proposed_update_bytes: Vec<u8>,
    ) {
        self.chain
            .lock()
            .await
            .apply_epoch_boundary_parameters(params, proposed_update_bytes);
    }

    async fn active_proposed_update_bytes(&self) -> Vec<u8> {
        self.chain.lock().await.active_proposed_update_bytes().to_vec()
    }

    async fn verify_nipopow_envelope(
        &self,
        envelope_body: &[u8],
    ) -> Result<enr_chain::NipopowVerificationResult, ChainError> {
        // Strip the P2P code-91 envelope (length-prefixed inner bytes + future
        // pad). The wire codec lives in the main crate's `nipopow_serve` so
        // sync stays codec-free.
        let inner = crate::nipopow_serve::parse_nipopow_proof(envelope_body)
            .map_err(|e| ChainError::Nipopow(format!("envelope parse: {e}")))?;
        let context = {
            let chain = self.chain.lock().await;
            enr_chain::NipopowVerificationContext::from_chain(&chain, 6, 10)?
        };
        enr_chain::verify_nipopow_proof_bytes(&inner, &context)
    }

    async fn is_better_nipopow(
        &self,
        this_envelope: &[u8],
        than_envelope: &[u8],
    ) -> Result<bool, ChainError> {
        let inner_a = crate::nipopow_serve::parse_nipopow_proof(this_envelope)
            .map_err(|e| ChainError::Nipopow(format!("envelope parse (a): {e}")))?;
        let inner_b = crate::nipopow_serve::parse_nipopow_proof(than_envelope)
            .map_err(|e| ChainError::Nipopow(format!("envelope parse (b): {e}")))?;
        enr_chain::compare_nipopow_proof_bytes(&inner_a, &inner_b)
    }

    async fn install_nipopow_suffix(
        &self,
        difficulty_headers: Vec<Header>,
        suffix_head: Header,
        suffix_tail: Vec<Header>,
    ) -> Result<(), ChainError> {
        let all_headers: Vec<Header> = std::iter::once(suffix_head.clone())
            .chain(suffix_tail.iter().cloned())
            .collect();
        let raw_headers = all_headers
            .iter()
            .map(|header| {
                header
                    .scorex_serialize_bytes()
                    .map_err(|e| ChainError::Nipopow(format!("serialize installed header: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let difficulty_context_bytes =
            enr_chain::serialize_nipopow_difficulty_context(&suffix_head, &difficulty_headers)?;
        let installed = self.chain.lock().await.install_from_nipopow_proof(
            difficulty_headers,
            suffix_head,
            suffix_tail,
        )?;

        // Persist each installed header with its real cumulative score so
        // the store's ScoreLoader can serve subsequent chain queries.
        // install_from_nipopow_proof returns InstalledHeader entries in
        // the same order as `all_headers`.
        for ((header, raw), ih) in all_headers
            .iter()
            .zip(raw_headers.iter())
            .zip(installed.iter())
        {
            self.store
                .put_header(&ih.id.0 .0, ih.height, 0, &ih.score_be, raw)
                .map_err(|e| ChainError::Nipopow(format!("persist installed header: {e}")))?;
            debug_assert_eq!(header.id, ih.id);
        }
        // This record is deliberately committed after every BEST_CHAIN write.
        // A crash before it lands makes the next startup refuse light mode
        // instead of continuing without authenticated difficulty history.
        self.store
            .chain_meta_put(
                enr_chain::NIPOPOW_DIFFICULTY_CONTEXT_META_KEY,
                &difficulty_context_bytes,
            )
            .map_err(|e| ChainError::Nipopow(format!("persist NiPoPoW difficulty context: {e}")))?;
        self.store
            .flush()
            .map_err(|e| ChainError::Nipopow(format!("flush NiPoPoW bootstrap state: {e}")))?;
        Ok(())
    }

    async fn voting_length(&self) -> u32 {
        self.chain.lock().await.voting_length()
    }
}

/// Wraps `Arc<RedbModifierStore>` to implement `SyncStore`.
pub struct SharedStore {
    store: Arc<RedbModifierStore>,
}

impl SharedStore {
    pub fn new(store: Arc<RedbModifierStore>) -> Self {
        Self { store }
    }
}

/// Reserved type_id for sync metadata (not a real modifier type).
const SYNC_META_TYPE_ID: u8 = 255;
/// Fixed key for script_verified_height metadata.
const SCRIPT_VERIFIED_HEIGHT_KEY: [u8; 32] = {
    let mut k = [0u8; 32];
    k[0] = b's'; k[1] = b'v'; k[2] = b'h'; // "svh" prefix
    k
};

impl SyncStore for SharedStore {
    async fn has_modifier(&self, type_id: u8, id: &[u8; 32]) -> bool {
        let store = self.store.clone();
        let id = *id;
        match tokio::task::spawn_blocking(move || match store.contains(type_id, &id) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("store.contains failed: {e}");
                false
            }
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(?e, type_id, id = %hex::encode(id), "spawn_blocking panicked: has_modifier");
                false
            }
        }
    }

    async fn get_modifier(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>> {
        let store = self.store.clone();
        let id = *id;
        match tokio::task::spawn_blocking(move || match store.get(type_id, &id) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("store.get failed: {e}");
                None
            }
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(?e, type_id, id = %hex::encode(id), "spawn_blocking panicked: get_modifier");
                None
            }
        }
    }

    async fn script_verified_height(&self) -> Option<u32> {
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            match store.get(SYNC_META_TYPE_ID, &SCRIPT_VERIFIED_HEIGHT_KEY) {
                Ok(Some(bytes)) if bytes.len() == 4 => {
                    Some(u32::from_le_bytes(bytes[..4].try_into().unwrap()))
                }
                _ => None,
            }
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(?e, "spawn_blocking panicked: script_verified_height");
                None
            }
        }
    }

    async fn set_script_verified_height(&self, height: u32) {
        let store = self.store.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.put(
                SYNC_META_TYPE_ID,
                &SCRIPT_VERIFIED_HEIGHT_KEY,
                0, // metadata, no meaningful height
                &height.to_le_bytes(),
            ) {
                tracing::warn!(height, "failed to persist script_verified_height: {e}");
            }
        })
        .await
        {
            tracing::error!(?e, height, "spawn_blocking panicked: set_script_verified_height");
        }
    }

    async fn flush(&self) {
        let store = self.store.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.flush() {
                tracing::warn!("modifier store flush failed: {e}");
            }
        })
        .await
        {
            tracing::error!(?e, "spawn_blocking panicked: flush");
        }
    }

    async fn validated_height(&self) -> Option<u32> {
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            match store.chain_meta_get(b"validated_height") {
                Ok(Some(bytes)) if bytes.len() == 4 => {
                    Some(u32::from_be_bytes(bytes[..4].try_into().unwrap()))
                }
                _ => None,
            }
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(?e, "spawn_blocking panicked: validated_height");
                None
            }
        }
    }

    async fn set_validated_height(&self, height: u32) {
        let store = self.store.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            if let Err(e) =
                store.chain_meta_put(b"validated_height", &height.to_be_bytes())
            {
                tracing::warn!(height, "failed to persist validated_height: {e}");
                return;
            }
            if let Err(e) = store.flush() {
                tracing::warn!(
                    height,
                    "failed to flush after persisting validated_height: {e}"
                );
            }
        })
        .await
        {
            tracing::error!(?e, height, "spawn_blocking panicked: set_validated_height");
        }
    }

    async fn prune_below_height(
        &self,
        horizon: u32,
        type_ids: &[u8],
    ) -> Result<usize, String> {
        let store = self.store.clone();
        let type_ids = type_ids.to_vec();
        match tokio::task::spawn_blocking(move || {
            store
                .prune_below_height(horizon, &type_ids)
                .map_err(|e| e.to_string())
        })
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(?e, horizon, "spawn_blocking panicked: prune_below_height");
                Err(format!("spawn_blocking panicked: {e}"))
            }
        }
    }

    async fn min_height_present(&self, type_id: u8) -> Result<Option<u32>, String> {
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            store
                .min_height_present(type_id)
                .map_err(|e| e.to_string())
        })
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(?e, type_id, "spawn_blocking panicked: min_height_present");
                Err(format!("spawn_blocking panicked: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enr_chain::ChainConfig;
    use ergo_chain_types::{ADDigest, AutolykosSolution, Digest32, EcPoint, Votes};
    use sigma_ser::ScorexSerializable;

    fn synthetic_header(height: u32, parent_id: BlockId, n_bits: u32) -> Header {
        let zero32 = Digest32::zero();
        let header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp: 1_000_000 + u64::from(height) * 50_000,
            n_bits,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: height.to_be_bytes().repeat(2),
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        Header::scorex_parse_bytes(
            &header
                .scorex_serialize_bytes()
                .expect("serialize synthetic header"),
        )
        .expect("parse canonical synthetic header")
    }

    #[test]
    fn restored_light_chain_fails_closed_when_difficulty_context_is_absent() {
        let dir = tempfile::tempdir().expect("temporary store directory");
        let store = RedbModifierStore::new(&dir.path().join("modifiers.redb"))
            .expect("open modifier store");
        let suffix_head_id = BlockId(Digest32::from([0x37; 32]));
        let mut chain = HeaderChain::restore(ChainConfig::testnet(), [(375, suffix_head_id)])
            .expect("restore light-chain index");

        let err = restore_nipopow_difficulty_context_from_store(&mut chain, &store)
            .expect_err("light mode without persisted anchors must fail closed");
        assert!(matches!(err, ChainError::Nipopow(ref msg) if msg.contains("missing")));
        assert!(chain.nipopow_difficulty_headers().is_empty());
    }

    #[test]
    fn restored_light_chain_fails_closed_when_difficulty_context_is_corrupt() {
        let dir = tempfile::tempdir().expect("temporary store directory");
        let store = RedbModifierStore::new(&dir.path().join("modifiers.redb"))
            .expect("open modifier store");
        store
            .chain_meta_put(enr_chain::NIPOPOW_DIFFICULTY_CONTEXT_META_KEY, b"corrupt")
            .expect("write corrupt context fixture");
        let suffix_head_id = BlockId(Digest32::from([0x37; 32]));
        let mut chain = HeaderChain::restore(ChainConfig::testnet(), [(375, suffix_head_id)])
            .expect("restore light-chain index");

        let err = restore_nipopow_difficulty_context_from_store(&mut chain, &store)
            .expect_err("corrupt persisted anchors must fail closed");
        assert!(matches!(err, ChainError::Nipopow(_)));
        assert!(chain.nipopow_difficulty_headers().is_empty());
    }

    #[test]
    fn restored_full_chain_does_not_require_nipopow_context() {
        let dir = tempfile::tempdir().expect("temporary store directory");
        let store = RedbModifierStore::new(&dir.path().join("modifiers.redb"))
            .expect("open modifier store");
        store
            .chain_meta_put(
                enr_chain::NIPOPOW_DIFFICULTY_CONTEXT_META_KEY,
                b"corrupt stale context",
            )
            .expect("write stale context fixture");
        let genesis_id = BlockId(Digest32::from([0x01; 32]));
        let mut chain = HeaderChain::restore(ChainConfig::testnet(), [(1, genesis_id)])
            .expect("restore full-chain index");

        restore_nipopow_difficulty_context_from_store(&mut chain, &store)
            .expect("full chain has no sparse-context dependency");
        assert!(!chain.light_client_mode());
    }

    #[tokio::test]
    async fn continuous_install_persists_and_restores_bound_difficulty_context() {
        let dir = tempfile::tempdir().expect("temporary store directory");
        let store = Arc::new(
            RedbModifierStore::new(&dir.path().join("modifiers.redb"))
                .expect("open modifier store"),
        );
        let config = ChainConfig::testnet();
        let suffix_head = synthetic_header(
            375,
            BlockId(Digest32::from([0x75; 32])),
            config.initial_n_bits,
        );
        let difficulty_headers = vec![
            synthetic_header(
                128,
                BlockId(Digest32::from([0x80; 32])),
                config.initial_n_bits,
            ),
            synthetic_header(
                256,
                BlockId(Digest32::from([0x81; 32])),
                config.initial_n_bits,
            ),
        ];
        let live_chain = Arc::new(Mutex::new(HeaderChain::new(config.clone())));
        let shared = SharedChain::new(live_chain, store.clone());

        shared
            .install_nipopow_suffix(difficulty_headers.clone(), suffix_head.clone(), Vec::new())
            .await
            .expect("install and persist continuous proof state");

        let raw_context = store
            .chain_meta_get(enr_chain::NIPOPOW_DIFFICULTY_CONTEXT_META_KEY)
            .expect("read context metadata")
            .expect("context metadata exists");
        let persisted = enr_chain::parse_nipopow_difficulty_context(&raw_context)
            .expect("parse persisted context");
        assert_eq!(persisted.suffix_head_height, suffix_head.height);
        assert_eq!(persisted.suffix_head_id, suffix_head.id);
        assert_eq!(persisted.headers, difficulty_headers);

        let entries = store
            .best_chain_entries()
            .expect("read persisted best chain")
            .into_iter()
            .map(|(height, id)| (height, BlockId(Digest32::from(id))));
        let mut restored = HeaderChain::restore(config, entries).expect("restore persisted suffix");
        restore_nipopow_difficulty_context_from_store(&mut restored, store.as_ref())
            .expect("restore persisted sparse context");
        assert_eq!(restored.nipopow_difficulty_headers(), difficulty_headers);
    }
}

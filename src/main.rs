#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::sync::Arc;

use bytes::Bytes;
use enr_chain::{ChainConfig, HeaderChain, StateType, HEADER_TYPE_ID};
use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage, SnapshotReader};
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::AVLTree;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use ergo_chain_types::ADDigest;
use ergo_lib::chain::emission::MonetarySettings;
use ergo_lib::chain::genesis;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_chain_types::EcPoint;
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_node_rust::{P2pTransport, SharedChain, SharedStore, ValidationPipeline};
use ergo_sync::{HeaderSync, SyncConfig};
use ergo_validation::{ApplyStateOutcome, BlockValidator, DigestValidator, UtxoValidator, ValidationError};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Testnet no-premine proof strings (UTF-8, stored in R4-R8).
const TESTNET_NO_PREMINE_PROOFS: &[&str] = &[
    "'Chaos reigns': what the papers say about the no-deal Brexit vote",
    "\u{4e60}\u{8fd1}\u{5e73}\u{7684}\u{4e24}\u{4f1a}\u{65f6}\u{95f4}|\u{8fd9}\u{91cc}\u{6709}\u{4efd}\u{4e60}\u{8fd1}\u{5e73}\u{4e24}\u{4f1a}\u{65e5}\u{5386}\u{ff0c}\u{8bf7}\u{67e5}\u{6536}\u{ff01}",
    "\u{0422}\u{0410}\u{0421}\u{0421} \u{0441}\u{043e}\u{043e}\u{0431}\u{0449}\u{0438}\u{043b} \u{043e}\u{0431} \u{043e}\u{0431}\u{043d}\u{0430}\u{0440}\u{0443}\u{0436}\u{0435}\u{043d}\u{0438}\u{0438} \u{043d}\u{0435}\u{0441}\u{043a}\u{043e}\u{043b}\u{044c}\u{043a}\u{0438}\u{0445} \u{043c}\u{0430}\u{0439}\u{043d}\u{0438}\u{043d}\u{0433}\u{043e}\u{0432}\u{044b}\u{0445} \u{0444}\u{0435}\u{0440}\u{043c} \u{043d}\u{0430} \u{0441}\u{0442}\u{043e}\u{043b}\u{0438}\u{0447}\u{043d}\u{044b}\u{0445} \u{0440}\u{044b}\u{043d}\u{043a}\u{0430}\u{0445}",
    "000000000000000000139a3e61bd5721827b51a5309a8bfeca0b8c4b5c060931",
    "0xef1d584d77e74e3c509de625dc17893b22b73d040b5d5302bbf832065f928d03",
];

/// Mainnet no-premine proof strings (UTF-8, stored in R4-R8).
/// Source: JVM mainnet.conf:24-30 (July 2019 headlines + block hashes).
const MAINNET_NO_PREMINE_PROOFS: &[&str] = &[
    "00000000000000000014c2e2e7e33d51ae7e66f6ccb6942c3437127b36c33747",
    "0xd07a97293468d9132c5a2adab2e52a23009e6798608e47b0d2623c7e3e923463",
    "Brexit: both Tory sides play down risk of no-deal after business alarm",
    "\u{8ff0}\u{8bc4}\u{ff1a}\u{5e73}\u{8861}\u{3001}\u{6301}\u{7eed}\u{3001}\u{5305}\u{5bb9}\u{2014}\u{2014}\u{65b0}\u{65f6}\u{4ee3}\u{5e94}\u{5bf9}\u{5168}\u{7403}\u{5316}\u{6311}\u{6218}\u{7684}\u{4e2d}\u{56fd}\u{4e4b}\u{9053}",
    "\u{0414}\u{0438}\u{0432}\u{0438}\u{0434}\u{0435}\u{043d}\u{0434}\u{044b} \u{0427}\u{0422}\u{041f}\u{0417} \u{0432}\u{044b}\u{0440}\u{0430}\u{0441}\u{0442}\u{0443}\u{0442} \u{043d}\u{0430} 33% \u{043d}\u{0430} \u{0430}\u{043a}\u{0446}\u{0438}\u{044e}",
];

/// Founders' public keys (hex-encoded compressed EC points).
/// Shared between mainnet and testnet. Source: JVM application.conf:209-213.
const FOUNDERS_PKS: &[&str] = &[
    "039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647",
    "031fb52cf6e805f80d97cde289f4f757d49accf0c83fb864b27d2cf982c37f9a8b",
    "0352ac2a471339b0d23b3d2c5ce0db0e81c969f77891b9edf0bda7fd39a78184e7",
];

/// Interval between snapshot creation checks.
const SNAPSHOT_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// Interval between mempool cleanup passes.
const MEMPOOL_CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// Mining task poll interval for new tip heights.
const MINING_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// Grace period before shutdown to let in-flight work finish.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Genesis UTXO state root digest (hex, 33 bytes with tree height suffix).
const TESTNET_GENESIS_DIGEST: &str =
    "cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502";
const MAINNET_GENESIS_DIGEST: &str =
    "a5df145d41ab15a01e0cd3ffbab046f0d029e5412293072ad0f5827428589b9302";

/// Construct the 3 genesis UTXO boxes from chain parameters.
///
/// Returns (box_id, sigma_serialized_bytes) for each box.
/// Uses ergo-lib's genesis module — ErgoTree scripts are built from IR,
/// not hardcoded hex.
fn build_genesis_boxes(network: enr_p2p::types::Network) -> Vec<([u8; 32], Vec<u8>)> {
    let settings = MonetarySettings::default();

    let proof_strings = match network {
        enr_p2p::types::Network::Testnet => TESTNET_NO_PREMINE_PROOFS,
        enr_p2p::types::Network::Mainnet => MAINNET_NO_PREMINE_PROOFS,
    };

    let founder_pks: Vec<ProveDlog> = FOUNDERS_PKS
        .iter()
        .map(|hex_str| {
            let bytes = hex::decode(hex_str).expect("invalid founder pk hex");
            let point = EcPoint::sigma_parse_bytes(&bytes).expect("invalid EC point");
            ProveDlog::new(point)
        })
        .collect();

    let (emission, no_premine, founders) = genesis::genesis_boxes(
        &settings,
        &founder_pks,
        2, // 2-of-3 threshold
        proof_strings,
    ).expect("genesis box construction failed");

    [emission, no_premine, founders]
        .into_iter()
        .map(|b| {
            let mut id = [0u8; 32];
            id.copy_from_slice(b.box_id().as_ref());
            let bytes = b.sigma_serialize_bytes().expect("genesis box serialization failed");
            (id, bytes)
        })
        .collect()
}

/// Pick the EIP-27 `ReemissionRules` for the active network. Mainnet uses
/// activation height 777,217 (live since April 2023). Testnet uses
/// 100,000,001 — effectively never, since testnet EIP-27 is deferred
/// indefinitely. Sourced from JVM `mainnet.conf` / `testnet.conf`.
fn reemission_rules_for(
    network: enr_p2p::types::Network,
) -> ergo_mining::emission::ReemissionRules {
    match network {
        enr_p2p::types::Network::Mainnet => ergo_mining::emission::ReemissionRules::mainnet(),
        enr_p2p::types::Network::Testnet => ergo_mining::emission::ReemissionRules::testnet(),
    }
}

/// Pre-computed state proofs for mining candidate generation.
/// Written by the validator after each block, read by the mining task.
#[derive(Clone)]
struct MiningProofData {
    parent: ergo_chain_types::Header,
    ad_proof_bytes: Vec<u8>,
    state_root: ADDigest,
    emission_tx: ergo_validation::Transaction,
    tip_height: u32,
}

type MiningProofCache = Arc<std::sync::Mutex<Option<MiningProofData>>>;

/// Context for mining proof pre-computation inside the validator callback.
struct MiningCtx {
    config: ergo_mining::MinerConfig,
    proof_cache: MiningProofCache,
    snapshot_reader: Arc<SnapshotReader>,
}

fn build_miner_config(
    pk: &ProveDlog,
    mining_cfg: &MiningConfig,
    votes: [u8; 3],
    network: enr_p2p::types::Network,
) -> ergo_mining::MinerConfig {
    ergo_mining::MinerConfig {
        miner_pk: pk.clone(),
        reward_delay: mining_cfg.reward_delay,
        votes,
        candidate_ttl: std::time::Duration::from_secs(mining_cfg.candidate_ttl_secs),
        reemission_rules: reemission_rules_for(network),
    }
}

/// Dispatches to either DigestValidator or UtxoValidator based on config.
/// Tracks validated_height in a shared atomic for the snapshot trigger.
/// Publishes ErgoStateContext and confirmed transactions after each block.
struct Validator {
    inner: ValidatorInner,
    /// Updated after every successful validate_block(). Read by the snapshot
    /// creation trigger to know the actual UTXO state height.
    shared_height: Arc<std::sync::atomic::AtomicU32>,
    /// Published after every successful validate_block(). Read by the mempool
    /// task and REST API for transaction validation.
    shared_state_context: Arc<tokio::sync::RwLock<Option<ergo_validation::ErgoStateContext>>>,
    /// Sends confirmed transactions to the mempool task after each block.
    block_applied_tx: tokio::sync::mpsc::Sender<Vec<ergo_validation::Transaction>>,
    /// Notifies the /info/wait long-poll endpoint when a new block is validated.
    height_watch_tx: tokio::sync::watch::Sender<u32>,
    /// Mining proof pre-computation (None if mining not configured or digest mode).
    mining: Option<MiningCtx>,
}

enum ValidatorInner {
    Digest(DigestValidator),
    Utxo(UtxoValidator),
}

impl Validator {
    fn new(
        inner: ValidatorInner,
        shared_height: Arc<std::sync::atomic::AtomicU32>,
        shared_state_context: Arc<tokio::sync::RwLock<Option<ergo_validation::ErgoStateContext>>>,
        block_applied_tx: tokio::sync::mpsc::Sender<Vec<ergo_validation::Transaction>>,
        height_watch_tx: tokio::sync::watch::Sender<u32>,
        mining: Option<MiningCtx>,
    ) -> Self {
        let h = match &inner {
            ValidatorInner::Digest(v) => v.validated_height(),
            ValidatorInner::Utxo(v) => v.validated_height(),
        };
        shared_height.store(h, std::sync::atomic::Ordering::Relaxed);
        let _ = height_watch_tx.send(h);
        Self { inner, shared_height, shared_state_context, block_applied_tx, height_watch_tx, mining }
    }

    /// Pre-compute mining proofs after a successful block validation.
    fn update_mining_proofs(&self, header: &ergo_chain_types::Header) {
        let mining = match &self.mining {
            Some(m) => m,
            None => return,
        };

        let next_height = header.height + 1;
        let emission_id = match self.emission_box_id() {
            Some(id) => id,
            None => return,
        };

        let box_bytes = match mining.snapshot_reader.lookup_key(&emission_id) {
            Some(b) => b,
            None => {
                tracing::debug!("mining: emission box not found in snapshot reader");
                return;
            }
        };

        let emission_box = match ergo_validation::deserialize_box(&box_bytes) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("mining: failed to deserialize emission box: {e}");
                return;
            }
        };

        let emission_tx = match ergo_mining::emission::build_emission_tx(
            &emission_box,
            next_height,
            &mining.config.miner_pk,
            mining.config.reward_delay,
            &mining.config.reemission_rules,
        ) {
            Ok(tx) => tx,
            Err(e) => {
                tracing::warn!("mining: failed to build emission tx: {e}");
                return;
            }
        };

        let (ad_proof_bytes, state_root) = match self.proofs_for_transactions(&[emission_tx.clone()]) {
            Some(Ok(result)) => result,
            Some(Err(e)) => {
                tracing::warn!("mining: proof computation failed: {e}");
                return;
            }
            None => return, // digest mode
        };

        let mut guard = mining.proof_cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(MiningProofData {
            parent: header.clone(),
            ad_proof_bytes,
            state_root,
            emission_tx,
            tip_height: header.height,
        });
    }
}

impl BlockValidator for Validator {
    fn apply_state(
        &mut self,
        header: &ergo_chain_types::Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[ergo_chain_types::Header],
        active_params: &ergo_validation::Parameters,
        expected_boundary_params: Option<&ergo_validation::Parameters>,
        expected_proposed_update: Option<&[u8]>,
    ) -> Result<ApplyStateOutcome, ValidationError> {
        let result = match &mut self.inner {
            ValidatorInner::Digest(v) => v.apply_state(
                header, block_txs, ad_proofs, extension, preceding_headers,
                active_params, expected_boundary_params, expected_proposed_update,
            ),
            ValidatorInner::Utxo(v) => v.apply_state(
                header, block_txs, ad_proofs, extension, preceding_headers,
                active_params, expected_boundary_params, expected_proposed_update,
            ),
        };
        if result.is_ok() {
            let h = self.validated_height();
            self.shared_height.store(h, std::sync::atomic::Ordering::Relaxed);
            let _ = self.height_watch_tx.send(h);

            // Publish state context for mempool/API transaction validation.
            // Only when we have preceding headers (height > 0).
            if !preceding_headers.is_empty() {
                let ctx = ergo_validation::build_state_context(
                    header,
                    preceding_headers,
                    active_params,
                );
                let ctx_lock = self.shared_state_context.clone();
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        *ctx_lock.write().await = Some(ctx);
                    });
                });
            }

            // Send confirmed transactions to the mempool task for apply_block().
            if let Ok(parsed) = ergo_validation::parse_block_transactions(block_txs) {
                let _ = self.block_applied_tx.try_send(parsed.transactions);
            }

            // Pre-compute mining proofs for the next block.
            self.update_mining_proofs(header);
        }
        result
    }

    fn validated_height(&self) -> u32 {
        match &self.inner {
            ValidatorInner::Digest(v) => v.validated_height(),
            ValidatorInner::Utxo(v) => v.validated_height(),
        }
    }

    fn current_digest(&self) -> &ADDigest {
        match &self.inner {
            ValidatorInner::Digest(v) => v.current_digest(),
            ValidatorInner::Utxo(v) => v.current_digest(),
        }
    }

    fn reset_to(&mut self, height: u32, digest: ADDigest) {
        match &mut self.inner {
            ValidatorInner::Digest(v) => v.reset_to(height, digest),
            ValidatorInner::Utxo(v) => v.reset_to(height, digest),
        };
        let h = self.validated_height();
        self.shared_height.store(h, std::sync::atomic::Ordering::Relaxed);
        let _ = self.height_watch_tx.send(h);
    }

    fn flush(&self) -> Result<(), ValidationError> {
        match &self.inner {
            ValidatorInner::Digest(v) => v.flush(),
            ValidatorInner::Utxo(v) => v.flush(),
        }
    }

    fn proofs_for_transactions(
        &self,
        txs: &[ergo_validation::Transaction],
    ) -> Option<Result<(Vec<u8>, ADDigest), ValidationError>> {
        match &self.inner {
            ValidatorInner::Utxo(v) => v.proofs_for_transactions(txs),
            ValidatorInner::Digest(_) => None,
        }
    }

    fn emission_box_id(&self) -> Option<[u8; 32]> {
        match &self.inner {
            ValidatorInner::Utxo(v) => v.emission_box_id(),
            ValidatorInner::Digest(_) => None,
        }
    }
}

// SAFETY: UtxoValidator contains PersistentBatchAVLProver which uses Rc<RefCell<Node>>
// (not Send). The Validator enum is only used from the sync task — a single logical
// owner with no cross-thread sharing. The Send bound is required by tokio::spawn but
// the actual access pattern is single-threaded.
unsafe impl Send for Validator {}

/// Emit a structured penalty log line for fail2ban and optionally disconnect the peer.
///
/// Format: `PENALTY peer_ip={ip} type={type} reason="{reason}"`
/// Types: permanent (instant ban), misbehavior (accumulates), spam, nondelivery
async fn penalize(
    p2p: &enr_p2p::node::P2pNode,
    peer_id: enr_p2p::types::PeerId,
    penalty_type: &str,
    reason: &str,
    disconnect: bool,
) {
    let ip = match p2p.peer_addr(peer_id).await {
        Some(addr) => addr.ip().to_string(),
        None => "unknown".to_string(),
    };
    tracing::warn!(
        "PENALTY peer_ip={ip} type={penalty_type} reason=\"{reason}\""
    );
    if disconnect {
        p2p.disconnect_peer(peer_id).await;
    }
}

/// Handle an incoming NiPoPoW message (code 90 GetNipopowProof or 91 NipopowProof).
///
/// For code 90: parse the request, lock the chain, build the proof, and send the
/// response via the P2P node. For code 91: parse the inner proof bytes and verify
/// against the chain (logged but not applied — light-client mode is a future session).
///
/// Errors during parsing/building/verification are logged at warn level and dropped.
/// We never send error responses — JVM doesn't expect them.
async fn handle_nipopow_event(
    code: u8,
    body: &[u8],
    peer_id: enr_p2p::types::PeerId,
    chain: &Arc<Mutex<HeaderChain>>,
    p2p: &Arc<enr_p2p::node::P2pNode>,
    shared_validated_height: &Arc<std::sync::atomic::AtomicU32>,
) {
    use ergo_node_rust::nipopow_serve;

    match code {
        nipopow_serve::GET_NIPOPOW_PROOF => {
            let req = match nipopow_serve::parse_get_nipopow_proof(body) {
                Ok(r) => r,
                Err(e) => {
                    penalize(p2p, peer_id, "misbehavior", &format!("GetNipopowProof parse failed: {e}"), false).await;
                    return;
                }
            };

            // Build the proof under chain lock. Build is bounded by chain length;
            // for a 270k-block chain this can be a few hundred ms — acceptable
            // for a single P2P request.
            let proof_bytes = {
                let chain_guard = chain.lock().await;

                // Anchor: explicit header_id from peer, or derive from validated tip.
                // `build_nipopow_proof` walks 1..=anchor_height pulling extensions
                // from the loader. Extensions are only present in the modifier store
                // for blocks the validator has actually processed — heights beyond
                // `validated_height` have headers but no extension bytes. If we let
                // `build_nipopow_proof` default to `chain.height()` (the header tip)
                // it will run off the validated edge and fail mid-walk.
                let anchor = match req.header_id {
                    Some(id) => Some(id),
                    None => {
                        let validated_h = shared_validated_height
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if validated_h == 0 {
                            tracing::warn!(
                                peer = %peer_id,
                                "GetNipopowProof: validator has not advanced yet, cannot serve"
                            );
                            return;
                        }
                        match chain_guard.header_at(validated_h) {
                            Some(h) => Some(h.id),
                            None => {
                                tracing::warn!(
                                    peer = %peer_id,
                                    validated_h,
                                    "GetNipopowProof: header at validated height missing from chain"
                                );
                                return;
                            }
                        }
                    }
                };

                match enr_chain::build_nipopow_proof(
                    &chain_guard,
                    req.m as u32,
                    req.k as u32,
                    anchor,
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(peer = %peer_id, m = req.m, k = req.k, "build_nipopow_proof failed: {e}");
                        return;
                    }
                }
            };

            tracing::debug!(
                peer = %peer_id,
                m = req.m,
                k = req.k,
                proof_size = proof_bytes.len(),
                "serving NiPoPoW proof"
            );

            let resp_body = nipopow_serve::serialize_nipopow_proof(&proof_bytes);
            let msg = enr_p2p::protocol::messages::ProtocolMessage::Unknown {
                code: nipopow_serve::NIPOPOW_PROOF,
                body: resp_body,
            };
            if let Err(e) = p2p.send_to(peer_id, msg).await {
                tracing::warn!(peer = %peer_id, "send NipopowProof response failed: {e}");
            }
        }

        nipopow_serve::NIPOPOW_PROOF => {
            let proof_bytes = match nipopow_serve::parse_nipopow_proof(body) {
                Ok(b) => b,
                Err(e) => {
                    penalize(p2p, peer_id, "misbehavior", &format!("NipopowProof parse failed: {e}"), false).await;
                    return;
                }
            };

            // Verify is a pure function over the proof bytes — no chain access needed.
            match enr_chain::verify_nipopow_proof_bytes(&proof_bytes) {
                Ok(meta) => {
                    tracing::info!(
                        peer = %peer_id,
                        suffix_tip_height = meta.suffix_tip_height,
                        total_headers = meta.total_headers,
                        continuous = meta.continuous,
                        "received and verified NiPoPoW proof (logged only — light-client mode pending)"
                    );
                }
                Err(e) => {
                    penalize(p2p, peer_id, "permanent", &format!("NiPoPoW proof verification failed: {e}"), true).await;
                }
            }
        }

        _ => {
            // is_nipopow_message guarantees code is 90 or 91; this branch is unreachable.
            debug_assert!(false, "handle_nipopow_event called with non-nipopow code {code}");
        }
    }
}

/// UtxoReader for the mempool — looks up boxes from the persistent AVL+ tree.
///
/// Uses SnapshotReader's read-only database handle to traverse the tree
/// without interfering with the validator's prover.
struct MempoolUtxoReader<'a> {
    snapshot_reader: Option<&'a SnapshotReader>,
}

impl<'a> ergo_mempool::types::UtxoReader for MempoolUtxoReader<'a> {
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox> {
        let reader = self.snapshot_reader?;
        let value_bytes = reader.lookup_key(box_id)?;
        ergo_validation::deserialize_box(&value_bytes).ok()
    }
}

/// Adapter: HeaderChain → ChainAccess for the API crate.
///
/// Uses block_in_place + block_on to acquire the async chain Mutex from sync trait methods.
/// This is safe on tokio's multi-threaded runtime (axum) — block_in_place moves the current
/// task off the worker thread so the lock acquisition doesn't deadlock with other tasks.
struct HeaderChainAdapter {
    chain: Arc<Mutex<HeaderChain>>,
}

impl HeaderChainAdapter {
    fn with_chain<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&HeaderChain) -> R,
    {
        tokio::task::block_in_place(|| {
            let chain = tokio::runtime::Handle::current().block_on(self.chain.lock());
            f(&chain)
        })
    }
}

impl ergo_api::ChainAccess for HeaderChainAdapter {
    fn height(&self) -> u32 {
        self.with_chain(|c| c.height())
    }
    fn header_at(&self, height: u32) -> Option<ergo_chain_types::Header> {
        self.with_chain(|c| c.header_at(height))
    }
    fn header_by_id(&self, id: &[u8; 32]) -> Option<ergo_chain_types::Header> {
        let block_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::from(*id));
        self.with_chain(|c| {
            let height = c.height_of(&block_id)?;
            c.header_at(height)
        })
    }
    fn tip(&self) -> Option<ergo_chain_types::Header> {
        self.with_chain(|c| {
            let h = c.height();
            if h == 0 { None } else { c.header_at(h) }
        })
    }
}

/// Adapter: RedbModifierStore → StoreAccess for the API crate.
struct StoreAdapter {
    store: Arc<RedbModifierStore>,
}

impl ergo_api::StoreAccess for StoreAdapter {
    fn get(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>> {
        self.store.get(type_id, id).ok().flatten()
    }

    fn get_at_height(&self, type_id: u8, height: u32) -> Option<Vec<u8>> {
        let modifier_id = self.store.get_id_at(type_id, height).ok().flatten()?;
        self.store.get(type_id, &modifier_id).ok().flatten()
    }
}

/// Adapter: SnapshotReader → UtxoAccess for the API crate.
struct ApiUtxoReader {
    snapshot_reader: Option<SnapshotReader>,
}

impl ergo_api::UtxoAccess for ApiUtxoReader {
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<ergo_validation::ErgoBox> {
        let reader = self.snapshot_reader.as_ref()?;
        let value_bytes = reader.lookup_key(box_id)?;
        ergo_validation::deserialize_box(&value_bytes).ok()
    }
}

/// BlockSubmitter implementation for the mining solution endpoint.
///
/// Stores the mined block sections directly in the modifier store, then
/// injects the header into the validation pipeline so the chain advances.
/// The sync task picks up the new tip, validates the block (sections are
/// already in the store), and the validator's mining callback fires for
/// the next candidate.
struct MinedBlockSubmitter {
    store: Arc<RedbModifierStore>,
    modifier_tx: tokio::sync::mpsc::Sender<ergo_api::ModifierBatchItem>,
}

impl ergo_api::BlockSubmitter for MinedBlockSubmitter {
    fn submit(
        &self,
        header: ergo_chain_types::Header,
        block_txs_bytes: Vec<u8>,
        ad_proofs_bytes: Vec<u8>,
        extension_bytes: Vec<u8>,
    ) -> Result<(), String> {
        use sigma_ser::ScorexSerializable;

        // Serialize the full header (with PoW solution)
        let header_bytes = header
            .scorex_serialize_bytes()
            .map_err(|e| format!("header serialize: {e}"))?;

        // Get section IDs computed from the header
        let mut header_id = [0u8; 32];
        header_id.copy_from_slice(header.id.0.as_ref());

        let section_ids = enr_chain::section_ids(&header);
        let block_txs_id = section_ids[0].1;
        let ad_proofs_id = section_ids[1].1;
        let extension_id = section_ids[2].1;

        // Pre-store all sections in the modifier store so the sync task can
        // find them when the chain advances.
        let entries = vec![
            (enr_chain::BLOCK_TRANSACTIONS_TYPE_ID, block_txs_id, header.height, block_txs_bytes),
            (enr_chain::AD_PROOFS_TYPE_ID, ad_proofs_id, header.height, ad_proofs_bytes),
            (enr_chain::EXTENSION_TYPE_ID, extension_id, header.height, extension_bytes),
        ];
        self.store
            .put_batch(&entries)
            .map_err(|e| format!("section store: {e}"))?;

        tracing::info!(
            height = header.height,
            header_id = %hex::encode(header_id),
            "mined block sections stored, injecting header into pipeline"
        );

        // Inject the header into the validation pipeline. The pipeline
        // validates PoW (passes — we just verified), stores the header,
        // and appends to the chain. The sync task then validates the block
        // by reading the sections we pre-stored.
        self.modifier_tx
            .try_send((HEADER_TYPE_ID, header_id, header_bytes, None))
            .map_err(|e| format!("pipeline injection: {e}"))?;

        Ok(())
    }
}

/// Mining config parsed from `[node.mining]` in ergo.toml.
#[derive(Debug, Deserialize, Default)]
struct MiningConfig {
    /// Miner public key (hex-encoded compressed EC point, 33 bytes).
    /// Empty = mining disabled.
    #[serde(default)]
    miner_pk: String,
    /// Voting preferences: 3 bytes as hex string. "000000" = no votes.
    #[serde(default = "default_votes")]
    votes: String,
    /// Miner reward maturity delay in blocks (default: 720).
    #[serde(default = "default_reward_delay")]
    reward_delay: i32,
    /// Maximum candidate lifetime before forced regeneration (seconds).
    #[serde(default = "default_candidate_ttl")]
    candidate_ttl_secs: u64,
}

fn default_votes() -> String { "000000".to_string() }
fn default_reward_delay() -> i32 { 720 }
fn default_candidate_ttl() -> u64 { 15 }

/// Node-level config parsed from the `[node]` section of ergo.toml.
#[derive(Debug, Deserialize)]
struct NodeConfig {
    #[serde(default = "default_data_dir")]
    data_dir: String,
    #[serde(default = "default_state_type")]
    state_type: String,
    #[serde(default = "default_verify_transactions")]
    verify_transactions: bool,
    #[serde(default = "default_blocks_to_keep")]
    blocks_to_keep: i64,
    /// Re-validate all stored blocks from genesis on startup.
    /// Keeps headers and sections — no re-download. Useful for testing
    /// validation logic changes against the full chain history.
    #[serde(default)]
    revalidate: bool,
    /// ErgoScript validation checkpoint. Blocks at or below this height
    /// skip script evaluation (AD proof verification alone is sufficient).
    /// 0 = validate everything. Overrides the default (tip - 100).
    #[serde(default)]
    checkpoint_height: Option<u32>,
    /// Enable UTXO snapshot bootstrapping — download state from peers
    /// instead of replaying blocks from genesis.
    #[serde(default)]
    utxo_bootstrap: bool,
    /// Minimum peers announcing the same snapshot before downloading.
    #[serde(default = "default_min_snapshot_peers")]
    min_snapshot_peers: u32,
    /// How many UTXO snapshots to keep for serving (0 = disabled).
    #[serde(default)]
    storing_snapshots: u32,
    /// Blocks between snapshot creation points.
    #[serde(default = "default_snapshot_interval")]
    snapshot_interval: u32,
    /// Maximum transactions in the mempool (default: 1000).
    #[serde(default = "default_mempool_capacity")]
    mempool_capacity: usize,
    /// Minimum fee in nanoERG to enter the mempool (default: 1,000,000 = 0.001 ERG).
    #[serde(default = "default_min_fee")]
    min_fee: u64,
    /// REST API bind address (default: 0.0.0.0:9053 testnet, 0.0.0.0:9052 mainnet).
    #[serde(default)]
    api_address: Option<String>,
    /// Auto-spawn `ergo-fastsync` on startup if the binary is in PATH.
    /// Fastsync fetches headers/blocks from JVM peers over HTTP and pushes
    /// them via the ingest endpoint — much faster than P2P for cold starts.
    #[serde(default = "default_fastsync")]
    fastsync: bool,
    /// Override peer URL for fastsync instead of auto-discovering via
    /// /peers/api-urls. Example: "http://213.239.193.208:9053"
    #[serde(default)]
    fastsync_peer: Option<String>,
    /// Minimum gap (peer_chain_tip - downloaded_height) that triggers fastsync.
    /// Below this, boot proceeds directly to P2P sync. Also passed to fastsync
    /// as --handoff-distance so both sides agree on when to hand off.
    #[serde(default = "default_fastsync_threshold_blocks")]
    fastsync_threshold_blocks: u32,
    /// How long to wait at boot for the first peer SyncInfo before deciding.
    /// If no peer reports a tip within this window, fastsync is skipped.
    #[serde(default = "default_fastsync_peer_wait_timeout_sec")]
    fastsync_peer_wait_timeout_sec: u64,
    /// redb cache size in megabytes (default: 256).
    #[serde(default = "default_cache_mb")]
    cache_mb: u64,
    /// Live-heap threshold (MB) above which the validation sweep commits
    /// the redb write transaction mid-sweep. 0 disables the memory trigger
    /// and flushes degenerate to every `flush_max_blocks`. Default tuned to
    /// 4 GB, empirically the point where the redb write-tx dirty-page
    /// cache starts dominating live heap during initial sync.
    #[serde(default = "default_flush_heap_threshold_mb")]
    flush_heap_threshold_mb: u64,
    /// Upper bound on blocks between flushes. Bounds crash-recovery work.
    #[serde(default = "default_flush_max_blocks")]
    flush_max_blocks: u32,
    /// Lower bound on blocks between flushes. Prevents storm-flushing when
    /// heap growth is driven by something other than the redb write tx.
    #[serde(default = "default_flush_min_blocks")]
    flush_min_blocks: u32,
    /// Mining configuration.
    #[serde(default)]
    mining: MiningConfig,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            state_type: default_state_type(),
            verify_transactions: default_verify_transactions(),
            blocks_to_keep: default_blocks_to_keep(),
            revalidate: false,
            checkpoint_height: None,
            utxo_bootstrap: false,
            min_snapshot_peers: default_min_snapshot_peers(),
            storing_snapshots: 0,
            snapshot_interval: default_snapshot_interval(),
            mempool_capacity: default_mempool_capacity(),
            min_fee: default_min_fee(),
            api_address: None,
            fastsync: default_fastsync(),
            fastsync_peer: None,
            fastsync_threshold_blocks: default_fastsync_threshold_blocks(),
            fastsync_peer_wait_timeout_sec: default_fastsync_peer_wait_timeout_sec(),
            cache_mb: default_cache_mb(),
            flush_heap_threshold_mb: default_flush_heap_threshold_mb(),
            flush_max_blocks: default_flush_max_blocks(),
            flush_min_blocks: default_flush_min_blocks(),
            mining: MiningConfig::default(),
        }
    }
}

fn default_data_dir() -> String {
    "/var/lib/ergo-node/data".to_string()
}
fn default_state_type() -> String {
    "utxo".to_string()
}
fn default_verify_transactions() -> bool {
    true
}
fn default_blocks_to_keep() -> i64 {
    -1
}
fn default_min_snapshot_peers() -> u32 {
    2
}
fn default_snapshot_interval() -> u32 {
    52224
}
fn default_mempool_capacity() -> usize {
    1000
}
fn default_min_fee() -> u64 {
    1_000_000
}
fn default_fastsync() -> bool {
    true
}
fn default_fastsync_threshold_blocks() -> u32 {
    25_000
}
fn default_fastsync_peer_wait_timeout_sec() -> u64 {
    30
}
fn default_cache_mb() -> u64 {
    256
}
fn default_flush_heap_threshold_mb() -> u64 {
    4096
}
fn default_flush_max_blocks() -> u32 {
    100
}
fn default_flush_min_blocks() -> u32 {
    5
}

/// Top-level config wrapper — just the [node] section, P2P is parsed separately.
#[derive(Debug, Deserialize)]
struct RootConfig {
    #[serde(default)]
    node: Option<NodeConfig>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("ergo-node-rust {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = args.get(1)
        .cloned()
        .unwrap_or_else(|| "ergo.toml".to_string());

    let config = enr_p2p::config::Config::load(&config_path)?;

    // Derive chain config from P2P network setting
    let network = config.proxy.network;
    let chain_config = match network {
        enr_p2p::types::Network::Testnet => ChainConfig::testnet(),
        enr_p2p::types::Network::Mainnet => ChainConfig::mainnet(),
    };

    // Parse node config from the same TOML file
    let config_content = std::fs::read_to_string(&config_path)?;
    let root_config: RootConfig = toml::from_str(&config_content)?;
    let node_config = root_config.node.unwrap_or_default();
    let state_type = match node_config.state_type.as_str() {
        "utxo" => StateType::Utxo,
        "digest" => StateType::Digest,
        "light" => StateType::Light,
        other => {
            return Err(format!("unknown state_type '{}' (expected 'utxo', 'digest', or 'light')", other).into());
        }
    };
    let verify_transactions = node_config.verify_transactions;
    let blocks_to_keep = node_config.blocks_to_keep;
    let revalidate = node_config.revalidate;
    let configured_checkpoint = node_config.checkpoint_height;
    tracing::info!(
        state_type = ?state_type, verify_transactions, blocks_to_keep, revalidate,
        checkpoint_height = ?configured_checkpoint,
        storing_snapshots = node_config.storing_snapshots,
        snapshot_interval = node_config.snapshot_interval,
        cache_mb = node_config.cache_mb,
        "node config"
    );

    // Parse mining config
    let miner_pk_opt: Option<ProveDlog> = if !node_config.mining.miner_pk.is_empty() {
        let pk_bytes = hex::decode(&node_config.mining.miner_pk)
            .map_err(|e| format!("invalid miner_pk hex: {e}"))?;
        let point = EcPoint::sigma_parse_bytes(&pk_bytes)
            .map_err(|e| format!("invalid miner_pk EC point: {e}"))?;
        Some(ProveDlog::new(point))
    } else {
        None
    };

    let miner_votes: [u8; 3] = {
        if node_config.mining.votes.is_empty() || node_config.mining.votes == "000000" {
            [0, 0, 0]
        } else {
            let v = hex::decode(&node_config.mining.votes)
                .map_err(|e| format!("invalid mining votes hex '{}': {e}", node_config.mining.votes))?;
            if v.len() != 3 {
                return Err(format!("mining votes must be exactly 3 bytes, got {}", v.len()).into());
            }
            [v[0], v[1], v[2]]
        }
    };

    // Mining proof cache — shared between the validator callback and the mining task
    let mining_proof_cache: MiningProofCache = Arc::new(std::sync::Mutex::new(None));

    if let Some(ref pk) = miner_pk_opt {
        let pk_hex: String = (*pk.h).into();
        tracing::info!(miner_pk = %pk_hex, votes = %node_config.mining.votes, "mining configured");
    }

    let data_dir = std::path::PathBuf::from(node_config.data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(RedbModifierStore::new(&data_dir.join("modifiers.redb"))?);

    // Shared header chain: pipeline writes, sync reads
    let mut chain = HeaderChain::new(chain_config);

    // Restore chain from stored headers by walking backward from the
    // best-chain tip via parent_id. This is authoritative and resilient
    // to holes in the best-chain height index: the headers-by-id
    // (PRIMARY) table is dense, so following parent links from the
    // recorded tip always reconstructs a contiguous chain if the header
    // data itself is intact. The height-index approach was observed to
    // fail on the Apr 7 test server, where a single-height hole at
    // 219851 truncated the restored chain at 219850, which in turn
    // made the resume-height scan fail and drop the validator back to
    // sweep-from-1 against a persistent AVL at height 228737. Root
    // cause in enr-store's write path is tracked separately; this
    // loader tolerates the divergence regardless.
    if let Some((tip_height, tip_id)) = store.best_header_tip()? {
        // Walk backward collecting IDs only. Parsing each header just to
        // extract parent_id and dropping it avoids keeping 1.76M fully
        // deserialized Header structs resident alongside chain's own
        // by_height storage — the previous Vec<Header> intermediate was
        // the dominant startup RSS peak.
        let mut ids: Vec<[u8; 32]> = Vec::with_capacity(tip_height as usize);
        let mut current_id: [u8; 32] = tip_id;
        let walk_ok = loop {
            let data = match store.get(HEADER_TYPE_ID, &current_id)? {
                Some(d) => d,
                None => {
                    tracing::error!(
                        walked = ids.len(),
                        missing_id = ?current_id,
                        "backward chain walk: header data not found in store"
                    );
                    break false;
                }
            };
            let header = match enr_chain::parse_header(&data) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(
                        walked = ids.len(),
                        "backward chain walk: header parse failed: {e}"
                    );
                    break false;
                }
            };
            let parent_id: [u8; 32] = header.parent_id.0.0;
            let is_genesis = header.height == 1;
            ids.push(current_id);
            if is_genesis {
                break true;
            }
            current_id = parent_id;
        };

        // Replay forward, re-fetching and re-parsing one header at a time.
        // redb's cache makes the second fetch cheap. If the walk failed
        // before reaching genesis, the first try_append will fail parent
        // linkage and we bail — matching the previous partial-replay behavior.
        let mut loaded = 0u32;
        for id in ids.into_iter().rev() {
            let data = match store.get(HEADER_TYPE_ID, &id)? {
                Some(d) => d,
                None => {
                    tracing::error!(
                        loaded,
                        "replay: header data vanished between walk and replay"
                    );
                    break;
                }
            };
            let header = match enr_chain::parse_header(&data) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(loaded, "replay: header parse failed: {e}");
                    break;
                }
            };
            let h = header.height;
            match chain.try_append(header) {
                Ok(enr_chain::AppendResult::Extended) => loaded += 1,
                Ok(enr_chain::AppendResult::Forked { .. }) => {
                    tracing::error!(height = h, "restored header detected as fork — store corrupted?");
                    break;
                }
                Err(e) => {
                    tracing::error!(height = h, "restored header append failed: {e}");
                    break;
                }
            }
        }
        tracing::info!(
            loaded,
            tip = chain.height(),
            declared_tip = tip_height,
            walk_ok,
            "restored header chain from store"
        );
    }

    // Wire the extension loader so chain can read epoch-boundary extensions
    // for parameter recomputation and nipopow proof construction. Bridges
    // chain (which knows nothing about storage) to enr-store via header lookup.
    {
        let store_for_loader = store.clone();
        chain.set_extension_loader(move |height: u32| -> Option<Vec<u8>> {
            let header_id = match store_for_loader.best_header_at(height) {
                Ok(v) => v?,
                Err(e) => {
                    tracing::warn!(height, error = %e, "extension_loader: best_header_at failed");
                    return None;
                }
            };
            let header_bytes = match store_for_loader.get(enr_chain::HEADER_TYPE_ID, &header_id) {
                Ok(v) => v?,
                Err(e) => {
                    tracing::warn!(height, error = %e, "extension_loader: get(header) failed");
                    return None;
                }
            };
            let header = match enr_chain::parse_header(&header_bytes) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(height, error = %e, "extension_loader: parse_header failed");
                    return None;
                }
            };
            let extension_id = enr_chain::section_ids(&header)
                .iter()
                .find(|(t, _)| *t == enr_chain::EXTENSION_TYPE_ID)
                .map(|(_, id)| *id)?;
            match store_for_loader.get(enr_chain::EXTENSION_TYPE_ID, &extension_id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(height, error = %e, "extension_loader: get(extension) failed");
                    None
                }
            }
        });

        // Wire the header loader so chain's LRU cache can fall through to
        // storage on miss. Replaces the materialize-all-headers-in-memory
        // behavior — at 1.76M mainnet headers that was 1.4 GB of live heap.
        // Score loader stays unwired: scores for main-chain headers are
        // empty placeholders in the store (only fork headers carry real
        // scores), so chain's Vec<BigUint> safety net is still authoritative.
        let store_for_header_loader = store.clone();
        chain.set_header_loader(move |height: u32| -> Option<ergo_chain_types::Header> {
            let header_bytes = match store_for_header_loader.read_header_at(height) {
                Ok(v) => v?,
                Err(e) => {
                    tracing::warn!(height, error = %e, "header_loader: read_header_at failed");
                    return None;
                }
            };
            match enr_chain::parse_header(&header_bytes) {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(height, error = %e, "header_loader: parse_header failed");
                    None
                }
            }
        });

        // Note: active_parameters is recomputed from storage AFTER the
        // validator's resume height is known (inside the validator init
        // block below). The chain tip's parameters often diverge from the
        // validator's resume point (e.g. fresh resync, partial state), so
        // recomputing against the chain tip here would load the wrong
        // parameter table for early epoch boundaries.
    }

    let chain = Arc::new(Mutex::new(chain));

    // Modifier channel — P2P produces, pipeline consumes
    let (modifier_tx, modifier_rx) = tokio::sync::mpsc::channel(4096);

    // Clone modifier_tx for the mining block submitter (used after P2P takes ownership)
    let modifier_tx_for_mining = modifier_tx.clone();

    // Grab network settings before P2P takes ownership of config
    let net_settings = config.network_settings();

    // Build Mode feature from node config — tells peers what we can serve.
    // Light mode advertises as Digest on the wire (the closest JVM-recognized
    // shape: state-via-authenticated-proofs, no UTXO set) with verifying=false
    // and blocks_to_keep=0. JVM peers will treat us as a header-only SPV node.
    // The dedicated NiPoPoW bootstrap flag in the wire's mode body lives in
    // p2p's ProxyMode (currently hardcoded to Full); plumbing that through
    // is a separate p2p change, out of scope here.
    let mode_config = enr_p2p::transport::handshake::ModeConfig {
        state_type_id: match state_type {
            StateType::Utxo => 0,
            StateType::Digest | StateType::Light => 1,
        },
        verifying: verify_transactions && state_type != StateType::Light,
        blocks_to_keep: if state_type == StateType::Light { 0 } else { blocks_to_keep as i32 },
    };

    // Start P2P with modifier sink (no validator)
    let p2p = Arc::new(enr_p2p::node::P2pNode::start(config, Some(modifier_tx), mode_config).await?);

    // Register message codes consumed by the main crate's event stream so
    // the router doesn't blindly forward them to all peers.
    for code in [76u8, 78, 80, 90, 91] {
        p2p.register_consumed_code(code).await;
    }

    // Validation pipeline — progress channel feeds sync, delivery channel feeds tracker
    let pipeline_chain = chain.clone();
    let api_store = store.clone(); // for REST API block queries
    let sync_store = SharedStore::new(store.clone());
    let revalidate_store = store.clone(); // for section scan during revalidation
    let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(4);
    // Control channel: unbounded — Reorg/NeedModifier must never be dropped
    let (delivery_control_tx, delivery_control_rx) = tokio::sync::mpsc::unbounded_channel();
    // Data channel: bounded — Received/Evicted are lossy, ok to drop
    // Bursty channel — pipeline produces one notification per stored
    // batch, sync-state drains in tokio::select!. The `try_send` path drops
    // on full (data plane is recoverable via section_ticker), but a tight
    // sliding-window cycle can produce hundreds of batches in milliseconds.
    let (delivery_data_tx, delivery_data_rx) = tokio::sync::mpsc::channel(4096);
    // Transaction channel — pipeline forwards unconfirmed txs to mempool task
    let (tx_tx, tx_rx) = tokio::sync::mpsc::channel::<([u8; 32], Vec<u8>)>(256);

    let pipeline_store = store.clone();
    tokio::spawn(async move {
        let mut pipeline =
            ValidationPipeline::new(modifier_rx, pipeline_chain, pipeline_store, progress_tx, delivery_control_tx, delivery_data_tx);
        pipeline.set_tx_sender(tx_tx);
        pipeline.run().await;
    });

    // Shared validated-height atomic — populated by the validator (see Validator::sync_shared)
    // and read by the snapshot trigger, mining task, and NiPoPoW serve handler.
    // Defined here (above the event demux) because the NiPoPoW closure needs to clone it.
    let shared_validated_height = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let shared_downloaded_height = Arc::new(std::sync::atomic::AtomicU32::new(0));
    // Block-request gate: closed at construction, opened after the boot-time
    // fastsync bootstrap decision resolves. While closed, the sync machine
    // processes incoming events but does not send ModifierRequest.
    let block_request_gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Peer chain tip: published by the sync machine on every incoming SyncInfo.
    // Read by the bootstrap task to decide whether to spawn fastsync.
    let peer_chain_tip = Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Subscribe to events for the sync machine — with snapshot serving demux
    let raw_events = p2p.subscribe().await;

    // Snapshot store: open if serving is enabled
    let snapshot_store = if node_config.storing_snapshots > 0 {
        let store = ergo_node_rust::snapshot_store::SnapshotStore::open(
            &data_dir.join("snapshots.redb"),
        )?;
        Some(std::sync::Arc::new(store))
    } else {
        None
    };

    // Event demux: intercept snapshot serving requests (76/78/80) and NiPoPoW
    // serving/verification (90/91), forward rest to sync.
    let (sync_events_tx, sync_events_rx) = tokio::sync::mpsc::channel(256);
    {
        let snapshot_store = snapshot_store.clone();
        let nipopow_chain = chain.clone();
        let p2p_serve = p2p.clone();
        let nipopow_validated_height = shared_validated_height.clone();
        tokio::spawn(async move {
            let mut events = raw_events;
            while let Some(event) = events.recv().await {
                let handled = if let enr_p2p::protocol::peer::ProtocolEvent::Message {
                    peer_id,
                    message: enr_p2p::protocol::messages::ProtocolMessage::Unknown { code, ref body },
                } = event
                {
                    // Snapshot serving (codes 76, 78, 80)
                    let snapshot_handled = if let Some(ref store) = snapshot_store {
                        if ergo_node_rust::snapshot_serve::is_snapshot_request(code) {
                            if let Some((resp_code, resp_body)) =
                                ergo_node_rust::snapshot_serve::handle_snapshot_request(
                                    code, body, store,
                                )
                            {
                                let msg = enr_p2p::protocol::messages::ProtocolMessage::Unknown {
                                    code: resp_code,
                                    body: resp_body,
                                };
                                let _ = p2p_serve.send_to(peer_id, msg).await;
                            }
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    // NiPoPoW serving (code 90) and verification (code 91).
                    // Code 90 (GetNipopowProof) is fully consumed here — the
                    // serve handler builds and sends the response. Code 91
                    // (NipopowProof) is processed for logging but ALSO
                    // forwarded to sync so the light-client bootstrap state
                    // machine can consume it via transport.next_event(). In
                    // non-light modes the forwarded code 91 event sits in
                    // sync's stream and is silently dropped at the next loop
                    // iteration as an unhandled Unknown — negligible cost.
                    let nipopow_handled = if !snapshot_handled
                        && ergo_node_rust::nipopow_serve::is_nipopow_message(code)
                    {
                        handle_nipopow_event(
                            code,
                            body,
                            peer_id,
                            &nipopow_chain,
                            &p2p_serve,
                            &nipopow_validated_height,
                        )
                        .await;
                        code == ergo_node_rust::nipopow_serve::GET_NIPOPOW_PROOF
                    } else {
                        false
                    };

                    snapshot_handled || nipopow_handled
                } else {
                    false
                };

                if !handled
                    && sync_events_tx.send(event).await.is_err() {
                        break;
                    }
            }
        });
    }

    // Bridge implementations
    let transport = P2pTransport::new(p2p.clone(), sync_events_rx);
    let sync_chain = SharedChain::new(chain.clone());

    // Genesis state root — needed for fresh start or revalidation
    let genesis_digest_hex = match network {
        enr_p2p::types::Network::Testnet => TESTNET_GENESIS_DIGEST,
        enr_p2p::types::Network::Mainnet => MAINNET_GENESIS_DIGEST,
    };
    let genesis_bytes = hex::decode(genesis_digest_hex).expect("invalid genesis digest hex");
    let genesis_digest = ADDigest::try_from(genesis_bytes.as_slice())
        .expect("invalid genesis digest length");

    let utxo_bootstrap = node_config.utxo_bootstrap;
    let min_snapshot_peers = node_config.min_snapshot_peers;
    let shared_state_context: Arc<tokio::sync::RwLock<Option<ergo_validation::ErgoStateContext>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    let (block_applied_tx, block_applied_rx) =
        tokio::sync::mpsc::channel::<Vec<ergo_validation::Transaction>>(64);
    let (height_watch_tx, height_watch_rx) = tokio::sync::watch::channel(0u32);
    let mut chain_guard = chain.lock().await;

    let mut snapshot_reader: Option<SnapshotReader> = None;

    let validator: Option<Validator> = match state_type {
        StateType::Utxo => {
            let state_path = data_dir.join("state.redb");
            let params = AVLTreeParams { key_length: 32, value_length: None };
            let keep_versions = 200u32;
            let mut storage = RedbAVLStorage::open(&state_path, params, keep_versions, CacheSize::Bytes(node_config.cache_mb as usize * 1024 * 1024))
                .expect("failed to open UTXO state storage");

            let checkpoint = configured_checkpoint.unwrap_or(0);

            if let Some(current_version) = storage.version() {
                // Resume branch: storage has data, load its root into a fresh
                // prover and resolve the block height from state's own metadata.
                snapshot_reader = Some(storage.snapshot_reader());
                let resolver = storage.resolver();
                let tree = AVLTree::new(resolver, 32, None);
                let mut prover = BatchAVLProver::new(tree, true);

                // Install the current version's root into the prover's
                // in-memory tree. `rollback` to the CURRENT version is a
                // cheap short-circuit in RedbAVLStorage — same pattern as
                // PersistentBatchAVLProver::new used before.
                let (root, tree_height) = storage
                    .rollback(&current_version)
                    .expect("failed to load current version root from storage");
                prover.base.tree.root = Some(root);
                prover.base.tree.height = tree_height;

                // Freshly-unpacked nodes have is_new=true (NodeHeader::new).
                // Clear it so the first flush after restart doesn't treat the
                // entire tree as "newly inserted".
                prover.base.tree.reset();

                let prover_digest = prover.digest().expect("prover has no root");
                let prover_digest_arr: [u8; 33] = prover_digest.as_ref().try_into()
                    .expect("prover digest should be 33 bytes");
                let chain_height = chain_guard.height();
                let stored_height = storage.block_height();

                // Resolve the validator's resume height from state.redb
                // metadata. `block_height().is_some() == version().is_some()`
                // per the state contract, so the None arm is unreachable here.
                //
                // The Some(0) + non-genesis-digest case is the one-shot legacy
                // migration path: state.redb was written before META_BLOCK_HEIGHT
                // existed; `RedbAVLStorage::open` stamped 0 to preserve the
                // invariant. We resolve via a chain scan; the first subsequent
                // apply_state writes the real height and makes this permanent.
                let height = match stored_height {
                    Some(h) if h > 0 => h,
                    Some(0) => {
                        let genesis_root: [u8; 33] = genesis_digest.into();
                        if prover_digest_arr == genesis_root {
                            0
                        } else {
                            let mut resolved = 0u32;
                            for h in (1..=chain_height).rev() {
                                if let Some(header) = chain_guard.header_at(h) {
                                    let header_root: [u8; 33] = header.state_root.into();
                                    if prover_digest_arr == header_root {
                                        resolved = h;
                                        break;
                                    }
                                }
                            }
                            if resolved == 0 {
                                panic!(
                                    "legacy state.redb digest={} matches no header in [1..{}] and is not genesis — state is corrupt",
                                    hex::encode(prover_digest.as_ref()),
                                    chain_height,
                                );
                            }
                            tracing::warn!(
                                resolved,
                                "legacy state migration: resolved validator height via chain scan"
                            );
                            resolved
                        }
                    }
                    Some(_) => unreachable!(),
                    None => unreachable!(
                        "block_height is None for non-empty storage — state contract violation"
                    ),
                };

                tracing::info!(height, chain_height, checkpoint, stored_height = ?stored_height, "block validator resuming (UTXO mode)");

                // Now that we know the validator's resume height, load the
                // chain's active parameters from the most recent epoch
                // boundary at or before that height. For a fresh resync
                // (height=0) this is a no-op and the chain stays at
                // construction defaults — see chain submodule docs.
                if let Err(e) = chain_guard.recompute_active_parameters_from_storage(height) {
                    tracing::warn!(
                        error = %e,
                        resume_height = height,
                        "failed to recompute active parameters; using current defaults"
                    );
                } else {
                    tracing::info!(
                        resume_height = height,
                        "recomputed active blockchain parameters for validator resume"
                    );
                }

                // Publish the resume height to the shared atomic so
                // consumers that read it at startup (snapshot trigger,
                // NiPoPoW serve handler, mining task) see the real
                // persistent state instead of the 0 the atomic was
                // initialized with. Without this, the no-anchor
                // GetNipopowProof path refuses to serve for the window
                // between binary restart and the first new block being
                // processed.
                shared_validated_height.store(height, std::sync::atomic::Ordering::Relaxed);
                let mining_ctx = miner_pk_opt.as_ref().and_then(|pk| {
                    snapshot_reader.as_ref().map(|sr| MiningCtx {
                        config: build_miner_config(pk, &node_config.mining, miner_votes, network),
                        proof_cache: mining_proof_cache.clone(),
                        snapshot_reader: Arc::new(sr.clone()),
                    })
                });
                Some(Validator::new(
                    ValidatorInner::Utxo(UtxoValidator::new(storage, prover, height, checkpoint)),
                    shared_validated_height.clone(),
                    shared_state_context.clone(),
                    block_applied_tx.clone(),
                    height_watch_tx.clone(),
                    mining_ctx,
                ))
            } else if utxo_bootstrap {
                // Snapshot bootstrap — validator will be created after snapshot download
                tracing::info!("UTXO state empty, will bootstrap from peer snapshot");
                None
            } else {
                // Genesis bootstrap: empty storage, no snapshot.
                snapshot_reader = Some(storage.snapshot_reader());
                let resolver = storage.resolver();
                let tree = AVLTree::new(resolver, 32, None);
                let mut prover = BatchAVLProver::new(tree, true);

                for (box_id, box_bytes) in build_genesis_boxes(network) {
                    prover.perform_one_operation(&Operation::Insert(KeyValue {
                        key: Bytes::copy_from_slice(&box_id),
                        value: Bytes::copy_from_slice(&box_bytes),
                    })).expect("genesis box insert failed");
                }

                // First update commits genesis state with block_height=0
                // (pre-block-1 state). Equivalent to what
                // PersistentBatchAVLProver::new did via its empty-storage branch,
                // but threads block_height through so the next startup resolves
                // directly without a header scan.
                storage
                    .update_with_height(&mut prover, vec![], 0)
                    .expect("genesis state update failed");

                let actual = prover.digest().expect("prover has no root after genesis");
                let expected: [u8; 33] = genesis_digest.into();
                assert_eq!(
                    actual.as_ref(), &expected[..],
                    "genesis UTXO state digest mismatch"
                );

                tracing::info!(checkpoint, "block validator starting from genesis (UTXO mode)");

                // Genesis resync — recompute(0) is a no-op per the chain
                // contract; active_parameters stays at construction defaults
                // (v1-era for mainnet, matching what block 1024's extension
                // will carry).
                let _ = chain_guard.recompute_active_parameters_from_storage(0);
                let mining_ctx = miner_pk_opt.as_ref().and_then(|pk| {
                    snapshot_reader.as_ref().map(|sr| MiningCtx {
                        config: build_miner_config(pk, &node_config.mining, miner_votes, network),
                        proof_cache: mining_proof_cache.clone(),
                        snapshot_reader: Arc::new(sr.clone()),
                    })
                });
                Some(Validator::new(
                    ValidatorInner::Utxo(UtxoValidator::new(storage, prover, 0, checkpoint)),
                    shared_validated_height.clone(),
                    shared_state_context.clone(),
                    block_applied_tx.clone(),
                    height_watch_tx.clone(),
                    mining_ctx,
                ))
            }
        }

        StateType::Digest => {
            let validator = if chain_guard.height() > 0 && !revalidate {
                let tip = chain_guard.tip();
                let height = chain_guard.height();
                let digest = tip.state_root;
                let checkpoint = configured_checkpoint.unwrap_or_else(|| height.saturating_sub(100));
                tracing::info!(
                    height,
                    checkpoint,
                    digest = ?digest,
                    "block validator resuming from stored chain tip (digest mode)"
                );
                // See UTXO resume branch above for the rationale —
                // publish the resume height so startup-time readers
                // don't see 0.
                shared_validated_height.store(height, std::sync::atomic::Ordering::Relaxed);
                DigestValidator::from_state(digest, height, checkpoint)
            } else if revalidate && chain_guard.height() > 0 {
                let checkpoint = configured_checkpoint.unwrap_or(0);
                let chain_height = chain_guard.height();

                // Scan forward to find the first height with all required sections.
                let mut start_from = 0u32;
                for height in 1..=chain_height {
                    let header = match chain_guard.header_at(height) {
                        Some(h) => h,
                        None => continue,
                    };
                    let sections = enr_chain::required_section_ids(&header, state_type);
                    let complete = sections.iter().all(|(type_id, id)| {
                        revalidate_store.get(*type_id, id).ok().flatten().is_some()
                    });
                    if complete {
                        start_from = height;
                        break;
                    }
                }

                if start_from == 0 {
                    tracing::warn!("revalidate: no complete blocks found in store, starting from genesis");
                    DigestValidator::new(genesis_digest, checkpoint)
                } else {
                    let prev_height = start_from - 1;
                    let digest = if prev_height == 0 {
                        genesis_digest
                    } else {
                        chain_guard.header_at(prev_height).unwrap().state_root
                    };
                    tracing::info!(
                        first_complete = start_from,
                        chain_height,
                        checkpoint,
                        "revalidating stored blocks from first complete section"
                    );
                    // Revalidation resets the effective validated
                    // height to prev_height — publish it so the
                    // atomic doesn't lie about the node's state.
                    shared_validated_height.store(prev_height, std::sync::atomic::Ordering::Relaxed);
                    DigestValidator::from_state(digest, prev_height, checkpoint)
                }
            } else {
                let checkpoint = configured_checkpoint.unwrap_or(0);
                tracing::info!(checkpoint, "block validator starting from genesis (digest mode)");
                DigestValidator::new(genesis_digest, checkpoint)
            };
            Some(Validator::new(
                ValidatorInner::Digest(validator),
                shared_validated_height.clone(),
                shared_state_context.clone(),
                block_applied_tx.clone(),
                height_watch_tx.clone(),
                None, // mining requires UTXO mode
            ))
        }

        StateType::Light => {
            // Light mode runs no validator. The chain is bootstrapped from a
            // verified NiPoPoW proof (see sync's light bootstrap state) and
            // tip-following uses HeaderChain::try_append, which the chain
            // crate's light_client_mode flag teaches to skip the
            // expected_difficulty recalc. Mining and transaction validation
            // are not available.
            tracing::info!("light-client mode: no block validator constructed");
            None
        }
    };
    drop(chain_guard);

    // Live-heap probe for the memory-aware flush trigger. Returns 0 when not
    // built with jemalloc; sync then falls back to the max-block guardrail.
    let flush_probe: Option<std::sync::Arc<dyn Fn() -> u64 + Send + Sync>> = {
        #[cfg(feature = "jemalloc")]
        {
            Some(std::sync::Arc::new(|| {
                // Advance the epoch so stats reflect the current allocator
                // state, then read the live allocated-bytes counter. Both
                // calls are documented as cheap (microseconds).
                let _ = tikv_jemalloc_ctl::epoch::advance();
                tikv_jemalloc_ctl::stats::allocated::read().unwrap_or(0) as u64
            }))
        }
        #[cfg(not(feature = "jemalloc"))]
        {
            None
        }
    };

    // Build sync config from P2P network settings
    let net = net_settings;
    let sync_config = SyncConfig {
        delivery_timeout: std::time::Duration::from_secs(net.delivery_timeout_secs),
        max_delivery_checks: net.max_delivery_checks,
        state_type,
        utxo_bootstrap,
        min_snapshot_peers,
        data_dir: data_dir.clone(),
        flush_heap_threshold_mb: node_config.flush_heap_threshold_mb,
        flush_max_blocks: node_config.flush_max_blocks,
        flush_min_blocks: node_config.flush_min_blocks,
        flush_probe,
        ..SyncConfig::default()
    };

    // Snapshot bootstrap channels — only created when needed
    let (snapshot_tx, snapshot_rx, validator_tx_send, validator_rx) = if validator.is_none() && utxo_bootstrap {
        let (stx, srx) = tokio::sync::oneshot::channel::<ergo_sync::snapshot::SnapshotData>();
        let (vtx, vrx) = tokio::sync::oneshot::channel::<Validator>();
        (Some(stx), Some(srx), Some(vtx), Some(vrx))
    } else {
        (None, None, None, None)
    };

    // Start sync in a background task
    let api_downloaded_height = shared_downloaded_height.clone();
    let sync_shared_downloaded_height = shared_downloaded_height.clone();
    let sync_block_request_gate = block_request_gate.clone();
    let sync_peer_chain_tip = peer_chain_tip.clone();
    tokio::spawn(async move {
        let mut sync = HeaderSync::new(
            sync_config, transport, sync_chain, sync_store, validator,
            progress_rx, delivery_control_rx, delivery_data_rx,
            snapshot_tx, validator_rx, sync_shared_downloaded_height,
            sync_block_request_gate, sync_peer_chain_tip,
        );
        sync.run().await;
    });

    // Snapshot handler — receives snapshot data from sync, loads state, sends validator back
    if let Some(snapshot_rx) = snapshot_rx {
        let state_path = data_dir.join("state.redb");
        let validator_tx = validator_tx_send.unwrap();
        let checkpoint = configured_checkpoint.unwrap_or(0);
        let shared_validated_height = shared_validated_height.clone();
        let shared_state_context = shared_state_context.clone();
        let block_applied_tx = block_applied_tx.clone();
        tokio::spawn(async move {
            match snapshot_rx.await {
                Ok(snapshot_data) => {
                    tracing::info!(
                        nodes = snapshot_data.nodes.len(),
                        height = snapshot_data.snapshot_height,
                        "loading snapshot into state"
                    );

                    let params = AVLTreeParams { key_length: 32, value_length: None };
                    let mut storage = RedbAVLStorage::open(&state_path, params, 200, CacheSize::Bytes(node_config.cache_mb as usize * 1024 * 1024))
                        .expect("failed to open state storage for snapshot");

                    let root_hash = snapshot_data.root_hash;
                    let tree_height = snapshot_data.tree_height as usize;
                    let height = snapshot_data.snapshot_height;

                    // Build ADDigest (33 bytes: root_hash[32] + tree_height[1])
                    let mut version_bytes = Vec::with_capacity(33);
                    version_bytes.extend_from_slice(&root_hash);
                    version_bytes.push(snapshot_data.tree_height);
                    let version = Bytes::from(version_bytes);

                    let nodes_iter = snapshot_data.nodes.into_iter().map(|(label, packed)| {
                        (label, Bytes::from(packed))
                    });

                    storage.load_snapshot(nodes_iter, root_hash, tree_height, version.clone(), height)
                        .expect("failed to load snapshot into state");

                    tracing::info!("snapshot loaded, creating validator");

                    // Build prover from the loaded snapshot. rollback() to the
                    // just-written snapshot version is a short-circuit that
                    // loads the root node — same shape as the resume branch.
                    let resolver = storage.resolver();
                    let tree = AVLTree::new(resolver, 32, None);
                    let mut prover = BatchAVLProver::new(tree, true);
                    let (root, tree_h) = storage
                        .rollback(&version)
                        .expect("failed to load snapshot root from storage");
                    prover.base.tree.root = Some(root);
                    prover.base.tree.height = tree_h;

                    // Freshly-unpacked nodes have is_new=true. Clear it so
                    // the first flush doesn't treat the whole snapshot tree
                    // as "newly inserted" in its undo record.
                    prover.base.tree.reset();

                    let validator = Validator::new(
                        ValidatorInner::Utxo(UtxoValidator::new(storage, prover, height, checkpoint)),
                        shared_validated_height.clone(),
                        shared_state_context.clone(),
                        block_applied_tx.clone(),
                        height_watch_tx.clone(),
                        None, // TODO: mining ctx for snapshot bootstrap
                    );
                    // Publish the bootstrap snapshot height to the
                    // shared atomic — see the UTXO resume branch in
                    // main() for the rationale. Snapshot bootstrap
                    // differs from normal resume because the atomic
                    // has been 0 the entire time sync was downloading
                    // the snapshot; this is the first opportunity to
                    // update it.
                    shared_validated_height.store(height, std::sync::atomic::Ordering::Relaxed);
                    let _ = validator_tx.send(validator);
                    tracing::info!(height, "validator sent to sync machine");
                }
                Err(_) => {
                    tracing::warn!("snapshot channel closed without data");
                }
            }
        });
    }

    // Clone snapshot reader for mempool UTXO lookups before snapshot trigger consumes it
    let mempool_snapshot_reader = snapshot_reader.clone();

    // Snapshot creation trigger — periodically dump UTXO state for serving.
    if let Some(reader) = snapshot_reader {
        if node_config.storing_snapshots > 0 {
            let snapshot_store_for_trigger = snapshot_store
                .clone()
                .expect("snapshot_store must exist when storing_snapshots > 0");
            let snapshot_interval = node_config.snapshot_interval;
            let storing_snapshots = node_config.storing_snapshots;
            let reader = std::sync::Arc::new(reader);
            let shared_height = shared_validated_height.clone();

            tokio::spawn(async move {
                let mut last_snapshot_boundary = 0u32;
                loop {
                    tokio::time::sleep(SNAPSHOT_CHECK_INTERVAL).await;

                    // Use the actual validated height, not the chain (header) height.
                    // The validator updates this atomic after each successful block.
                    let validated = shared_height.load(std::sync::atomic::Ordering::Relaxed);
                    // Skip if below the first snapshot boundary. The boundary
                    // formula `validated - ((validated + 1) % interval)` underflows
                    // when validated < interval - 1; the early-return on validated == 0
                    // alone wasn't enough.
                    if validated < snapshot_interval.saturating_sub(1) {
                        continue;
                    }

                    // Find the latest snapshot boundary at or below validated height.
                    // Boundary = largest h where h % interval == interval - 1 and h <= validated.
                    let snapshot_height = validated - ((validated + 1) % snapshot_interval);
                    if snapshot_height == 0 || snapshot_height <= last_snapshot_boundary {
                        continue;
                    }
                    last_snapshot_boundary = snapshot_height;

                    // Skip if we already have a snapshot at this height
                    if let Ok(info) = snapshot_store_for_trigger.snapshots_info() {
                        if info.iter().any(|(h, _)| *h == snapshot_height) {
                            continue;
                        }
                    }

                    let height = snapshot_height;
                    tracing::info!(height, "creating UTXO snapshot");

                    let reader = reader.clone();
                    let store = snapshot_store_for_trigger.clone();
                    let storing = storing_snapshots;

                    let result = tokio::task::spawn_blocking(move || {
                        let dump = reader.dump_snapshot(14)?;
                        match dump {
                            Some(d) => {
                                store.write_snapshot(
                                    height,
                                    d.root_hash,
                                    &d.manifest,
                                    &d.chunks,
                                    storing,
                                )?;
                                Ok::<_, anyhow::Error>(Some(height))
                            }
                            None => Ok(None),
                        }
                    })
                    .await;

                    match result {
                        Ok(Ok(Some(h))) => {
                            tracing::info!(height = h, "UTXO snapshot created and stored");
                        }
                        Ok(Ok(None)) => {
                            tracing::debug!("snapshot skipped — state is empty");
                        }
                        Ok(Err(e)) => {
                            tracing::error!("snapshot creation failed: {e}");
                        }
                        Err(e) => {
                            tracing::error!("snapshot task panicked: {e}");
                        }
                    }
                }
            });
            tracing::info!(snapshot_interval, storing_snapshots, "snapshot creation trigger active");
        }
    }

    // Mempool — in-memory transaction pool with P2P transaction receiver
    let mempool = Arc::new(Mutex::new(ergo_mempool::Mempool::new(
        ergo_mempool::types::MempoolConfig {
            capacity: node_config.mempool_capacity,
            min_fee: node_config.min_fee,
            ..Default::default()
        },
    )));

    // Mempool task: validates incoming transactions, applies confirmed blocks,
    // and runs periodic cleanup/revalidation.
    {
        let mempool = mempool.clone();
        let snapshot_reader_for_mempool = mempool_snapshot_reader.clone();
        let state_context = shared_state_context.clone();
        let p2p_for_mempool = p2p.clone();
        let mut block_applied_rx = block_applied_rx;
        let mut cleanup_interval = tokio::time::interval(MEMPOOL_CLEANUP_INTERVAL);
        cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tokio::spawn(async move {
            let mut tx_rx = tx_rx;
            loop {
                tokio::select! {
                    // Block confirmed — purge confirmed txs + double-spends from pool
                    Some(confirmed_txs) = block_applied_rx.recv() => {
                        let mut pool = mempool.lock().await;
                        let removed = pool.apply_block(&confirmed_txs);
                        if !removed.is_empty() {
                            tracing::debug!(
                                confirmed = confirmed_txs.len(),
                                removed = removed.len(),
                                pool_size = pool.len(),
                                "mempool: applied block"
                            );
                        }
                    }

                    // P2P transaction — deserialize, validate, add to pool
                    Some((tx_id, tx_bytes)) = tx_rx.recv() => {
                        use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
                        use std::io::Cursor;
                        use ergo_lib::ergotree_ir::serialization::constant_store::ConstantStore;
                        use ergo_lib::ergotree_ir::serialization::sigma_byte_reader::SigmaByteReader;

                        // Need state context to validate — skip if not yet available
                        // (still syncing, no blocks validated yet)
                        let ctx_guard = state_context.read().await;
                        let Some(ref ctx) = *ctx_guard else {
                            tracing::trace!(
                                tx_id = hex::encode(tx_id),
                                "mempool: skipping tx, no state context yet"
                            );
                            continue;
                        };

                        let cursor = Cursor::new(&tx_bytes);
                        let mut reader = SigmaByteReader::new(cursor, ConstantStore::empty());
                        let tx = match ergo_validation::Transaction::sigma_parse(&mut reader) {
                            Ok(tx) => tx,
                            Err(e) => {
                                tracing::debug!(
                                    tx_id = hex::encode(tx_id),
                                    "mempool: tx deserialization failed: {e}"
                                );
                                continue;
                            }
                        };

                        let utxo_reader = MempoolUtxoReader {
                            snapshot_reader: snapshot_reader_for_mempool.as_ref(),
                        };

                        let mut pool = mempool.lock().await;
                        let outcome = pool.process(
                            tx,
                            tx_bytes,
                            &utxo_reader,
                            ctx,
                            Some(0), // all P2P txs share one rate-limit budget for now
                        );
                        drop(ctx_guard);

                        match &outcome {
                            ergo_mempool::types::ProcessingOutcome::Accepted { tx_id } => {
                                tracing::info!(
                                    tx_id = hex::encode(tx_id),
                                    pool_size = pool.len(),
                                    "mempool: tx accepted"
                                );
                                let inv = enr_p2p::protocol::messages::ProtocolMessage::Inv {
                                    modifier_type: 2,
                                    ids: vec![*tx_id],
                                };
                                p2p_for_mempool.broadcast_outbound(inv).await;
                            }
                            ergo_mempool::types::ProcessingOutcome::Replaced { tx_id, removed } => {
                                tracing::info!(
                                    tx_id = hex::encode(tx_id),
                                    replaced = removed.len(),
                                    "mempool: tx replaced double-spend"
                                );
                                let inv = enr_p2p::protocol::messages::ProtocolMessage::Inv {
                                    modifier_type: 2,
                                    ids: vec![*tx_id],
                                };
                                p2p_for_mempool.broadcast_outbound(inv).await;
                            }
                            ergo_mempool::types::ProcessingOutcome::Invalidated { reason } => {
                                tracing::debug!(
                                    tx_id = hex::encode(tx_id),
                                    reason,
                                    "mempool: tx invalidated"
                                );
                            }
                            ergo_mempool::types::ProcessingOutcome::Declined { reason } => {
                                tracing::trace!(
                                    tx_id = hex::encode(tx_id),
                                    reason,
                                    "mempool: tx declined"
                                );
                            }
                            ergo_mempool::types::ProcessingOutcome::AlreadyInPool => {}
                            ergo_mempool::types::ProcessingOutcome::DoubleSpendLoser { .. } => {
                                tracing::trace!(
                                    tx_id = hex::encode(tx_id),
                                    "mempool: tx lost double-spend contest"
                                );
                            }
                        }
                    }

                    // Periodic cleanup — revalidate pool + rebroadcast
                    _ = cleanup_interval.tick() => {
                        let ctx_guard = state_context.read().await;
                        if let Some(ref ctx) = *ctx_guard {
                            let utxo_reader = MempoolUtxoReader {
                                snapshot_reader: snapshot_reader_for_mempool.as_ref(),
                            };
                            let mut pool = mempool.lock().await;
                            let removed = pool.revalidate(&utxo_reader, ctx);
                            if !removed.is_empty() {
                                tracing::info!(
                                    removed = removed.len(),
                                    pool_size = pool.len(),
                                    "mempool: cleanup removed invalid txs"
                                );
                            }

                            // Rebroadcast selected txs to peers
                            let rebroadcast = pool.select_for_rebroadcast(&utxo_reader);
                            if !rebroadcast.is_empty() {
                                let ids: Vec<[u8; 32]> = rebroadcast.iter()
                                    .map(|utx| {
                                        let id: [u8; 32] = utx.tx.id().as_ref().try_into().unwrap();
                                        id
                                    })
                                    .collect();
                                tracing::debug!(count = ids.len(), "mempool: rebroadcasting txs");
                                let inv = enr_p2p::protocol::messages::ProtocolMessage::Inv {
                                    modifier_type: 2,
                                    ids,
                                };
                                p2p_for_mempool.broadcast_outbound(inv).await;
                            }
                        }
                    }

                    else => break,
                }
            }
        });
    }

    // REST API server
    {
        let api_bind_addr: std::net::SocketAddr = node_config.api_address
            .as_deref()
            .unwrap_or(match network {
                enr_p2p::types::Network::Testnet => "0.0.0.0:9053",
                enr_p2p::types::Network::Mainnet => "0.0.0.0:9052",
            })
            .parse()
            .expect("invalid api_address");

        let api_chain = chain.clone();
        let api_mempool = mempool.clone();
        let api_state_ctx = shared_state_context.clone();
        let p2p_for_api = p2p.clone();
        let p2p_for_api_urls = p2p.clone();

        // Mining: construct CandidateGenerator + mining task if configured
        let mining_generator: Option<Arc<ergo_mining::CandidateGenerator>> =
            if let Some(ref pk) = miner_pk_opt {
                if state_type == StateType::Utxo {
                    let generator = Arc::new(ergo_mining::CandidateGenerator::new(
                        build_miner_config(pk, &node_config.mining, miner_votes, network),
                    ));

                    // Mining task: watches shared_height for tip changes, builds candidates
                    let gen = generator.clone();
                    let proof_cache = mining_proof_cache.clone();
                    let mining_height = shared_validated_height.clone();
                    let mining_chain = chain.clone();
                    let mining_store = store.clone();
                    tokio::spawn(async move {
                        let mut last_height = 0u32;
                        loop {
                            tokio::time::sleep(MINING_POLL_INTERVAL).await;
                            let current = mining_height.load(std::sync::atomic::Ordering::Relaxed);
                            if current == last_height || current == 0 {
                                continue;
                            }
                            last_height = current;

                            // Read pre-computed proofs from the validator callback
                            let proof_data = {
                                let guard = proof_cache.lock().unwrap_or_else(|e| e.into_inner());
                                guard.clone()
                            };

                            let proof_data = match proof_data {
                                Some(d) if d.tip_height == current => d,
                                _ => continue, // proofs not ready yet
                            };

                            let candidate_height = proof_data.parent.height + 1;

                            // Single chain lock: read n_bits, capture the active
                            // proposed-update payload, check epoch boundary, compute
                            // expected params if needed. The proposed-update bytes
                            // must match what we encode into the extension below so
                            // other peers compute identical expected params during
                            // validation.
                            let (n_bits, proposed_update_bytes, boundary_params) = {
                                let chain_guard = mining_chain.lock().await;
                                let n_bits = chain_guard.tip().n_bits;
                                let proposed_update_bytes =
                                    chain_guard.active_proposed_update_bytes().to_vec();
                                let bp = if chain_guard.is_epoch_boundary(candidate_height) {
                                    match chain_guard.compute_expected_parameters(
                                        candidate_height,
                                        &proposed_update_bytes,
                                    ) {
                                        Ok(p) => Some(p),
                                        Err(e) => {
                                            tracing::warn!(
                                                candidate_height,
                                                "mining: compute_expected_parameters failed: {e}"
                                            );
                                            continue;
                                        }
                                    }
                                } else {
                                    None
                                };
                                (n_bits, proposed_update_bytes, bp)
                            };

                            // Read parent extension to unpack interlinks for the new block.
                            // The parent extension lookup mirrors the chain extension loader:
                            // header → section_ids[2] → extension bytes → mining helper.
                            let parent_interlinks = {
                                let parent_extension_id =
                                    enr_chain::section_ids(&proof_data.parent)[2].1;
                                match mining_store
                                    .get(enr_chain::EXTENSION_TYPE_ID, &parent_extension_id)
                                {
                                    Ok(Some(ext_bytes)) => {
                                        ergo_mining::extension::unpack_parent_interlinks(&ext_bytes)
                                    }
                                    Ok(None) => {
                                        // Parent extension not yet stored (genesis or fresh chain)
                                        vec![]
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "mining: parent extension store read failed: {e}; using empty interlinks"
                                        );
                                        vec![]
                                    }
                                }
                            };

                            // Build extension + header + WorkMessage
                            let extension = match ergo_mining::extension::build_extension(
                                &proof_data.parent,
                                &parent_interlinks,
                                boundary_params.as_ref(),
                                &proposed_update_bytes,
                            ) {
                                Ok(ext) => ext,
                                Err(e) => {
                                    tracing::warn!("mining: extension build failed: {e}");
                                    continue;
                                }
                            };

                            let candidate = ergo_mining::CandidateBlock {
                                parent: proof_data.parent.clone(),
                                version: proof_data.parent.version,
                                n_bits,
                                state_root: proof_data.state_root,
                                ad_proof_bytes: proof_data.ad_proof_bytes.clone(),
                                transactions: vec![proof_data.emission_tx.clone()],
                                timestamp: {
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_millis() as u64;
                                    std::cmp::max(now, proof_data.parent.timestamp + 1)
                                },
                                extension,
                                votes: gen.config.votes,
                                header_bytes: vec![],
                            };

                            match ergo_mining::candidate::build_work_message(
                                &candidate,
                                &gen.config.miner_pk.h,
                            ) {
                                Ok((header_bytes, work)) => {
                                    let mut candidate = candidate;
                                    candidate.header_bytes = header_bytes;
                                    gen.cache_candidate(candidate, work, current);
                                    tracing::debug!(height = current + 1, "mining candidate cached");
                                }
                                Err(e) => {
                                    tracing::warn!("mining: work message build failed: {e}");
                                }
                            }
                        }
                    });
                    tracing::info!("mining task started");
                    Some(generator)
                } else {
                    tracing::warn!("mining configured but node is in digest mode — mining disabled");
                    None
                }
            } else {
                None
            };

        let api_state = ergo_api::ApiState {
            chain: Arc::new(HeaderChainAdapter { chain: api_chain }),
            store: Arc::new(StoreAdapter { store: api_store }),
            mempool: api_mempool,
            utxo_reader: Arc::new(ApiUtxoReader {
                snapshot_reader: mempool_snapshot_reader.clone(),
            }),
            state_context: api_state_ctx,
            peer_count: Arc::new(move || {
                let count = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(p2p_for_api.peer_count())
                });
                ergo_api::PeerCounts { connected: count }
            }),
            mining: mining_generator.clone(),
            block_submitter: mining_generator.as_ref().map(|_| {
                Arc::new(MinedBlockSubmitter {
                    store: store.clone(),
                    modifier_tx: modifier_tx_for_mining.clone(),
                }) as Arc<dyn ergo_api::BlockSubmitter>
            }),
            validated_height: shared_validated_height.clone(),
            downloaded_height: api_downloaded_height.clone(),
            peer_api_urls: Arc::new(move || {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(p2p_for_api_urls.peer_rest_urls())
                })
                .into_iter()
                .map(|(peer_id, addr, rest_url)| ergo_api::PeerRestInfo {
                    peer_id: peer_id.0,
                    addr,
                    rest_url,
                })
                .collect()
            }),
            modifier_tx: Some(modifier_tx_for_mining.clone()),
            height_watch: height_watch_rx,
            jemalloc_probe: {
                #[cfg(feature = "jemalloc")]
                {
                    Some(Arc::new(|| {
                        let _ = tikv_jemalloc_ctl::epoch::advance();
                        ergo_api::JemallocSnapshot {
                            allocated: tikv_jemalloc_ctl::stats::allocated::read().unwrap_or(0) as u64,
                            active: tikv_jemalloc_ctl::stats::active::read().unwrap_or(0) as u64,
                            resident: tikv_jemalloc_ctl::stats::resident::read().unwrap_or(0) as u64,
                            retained: tikv_jemalloc_ctl::stats::retained::read().unwrap_or(0) as u64,
                            metadata: tikv_jemalloc_ctl::stats::metadata::read().unwrap_or(0) as u64,
                        }
                    }))
                }
                #[cfg(not(feature = "jemalloc"))]
                {
                    None
                }
            },
            node_info: std::sync::Arc::new(ergo_api::NodeMeta {
                name: "ergo-node-rust".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                network: match network {
                    enr_p2p::types::Network::Testnet => "testnet".to_string(),
                    enr_p2p::types::Network::Mainnet => "mainnet".to_string(),
                },
                state_type: match state_type {
                    StateType::Utxo => "utxo".to_string(),
                    StateType::Digest => "digest".to_string(),
                    StateType::Light => "light".to_string(),
                },
            }),
        };

        tokio::spawn(async move {
            if let Err(e) = ergo_api::serve(api_state, api_bind_addr).await {
                tracing::error!("REST API server failed: {e}");
            }
        });

        // Boot-time bootstrap decision: optionally spawn fastsync and wait
        // for it to exit, then open the P2P block-request gate. The gate is
        // opened in every path so P2P sync always resumes. See facts/sync.md
        // "Bootstrap Mode (Optional Fastsync)".
        let fastsync_enabled = node_config.fastsync;
        let fastsync_peer = node_config.fastsync_peer.clone();
        let fastsync_threshold = node_config.fastsync_threshold_blocks;
        let fastsync_peer_wait = std::time::Duration::from_secs(
            node_config.fastsync_peer_wait_timeout_sec,
        );
        let api_port = api_bind_addr.port();
        let bootstrap_gate = block_request_gate.clone();
        let bootstrap_peer_tip = peer_chain_tip.clone();
        let bootstrap_downloaded = shared_downloaded_height.clone();

        tokio::spawn(async move {
            use std::sync::atomic::Ordering;

            if !fastsync_enabled {
                tracing::info!("fastsync disabled in config — skipping bootstrap");
                bootstrap_gate.store(true, Ordering::Relaxed);
                return;
            }

            // Probe the binary — absent binary means skip fastsync entirely.
            let probe = tokio::process::Command::new("ergo-fastsync")
                .arg("--version")
                .output()
                .await;
            if probe.is_err() || !probe.unwrap().status.success() {
                tracing::info!("ergo-fastsync not found in PATH — skipping fastsync");
                bootstrap_gate.store(true, Ordering::Relaxed);
                return;
            }

            // Wait for at least one peer SyncInfo, bounded by timeout.
            let wait_started = std::time::Instant::now();
            let poll_interval = std::time::Duration::from_millis(500);
            let peer_tip = loop {
                let tip = bootstrap_peer_tip.load(Ordering::Relaxed);
                if tip > 0 {
                    break tip;
                }
                if wait_started.elapsed() >= fastsync_peer_wait {
                    tracing::warn!(
                        wait_secs = fastsync_peer_wait.as_secs(),
                        "no peer SyncInfo within fastsync_peer_wait_timeout_sec — skipping fastsync"
                    );
                    bootstrap_gate.store(true, Ordering::Relaxed);
                    return;
                }
                tokio::time::sleep(poll_interval).await;
            };

            let downloaded = bootstrap_downloaded.load(Ordering::Relaxed);
            let gap = peer_tip.saturating_sub(downloaded);

            if gap <= fastsync_threshold {
                tracing::info!(
                    peer_tip, downloaded, gap, threshold = fastsync_threshold,
                    "gap at/below fastsync threshold — going straight to P2P"
                );
                bootstrap_gate.store(true, Ordering::Relaxed);
                return;
            }

            tracing::info!(
                peer_tip, downloaded, gap, threshold = fastsync_threshold,
                "gap exceeds fastsync threshold — spawning fastsync"
            );
            let node_url = format!("http://127.0.0.1:{api_port}");
            let mut cmd = tokio::process::Command::new("ergo-fastsync");
            cmd.arg("--node-url").arg(&node_url);
            cmd.arg("--handoff-distance").arg(fastsync_threshold.to_string());
            if let Some(ref peer) = fastsync_peer {
                cmd.arg("--peer-url").arg(peer);
            }
            let spawn_started = std::time::Instant::now();
            match cmd.status().await {
                Ok(s) if s.success() => tracing::info!(
                    elapsed_secs = spawn_started.elapsed().as_secs(),
                    "fastsync completed"
                ),
                Ok(s) => tracing::warn!(
                    code = ?s.code(),
                    elapsed_secs = spawn_started.elapsed().as_secs(),
                    "fastsync exited with error"
                ),
                Err(e) => tracing::warn!(error = %e, "fastsync spawn failed"),
            }

            // Open the gate regardless of exit status. Validation catches
            // anything fastsync delivered in bad faith; P2P picks up the
            // remainder for any blocks fastsync didn't close the gap on.
            bootstrap_gate.store(true, Ordering::Relaxed);
            tracing::info!("block-request gate opened — P2P block sync active");
        });
    }

    tracing::info!("Ergo node running");

    // Run until interrupted — handle both SIGINT (ctrl-c) and SIGTERM
    // (systemd stop). Without SIGTERM handling, the process exits via
    // default handler and in-progress state writes are lost.
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT received");
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received");
            }
        }
    }

    let height = chain.lock().await.height();
    let peers = p2p.peer_count().await;
    tracing::info!(chain_height = height, peers, "Shutting down");

    // Drop P2P node to close event streams, triggering task shutdown.
    // The pipeline exits when its modifier channel closes (sender dropped
    // with the P2P node). The sync task exits when its event stream ends
    // and runs its end-of-sweep flush in the process.
    drop(p2p);
    // Brief grace period for tasks to finish in-flight work and flush state
    tokio::time::sleep(SHUTDOWN_GRACE).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
    use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
    use ergo_avltree_rust::batch_node::{AVLTree, Node, NodeHeader};
    use ergo_avltree_rust::operation::{KeyValue, Operation};
    use std::sync::Arc;

    #[test]
    fn testnet_genesis_boxes_produce_correct_digest() {
        let boxes = build_genesis_boxes(enr_p2p::types::Network::Testnet);
        assert_eq!(boxes.len(), 3, "expected 3 genesis boxes");

        // Verify box IDs match the JVM's
        let expected_ids = [
            "b69575e11c5c43400bfead5976ee0d6245a1168396b2e2a4f384691f275d501c",
            "3bfaf76c824df668822dfce71abaf688d0281f91c3ac2a271f92fa28c3efaac7",
            "5527430474b673e4aafb08e0079c639de23e6a17e87edd00f78662b43c88aeda",
        ];
        for (i, (id, _)) in boxes.iter().enumerate() {
            assert_eq!(
                hex::encode(id),
                expected_ids[i],
                "box {} ID mismatch",
                i
            );
        }

        // Insert into AVL+ tree and verify genesis state digest
        let resolver: ergo_avltree_rust::batch_node::Resolver =
            Arc::new(|digest: &[u8; 32]| Node::LabelOnly(NodeHeader::new(Some(*digest), None)));
        let tree = AVLTree::new(resolver, 32, None);
        let mut prover = BatchAVLProver::new(tree, false);

        for (id, value) in &boxes {
            prover
                .perform_one_operation(&Operation::Insert(KeyValue {
                    key: Bytes::copy_from_slice(id),
                    value: Bytes::copy_from_slice(value),
                }))
                .expect("genesis box insert failed");
        }

        let digest = prover.digest().expect("prover has no digest");
        let expected_hex = TESTNET_GENESIS_DIGEST;
        assert_eq!(
            hex::encode(&digest),
            expected_hex,
            "genesis state digest mismatch"
        );
    }

    #[test]
    fn mainnet_genesis_boxes_produce_correct_digest() {
        let boxes = build_genesis_boxes(enr_p2p::types::Network::Mainnet);
        assert_eq!(boxes.len(), 3, "expected 3 genesis boxes");

        // Emission and founders boxes are identical to testnet (same monetary
        // settings, same founder PKs). Only the no-premine box differs
        // (different proof strings in registers R4-R8).
        let expected_ids = [
            "b69575e11c5c43400bfead5976ee0d6245a1168396b2e2a4f384691f275d501c",
            "b8ce8cfe331e5eadfb0783bdc375c94413433f65e1e45857d71550d42e4d83bd",
            "5527430474b673e4aafb08e0079c639de23e6a17e87edd00f78662b43c88aeda",
        ];
        for (i, (id, _)) in boxes.iter().enumerate() {
            assert_eq!(
                hex::encode(id),
                expected_ids[i],
                "box {} ID mismatch",
                i
            );
        }

        // Insert into AVL+ tree and verify genesis state digest
        let resolver: ergo_avltree_rust::batch_node::Resolver =
            Arc::new(|digest: &[u8; 32]| Node::LabelOnly(NodeHeader::new(Some(*digest), None)));
        let tree = AVLTree::new(resolver, 32, None);
        let mut prover = BatchAVLProver::new(tree, false);

        for (id, value) in &boxes {
            prover
                .perform_one_operation(&Operation::Insert(KeyValue {
                    key: Bytes::copy_from_slice(id),
                    value: Bytes::copy_from_slice(value),
                }))
                .expect("genesis box insert failed");
        }

        let digest = prover.digest().expect("prover has no digest");
        assert_eq!(
            hex::encode(&digest),
            MAINNET_GENESIS_DIGEST,
            "mainnet genesis state digest mismatch"
        );
    }
}

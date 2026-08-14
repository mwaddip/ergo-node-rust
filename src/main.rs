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
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::AVLTree;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use ergo_chain_types::ADDigest;
use ergo_chain_types::EcPoint;
use ergo_lib::chain::emission::MonetarySettings;
use ergo_lib::chain::genesis;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_node_rust::{
    P2pTransport, PeerStorageAdapter, SharedChain, SharedStore, ValidationPipeline,
};
use ergo_sync::{HeaderSync, SyncConfig, SyncStore};
use ergo_validation::{
    ApplyStateOutcome, BlockValidator, DigestValidator, MiningState, StatePersistence,
    UtxoValidator, ValidationError,
};
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
/// Bounded wait on sync's `JoinHandle` during shutdown — caps how long
/// the process will block for sync's final flush sequence before
/// forcing exit. See facts/sync.md "Graceful shutdown".
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

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
    )
    .expect("genesis box construction failed");

    [emission, no_premine, founders]
        .into_iter()
        .map(|b| {
            let mut id = [0u8; 32];
            id.copy_from_slice(b.box_id().as_ref());
            let bytes = b
                .sigma_serialize_bytes()
                .expect("genesis box serialization failed");
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
/// What the post-apply hook can supply that the mining task cannot get for
/// itself: the parent it was applied against, and the emission box — whose id
/// comes from `MiningState`, i.e. from the validator, which the mining task
/// cannot reach.
///
/// It no longer carries AD proofs or a pre-built emission transaction. Those
/// were computed for an emission-ONLY block; with transaction selection the
/// proofs must cover the assembled list, so `generate_candidate` computes them
/// through `proofs_from_storage` against the reader.
struct MiningProofData {
    parent: ergo_chain_types::Header,
    emission_box: ergo_validation::ErgoBox,
    tip_height: u32,
}

type MiningProofCache = Arc<std::sync::Mutex<Option<MiningProofData>>>;

/// Context for mining proof pre-computation inside the validator callback.
struct MiningCtx {
    // No `config` here: the hook stopped building the emission transaction when
    // assembly moved into `generate_candidate`, and the mining task reads the
    // config off the generator it already holds.
    proof_cache: MiningProofCache,
    snapshot_reader: Arc<SnapshotReader>,
    /// Candidate lifecycle handle — the post-apply hook calls
    /// `on_block_applied` for every applied block (facts/mining.md).
    generator: Arc<ergo_mining::CandidateGenerator>,
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
    /// AVL prover memory gauges, published every
    /// [`PROVER_GAUGE_INTERVAL_BLOCKS`] applied blocks and read by
    /// `/debug/memory`. Digest mode never writes them, so they stay unset and
    /// the endpoint omits the fields — correct, digest mode has no prover.
    prover_modified_nodes_bytes: Arc<std::sync::atomic::AtomicU64>,
    prover_resident_nodes_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// Applied blocks since the gauges were last written.
    blocks_since_prover_gauge: u32,
    /// When the gauges were last written, for the at-tip fallback.
    last_prover_gauge: std::time::Instant,
}

/// How often the prover memory gauges are recomputed, in applied blocks.
///
/// `prover_memory_estimate` walks the resident tree — O(resident nodes), the
/// same order as applying a block at mainnet scale — so this must never run per
/// block (`facts/validation.md`). The structure it measures grows monotonically
/// over hundreds of thousands of blocks, so a coarse gauge loses nothing.
const PROVER_GAUGE_INTERVAL_BLOCKS: u32 = 512;

/// Wall-clock ceiling between publishes, whichever trigger fires first.
///
/// The block interval alone is a sync-shaped rule: 512 blocks is seconds during
/// catch-up and roughly seventeen HOURS at tip, where blocks arrive every two
/// minutes. A gauge that updates twice a day cannot show a growth curve on a
/// synced node, which is the case an operator actually watches.
///
/// The cost that motivated the block interval does not exist at tip — one walk
/// per two-minute block is free — so the two triggers do not conflict: during
/// sync the block count is reached long before this elapses, and at tip this
/// one carries it.
const PROVER_GAUGE_MAX_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

// Size difference between variants is fundamental: UTXO mode carries a
// persistent AVL+ prover (~384 bytes); digest mode doesn't (~44 bytes).
// There's exactly one validator instance per process, so boxing
// `UtxoValidator` saves ~330 bytes once at the cost of an extra alloc
// and a heap indirection on every apply_state / flush call. Not worth it.
#[allow(clippy::large_enum_variant)]
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
        prover_modified_nodes_bytes: Arc<std::sync::atomic::AtomicU64>,
        prover_resident_nodes_bytes: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let h = match &inner {
            ValidatorInner::Digest(v) => v.validated_height(),
            ValidatorInner::Utxo(v) => v.validated_height(),
        };
        shared_height.store(h, std::sync::atomic::Ordering::Relaxed);
        let _ = height_watch_tx.send(h);
        Self {
            inner,
            shared_height,
            shared_state_context,
            block_applied_tx,
            height_watch_tx,
            mining,
            prover_modified_nodes_bytes,
            prover_resident_nodes_bytes,
            // Publish on the first applied block rather than after 512, so a
            // node that is restarted often still reports something.
            blocks_since_prover_gauge: PROVER_GAUGE_INTERVAL_BLOCKS,
            last_prover_gauge: std::time::Instant::now(),
        }
    }

    /// The active variant's mining support. `None` in digest mode, where
    /// candidate assembly has no UTXO set to work from.
    ///
    /// Inherent rather than a `BlockValidator` method because `main` is its
    /// only consumer and does name this type. Putting it on the trait would
    /// force six `sync/` test stubs to declare a capability nothing asks them
    /// for. See facts/validation.md § "How callers reach the split traits" —
    /// the asymmetry with `state_persistence` follows the consumers and is
    /// deliberate.
    fn mining_state(&self) -> Option<&dyn MiningState> {
        match &self.inner {
            ValidatorInner::Digest(_) => None,
            ValidatorInner::Utxo(v) => Some(v),
        }
    }

    /// Pre-compute mining proofs after a successful block validation.
    fn update_mining_proofs(&self, header: &ergo_chain_types::Header) {
        let mining = match &self.mining {
            Some(m) => m,
            None => return,
        };

        // Digest mode implements no MiningState — candidate assembly needs a
        // live UTXO set. `self.mining` is already None there, so this arm is
        // belt-and-braces; the point is that "wrong mode" and "all ERG
        // emitted" are now two distinct returns instead of one shared None.
        let mining_state = match self.mining_state() {
            Some(s) => s,
            None => return,
        };

        let emission_id = match mining_state.emission_box_id() {
            Some(id) => id,
            None => return, // all ERG emitted — and now that is all it means
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

        // Deliberately stops here. Building the emission tx and its proofs
        // belongs to `generate_candidate`, which knows the full transaction
        // list; doing it here would compute proofs for a block that is not the
        // one we are about to mine.
        let mut guard = mining.proof_cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(MiningProofData {
            parent: header.clone(),
            emission_box,
            tip_height: header.height,
        });
    }

    /// Recompute and publish the prover memory gauges, at most once every
    /// [`PROVER_GAUGE_INTERVAL_BLOCKS`] applied blocks.
    ///
    /// The walk is O(resident nodes), so the interval is the point — see
    /// `facts/validation.md`. Digest mode returns `None` from
    /// `state_persistence()` and nothing is ever written, which is why
    /// `/debug/memory` omits the fields there instead of reporting zero.
    fn publish_prover_gauges(&mut self) {
        self.blocks_since_prover_gauge = self.blocks_since_prover_gauge.saturating_add(1);
        let due_by_blocks = self.blocks_since_prover_gauge >= PROVER_GAUGE_INTERVAL_BLOCKS;
        let due_by_time = self.last_prover_gauge.elapsed() >= PROVER_GAUGE_MAX_INTERVAL;
        if !due_by_blocks && !due_by_time {
            return;
        }
        let Some(estimate) = self
            .state_persistence()
            .and_then(|p| p.prover_memory_estimate())
        else {
            return;
        };
        self.blocks_since_prover_gauge = 0;
        self.last_prover_gauge = std::time::Instant::now();
        self.prover_modified_nodes_bytes.store(
            estimate.modified_nodes_bytes,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.prover_resident_nodes_bytes.store(
            estimate.resident_nodes_bytes,
            std::sync::atomic::Ordering::Relaxed,
        );
        tracing::debug!(
            modified_nodes_bytes = estimate.modified_nodes_bytes,
            resident_nodes_bytes = estimate.resident_nodes_bytes,
            node_count = estimate.node_count,
            "prover memory gauges published"
        );
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
                header,
                block_txs,
                ad_proofs,
                extension,
                preceding_headers,
                active_params,
                expected_boundary_params,
                expected_proposed_update,
            ),
            ValidatorInner::Utxo(v) => v.apply_state(
                header,
                block_txs,
                ad_proofs,
                extension,
                preceding_headers,
                active_params,
                expected_boundary_params,
                expected_proposed_update,
            ),
        };
        if result.is_ok() {
            let h = self.validated_height();
            self.shared_height
                .store(h, std::sync::atomic::Ordering::Relaxed);
            let _ = self.height_watch_tx.send(h);

            // Publish state context for mempool/API transaction validation.
            //
            // The UPCOMING context, not `build_state_context`: its consumers
            // validate transactions that are not in a block yet, and those
            // target the block after this one. `header` is the block just
            // applied, so it is the *last* header here, not the preheader —
            // and `preceding_headers` is unchanged, since the builder prepends
            // `header` itself (facts/validation.md § Free Functions: state
            // context). Publishing the current-tip context instead rejects
            // every well-formed transaction on the network with
            // "Creation height H+1 > preheader height".
            //
            // The guard no longer protects the builder — `header` alone
            // satisfies `Headers` — but a context published at height 0 has no
            // meaningful UTXO root for its consumers, so it stays.
            if !preceding_headers.is_empty() {
                let ctx = ergo_validation::build_upcoming_state_context(
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

            // Candidate lifecycle: drop candidates that no longer build on
            // the new tip; clear the solved latch when reached
            // (facts/mining.md, Lifecycle API). Runs for EVERY applied
            // block — own or peer.
            if let Some(m) = &self.mining {
                m.generator.on_block_applied(&header.id, header.height);
            }

            // Pre-compute mining proofs for the next block.
            self.update_mining_proofs(header);

            self.publish_prover_gauges();
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

    fn reset_to(&mut self, height: u32, digest: ADDigest) -> Result<(), ValidationError> {
        let result = match &mut self.inner {
            ValidatorInner::Digest(v) => v.reset_to(height, digest),
            ValidatorInner::Utxo(v) => v.reset_to(height, digest),
        };
        // Republish the validator's ACTUAL height: the new one on Ok, the
        // unchanged one on Err (reset_to Err = validator unmoved).
        let h = self.validated_height();
        self.shared_height
            .store(h, std::sync::atomic::Ordering::Relaxed);
        let _ = self.height_watch_tx.send(h);
        result
    }

    // The four forwards that used to live here — flush, resize_cache,
    // proofs_for_transactions, emission_box_id — are gone. One of them was
    // silently missing for the life of the feature; the narrative is kept in
    // facts/validation.md § "Traits", where it documents why the split
    // happened rather than sitting next to code that no longer exists.
    //
    // Storage and mining are reached through the two accessors below. There
    // is no per-method forwarding left to forget.

    /// The active variant's storage lifecycle. `None` in digest mode, which
    /// owns no redb.
    ///
    /// `None` means "nothing to persist" — never "the flush failed". Callers
    /// must keep those apart; collapsing them is the shape that produced the
    /// `resize_cache` bug documented above. See facts/sync.md § "Flushing a
    /// validator that owns no state".
    ///
    /// This one is a trait method rather than an inherent accessor because
    /// `sync/` is generic over `V: BlockValidator` and never names this type,
    /// so an inherent method here would be out of reach of its only caller.
    fn state_persistence(&self) -> Option<&dyn StatePersistence> {
        match &self.inner {
            ValidatorInner::Digest(_) => None,
            ValidatorInner::Utxo(v) => Some(v),
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
    tracing::warn!("PENALTY peer_ip={ip} type={penalty_type} reason=\"{reason}\"");
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
                    penalize(
                        p2p,
                        peer_id,
                        "misbehavior",
                        &format!("GetNipopowProof parse failed: {e}"),
                        false,
                    )
                    .await;
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
                        let validated_h =
                            shared_validated_height.load(std::sync::atomic::Ordering::Relaxed);
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
                    penalize(
                        p2p,
                        peer_id,
                        "misbehavior",
                        &format!("NipopowProof parse failed: {e}"),
                        false,
                    )
                    .await;
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
                    penalize(
                        p2p,
                        peer_id,
                        "permanent",
                        &format!("NiPoPoW proof verification failed: {e}"),
                        true,
                    )
                    .await;
                }
            }
        }

        _ => {
            // is_nipopow_message guarantees code is 90 or 91; this branch is unreachable.
            debug_assert!(
                false,
                "handle_nipopow_event called with non-nipopow code {code}"
            );
        }
    }
}

/// UtxoReader for the mempool — looks up boxes from the persistent AVL+ tree.
///
/// Owns an `Arc<SnapshotReader>` for the duration of one mempool operation.
/// Constructed per-call from `SwappableReader::current()` so the at-tip
/// transition's reopen of state.redb is observable on subsequent calls.
struct MempoolUtxoReader {
    reader: Option<Arc<SnapshotReader>>,
}

impl ergo_mempool::types::UtxoReader for MempoolUtxoReader {
    fn box_by_id(
        &self,
        box_id: &[u8; 32],
    ) -> Option<ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox> {
        let reader = self.reader.as_ref()?;
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
            if h == 0 {
                None
            } else {
                c.header_at(h)
            }
        })
    }
    /// Maps `chain/`'s estimate onto the api-local mirror type. `api/`
    /// deliberately does not depend on `enr-chain`, so this adapter is the
    /// single seam between the two — and it stays a pure field mapping. Any
    /// arithmetic here would recreate the split that let `AVG_HEADER_BYTES`
    /// go stale against a structure `chain/` had already retired.
    fn memory_estimate(&self) -> ergo_api::ChainMemory {
        let e = self.with_chain(|c| c.memory_estimate());
        ergo_api::ChainMemory {
            index_bytes: e.index_bytes,
            header_cache_bytes: e.header_cache_bytes,
            score_cache_bytes: e.score_cache_bytes,
        }
    }
    fn build_nipopow_proof(
        &self,
        m: u32,
        k: u32,
        header_id: Option<[u8; 32]>,
    ) -> Result<Vec<u8>, String> {
        let block_id =
            header_id.map(|id| ergo_chain_types::BlockId(ergo_chain_types::Digest32::from(id)));
        self.with_chain(|c| {
            enr_chain::build_nipopow_proof(c, m, k, block_id).map_err(|e| e.to_string())
        })
    }
    fn header_ids(&self, offset: u32, limit: u32) -> Vec<[u8; 32]> {
        self.with_chain(|c| {
            let tip = c.height();
            if tip == 0 || offset >= tip {
                return Vec::new();
            }
            let start_height = tip - offset;
            let mut out = Vec::with_capacity(limit as usize);
            let mut h = start_height;
            while out.len() < limit as usize && h >= 1 {
                if let Some(header) = c.header_at(h) {
                    let mut id = [0u8; 32];
                    id.copy_from_slice(header.id.0.as_ref());
                    out.push(id);
                }
                if h == 0 {
                    break;
                }
                h -= 1;
            }
            out
        })
    }
    fn popow_header_by_id(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
        let block_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::from(*id));
        self.with_chain(|c| enr_chain::popow_header_by_id(c, &block_id).map_err(|e| e.to_string()))
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

    /// Always available — the store handle outlives the adapter.
    fn cache_bytes_used(&self) -> Option<u64> {
        Some(self.store.cache_bytes_used())
    }

    /// Evictions, not occupancy, are what respond to the configured cache
    /// size. See facts/store.md.
    fn cache_evictions(&self) -> Option<u64> {
        Some(self.store.cache_evictions())
    }
}

/// Adapter: SwappableReader → UtxoAccess for the API crate.
///
/// Returns `None` when the at-tip handler is mid-swap (reopen window).
/// Callers see 404; the window is bounded to milliseconds in the typical
/// path, ~6s worst case if a snapshot dump holds the old DB.
struct ApiUtxoReader {
    swap_reader: Arc<ergo_node_rust::SwappableReader>,
}

impl ergo_api::UtxoAccess for ApiUtxoReader {
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<ergo_validation::ErgoBox> {
        let reader = self.swap_reader.current()?;
        let value_bytes = reader.lookup_key(box_id)?;
        ergo_validation::deserialize_box(&value_bytes).ok()
    }

    /// `None` during the reopen window, when no reader exists — the same
    /// condition under which every other lookup here returns `None`. Omitting
    /// the field is correct then; reporting 0 would claim an empty cache.
    fn cache_bytes_used(&self) -> Option<u64> {
        Some(self.swap_reader.current()?.cache_bytes_used())
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
            (
                enr_chain::BLOCK_TRANSACTIONS_TYPE_ID,
                block_txs_id,
                header.height,
                block_txs_bytes,
                None,
            ),
            (
                enr_chain::AD_PROOFS_TYPE_ID,
                ad_proofs_id,
                header.height,
                ad_proofs_bytes,
                None,
            ),
            (
                enr_chain::EXTENSION_TYPE_ID,
                extension_id,
                header.height,
                extension_bytes,
                None,
            ),
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
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
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

fn default_votes() -> String {
    "000000".to_string()
}
fn default_reward_delay() -> i32 {
    720
}
fn default_candidate_ttl() -> u64 {
    15
}

/// Node-level config parsed from the `[node]` section of ergo.toml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// REST API bind address (default: 0.0.0.0:9053 mainnet, 0.0.0.0:9052 testnet).
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
    /// TOTAL redb page cache across BOTH databases, in megabytes (default: 512).
    ///
    /// Breaking change: this previously sized `state.redb` alone while
    /// `modifiers.redb` silently took redb's built-in 1 GiB default, so a
    /// config saying 1024 actually used 2048 MB.
    /// Absent = derived from the memory budget (facts/memory.md).
    cache_mb: Option<u64>,
    /// Total memory the node may use, MB. Absent = read the cgroup limit,
    /// else a conservative share of MemTotal.
    memory_budget_mb: Option<u64>,
    /// Percentage of `cache_mb` given to `modifiers.redb`; the remainder goes
    /// to `state.redb`. Valid 1-99, validated at startup.
    cache_store_pct: Option<u32>,
    /// Live-heap threshold (MB) above which the validation sweep commits
    /// the redb write transaction mid-sweep. 0 disables the memory trigger
    /// and flushes degenerate to every `flush_max_blocks`. Default tuned to
    /// 4 GB, empirically the point where the redb write-tx dirty-page
    /// cache starts dominating live heap during initial sync.
    flush_heap_threshold_mb: Option<u64>,
    /// Upper bound on blocks between flushes. Bounds crash-recovery work.
    flush_max_blocks: Option<u32>,
    /// Lower bound on blocks between flushes. Prevents storm-flushing when
    /// heap growth is driven by something other than the redb write tx.
    flush_min_blocks: Option<u32>,

    // ── Deferred-eval backpressure ───────────────────────────────────────
    // Bounds the queue of script evaluations dispatched to rayon but not
    // yet drained. Governs a pool of memory DISJOINT from the redb dirty
    // pages that flush_heap_threshold_mb controls — flushing redb does not
    // free a queued eval, so the two must not share a budget.

    // ── At-tip memory mirrors ────────────────────────────────────────────
    // These take effect once chain sync reaches tip. Until then the cold-
    // sync values above are used. The transition swaps flush settings live
    // and reopens the AVL state DB once with the synced_cache_mb value.
    // Default: each mirror equals its cold-sync parent (= no-op until
    // configured), so existing configs keep their current behavior.
    /// redb cache size (MB) used at tip. Smaller = lower steady-state RSS;
    /// the tradeoff is more disk reads when cold-restarting at tip (cache
    /// has to re-warm from the working set).
    #[serde(default)]
    synced_cache_mb: Option<u64>,
    /// Live-heap flush threshold (MB) used at tip. Per-block updates are
    /// tiny at tip cadence (~1 block / 2 min), so a much lower threshold
    /// is essentially free and keeps the dirty-page cache small.
    #[serde(default)]
    synced_flush_heap_threshold_mb: Option<u64>,
    /// Upper bound on blocks between flushes used at tip.
    #[serde(default)]
    synced_flush_max_blocks: Option<u32>,
    /// Lower bound on blocks between flushes used at tip.
    #[serde(default)]
    synced_flush_min_blocks: Option<u32>,

    /// Maximum gap between state.META_BLOCK_HEIGHT and the modifier
    /// store's recorded validated_height that the startup reconciliation
    /// will trust without a state rollback. Gaps within this threshold
    /// are treated as ordinary flush-window races (cheap: one Immediate
    /// write to bring the store's recorded value forward). Larger gaps
    /// trigger a state rollback to the store's recorded value, forcing
    /// re-validation of the intermediate blocks. Bounded above by
    /// state.keep_versions (currently 256) — gaps beyond that fall back
    /// to forced trust with a loud warning. See `facts/sync.md`
    /// "Cross-DB Durability Handshake" § Configuration.
    #[serde(default = "default_reconciliation_trust_threshold")]
    reconciliation_trust_threshold: u32,

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
            cache_mb: None,
            memory_budget_mb: None,
            cache_store_pct: None,
            flush_heap_threshold_mb: None,
            flush_max_blocks: None,
            flush_min_blocks: None,
            synced_cache_mb: None,
            synced_flush_heap_threshold_mb: None,
            synced_flush_max_blocks: None,
            synced_flush_min_blocks: None,
            reconciliation_trust_threshold: default_reconciliation_trust_threshold(),
            mining: MiningConfig::default(),
        }
    }
}

fn default_data_dir() -> String {
    "./ergo-node-data".to_string()
}
fn default_state_type() -> String {
    "utxo".to_string()
}
/// Deferred until a slow box tells us what inline actually costs. Changing
/// this default is a durability decision, not a tuning one — see
/// facts/validation.md § "Script evaluation modes".
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

fn default_cache_store_pct() -> u32 {
    50
}

/// One-shot maintenance subcommands open the store to touch a handful of keys
/// and exit. They have no working set worth caching, so they take a small
/// fixed budget rather than the operator's configured total — 256 MB to delete
/// one chain-meta key would be absurd.
const MAINTENANCE_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// Reject a split that starves either database. A zero share means a database
/// with no page cache at all, which is a configuration mistake rather than a
/// tuning choice, and should fail at startup rather than produce a node that
/// technically runs.
fn validate_cache_split(cfg: &NodeConfig) -> Result<(), String> {
    // Only an operator-supplied value can be out of range; derivation never
    // produces one. Absent is valid and means "derive".
    if let Some(pct) = cfg.cache_store_pct {
        if !(1..=99).contains(&pct) {
            return Err(format!("cache_store_pct must be 1-99, got {pct}"));
        }
    }
    Ok(())
}

/// `(modifiers_bytes, state_bytes)`. Integer split with the remainder given to
/// state, so the two always sum to exactly `cache_mb` regardless of rounding.
///
/// Note that redb splits whatever it receives a further 90% read / 10% write
/// (`patches/redb/src/db.rs:1177`), and an at-tip in-place resize moves only
/// the read half — see `facts/state.md`.
fn cache_split_bytes(plan: &MemoryPlan) -> (usize, usize) {
    split_cache_mb(plan.cache_mb, plan.cache_store_pct)
}

/// Split an arbitrary MB budget by the store percentage. Used for both the
/// cold-sync `cache_mb` and the at-tip `synced_cache_mb`, so the ratio is one
/// concept rather than two.
fn split_cache_mb(total_mb: u64, store_pct: u32) -> (usize, usize) {
    let total = total_mb as usize * 1024 * 1024;
    let store = total * store_pct as usize / 100;
    (store, total - store)
}
// ── Memory budget derivation ─────────────────────────────────────────────
//
// See facts/memory.md. The node reads its own ceiling rather than being told
// it: a systemd unit with MemoryMax, or a container with --memory, already
// states how much this node may use, and until v0.8.0 the node never looked.

/// Where the ceiling came from. Decides how much of it we are willing to
/// spend — an explicit budget or a cgroup limit is somebody stating this node
/// may have that much; `MemTotal` states only that the machine has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetSource {
    Explicit,
    Cgroup,
    MemTotal,
}

impl BudgetSource {
    fn as_str(self) -> &'static str {
        match self {
            BudgetSource::Explicit => "config",
            BudgetSource::Cgroup => "cgroup",
            BudgetSource::MemTotal => "meminfo",
        }
    }

    fn usable_fraction(self) -> f64 {
        match self {
            BudgetSource::Explicit => 1.00,
            BudgetSource::Cgroup => 0.90,
            BudgetSource::MemTotal => 0.50,
        }
    }
}

/// This process's cgroup memory limit, or None when unconfined.
///
/// Walks the v2 hierarchy upward and takes the MINIMUM: our own cgroup may say
/// `max` while a parent slice carries the real limit, and the effective
/// ceiling is the tightest one on the path.
fn cgroup_memory_limit() -> Option<u64> {
    let self_cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;

    // cgroup v2 — one line, "0::/path".
    if let Some(path) = self_cgroup.lines().find_map(|l| l.strip_prefix("0::")) {
        let mut best: Option<u64> = None;
        let root = std::path::Path::new("/sys/fs/cgroup");
        let mut cur = root.join(path.trim_start_matches('/'));
        loop {
            if let Ok(txt) = std::fs::read_to_string(cur.join("memory.max")) {
                let t = txt.trim();
                // The literal "max" means no limit at this level, not zero.
                if t != "max" {
                    if let Ok(v) = t.parse::<u64>() {
                        best = Some(best.map_or(v, |b: u64| b.min(v)));
                    }
                }
            }
            if cur == root || !cur.pop() {
                break;
            }
        }
        return best;
    }

    // cgroup v1 — "hierarchy:controllers:path", memory controller.
    for line in self_cgroup.lines() {
        let mut parts = line.splitn(3, ':');
        let (_, ctrl, path) = (parts.next()?, parts.next()?, parts.next()?);
        if ctrl.split(',').any(|c| c == "memory") {
            let f = format!("/sys/fs/cgroup/memory{path}/memory.limit_in_bytes");
            if let Ok(txt) = std::fs::read_to_string(f) {
                if let Ok(v) = txt.trim().parse::<u64>() {
                    // v1 signals unlimited with a page-count sentinel near u64::MAX
                    // rather than a word, so treat implausibly large as absent.
                    if v < (1u64 << 62) {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

fn meminfo_total_bytes() -> Option<u64> {
    let txt = std::fs::read_to_string("/proc/meminfo").ok()?;
    txt.lines().find_map(|line| {
        let rest = line.strip_prefix("MemTotal:")?;
        let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb * 1024)
    })
}

/// Anonymous heap the node holds that no cache knob governs. Measured at tip
/// on mainnet 1.85M: jemalloc `allocated` 703 MB against 402 MB of tracked
/// components. Excludes jemalloc's own overhead (a further ~260 MB between
/// `allocated` and `resident`), which is not ours to allocate against.
const BASELINE_ANON_BYTES: u64 = 300 * 1024 * 1024;

/// Shares of the available budget. ⚠ Calibrated on a 32-core box; the
/// constrained-box run that settles them is still in flight (facts/memory.md).
const CACHE_SHARE: f64 = 0.40;
const WRITE_SHARE: f64 = 0.40;
const SYNCED_RATIO: f64 = 0.25;

#[derive(Debug, Clone, Copy)]
struct MemoryBudget {
    source: BudgetSource,
    ceiling_bytes: u64,
    usable_bytes: u64,
}

/// Resolve the ceiling, in the priority order facts/memory.md specifies.
fn detect_memory_budget(explicit_mb: Option<u64>) -> MemoryBudget {
    let mem_total = meminfo_total_bytes();

    let (ceiling, source) = if let Some(mb) = explicit_mb {
        (mb.saturating_mul(1024 * 1024), BudgetSource::Explicit)
    } else if let Some(cg) = cgroup_memory_limit() {
        // A cgroup limit above physical RAM is not a licence to use more than
        // exists, so the ceiling is the tighter of the two.
        (cg.min(mem_total.unwrap_or(u64::MAX)), BudgetSource::Cgroup)
    } else if let Some(mt) = mem_total {
        (mt, BudgetSource::MemTotal)
    } else {
        // No cgroup, no /proc — assume a small box rather than a large one.
        (1024 * 1024 * 1024, BudgetSource::MemTotal)
    };

    let usable = (ceiling as f64 * source.usable_fraction()) as u64;
    MemoryBudget {
        source,
        ceiling_bytes: ceiling,
        usable_bytes: usable,
    }
}

/// Budget left for caches and write buffers after the parts nothing governs.
/// `chain_index_bytes` is 0 in phase 1, when the store has not been opened and
/// the index is not yet knowable.
fn available_bytes(budget: &MemoryBudget, chain_index_bytes: u64) -> u64 {
    let floor = BASELINE_ANON_BYTES.saturating_add(chain_index_bytes);
    budget.usable_bytes.saturating_sub(floor)
}

/// Every memory setting, resolved: derived from the budget unless the operator
/// stated it. An absent config key derives; a present one is obeyed exactly.
#[derive(Debug, Clone, Copy)]
struct MemoryPlan {
    cache_mb: u64,
    cache_store_pct: u32,
    flush_heap_threshold_mb: u64,
    flush_max_blocks: u32,
    flush_min_blocks: u32,
    synced_cache_mb: Option<u64>,
    synced_flush_heap_threshold_mb: Option<u64>,
    synced_flush_max_blocks: Option<u32>,
    synced_flush_min_blocks: Option<u32>,
    /// True when any field came from derivation rather than the config —
    /// decides whether the startup record is worth emitting.
    derived_any: bool,
}

const MIB: u64 = 1024 * 1024;

/// Resolve every memory setting. `chain_index_bytes` is 0 in phase 1, before
/// the store is open and the index is knowable — see facts/memory.md.
fn derive_memory_plan(
    cfg: &NodeConfig,
    budget: &MemoryBudget,
    chain_index_bytes: u64,
) -> MemoryPlan {
    let avail = available_bytes(budget, chain_index_bytes);

    let derived_cache_mb = ((avail as f64 * CACHE_SHARE) as u64 / MIB).max(64);

    // An ABSOLUTE heap level, not a share: the trigger compares against total
    // jemalloc `allocated`, which already contains the caches. A threshold
    // below the cache size would fire on every check forever.
    let derived_flush_mb = (BASELINE_ANON_BYTES
        + chain_index_bytes
        + (avail as f64 * CACHE_SHARE) as u64
        + (avail as f64 * WRITE_SHARE) as u64)
        / MIB;

    let cache_mb = cfg.cache_mb.unwrap_or(derived_cache_mb);
    let derived_any = cfg.cache_mb.is_none()
        || cfg.flush_heap_threshold_mb.is_none()
        || cfg.synced_cache_mb.is_none();

    MemoryPlan {
        cache_mb,
        cache_store_pct: cfg.cache_store_pct.unwrap_or_else(default_cache_store_pct),
        flush_heap_threshold_mb: cfg.flush_heap_threshold_mb.unwrap_or(derived_flush_mb),
        // Not derived: block counts bound crash-recovery work, and nothing
        // measured relates a block count to a memory budget.
        flush_max_blocks: cfg
            .flush_max_blocks
            .unwrap_or_else(default_flush_max_blocks),
        flush_min_blocks: cfg
            .flush_min_blocks
            .unwrap_or_else(default_flush_min_blocks),
        synced_cache_mb: Some(
            cfg.synced_cache_mb
                .unwrap_or(((cache_mb as f64 * SYNCED_RATIO) as u64).max(32)),
        ),
        synced_flush_heap_threshold_mb: cfg.synced_flush_heap_threshold_mb,
        synced_flush_max_blocks: cfg.synced_flush_max_blocks,
        synced_flush_min_blocks: cfg.synced_flush_min_blocks,
        derived_any,
    }
}

fn default_flush_max_blocks() -> u32 {
    100
}
fn default_flush_min_blocks() -> u32 {
    5
}
fn default_reconciliation_trust_threshold() -> u32 {
    100
}
/// Top-level config wrapper.
// ⚠ NO `deny_unknown_fields` here, deliberately. This same file is parsed a
// second time into `enr_p2p`'s own `Config` for [proxy], [listen.*],
// [outbound] and [identity] — sections RootConfig does not declare. Denying
// unknown fields at THIS level would reject every real config at startup.
// The nested sections below are wholly owned by this crate and do deny.
#[derive(Debug, Deserialize)]
struct RootConfig {
    #[serde(default)]
    node: Option<NodeConfig>,
    #[serde(default)]
    stats: Option<StatsConfig>,
    #[serde(default)]
    debug: Option<DebugConfig>,
}

/// `[debug]` toml section — opt-in container for diagnostic subsystems.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
struct DebugConfig {
    #[serde(default)]
    p2p_capture: Option<enr_p2p::capture::CaptureConfig>,
}

/// `[stats]` toml section — opt-in. See `facts/stats.md`.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct StatsConfig {
    #[serde(default = "default_stats_bind")]
    bind_address: std::net::SocketAddr,
}

fn default_stats_bind() -> std::net::SocketAddr {
    "127.0.0.1:9055".parse().expect("static parse")
}

/// Adapter wiring p2p/'s `TrafficSnapshot` to api/'s `P2pCountersSource`.
struct P2pCountersAdapter {
    node: Arc<enr_p2p::node::P2pNode>,
}

impl ergo_api::stats::P2pCountersSource for P2pCountersAdapter {
    fn snapshot(&self) -> ergo_api::stats::P2pCountersSnapshot {
        let s = self.node.traffic_snapshot();
        ergo_api::stats::P2pCountersSnapshot {
            since_unix_seconds: s.since_unix_seconds,
            handshake: conv_p2p_counter(s.handshake),
            get_peers: conv_p2p_counter(s.get_peers),
            peers: conv_p2p_counter(s.peers),
            sync_info: conv_p2p_counter(s.sync_info),
            inv_by_modifier: conv_modifier_map(&s.inv_by_modifier),
            modifier_request_by_modifier: conv_modifier_map(&s.modifier_request_by_modifier),
            modifier_response_by_modifier: conv_modifier_map(&s.modifier_response_by_modifier),
            snapshot_by_code: conv_snapshot_map(&s.snapshot_by_code),
            unknown: conv_p2p_counter(s.unknown),
        }
    }
}

fn conv_p2p_counter(
    c: enr_p2p::protocol::counters::DirectionalCounter,
) -> ergo_api::stats::DirectionalCounter {
    ergo_api::stats::DirectionalCounter {
        in_count: c.in_count,
        in_bytes: c.in_bytes,
        out_count: c.out_count,
        out_bytes: c.out_bytes,
    }
}

fn conv_modifier_map(
    m: &std::collections::BTreeMap<u8, enr_p2p::protocol::counters::DirectionalCounter>,
) -> std::collections::BTreeMap<ergo_api::stats::ModifierTypeKey, ergo_api::stats::DirectionalCounter>
{
    use ergo_api::stats::ModifierTypeKey as K;
    let mut out = std::collections::BTreeMap::new();
    for (&k, &v) in m {
        let key = match k {
            1 => K::Header,
            2 => K::Transaction,
            3 => K::BlockTransactions,
            4 => K::AdProofs,
            5 => K::Extension,
            _ => continue,
        };
        out.insert(key, conv_p2p_counter(v));
    }
    out
}

fn conv_snapshot_map(
    m: &std::collections::BTreeMap<u8, enr_p2p::protocol::counters::DirectionalCounter>,
) -> std::collections::BTreeMap<ergo_api::stats::SnapshotCodeKey, ergo_api::stats::DirectionalCounter>
{
    use ergo_api::stats::SnapshotCodeKey as K;
    let mut out = std::collections::BTreeMap::new();
    for (&k, &v) in m {
        let key = match k {
            76 => K::RequestManifest,
            77 => K::Manifest,
            78 => K::RequestSubtree,
            79 => K::Subtree,
            80 => K::RequestUtxoChunk,
            81 => K::UtxoChunk,
            _ => continue,
        };
        out.insert(key, conv_p2p_counter(v));
    }
    out
}

fn locate_config(args: &[String]) -> Option<String> {
    if let Some(p) = args.iter().skip(1).find(|a| !a.starts_with("--")).cloned() {
        return Some(p);
    }
    let pwd = std::path::Path::new("./ergo.toml");
    if pwd.exists() {
        return Some("./ergo.toml".to_string());
    }
    let xdg_dir = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".config"))
                .ok()
        });
    if let Some(d) = xdg_dir {
        let candidate = d.join("ergo-node/ergo.toml");
        if candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    let etc = std::path::Path::new("/etc/ergo-node/ergo.toml");
    if etc.exists() {
        return Some("/etc/ergo-node/ergo.toml".to_string());
    }
    None
}

/// Minimal config written to `./ergo.toml` on first run when no config file
/// was found in any search location. Testnet, full archival node, IPv6
/// listener — data_dir falls through to the in-pwd `./ergo-node-data`
/// default. For the full annotated reference see `ergo.toml.example`.
const COLD_BOOTSTRAP_CONFIG: &str = r#"# Ergo Node Rust — cold-bootstrap config
#
# Auto-written on first run when no config file was found. Safe to edit.
# For the full annotated reference (every option, with defaults), see
# ergo.toml.example shipped with the tarball, or at
# /usr/share/doc/ergo-node-rust/examples/ on a .deb install.

[proxy]
network = "testnet"

[listen.ipv6]
address = "[::]:9030"
mode = "full"
max_inbound = 20

[outbound]
min_peers = 3
max_peers = 10
seed_peers = [
    "213.239.193.208:9023",
    "128.253.41.110:9020",
    "176.9.15.237:9021",
]

[identity]
agent_name = "ergo-node-rust"
peer_name = "ergo-node-rust"
protocol_version = "6.0.3"
"#;

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

    // Maintenance subcommand: clear the scores-migration sentinel so the
    // next normal start re-runs the migration. Operator tool; intentionally
    // hidden from --help. Takes the same config path arg as the daemon so
    // the data_dir is resolved consistently.
    if args.iter().any(|a| a == "--reset-scores-migration") {
        let config_path = locate_config(&args).ok_or_else(|| -> Box<dyn std::error::Error> {
            "no config found (pass an explicit path, or place one at ./ergo.toml, \
             ~/.config/ergo-node/ergo.toml, or /etc/ergo-node/ergo.toml)"
                .into()
        })?;
        let config_content = std::fs::read_to_string(&config_path)?;
        let root_config: RootConfig = toml::from_str(&config_content)?;
        let node_config = root_config.node.unwrap_or_default();
        let data_dir = std::path::PathBuf::from(node_config.data_dir.clone());
        // Deliberately small: this path deletes one chain-meta key and exits.
        let store =
            RedbModifierStore::new(&data_dir.join("modifiers.redb"), MAINTENANCE_CACHE_BYTES)?;
        store.chain_meta_delete(b"scores_migrated_v1")?;
        store.flush()?;
        tracing::info!(
            "scores-migration sentinel cleared; next normal start will re-run the migration"
        );
        return Ok(());
    }

    // Maintenance subcommand: reclaim unreachable rows from state.redb by
    // rewriting the reachable tree into a fresh database. See the "Offline
    // Compaction" section of facts/state.md.
    //
    // Writes alongside the original and never touches it — swapping is a
    // deliberate operator step, so that a compaction whose stats look wrong
    // costs nothing. Requires the node to be stopped (redb file lock).
    if args.iter().any(|a| a == "--compact-state") {
        let config_path = locate_config(&args).ok_or_else(|| -> Box<dyn std::error::Error> {
            "no config found (pass an explicit path, or place one at ./ergo.toml, \
             ~/.config/ergo-node/ergo.toml, or /etc/ergo-node/ergo.toml)"
                .into()
        })?;
        let config_content = std::fs::read_to_string(&config_path)?;
        let root_config: RootConfig = toml::from_str(&config_content)?;
        let node_config = root_config.node.unwrap_or_default();
        let data_dir = std::path::PathBuf::from(node_config.data_dir);
        let source = data_dir.join("state.redb");
        let dest = data_dir.join("state.redb.compacted");

        if !source.exists() {
            return Err(format!("no state database at {}", source.display()).into());
        }
        if dest.exists() {
            return Err(format!(
                "{} already exists — remove it or move it aside first",
                dest.display()
            )
            .into());
        }

        tracing::info!(
            source = %source.display(),
            dest = %dest.display(),
            "compaction starting — this rewrites only the reachable tree; \
             the source is not modified"
        );

        let started = std::time::Instant::now();
        let progress = |p: enr_state::CompactionProgress| {
            tracing::info!(nodes = p.nodes_written, "compaction: copying");
        };

        let stats = enr_state::RedbAVLStorage::compact_to(&source, &dest, Some(&progress))?;

        let pct = if stats.source_bytes > 0 {
            100.0 - (stats.dest_bytes as f64 / stats.source_bytes as f64 * 100.0)
        } else {
            0.0
        };
        tracing::info!(
            nodes = stats.nodes_written,
            source_bytes = stats.source_bytes,
            dest_bytes = stats.dest_bytes,
            reclaimed_pct = format!("{pct:.1}"),
            block_height = stats.block_height,
            digest = %hex::encode(AsRef::<[u8]>::as_ref(&stats.digest)),
            elapsed_secs = started.elapsed().as_secs(),
            "compaction complete — digest verified against every reachable node"
        );
        tracing::info!(
            "next steps: verify the digest above matches the stateRoot of the header at \
             height {}, then (1) keep the node stopped, (2) move {} aside — do not delete it, \
             (3) rename {} into its place, (4) start the node and confirm it resumes at \
             height {} and applies the next block, (5) only then remove the original. \
             NOTE: the compacted database retains a single version, so rollback is \
             unavailable until {} further blocks have been applied.",
            stats.block_height,
            source.display(),
            dest.display(),
            stats.block_height,
            // Mirrors the hardcoded keep_versions at the storage open site below.
            256u32,
        );
        return Ok(());
    }

    let config_path = match locate_config(&args) {
        Some(p) => p,
        None => {
            std::fs::write("./ergo.toml", COLD_BOOTSTRAP_CONFIG)?;
            tracing::warn!(
                written = "./ergo.toml",
                search_paths = "./ergo.toml, ~/.config/ergo-node/ergo.toml, /etc/ergo-node/ergo.toml",
                "no config found — wrote a default (testnet, full archival, state in ./ergo-node-data). \
                 Edit ./ergo.toml or run ./install.sh for interactive setup."
            );
            "./ergo.toml".to_string()
        }
    };

    let config = enr_p2p::config::Config::load(&config_path)?;

    // Derive chain config from P2P network setting
    let network = config.proxy.network;
    let chain_config = match network {
        enr_p2p::types::Network::Testnet => ChainConfig::testnet(),
        enr_p2p::types::Network::Mainnet => ChainConfig::mainnet(),
    };

    // Wall-clock start, surfaced as `launchTime` on GET /info (see
    // ../facts/api.md). Epoch milliseconds rather than Instant because it
    // leaves the process; consumers derive uptime as currentTime - launchTime.
    let launch_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        network = match network {
            enr_p2p::types::Network::Testnet => "testnet",
            enr_p2p::types::Network::Mainnet => "mainnet",
        },
        "Ergo node starting"
    );

    // Parse node config from the same TOML file
    let config_content = std::fs::read_to_string(&config_path)?;
    let root_config: RootConfig = toml::from_str(&config_content)?;
    let stats_config = root_config.stats.clone();
    let capture_config = root_config.debug.clone().and_then(|d| d.p2p_capture);
    let node_config = root_config.node.unwrap_or_default();

    // Initialize the P2P wire-traffic capture ring per facts/p2p-capture.md.
    // `None` when no [debug.p2p_capture] section, or when enabled=false.
    let capture_handle: Option<Arc<enr_p2p::capture::CaptureHandle>> = match capture_config {
        Some(cfg) => match cfg.resolve()? {
            Some(resolved) => {
                let h = enr_p2p::capture::init(resolved)?;
                tracing::info!("p2p_capture enabled");
                Some(Arc::new(h))
            }
            None => None,
        },
        None => None,
    };
    let capture_tap = capture_handle.as_ref().map(|h| h.tap());
    let capture_access: Option<Arc<dyn enr_p2p::capture::CaptureAccess>> = capture_handle
        .as_ref()
        .map(|h| h.clone() as Arc<dyn enr_p2p::capture::CaptureAccess>);
    let state_type = match node_config.state_type.as_str() {
        "utxo" => StateType::Utxo,
        "digest" => StateType::Digest,
        "light" => StateType::Light,
        other => {
            return Err(format!(
                "unknown state_type '{}' (expected 'utxo', 'digest', or 'light')",
                other
            )
            .into());
        }
    };
    let verify_transactions = node_config.verify_transactions;
    let blocks_to_keep = node_config.blocks_to_keep;
    let revalidate = node_config.revalidate;
    let configured_checkpoint = node_config.checkpoint_height;
    if revalidate && node_config.utxo_bootstrap {
        // Contradictory intent: replay-from-genesis vs skip-to-snapshot.
        return Err("config error: revalidate and utxo_bootstrap are mutually exclusive".into());
    }
    tracing::info!(
        state_type = ?state_type, verify_transactions, blocks_to_keep, revalidate,
        checkpoint_height = ?configured_checkpoint,
        storing_snapshots = node_config.storing_snapshots,
        snapshot_interval = node_config.snapshot_interval,
        cache_mb = ?node_config.cache_mb,
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
            let v = hex::decode(&node_config.mining.votes).map_err(|e| {
                format!(
                    "invalid mining votes hex '{}': {e}",
                    node_config.mining.votes
                )
            })?;
            if v.len() != 3 {
                return Err(
                    format!("mining votes must be exactly 3 bytes, got {}", v.len()).into(),
                );
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

    validate_cache_split(&node_config).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Phase 1 of memory derivation (facts/memory.md). The modifier store must
    // open before the header chain can be restored — the chain is restored FROM
    // it — and redb fixes its cache at open with no resize path. So the index
    // half of the floor is not knowable yet and enters as 0; phase 2 re-derives
    // with the real figure for everything opened later.
    let memory_budget = detect_memory_budget(node_config.memory_budget_mb);
    let plan_phase1 = derive_memory_plan(&node_config, &memory_budget, 0);
    let (store_cache_bytes, _) = cache_split_bytes(&plan_phase1);

    let data_dir = std::path::PathBuf::from(node_config.data_dir.clone());
    std::fs::create_dir_all(&data_dir)?;
    tracing::info!(
        path = %data_dir.join("modifiers.redb").display(),
        cache_bytes = store_cache_bytes,
        "opening modifier store"
    );
    let store = Arc::new(RedbModifierStore::new(
        &data_dir.join("modifiers.redb"),
        store_cache_bytes,
    )?);
    tracing::info!("modifier store opened");

    // One-shot scores backfill migration. v0.4.x stored empty
    // placeholder scores for main-chain headers; v0.5.0 needs real
    // cumulative scores so the chain's ScoreLoader can serve them.
    // Idempotent — re-runs safely if killed mid-walk. Batched into
    // 50_000-entry chunks per redb write tx so unclean-restart
    // recovery work stays bounded.
    if store.chain_meta_get(b"scores_migrated_v1")?.is_none() {
        let entries = store.best_chain_entries()?;
        let total = entries.len();
        if total > 0 {
            tracing::info!(total, "scores migration: starting (one-time backfill)");
            const CHUNK_SIZE: usize = 50_000;
            let mut prev_score = enr_chain::BigUint::default();
            let mut batch: Vec<([u8; 32], Vec<u8>)> = Vec::with_capacity(CHUNK_SIZE);
            for (i, (height, header_id)) in entries.iter().enumerate() {
                let data = store.get(HEADER_TYPE_ID, header_id)?.ok_or_else(|| {
                    format!(
                        "scores migration: header at h={} missing from PRIMARY (id={})",
                        height,
                        hex::encode(header_id),
                    )
                })?;
                let header = enr_chain::parse_header(&data)
                    .map_err(|e| format!("scores migration: parse_header at h={}: {e}", height))?;
                let difficulty = enr_chain::decode_compact_bits(header.n_bits)
                    .to_biguint()
                    .ok_or_else(|| format!("scores migration: bad nBits at h={}", height))?;
                let score = if i == 0 && *height == 1 {
                    // Full chain from genesis: score(1) = difficulty(1).
                    difficulty.clone()
                } else if i == 0 {
                    // Light-client install boundary: base header score = 0
                    // (matches install_from_nipopow_proof convention).
                    enr_chain::BigUint::default()
                } else {
                    &prev_score + &difficulty
                };
                batch.push((*header_id, score.to_bytes_be()));
                prev_score = score;
                let done = i + 1;
                if batch.len() >= CHUNK_SIZE || done == total {
                    store.put_header_score_batch(&batch)?;
                    batch.clear();
                    tracing::info!(done, total, "scores migration: progress");
                }
            }
            tracing::info!(
                headers = total,
                "scores migration: complete, persisting sentinel"
            );
            store.flush()?;
        }
        store.chain_meta_put(b"scores_migrated_v1", &[1u8])?;
        store.flush()?;
        tracing::info!("scores migration: sentinel written");
    }

    // Restore chain state from BEST_CHAIN. Single sequential scan;
    // ~25 MB ID material for a 1.76M-height chain, restored in
    // milliseconds. No header parsing, no PoW recheck, no
    // difficulty replay — the store vouches for that data.
    tracing::info!("restoring header chain from store");
    let best_chain_entries = store.best_chain_entries()?;
    let entry_count = best_chain_entries.len();
    // Capture the tip BlockId before consuming the entries — calling
    // chain.tip() after HeaderChain::restore but before set_header_loader
    // (wired further down) panics with "tip header unavailable — cache
    // evicted with no loader wired", because restore only populates the
    // score table, not the lazy Header cache.
    let tip_id = best_chain_entries.last().map(|(_, id_bytes)| {
        ergo_chain_types::BlockId(ergo_chain_types::Digest32::from(*id_bytes))
    });
    let restore_entries = best_chain_entries.into_iter().map(|(h, id_bytes)| {
        (
            h,
            ergo_chain_types::BlockId(ergo_chain_types::Digest32::from(id_bytes)),
        )
    });
    let mut chain = HeaderChain::restore(chain_config, restore_entries)
        .map_err(|e| format!("header chain restore failed: {e:?}"))?;
    match tip_id {
        Some(tip) => tracing::info!(
            headers = entry_count as u64,
            tip = %tip,
            "header chain restored",
        ),
        None => tracing::info!(headers = 0u64, "header chain restored",),
    }

    // Phase 2: the header index is real now, so re-derive everything that has
    // not been allocated yet — the state cache and the flush thresholds.
    let chain_index_bytes = chain.memory_estimate().index_bytes;
    let memory_plan = derive_memory_plan(&node_config, &memory_budget, chain_index_bytes);
    let (_, state_cache_bytes) = cache_split_bytes(&memory_plan);

    if memory_plan.derived_any {
        // Every input and every output, because auto-sizing's failure mode is
        // not being wrong — it is being wrong invisibly, leaving the operator
        // no line to point at. facts/journal-events.md § memory_budget_derived.
        tracing::info!(
            source = memory_budget.source.as_str(),
            ceiling_mb = memory_budget.ceiling_bytes / MIB,
            usable_mb = memory_budget.usable_bytes / MIB,
            baseline_mb = BASELINE_ANON_BYTES / MIB,
            chain_index_mb = chain_index_bytes / MIB,
            available_mb = available_bytes(&memory_budget, chain_index_bytes) / MIB,
            cache_mb = memory_plan.cache_mb,
            cache_store_pct = memory_plan.cache_store_pct,
            store_cache_mb = store_cache_bytes as u64 / MIB,
            state_cache_mb = state_cache_bytes as u64 / MIB,
            flush_heap_threshold_mb = memory_plan.flush_heap_threshold_mb,
            synced_cache_mb = memory_plan.synced_cache_mb.unwrap_or(0),
            "memory budget derived"
        );
        if memory_budget.source == BudgetSource::MemTotal {
            tracing::info!(
                "memory budget came from MemTotal and takes a conservative share; \
                 set MemoryMax on the unit or memory_budget_mb in ergo.toml to let \
                 the node use more"
            );
        }
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

        // Wire the score loader. After v0.5.0 HEADER_SCORES carries
        // real cumulative scores for every header, so this is now
        // authoritative — there is no in-memory Vec<BigUint> safety
        // net in the chain. Must be wired before any try_append that
        // could miss the LRU cache (e.g. during sync's difficulty
        // recalc walk on long chains).
        let store_for_score_loader = store.clone();
        chain.set_score_loader(move |height: u32| -> Option<enr_chain::BigUint> {
            let header_id = match store_for_score_loader.best_header_at(height) {
                Ok(v) => v?,
                Err(e) => {
                    tracing::warn!(height, error = %e, "score_loader: best_header_at failed");
                    return None;
                }
            };
            match store_for_score_loader.header_score(&header_id) {
                Ok(Some(bytes)) if !bytes.is_empty() => {
                    Some(enr_chain::BigUint::from_bytes_be(&bytes))
                }
                Ok(_) => {
                    tracing::warn!(
                        height,
                        "score_loader: empty or missing score (post-migration store bug?)"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!(height, error = %e, "score_loader: header_score failed");
                    None
                }
            }
        });

        // Verify best-chain parent linkage now that both loaders are wired
        // (the walk reads through them, so it must come after).
        //
        // A restored chain can carry a broken link — `header_at(N).id !=
        // header_at(N+1).parent_id` — if a demoted header ever clobbered a
        // BEST_CHAIN slot the reorg owned. The in-memory chain is correct
        // while the process lives; the damage only materialises on the next
        // restore(). Mainnet ran 1.84M headers restored from a clobbered
        // index without noticing, until the validator applied a losing block
        // body and wedged on an AVL missing-key two thousand blocks later.
        //
        // Bounded by default: a full walk is ~1.8M loader reads. The recent
        // tail is where reorg damage lands, so checking it catches the real
        // failure mode at negligible boot cost. This is diagnostic only — it
        // reports and does not refuse to start, because a broken link is
        // recoverable (rewrite the BEST_CHAIN slot, roll state back to the
        // fork point) and refusing to boot would remove the operator's means
        // of doing so.
        const LINKAGE_CHECK_DEPTH: u32 = 4096;
        match chain.verify_best_chain_linkage(Some(LINKAGE_CHECK_DEPTH)) {
            Ok(()) => {
                tracing::info!(depth = LINKAGE_CHECK_DEPTH, "best-chain linkage verified");
            }
            Err(e) => {
                tracing::error!(
                    depth = LINKAGE_CHECK_DEPTH,
                    error = %e,
                    "BEST-CHAIN LINKAGE BROKEN — the restored chain is internally \
                     inconsistent. Block application will wedge at or above the \
                     reported height with an AVL missing-key error. Recovery needs \
                     the BEST_CHAIN slot rewritten to the correct branch AND the \
                     UTXO state rolled back to the fork point."
                );
            }
        }

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
        blocks_to_keep: if state_type == StateType::Light {
            0
        } else {
            blocks_to_keep as i32
        },
    };

    // Start P2P with modifier sink (no validator)
    let peer_storage = Box::new(PeerStorageAdapter::new(store.clone()));
    // capture_tap from [debug.p2p_capture] in ergo.toml — None when the
    // section is absent or `enabled = false`. See facts/p2p-capture.md.
    let p2p = Arc::new(
        enr_p2p::node::P2pNode::start(
            config,
            Some(modifier_tx),
            mode_config,
            peer_storage,
            capture_tap,
        )
        .await?,
    );

    // Register message codes consumed by the main crate's event stream so
    // the router doesn't blindly forward them to all peers.
    for code in [76u8, 78, 80, 90, 91] {
        p2p.register_consumed_code(code).await;
    }

    // Local-serve hook: answer ModifierRequest from our own store before the
    // router's relay fallback (facts/p2p-routing.md § Local serve hook).
    // Store-blind router, store-aware closure. redb reads are sync + cheap.
    {
        let serve_store = store.clone();
        p2p.set_local_serve(std::sync::Arc::new(
            move |modifier_type: u8, id: &[u8; 32]| {
                serve_store.get(modifier_type, id).ok().flatten()
            },
        ))
        .await;
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
        let mut pipeline = ValidationPipeline::new(
            modifier_rx,
            pipeline_chain,
            pipeline_store,
            progress_tx,
            delivery_control_tx,
            delivery_data_tx,
        );
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
        let store =
            ergo_node_rust::snapshot_store::SnapshotStore::open(&data_dir.join("snapshots.redb"))?;
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
                    message:
                        enr_p2p::protocol::messages::ProtocolMessage::Unknown { code, ref body },
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

                if !handled && sync_events_tx.send(event).await.is_err() {
                    break;
                }
            }
        });
    }

    // Bridge implementations
    let transport = P2pTransport::new(p2p.clone(), sync_events_rx);
    let sync_chain = SharedChain::new(chain.clone(), store.clone());

    // Genesis state root — needed for fresh start or revalidation
    let genesis_digest_hex = match network {
        enr_p2p::types::Network::Testnet => TESTNET_GENESIS_DIGEST,
        enr_p2p::types::Network::Mainnet => MAINNET_GENESIS_DIGEST,
    };
    let genesis_bytes = hex::decode(genesis_digest_hex).expect("invalid genesis digest hex");
    let genesis_digest =
        ADDigest::try_from(genesis_bytes.as_slice()).expect("invalid genesis digest length");

    // Candidate generator — constructed before the validator so the
    // post-apply lifecycle hook (CandidateGenerator::on_block_applied) can
    // hold a handle. Shared with the mining task and the API layer below.
    let mining_generator: Option<Arc<ergo_mining::CandidateGenerator>> =
        if let Some(ref pk) = miner_pk_opt {
            if state_type == StateType::Utxo {
                Some(Arc::new(ergo_mining::CandidateGenerator::new(
                    build_miner_config(pk, &node_config.mining, miner_votes, network),
                )))
            } else {
                tracing::warn!("mining configured but node is in digest mode — mining disabled");
                None
            }
        } else {
            None
        };

    let utxo_bootstrap = node_config.utxo_bootstrap;
    let min_snapshot_peers = node_config.min_snapshot_peers;
    let shared_state_context: Arc<tokio::sync::RwLock<Option<ergo_validation::ErgoStateContext>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    let (block_applied_tx, block_applied_rx) =
        tokio::sync::mpsc::channel::<Vec<ergo_validation::Transaction>>(64);
    let (height_watch_tx, height_watch_rx) = tokio::sync::watch::channel(0u32);

    // Memory gauges for /debug/memory. Storage is a plain Arc<AtomicU64> so one
    // allocation serves a producer in `validation/`-via-`Validator` or `sync/`
    // and a reader in `api/` — those crates cannot name each other's types
    // (facts/api.md). `u64::MAX` is the never-published sentinel on both ends,
    // which is what makes an unmeasured field an absent JSON key rather than a
    // zero asserting an empty prover.
    // Minted through the reader's own constructor so the sentinel has one
    // definition at the mint site rather than two that happen to agree.
    let unset_gauge = || ergo_api::PublishedGauge::unset().storage();
    let prover_modified_nodes_bytes = unset_gauge();
    let prover_resident_nodes_bytes = unset_gauge();
    let shared_window_bytes = unset_gauge();

    // `sync/` re-stamps its own `WINDOW_BYTES_UNSET` into the window gauge in
    // `HeaderSync::new`, and that constant is defined independently of the
    // reader's. They agree today and nothing enforces it: if they diverged, an
    // unmeasured window would render as a ~16 EiB reading instead of an absent
    // key — a wrong answer wearing the shape of a right one, which is the exact
    // failure `/debug/memory` exists to avoid.
    debug_assert!(
        {
            let probe = Arc::new(std::sync::atomic::AtomicU64::new(
                ergo_sync::WINDOW_BYTES_UNSET,
            ));
            ergo_api::PublishedGauge::from_storage(probe)
                .get()
                .is_none()
        },
        "ergo_sync::WINDOW_BYTES_UNSET is not the sentinel PublishedGauge reads as unset"
    );
    let mut chain_guard = chain.lock().await;

    let swap_reader = Arc::new(ergo_node_rust::SwappableReader::empty());

    // The checkpoint the validator is ACTUALLY constructed with, captured from
    // whichever branch below runs. It decides which blocks skip script
    // evaluation entirely: at or below it, `apply_state` builds no
    // `ScriptEvalInputs` and runs nothing.
    //
    // It used to have a second consumer — `sync` floored `script_verified_height`
    // at this value, and the two had to be the same number or a checkpointed
    // node left a permanent frontier hole. That watermark is gone with deferred
    // evaluation, so the coupling is gone with it; the value now has exactly
    // one meaning and one reader.
    //
    // This is deliberately NOT `configured_checkpoint.unwrap_or(0)`. Digest
    // mode resuming from a stored tip defaults to `height - 100`, not 0 — so
    // that one expression would put the floor up to a whole chain below the
    // eval-skip boundary on an unconfigured digest node.
    //
    // Every branch obtains its checkpoint through `resolve_checkpoint` so a
    // future branch computing one directly stands out from its neighbours.
    let effective_checkpoint = std::cell::Cell::new(0u32);
    let resolve_checkpoint = |checkpoint: u32| -> u32 {
        effective_checkpoint.set(checkpoint);
        checkpoint
    };

    let mut validator: Option<Validator> = match state_type {
        StateType::Utxo => {
            let state_path = data_dir.join("state.redb");
            let params = AVLTreeParams {
                key_length: 32,
                value_length: None,
            };
            let keep_versions = 256u32;
            tracing::info!(
                path = %state_path.display(),
                // The split, not the configured total — logging cache_mb here
                // would claim this database got the whole budget.
                cache_bytes = state_cache_bytes,
                "opening UTXO state storage"
            );
            let mut storage = RedbAVLStorage::open(
                &state_path,
                params,
                keep_versions,
                CacheSize::Bytes(state_cache_bytes),
            )
            .expect("failed to open UTXO state storage");

            // `revalidate` in UTXO mode: the state tree cannot be rolled back
            // to genesis in place (the undo log holds keep_versions, not the
            // whole chain), so a replay means discarding the state file and
            // rebuilding through the genesis-bootstrap branch below. Stored
            // headers and sections are untouched — sync re-applies them
            // through full validation; nothing is re-downloaded.
            if revalidate && storage.version().is_some() {
                tracing::info!(
                    path = %state_path.display(),
                    "revalidate: discarding UTXO state, rebuilding from genesis"
                );
                drop(storage);
                std::fs::remove_file(&state_path)
                    .expect("revalidate: failed to remove UTXO state file");
                storage = RedbAVLStorage::open(
                    &state_path,
                    AVLTreeParams {
                        key_length: 32,
                        value_length: None,
                    },
                    keep_versions,
                    CacheSize::Bytes(state_cache_bytes),
                )
                .expect("revalidate: failed to re-open UTXO state storage");
            }

            let checkpoint = resolve_checkpoint(configured_checkpoint.unwrap_or(0));

            if let Some(current_version) = storage.version() {
                // Resume branch: storage has data, load its root into a fresh
                // prover and resolve the block height from state's own metadata.
                let sr = storage.snapshot_reader();
                swap_reader.install(sr.clone());

                // Load the persisted root BEFORE constructing the prover.
                // BatchAVLProver::new snapshots tree.root into old_top_node
                // (line 74). If the tree is empty at construction time, the
                // sentinel infinity-leaf is snapshotted instead, and the first
                // resumed block produces a proof against an empty-tree baseline
                // → ProofDigestMismatch. Installing the real root first skips
                // the constructor's is_none() branch and snapshots the correct
                // root. Same fix as PersistentBatchAVLProver::rollback() line 66
                // in avltree 042c830.
                let (root, tree_height) = storage
                    .rollback(&current_version)
                    .expect("failed to load current version root from storage");

                let resolver = storage.resolver();
                let tree = AVLTree::with_resolver(resolver, 32, None);
                let mut prover = BatchAVLProver::new(tree, true);
                prover.restore_root(root, tree_height);

                let prover_digest = prover.digest().expect("prover has no root");
                let prover_digest_arr: [u8; 33] = prover_digest
                    .as_ref()
                    .try_into()
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
                let mining_ctx = mining_generator.as_ref().map(|g| MiningCtx {
                    proof_cache: mining_proof_cache.clone(),
                    snapshot_reader: Arc::new(sr),
                    generator: g.clone(),
                });
                Some(Validator::new(
                    ValidatorInner::Utxo(UtxoValidator::new(storage, prover, height, checkpoint)),
                    shared_validated_height.clone(),
                    shared_state_context.clone(),
                    block_applied_tx.clone(),
                    height_watch_tx.clone(),
                    mining_ctx,
                    prover_modified_nodes_bytes.clone(),
                    prover_resident_nodes_bytes.clone(),
                ))
            } else if utxo_bootstrap {
                // Snapshot bootstrap — validator will be created after snapshot download
                tracing::info!("UTXO state empty, will bootstrap from peer snapshot");
                None
            } else {
                // Genesis bootstrap: empty storage, no snapshot.
                let sr = storage.snapshot_reader();
                swap_reader.install(sr.clone());
                let resolver = storage.resolver();
                let tree = AVLTree::with_resolver(resolver, 32, None);
                let mut prover = BatchAVLProver::new(tree, true);

                for (box_id, box_bytes) in build_genesis_boxes(network) {
                    prover
                        .perform_one_operation(&Operation::Insert(KeyValue {
                            key: Bytes::copy_from_slice(&box_id),
                            value: Bytes::copy_from_slice(&box_bytes),
                        }))
                        .expect("genesis box insert failed");
                }

                // First update commits genesis state with block_height=0
                // (pre-block-1 state). Equivalent to what
                // PersistentBatchAVLProver::new did via its empty-storage branch,
                // but threads block_height through so the next startup resolves
                // directly without a header scan.
                storage
                    .update_with_height(&mut prover, vec![], 0)
                    .expect("genesis state update failed");

                // Commit the genesis batch — without this, the prover's internal
                // batch state (old_top_node, directions) bleeds into block 1, so
                // the proof generated at height 1 covers the genesis bootstrap
                // inserts plus block 1's operations starting from an empty tree
                // instead of just block 1's ops from the 3-box genesis tree.
                // The resulting blake2b256(proof) != header.ad_proofs_root for
                // block 1, triggering a false ProofDigestMismatch.
                prover.generate_proof();

                let actual = prover.digest().expect("prover has no root after genesis");
                let expected: [u8; 33] = genesis_digest.into();
                assert_eq!(
                    actual.as_ref(),
                    &expected[..],
                    "genesis UTXO state digest mismatch"
                );

                tracing::info!(
                    checkpoint,
                    "block validator starting from genesis (UTXO mode)"
                );

                // Genesis resync — recompute(0) is a no-op per the chain
                // contract; active_parameters stays at construction defaults
                // (v1-era for mainnet, matching what block 1024's extension
                // will carry).
                let _ = chain_guard.recompute_active_parameters_from_storage(0);
                let mining_ctx = mining_generator.as_ref().map(|g| MiningCtx {
                    proof_cache: mining_proof_cache.clone(),
                    snapshot_reader: Arc::new(sr),
                    generator: g.clone(),
                });
                Some(Validator::new(
                    ValidatorInner::Utxo({
                        let mut uv = UtxoValidator::new(storage, prover, 0, checkpoint);
                        // Diagnostic: regenerate historical ADProofs that UTXO mode does
                        // not store. Set ENR_DUMP_ADPROOFS_AT=h1,h2,... for a one-shot
                        // genesis replay; writes adproofs-<H>.104 (raw type-104 section)
                        // into data_dir at each listed height. Empty/unset = no-op.
                        if let Ok(spec) = std::env::var("ENR_DUMP_ADPROOFS_AT") {
                            let heights: std::collections::HashSet<u32> = spec
                                .split(',')
                                .filter_map(|s| s.trim().parse().ok())
                                .collect();
                            if !heights.is_empty() {
                                tracing::warn!(
                                    ?heights, dir = %data_dir.display(),
                                    "ENR_DUMP_ADPROOFS_AT set — ADProof dump enabled for this replay"
                                );
                                uv.set_adproof_dump(heights, data_dir.clone());
                            }
                        }
                        uv
                    }),
                    shared_validated_height.clone(),
                    shared_state_context.clone(),
                    block_applied_tx.clone(),
                    height_watch_tx.clone(),
                    mining_ctx,
                    prover_modified_nodes_bytes.clone(),
                    prover_resident_nodes_bytes.clone(),
                ))
            }
        }

        StateType::Digest => {
            let validator = if chain_guard.height() > 0 && !revalidate {
                let tip = chain_guard.tip();
                let height = chain_guard.height();
                let digest = tip.state_root;
                let checkpoint = resolve_checkpoint(
                    configured_checkpoint.unwrap_or_else(|| height.saturating_sub(100)),
                );
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
                let checkpoint = resolve_checkpoint(configured_checkpoint.unwrap_or(0));
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
                    tracing::warn!(
                        "revalidate: no complete blocks found in store, starting from genesis"
                    );
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
                    shared_validated_height
                        .store(prev_height, std::sync::atomic::Ordering::Relaxed);
                    DigestValidator::from_state(digest, prev_height, checkpoint)
                }
            } else {
                let checkpoint = resolve_checkpoint(configured_checkpoint.unwrap_or(0));
                tracing::info!(
                    checkpoint,
                    "block validator starting from genesis (digest mode)"
                );
                DigestValidator::new(genesis_digest, checkpoint)
            };
            Some(Validator::new(
                ValidatorInner::Digest(validator),
                shared_validated_height.clone(),
                shared_state_context.clone(),
                block_applied_tx.clone(),
                height_watch_tx.clone(),
                None, // mining requires UTXO mode
                prover_modified_nodes_bytes.clone(),
                prover_resident_nodes_bytes.clone(),
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
    // Source for the pacing constants at the bottom of this literal. See the
    // comment there for why they are named individually rather than swept in
    // with `..SyncConfig::default()`.
    let sync_defaults = SyncConfig::default();
    let sync_config = SyncConfig {
        delivery_timeout: std::time::Duration::from_secs(net.delivery_timeout_secs),
        max_delivery_checks: net.max_delivery_checks,
        state_type,
        utxo_bootstrap,
        min_snapshot_peers,
        data_dir: data_dir.clone(),
        flush_heap_threshold_mb: memory_plan.flush_heap_threshold_mb,
        flush_max_blocks: memory_plan.flush_max_blocks,
        flush_min_blocks: memory_plan.flush_min_blocks,
        // Derived from THE mode binding, never re-parsed from node_config —
        // sync doing deferred bookkeeping while the validator evaluates
        // inline freezes the frontier; the reverse advances it over blocks
        // nothing verified. Omitting this line used to compile, because the
        // literal ended in `..SyncConfig::default()` and this would have taken
        // `false` in silence. It no longer does; see the note at the bottom.
        synced_flush_heap_threshold_mb: memory_plan.synced_flush_heap_threshold_mb,
        synced_flush_max_blocks: memory_plan.synced_flush_max_blocks,
        synced_flush_min_blocks: memory_plan.synced_flush_min_blocks,
        flush_probe,
        reconciliation_trust_threshold: node_config.reconciliation_trust_threshold,
        // Mirror the handshake's Light-mode override (line above) — in Light
        // there are no bodies to prune anyway, so 0 keeps sync/pruning + the
        // wire advertisement consistent.
        blocks_to_keep: if state_type == StateType::Light {
            0
        } else {
            blocks_to_keep as i32
        },

        // Pacing constants: deliberately not operator-tunable, not derived from
        // config, and not consensus-, memory-, or durability-relevant.
        //
        // Named individually instead of riding `..SyncConfig::default()` so that
        // a 25th field on SyncConfig is a COMPILE ERROR here rather than a
        // silent default. That trap already came close once: `script_eval_inline`
        // would have taken `false` through the fallthrough and left sync doing
        // deferred bookkeeping while the validator evaluated inline — green
        // build, wrong node.
        //
        // Values still come from the Default impl, so there is one source of
        // truth for them. Only the *enumeration* is restated here, which is
        // precisely the part we want the compiler to police.
        //
        // Note this works because struct literals without `..` are exhaustively
        // checked — unlike a `match` on an enum, where `if let` and `matches!`
        // let a new variant through in silence.
        sync_interval: sync_defaults.sync_interval,
        stall_timeout: sync_defaults.stall_timeout,
        synced_poll_interval: sync_defaults.synced_poll_interval,
        delivery_check_interval: sync_defaults.delivery_check_interval,
        min_sync_send_interval: sync_defaults.min_sync_send_interval,
    };

    // Snapshot bootstrap channels — only created when needed
    let (snapshot_tx, snapshot_rx, validator_tx_send, validator_rx) =
        if validator.is_none() && utxo_bootstrap {
            let (stx, srx) = tokio::sync::oneshot::channel::<ergo_sync::snapshot::SnapshotData>();
            let (vtx, vrx) = tokio::sync::oneshot::channel::<Validator>();
            (Some(stx), Some(srx), Some(vtx), Some(vrx))
        } else {
            (None, None, None, None)
        };

    // Cross-DB durability handshake — startup reconciliation.
    // Detect drift between state.redb's META_BLOCK_HEIGHT (canonical) and
    // modifier store's recorded validated_height (durable mirror).
    // Policy in facts/sync.md § "Cross-DB Durability Handshake".
    // Skipped when validator is None (light mode, or utxo_bootstrap pending —
    // the snapshot handler establishes a fresh validator with M == 0, no
    // drift to reconcile).
    if let Some(ref mut v) = validator {
        let m = v.validated_height();
        let stored = sync_store.validated_height().await.unwrap_or(0);
        if m != stored {
            let threshold = node_config.reconciliation_trust_threshold;
            if m > stored {
                let gap = m - stored;
                if gap <= threshold {
                    sync_store.set_validated_height(m).await;
                    tracing::warn!(
                        state_height = m,
                        store_height = stored,
                        mode = "forward",
                        gap,
                        "validated_height drift detected"
                    );
                } else {
                    let chain_guard = chain.lock().await;
                    let header_at_stored = chain_guard.header_at(stored);
                    drop(chain_guard);
                    if let Some(header) = header_at_stored {
                        match v.reset_to(stored, header.state_root) {
                            Ok(()) => {
                                tracing::warn!(
                                    state_height = m,
                                    store_height = stored,
                                    mode = "rollback",
                                    gap,
                                    "validated_height drift detected"
                                );
                            }
                            Err(e) => {
                                // Rollback failed — the validator did not
                                // move (facts/validation.md reset_to Err
                                // postcondition). State genuinely sits at
                                // m: trust it, like the rollback-impossible
                                // arm below; self-corrects on next flush.
                                sync_store.set_validated_height(m).await;
                                tracing::warn!(
                                    state_height = m,
                                    store_height = stored,
                                    mode = "rollback_failed",
                                    gap,
                                    error = %e,
                                    "validated_height drift detected"
                                );
                            }
                        }
                    } else {
                        sync_store.set_validated_height(m).await;
                        tracing::warn!(
                            state_height = m,
                            store_height = stored,
                            mode = "forced_trust",
                            gap,
                            "validated_height drift detected"
                        );
                    }
                }
            } else {
                let gap = stored - m;
                tracing::warn!(
                    state_height = m,
                    store_height = stored,
                    mode = "regressed",
                    gap,
                    "validated_height drift detected"
                );
            }
        }
    }

    // Start sync in a background task
    let api_downloaded_height = shared_downloaded_height.clone();
    let sync_shared_downloaded_height = shared_downloaded_height.clone();
    let sync_block_request_gate = block_request_gate.clone();
    let sync_peer_chain_tip = peer_chain_tip.clone();
    // Shutdown signal: an explicit oneshot is the only deterministic way
    // to tell sync to exit. `drop(p2p)` alone can't close sync's events
    // channel because `P2pTransport` and many other consumers hold Arc
    // clones of the P2P node. See facts/sync.md "Graceful shutdown".
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut sync = HeaderSync::new(
        sync_config,
        transport,
        sync_chain,
        sync_store,
        validator,
        progress_rx,
        delivery_control_rx,
        delivery_data_rx,
        snapshot_tx,
        validator_rx,
        sync_shared_downloaded_height,
        sync_block_request_gate,
        sync_peer_chain_tip,
        shared_window_bytes.clone(),
        shutdown_rx,
    );

    // At-tip cache resize. When `synced_cache_mb` is configured, the sync
    // layer calls resize_cache() on the existing storage handle at first
    // synced() entry — no second Database handle, no mmap coherency bug.
    if state_type == StateType::Utxo {
        if let Some(synced_cache_mb) = memory_plan.synced_cache_mb {
            // `synced_cache_mb` is the at-tip TOTAL, mirroring `cache_mb`, so
            // the same store/state split applies. Only the state side is
            // resizable in place — the modifier store has no resize path.
            //
            // Note this moves less than it appears to: redb splits its budget
            // 90% read / 10% write, and the in-place resize reaches only the
            // read half. See facts/state.md § "An in-place resize moves only
            // 90% of the budget".
            let (_, synced_state_bytes) =
                split_cache_mb(synced_cache_mb, memory_plan.cache_store_pct);
            sync.set_at_tip_cache(synced_state_bytes);
            tracing::info!(
                synced_cache_mb,
                synced_state_bytes,
                "at-tip cache resize wired; will fire on first synced() entry"
            );
        }
    }

    let sync_handle = tokio::spawn(async move {
        sync.run().await;
    });

    // Snapshot handler — receives snapshot data from sync, loads state, sends validator back
    if let Some(snapshot_rx) = snapshot_rx {
        let state_path = data_dir.join("state.redb");
        let validator_tx = validator_tx_send.unwrap();
        // Not routed through `resolve_checkpoint`: this validator is built
        // after `sync_config` already exists, so recording here would be too
        // late to reach it. Safe because this is the UTXO snapshot path, whose
        // match branch above recorded the identical `unwrap_or(0)`. If this
        // ever gains a different default the way digest-resume has, the floor
        // and the eval-skip boundary diverge and `sync` must be told directly.
        let checkpoint = configured_checkpoint.unwrap_or(0);
        let shared_validated_height = shared_validated_height.clone();
        let shared_state_context = shared_state_context.clone();
        let block_applied_tx = block_applied_tx.clone();
        let snapshot_swap_reader = swap_reader.clone();
        let snapshot_chain = chain.clone();
        let prover_modified_nodes_bytes = prover_modified_nodes_bytes.clone();
        let prover_resident_nodes_bytes = prover_resident_nodes_bytes.clone();
        tokio::spawn(async move {
            match snapshot_rx.await {
                Ok(snapshot_data) => {
                    tracing::info!(
                        nodes = snapshot_data.nodes.len(),
                        height = snapshot_data.snapshot_height,
                        "loading snapshot into state"
                    );

                    let params = AVLTreeParams {
                        key_length: 32,
                        value_length: None,
                    };
                    let mut storage = RedbAVLStorage::open(
                        &state_path,
                        params,
                        256,
                        CacheSize::Bytes(state_cache_bytes),
                    )
                    .expect("failed to open state storage for snapshot");

                    let root_hash = snapshot_data.root_hash;
                    let tree_height = snapshot_data.tree_height as usize;
                    let height = snapshot_data.snapshot_height;

                    // Build ADDigest (33 bytes: root_hash[32] + tree_height[1])
                    let mut version_bytes = Vec::with_capacity(33);
                    version_bytes.extend_from_slice(&root_hash);
                    version_bytes.push(snapshot_data.tree_height);
                    let version = Bytes::from(version_bytes);

                    let nodes_iter = snapshot_data
                        .nodes
                        .into_iter()
                        .map(|(label, packed)| (label, Bytes::from(packed)));

                    storage
                        .load_snapshot(nodes_iter, root_hash, tree_height, version.clone(), height)
                        .expect("failed to load snapshot into state");

                    tracing::info!("snapshot loaded, creating validator");

                    // Recompute chain's active parameters from the most recent
                    // epoch boundary at or before the snapshot height. Done
                    // BEFORE constructing the prover — `BatchAVLProver` holds
                    // !Send Rc internals, so any .await between prover creation
                    // and the val_tx.send() makes the spawned future !Send.
                    // Mirrors the resume branch in the UTXO validator block.
                    {
                        let mut chain_guard = snapshot_chain.lock().await;
                        if let Err(e) = chain_guard.recompute_active_parameters_from_storage(height)
                        {
                            tracing::warn!(
                                error = %e,
                                resume_height = height,
                                "failed to recompute active parameters from snapshot height; using current defaults"
                            );
                        } else {
                            tracing::info!(
                                resume_height = height,
                                "recomputed active blockchain parameters for snapshot bootstrap"
                            );
                        }
                    }

                    // Install the SnapshotReader so mempool/API/dump trigger see
                    // the loaded state. Done before constructing the validator so
                    // any concurrent reads land on the new DB.
                    snapshot_swap_reader.install(storage.snapshot_reader());

                    let (root, tree_h) = storage
                        .rollback(&version)
                        .expect("failed to load snapshot root from storage");

                    let resolver = storage.resolver();
                    let tree = AVLTree::with_resolver(resolver, 32, None);
                    let mut prover = BatchAVLProver::new(tree, true);
                    prover.restore_root(root, tree_h);

                    let validator = Validator::new(
                        ValidatorInner::Utxo(UtxoValidator::new(
                            storage, prover, height, checkpoint,
                        )),
                        shared_validated_height.clone(),
                        shared_state_context.clone(),
                        block_applied_tx.clone(),
                        height_watch_tx.clone(),
                        None, // TODO: mining ctx for snapshot bootstrap
                        prover_modified_nodes_bytes.clone(),
                        prover_resident_nodes_bytes.clone(),
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

    // Snapshot creation trigger — periodically dump UTXO state for serving.
    // Always spawned when configured; pulls a fresh reader from `swap_reader`
    // per iteration so the at-tip storage reopen is observable next cycle.
    if node_config.storing_snapshots > 0 && state_type == StateType::Utxo {
        if let Some(snapshot_store_for_trigger) = snapshot_store.clone() {
            let snapshot_interval = node_config.snapshot_interval;
            let storing_snapshots = node_config.storing_snapshots;
            let trigger_swap = swap_reader.clone();
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

                    let Some(reader) = trigger_swap.current() else {
                        // Mid at-tip swap or pre-bootstrap. Skip; the next
                        // boundary tick re-fetches.
                        continue;
                    };

                    let height = snapshot_height;
                    tracing::info!(height, "creating UTXO snapshot");

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
                            tracing::info!(height = h as u64, "UTXO snapshot created and stored");
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
            tracing::info!(
                snapshot_interval,
                storing_snapshots,
                "snapshot creation trigger active"
            );
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
        let mempool_swap = swap_reader.clone();
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
                            reader: mempool_swap.current(),
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
                                reader: mempool_swap.current(),
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
        let api_bind_addr: std::net::SocketAddr = node_config
            .api_address
            .as_deref()
            .unwrap_or(match network {
                enr_p2p::types::Network::Testnet => "0.0.0.0:9052",
                enr_p2p::types::Network::Mainnet => "0.0.0.0:9053",
            })
            .parse()
            .expect("invalid api_address");

        let api_chain = chain.clone();
        let api_mempool = mempool.clone();
        let api_state_ctx = shared_state_context.clone();
        let p2p_for_api = p2p.clone();
        let p2p_for_api_urls = p2p.clone();
        let p2p_for_all = p2p.clone();
        let p2p_for_status = p2p.clone();
        let p2p_for_blacklisted = p2p.clone();
        let p2p_for_connect = p2p.clone();
        let snapshot_store_for_api = snapshot_store.clone();

        // Mining task: watches shared_height for tip changes, builds
        // candidates. The generator itself was constructed pre-validator
        // (the post-apply lifecycle hook holds it); this only spawns the
        // polling worker when mining is configured.
        if let Some(ref generator) = mining_generator {
            let gen = generator.clone();
            let proof_cache = mining_proof_cache.clone();
            let mining_height = shared_validated_height.clone();
            let mining_chain = chain.clone();
            let mining_store = store.clone();
            // Read-only state access for proofs and UTXO lookups. Re-read per
            // iteration via `current()` rather than captured once, so the
            // at-tip storage reopen swaps underneath us correctly.
            let mining_swap_reader = swap_reader.clone();
            let mining_mempool = mempool.clone();
            tokio::spawn(async move {
                let mut last_height = 0u32;
                loop {
                    tokio::time::sleep(MINING_POLL_INTERVAL).await;
                    let current = mining_height.load(std::sync::atomic::Ordering::Relaxed);
                    if current == 0 {
                        continue;
                    }
                    // Rebuild on a new tip OR when the cached candidate has aged
                    // out. `candidate_ttl` (default 15s) invalidates the cache,
                    // but nothing used to regenerate on expiry — and the tip only
                    // moves every ~2 min on mainnet, so /mining/candidate served
                    // work for 15s after each block and returned 503 for the
                    // remaining ~105. The knob is documented as "maximum candidate
                    // lifetime before forced regeneration"; this is the forced
                    // regeneration half.
                    //
                    // Refreshing also keeps the candidate's timestamp current and,
                    // once mempool selection is wired in, picks up transactions
                    // that arrived after the last block rather than mining the
                    // near-empty pool left behind by it.
                    let stale = gen.cached_work(current).is_none();
                    if current == last_height && !stale {
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
                            // Candidate-aware variant: boundary_fork_vote
                            // comes from the candidate's own configured votes
                            // — the candidate header isn't in the chain, so
                            // the header-reading method would derive false
                            // and a soft-fork-voting candidate would declare
                            // a table missing its own fork-round start
                            // (self-orphan). Activated update unused here:
                            // the extension's [0x00,124] carries the
                            // PROPOSED update captured above.
                            match chain_guard.compute_expected_parameters_for_candidate(
                                candidate_height,
                                &proposed_update_bytes,
                                gen.config.votes,
                            ) {
                                Ok((p, _activated_update)) => Some(p),
                                Err(e) => {
                                    tracing::warn!(
                                        candidate_height,
                                        "mining: compute_expected_parameters_for_candidate failed: {e}"
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
                        let parent_extension_id = enr_chain::section_ids(&proof_data.parent)[2].1;
                        match mining_store.get(enr_chain::EXTENSION_TYPE_ID, &parent_extension_id) {
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

                    // Everything below used to be assembled inline here —
                    // extension, CandidateBlock, work message — which is why
                    // `generate_candidate` had no production caller and mined
                    // blocks carried the emission transaction alone. The crate
                    // owns assembly; this task supplies what only it can reach.
                    // See facts/mining.md § "Ownership".

                    let reader = match mining_swap_reader.current() {
                        Some(r) => r,
                        // Mid-swap at the at-tip storage reopen. Skipping costs
                        // one poll interval; the next iteration re-reads.
                        None => continue,
                    };

                    // Prioritised mempool transactions, with the serialized size
                    // selection bounds against.
                    let candidate_txs: Vec<(ergo_validation::Transaction, usize)> = {
                        let pool = mining_mempool.lock().await;
                        pool.all_prioritized()
                            .into_iter()
                            .map(|u| (u.tx.clone(), u.tx_bytes.len()))
                            .collect()
                    };

                    // Ancestors for the upcoming-block context, newest first,
                    // WITHOUT the parent — generate_candidate prepends it. A
                    // one-header window here would fail any script reading
                    // headers[5] and get a valid transaction evicted.
                    let (active_params, ancestor_headers) = {
                        let chain_guard = mining_chain.lock().await;
                        let params = chain_guard.active_parameters().clone();
                        let mut hs = chain_guard
                            .headers_from(proof_data.parent.height.saturating_sub(9), 10);
                        hs.reverse();
                        hs.retain(|h| h.height != proof_data.parent.height);
                        (params, hs)
                    };

                    let lookup_reader = reader.clone();
                    let utxo_lookup = move |id: &[u8; 32]| -> Option<ergo_validation::ErgoBox> {
                        let bytes = lookup_reader.lookup_key(id)?;
                        ergo_validation::deserialize_box(&bytes).ok()
                    };

                    // Proofs without the validator: sync/ owns it and it is
                    // !Sync. This reads the committed tree through the reader
                    // and builds its own prover, so it cannot disturb
                    // validation. facts/validation.md § "Free Function".
                    let proof_reader = reader.clone();
                    // Always `Some`: the Option layer means "no UTXO access at
                    // all" (digest mode), and we hold a reader by this point.
                    let validator_proofs = move |txs: &[ergo_validation::Transaction]| {
                        Some(ergo_validation::proofs_from_storage(
                            proof_reader.resolver(),
                            proof_reader.root_state(),
                            txs,
                        ))
                    };

                    match ergo_mining::generate_candidate(
                        &gen.config,
                        &proof_data.parent,
                        n_bits,
                        &parent_interlinks,
                        &proof_data.emission_box,
                        boundary_params.as_ref(),
                        &proposed_update_bytes,
                        &candidate_txs,
                        &active_params,
                        &ancestor_headers,
                        &utxo_lookup,
                        &validator_proofs,
                    ) {
                        Ok(generated) => {
                            // Step 3.6: the crate identifies unusable
                            // transactions and cannot remove them itself.
                            // Dropping these on the floor silently re-selects
                            // and re-rejects them every 15s forever.
                            if !generated.invalid_txs.is_empty() {
                                let mut pool = mining_mempool.lock().await;
                                for id in &generated.invalid_txs {
                                    pool.invalidate(id);
                                }
                                tracing::debug!(
                                    count = generated.invalid_txs.len(),
                                    "mining: evicted transactions rejected during selection"
                                );
                            }
                            let tx_count = generated.block.transactions.len();
                            gen.cache_candidate(generated.block, generated.work, current);
                            tracing::debug!(
                                height = current + 1,
                                transactions = tx_count,
                                "mining candidate cached"
                            );
                        }
                        Err(e) => {
                            tracing::warn!("mining: candidate generation failed: {e}");
                        }
                    }
                }
            });
            tracing::info!("mining task started");
        }

        let api_state = ergo_api::ApiState {
            chain: Arc::new(HeaderChainAdapter { chain: api_chain }),
            store: Arc::new(StoreAdapter { store: api_store }),
            mempool: api_mempool,
            utxo_reader: Arc::new(ApiUtxoReader {
                swap_reader: swap_reader.clone(),
            }),
            state_context: api_state_ctx,
            peer_count: Arc::new(move || {
                let count = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(p2p_for_api.peer_count())
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
            // Memory gauges. Views over the same storage the producers write:
            // `Validator` for the two prover figures every
            // PROVER_GAUGE_INTERVAL_BLOCKS, `HeaderSync` for the window after
            // each applied block. Never written = absent JSON key, so digest
            // mode omits the prover fields rather than claiming an empty one.
            prover_modified_nodes_bytes: Arc::new(ergo_api::PublishedGauge::from_storage(
                prover_modified_nodes_bytes.clone(),
            )),
            prover_resident_nodes_bytes: Arc::new(ergo_api::PublishedGauge::from_storage(
                prover_resident_nodes_bytes.clone(),
            )),
            sync_window_bytes: Arc::new(ergo_api::PublishedGauge::from_storage(
                shared_window_bytes.clone(),
            )),
            // Same atomic the fastsync gap decision reads — sync maintains it
            // as a monotonic max over every peer's SyncInfo. Advisory only.
            max_peer_height: peer_chain_tip.clone(),
            peer_api_urls: Arc::new(move || {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(p2p_for_api_urls.peer_rest_urls())
                })
                .into_iter()
                .map(|(peer_id, addr, rest_url)| ergo_api::PeerRestInfo {
                    peer_id: peer_id.0,
                    addr,
                    rest_url,
                })
                .collect()
            }),
            peer_all: Arc::new(move || {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(p2p_for_all.all_peers())
                })
                .into_iter()
                .map(|entry| ergo_api::PeerInfo {
                    address: entry.address,
                    name: entry.agent_name,
                    last_seen: entry.last_seen_ms,
                    connection_type: entry.connection_type.map(|ct| match ct {
                        enr_p2p::types::ConnectionType::Outgoing => "Outgoing".to_string(),
                        enr_p2p::types::ConnectionType::Incoming => "Incoming".to_string(),
                    }),
                })
                .collect()
            }),
            peer_status: Arc::new(move || {
                let status = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(p2p_for_status.network_status())
                });
                ergo_api::PeerStatusSummary {
                    last_incoming_message: status.last_incoming_message_ms,
                    current_network_time: status.current_network_time_ms,
                }
            }),
            peer_blacklisted: Arc::new(move || {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(p2p_for_blacklisted.blacklisted_peers())
                })
            }),
            peer_connect: Arc::new(move |addr| {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(p2p_for_connect.queue_outbound_connection(addr))
                })
            }),
            snapshots_info: Arc::new(move || match &snapshot_store_for_api {
                Some(store) => store
                    .snapshots_info()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(height, digest)| ergo_api::SnapshotInfoEntry { height, digest })
                    .collect(),
                None => Vec::new(),
            }),
            api_key_hash: None,
            modifier_tx: Some(modifier_tx_for_mining.clone()),
            height_watch: height_watch_rx,
            jemalloc_probe: {
                #[cfg(feature = "jemalloc")]
                {
                    Some(Arc::new(|| {
                        let _ = tikv_jemalloc_ctl::epoch::advance();
                        ergo_api::JemallocSnapshot {
                            allocated: tikv_jemalloc_ctl::stats::allocated::read().unwrap_or(0)
                                as u64,
                            active: tikv_jemalloc_ctl::stats::active::read().unwrap_or(0) as u64,
                            resident: tikv_jemalloc_ctl::stats::resident::read().unwrap_or(0)
                                as u64,
                            retained: tikv_jemalloc_ctl::stats::retained::read().unwrap_or(0)
                                as u64,
                            metadata: tikv_jemalloc_ctl::stats::metadata::read().unwrap_or(0)
                                as u64,
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
                launch_time: launch_time_ms,
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
            stats_enabled: stats_config.is_some(),
            // capture is Some(...) when [debug.p2p_capture] is configured
            // with enabled=true; None otherwise. See facts/p2p-capture.md.
            capture: capture_access.clone(),
        };

        let api_stats_config = stats_config.as_ref().map(|c| ergo_api::stats::StatsConfig {
            bind_address: c.bind_address,
        });
        let api_p2p_counters: Option<Arc<dyn ergo_api::stats::P2pCountersSource>> =
            stats_config.as_ref().map(|_| {
                Arc::new(P2pCountersAdapter { node: p2p.clone() })
                    as Arc<dyn ergo_api::stats::P2pCountersSource>
            });

        tokio::spawn(async move {
            if let Err(e) =
                ergo_api::serve(api_state, api_bind_addr, api_stats_config, api_p2p_counters).await
            {
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
        let fastsync_peer_wait =
            std::time::Duration::from_secs(node_config.fastsync_peer_wait_timeout_sec);
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
                    peer_tip,
                    downloaded,
                    gap,
                    threshold = fastsync_threshold,
                    "gap at/below fastsync threshold — going straight to P2P"
                );
                bootstrap_gate.store(true, Ordering::Relaxed);
                return;
            }

            tracing::info!(
                peer_tip,
                downloaded,
                gap,
                threshold = fastsync_threshold,
                "gap exceeds fastsync threshold — spawning fastsync"
            );
            let node_url = format!("http://127.0.0.1:{api_port}");
            let mut cmd = tokio::process::Command::new("ergo-fastsync");
            cmd.arg("--node-url").arg(&node_url);
            cmd.arg("--handoff-distance")
                .arg(fastsync_threshold.to_string());
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

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Ergo node running");

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

    // Signal sync to exit. The oneshot is the only deterministic
    // shutdown signal: P2pTransport holds an Arc<P2pNode> clone (and
    // mining/API/mempool hold more), so dropping our own reference
    // can't close sync's events channel. See facts/sync.md
    // "Graceful shutdown".
    if shutdown_tx.send(()).is_err() {
        tracing::info!("sync task already exited before shutdown signal");
    }

    // Await sync's completion with a bounded timeout. Hitting the
    // timeout is an error condition — sync should normally complete its
    // flush in tens of milliseconds, even at tip. We log and proceed
    // rather than blocking process exit indefinitely.
    match tokio::time::timeout(SHUTDOWN_GRACE, sync_handle).await {
        Ok(Ok(())) => {
            tracing::info!("sync task exited cleanly");
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "sync task panicked during shutdown");
        }
        Err(_) => {
            tracing::error!(
                timeout_secs = SHUTDOWN_GRACE.as_secs(),
                "sync task did not complete within shutdown timeout; state may be incomplete"
            );
        }
    }

    // Drop the P2P node after sync has exited — sync was the only
    // consumer that needed the node alive for shutdown ordering.
    drop(p2p);

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
    fn cache_store_pct_out_of_range_is_rejected() {
        for pct in [0u32, 100, 250] {
            let cfg = NodeConfig {
                cache_store_pct: Some(pct),
                ..NodeConfig::default()
            };
            assert!(
                validate_cache_split(&cfg).is_err(),
                "cache_store_pct {pct} should be rejected — a zero share means a \
                 database with no page cache at all"
            );
        }
        for pct in [1u32, 50, 99] {
            let cfg = NodeConfig {
                cache_store_pct: Some(pct),
                ..NodeConfig::default()
            };
            assert!(
                validate_cache_split(&cfg).is_ok(),
                "pct {pct} should be accepted"
            );
        }
    }

    /// Build a `Validator` around either variant, with throwaway channels.
    fn wrap(inner: ValidatorInner) -> Validator {
        let (block_tx, _rx) = tokio::sync::mpsc::channel(1);
        let (height_tx, _hrx) = tokio::sync::watch::channel(0u32);
        Validator::new(
            inner,
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            Arc::new(tokio::sync::RwLock::new(None)),
            block_tx,
            height_tx,
            None,
            ergo_api::PublishedGauge::unset().storage(),
            ergo_api::PublishedGauge::unset().storage(),
        )
    }

    #[test]
    fn the_wrapper_hands_out_utxo_persistence_and_withholds_digest_persistence() {
        // A missing forward here does NOT break visibly. `state_persistence`
        // returning None for both arms compiles, and `sync/` reads None as
        // "nothing to persist" — so the node simply stops flushing and loses
        // state on the next unclean shutdown. The compiler forces the method
        // to exist, never to be right.
        //
        // BOTH arms are asserted deliberately: a test that only checked the
        // digest side would pass with both arms wrongly returning None, which
        // is precisely the bug it is meant to catch.

        let digest = wrap(ValidatorInner::Digest(DigestValidator::new(
            ergo_chain_types::ADDigest::zero(),
            0,
        )));
        assert!(
            digest.state_persistence().is_none(),
            "digest mode owns no redb and must hand out no persistence"
        );
        assert!(
            digest.mining_state().is_none(),
            "digest mode has no UTXO set to assemble candidates from"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let params = AVLTreeParams {
            key_length: 32,
            value_length: None,
        };
        let mut storage = RedbAVLStorage::open(
            &dir.path().join("state.redb"),
            params,
            16,
            CacheSize::Bytes(1024 * 1024),
        )
        .expect("open state storage");

        // Establish a version so UtxoValidator::new's precondition holds —
        // the empty first commit documented on that constructor.
        let mut prover =
            BatchAVLProver::new(AVLTree::with_resolver(storage.resolver(), 32, None), true);
        storage
            .update_with_height(&mut prover, vec![], 0)
            .expect("empty first commit");

        let utxo = wrap(ValidatorInner::Utxo(UtxoValidator::new(
            storage, prover, 0, 0,
        )));
        assert!(
            utxo.state_persistence().is_some(),
            "UTXO mode owns state.redb — handing out None here silently stops \
             every flush in the node"
        );
        assert!(
            utxo.mining_state().is_some(),
            "UTXO mode can assemble candidates and must expose MiningState"
        );
    }

    fn budget(usable_mb: u64) -> MemoryBudget {
        MemoryBudget {
            source: BudgetSource::Explicit,
            ceiling_bytes: usable_mb * MIB,
            usable_bytes: usable_mb * MIB,
        }
    }

    #[test]
    fn an_absent_key_is_derived_and_a_present_one_is_obeyed_exactly() {
        let b = budget(4096);

        let derived = derive_memory_plan(&NodeConfig::default(), &b, 0);
        assert!(
            derived.cache_mb > 0,
            "absent cache_mb must derive a real size"
        );
        assert!(derived.derived_any);

        let stated = NodeConfig {
            cache_mb: Some(777),
            ..NodeConfig::default()
        };
        let plan = derive_memory_plan(&stated, &b, 0);
        assert_eq!(
            plan.cache_mb, 777,
            "a stated value must be obeyed exactly, not adjusted — silently \
             overriding it is the failure this design exists to prevent"
        );
        assert_ne!(
            plan.flush_heap_threshold_mb, 0,
            "stating one key must not disable derivation of the others"
        );
    }

    #[test]
    fn the_flush_trigger_always_sits_above_the_cache_it_must_outlive() {
        // The trigger compares against total jemalloc `allocated`, which
        // already contains the caches. A threshold at or below cache size
        // fires on every check forever — a pathology that produces a node
        // that runs and flushes constantly rather than one that fails.
        for usable_mb in [512u64, 1024, 2048, 4096, 16384] {
            let plan = derive_memory_plan(&NodeConfig::default(), &budget(usable_mb), 0);
            assert!(
                plan.flush_heap_threshold_mb > plan.cache_mb,
                "usable {usable_mb} MB: flush trigger {} must exceed cache {}",
                plan.flush_heap_threshold_mb,
                plan.cache_mb
            );
        }
    }

    #[test]
    fn a_budget_smaller_than_the_floor_still_yields_a_usable_cache() {
        // 128 MB is below BASELINE_ANON_BYTES, so `available` saturates to
        // zero. Deriving a zero-byte cache would be worse than a small one:
        // redb with no page cache makes the startup chain walk ~70x slower.
        let plan = derive_memory_plan(&NodeConfig::default(), &budget(128), 0);
        assert!(
            plan.cache_mb >= 64,
            "a starved budget must still floor at a usable cache, got {}",
            plan.cache_mb
        );
    }

    #[test]
    fn the_growing_chain_index_shrinks_the_derived_cache() {
        let b = budget(4096);
        let early = derive_memory_plan(&NodeConfig::default(), &b, 10 * MIB);
        let late = derive_memory_plan(&NodeConfig::default(), &b, 400 * MIB);
        assert!(
            late.cache_mb < early.cache_mb,
            "the index is part of the floor and grows with the chain, so the \
             cache derived against it must shrink: early {} late {}",
            early.cache_mb,
            late.cache_mb
        );
    }

    #[test]
    fn budget_source_decides_how_much_of_the_ceiling_is_spent() {
        // A cgroup limit is somebody stating this node may have that much.
        // MemTotal states only that the machine has it.
        assert_eq!(BudgetSource::Explicit.usable_fraction(), 1.00);
        assert!(BudgetSource::Cgroup.usable_fraction() > BudgetSource::MemTotal.usable_fraction());
    }

    /// A plan with only the two fields the split reads. Built directly rather
    /// than through derivation so these tests keep asserting the split's
    /// arithmetic and not the budget calibration, which moves.
    fn plan_for_test(cache_mb: u64, cache_store_pct: u32) -> MemoryPlan {
        MemoryPlan {
            cache_mb,
            cache_store_pct,
            flush_heap_threshold_mb: 0,
            flush_max_blocks: 0,
            flush_min_blocks: 0,
            synced_cache_mb: None,
            synced_flush_heap_threshold_mb: None,
            synced_flush_max_blocks: None,
            synced_flush_min_blocks: None,
            derived_any: false,
        }
    }

    #[test]
    fn cache_split_divides_the_total_exactly() {
        let plan = plan_for_test(512, 25);
        let (store, state) = cache_split_bytes(&plan);
        assert_eq!(store, 128 * 1024 * 1024);
        assert_eq!(state, 384 * 1024 * 1024);
        assert_eq!(
            store + state,
            512 * 1024 * 1024,
            "the split must sum to cache_mb exactly"
        );
    }

    #[test]
    fn cache_split_rounding_loses_no_bytes() {
        // 333 is deliberately awkward: 777 MB * 33% does not divide evenly.
        // The remainder must land in state rather than vanishing, or the
        // configured total silently under-delivers.
        let plan = plan_for_test(777, 33);
        let (store, state) = cache_split_bytes(&plan);
        assert_eq!(store + state, 777 * 1024 * 1024);
    }

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
            assert_eq!(hex::encode(id), expected_ids[i], "box {} ID mismatch", i);
        }

        // Insert into AVL+ tree and verify genesis state digest
        let resolver: ergo_avltree_rust::batch_node::Resolver =
            Arc::new(|digest: &[u8; 32]| Node::LabelOnly(NodeHeader::new(Some(*digest), None)));
        let tree = AVLTree::with_resolver(resolver, 32, None);
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
            assert_eq!(hex::encode(id), expected_ids[i], "box {} ID mismatch", i);
        }

        // Insert into AVL+ tree and verify genesis state digest
        let resolver: ergo_avltree_rust::batch_node::Resolver =
            Arc::new(|digest: &[u8; 32]| Node::LabelOnly(NodeHeader::new(Some(*digest), None)));
        let tree = AVLTree::with_resolver(resolver, 32, None);
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

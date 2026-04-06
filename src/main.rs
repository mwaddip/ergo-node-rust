use std::sync::Arc;

use bytes::Bytes;
use enr_chain::{ChainConfig, HeaderChain, StateType, HEADER_TYPE_ID};
use enr_state::{AVLTreeParams, RedbAVLStorage, SnapshotReader};
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::AVLTree;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::persistent_batch_avl_prover::PersistentBatchAVLProver;
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
use ergo_validation::{BlockValidator, DigestValidator, UtxoValidator, ValidationError};
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

/// Testnet founders' public keys (hex-encoded compressed EC points).
const TESTNET_FOUNDERS_PKS: &[&str] = &[
    "039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647",
    "031fb52cf6e805f80d97cde289f4f757d49accf0c83fb864b27d2cf982c37f9a8b",
    "0352ac2a471339b0d23b3d2c5ce0db0e81c969f77891b9edf0bda7fd39a78184e7",
];

/// Construct the 3 genesis UTXO boxes from chain parameters.
///
/// Returns (box_id, sigma_serialized_bytes) for each box.
/// Uses ergo-lib's genesis module — ErgoTree scripts are built from IR,
/// not hardcoded hex.
fn build_genesis_boxes(network: enr_p2p::types::Network) -> Vec<([u8; 32], Vec<u8>)> {
    let settings = MonetarySettings::default();

    let (proof_strings, founder_pk_hexes) = match network {
        enr_p2p::types::Network::Testnet => (TESTNET_NO_PREMINE_PROOFS, TESTNET_FOUNDERS_PKS),
        enr_p2p::types::Network::Mainnet => {
            unimplemented!("mainnet genesis not yet implemented")
        }
    };

    let founder_pks: Vec<ProveDlog> = founder_pk_hexes
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

/// Pre-computed state proofs for mining candidate generation.
/// Written by the validator after each block, read by the mining task.
#[derive(Clone)]
struct MiningProofData {
    parent: ergo_chain_types::Header,
    emission_box: ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox,
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
        mining: Option<MiningCtx>,
    ) -> Self {
        let h = match &inner {
            ValidatorInner::Digest(v) => v.validated_height(),
            ValidatorInner::Utxo(v) => v.validated_height(),
        };
        shared_height.store(h, std::sync::atomic::Ordering::Relaxed);
        Self { inner, shared_height, shared_state_context, block_applied_tx, mining }
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

        let reemission = ergo_mining::emission::ReemissionRules::mainnet();
        let emission_tx = match ergo_mining::emission::build_emission_tx(
            &emission_box,
            next_height,
            &mining.config.miner_pk,
            mining.config.reward_delay,
            &reemission,
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

        let mut guard = mining.proof_cache.lock().unwrap();
        *guard = Some(MiningProofData {
            parent: header.clone(),
            emission_box,
            ad_proof_bytes,
            state_root,
            emission_tx,
            tip_height: header.height,
        });
    }
}

impl BlockValidator for Validator {
    fn validate_block(
        &mut self,
        header: &ergo_chain_types::Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[ergo_chain_types::Header],
    ) -> Result<(), ValidationError> {
        let result = match &mut self.inner {
            ValidatorInner::Digest(v) => v.validate_block(header, block_txs, ad_proofs, extension, preceding_headers),
            ValidatorInner::Utxo(v) => v.validate_block(header, block_txs, ad_proofs, extension, preceding_headers),
        };
        if result.is_ok() {
            self.shared_height.store(self.validated_height(), std::sync::atomic::Ordering::Relaxed);

            // Publish state context for mempool/API transaction validation.
            // Only when we have preceding headers (height > 0).
            if !preceding_headers.is_empty() {
                let ctx = ergo_validation::build_state_context(
                    header,
                    preceding_headers,
                    self.parameters(),
                );
                // Block on the write lock — we're in the sync task's single thread,
                // and this is a fast in-memory swap.
                *self.shared_state_context.blocking_write() = Some(ctx);
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
        self.shared_height.store(self.validated_height(), std::sync::atomic::Ordering::Relaxed);
    }

    fn parameters(&self) -> &ergo_validation::Parameters {
        match &self.inner {
            ValidatorInner::Digest(v) => v.parameters(),
            ValidatorInner::Utxo(v) => v.parameters(),
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
        self.with_chain(|c| c.header_at(height).cloned())
    }
    fn header_by_id(&self, id: &[u8; 32]) -> Option<ergo_chain_types::Header> {
        self.with_chain(|c| {
            // Linear scan — headers are indexed by height, not by ID.
            // For the API this is acceptable; optimize later if needed.
            for h in (0..=c.height()).rev() {
                if let Some(header) = c.header_at(h) {
                    let header_id: &[u8] = header.id.0.as_ref();
                    if header_id == id {
                        return Some(header.clone());
                    }
                }
            }
            None
        })
    }
    fn tip(&self) -> ergo_chain_types::Header {
        self.with_chain(|c| c.tip().clone())
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

/// Top-level config wrapper — just the [node] section, P2P is parsed separately.
#[derive(Debug, Deserialize)]
struct RootConfig {
    #[serde(default)]
    node: Option<NodeConfig>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
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
        other => {
            return Err(format!("unknown state_type '{}' (expected 'utxo' or 'digest')", other).into());
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
        let v = hex::decode(&node_config.mining.votes)
            .unwrap_or_else(|_| vec![0, 0, 0]);
        [v.get(0).copied().unwrap_or(0), v.get(1).copied().unwrap_or(0), v.get(2).copied().unwrap_or(0)]
    };

    // Mining proof cache — shared between the validator callback and the mining task
    let mining_proof_cache: MiningProofCache = Arc::new(std::sync::Mutex::new(None));

    if let Some(ref pk) = miner_pk_opt {
        let pk_hex: String = (*pk.h).clone().into();
        tracing::info!(miner_pk = %pk_hex, votes = %node_config.mining.votes, "mining configured");
    }

    let data_dir = std::path::PathBuf::from(node_config.data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(RedbModifierStore::new(&data_dir.join("modifiers.redb"))?);

    // Shared header chain: pipeline writes, sync reads
    let mut chain = HeaderChain::new(chain_config);

    // Restore chain from stored headers (using best chain index)
    if let Some((tip_height, _)) = store.best_header_tip()? {
        let mut loaded = 0u32;
        for height in 1..=tip_height {
            let id = match store.best_header_at(height)? {
                Some(id) => id,
                None => {
                    tracing::warn!(height, "gap in best chain, stopping load");
                    break;
                }
            };
            let data = match store.get(HEADER_TYPE_ID, &id)? {
                Some(d) => d,
                None => {
                    tracing::warn!(height, "stored header ID but no data, stopping load");
                    break;
                }
            };
            let header = match enr_chain::parse_header(&data) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(height, "stored header parse failed: {e}, stopping load");
                    break;
                }
            };
            match chain.try_append(header) {
                Ok(enr_chain::AppendResult::Extended) => {}
                Ok(enr_chain::AppendResult::Forked { .. }) => {
                    tracing::error!(height, "stored best-chain header detected as fork — store corrupted?");
                    break;
                }
                Err(e) => {
                    tracing::error!(height, "stored header chain failed: {e}, stopping load");
                    break;
                }
            }
            loaded += 1;
        }
        tracing::info!(loaded, tip = chain.height(), "restored header chain from store");
    }

    let chain = Arc::new(Mutex::new(chain));

    // Modifier channel — P2P produces, pipeline consumes
    let (modifier_tx, modifier_rx) = tokio::sync::mpsc::channel(4096);

    // Grab network settings before P2P takes ownership of config
    let net_settings = config.network_settings();

    // Build Mode feature from node config — tells peers what we can serve
    let mode_config = enr_p2p::transport::handshake::ModeConfig {
        state_type_id: match state_type {
            StateType::Utxo => 0,
            StateType::Digest => 1,
        },
        verifying: verify_transactions,
        blocks_to_keep: blocks_to_keep as i32,
    };

    // Start P2P with modifier sink (no validator)
    let p2p = Arc::new(enr_p2p::node::P2pNode::start(config, Some(modifier_tx), mode_config).await?);

    // Validation pipeline — progress channel feeds sync, delivery channel feeds tracker
    let pipeline_chain = chain.clone();
    let api_store = store.clone(); // for REST API block queries
    let sync_store = SharedStore::new(store.clone());
    let revalidate_store = store.clone(); // for section scan during revalidation
    let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(4);
    // Control channel: unbounded — Reorg/NeedModifier must never be dropped
    let (delivery_control_tx, delivery_control_rx) = tokio::sync::mpsc::unbounded_channel();
    // Data channel: bounded — Received/Evicted are lossy, ok to drop
    let (delivery_data_tx, delivery_data_rx) = tokio::sync::mpsc::channel(64);
    // Transaction channel — pipeline forwards unconfirmed txs to mempool task
    let (tx_tx, tx_rx) = tokio::sync::mpsc::channel::<([u8; 32], Vec<u8>)>(256);

    tokio::spawn(async move {
        let mut pipeline =
            ValidationPipeline::new(modifier_rx, pipeline_chain, store, progress_tx, delivery_control_tx, delivery_data_tx);
        pipeline.set_tx_sender(tx_tx);
        pipeline.run().await;
    });

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

    // Event demux: intercept snapshot serving requests (76/78/80), forward rest to sync
    let (sync_events_tx, sync_events_rx) = tokio::sync::mpsc::channel(256);
    {
        let snapshot_store = snapshot_store.clone();
        let p2p_serve = p2p.clone();
        tokio::spawn(async move {
            let mut events = raw_events;
            while let Some(event) = events.recv().await {
                let handled = if let enr_p2p::protocol::peer::ProtocolEvent::Message {
                    peer_id,
                    message: enr_p2p::protocol::messages::ProtocolMessage::Unknown { code, ref body },
                } = event
                {
                    if let Some(ref store) = snapshot_store {
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
                    }
                } else {
                    false
                };

                if !handled {
                    if sync_events_tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
        });
    }

    // Bridge implementations
    let transport = P2pTransport::new(p2p.clone(), sync_events_rx);
    let sync_chain = SharedChain::new(chain.clone());

    // Genesis state root — needed for fresh start or revalidation
    let genesis_digest_hex = match network {
        enr_p2p::types::Network::Testnet =>
            "cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502",
        enr_p2p::types::Network::Mainnet =>
            "a5df145d41ab15a01e0cd3ffbab046f0d029e5412293072ad0f5827428589b9302",
    };
    let genesis_bytes = hex::decode(genesis_digest_hex).expect("invalid genesis digest hex");
    let genesis_digest = ADDigest::try_from(genesis_bytes.as_slice())
        .expect("invalid genesis digest length");

    let utxo_bootstrap = node_config.utxo_bootstrap;
    let min_snapshot_peers = node_config.min_snapshot_peers;
    let shared_validated_height = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let shared_state_context: Arc<tokio::sync::RwLock<Option<ergo_validation::ErgoStateContext>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    let (block_applied_tx, block_applied_rx) =
        tokio::sync::mpsc::channel::<Vec<ergo_validation::Transaction>>(64);
    let chain_guard = chain.lock().await;

    let mut snapshot_reader: Option<SnapshotReader> = None;

    let validator: Option<Validator> = match state_type {
        StateType::Utxo => {
            let state_path = data_dir.join("state.redb");
            let params = AVLTreeParams { key_length: 32, value_length: None };
            let keep_versions = 200u32;
            let storage = RedbAVLStorage::open(&state_path, params, keep_versions)
                .expect("failed to open UTXO state storage");

            let checkpoint = configured_checkpoint.unwrap_or(0);

            if storage.version().is_some() {
                // Create snapshot reader BEFORE prover consumes storage
                snapshot_reader = Some(storage.snapshot_reader());
                let resolver = storage.resolver();
                let tree = AVLTree::new(resolver, 32, None);
                let prover = BatchAVLProver::new(tree, true);
                let persistent_prover = PersistentBatchAVLProver::new(prover, Box::new(storage), vec![])
                    .expect("failed to create persistent prover from stored state");

                // Find the actual validated height by matching the prover's
                // current digest against header state roots. The chain height
                // may be far ahead of the validated height during sync.
                let prover_digest = persistent_prover.digest();
                let prover_digest_arr: [u8; 33] = prover_digest.as_ref().try_into()
                    .expect("prover digest should be 33 bytes");
                let chain_height = chain_guard.height();
                let mut height = 0u32;
                for h in (0..=chain_height).rev() {
                    if h == 0 {
                        // Genesis state root
                        let genesis_root: [u8; 33] = genesis_digest.into();
                        if prover_digest_arr == genesis_root {
                            height = 0;
                            break;
                        }
                    } else if let Some(header) = chain_guard.header_at(h) {
                        let header_root: [u8; 33] = header.state_root.into();
                        if prover_digest_arr == header_root {
                            height = h;
                            break;
                        }
                    }
                }
                tracing::info!(height, chain_height, checkpoint, "block validator resuming (UTXO mode)");
                let mining_ctx = miner_pk_opt.as_ref().and_then(|pk| {
                    snapshot_reader.as_ref().map(|sr| MiningCtx {
                        config: ergo_mining::MinerConfig {
                            miner_pk: pk.clone(),
                            reward_delay: node_config.mining.reward_delay,
                            votes: miner_votes,
                            candidate_ttl: std::time::Duration::from_secs(node_config.mining.candidate_ttl_secs),
                        },
                        proof_cache: mining_proof_cache.clone(),
                        snapshot_reader: Arc::new(sr.clone()),
                    })
                });
                Some(Validator::new(
                    ValidatorInner::Utxo(UtxoValidator::new(persistent_prover, height, checkpoint)),
                    shared_validated_height.clone(),
                    shared_state_context.clone(),
                    block_applied_tx.clone(),
                    mining_ctx,
                ))
            } else if utxo_bootstrap {
                // Snapshot bootstrap — validator will be created after snapshot download
                tracing::info!("UTXO state empty, will bootstrap from peer snapshot");
                None
            } else {
                // Create snapshot reader BEFORE prover consumes storage
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

                let persistent_prover = PersistentBatchAVLProver::new(prover, Box::new(storage), vec![])
                    .expect("failed to create persistent prover from genesis");

                // Verify genesis digest matches expected
                let actual = persistent_prover.digest();
                let expected: [u8; 33] = genesis_digest.into();
                assert_eq!(
                    actual.as_ref(), &expected[..],
                    "genesis UTXO state digest mismatch"
                );

                tracing::info!(checkpoint, "block validator starting from genesis (UTXO mode)");
                let mining_ctx = miner_pk_opt.as_ref().and_then(|pk| {
                    snapshot_reader.as_ref().map(|sr| MiningCtx {
                        config: ergo_mining::MinerConfig {
                            miner_pk: pk.clone(),
                            reward_delay: node_config.mining.reward_delay,
                            votes: miner_votes,
                            candidate_ttl: std::time::Duration::from_secs(node_config.mining.candidate_ttl_secs),
                        },
                        proof_cache: mining_proof_cache.clone(),
                        snapshot_reader: Arc::new(sr.clone()),
                    })
                });
                Some(Validator::new(
                    ValidatorInner::Utxo(UtxoValidator::new(persistent_prover, 0, checkpoint)),
                    shared_validated_height.clone(),
                    shared_state_context.clone(),
                    block_applied_tx.clone(),
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
                None, // mining requires UTXO mode
            ))
        }
    };
    drop(chain_guard);

    // Build sync config from P2P network settings
    let net = net_settings;
    let sync_config = SyncConfig {
        delivery_timeout: std::time::Duration::from_secs(net.delivery_timeout_secs),
        max_delivery_checks: net.max_delivery_checks,
        state_type,
        utxo_bootstrap,
        min_snapshot_peers,
        data_dir: data_dir.clone(),
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
    tokio::spawn(async move {
        let mut sync = HeaderSync::new(
            sync_config, transport, sync_chain, sync_store, validator,
            progress_rx, delivery_control_rx, delivery_data_rx,
            snapshot_tx, validator_rx,
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
                    let mut storage = RedbAVLStorage::open(&state_path, params, 200)
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

                    storage.load_snapshot(nodes_iter, root_hash, tree_height, version)
                        .expect("failed to load snapshot into state");

                    tracing::info!("snapshot loaded, creating validator");

                    // Create validator from loaded state
                    let resolver = storage.resolver();
                    let tree = AVLTree::new(resolver, 32, None);
                    let prover = BatchAVLProver::new(tree, true);
                    let persistent = PersistentBatchAVLProver::new(
                        prover, Box::new(storage), vec![],
                    ).expect("failed to create prover from snapshot state");

                    let validator = Validator::new(
                        ValidatorInner::Utxo(UtxoValidator::new(persistent, height, checkpoint)),
                        shared_validated_height.clone(),
                        shared_state_context.clone(),
                        block_applied_tx.clone(),
                        None, // TODO: mining ctx for snapshot bootstrap
                    );
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
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                    // Use the actual validated height, not the chain (header) height.
                    // The validator updates this atomic after each successful block.
                    let validated = shared_height.load(std::sync::atomic::Ordering::Relaxed);
                    if validated == 0 {
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
        let mut cleanup_interval = tokio::time::interval(std::time::Duration::from_secs(30));
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

        // Mining: construct CandidateGenerator + mining task if configured
        let mining_generator: Option<Arc<ergo_mining::CandidateGenerator>> =
            if let Some(ref pk) = miner_pk_opt {
                if state_type == StateType::Utxo {
                    let generator = Arc::new(ergo_mining::CandidateGenerator::new(
                        ergo_mining::MinerConfig {
                            miner_pk: pk.clone(),
                            reward_delay: node_config.mining.reward_delay,
                            votes: miner_votes,
                            candidate_ttl: std::time::Duration::from_secs(node_config.mining.candidate_ttl_secs),
                        },
                    ));

                    // Mining task: watches shared_height for tip changes, builds candidates
                    let gen = generator.clone();
                    let proof_cache = mining_proof_cache.clone();
                    let mining_height = shared_validated_height.clone();
                    let mining_chain = chain.clone();
                    tokio::spawn(async move {
                        let mut last_height = 0u32;
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let current = mining_height.load(std::sync::atomic::Ordering::Relaxed);
                            if current == last_height || current == 0 {
                                continue;
                            }
                            last_height = current;

                            // Read pre-computed proofs from the validator callback
                            let proof_data = {
                                let guard = proof_cache.lock().unwrap();
                                guard.clone()
                            };

                            let proof_data = match proof_data {
                                Some(d) if d.tip_height == current => d,
                                _ => continue, // proofs not ready yet
                            };

                            // Get n_bits for next block from chain
                            let chain_guard = mining_chain.lock().await;
                            let n_bits = chain_guard.tip().n_bits; // same as current for first release
                            drop(chain_guard);

                            // Build extension + header + WorkMessage
                            let extension = match ergo_mining::extension::build_extension(
                                &proof_data.parent,
                                &[], // interlinks tracking deferred
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
            mining: mining_generator,
            node_info: ergo_api::NodeMeta {
                name: "ergo-node-rust".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                network: match network {
                    enr_p2p::types::Network::Testnet => "testnet".to_string(),
                    enr_p2p::types::Network::Mainnet => "mainnet".to_string(),
                },
                state_type: match state_type {
                    StateType::Utxo => "utxo".to_string(),
                    StateType::Digest => "digest".to_string(),
                },
            },
        };

        tokio::spawn(async move {
            if let Err(e) = ergo_api::serve(api_state, api_bind_addr).await {
                tracing::error!("REST API server failed: {e}");
            }
        });
    }

    tracing::info!("Ergo node running");

    // Run until interrupted
    tokio::signal::ctrl_c().await?;

    let height = chain.lock().await.height();
    let peers = p2p.peer_count().await;
    tracing::info!(chain_height = height, peers, "Shutting down");

    // Drop P2P node to close event streams, triggering task shutdown.
    // The pipeline exits when its modifier channel closes (sender dropped
    // with the P2P node). The sync task exits when its event stream ends.
    drop(p2p);
    // Brief grace period for tasks to finish in-flight work
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

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
        let expected_hex = "cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502";
        assert_eq!(
            hex::encode(&digest),
            expected_hex,
            "genesis state digest mismatch"
        );
    }
}

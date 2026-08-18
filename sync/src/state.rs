use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::apply_state_error::classify_apply_state_error;
use crate::delivery::{DeliveryControl, DeliveryData, DeliveryTracker};
use crate::sweep_backoff::StallDetail;
use enr_chain::{
    BlockId, StateType, SyncInfo, AD_PROOFS_TYPE_ID, BLOCK_TRANSACTIONS_TYPE_ID, EXTENSION_TYPE_ID,
    HEADER_TYPE_ID, TRANSACTION_TYPE_ID,
};

use crate::traits::{SyncChain, SyncStore, SyncTransport};
use ergo_validation::BlockValidator;

/// Number of block section requests to send per batch (per type).
/// The JVM's Akka layer can silently drop large ModifierResponse bodies via
/// backpressure, so this is capped below the JVM's `desiredInvObjects` (400).
/// 64 per type × 2 types = 128 sections per cycle = 64 blocks/cycle.
fn is_block_section_type(type_id: u8) -> bool {
    matches!(
        type_id,
        BLOCK_TRANSACTIONS_TYPE_ID | AD_PROOFS_TYPE_ID | EXTENSION_TYPE_ID
    )
}

/// Max continuation header ids served to a behind/forked peer per incoming
/// SyncInfo (JVM `processSyncV1`/`V2`: `continuationIds(syncInfo, size = 400)`).
const CONTINUATION_IDS_LIMIT: usize = 400;

/// Anchor ids from a parsed SyncInfo, newest first — the order
/// `SyncChain::continuation_ids` expects. V2 headers arrive tip-first on the
/// wire; V1 ids arrive oldest-first (JVM `lastHeaderIds` convention) and are
/// reversed here.
fn sync_info_anchor_ids(info: &SyncInfo) -> Vec<BlockId> {
    match info {
        SyncInfo::V2 { headers } => headers.iter().map(|h| h.id).collect(),
        SyncInfo::V1 { header_ids } => header_ids.iter().rev().copied().collect(),
    }
}

/// Result of a paired state/store flush.
///
/// Discriminates four cases:
/// - `Flushed(M)`: validator flushed successfully at height `M`; modifier
///   store's `validated_height` was advanced to `M`.
/// - `NothingToPersist(M)`: the validator owns no persistent state
///   (`state_persistence()` is `None` — digest mode). Nothing was fsynced,
///   but the validator has a real `validated_height()` and the store side of
///   the pair runs exactly as it does after a successful flush.
/// - `NoValidator`: light-mode path; there is no validator at all, and
///   `validated_height` is not recorded.
/// - `Failed`: validator's `flush()` returned an error; modifier store's
///   `validated_height` was NOT advanced.
///
/// ⚠ `NothingToPersist` and `NoValidator` are NOT interchangeable, and folding
/// the first into the second is a behaviour change, not a simplification.
/// Digest mode has a validator and a real height — `set_validated_height(M)`
/// and the prune must keep happening. Light mode deliberately skips both.
/// See `../facts/sync.md` § "Flushing a validator that owns no state".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlushOutcome {
    Flushed(u32),
    NothingToPersist(u32),
    NoValidator,
    Failed,
}

impl FlushOutcome {
    /// Whether the caller should advance its `last_flush_height` bookkeeping.
    /// Advances on a successful flush, on nothing-to-persist (digest mode),
    /// and on no-validator (light mode); stays put on flush failure so the
    /// next `should_flush()` retries.
    pub(crate) fn advances_last_flush(self) -> bool {
        matches!(
            self,
            FlushOutcome::Flushed(_)
                | FlushOutcome::NothingToPersist(_)
                | FlushOutcome::NoValidator
        )
    }

    /// The height the cross-DB pair may record and prune against, if any.
    ///
    /// `Some(M)` for both a real fsync and the nothing-to-persist case: the
    /// store side of the handshake does not distinguish them, because step (1)
    /// is either satisfied or vacuous. `None` for light mode (no height exists)
    /// and for a failed flush (durability is UNKNOWN — advancing anything on
    /// the strength of it is exactly the bug the split exists to prevent).
    ///
    /// Single decision point on purpose: the prune call sites match on the
    /// outcome with `if let`, which the compiler does not check for
    /// exhaustiveness, so a future variant must be considered here rather than
    /// silently falling out of three separate patterns.
    pub(crate) fn committed_height(self) -> Option<u32> {
        match self {
            FlushOutcome::Flushed(m) | FlushOutcome::NothingToPersist(m) => Some(m),
            FlushOutcome::NoValidator | FlushOutcome::Failed => None,
        }
    }
}

/// Synchronously flush the validator and report the outcome.
///
/// Pure validator interaction — no `.await` inside, so `&V` is never held
/// across an async boundary. This matters because the real `Validator` is
/// `!Sync` (it owns a `PersistentBatchAVLProver` whose AVL nodes hold
/// `Rc<RefCell<_>>`); spanning an await would force `V: Sync` to satisfy
/// `tokio::spawn`'s `Send` bound on the surrounding future.
///
/// Pair with [`complete_store_flush_pair`] to perform the full cross-DB
/// durability handshake. See `facts/sync.md` § "Cross-DB Durability
/// Handshake" for the invariant.
///
/// `height_context` is used only for the warn-log on a failed flush — the
/// height the sweep was attempting to commit. Not required for correctness;
/// purely an operator-facing breadcrumb.
pub(crate) fn try_flush_validator<V: BlockValidator>(
    validator: Option<&V>,
    height_context: u32,
) -> FlushOutcome {
    match validator {
        None => FlushOutcome::NoValidator,
        // No `StatePersistence` = digest mode: nothing to fsync, which is not
        // a failure and not the same as having no validator. See
        // `FlushOutcome::NothingToPersist`.
        Some(v) => match v.state_persistence() {
            None => FlushOutcome::NothingToPersist(v.validated_height()),
            Some(p) => match p.flush() {
                Ok(()) => FlushOutcome::Flushed(v.validated_height()),
                Err(e) => {
                    tracing::warn!(height = height_context, error = %e, "validator flush failed");
                    FlushOutcome::Failed
                }
            },
        },
    }
}

/// Complete the cross-DB flush pair on the store side.
///
/// Takes the outcome of [`try_flush_validator`] (validator already flushed)
/// and finishes the durability handshake on the store side. Order is
/// load-bearing: `set_validated_height(M)` (only when the outcome carries a
/// height) then `store.flush()`. A failed validator flush MUST NOT advance the
/// modifier store's recorded `validated_height`; light mode (no validator)
/// skips the `set_validated_height` write entirely. Digest mode
/// (`NothingToPersist`) does write it — see that variant's doc. The store flush
/// always runs to fsync any accumulated section writes.
pub(crate) async fn complete_store_flush_pair<S: SyncStore>(outcome: FlushOutcome, store: &S) {
    if let Some(m) = outcome.committed_height() {
        store.set_validated_height(m).await;
    }
    store.flush().await;
}

/// Prune section bodies (102, 104, 108) at heights `< horizon` derived from
/// `blocks_to_keep` and the just-flushed height. Non-fatal: errors are
/// logged at WARN and the next flush retries with the same (idempotent)
/// horizon. No-op when `blocks_to_keep < 0` (archival mode) or when the
/// computed horizon is 0 (chain hasn't grown past the retention window).
///
/// See `../facts/sync.md` § "Pruning at flush time".
pub(crate) async fn maybe_prune_at_horizon<S: SyncStore, C: SyncChain>(
    store: &S,
    chain: &C,
    flushed_height: u32,
    blocks_to_keep: i32,
) {
    if blocks_to_keep < 0 {
        return;
    }
    let voting_length = chain.voting_length().await;
    let horizon = crate::retention::compute_prune_horizon(
        flushed_height,
        blocks_to_keep as u32,
        voting_length,
    );
    if horizon == 0 {
        return;
    }
    match store
        .prune_below_height(
            horizon,
            &[
                BLOCK_TRANSACTIONS_TYPE_ID,
                AD_PROOFS_TYPE_ID,
                EXTENSION_TYPE_ID,
            ],
        )
        .await
    {
        Ok(n) => tracing::debug!(pruned = n, horizon, "pruned modifier rows"),
        Err(e) => tracing::warn!(
            error = %e,
            horizon,
            "prune failed; next flush will retry"
        ),
    }
}

/// Timing configuration for the sync state machine.
/// Built from the P2P network config by the main crate.
pub struct SyncConfig {
    /// Minimum time between scheduled SyncInfo sends (JVM: MinSyncInterval = 20s).
    pub sync_interval: Duration,
    /// How long without progress before rotating peer (JVM: syncTimeout = 10s, ours = 60s).
    pub stall_timeout: Duration,
    /// SyncInfo poll interval when synced (JVM: syncIntervalStable = 30s).
    pub synced_poll_interval: Duration,
    /// Delivery check interval (JVM: 5s cycle).
    pub delivery_check_interval: Duration,
    /// Minimum gap between SyncInfo sends (JVM: PerPeerSyncLockTime = 100ms, ours = 200ms).
    pub min_sync_send_interval: Duration,
    /// Delivery timeout per modifier request (JVM: deliveryTimeout).
    pub delivery_timeout: Duration,
    /// Max delivery re-attempts (JVM: maxDeliveryChecks).
    pub max_delivery_checks: u32,
    /// Node state type — determines which block sections to download.
    /// UTXO mode skips AD proofs; digest mode downloads all sections.
    pub state_type: StateType,
    /// Enable UTXO snapshot bootstrapping.
    pub utxo_bootstrap: bool,
    /// Minimum peers announcing the same manifest before downloading.
    pub min_snapshot_peers: u32,
    /// Data directory for temporary snapshot download storage.
    pub data_dir: std::path::PathBuf,
    /// Heap-allocated-bytes threshold above which `validator.flush()` is called
    /// mid-sweep. When the probe (see `flush_probe`) reports live heap above
    /// this, and at least `flush_min_blocks` have elapsed since the last flush,
    /// the sweep commits the redb write transaction. Setting to 0 disables
    /// memory-based triggering.
    pub flush_heap_threshold_mb: u64,
    /// Upper guardrail: flush at least every N validated blocks regardless of
    /// memory. Bounds crash recovery work. Always applied.
    pub flush_max_blocks: u32,
    /// Lower guardrail: never flush more often than every N validated blocks,
    /// even if heap is over threshold. Prevents flush storms when heap growth
    /// comes from sources other than the redb write transaction.
    pub flush_min_blocks: u32,
    /// At-tip override for `flush_heap_threshold_mb`. Applied once on entry to
    /// `synced()`. None = keep the cold-sync value.
    pub synced_flush_heap_threshold_mb: Option<u64>,
    /// At-tip override for `flush_max_blocks`. Applied once on entry to
    /// `synced()`. None = keep the cold-sync value.
    pub synced_flush_max_blocks: Option<u32>,
    /// At-tip override for `flush_min_blocks`. Applied once on entry to
    /// `synced()`. None = keep the cold-sync value.
    pub synced_flush_min_blocks: Option<u32>,
    /// Probe returning the current live heap in bytes. Main crate wires this
    /// to `tikv_jemalloc_ctl::stats::allocated` when built with jemalloc.
    /// When `None`, flushing is purely count-based (every `flush_max_blocks`).
    pub flush_probe: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    /// Maximum gap between state.META_BLOCK_HEIGHT and the modifier
    /// store's recorded validated_height that the startup
    /// reconciliation will trust without a state rollback. Default
    /// matches `flush_max_blocks` (100). Used by the main crate's
    /// reconciliation step, not by sync itself; threaded through here
    /// because SyncConfig is sync's public configuration surface.
    ///
    /// See ../facts/sync.md "Cross-DB Durability Handshake" §
    /// Configuration for the policy this knob controls.
    pub reconciliation_trust_threshold: u32,
    /// Block-body retention horizon. `-1` (default) means "no pruning, full
    /// archival." `>= 0` enables pruning of non-header section bodies
    /// (102 BlockTransactions, 104 ADProofs, 108 Extension) older than the
    /// retention horizon at each flush_pair, and caps the flush dial's
    /// min/max guardrails at `blocks_to_keep` so crash recovery never
    /// needs bodies that pruning has deleted.
    ///
    /// See ../facts/sync.md § "Block Body Retention".
    pub blocks_to_keep: i32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(20),
            stall_timeout: Duration::from_secs(60),
            synced_poll_interval: Duration::from_secs(30),
            delivery_check_interval: Duration::from_secs(5),
            min_sync_send_interval: Duration::from_millis(200),
            delivery_timeout: Duration::from_secs(10),
            max_delivery_checks: 100,
            state_type: StateType::Utxo,
            utxo_bootstrap: false,
            min_snapshot_peers: 2,
            data_dir: std::path::PathBuf::from("."),
            // 0 disables the memory trigger. Effective policy then degenerates
            // to "flush every flush_max_blocks". Main crate overrides with a
            // real threshold when a probe is wired.
            flush_heap_threshold_mb: 0,
            // 100 preserves the prior hardcoded cadence as the upper bound.
            flush_max_blocks: 100,
            flush_min_blocks: 5,
            synced_flush_heap_threshold_mb: None,
            synced_flush_max_blocks: None,
            synced_flush_min_blocks: None,
            flush_probe: None,
            reconciliation_trust_threshold: 100,
            // -1 preserves the pre-pruning default (full archival). Main
            // crate overrides from the node config's `blocks_to_keep` knob
            // when the operator opts in.
            blocks_to_keep: -1,
        }
    }
}

/// What the sync machine's in-flight window holds right now.
///
/// [`crate::delivery::DeliveryTracker`] bookkeeping and nothing else — this
/// crate holds no section payloads. See
/// [`HeaderSync::window_memory_estimate`] for what is and is not counted, and
/// `../facts/sync.md` § "Memory attribution".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncWindowEstimate {
    /// Heap bytes held by [`crate::delivery::DeliveryTracker`]: its
    /// pending-request map and its evicted-id vec. Id-keyed bookkeeping only —
    /// downloaded sections go straight from the p2p pipeline into the modifier
    /// store, and the validation sweep reads each block back out, applies it,
    /// and drops it inside one loop iteration. Nothing between "downloaded"
    /// and "applied" lives on sync's heap.
    ///
    /// A lower bound, by a bounded and one-directional margin: it counts the
    /// live entries exactly and not the hash table's unused slots. See
    /// [`crate::delivery::DeliveryTracker::memory_bytes`] for why the slack
    /// cannot be counted honestly and how large it can get (≈2.3×).
    pub tracker_bytes: u64,
    /// Live entries behind that figure: in-flight requests in the pending map
    /// plus ids in the evicted vec awaiting re-request.
    ///
    /// There is no separate download queue to count. `request_next_sections`
    /// recomputes the window's missing sections into a local map on every
    /// cycle and drops it before returning.
    pub tracker_entries: u64,
}

/// Sentinel in the published window atomic meaning "sync has never written it".
///
/// Zero is a legitimate reading — an idle tracker really has allocated
/// nothing — so absence needs a value no byte count can take. `u64::MAX` is
/// 16 EiB. Readers MUST map this to absent rather than reporting 0, per
/// `../facts/api.md` § "Component memory attribution": a node that has applied
/// no blocks must not claim an empty window.
pub const WINDOW_BYTES_UNSET: u64 = u64::MAX;

/// Header chain sync state machine.
///
/// Event-driven loop matching the JVM's sync exchange pattern:
/// - Sends SyncInfo, receives Inv, sends ModifierRequest
/// - On pipeline progress: sends SyncInfo again (two-batch cycle)
/// - On peer SyncInfo: responds with our SyncInfo (bidirectional exchange)
/// - On stall: rotates to a different peer
pub struct HeaderSync<T: SyncTransport, C: SyncChain, S: SyncStore, V: BlockValidator> {
    config: SyncConfig,
    transport: T,
    chain: C,
    store: S,
    validator: Option<V>,
    progress: mpsc::Receiver<u32>,
    delivery_control_rx: mpsc::UnboundedReceiver<DeliveryControl>,
    delivery_data_rx: mpsc::Receiver<DeliveryData>,
    /// Oneshot channel to send snapshot data to the main crate for state loading.
    snapshot_tx: Option<tokio::sync::oneshot::Sender<crate::snapshot::SnapshotData>>,
    /// Oneshot channel to receive the validator back after snapshot loading.
    validator_rx: Option<tokio::sync::oneshot::Receiver<V>>,
    tracker: DeliveryTracker,
    /// Peer we're currently syncing from.
    sync_peer: Option<PeerId>,
    /// Peers that failed to produce progress — skipped during peer selection.
    stalled_peers: HashSet<PeerId>,
    /// When the last chain height increase was observed.
    last_progress: Instant,
    /// When we last sent a scheduled SyncInfo (20s floor applies to these).
    last_scheduled_sync: Instant,
    /// When we last sent ANY SyncInfo (rate-limit gate for PerPeerSyncLockTime).
    last_sync_sent: Instant,
    /// Total SyncInfo messages sent this session (diagnostics).
    sync_sent_count: u32,
    /// Highest height where ALL required block sections are in the store.
    downloaded_height: u32,
    /// Highest height where apply_state() returned Ok (state advanced).
    /// The sole sweep watermark besides `downloaded_height`.
    state_applied_height: u32,
    /// Interval gate for the periodic catch-up progress record. Diagnostic
    /// only. In-memory: a restart re-baselines. See
    /// [`crate::catchup_progress`].
    catchup_progress: crate::catchup_progress::CatchUpProgressReporter,
    /// Shared downloaded_height for the API (read by fastsync to avoid redundant work).
    shared_downloaded_height: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Gate controlling whether block/header/tx ModifierRequest sends actually fire.
    /// Closed (false) at construction, opened (true) by the main crate after the
    /// boot-time fastsync bootstrap decision resolves. See facts/sync.md "Bootstrap Mode".
    block_request_gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Peer's chain tip, updated on every incoming SyncInfo to the max observed.
    /// Read by the main crate to compute the bootstrap gap.
    peer_chain_tip: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Published window memory estimate, written after each applied block.
    ///
    /// Publish, don't expose: `sync/` owns the validator and does not share it
    /// (`../facts/validation.md`), so no HTTP path can call
    /// [`Self::window_memory_estimate`] synchronously. The main crate hands in
    /// this atomic and passes a clone to the API. Holds [`WINDOW_BYTES_UNSET`]
    /// until the first block is applied.
    shared_window_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Height of the most recent `validator.flush()` call. Used by the
    /// memory-aware flush policy to enforce `flush_min_blocks` spacing and
    /// `flush_max_blocks` upper bound.
    last_flush_height: u32,
    /// At-tip transition: oneshot to request a validator rebuild from main.
    /// Set by the integrator via [`set_at_tip_channels`]. None if the
    /// integrator did not configure synced-mode cache resize.
    at_tip_request_tx: Option<tokio::sync::oneshot::Sender<u32>>,
    /// At-tip transition: oneshot to receive the rebuilt validator.
    at_tip_validator_rx: Option<tokio::sync::oneshot::Receiver<V>>,
    /// At-tip transition: cache size to apply via resize_cache() on the
    /// existing storage handle (no reopen, no second mmap).
    synced_cache_bytes: Option<usize>,
    /// Exponential backoff gating the validation sweep when the applied
    /// tip fails to advance. Derived purely from the validator's applied
    /// tip, so stall detection is blind to which subsystem rejected the
    /// block. Also the single emitter of the contract `validation_stuck`
    /// event, fired once a frontier has stalled 5 sweeps in a row (the
    /// caller hands in the `error_kind`/`missing_key` label). Resets on
    /// real progress or a frontier change. In-memory — a restart is a
    /// legitimate reset. See [`crate::sweep_backoff`].
    sweep_backoff: crate::sweep_backoff::SweepBackoff,
    /// Explicit shutdown signal from the host. `run()` selects against
    /// this alongside `run_inner()`; the signal cancels the loop and
    /// falls through to `shutdown_flush`. An explicit channel is
    /// required because `P2pTransport` holds a clone of the host's
    /// `Arc<P2pNode>` (along with mining, API, mempool), so dropping
    /// the host's reference does not close the events channel.
    /// See `../facts/sync.md` § "Graceful shutdown".
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
}

impl<T: SyncTransport, C: SyncChain, S: SyncStore, V: BlockValidator> HeaderSync<T, C, S, V> {
    // 15-arg constructor reflects the dependency-inversion surface (4 trait
    // objects + 7 channels/atomics + 3 config-ish args + shutdown). Bundling
    // these would hide the wiring without simplifying it. A builder is
    // reasonable future work but not justified for one caller (`src/main.rs`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: SyncConfig,
        transport: T,
        chain: C,
        store: S,
        validator: Option<V>,
        progress: mpsc::Receiver<u32>,
        delivery_control_rx: mpsc::UnboundedReceiver<DeliveryControl>,
        delivery_data_rx: mpsc::Receiver<DeliveryData>,
        snapshot_tx: Option<tokio::sync::oneshot::Sender<crate::snapshot::SnapshotData>>,
        validator_rx: Option<tokio::sync::oneshot::Receiver<V>>,
        shared_downloaded_height: std::sync::Arc<std::sync::atomic::AtomicU32>,
        block_request_gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
        peer_chain_tip: std::sync::Arc<std::sync::atomic::AtomicU32>,
        shared_window_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        let tracker =
            DeliveryTracker::with_config(config.delivery_timeout, config.max_delivery_checks);
        let initial_validated = validator.as_ref().map_or(0, |v| v.validated_height());
        let last_flush_height = initial_validated;
        // Stamp "never written" here rather than trusting the caller's initial
        // value. Whatever the host constructed the atomic with, a reader that
        // polls before the first applied block must see absent, not zero.
        shared_window_bytes.store(WINDOW_BYTES_UNSET, std::sync::atomic::Ordering::Relaxed);
        Self {
            config,
            transport,
            chain,
            store,
            validator,
            progress,
            delivery_control_rx,
            delivery_data_rx,
            snapshot_tx,
            validator_rx,
            tracker,
            sync_peer: None,
            stalled_peers: HashSet::new(),
            last_progress: Instant::now(),
            last_scheduled_sync: Instant::now(),
            last_sync_sent: Instant::now(),
            sync_sent_count: 0,
            downloaded_height: initial_validated,
            state_applied_height: initial_validated,
            catchup_progress: crate::catchup_progress::CatchUpProgressReporter::default(),
            shared_downloaded_height,
            block_request_gate,
            peer_chain_tip,
            shared_window_bytes,
            last_flush_height,
            at_tip_request_tx: None,
            at_tip_validator_rx: None,
            synced_cache_bytes: None,
            sweep_backoff: crate::sweep_backoff::SweepBackoff::default(),
            shutdown_rx,
        }
    }

    /// Bytes held by the in-flight delivery bookkeeping. `None` if not
    /// computable.
    ///
    /// [`crate::delivery::DeliveryTracker`] is the whole of it: `sync/` holds
    /// no section payloads and no persistent download queue.
    ///
    /// - Sections never pass through sync's heap. `ModifierResponse` is not a
    ///   message this state machine handles at all — the p2p pipeline writes
    ///   the bytes to the modifier store and notifies sync with ids only
    ///   ([`crate::delivery::DeliveryData::Received`]). The validation sweep
    ///   reads each block back out of the store, applies it, and drops it
    ///   within one iteration of the loop in `advance_state_applied_height`.
    ///   Downloaded-but-unapplied section bytes live in redb, and redb's cache
    ///   is already attributed as `storeCacheBytes`.
    /// - There is no download queue to size. `request_next_sections`
    ///   recomputes the missing sections in the 192-block window into a local
    ///   map every cycle and drops it before returning.
    /// - What is left is the [`crate::delivery::DeliveryTracker`]: the
    ///   pending-request map and the evicted-id queue, both id-keyed. Their
    ///   footprint is counted from the live entries, so it rises when requests
    ///   go in flight and falls as they are delivered — never derived from a
    ///   per-block constant.
    ///
    /// Currently always `Some`: both structures are always present and always
    /// measurable. The `Option` is in the contract so that a future in-flight
    /// structure sync cannot size reports absence rather than quietly
    /// publishing a total that omits it.
    pub fn window_memory_estimate(&self) -> Option<SyncWindowEstimate> {
        Some(SyncWindowEstimate {
            tracker_bytes: self.tracker.memory_bytes(),
            tracker_entries: (self.tracker.pending_count() + self.tracker.evicted_count()) as u64,
        })
    }

    /// Publish the current window estimate to the shared atomic.
    ///
    /// Called after each applied block. Not `async` and takes `&self`: the
    /// borrow never spans an `.await`, so the enclosing future stays `Send`
    /// even though `V` is `!Sync`.
    fn publish_window_estimate(&self) {
        let bytes = self
            .window_memory_estimate()
            .map_or(WINDOW_BYTES_UNSET, |e| e.tracker_bytes);
        self.shared_window_bytes
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Configure the at-tip transition. When `synced()` is first entered,
    /// `resize_cache()` is called on the validator with `synced_cache_bytes`
    /// to shrink the read cache without reopening the database.
    pub fn set_at_tip_cache(&mut self, synced_cache_bytes: usize) {
        self.synced_cache_bytes = Some(synced_cache_bytes);
    }

    /// Configure the at-tip transition channels. When `synced()` is first
    /// entered, the existing validator is taken (releasing its AVL storage
    /// handle), its height is sent on `request_tx`, and the new validator
    /// is awaited on `validator_rx`. The integrator handles the actual
    /// storage reopen + validator rebuild on the other end of the pair.
    /// Deprecated: prefer [`set_at_tip_cache`] which resizes in-place.
    pub fn set_at_tip_channels(
        &mut self,
        request_tx: tokio::sync::oneshot::Sender<u32>,
        validator_rx: tokio::sync::oneshot::Receiver<V>,
    ) {
        self.at_tip_request_tx = Some(request_tx);
        self.at_tip_validator_rx = Some(validator_rx);
    }

    /// Decide whether to `validator.flush()` after validating `height`.
    ///
    /// Policy (dial between memory usage and I/O, see docs/profiling-results.md):
    ///
    /// 1. Forced flush if at least `flush_max_blocks` have passed since the
    ///    last flush. Bounds crash-recovery work.
    /// 2. Suppressed flush if fewer than `flush_min_blocks` have passed.
    ///    Prevents flush storms when heap growth is driven by something other
    ///    than the redb write transaction.
    /// 3. Between those bounds, flush when the heap probe reports live heap
    ///    above `flush_heap_threshold_mb`. The probe is configured by the
    ///    main crate — typically `tikv_jemalloc_ctl::stats::allocated` when
    ///    the binary is built with jemalloc.
    /// 4. If no probe is configured OR threshold is 0, the policy degenerates
    ///    to "flush every `flush_max_blocks`" via rule (1).
    fn should_flush(&self, height: u32) -> bool {
        // Apply the `blocks_to_keep` cap to the dial's min/max guardrails so
        // the validated-to-tip gap can never exceed what pruning retains.
        // See `../facts/sync.md` § "Flush dial cap".
        let (effective_min, effective_max) = crate::retention::effective_flush_bounds(
            self.config.blocks_to_keep,
            self.config.flush_min_blocks,
            self.config.flush_max_blocks,
        );
        let since_last = height.saturating_sub(self.last_flush_height);
        if since_last >= effective_max {
            return true;
        }
        if since_last < effective_min {
            return false;
        }
        if self.config.flush_heap_threshold_mb == 0 {
            return false;
        }
        let threshold_bytes = self.config.flush_heap_threshold_mb * 1024 * 1024;
        match self.config.flush_probe.as_ref() {
            Some(probe) => probe() >= threshold_bytes,
            None => false,
        }
    }

    /// One-shot startup advisory: WARN if the on-disk archive holds
    /// section bodies older than the `blocks_to_keep`-derived retention
    /// horizon. Points the operator at `sharpen prune` for explicit
    /// reclamation. No-op when `blocks_to_keep <= 0` (full archival or
    /// flush-every-block — nothing to reclaim) or in light mode (no
    /// validator means no validated_height to anchor against, and no
    /// bodies on disk to begin with).
    ///
    /// See `../facts/sync.md` § "Startup WARN (lazy migration)".
    ///
    /// `&mut self` (not `&self`) is load-bearing for the call site in
    /// `run_inner`: `Validator` holds `Rc<RefCell<…>>` (the AVL prover)
    /// and is `!Sync`, so any `async fn(&self)` on `HeaderSync` produces
    /// a non-`Send` future (`&Self: Send` requires `Self: Sync`).
    /// `tokio::spawn` in the host's main.rs needs `Send`. Mirroring
    /// `run` / `run_inner`'s `&mut self` keeps the spawned future
    /// `Send`. The body doesn't actually mutate state.
    async fn maybe_warn_reclaimable_bodies(&mut self) {
        if self.config.blocks_to_keep <= 0 {
            return;
        }
        let validator_height = match self.validator.as_ref() {
            Some(v) => v.validated_height(),
            None => return,
        };
        let Ok(Some(actual_min)) = self
            .store
            .min_height_present(BLOCK_TRANSACTIONS_TYPE_ID)
            .await
        else {
            return;
        };
        let voting_length = self.chain.voting_length().await;
        let configured_horizon = crate::retention::compute_prune_horizon(
            validator_height,
            self.config.blocks_to_keep as u32,
            voting_length,
        );
        if actual_min < configured_horizon {
            let reclaimable = configured_horizon - actual_min;
            let blocks_to_keep = self.config.blocks_to_keep;
            tracing::warn!(
                "{reclaimable} historical blocks reclaimable; \
                 run `sharpen prune --keep={blocks_to_keep}` to free disk"
            );
        }
    }

    /// How many blocks ahead of `downloaded_height` to request sections for.
    /// Matches JVM's `FullBlocksToDownloadAhead = 192`.
    const DOWNLOAD_WINDOW: u32 = 192;

    /// Run the sync loop. Returns when the host signals shutdown via
    /// `shutdown_rx` or `run_inner` exits on its own (light-bootstrap
    /// error / snapshot-bootstrap validator channel closed).
    ///
    /// All exit paths funnel through [`Self::shutdown_flush`] so that
    /// `Durability::None` commits accumulated since the last sweep flush
    /// are persisted before the function returns. See
    /// `../facts/sync.md` § "Graceful shutdown".
    pub async fn run(&mut self) {
        tracing::info!("header sync started");
        // Move `shutdown_rx` out of `self` so the select arm doesn't
        // collide with `run_inner`'s `&mut self` borrow. A sentinel
        // receiver takes its place; `_sentinel_tx` is held alive for the
        // duration of `run()` so the sentinel never resolves spuriously.
        let (_sentinel_tx, sentinel_rx) = tokio::sync::oneshot::channel::<()>();
        let mut shutdown_rx = std::mem::replace(&mut self.shutdown_rx, sentinel_rx);
        tokio::select! {
            _ = self.run_inner() => {
                // run_inner returned on its own (light-bootstrap error
                // or snapshot-bootstrap validator-channel-closed path).
            }
            _ = &mut shutdown_rx => {
                tracing::info!("shutdown signal received");
            }
        }
        self.shutdown_flush().await;
    }

    /// Inner driver containing the loop body. Each `return` here falls
    /// through to [`Self::shutdown_flush`] via [`Self::run`].
    async fn run_inner(&mut self) {
        // Light-client bootstrap: if state_type is Light AND chain is empty,
        // run a one-shot NiPoPoW bootstrap before entering the normal sync
        // cycle. The bootstrap installs the proof's suffix as the chain
        // origin; subsequent tip-following uses the existing loop unchanged.
        // Idempotent: skipped on restart when the chain is non-empty.
        if self.config.state_type == StateType::Light && self.chain.chain_height().await == 0 {
            tracing::info!("light-client mode: running NiPoPoW bootstrap");
            match crate::light_bootstrap::run_light_bootstrap(&mut self.transport, &self.chain)
                .await
            {
                Ok(()) => {
                    let height = self.chain.chain_height().await;
                    // Light mode treats all installed headers as "validated"
                    // — the proof's PoW checks ARE the validation. There's
                    // no validator running and no block sections to download.
                    self.downloaded_height = height;
                    self.state_applied_height = height;
                    tracing::info!(
                        height,
                        "light bootstrap installed, entering tip-following sync"
                    );
                }
                Err(e) => {
                    tracing::error!("light bootstrap failed: {e}");
                    return;
                }
            }
        }

        // Startup: scan for already-downloaded sections in the store
        let tip = self.chain.chain_height().await;
        if tip > 0 {
            self.advance_downloaded_height().await;
        }

        // Startup WARN: surface bodies older than the configured retention
        // horizon so the operator knows to run `sharpen prune` to reclaim
        // disk. Self-correcting: after the first prune sweep the gap
        // closes on its own. Skipped in light mode (no bodies on disk to
        // begin with). See `../facts/sync.md` § "Startup WARN".
        self.maybe_warn_reclaimable_bodies().await;

        loop {
            // Phase 1: wait for outbound peers (skip if already targeting one)
            if self.sync_peer.is_none() && !self.pick_sync_peer().await {
                return; // event stream ended
            }

            // Phase 2: sync from the selected peer
            match self.sync_from_peer().await {
                SyncOutcome::Synced => {
                    // Snapshot bootstrap: if enabled, no validator, and channels ready
                    tracing::debug!(
                        utxo_bootstrap = self.config.utxo_bootstrap,
                        has_validator = self.validator.is_some(),
                        has_snapshot_tx = self.snapshot_tx.is_some(),
                        "SyncOutcome::Synced reached"
                    );
                    if self.config.utxo_bootstrap
                        && self.validator.is_none()
                        && self.snapshot_tx.is_some()
                    {
                        tracing::info!("headers synced, starting UTXO snapshot sync");
                        let snapshot_config = crate::snapshot::SnapshotConfig {
                            min_snapshot_peers: self.config.min_snapshot_peers,
                            chunk_timeout_multiplier: 4,
                            data_dir: self.config.data_dir.clone(),
                        };

                        match crate::snapshot::run_snapshot_sync(
                            &mut self.transport,
                            &self.chain,
                            &snapshot_config,
                        )
                        .await
                        {
                            Ok(snapshot_data) => {
                                let height = snapshot_data.snapshot_height;
                                // Send snapshot to main for state loading + validator creation
                                if let Some(tx) = self.snapshot_tx.take() {
                                    let _ = tx.send(snapshot_data);
                                }
                                // Wait for the validator to come back
                                if let Some(rx) = self.validator_rx.take() {
                                    match rx.await {
                                        Ok(validator) => {
                                            self.validator = Some(validator);
                                            self.downloaded_height = height;
                                            self.state_applied_height = height;
                                            tracing::info!(
                                                height,
                                                "snapshot loaded, resuming block sync"
                                            );
                                        }
                                        Err(_) => {
                                            tracing::error!(
                                                "validator channel closed during snapshot bootstrap"
                                            );
                                            return;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("snapshot sync failed: {e}, retrying next cycle");
                                continue;
                            }
                        }
                    }
                    self.synced().await;
                }
                SyncOutcome::SwitchPeer => continue,
                SyncOutcome::Stalled => {
                    if let Some(peer) = self.sync_peer.take() {
                        let height = self.chain.chain_height().await;
                        tracing::warn!(
                            peer = %peer, height,
                            syncs_sent = self.sync_sent_count,
                            "sync stalled, rotating peer"
                        );
                        self.stalled_peers.insert(peer);
                    }
                }
                SyncOutcome::PeerDisconnected | SyncOutcome::StreamEnded => {
                    self.sync_peer = None;
                }
            }
        }
    }

    /// Durably persist all in-memory state before `run` returns. Mirrors
    /// the end-of-sweep flush block (cross-DB durability handshake order:
    /// `validator.flush()` → `store.set_validated_height(M)` →
    /// `store.flush()`). Runs unconditionally on every exit path of
    /// [`Self::run_inner`]. Without this, `Durability::None` commits
    /// since the last sweep flush sit in redb's page cache and are
    /// lost on process exit. See `../facts/sync.md` § "Graceful shutdown".
    ///
    /// Failure of `validator.flush()` is logged by
    /// [`try_flush_validator`] and does NOT block return — the host
    /// must be able to exit. The next startup's reconciliation handles
    /// the resulting gap.
    async fn shutdown_flush(&mut self) {
        // Prefer the validator's own height when available; fall back
        // to `state_applied_height` for light mode (no validator) and
        // for the snapshot-bootstrap exit (validator may be `None`).
        let height = match self.validator.as_ref() {
            Some(v) => v.validated_height(),
            None => self.state_applied_height,
        };
        tracing::info!(height, "header sync exiting — flushing state");
        let outcome = try_flush_validator(self.validator.as_ref(), height);
        complete_store_flush_pair(outcome, &self.store).await;
        if let Some(m) = outcome.committed_height() {
            maybe_prune_at_horizon(&self.store, &self.chain, m, self.config.blocks_to_keep).await;
        }
        if outcome.advances_last_flush() {
            self.last_flush_height = height;
        }
        tracing::info!("header sync stopped");
    }

    /// Wait until we have an outbound peer to sync from.
    /// Returns false if the event stream ends.
    async fn pick_sync_peer(&mut self) -> bool {
        loop {
            let peers = self.transport.outbound_peers().await;
            if let Some(peer) = self.select_peer(&peers) {
                self.sync_peer = Some(peer);
                self.last_progress = Instant::now();
                self.sync_sent_count = 0;
                let height = self.chain.chain_height().await;
                tracing::debug!(peer = %peer, height, "starting header sync");
                return true;
            }

            // No eligible peers — wait for a connection event
            match self.transport.next_event().await {
                Some(_) => {}
                None => return false,
            }
        }
    }

    /// Pick an outbound peer, preferring those not in the stalled set.
    fn select_peer(&mut self, peers: &[PeerId]) -> Option<PeerId> {
        let choice = peers
            .iter()
            .find(|p| !self.stalled_peers.contains(p))
            .or_else(|| {
                // All peers stalled — clear and retry
                self.stalled_peers.clear();
                peers.first()
            });
        choice.copied()
    }

    /// Run the event-driven sync cycle with the current peer.
    async fn sync_from_peer(&mut self) -> SyncOutcome {
        let peer = match self.sync_peer {
            Some(p) => p,
            None => return SyncOutcome::StreamEnded,
        };

        // Send initial SyncInfo to kick off the exchange
        if self.send_sync_info(peer).await.is_err() {
            return SyncOutcome::PeerDisconnected;
        }
        self.last_scheduled_sync = Instant::now();
        let mut last_delivery_check = Instant::now();
        // One progress-triggered send per 20s cycle — gives the two-batch
        // pattern (one scheduled + one progress) matching JVM behavior.
        let mut progress_send_used = false;
        // Sweep timer: retry validation periodically even when downloads stall.
        // The sweep's internal backoff gate (sweep_backoff.should_defer) prevents
        // tight-looping on deterministic failures — this just ensures the sweep
        // gets a chance to run at regular intervals regardless of download activity.
        let mut sweep_ticker = tokio::time::interval(Duration::from_secs(10));

        loop {
            let until_next = self
                .config
                .sync_interval
                .saturating_sub(self.last_scheduled_sync.elapsed());
            let until_delivery = self
                .config
                .delivery_check_interval
                .saturating_sub(last_delivery_check.elapsed());

            tokio::select! {
                biased;

                // Control-plane events — checked first, never dropped
                Some(ctrl) = self.delivery_control_rx.recv() => {
                    self.handle_control_event(ctrl, peer).await;
                }

                // P2P events: Inv, peer SyncInfo, disconnect
                event = self.transport.next_event() => {
                    match event {
                        Some(event) => {
                            match self.handle_event(peer, event).await {
                                EventResult::Continue => {}
                                EventResult::Synced => return SyncOutcome::Synced,
                                EventResult::BehindPeer(ahead_peer) => {
                                    self.sync_peer = Some(ahead_peer);
                                    self.stalled_peers.clear();
                                    return SyncOutcome::SwitchPeer;
                                }
                                EventResult::PeerGone => {
                                    // Re-request any modifiers that were pending from this peer
                                    let orphaned = self.tracker.purge_peer(peer);
                                    if !orphaned.is_empty() {
                                        self.rerequest_from_any(HEADER_TYPE_ID, &orphaned).await;
                                    }
                                    return SyncOutcome::PeerDisconnected;
                                }
                            }
                        }
                        None => return SyncOutcome::StreamEnded,
                    }
                }

                // Data-plane delivery notifications: received / evicted modifiers
                Some(data) = self.delivery_data_rx.recv() => {
                    match data {
                        DeliveryData::Received(ids) => {
                            for id in &ids {
                                self.tracker.mark_received(id);
                            }
                            self.advance_downloaded_height().await;
                        }
                        DeliveryData::Evicted(ids) => {
                            self.tracker.schedule_rerequest(&ids);
                        }
                    }
                }

                // Pipeline progress: send ONE SyncInfo per cycle for the two-batch pattern
                Some(height) = self.progress.recv() => {
                    self.last_progress = Instant::now();
                    self.stalled_peers.clear();

                    self.request_next_sections().await;

                    if !progress_send_used {
                        progress_send_used = true;
                        tracing::debug!(height, "progress → second batch SyncInfo");
                        let _ = self.send_sync_info(peer).await;
                    }
                }

                // Delivery check: re-request timed-out modifiers + advance watermark
                _ = tokio::time::sleep(until_delivery) => {
                    last_delivery_check = Instant::now();
                    let result = self.tracker.check_timeouts();
                    self.handle_delivery_check(result, peer).await;
                    self.advance_downloaded_height().await;
                }

                // Scheduled SyncInfo: 20-second cycle start
                _ = tokio::time::sleep(until_next) => {
                    let _ = self.send_sync_info(peer).await;
                    self.last_scheduled_sync = Instant::now();
                    progress_send_used = false; // allow one progress send next cycle

                    if self.last_progress.elapsed() > self.config.stall_timeout {
                        return SyncOutcome::Stalled;
                    }
                }

                // Sweep timer: retry validation periodically even when downloads stall.
                // The sweep's internal backoff gate (sweep_backoff.should_defer) prevents
                // tight-looping on deterministic failures — this just ensures the sweep
                // gets a chance to run at regular intervals regardless of download activity.
                _ = sweep_ticker.tick() => {
                    self.advance_state_applied_height().await;
                }
            }
        }
    }

    /// Request announced modifiers from a peer and track delivery.
    ///
    /// Filters out IDs already in the store or pending in the delivery
    /// tracker before sending. Chunks into messages of at most 400 IDs
    /// to stay within the JVM's `desiredInvObjects` limit.
    async fn request_announced(&mut self, peer: PeerId, modifier_type: u8, ids: Vec<[u8; 32]>) {
        if !self
            .block_request_gate
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        // Filter out already-known and already-in-flight IDs
        let mut needed = Vec::with_capacity(ids.len());
        for id in &ids {
            if self.tracker.is_pending(id) {
                continue;
            }
            if self.store.has_modifier(modifier_type, id).await {
                continue;
            }
            needed.push(*id);
        }
        let filtered = ids.len() - needed.len();
        if filtered > 0 {
            tracing::debug!(
                announced = ids.len(),
                filtered,
                remaining = needed.len(),
                "Inv pre-filter: skipped known/pending modifiers"
            );
        }
        if needed.is_empty() {
            return;
        }
        self.tracker.mark_requested(&needed, peer, modifier_type);
        // JVM rejects ModifierRequest with >400 elements
        for chunk in needed.chunks(400) {
            if let Err(e) = self
                .transport
                .send_to(
                    peer,
                    ProtocolMessage::ModifierRequest {
                        modifier_type,
                        ids: chunk.to_vec(),
                    },
                )
                .await
            {
                tracing::warn!(peer = %peer, "modifier request send failed: {e}");
                break;
            }
        }
    }

    /// Advance the full block height watermark by scanning forward.
    ///
    /// For each height above the current watermark, checks whether all
    /// required block sections (per `config.state_type`) are in the store.
    /// Advances as far as possible in one call — stops at the first gap.
    async fn advance_downloaded_height(&mut self) {
        if self.validator.is_none() && self.config.utxo_bootstrap {
            return; // Snapshot bootstrap pending — don't download sections from height 0
        }
        let chain_height = self.chain.chain_height().await;
        if self.downloaded_height >= chain_height {
            return;
        }

        let start = self.downloaded_height + 1;
        let mut new_height = self.downloaded_height;

        for height in start..=chain_height {
            let header = match self.chain.header_at(height).await {
                Some(h) => h,
                None => break,
            };
            let sections = enr_chain::required_section_ids(&header, self.config.state_type);
            let mut complete = true;
            for (type_id, id) in &sections {
                if !self.store.has_modifier(*type_id, id).await {
                    complete = false;
                    break;
                }
            }
            if !complete {
                break;
            }
            new_height = height;
        }

        if new_height > self.downloaded_height {
            let advanced = new_height - self.downloaded_height;
            self.downloaded_height = new_height;
            self.shared_downloaded_height
                .store(new_height, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(
                downloaded_height = new_height,
                advanced,
                chain_height,
                "downloaded height advanced"
            );
            self.advance_state_applied_height().await;
        }
    }

    /// Advance the validated height by running the block validator on
    /// downloaded-but-not-yet-validated blocks.
    async fn advance_state_applied_height(&mut self) {
        // The applied tip is the validator's own `validated_height()` — it
        // advances inside `apply_state` and retreats inside `reset_to`
        // atomically with the AVL prover, so it is the single source of
        // truth for what state has actually been applied. The sweep MUST
        // resume from there, never from `state_applied_height`, which is a
        // cache a reorg rollback can leave stale (ahead of the real tip).
        // Deriving the start from the prover tip
        // makes it impossible to feed `apply_state` a non-consecutive block
        // — that was the wedge where the sweep skipped on-disk blocks and
        // looped forever on a `HeightMismatch`. See `../facts/sync.md`.
        let Some(applied_tip) = self.validator.as_ref().map(|v| v.validated_height()) else {
            return; // No validator yet (snapshot bootstrap pending / light mode)
        };
        if applied_tip >= self.downloaded_height {
            // Caught up — nothing on disk past the applied tip. Re-sync the
            // cache in case a prior sweep left it drifted above the tip.
            self.state_applied_height = applied_tip;
            return;
        }

        // Backoff gate. A prior sweep that could not advance the tip past
        // `applied_tip` arms an exponential delay; until it elapses, defer
        // this retry so a deterministic block failure cannot peg a core.
        // Gates ONLY the sweep: the caller's select! loop — header download,
        // peer handling, request servicing — keeps running. The block is
        // still retried once the delay lapses, so the node self-heals the
        // moment a fix / restart / reorg changes things. See
        // [`crate::sweep_backoff`].
        if self.sweep_backoff.should_defer(applied_tip, Instant::now()) {
            return;
        }

        let sweep_from = applied_tip + 1;
        let sweep_to = self.downloaded_height;
        let sweep_size = sweep_to - applied_tip;
        let sweep_start = Instant::now();

        if sweep_size > 100 {
            tracing::info!(
                from = sweep_from as u64,
                to = sweep_to as u64,
                blocks = sweep_size as u64,
                "VALIDATION SWEEP STARTED"
            );
        }

        let mut validated_to = applied_tip;
        // Label handed to the backoff if this sweep stalls, so a
        // `validation_stuck` emission names the mode. Overwritten at the
        // apply_state-error and eval-rollback break points; the `other`
        // default covers the rarer header/epoch-boundary breaks.
        let mut stall_detail = StallDetail::other();

        for height in sweep_from..=sweep_to {
            let header = match self.chain.header_at(height).await {
                Some(h) => h,
                None => break,
            };

            let sections = enr_chain::required_section_ids(&header, self.config.state_type);

            let mut block_txs = None;
            let mut ad_proofs = None;
            let mut extension = None;

            for (type_id, id) in &sections {
                let data = match self.store.get_modifier(*type_id, id).await {
                    Some(d) => d,
                    None => {
                        tracing::warn!(height, type_id, "section bytes missing during validation");
                        return;
                    }
                };
                match *type_id {
                    BLOCK_TRANSACTIONS_TYPE_ID => block_txs = Some(data),
                    AD_PROOFS_TYPE_ID => ad_proofs = Some(data),
                    EXTENSION_TYPE_ID => extension = Some(data),
                    _ => {}
                }
            }

            let block_txs = match block_txs {
                Some(d) => d,
                None => {
                    tracing::warn!(height, "BlockTransactions missing");
                    return;
                }
            };
            let extension = match extension {
                Some(d) => d,
                None => {
                    tracing::warn!(height, "Extension missing");
                    return;
                }
            };

            // Preceding headers for the block's ErgoStateContext, newest
            // first. Nine, not ten: `CONTEXT.headers` is `sigmaLastHeaders`
            // (the JVM's `lastHeaders.drop(1)`), and our window holds headers
            // strictly preceding the block because its own header goes in the
            // preheader. Ten here would be trimmed to nine by
            // `build_state_context` anyway — but a window that is only the
            // right size because the callee trims it is how the ten-vs-nine
            // divergence hid in the first place. See `../facts/validation.md`
            // § "Window size: `CONTEXT.headers` is 9 for a block, never 10".
            let preceding_start = height.saturating_sub(9).max(1);
            let mut preceding = Vec::new();
            for h in (preceding_start..height).rev() {
                if let Some(hdr) = self.chain.header_at(h).await {
                    preceding.push(hdr);
                }
            }

            // Read active parameters and (if at epoch boundary) expected boundary
            // parameters from chain BEFORE calling validate_block. The validator
            // is sync and stateless w.r.t. chain state, so all chain queries
            // happen out-of-band before/after the call.
            let active_params = self.chain.active_parameters().await;
            let (expected_boundary_params, expected_proposed_update) = if self
                .chain
                .is_epoch_boundary(height)
                .await
            {
                let block_proposed_update = match enr_chain::parse_extension_bytes(&extension) {
                    Ok((_header_id, fields)) => enr_chain::extract_disabling_rules_from_kv(&fields),
                    Err(e) => {
                        tracing::error!(height, error = %e, "extension parse for proposed update failed");
                        break;
                    }
                };
                let params = match self
                    .chain
                    .compute_expected_parameters(height, &block_proposed_update)
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(height, error = %e, "compute_expected_parameters failed");
                        break;
                    }
                };
                let expected_pu = self.chain.active_proposed_update_bytes().await;
                (Some(params), Some(expected_pu))
            } else {
                (None, None)
            };

            let result = self.validator.as_mut().unwrap().apply_state(
                &header,
                &block_txs,
                ad_proofs.as_deref(),
                &extension,
                &preceding,
                &active_params,
                expected_boundary_params.as_ref(),
                expected_proposed_update.as_deref(),
            );

            match result {
                Ok(outcome) => {
                    // If this was an epoch-boundary block, apply the new parameters
                    // and proposed-update bytes to chain state atomically. The
                    // validator already verified they match expected.
                    if let (Some(new_params), Some(new_pu)) = (
                        outcome.epoch_boundary_params,
                        outcome.epoch_boundary_proposed_update,
                    ) {
                        self.chain
                            .apply_epoch_boundary_parameters(new_params, new_pu)
                            .await;
                    }
                    validated_to = height;

                    tracing::info!(
                        height = height as u64,
                        id = %header.id,
                        "block applied"
                    );

                    // Publish the window estimate after each applied block —
                    // the cadence the contract specifies, and the one that
                    // matters: the window is at its widest during exactly the
                    // catch-up sync this figure exists to characterise.
                    self.publish_window_estimate();

                    // Periodic catch-up record. Placed before the flush below so
                    // the heap reading is the pre-flush peak. Time-gated and
                    // catch-up-only inside `maybe_emit`; the probe closure runs
                    // only when a record actually fires. See
                    // [`crate::catchup_progress`] and `../facts/sync.md`.
                    //
                    // The height is read from the validator tip, not
                    // `self.state_applied_height`: that field is a cache
                    // reconciled after the loop, so mid-sweep it is frozen at
                    // the pre-sweep tip and this record would report a height
                    // the sweep passed some time ago.
                    let applied = self
                        .validator
                        .as_ref()
                        .map_or(validated_to, |v| v.validated_height());
                    self.catchup_progress
                        .maybe_emit(Instant::now(), sweep_size, applied, || {
                            self.config.flush_probe.as_ref().map(|probe| probe())
                        });

                    // Progress report every 1000 blocks during large sweeps
                    let done = height - applied_tip;
                    if done.is_multiple_of(1000) && sweep_size > 100 {
                        let elapsed = sweep_start.elapsed().as_secs().max(1);
                        let rate = done as f64 / elapsed as f64;
                        let remaining = sweep_to - height;
                        let eta_secs = if rate > 0.0 {
                            remaining as f64 / rate
                        } else {
                            0.0
                        };
                        tracing::debug!(
                            height,
                            done,
                            remaining,
                            elapsed_secs = elapsed,
                            rate = format!("{rate:.0}/s"),
                            eta = format!("{:.0}m", eta_secs / 60.0),
                            "validation progress"
                        );
                    }

                    // Memory-aware flush. `should_flush` applies the configured
                    // heap threshold + min/max block guardrails. See the method
                    // doc for the full policy. Keeps crash-recovery work bounded
                    // by `flush_max_blocks` and peak heap bounded by
                    // `flush_heap_threshold_mb` (when a probe is wired).
                    if self.should_flush(height) {
                        let outcome = try_flush_validator(self.validator.as_ref(), height);
                        complete_store_flush_pair(outcome, &self.store).await;
                        if let Some(m) = outcome.committed_height() {
                            maybe_prune_at_horizon(
                                &self.store,
                                &self.chain,
                                m,
                                self.config.blocks_to_keep,
                            )
                            .await;
                        }
                        if outcome.advances_last_flush() {
                            self.last_flush_height = height;
                        }
                    }
                }
                Err(e) => {
                    // Label the stall so the backoff's validation_stuck — if
                    // this frontier wedges for 5 sweeps — names the apply_state
                    // error kind and, for the AVL missing-key case, the key.
                    //
                    // Classified from the typed error, never from the rendered
                    // string below. The string is for the human reading the log;
                    // routing a contract field through it is how the
                    // script-failure kind went missing without a compile error
                    // when deferred evaluation was removed.
                    let (error_kind, missing_key) = classify_apply_state_error(&e);
                    stall_detail = StallDetail {
                        error_kind,
                        missing_key,
                    };
                    tracing::error!(height, error = %e, "apply_state failed");
                    break;
                }
            }
        }

        // Reconcile the cache to the validator's own tip — the ground truth
        // after every apply AND every mid-sweep rollback. Committing the
        // loop-local `validated_to` here instead would let a rollback that
        // fired after the last successful apply leave `state_applied_height`
        // ahead of the prover, which then feeds the next sweep a
        // non-consecutive start (the wedge). `validated_height()` can only be
        // what the prover actually holds; the cache must mirror it in both
        // directions. Persistence is handled atomically inside state.redb by
        // `UtxoValidator::apply_state` — no separate hint write needed.
        let true_tip = self
            .validator
            .as_ref()
            .map_or(validated_to, |v| v.validated_height());

        // Drive the sweep backoff off the authoritative applied tip. If the
        // sweep moved the tip past where it started, that is real progress —
        // clear any backoff. If it did not, the frontier is failing
        // deterministically on `apply_state`; arm/escalate
        // the exponential delay so the next retry is throttled, and — once the
        // frontier has stalled 5 sweeps in a row — emit the contract
        // `validation_stuck` event, labelled by `stall_detail`. This
        // per-engage WARN is the finer-grained signal — once per actual retry,
        // distinct from `validation_stuck` and deliberately not doubling it.
        if let Some(stall) =
            self.sweep_backoff
                .record(applied_tip, true_tip, Instant::now(), stall_detail)
        {
            tracing::warn!(
                height = applied_tip as u64,
                attempt = stall.attempt,
                delay_secs = stall.delay.as_secs(),
                "validation sweep stalled at frontier; backing off retry"
            );
        }

        if true_tip != self.state_applied_height {
            let prev = self.state_applied_height;
            self.state_applied_height = true_tip;

            if true_tip > prev {
                let advanced = true_tip - prev;
                if sweep_size > 100 {
                    let elapsed = sweep_start.elapsed();
                    let secs = elapsed.as_secs().max(1);
                    let rate = advanced as f64 / secs as f64;
                    tracing::info!(
                        from = sweep_from as u64,
                        to = true_tip as u64,
                        blocks = advanced as u64,
                        elapsed = format!("{}m{}s", secs / 60, secs % 60),
                        rate = format!("{rate:.0}/s"),
                        "VALIDATION SWEEP COMPLETE"
                    );
                } else {
                    tracing::debug!(
                        state_applied_height = true_tip,
                        advanced,
                        downloaded_height = self.downloaded_height,
                        "state applied height advanced"
                    );
                }
            } else {
                // Cache was stale-ahead of the prover (a reorg rollback that
                // landed after the cache was last written) — pulled back down
                // to the truth.
                tracing::debug!(
                    state_applied_height = true_tip,
                    previous = prev,
                    "state applied height reconciled down to validator tip"
                );
            }
        }

        // Durable flush at sweep end — catches the tail of blocks that
        // didn't hit a heap-threshold or max-blocks trigger mid-sweep. The
        // tip-following path (single block per sweep) always reaches this.
        let outcome = try_flush_validator(self.validator.as_ref(), self.state_applied_height);
        complete_store_flush_pair(outcome, &self.store).await;
        if let Some(m) = outcome.committed_height() {
            maybe_prune_at_horizon(&self.store, &self.chain, m, self.config.blocks_to_keep).await;
        }
        if outcome.advances_last_flush() {
            self.last_flush_height = self.state_applied_height;
        }
    }

    /// Handle a control-plane event (Reorg or NeedModifier).
    async fn handle_control_event(&mut self, ctrl: DeliveryControl, peer: PeerId) {
        match ctrl {
            DeliveryControl::NeedModifier { type_id, id } => {
                tracing::info!(type_id, "pipeline needs modifier for reorg");
                self.request_announced(peer, type_id, vec![id]).await;
            }
            DeliveryControl::Reorg {
                fork_point,
                old_tip,
                new_tip,
            } => {
                tracing::info!(
                    fork_point,
                    old_tip,
                    new_tip,
                    "reorg: adjusting section queue and watermark"
                );

                // No in-flight results to invalidate — evaluation is
                // synchronous.

                // Reset watermarks if they were above the fork point
                if self.downloaded_height > fork_point {
                    tracing::info!(
                        old = self.downloaded_height,
                        new = fork_point,
                        "resetting downloaded_height to fork point"
                    );
                    self.downloaded_height = fork_point;
                }
                if self.state_applied_height > fork_point {
                    // The state watermark may only retreat if the validator
                    // actually rolled back — on a failed rollback the state
                    // genuinely sits where it was, and retreating the
                    // watermark onto un-rolled state is the gap-wedge hole
                    // (see `../facts/sync.md`). No validator (light mode) =
                    // nothing to roll, the watermark is sync-local.
                    let rolled_back = match self.validator.as_mut() {
                        Some(v) => match self.chain.header_at(fork_point).await {
                            Some(fork_header) => {
                                match v.reset_to(fork_point, fork_header.state_root) {
                                    Ok(()) => true,
                                    Err(e) => {
                                        tracing::error!(
                                            height = fork_point as u64,
                                            path = "reorg",
                                            error = %e,
                                            "validation rollback failed"
                                        );
                                        false
                                    }
                                }
                            }
                            None => {
                                tracing::error!(
                                    fork_point,
                                    "cannot find fork-point header for rollback"
                                );
                                false
                            }
                        },
                        None => true,
                    };
                    if rolled_back {
                        self.state_applied_height = fork_point;
                    }
                }

                // Purge pending requests — they're for the wrong branch
                self.tracker.purge_all();

                // Re-scan watermark and request sections for the new branch
                self.advance_downloaded_height().await;
                self.request_next_sections().await;
            }
        }
    }

    /// Handle a single P2P event during sync.
    async fn handle_event(&mut self, peer: PeerId, event: ProtocolEvent) -> EventResult {
        match event {
            ProtocolEvent::Message { peer_id, message } => match message {
                // Inv with header IDs: request them
                ProtocolMessage::Inv { modifier_type, ids }
                    if modifier_type == HEADER_TYPE_ID && !ids.is_empty() =>
                {
                    tracing::debug!(count = ids.len(), "Inv → requesting headers");
                    self.request_announced(peer_id, modifier_type, ids).await;
                    EventResult::Continue
                }

                // Empty Inv: peer has no more headers
                ProtocolMessage::Inv { modifier_type, .. } if modifier_type == HEADER_TYPE_ID => {
                    let height = self.chain.chain_height().await;
                    tracing::debug!(height, "peer reports no more headers");
                    EventResult::Synced
                }

                // Inv with block section IDs: request them
                ProtocolMessage::Inv { modifier_type, ids }
                    if is_block_section_type(modifier_type) && !ids.is_empty() =>
                {
                    tracing::debug!(
                        type_id = modifier_type,
                        count = ids.len(),
                        "block section Inv → requesting"
                    );
                    self.request_announced(peer_id, modifier_type, ids).await;
                    EventResult::Continue
                }

                // Peer's SyncInfo: respond with ours to keep the bidirectional
                // exchange alive (the JVM expects this — without it, per-peer
                // sync state goes stale and the JVM stops sending Inv), and
                // serve a continuation Inv when the sender's chain is behind
                // or forked from ours — the other half of the bidirectional
                // exchange (JVM processSyncV1/V2: Younger | Fork →
                // continuationIds(400) → sendExtension). Without the serve
                // side, a from-genesis peer syncing FROM us stalls at height
                // 0 forever. See ../facts/sync.md § "Serving sync".
                ProtocolMessage::SyncInfo { body } => {
                    let mut result = EventResult::Continue;
                    if let Ok(info) = self.chain.parse_sync_info(&body) {
                        let peer_heights = C::sync_info_heights(&info);
                        let our_height = self.chain.chain_height().await;
                        let peer_tip = peer_heights.first().copied();

                        if let Some(peer_tip) = peer_tip {
                            // Publish the max peer tip we've seen — main crate
                            // reads this for the bootstrap gap decision.
                            let prev = self
                                .peer_chain_tip
                                .load(std::sync::atomic::Ordering::Relaxed);
                            if peer_tip > prev {
                                self.peer_chain_tip
                                    .store(peer_tip, std::sync::atomic::Ordering::Relaxed);
                            }

                            // "Caught up" only counts from the peer we're syncing from
                            if peer_id == peer && peer_tip <= our_height {
                                tracing::debug!(our_height, peer_tip, "caught up with peer");
                                result = EventResult::Synced;
                            } else if peer_tip > our_height + 1 {
                                // Any peer ahead of us triggers a switch
                                tracing::debug!(our_height, peer_tip, peer = %peer_id, "peer is ahead, resuming sync");
                                result = EventResult::BehindPeer(peer_id);
                            }
                        }

                        // Serve the continuation. A peer strictly ahead is
                        // never served (JVM: Older — we request from them
                        // instead); equal or unknown chains self-gate via an
                        // empty continuation, mirroring JVM sendExtension's
                        // no-op on an empty extension. Empty anchors = fresh
                        // peer = served from height 1. An empty own chain has
                        // nothing to serve. Runs even when `result` flips to
                        // Synced — a behind sync-peer still gets its Inv.
                        let peer_strictly_ahead = peer_tip.is_some_and(|t| t > our_height);
                        if our_height > 0 && !peer_strictly_ahead {
                            let anchors = sync_info_anchor_ids(&info);
                            let ids = self
                                .chain
                                .continuation_ids(&anchors, CONTINUATION_IDS_LIMIT)
                                .await;
                            if !ids.is_empty() {
                                tracing::debug!(
                                    peer = %peer_id,
                                    count = ids.len(),
                                    "peer behind/forked → serving continuation Inv"
                                );
                                let _ = self
                                    .transport
                                    .send_to(
                                        peer_id,
                                        ProtocolMessage::Inv {
                                            modifier_type: HEADER_TYPE_ID,
                                            ids,
                                        },
                                    )
                                    .await;
                            }
                        }
                    }

                    // Respond with our SyncInfo — mirrors JVM's syncSendNeeded
                    // behavior. Skipped on state transitions, preserving the
                    // pre-serve control flow where Synced/BehindPeer returned
                    // before reaching this send. Addressed to the SENDER
                    // (`peer_id`), not our active sync peer (`peer`): the JVM's
                    // processSync replies to `remote`, and this response is the
                    // only caught-up signal a peer syncing FROM us receives
                    // (empty continuation ⇒ no Inv). Misaddressing it to our
                    // sync peer strands the requester in the header loop
                    // forever. See ../facts/sync.md § "Serving sync".
                    if matches!(result, EventResult::Continue) {
                        let _ = self.send_sync_info(peer_id).await;
                    }
                    result
                }

                // Transaction Inv: request unconfirmed txs from peers
                ProtocolMessage::Inv { modifier_type, ids }
                    if modifier_type == TRANSACTION_TYPE_ID && !ids.is_empty() =>
                {
                    tracing::debug!(count = ids.len(), "tx Inv → requesting transactions");
                    self.request_announced(peer_id, modifier_type, ids).await;
                    EventResult::Continue
                }

                ProtocolMessage::Inv {
                    modifier_type,
                    ref ids,
                } => {
                    tracing::debug!(
                        peer = %peer_id,
                        modifier_type,
                        count = ids.len(),
                        "unhandled Inv type"
                    );
                    EventResult::Continue
                }

                other => {
                    tracing::debug!(
                        peer = %peer_id,
                        msg_type = %msg_type_name(&other),
                        "unhandled message from peer"
                    );
                    EventResult::Continue
                }
            },

            ProtocolEvent::PeerDisconnected { peer_id, .. } if peer_id == peer => {
                tracing::info!(peer = %peer_id, "sync peer disconnected");
                EventResult::PeerGone
            }

            _ => EventResult::Continue,
        }
    }

    /// Handle delivery check results: re-request timed-out and evicted modifiers.
    async fn handle_delivery_check(
        &mut self,
        result: crate::delivery::CheckResult,
        _current_peer: PeerId,
    ) {
        // Re-request timed-out modifiers from a different peer
        if !result.retries.is_empty()
            && self
                .block_request_gate
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            let peers = self.transport.outbound_peers().await;
            for retry in &result.retries {
                let target = peers
                    .iter()
                    .find(|&&p| p != retry.failed_peer)
                    .copied()
                    .unwrap_or(retry.failed_peer);

                self.tracker
                    .mark_requested(&[retry.id], target, retry.type_id);
                let _ = self
                    .transport
                    .send_to(
                        target,
                        ProtocolMessage::ModifierRequest {
                            modifier_type: retry.type_id,
                            ids: vec![retry.id],
                        },
                    )
                    .await;
            }
            tracing::debug!(
                count = result.retries.len(),
                "re-requested timed-out modifiers"
            );
        }

        // Re-request evicted modifiers (LRU buffer evictions — headers only)
        if !result.fresh.is_empty() {
            self.rerequest_from_any(HEADER_TYPE_ID, &result.fresh).await;
        }

        if !result.abandoned.is_empty() {
            tracing::warn!(
                count = result.abandoned.len(),
                "abandoned modifiers after max delivery attempts"
            );
        }

        let pending = self.tracker.pending_count();
        if pending > 0 {
            tracing::debug!(pending, "delivery tracker status");
        }
    }

    /// Re-request modifier IDs from any available outbound peer.
    async fn rerequest_from_any(&mut self, modifier_type: u8, ids: &[[u8; 32]]) {
        let peers = self.transport.outbound_peers().await;
        if let Some(&target) = peers.first() {
            self.request_announced(target, modifier_type, ids.to_vec())
                .await;
            tracing::debug!(count = ids.len(), peer = %target, "re-requested modifiers");
        }
    }

    /// At-tip predicate window. The handshake fires when
    /// `validator.validated_height() + AT_TIP_WINDOW >= chain.chain_height()`.
    /// 16 blocks is bigger than typical reorg depth and small enough that any
    /// remaining replay on the cold-sync cache before the swap is negligible.
    const AT_TIP_WINDOW: u32 = 16;

    /// Run the at-tip transition (flush settings swap + storage reopen) iff
    /// the validator is within `AT_TIP_WINDOW` of the header tip.
    ///
    /// Both transitions are idempotent via `Option::take`: once fired, the
    /// channels and `synced_*` overrides are `None` and subsequent calls are
    /// no-ops. Safe to invoke on `synced()` entry and on every section ticker.
    ///
    /// On flush failure or channel close the validator is restored / kept,
    /// but the channels have already been taken — the handshake won't retry
    /// in this process. Operator restart is the documented recovery path
    /// (cold-sync re-opens with `cache_mb` and re-attempts at next tip arrival).
    async fn maybe_fire_at_tip(&mut self) {
        // Fast-path: nothing left to do. Avoids the chain lock acquisition
        // every section_ticker tick once the transition has already fired
        // (or was never wired in the first place).
        let nothing_left = self.at_tip_request_tx.is_none()
            && self.at_tip_validator_rx.is_none()
            && self.synced_cache_bytes.is_none()
            && self.config.synced_flush_heap_threshold_mb.is_none()
            && self.config.synced_flush_max_blocks.is_none()
            && self.config.synced_flush_min_blocks.is_none();
        if nothing_left {
            return;
        }

        // Gate on validator-tip proximity. Without a validator (light mode,
        // or pre-snapshot-bootstrap before validator_rx delivers) we have no
        // height to compare and nothing to swap into anyway — defer.
        let validator_h = match self.validator.as_ref() {
            Some(v) => v.validated_height(),
            None => return,
        };
        let chain_h = self.chain.chain_height().await;
        if validator_h.saturating_add(Self::AT_TIP_WINDOW) < chain_h {
            return;
        }

        // ── Flush settings swap (option a: gated alongside cache reopen) ──
        if let Some(v) = self.config.synced_flush_heap_threshold_mb.take() {
            tracing::info!(
                from = self.config.flush_heap_threshold_mb,
                to = v,
                "at-tip: switching flush_heap_threshold_mb"
            );
            self.config.flush_heap_threshold_mb = v;
        }
        if let Some(v) = self.config.synced_flush_max_blocks.take() {
            tracing::info!(
                from = self.config.flush_max_blocks,
                to = v,
                "at-tip: switching flush_max_blocks"
            );
            self.config.flush_max_blocks = v;
        }
        if let Some(v) = self.config.synced_flush_min_blocks.take() {
            tracing::info!(
                from = self.config.flush_min_blocks,
                to = v,
                "at-tip: switching flush_min_blocks"
            );
            self.config.flush_min_blocks = v;
        }

        // ── In-place cache resize (no reopen, no second mmap) ──
        if let Some(cache_bytes) = self.synced_cache_bytes.take() {
            // Only a validator that owns storage has a cache to resize.
            // Digest mode has none — see facts/validation.md § "Traits".
            match self.validator.as_ref().and_then(|v| v.state_persistence()) {
                Some(persistence) => {
                    if let Err(e) = persistence.resize_cache(cache_bytes) {
                        tracing::error!(
                            cache_bytes,
                            error = ?e,
                            "at-tip: resize_cache failed; continuing with cold-sync cache size"
                        );
                    } else {
                        tracing::info!(
                            cache_bytes,
                            "at-tip: cache resized in-place on existing storage handle"
                        );
                    }
                }
                None => tracing::debug!(
                    cache_bytes,
                    "at-tip: validator owns no persistent state; no cache to resize"
                ),
            }
        }

        // ── Storage reopen handshake (digest-mode) ──
        if let (Some(req_tx), Some(val_rx)) = (
            self.at_tip_request_tx.take(),
            self.at_tip_validator_rx.take(),
        ) {
            if let Some(validator) = self.validator.take() {
                let height = validator.validated_height();
                // Persist any pending in-memory state BEFORE dropping. Without
                // this, blocks applied since the last cold-sync flush remain in
                // the redb write tx and are GC'd with the prover. The rebuilt
                // validator would then load the older stored digest while sync
                // (and the new validator's height field) believe state is at
                // `height`, leaving an N-block gap where any later block that
                // spends an output from the gap fails with "Key does not exist".
                //
                // A validator that owns no persistent state (digest mode) has
                // no write tx to lose, so the absence of `StatePersistence`
                // proceeds to the rebuild; only a real `Err` aborts it
                // (../facts/sync.md § "At-tip Storage Reopen", step 1).
                if let Some(Err(e)) = validator.state_persistence().map(|p| p.flush()) {
                    tracing::error!(
                        height,
                        error = ?e,
                        "at-tip: flush before rebuild failed; aborting rebuild and keeping old validator"
                    );
                    self.validator = Some(validator);
                    return;
                }
                drop(validator); // releases AVL storage so main can reopen it
                tracing::info!(
                    height,
                    "at-tip: requesting validator rebuild with synced cache"
                );
                if req_tx.send(height).is_err() {
                    tracing::error!("at-tip: failed to signal main; continuing without rebuild");
                    return;
                }
                match val_rx.await {
                    Ok(new_validator) => {
                        debug_assert_eq!(
                            new_validator.validated_height(),
                            height,
                            "at-tip: rebuilt validator height does not match flushed height"
                        );
                        tracing::info!(height, "at-tip: validator rebuilt with synced cache");
                        self.validator = Some(new_validator);
                    }
                    Err(_) => {
                        tracing::error!(
                            "at-tip: validator rebuild channel closed; sync cannot proceed"
                        );
                    }
                }
            }
        }
    }

    /// Synced: periodically check for new blocks, download block sections.
    async fn synced(&mut self) {
        let tip_height = self.chain.chain_height().await;
        tracing::info!(height = tip_height as u64, "chain tip reached");

        // The at-tip transition (flush settings swap + storage reopen) is
        // gated on validator-tip proximity, not on `synced()` entry. The
        // header chain reaches tip well before validator catches up after
        // snapshot bootstrap, and firing prematurely runs the entire
        // catch-up replay on the smaller `synced_cache_mb` cache. See
        // [`Self::maybe_fire_at_tip`].
        self.maybe_fire_at_tip().await;

        let mut ticker = tokio::time::interval(self.config.synced_poll_interval);
        // Section download timer — recomputes the sliding window every 2s
        let mut section_ticker = tokio::time::interval(Duration::from_secs(2));
        // Delivery timeout check — cleans up stale pending requests
        let mut delivery_ticker = tokio::time::interval(self.config.delivery_check_interval);

        // Request first batch on entry
        self.request_next_sections().await;

        loop {
            tokio::select! {
                biased;

                // Control-plane events — checked first, even while synced
                Some(ctrl) = self.delivery_control_rx.recv() => {
                    let peer = self.sync_peer.unwrap_or(PeerId(0));
                    self.handle_control_event(ctrl, peer).await;
                }

                _ = ticker.tick() => {
                    let peers = self.transport.outbound_peers().await;
                    if let Some(&peer) = peers.first() {
                        let _ = self.send_sync_info(peer).await;
                    } else {
                        tracing::debug!("no outbound peers, returning to idle");
                        self.sync_peer = None;
                        return;
                    }

                    self.advance_downloaded_height().await;
                }

                // Delivery timeout: expire stale requests, free pending slots
                _ = delivery_ticker.tick() => {
                    let peer = self.sync_peer.unwrap_or(PeerId(0));
                    let result = self.tracker.check_timeouts();
                    self.handle_delivery_check(result, peer).await;
                }

                // Sliding window section download — recompute and request every 2s.
                // Also re-evaluates the at-tip gate so the handshake fires the
                // moment validator catches up to the header tip during a long
                // snapshot-bootstrap replay, without bouncing through
                // sync_from_peer to re-enter `synced()`.
                _ = section_ticker.tick() => {
                    self.advance_downloaded_height().await;
                    self.request_next_sections().await;
                    self.maybe_fire_at_tip().await;
                }

                event = self.transport.next_event() => {
                    match event {
                        Some(event) => {
                            let peer = self.sync_peer.unwrap_or(PeerId(0));
                            match self.handle_event(peer, event).await {
                                EventResult::Continue | EventResult::Synced => {}
                                EventResult::BehindPeer(ahead_peer) => {
                                    self.sync_peer = Some(ahead_peer);
                                    self.stalled_peers.clear();
                                    return;
                                }
                                EventResult::PeerGone => {
                                    self.sync_peer = None;
                                    return;
                                }
                            }
                        }
                        None => {
                            self.sync_peer = None;
                            return;
                        }
                    }
                }

                Some(height) = self.progress.recv() => {
                    tracing::debug!(height, "pipeline progress while synced");
                    self.request_next_sections().await;
                }

                // Data-plane delivery notifications: received / evicted modifiers.
                // Mirrors the arm in `sync_from_peer`. Without this, the
                // pipeline's try_send on the bounded delivery_data channel fills,
                // notifications get dropped, and `tracker.mark_received` is never
                // called for arrived sections — so the tracker treats them as
                // pending forever, and `request_announced` skips re-requesting
                // (and skips requesting the next window, because pending count
                // saturates the per-window picks). End result: peers keep
                // streaming data the node ignores, fullHeight stalls until a
                // restart clears the tracker.
                Some(data) = self.delivery_data_rx.recv() => {
                    match data {
                        DeliveryData::Received(ids) => {
                            for id in &ids {
                                self.tracker.mark_received(id);
                            }
                            self.advance_downloaded_height().await;
                        }
                        DeliveryData::Evicted(ids) => {
                            self.tracker.schedule_rerequest(&ids);
                        }
                    }
                }
            }
        }
    }

    /// Compute and request the next batch of block sections using a sliding window.
    ///
    /// Scans from `downloaded_height + 1` up to `downloaded_height + DOWNLOAD_WINDOW`,
    /// finds sections not yet in the store and not already pending in the tracker,
    /// and requests them. Recomputed each cycle — no persistent queue.
    ///
    /// Mirrors JVM's `ToDownloadProcessor.nextModifiersToDownload` with a 192-block
    /// forward window.
    async fn request_next_sections(&mut self) {
        let chain_height = self.chain.chain_height().await;
        if self.downloaded_height >= chain_height {
            return;
        }

        let peers = self.transport.outbound_peers().await;
        if peers.is_empty() {
            return;
        }

        let window_end =
            std::cmp::min(self.downloaded_height + Self::DOWNLOAD_WINDOW, chain_height);

        let mut by_type: HashMap<u8, Vec<[u8; 32]>> = HashMap::new();
        let mut total = 0usize;

        for height in (self.downloaded_height + 1)..=window_end {
            let header = match self.chain.header_at(height).await {
                Some(h) => h,
                None => break, // gap in header chain — stop
            };
            let sections = enr_chain::required_section_ids(&header, self.config.state_type);
            for (type_id, id) in &sections {
                // Skip if already in store or already pending
                if self.store.has_modifier(*type_id, id).await {
                    continue;
                }
                if self.tracker.is_pending(id) {
                    continue;
                }
                by_type.entry(*type_id).or_default().push(*id);
                total += 1;
            }
        }

        if total == 0 {
            return;
        }

        // Send to all outbound peers — distribute the load
        let mut sent = 0usize;
        for (type_id, ids) in &by_type {
            if ids.is_empty() {
                continue;
            }
            for &peer in &peers {
                self.request_announced(peer, *type_id, ids.clone()).await;
            }
            sent += ids.len();
        }

        if sent > 0 {
            tracing::debug!(
                sent,
                peer_count = peers.len(),
                window = format!("{}..{}", self.downloaded_height + 1, window_end),
                "requested block sections"
            );
        }
    }

    /// Send our current SyncInfo to a peer, respecting the JVM's PerPeerSyncLockTime.
    /// Returns Ok(()) even if rate-limited (skipped sends are not errors).
    async fn send_sync_info(
        &mut self,
        peer: PeerId,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        // Rate-limit: JVM drops SyncInfo received within 100ms of previous
        if self.last_sync_sent.elapsed() < self.config.min_sync_send_interval {
            return Ok(());
        }

        let body = self.chain.build_sync_info().await;
        self.sync_sent_count += 1;
        self.last_sync_sent = Instant::now();

        // Diagnostic: log SyncInfo content + first 40 hex bytes for wire analysis
        if let Ok(info) = self.chain.parse_sync_info(&body) {
            let heights = C::sync_info_heights(&info);
            let hex_prefix: String = body
                .iter()
                .take(40)
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" ");
            tracing::debug!(
                body_len = body.len(),
                headers = ?heights,
                count = self.sync_sent_count,
                hex = %hex_prefix,
                "sending SyncInfo"
            );
        }

        self.transport
            .send_to(peer, ProtocolMessage::SyncInfo { body })
            .await
    }
}

/// Result of handling a single event.
enum EventResult {
    /// Keep syncing.
    Continue,
    /// Caught up with the peer.
    Synced,
    /// A peer has a significantly higher chain — resume header sync.
    BehindPeer(PeerId),
    /// Sync peer disconnected.
    PeerGone,
}

/// Outcome of a sync_from_peer session.
enum SyncOutcome {
    /// Caught up with the peer's chain tip.
    Synced,
    /// No progress for stall_timeout — rotate peer.
    Stalled,
    /// A different peer has more headers — switch to it.
    SwitchPeer,
    /// Sync peer disconnected.
    PeerDisconnected,
    /// Event stream closed (shutdown).
    StreamEnded,
}

/// Human-readable message type name for diagnostics.
fn msg_type_name(msg: &ProtocolMessage) -> &'static str {
    match msg {
        ProtocolMessage::GetPeers => "GetPeers",
        ProtocolMessage::Peers { .. } => "Peers",
        ProtocolMessage::SyncInfo { .. } => "SyncInfo",
        ProtocolMessage::Inv { .. } => "Inv",
        ProtocolMessage::ModifierRequest { .. } => "ModifierRequest",
        ProtocolMessage::ModifierResponse { .. } => "ModifierResponse",
        ProtocolMessage::Unknown { code, .. } => {
            // Leak a static string for unknown codes — only a few will ever exist
            // and this is diagnostics-only code
            match code {
                75 => "GetNipopowProof",
                76 => "NipopowProof",
                77 => "GetSnapshotsInfo",
                78 => "SnapshotsInfo",
                _ => "Unknown",
            }
        }
    }
}

#[cfg(test)]
mod cross_db_flush_tests {
    //! Tests for the cross-DB durability handshake (`facts/sync.md`).
    //!
    //! The invariant under test: a successful `validator.flush()` MUST be
    //! followed by `store.set_validated_height(M)` BEFORE `store.flush()`,
    //! where `M = validator.validated_height()`. A failed validator flush
    //! MUST NOT touch `validated_height`. Light mode (no validator) skips
    //! both — there is no validated_height to record.
    use super::*;
    use ergo_chain_types::{ADDigest, Header};
    use ergo_validation::{
        ApplyStateOutcome, BlockValidator, Parameters, StatePersistence, ValidationError,
    };
    use std::sync::Mutex;

    /// Recorded call against the mock store. Order is the assertion target.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum StoreCall {
        SetValidatedHeight(u32),
        Flush,
    }

    /// Mock SyncStore for cross-DB flush ordering tests. Records every call
    /// in invocation order; tests assert on the recorded sequence.
    struct MockStore {
        calls: Mutex<Vec<StoreCall>>,
        validated_height: Mutex<Option<u32>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                validated_height: Mutex::new(None),
            }
        }

        fn calls(&self) -> Vec<StoreCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SyncStore for MockStore {
        async fn has_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> bool {
            unimplemented!("not called in flush-ordering tests")
        }

        async fn get_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> Option<Vec<u8>> {
            unimplemented!("not called in flush-ordering tests")
        }

        async fn validated_height(&self) -> Option<u32> {
            *self.validated_height.lock().unwrap()
        }

        async fn set_validated_height(&self, height: u32) {
            self.calls
                .lock()
                .unwrap()
                .push(StoreCall::SetValidatedHeight(height));
            *self.validated_height.lock().unwrap() = Some(height);
        }

        async fn flush(&self) {
            self.calls.lock().unwrap().push(StoreCall::Flush);
        }

        async fn prune_below_height(
            &self,
            _horizon: u32,
            _type_ids: &[u8],
        ) -> Result<usize, String> {
            // Cross-DB flush tests run with blocks_to_keep = -1 (default),
            // so the prune path is gated off and this is never called.
            unreachable!("prune_below_height not exercised by cross-DB flush tests")
        }

        async fn min_height_present(&self, _type_id: u8) -> Result<Option<u32>, String> {
            unreachable!("min_height_present not exercised by cross-DB flush tests")
        }
    }

    /// Fake BlockValidator with a controllable flush result. Other trait
    /// methods are not exercised by these tests.
    struct FakeValidator {
        validated_height: u32,
        flush_result: Result<(), &'static str>,
        digest: ADDigest,
    }

    impl FakeValidator {
        fn flushes_ok(height: u32) -> Self {
            Self {
                validated_height: height,
                flush_result: Ok(()),
                digest: ADDigest::zero(),
            }
        }

        fn flushes_err(height: u32) -> Self {
            Self {
                validated_height: height,
                flush_result: Err("simulated flush failure"),
                digest: ADDigest::zero(),
            }
        }
    }

    impl BlockValidator for FakeValidator {
        fn apply_state(
            &mut self,
            _header: &Header,
            _block_txs: &[u8],
            _ad_proofs: Option<&[u8]>,
            _extension: &[u8],
            _preceding_headers: &[Header],
            _active_params: &Parameters,
            _expected_boundary_params: Option<&Parameters>,
            _expected_proposed_update: Option<&[u8]>,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            unimplemented!("not called in flush-ordering tests")
        }

        fn validated_height(&self) -> u32 {
            self.validated_height
        }

        fn current_digest(&self) -> &ADDigest {
            &self.digest
        }

        fn reset_to(&mut self, _height: u32, _digest: ADDigest) -> Result<(), ValidationError> {
            unimplemented!("not called in flush-ordering tests")
        }

        fn state_persistence(&self) -> Option<&dyn StatePersistence> {
            Some(self)
        }
    }

    impl StatePersistence for FakeValidator {
        fn flush(&self) -> Result<(), ValidationError> {
            self.flush_result
                .map_err(|s| ValidationError::ProofVerificationFailed(s.to_string()))
        }

        fn resize_cache(&self, _cache_bytes: usize) -> Result<(), ValidationError> {
            unimplemented!("not called in flush-ordering tests")
        }

        /// No prover behind this double, so nothing to size.
        fn prover_memory_estimate(&self) -> Option<ergo_validation::ProverMemoryEstimate> {
            None
        }
    }

    /// Digest-mode shape: a validator that owns no persistent state. Its
    /// `state_persistence()` is `None`, which is NOT the light-mode
    /// no-validator case — the store side of the pair still runs.
    struct NoPersistenceValidator {
        validated_height: u32,
        digest: ADDigest,
    }

    impl BlockValidator for NoPersistenceValidator {
        fn apply_state(
            &mut self,
            _header: &Header,
            _block_txs: &[u8],
            _ad_proofs: Option<&[u8]>,
            _extension: &[u8],
            _preceding_headers: &[Header],
            _active_params: &Parameters,
            _expected_boundary_params: Option<&Parameters>,
            _expected_proposed_update: Option<&[u8]>,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            unimplemented!("not called in flush-ordering tests")
        }

        fn validated_height(&self) -> u32 {
            self.validated_height
        }

        fn current_digest(&self) -> &ADDigest {
            &self.digest
        }

        fn reset_to(&mut self, _height: u32, _digest: ADDigest) -> Result<(), ValidationError> {
            unimplemented!("not called in flush-ordering tests")
        }

        fn state_persistence(&self) -> Option<&dyn StatePersistence> {
            None
        }
    }

    #[tokio::test]
    async fn records_validated_height_before_store_flush_on_success() {
        let store = MockStore::new();
        let validator = FakeValidator::flushes_ok(1_785_000);

        let outcome = try_flush_validator(Some(&validator), 1_785_000);
        complete_store_flush_pair(outcome, &store).await;

        assert_eq!(outcome, FlushOutcome::Flushed(1_785_000));
        assert_eq!(
            store.calls(),
            vec![StoreCall::SetValidatedHeight(1_785_000), StoreCall::Flush,],
            "set_validated_height MUST precede store.flush() (cross-DB handshake ordering)"
        );
    }

    #[tokio::test]
    async fn failed_validator_flush_does_not_advance_validated_height() {
        let store = MockStore::new();
        let validator = FakeValidator::flushes_err(1_785_000);

        let outcome = try_flush_validator(Some(&validator), 1_785_000);
        complete_store_flush_pair(outcome, &store).await;

        assert_eq!(outcome, FlushOutcome::Failed);
        assert_eq!(
            store.calls(),
            vec![StoreCall::Flush],
            "failed validator.flush() MUST NOT advance modifier store's validated_height"
        );
    }

    #[tokio::test]
    async fn no_validator_does_not_record_validated_height() {
        let store = MockStore::new();

        // Light mode: validator is None — no state to flush, no validated_height
        // to record. Modifier store flush still runs to fsync section writes.
        let outcome = try_flush_validator::<FakeValidator>(None, 0);
        complete_store_flush_pair(outcome, &store).await;

        assert_eq!(outcome, FlushOutcome::NoValidator);
        assert_eq!(
            store.calls(),
            vec![StoreCall::Flush],
            "light mode MUST NOT touch validated_height — nothing to record"
        );
    }

    #[tokio::test]
    async fn validator_without_persistence_still_records_validated_height() {
        let store = MockStore::new();
        let validator = NoPersistenceValidator {
            validated_height: 1_785_000,
            digest: ADDigest::zero(),
        };

        // Digest mode's flush writes validated_height even though no state
        // is persisted. See ../facts/sync.md § "Flushing a validator that
        // owns no state".
        let outcome = try_flush_validator(Some(&validator), 1_785_000);
        complete_store_flush_pair(outcome, &store).await;

        assert_eq!(outcome, FlushOutcome::NothingToPersist(1_785_000));
        assert_eq!(
            store.calls(),
            vec![StoreCall::SetValidatedHeight(1_785_000), StoreCall::Flush,],
            "nothing to persist is NOT the light-mode case — validated_height \
             MUST still be recorded, before store.flush()"
        );
    }

    #[test]
    fn flush_outcome_advances_last_flush_unless_the_flush_failed() {
        assert!(FlushOutcome::Flushed(42).advances_last_flush());
        assert!(FlushOutcome::NothingToPersist(42).advances_last_flush());
        assert!(FlushOutcome::NoValidator.advances_last_flush());
        assert!(!FlushOutcome::Failed.advances_last_flush());
    }

    #[test]
    fn committed_height_covers_flushed_and_nothing_to_persist_only() {
        assert_eq!(FlushOutcome::Flushed(42).committed_height(), Some(42));
        assert_eq!(
            FlushOutcome::NothingToPersist(42).committed_height(),
            Some(42),
            "digest mode records and prunes at M exactly as a real flush does"
        );
        assert_eq!(FlushOutcome::NoValidator.committed_height(), None);
        assert_eq!(FlushOutcome::Failed.committed_height(), None);
    }
}

#[cfg(test)]
mod shutdown_flush_tests {
    //! Tests for the run-loop shutdown flush (`facts/sync.md` §
    //! "Graceful shutdown").
    //!
    //! Invariant under test: every exit path of `run_inner` funnels
    //! through `shutdown_flush`, which mirrors the end-of-sweep flush
    //! block (validator.flush → store.set_validated_height(M) →
    //! store.flush). Without this, `Durability::None` commits
    //! accumulated since the last sweep flush are lost on process exit.
    use super::*;
    use enr_chain::{ChainError, SyncInfo};
    use ergo_chain_types::{ADDigest, Header};
    use ergo_validation::{
        ApplyStateOutcome, BlockValidator, Parameters, StatePersistence, ValidationError,
    };
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum StoreCall {
        SetValidatedHeight(u32),
        Flush,
    }

    struct MockStore {
        calls: Mutex<Vec<StoreCall>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<StoreCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SyncStore for MockStore {
        async fn has_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> bool {
            unreachable!("not called when chain_height=0")
        }

        async fn get_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> Option<Vec<u8>> {
            unreachable!("not called in shutdown-flush tests")
        }

        async fn validated_height(&self) -> Option<u32> {
            None
        }

        async fn set_validated_height(&self, height: u32) {
            self.calls
                .lock()
                .unwrap()
                .push(StoreCall::SetValidatedHeight(height));
        }

        async fn flush(&self) {
            self.calls.lock().unwrap().push(StoreCall::Flush);
        }

        async fn prune_below_height(
            &self,
            _horizon: u32,
            _type_ids: &[u8],
        ) -> Result<usize, String> {
            // Shutdown-flush tests run with default SyncConfig
            // (blocks_to_keep = -1) so pruning is gated off.
            unreachable!("prune_below_height not exercised by shutdown-flush tests")
        }

        async fn min_height_present(&self, _type_id: u8) -> Result<Option<u32>, String> {
            unreachable!("min_height_present not exercised by shutdown-flush tests")
        }
    }

    /// Tracks `flush()` invocations so the test can assert exactly-once.
    struct FakeValidator {
        validated_height: u32,
        flush_result: Result<(), &'static str>,
        flush_count: Mutex<u32>,
        digest: ADDigest,
    }

    impl FakeValidator {
        fn flushes_ok(height: u32) -> Self {
            Self {
                validated_height: height,
                flush_result: Ok(()),
                flush_count: Mutex::new(0),
                digest: ADDigest::zero(),
            }
        }

        fn flushes_err(height: u32) -> Self {
            Self {
                validated_height: height,
                flush_result: Err("simulated flush failure"),
                flush_count: Mutex::new(0),
                digest: ADDigest::zero(),
            }
        }

        fn flush_count(&self) -> u32 {
            *self.flush_count.lock().unwrap()
        }
    }

    impl BlockValidator for FakeValidator {
        fn apply_state(
            &mut self,
            _header: &Header,
            _block_txs: &[u8],
            _ad_proofs: Option<&[u8]>,
            _extension: &[u8],
            _preceding_headers: &[Header],
            _active_params: &Parameters,
            _expected_boundary_params: Option<&Parameters>,
            _expected_proposed_update: Option<&[u8]>,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            unreachable!("not called in shutdown-flush tests")
        }

        fn validated_height(&self) -> u32 {
            self.validated_height
        }

        fn current_digest(&self) -> &ADDigest {
            &self.digest
        }

        fn reset_to(&mut self, _height: u32, _digest: ADDigest) -> Result<(), ValidationError> {
            unreachable!("not called in shutdown-flush tests")
        }

        fn state_persistence(&self) -> Option<&dyn StatePersistence> {
            Some(self)
        }
    }

    impl StatePersistence for FakeValidator {
        fn flush(&self) -> Result<(), ValidationError> {
            *self.flush_count.lock().unwrap() += 1;
            self.flush_result
                .map_err(|s| ValidationError::ProofVerificationFailed(s.to_string()))
        }

        fn resize_cache(&self, _cache_bytes: usize) -> Result<(), ValidationError> {
            unreachable!("not called in shutdown-flush tests")
        }

        /// No prover behind this double, so nothing to size.
        fn prover_memory_estimate(&self) -> Option<ergo_validation::ProverMemoryEstimate> {
            None
        }
    }

    /// Transport that models production behavior: `outbound_peers`
    /// returns empty and `next_event` hangs forever. The events channel
    /// in production never closes (multi-Arc to `P2pNode` keeps it
    /// alive even after the host drops its reference), so the only
    /// deterministic exit is the explicit `shutdown_rx` signal.
    struct HangingTransport;

    impl SyncTransport for HangingTransport {
        async fn send_to(
            &self,
            _peer: PeerId,
            _message: ProtocolMessage,
        ) -> Result<(), Box<dyn std::error::Error + Send>> {
            unreachable!("not called when there are no peers")
        }

        async fn outbound_peers(&self) -> Vec<PeerId> {
            Vec::new()
        }

        async fn next_event(&mut self) -> Option<ProtocolEvent> {
            std::future::pending().await
        }
    }

    /// Empty chain (height 0) — the startup tip-scan and the
    /// light-bootstrap branch both fall through without exercising
    /// the rest of the `SyncChain` surface.
    struct EmptyChain;

    impl SyncChain for EmptyChain {
        async fn chain_height(&self) -> u32 {
            0
        }

        async fn build_sync_info(&self) -> Vec<u8> {
            unreachable!("not called without a sync peer")
        }

        async fn header_at(&self, _height: u32) -> Option<Header> {
            unreachable!("not called when chain_height=0")
        }

        async fn header_state_root(&self, _height: u32) -> Option<[u8; 33]> {
            unreachable!("not called in shutdown-flush tests")
        }

        fn parse_sync_info(&self, _body: &[u8]) -> Result<SyncInfo, ChainError> {
            unreachable!("not called without a sync peer")
        }

        async fn continuation_ids(
            &self,
            _peer_last_ids: &[BlockId],
            _limit: usize,
        ) -> Vec<[u8; 32]> {
            // Empty chain — nothing to serve a behind peer. The handler's
            // `our_height > 0` gate skips the call anyway.
            Vec::new()
        }

        async fn active_parameters(&self) -> Parameters {
            unreachable!("not called in shutdown-flush tests")
        }

        async fn is_epoch_boundary(&self, _height: u32) -> bool {
            unreachable!("not called in shutdown-flush tests")
        }

        async fn compute_expected_parameters(
            &self,
            _epoch_boundary_height: u32,
            _block_proposed_update: &[u8],
        ) -> Result<Parameters, ChainError> {
            unreachable!("not called in shutdown-flush tests")
        }

        async fn apply_epoch_boundary_parameters(
            &self,
            _params: Parameters,
            _proposed_update_bytes: Vec<u8>,
        ) {
            unreachable!("not called in shutdown-flush tests")
        }

        async fn active_proposed_update_bytes(&self) -> Vec<u8> {
            unreachable!("not called in shutdown-flush tests")
        }

        async fn verify_nipopow_envelope(
            &self,
            _envelope_body: &[u8],
        ) -> Result<Vec<Header>, ChainError> {
            unreachable!("not called when state_type=Utxo")
        }

        async fn is_better_nipopow(&self, _this: &[u8], _than: &[u8]) -> Result<bool, ChainError> {
            unreachable!("not called in shutdown-flush tests")
        }

        async fn install_nipopow_suffix(
            &self,
            _suffix_head: Header,
            _suffix_tail: Vec<Header>,
        ) -> Result<(), ChainError> {
            unreachable!("not called when state_type=Utxo")
        }

        async fn voting_length(&self) -> u32 {
            // Default SyncConfig has blocks_to_keep = -1 → prune/WARN
            // gates skip the chain call. Returns a sensible mainnet value
            // for completeness in case the gating ever shifts.
            1024
        }
    }

    fn build_sync(
        validator: Option<FakeValidator>,
        store: MockStore,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> HeaderSync<HangingTransport, EmptyChain, MockStore, FakeValidator> {
        let (_progress_tx, progress_rx) = mpsc::channel(1);
        let (_dc_tx, delivery_control_rx) = mpsc::unbounded_channel::<DeliveryControl>();
        let (_dd_tx, delivery_data_rx) = mpsc::channel::<DeliveryData>(1);
        HeaderSync::new(
            SyncConfig::default(),
            HangingTransport,
            EmptyChain,
            store,
            validator,
            progress_rx,
            delivery_control_rx,
            delivery_data_rx,
            None,
            None,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU64::new(0)),
            shutdown_rx,
        )
    }

    #[tokio::test]
    async fn run_flushes_state_on_shutdown_signal() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            Some(FakeValidator::flushes_ok(1_785_000)),
            MockStore::new(),
            shutdown_rx,
        );

        shutdown_tx
            .send(())
            .expect("sync must be polling shutdown_rx");
        sync.run().await;

        assert_eq!(
            sync.validator.as_ref().unwrap().flush_count(),
            1,
            "validator.flush() must be called exactly once on shutdown"
        );
        assert_eq!(
            sync.store.calls(),
            vec![StoreCall::SetValidatedHeight(1_785_000), StoreCall::Flush,],
            "shutdown_flush must mirror the end-of-sweep ordering: \
             set_validated_height(M) precedes store.flush(), where \
             M = validator.validated_height()"
        );
        assert_eq!(
            sync.last_flush_height, 1_785_000,
            "successful shutdown flush advances last_flush_height"
        );
    }

    #[tokio::test]
    async fn run_flushes_store_even_when_validator_flush_fails() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            Some(FakeValidator::flushes_err(1_785_000)),
            MockStore::new(),
            shutdown_rx,
        );

        // Dropping the sender closes the channel; the receiver resolves
        // with `Err(_)` and triggers the shutdown arm of the select.
        drop(shutdown_tx);
        sync.run().await;

        assert_eq!(
            sync.validator.as_ref().unwrap().flush_count(),
            1,
            "validator.flush() must still be attempted on shutdown"
        );
        assert_eq!(
            sync.store.calls(),
            vec![StoreCall::Flush],
            "failed validator.flush() must NOT advance modifier store's \
             validated_height, but store.flush() must still run to \
             fsync section writes"
        );
    }

    #[tokio::test]
    async fn run_flushes_store_in_light_mode_without_validator() {
        // No validator wired — the snapshot-bootstrap and light-mode
        // exit paths share this shape. shutdown_flush falls back to
        // state_applied_height for the height_context and the
        // FlushOutcome::NoValidator branch skips set_validated_height.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(None, MockStore::new(), shutdown_rx);

        drop(shutdown_tx);
        sync.run().await;

        assert_eq!(
            sync.store.calls(),
            vec![StoreCall::Flush],
            "shutdown without a validator records no validated_height \
             but still flushes the store"
        );
    }
}

#[cfg(test)]
mod blocks_to_keep_tests {
    //! Tests for the `blocks_to_keep` retention wiring:
    //! - `maybe_prune_at_horizon` calls `prune_below_height` with the
    //!   correct horizon (raw or voting-epoch-aligned) after a flush_pair.
    //! - Negative `blocks_to_keep` skips pruning entirely.
    //! - Startup WARN fires once when on-disk bodies precede the
    //!   configured horizon, and stays silent otherwise.
    //!
    //! See `../facts/sync.md` § "Block Body Retention".
    use super::*;
    use crate::test_support::capture_warn;
    use enr_chain::{ChainError, SyncInfo};
    use ergo_chain_types::{ADDigest, Header};
    use ergo_validation::{
        ApplyStateOutcome, BlockValidator, Parameters, StatePersistence, ValidationError,
    };
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
    use std::sync::{Arc, Mutex};

    /// Mock store recording prune + min_height calls in addition to the
    /// flush-pair calls. Tests assert on the recorded sequence.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum StoreCall {
        SetValidatedHeight(u32),
        Flush,
        PruneBelowHeight { horizon: u32, type_ids: Vec<u8> },
    }

    struct MockStore {
        calls: Mutex<Vec<StoreCall>>,
        /// Value to return from `min_height_present(102)`.
        min_block_txs_height: Mutex<Option<u32>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                min_block_txs_height: Mutex::new(None),
            }
        }

        fn with_min_block_txs(min_height: u32) -> Self {
            let s = Self::new();
            *s.min_block_txs_height.lock().unwrap() = Some(min_height);
            s
        }

        fn calls(&self) -> Vec<StoreCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SyncStore for MockStore {
        async fn has_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> bool {
            unreachable!("not called when chain_height=0")
        }

        async fn get_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> Option<Vec<u8>> {
            unreachable!("not called in blocks_to_keep tests")
        }

        async fn validated_height(&self) -> Option<u32> {
            None
        }

        async fn set_validated_height(&self, height: u32) {
            self.calls
                .lock()
                .unwrap()
                .push(StoreCall::SetValidatedHeight(height));
        }

        async fn flush(&self) {
            self.calls.lock().unwrap().push(StoreCall::Flush);
        }

        async fn prune_below_height(&self, horizon: u32, type_ids: &[u8]) -> Result<usize, String> {
            self.calls
                .lock()
                .unwrap()
                .push(StoreCall::PruneBelowHeight {
                    horizon,
                    type_ids: type_ids.to_vec(),
                });
            Ok(0)
        }

        async fn min_height_present(&self, type_id: u8) -> Result<Option<u32>, String> {
            if type_id == BLOCK_TRANSACTIONS_TYPE_ID {
                Ok(*self.min_block_txs_height.lock().unwrap())
            } else {
                Ok(None)
            }
        }
    }

    struct FakeValidator {
        validated_height: u32,
        digest: ADDigest,
    }

    impl FakeValidator {
        fn at(height: u32) -> Self {
            Self {
                validated_height: height,
                digest: ADDigest::zero(),
            }
        }
    }

    impl BlockValidator for FakeValidator {
        fn apply_state(
            &mut self,
            _header: &Header,
            _block_txs: &[u8],
            _ad_proofs: Option<&[u8]>,
            _extension: &[u8],
            _preceding_headers: &[Header],
            _active_params: &Parameters,
            _expected_boundary_params: Option<&Parameters>,
            _expected_proposed_update: Option<&[u8]>,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            unreachable!("not called in blocks_to_keep tests")
        }

        fn validated_height(&self) -> u32 {
            self.validated_height
        }

        fn current_digest(&self) -> &ADDigest {
            &self.digest
        }

        fn reset_to(&mut self, _height: u32, _digest: ADDigest) -> Result<(), ValidationError> {
            unreachable!("not called in blocks_to_keep tests")
        }

        fn state_persistence(&self) -> Option<&dyn StatePersistence> {
            Some(self)
        }
    }

    impl StatePersistence for FakeValidator {
        fn flush(&self) -> Result<(), ValidationError> {
            Ok(())
        }

        fn resize_cache(&self, _cache_bytes: usize) -> Result<(), ValidationError> {
            unreachable!("not called in blocks_to_keep tests")
        }

        /// No prover behind this double, so nothing to size.
        fn prover_memory_estimate(&self) -> Option<ergo_validation::ProverMemoryEstimate> {
            None
        }
    }

    /// Digest-mode shape: no persistent state, so nothing to fsync — but the
    /// flush pair still records and prunes at the validator's height.
    struct NoPersistenceValidator(FakeValidator);

    impl BlockValidator for NoPersistenceValidator {
        fn apply_state(
            &mut self,
            _h: &Header,
            _bt: &[u8],
            _ap: Option<&[u8]>,
            _e: &[u8],
            _ph: &[Header],
            _p: &Parameters,
            _bp: Option<&Parameters>,
            _pu: Option<&[u8]>,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            unreachable!("not called in blocks_to_keep tests")
        }
        fn validated_height(&self) -> u32 {
            self.0.validated_height
        }
        fn current_digest(&self) -> &ADDigest {
            &self.0.digest
        }
        fn reset_to(&mut self, _h: u32, _d: ADDigest) -> Result<(), ValidationError> {
            unreachable!("not called in blocks_to_keep tests")
        }
        fn state_persistence(&self) -> Option<&dyn StatePersistence> {
            None
        }
    }

    struct HangingTransport;

    impl SyncTransport for HangingTransport {
        async fn send_to(
            &self,
            _peer: PeerId,
            _message: ProtocolMessage,
        ) -> Result<(), Box<dyn std::error::Error + Send>> {
            unreachable!()
        }

        async fn outbound_peers(&self) -> Vec<PeerId> {
            Vec::new()
        }

        async fn next_event(&mut self) -> Option<ProtocolEvent> {
            std::future::pending().await
        }
    }

    /// Chain at height 0 (so the startup tip-scan skips), with a
    /// configurable voting_length so tests can pick mainnet (1024) or
    /// small custom values without re-spinning a real `HeaderChain`.
    struct FixedVotingLengthChain {
        voting_length: u32,
    }

    impl SyncChain for FixedVotingLengthChain {
        async fn chain_height(&self) -> u32 {
            0
        }

        async fn build_sync_info(&self) -> Vec<u8> {
            unreachable!()
        }

        async fn header_at(&self, _height: u32) -> Option<Header> {
            unreachable!()
        }

        async fn header_state_root(&self, _height: u32) -> Option<[u8; 33]> {
            unreachable!()
        }

        fn parse_sync_info(&self, _body: &[u8]) -> Result<SyncInfo, ChainError> {
            unreachable!()
        }

        async fn continuation_ids(
            &self,
            _peer_last_ids: &[BlockId],
            _limit: usize,
        ) -> Vec<[u8; 32]> {
            // Chain at height 0 — nothing to serve a behind peer.
            Vec::new()
        }

        async fn active_parameters(&self) -> Parameters {
            unreachable!()
        }

        async fn is_epoch_boundary(&self, _height: u32) -> bool {
            unreachable!()
        }

        async fn compute_expected_parameters(
            &self,
            _epoch_boundary_height: u32,
            _block_proposed_update: &[u8],
        ) -> Result<Parameters, ChainError> {
            unreachable!()
        }

        async fn apply_epoch_boundary_parameters(
            &self,
            _params: Parameters,
            _proposed_update_bytes: Vec<u8>,
        ) {
            unreachable!()
        }

        async fn active_proposed_update_bytes(&self) -> Vec<u8> {
            unreachable!()
        }

        async fn verify_nipopow_envelope(
            &self,
            _envelope_body: &[u8],
        ) -> Result<Vec<Header>, ChainError> {
            unreachable!()
        }

        async fn is_better_nipopow(&self, _this: &[u8], _than: &[u8]) -> Result<bool, ChainError> {
            unreachable!()
        }

        async fn install_nipopow_suffix(
            &self,
            _suffix_head: Header,
            _suffix_tail: Vec<Header>,
        ) -> Result<(), ChainError> {
            unreachable!()
        }

        async fn voting_length(&self) -> u32 {
            self.voting_length
        }
    }

    fn build_sync(
        config: SyncConfig,
        validator: Option<FakeValidator>,
        store: MockStore,
        chain: FixedVotingLengthChain,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> HeaderSync<HangingTransport, FixedVotingLengthChain, MockStore, FakeValidator> {
        let (_progress_tx, progress_rx) = mpsc::channel(1);
        let (_dc_tx, delivery_control_rx) = mpsc::unbounded_channel::<DeliveryControl>();
        let (_dd_tx, delivery_data_rx) = mpsc::channel::<DeliveryData>(1);
        HeaderSync::new(
            config,
            HangingTransport,
            chain,
            store,
            validator,
            progress_rx,
            delivery_control_rx,
            delivery_data_rx,
            None,
            None,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU64::new(0)),
            shutdown_rx,
        )
    }

    fn config_with_keep(blocks_to_keep: i32) -> SyncConfig {
        SyncConfig {
            blocks_to_keep,
            ..SyncConfig::default()
        }
    }

    fn last_prune_call(calls: &[StoreCall]) -> Option<(u32, Vec<u8>)> {
        calls.iter().rev().find_map(|c| match c {
            StoreCall::PruneBelowHeight { horizon, type_ids } => Some((*horizon, type_ids.clone())),
            _ => None,
        })
    }

    #[tokio::test]
    async fn prune_called_with_raw_horizon_when_flushed_below_voting_length() {
        // blocks_to_keep=50, validator at 1000, voting_length=1024.
        // raw = 1000 - 50 + 1 = 951; 951 <= 1024 → no alignment.
        // The shutdown_flush path runs validator.flush() then triggers
        // prune via maybe_prune_at_horizon.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            config_with_keep(50),
            Some(FakeValidator::at(1000)),
            MockStore::new(),
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            shutdown_rx,
        );

        shutdown_tx.send(()).unwrap();
        sync.run().await;

        let calls = sync.store.calls();
        let (horizon, type_ids) =
            last_prune_call(&calls).expect("prune_below_height should be called once on shutdown");
        assert_eq!(horizon, 951, "raw horizon = flushed - keep + 1");
        assert_eq!(
            type_ids,
            vec![
                BLOCK_TRANSACTIONS_TYPE_ID,
                AD_PROOFS_TYPE_ID,
                EXTENSION_TYPE_ID,
            ],
            "prune MUST target only section bodies — never headers (101)"
        );
    }

    #[tokio::test]
    async fn prune_horizon_pulled_back_to_voting_epoch_start_mid_epoch() {
        // blocks_to_keep=50, validator at 2500, voting_length=1024.
        // raw = 2451 > 1024 → epoch_start = (2451/1024)*1024 = 2048.
        // raw.min(epoch_start) = 2048 → retains the current epoch.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            config_with_keep(50),
            Some(FakeValidator::at(2500)),
            MockStore::new(),
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            shutdown_rx,
        );

        shutdown_tx.send(()).unwrap();
        sync.run().await;

        let calls = sync.store.calls();
        let (horizon, _) = last_prune_call(&calls).expect("prune should be called");
        assert_eq!(
            horizon, 2048,
            "raw horizon 2451 pulled back to voting-epoch start 2048 so extensions stay intact"
        );
    }

    #[tokio::test]
    async fn no_prune_when_blocks_to_keep_negative() {
        // blocks_to_keep=-1 (archival) — pruning gate closed; the prune
        // call must not appear in the store's call log.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            config_with_keep(-1),
            Some(FakeValidator::at(1000)),
            MockStore::new(),
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            shutdown_rx,
        );

        shutdown_tx.send(()).unwrap();
        sync.run().await;

        assert!(
            last_prune_call(&sync.store.calls()).is_none(),
            "blocks_to_keep < 0 MUST NOT call prune_below_height"
        );
    }

    #[tokio::test]
    async fn no_prune_when_validator_flush_fails() {
        // Validator.flush() failure → FlushOutcome::Failed → no prune.
        // Pruning is gated on a successful flush so we never delete bodies
        // covering the gap between state.redb's persisted height and the
        // (failed) attempted flush height.
        struct FailingValidator(FakeValidator);

        impl BlockValidator for FailingValidator {
            fn apply_state(
                &mut self,
                _h: &Header,
                _bt: &[u8],
                _ap: Option<&[u8]>,
                _e: &[u8],
                _ph: &[Header],
                _p: &Parameters,
                _bp: Option<&Parameters>,
                _pu: Option<&[u8]>,
            ) -> Result<ApplyStateOutcome, ValidationError> {
                unreachable!()
            }
            fn validated_height(&self) -> u32 {
                self.0.validated_height
            }
            fn current_digest(&self) -> &ADDigest {
                &self.0.digest
            }
            fn reset_to(&mut self, _h: u32, _d: ADDigest) -> Result<(), ValidationError> {
                unreachable!()
            }
            fn state_persistence(&self) -> Option<&dyn StatePersistence> {
                Some(self)
            }
        }

        impl StatePersistence for FailingValidator {
            fn flush(&self) -> Result<(), ValidationError> {
                Err(ValidationError::ProofVerificationFailed("nope".into()))
            }
            fn resize_cache(&self, _cache_bytes: usize) -> Result<(), ValidationError> {
                unreachable!()
            }

            /// No prover behind this double, so nothing to size.
            fn prover_memory_estimate(&self) -> Option<ergo_validation::ProverMemoryEstimate> {
                None
            }
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (_progress_tx, progress_rx) = mpsc::channel(1);
        let (_dc_tx, delivery_control_rx) = mpsc::unbounded_channel::<DeliveryControl>();
        let (_dd_tx, delivery_data_rx) = mpsc::channel::<DeliveryData>(1);
        let mut sync = HeaderSync::<
            HangingTransport,
            FixedVotingLengthChain,
            MockStore,
            FailingValidator,
        >::new(
            config_with_keep(50),
            HangingTransport,
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            MockStore::new(),
            Some(FailingValidator(FakeValidator::at(1000))),
            progress_rx,
            delivery_control_rx,
            delivery_data_rx,
            None,
            None,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU64::new(0)),
            shutdown_rx,
        );

        shutdown_tx.send(()).unwrap();
        sync.run().await;

        assert!(
            last_prune_call(&sync.store.calls()).is_none(),
            "failed validator.flush() MUST NOT trigger prune"
        );
    }

    #[tokio::test]
    async fn prune_still_runs_when_validator_owns_no_persistent_state() {
        // NothingToPersist still triggers pruning. The prune call sites
        // use `if let`, which the compiler does not exhaustiveness-check —
        // this test is the guard the compiler cannot be.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (_progress_tx, progress_rx) = mpsc::channel(1);
        let (_dc_tx, delivery_control_rx) = mpsc::unbounded_channel::<DeliveryControl>();
        let (_dd_tx, delivery_data_rx) = mpsc::channel::<DeliveryData>(1);
        let mut sync = HeaderSync::<
            HangingTransport,
            FixedVotingLengthChain,
            MockStore,
            NoPersistenceValidator,
        >::new(
            config_with_keep(50),
            HangingTransport,
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            MockStore::new(),
            Some(NoPersistenceValidator(FakeValidator::at(1000))),
            progress_rx,
            delivery_control_rx,
            delivery_data_rx,
            None,
            None,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU64::new(0)),
            shutdown_rx,
        );

        shutdown_tx.send(()).unwrap();
        sync.run().await;

        let calls = sync.store.calls();
        let (horizon, _type_ids) = last_prune_call(&calls)
            .expect("nothing-to-persist MUST still prune — same horizon as a real flush");
        assert_eq!(horizon, 951, "raw horizon = validated_height - keep + 1");
    }

    // ----- startup WARN capture -----

    #[tokio::test]
    async fn startup_warn_fires_when_archive_predates_horizon() {
        // blocks_to_keep=100, validator at 1000, voting_length=1024.
        // configured_horizon = 1000 - 100 + 1 = 901 (raw, no alignment
        // since 901 <= 1024).
        // Pre-populate min_height_present(102) = 1, so the archive
        // extends 900 blocks below the horizon.
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            config_with_keep(100),
            Some(FakeValidator::at(1000)),
            MockStore::with_min_block_txs(1),
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            shutdown_rx,
        );

        let output = capture_warn(sync.maybe_warn_reclaimable_bodies()).await;

        assert!(
            output.contains("900 historical blocks reclaimable"),
            "expected reclaimable count = 901 - 1 = 900 in output: {output}"
        );
        assert!(
            output.contains("sharpen prune --keep=100"),
            "WARN must include sharpen prune invocation: {output}"
        );
    }

    #[tokio::test]
    async fn startup_warn_silent_when_archive_already_pruned() {
        // Archive's min is at the horizon already — no reclaimable bodies.
        // configured_horizon = 1000 - 100 + 1 = 901; min = 901 → silent.
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            config_with_keep(100),
            Some(FakeValidator::at(1000)),
            MockStore::with_min_block_txs(901),
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            shutdown_rx,
        );

        let output = capture_warn(sync.maybe_warn_reclaimable_bodies()).await;

        assert!(
            !output.contains("reclaimable"),
            "no WARN expected when archive min equals configured_horizon: {output}"
        );
    }

    #[tokio::test]
    async fn startup_warn_skipped_when_blocks_to_keep_negative() {
        // Archival mode (blocks_to_keep = -1) → no advisory; archive is
        // the intended state.
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            config_with_keep(-1),
            Some(FakeValidator::at(1000)),
            MockStore::with_min_block_txs(1),
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            shutdown_rx,
        );

        let output = capture_warn(sync.maybe_warn_reclaimable_bodies()).await;

        assert!(
            !output.contains("reclaimable"),
            "blocks_to_keep < 0 MUST suppress the startup WARN: {output}"
        );
    }

    #[tokio::test]
    async fn startup_warn_skipped_in_light_mode() {
        // No validator (light mode) → no validator height to anchor
        // against, and no bodies on disk to begin with. The advisory is a
        // no-op.
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut sync = build_sync(
            config_with_keep(100),
            None,
            MockStore::with_min_block_txs(1),
            FixedVotingLengthChain {
                voting_length: 1024,
            },
            shutdown_rx,
        );

        let output = capture_warn(sync.maybe_warn_reclaimable_bodies()).await;

        assert!(
            !output.contains("reclaimable"),
            "light mode MUST suppress the startup WARN: {output}"
        );
    }
}

#[cfg(test)]
mod sweep_resume_tests {
    //! Ensure the sweep resumes from `validated_height()`, never the
    //! stale `state_applied_height` cache.
    use super::*;
    use crate::test_support::capture_async;
    use enr_chain::{ChainError, SyncInfo};
    use ergo_chain_types::{ADDigest, Header};
    use ergo_validation::{
        ApplyStateOutcome, BlockValidator, Parameters, StatePersistence, ValidationError,
    };
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
    use std::sync::Arc;
    use tracing::level_filters::LevelFilter;

    fn fake_header(height: u32) -> Header {
        use ergo_chain_types::*;
        Header {
            version: 2,
            id: BlockId(Digest32::from([height as u8; 32])),
            parent_id: BlockId(Digest32::zero()),
            ad_proofs_root: Digest32::zero(),
            state_root: ADDigest::zero(),
            transaction_root: Digest32::zero(),
            timestamp: 1_000_000 + height as u64,
            n_bits: 100_000,
            height,
            extension_root: Digest32::zero(),
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: vec![0; 8],
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        }
    }

    /// Validator that enforces the real consecutiveness guard: it accepts
    /// only `validated_height + 1` and advances on success, mirroring
    /// `UtxoValidator::apply_state_internal`'s `HeightMismatch`. `reset_to`
    /// lowers the height as a successful storage rollback would — unless
    /// `failing_reset()` arms the failure mode, in which case it returns Err
    /// and leaves the height untouched, exactly like the real validator's
    /// unchanged-on-Err contract. Records the exact sequence of heights it
    /// applied and the rollback targets it was asked for, so a test can
    /// prove the sweep resumed at the right block and the rollback was
    /// actually attempted.
    struct SweepValidator {
        validated_height: u32,
        digest: ADDigest,
        applied: Vec<u32>,
        /// Rollback targets passed to `reset_to`, in call order — recorded
        /// even when the reset is made to fail.
        resets: Vec<u32>,
        /// When `Some(h)`, `apply_state` rejects height `h` every time with a
        /// `StateRootMismatch`, modelling a deterministic divergence that
        /// never advances the tip past `h - 1`. The backoff keys on the tip
        /// stall, not the error kind, so this stands in equally for an
        /// `apply_state` error and a deferred-eval rollback.
        fail_at: Option<u32>,
        /// When true, `reset_to` returns Err and does NOT move
        /// `validated_height`, modelling a failed storage rollback.
        reset_fails: bool,
        /// Every `apply_state` call, success or failure — lets a test prove
        /// the gate actually suppressed a retry.
        attempts: u32,
    }

    impl SweepValidator {
        fn at(height: u32) -> Self {
            Self {
                validated_height: height,
                digest: ADDigest::zero(),
                applied: Vec::new(),
                resets: Vec::new(),
                fail_at: None,
                reset_fails: false,
                attempts: 0,
            }
        }

        fn failing_at(mut self, height: u32) -> Self {
            self.fail_at = Some(height);
            self
        }

        fn failing_reset(mut self) -> Self {
            self.reset_fails = true;
            self
        }
    }

    impl BlockValidator for SweepValidator {
        fn apply_state(
            &mut self,
            header: &Header,
            _block_txs: &[u8],
            _ad_proofs: Option<&[u8]>,
            _extension: &[u8],
            _preceding_headers: &[Header],
            _active_params: &Parameters,
            _expected_boundary_params: Option<&Parameters>,
            _expected_proposed_update: Option<&[u8]>,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            self.attempts += 1;
            if self.fail_at == Some(header.height) {
                return Err(ValidationError::StateRootMismatch {
                    expected: Vec::new(),
                    got: Vec::new(),
                });
            }
            let expected = self.validated_height + 1;
            if header.height != expected {
                return Err(ValidationError::HeightMismatch {
                    expected,
                    got: header.height,
                });
            }
            self.validated_height = header.height;
            self.applied.push(header.height);
            Ok(ApplyStateOutcome {
                epoch_boundary_params: None,
                epoch_boundary_proposed_update: None,
            })
        }

        fn validated_height(&self) -> u32 {
            self.validated_height
        }

        fn current_digest(&self) -> &ADDigest {
            &self.digest
        }

        fn reset_to(&mut self, height: u32, _digest: ADDigest) -> Result<(), ValidationError> {
            self.resets.push(height);
            if self.reset_fails {
                return Err(ValidationError::StateOperationFailed(format!(
                    "rollback to height {height} failed: simulated storage failure"
                )));
            }
            self.validated_height = height;
            Ok(())
        }

        /// These tests assert sweep/resume behaviour, not durability; there is
        /// no storage to flush and none of them look at the flush pair.
        fn state_persistence(&self) -> Option<&dyn StatePersistence> {
            None
        }
    }

    /// Chain that hands out a header for every height up to `tip` and is
    /// never at an epoch boundary, so the sweep takes its plain (non-voting)
    /// path. The sweep tests want an effectively unbounded chain (the sweep
    /// range is capped by `downloaded_height`); the reorg tests need a real
    /// tip so the post-reorg rescan terminates.
    struct SweepChain {
        tip: u32,
    }

    impl SweepChain {
        fn unbounded() -> Self {
            Self { tip: 1_000_000 }
        }

        fn at_tip(tip: u32) -> Self {
            Self { tip }
        }
    }

    impl SyncChain for SweepChain {
        async fn chain_height(&self) -> u32 {
            self.tip
        }
        async fn build_sync_info(&self) -> Vec<u8> {
            unreachable!()
        }
        async fn header_at(&self, height: u32) -> Option<Header> {
            (height >= 1 && height <= self.tip).then(|| fake_header(height))
        }
        async fn header_state_root(&self, _height: u32) -> Option<[u8; 33]> {
            Some([0u8; 33])
        }
        fn parse_sync_info(&self, _body: &[u8]) -> Result<SyncInfo, ChainError> {
            unreachable!()
        }
        async fn continuation_ids(
            &self,
            _peer_last_ids: &[BlockId],
            _limit: usize,
        ) -> Vec<[u8; 32]> {
            unreachable!("no SyncInfo events routed in sweep tests")
        }
        async fn active_parameters(&self) -> Parameters {
            Parameters::default()
        }
        async fn is_epoch_boundary(&self, _height: u32) -> bool {
            false
        }
        async fn compute_expected_parameters(
            &self,
            _epoch_boundary_height: u32,
            _block_proposed_update: &[u8],
        ) -> Result<Parameters, ChainError> {
            unreachable!()
        }
        async fn apply_epoch_boundary_parameters(
            &self,
            _params: Parameters,
            _proposed_update_bytes: Vec<u8>,
        ) {
            unreachable!()
        }
        async fn active_proposed_update_bytes(&self) -> Vec<u8> {
            unreachable!()
        }
        async fn verify_nipopow_envelope(
            &self,
            _envelope_body: &[u8],
        ) -> Result<Vec<Header>, ChainError> {
            unreachable!()
        }
        async fn is_better_nipopow(&self, _this: &[u8], _than: &[u8]) -> Result<bool, ChainError> {
            unreachable!()
        }
        async fn install_nipopow_suffix(
            &self,
            _suffix_head: Header,
            _suffix_tail: Vec<Header>,
        ) -> Result<(), ChainError> {
            unreachable!()
        }
        async fn voting_length(&self) -> u32 {
            1024
        }
    }

    /// Store where every requested section is "on disk".
    struct SweepStore;

    impl SyncStore for SweepStore {
        async fn has_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> bool {
            true
        }
        async fn get_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> Option<Vec<u8>> {
            Some(vec![0])
        }
        async fn validated_height(&self) -> Option<u32> {
            None
        }
        async fn set_validated_height(&self, _height: u32) {}
        async fn flush(&self) {}
        async fn prune_below_height(
            &self,
            _horizon: u32,
            _type_ids: &[u8],
        ) -> Result<usize, String> {
            Ok(0)
        }
        async fn min_height_present(&self, _type_id: u8) -> Result<Option<u32>, String> {
            Ok(None)
        }
    }

    struct SweepTransport;

    impl SyncTransport for SweepTransport {
        async fn send_to(
            &self,
            _peer: PeerId,
            _message: ProtocolMessage,
        ) -> Result<(), Box<dyn std::error::Error + Send>> {
            unreachable!()
        }
        async fn outbound_peers(&self) -> Vec<PeerId> {
            Vec::new()
        }
        async fn next_event(&mut self) -> Option<ProtocolEvent> {
            std::future::pending().await
        }
    }

    type SweepSync = HeaderSync<SweepTransport, SweepChain, SweepStore, SweepValidator>;

    fn build_sync(validator: SweepValidator) -> SweepSync {
        build_sync_with_chain(validator, SweepChain::unbounded())
    }

    fn build_sync_with_chain(validator: SweepValidator, chain: SweepChain) -> SweepSync {
        build_sync_full(
            validator,
            chain,
            SyncConfig::default(),
            Arc::new(AtomicU64::new(0)),
        )
    }

    /// Same harness, but hands back the published window atomic so a test can
    /// observe what the sweep wrote into it.
    fn build_sync_with_window_sink(validator: SweepValidator) -> (SweepSync, Arc<AtomicU64>) {
        let sink = Arc::new(AtomicU64::new(0));
        let sync = build_sync_full(
            validator,
            SweepChain::unbounded(),
            SyncConfig::default(),
            Arc::clone(&sink),
        );
        (sync, sink)
    }

    fn build_sync_full(
        validator: SweepValidator,
        chain: SweepChain,
        config: SyncConfig,
        window_sink: Arc<AtomicU64>,
    ) -> SweepSync {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (_progress_tx, progress_rx) = mpsc::channel(1);
        let (_dc_tx, delivery_control_rx) = mpsc::unbounded_channel::<DeliveryControl>();
        let (_dd_tx, delivery_data_rx) = mpsc::channel::<DeliveryData>(1);
        HeaderSync::new(
            config,
            SweepTransport,
            chain,
            SweepStore,
            Some(validator),
            progress_rx,
            delivery_control_rx,
            delivery_data_rx,
            None,
            None,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0)),
            window_sink,
            shutdown_rx,
        )
    }

    #[tokio::test]
    async fn sweep_resumes_at_applied_tip_when_cache_ran_ahead() {
        // Applied tip 2665, cache desynced two ahead at 2667, blocks
        // 2666..=2670 on disk. Must resume at validated_height() + 1 = 2666.
        let mut sync = build_sync(SweepValidator::at(2665));
        sync.state_applied_height = 2667; // cache ran ahead of the prover
        sync.downloaded_height = 2670; // 2666..=2670 are downloaded

        sync.advance_state_applied_height().await;

        let v = sync.validator.as_ref().unwrap();
        assert_eq!(
            v.applied,
            vec![2666, 2667, 2668, 2669, 2670],
            "sweep MUST resume at applied_tip+1 (2666) and apply consecutively, \
             not skip to the stale cache height"
        );
        assert_eq!(
            v.validated_height(),
            2670,
            "validator advanced to the downloaded tip"
        );
        assert_eq!(
            sync.state_applied_height, 2670,
            "cache reconciled to the validator's true tip — no lingering desync"
        );
    }

    #[tokio::test]
    async fn stale_ahead_cache_is_pulled_back_when_nothing_to_apply() {
        // Cache ahead of the prover but nothing new on disk (downloaded ==
        // applied tip). The sweep has no work, but it must still pull the
        // stale cache back down to the truth so the desync cannot persist.
        let mut sync = build_sync(SweepValidator::at(2665));
        sync.state_applied_height = 2667;
        sync.downloaded_height = 2665;

        sync.advance_state_applied_height().await;

        let v = sync.validator.as_ref().unwrap();
        assert!(
            v.applied.is_empty(),
            "no blocks past the applied tip — nothing to apply"
        );
        assert_eq!(
            sync.state_applied_height, 2665,
            "stale-ahead cache reconciled down to the validator tip"
        );
    }

    #[tokio::test]
    async fn rollback_via_reset_keeps_cache_in_sync_next_sweep() {
        // After a reorg/eval rollback lowers the validator below the cache,
        // the very next sweep must re-anchor on the prover and re-apply the
        // demoted blocks from the fork point — never feed a block past the
        // rolled-back tip.
        let mut sync = build_sync(SweepValidator::at(2670));
        sync.state_applied_height = 2670;
        sync.downloaded_height = 2670;
        // Simulate a rollback to 2667 (validator only — cache left stale,
        // exactly the hazardous state the fix must tolerate).
        sync.validator
            .as_mut()
            .unwrap()
            .reset_to(2667, ADDigest::zero())
            .expect("simulated rollback succeeds");

        sync.advance_state_applied_height().await;

        let v = sync.validator.as_ref().unwrap();
        assert_eq!(
            v.applied,
            vec![2668, 2669, 2670],
            "sweep re-applies from the rolled-back tip + 1, not from the stale cache"
        );
        assert_eq!(
            sync.state_applied_height, 2670,
            "cache back in sync with the prover"
        );
    }

    #[tokio::test]
    async fn reorg_rollback_ok_resets_then_revalidates_new_branch() {
        // Successful reorg rollback: validator rolls to the fork point, the
        // state watermark follows, and the handler's immediate rescan
        // re-applies the new branch's on-disk blocks from fork_point + 1.
        let mut sync = build_sync_with_chain(SweepValidator::at(2670), SweepChain::at_tip(2670));
        sync.state_applied_height = 2670;
        sync.downloaded_height = 2670;

        sync.handle_control_event(
            DeliveryControl::Reorg {
                fork_point: 2667,
                old_tip: 2670,
                new_tip: 2670,
            },
            PeerId(0),
        )
        .await;

        let v = sync.validator.as_ref().unwrap();
        assert_eq!(
            v.resets,
            vec![2667],
            "validator rolled back to the fork point"
        );
        assert_eq!(
            v.applied,
            vec![2668, 2669, 2670],
            "rescan re-applied the new branch from fork_point + 1"
        );
        assert_eq!(
            sync.state_applied_height, 2670,
            "sweep re-advanced after the rollback"
        );
    }

    #[tokio::test]
    async fn reorg_rollback_err_keeps_state_watermarks() {
        // Same reorg, but the storage rollback fails: the validator stays
        // at 2670, so state_applied_height MUST NOT retreat to the fork
        // point — and nothing may be re-applied onto the un-rolled state.
        // (downloaded_height legitimately resets and rescans: it tracks the
        // store against the already-switched header chain, not the prover —
        // facts/validation.md reorg steps 2 vs 5.)
        let mut sync = build_sync_with_chain(
            SweepValidator::at(2670).failing_reset(),
            SweepChain::at_tip(2670),
        );
        sync.state_applied_height = 2670;
        sync.downloaded_height = 2670;

        sync.handle_control_event(
            DeliveryControl::Reorg {
                fork_point: 2667,
                old_tip: 2670,
                new_tip: 2670,
            },
            PeerId(0),
        )
        .await;

        let v = sync.validator.as_ref().unwrap();
        assert_eq!(v.resets, vec![2667], "rollback was attempted");
        assert_eq!(v.validated_height(), 2670, "validator unmoved on Err");
        assert!(
            v.applied.is_empty(),
            "nothing re-applied onto un-rolled state"
        );
        assert_eq!(
            sync.state_applied_height, 2670,
            "state watermark NOT retreated"
        );
    }

    #[tokio::test]
    async fn stalled_sweep_arms_backoff_and_gate_suppresses_retry() {
        // Frontier 2665 is wedged: block 2666 fails apply_state every
        // attempt (a deterministic divergence). Blocks 2666..=2670 are all
        // on disk, so without the gate the sweep would re-run at full tilt.
        let mut sync = build_sync(SweepValidator::at(2665).failing_at(2666));
        sync.downloaded_height = 2670;

        // First sweep: one attempt at 2666, it stalls, the tip stays pinned,
        // and the backoff arms at attempt 1.
        sync.advance_state_applied_height().await;
        assert_eq!(
            sync.validator.as_ref().unwrap().attempts,
            1,
            "sweep made one attempt"
        );
        assert_eq!(
            sync.validator.as_ref().unwrap().validated_height(),
            2665,
            "deterministic failure left the tip pinned"
        );
        assert_eq!(
            sync.sweep_backoff.consecutive(),
            1,
            "non-advancing sweep armed the backoff"
        );

        // Immediate re-entry, well inside the backoff window: the gate must
        // short-circuit the sweep — no second apply_state attempt, no
        // escalation. This is the core of the fix: the deterministic failure
        // can no longer peg a core.
        sync.advance_state_applied_height().await;
        assert_eq!(
            sync.validator.as_ref().unwrap().attempts,
            1,
            "gate suppressed the retry inside the backoff window"
        );
        assert_eq!(
            sync.sweep_backoff.consecutive(),
            1,
            "still attempt 1 — no fresh stall recorded"
        );
    }

    #[tokio::test]
    async fn advancing_sweep_never_arms_backoff() {
        // A clean sweep that advances the tip must leave the backoff idle —
        // the end-of-sweep bookkeeping records progress, not a stall. Guards
        // against a false-positive that would throttle a healthy node.
        let mut sync = build_sync(SweepValidator::at(2665));
        sync.downloaded_height = 2670;

        sync.advance_state_applied_height().await;

        assert_eq!(
            sync.validator.as_ref().unwrap().validated_height(),
            2670,
            "healthy sweep caught up to the downloaded tip"
        );
        assert_eq!(
            sync.sweep_backoff.consecutive(),
            0,
            "progress arms no backoff"
        );
    }

    // ---- journal-event conformance (`../facts/journal-events.md`) ----
    //
    // Every assertion below runs the REAL emit site through this module's
    // harness and reads the rendered line. That distinction is the whole point
    // of the section: the conformance tests these replace called `info!()` with
    // their own copy of the marker and then asserted the output contained that
    // copy, which tests `tracing`'s formatter against itself and cannot fail
    // when an emit site drifts. Two of the events below shipped not matching
    // the contract under a green suite for exactly that reason.
    //
    // Break a marker or drop a field in `src/state.rs` and these go red.

    /// Assert `output` renders `key=value` as a **whole field**.
    ///
    /// `contains("height=1785500")` is not sufficient and the difference is not
    /// pedantic: it also matches `tip_height=1785500` — a renamed field, which
    /// breaks a consumer exactly as hard as a dropped one — and it matches
    /// `height=17855001`. Fields in the default formatter follow the message
    /// and are space-separated, so a real field is a whole whitespace token,
    /// which is also how a consumer's parser has to find it. Values containing
    /// spaces (a Display-formatted error) need a substring assertion instead.
    fn has_field(output: &str, key: &str, value: &str) -> bool {
        let want = format!("{key}={value}");
        output.split_whitespace().any(|token| token == want)
    }

    #[track_caller]
    fn assert_field(output: &str, key: &str, value: &str) {
        assert!(
            has_field(output, key, value),
            "missing field {key}={value}: {output}"
        );
    }

    #[test]
    fn has_field_rejects_a_renamed_or_truncated_field() {
        // The self-check for the assertion helper: without it these three all
        // look like a present `height=1785500`.
        assert!(has_field(
            " INFO chain tip reached height=1785500",
            "height",
            "1785500"
        ));
        assert!(
            !has_field(
                " INFO chain tip reached tip_height=1785500",
                "height",
                "1785500"
            ),
            "a renamed field must not satisfy the assertion"
        );
        assert!(
            !has_field(
                " INFO chain tip reached height=17855001",
                "height",
                "1785500"
            ),
            "a different value must not satisfy the assertion"
        );
    }

    /// `validation_rollback_failed` — ERROR, `path="reorg"`.
    ///
    /// The reorg path is this event's sole emitter.
    #[tokio::test]
    async fn journal_validation_rollback_failed_conforms() {
        let mut sync = build_sync_with_chain(
            SweepValidator::at(2670).failing_reset(),
            SweepChain::at_tip(2670),
        );
        sync.state_applied_height = 2670;
        sync.downloaded_height = 2670;

        let output = capture_async(
            LevelFilter::ERROR,
            sync.handle_control_event(
                DeliveryControl::Reorg {
                    fork_point: 2668,
                    old_tip: 2670,
                    new_tip: 2671,
                },
                PeerId(0),
            ),
        )
        .await;

        assert!(
            output.contains("validation rollback failed"),
            "missing marker: {output}"
        );
        // The rollback TARGET (the fork point), not the tip it failed to leave.
        assert_field(&output, "height", "2668");
        assert_field(&output, "path", "\"reorg\"");
        // `ValidationError`'s Display, not the bare message the validator
        // constructed. The contract says "Display-formatted".
        assert!(
            output.contains("error=UTXO state operation failed: rollback to height 2668 failed"),
            "missing the Display-formatted underlying error: {output}"
        );
    }

    /// `validation_sweep_started` / `block_applied` / `validation_sweep_complete`
    /// in one pass — they are emitted by one sweep and a test that drives the
    /// sweep gets all three.
    #[tokio::test]
    async fn journal_sweep_and_block_applied_conform() {
        // The sweep markers are gated on `sweep_size > 100`, so the window has
        // to be a real catch-up sweep rather than a tip follow.
        let mut sync = build_sync(SweepValidator::at(1000));
        sync.downloaded_height = 1150;

        let output = capture_async(LevelFilter::INFO, sync.advance_state_applied_height()).await;

        assert!(
            output.contains("VALIDATION SWEEP STARTED"),
            "missing started marker: {output}"
        );
        assert!(
            output.contains("VALIDATION SWEEP COMPLETE"),
            "missing complete marker: {output}"
        );
        // The marker is the literal prefix, not a decorated banner.
        assert!(
            !output.contains("==="),
            "markers are plain text, not decorated: {output}"
        );
        assert_field(&output, "from", "1001");
        assert_field(&output, "to", "1150");
        assert_field(&output, "blocks", "150");

        // `block applied` once per block that advanced the tip, carrying the
        // height and the 32-byte hex id. `fake_header` derives the id from the
        // height, so this pins the Display rendering of a real `BlockId`.
        assert_eq!(
            output.matches("block applied").count(),
            150,
            "one per applied block: {output}"
        );
        let applied = output
            .lines()
            .find(|l| l.contains("block applied"))
            .expect("counted above");
        assert_field(applied, "height", "1001");
        // fake_header(1001): BlockId(Digest32::from([1001 as u8; 32])) → 0xe9.
        assert_field(applied, "id", &"e9".repeat(32));
    }

    /// `chain_tip_reached` — INFO, emitted on entry to `synced()`.
    ///
    /// `synced()` is a `select!` loop, but it is driveable here: the first
    /// `ticker.tick()` completes immediately, finds `outbound_peers()` empty,
    /// and returns. The emit is above the loop, so it has already fired.
    #[tokio::test]
    async fn journal_chain_tip_reached_conforms() {
        let mut sync =
            build_sync_with_chain(SweepValidator::at(1_785_500), SweepChain::at_tip(1_785_500));
        sync.downloaded_height = 1_785_500;

        let output = capture_async(LevelFilter::INFO, sync.synced()).await;

        assert!(
            output.contains("chain tip reached"),
            "missing marker: {output}"
        );
        assert_field(&output, "height", "1785500");
    }

    // ---- Window memory attribution (`facts/sync.md` § "Memory attribution")
    //
    // These live here rather than in a module of their own because the thing
    // under test fires after an applied block, and this is the harness that
    // applies blocks.

    #[tokio::test]
    async fn window_estimate_tracks_the_delivery_tracker_and_falls_back_down() {
        // The symptom under investigation is heap that only ever rises, so the
        // figure has to be shown to come back down. It counts the tracker's
        // live entries, so it rises as requests go in flight and falls as they
        // are delivered.
        let (mut sync, _sink) = build_sync_with_window_sink(SweepValidator::at(100));

        let idle = sync.window_memory_estimate().expect("always computable");
        assert_eq!(idle.tracker_bytes, 0);
        assert_eq!(idle.tracker_entries, 0);

        let ids: Vec<[u8; 32]> = (0u8..150).map(|n| [n; 32]).collect();
        sync.tracker.mark_requested(&ids, PeerId(1), 102);

        let loaded = sync.window_memory_estimate().unwrap();
        assert_eq!(loaded.tracker_entries, 150, "150 sections in flight");
        assert!(
            loaded.tracker_bytes > 0,
            "in-flight requests must report bytes"
        );

        // Fewer in flight ⇒ proportionally fewer bytes. Proves the figure is
        // derived from the structure and not from a constant.
        let (mut small, _) = build_sync_with_window_sink(SweepValidator::at(100));
        small.tracker.mark_requested(&ids[..15], PeerId(1), 102);
        let small_est = small.window_memory_estimate().unwrap();
        assert_eq!(
            small_est.tracker_bytes * 10,
            loaded.tracker_bytes,
            "15 in flight ({}) must be exactly a tenth of 150 in flight ({})",
            small_est.tracker_bytes,
            loaded.tracker_bytes
        );

        // Delivery drains the tracker and both fields come back down. A figure
        // that only ever rises is measuring the wrong thing — that is the
        // symptom under investigation.
        for id in &ids {
            sync.tracker.mark_received(id);
        }
        let drained = sync.window_memory_estimate().unwrap();
        assert_eq!(drained.tracker_entries, 0, "the window emptied");
        assert_eq!(
            drained.tracker_bytes, 0,
            "and the bytes it was holding came back"
        );
    }

    #[tokio::test]
    async fn window_atomic_reads_never_written_until_a_block_is_applied() {
        let (mut sync, sink) = build_sync_with_window_sink(SweepValidator::at(2665));
        assert_eq!(
            sink.load(std::sync::atomic::Ordering::Relaxed),
            WINDOW_BYTES_UNSET,
            "constructed but nothing applied — the reader must see absent, \
             not an assertion that the window is empty"
        );

        // A sweep that applies nothing must not stamp the atomic either.
        sync.downloaded_height = 2665;
        sync.advance_state_applied_height().await;
        assert_eq!(
            sink.load(std::sync::atomic::Ordering::Relaxed),
            WINDOW_BYTES_UNSET,
            "no block applied ⇒ still never-written"
        );
    }

    #[tokio::test]
    async fn window_atomic_is_written_after_an_applied_block() {
        let (mut sync, sink) = build_sync_with_window_sink(SweepValidator::at(2665));
        sync.downloaded_height = 2670;

        // Put sections in flight so the published figure is non-zero and a
        // written zero cannot be mistaken for the sentinel or the reverse.
        let ids: Vec<[u8; 32]> = (0u8..64).map(|n| [n; 32]).collect();
        sync.tracker.mark_requested(&ids, PeerId(1), 102);
        let expected = sync.window_memory_estimate().unwrap().tracker_bytes;
        assert!(expected > 0);

        sync.advance_state_applied_height().await;

        assert_eq!(
            sync.validator.as_ref().unwrap().applied,
            vec![2666, 2667, 2668, 2669, 2670],
            "harness sanity: blocks really were applied"
        );
        assert_eq!(
            sink.load(std::sync::atomic::Ordering::Relaxed),
            expected,
            "the estimate must be published after an applied block"
        );
    }
}

#[cfg(test)]
mod serve_continuation_tests {
    //! Tests for the serve side of the sync exchange (`facts/sync.md`
    //! § "Serving sync (peer behind us)").
    //!
    //! An incoming SyncInfo from a behind or forked peer must be answered
    //! with `Inv { HEADER_TYPE_ID, continuation ids }` (JVM
    //! processSyncV1/V2: Younger | Fork → continuationIds(size=400) →
    //! sendExtension); peers ahead or equal must NOT be served.
    //!
    //! History (2026-06-08): only the consume side existed — a
    //! from-genesis rust peer syncing FROM us stalled at height 0
    //! forever ("sync stalled, rotating peer height=0" on loop).
    use super::*;
    use enr_chain::{ChainError, SyncInfo};
    use ergo_chain_types::{ADDigest, Header};
    use ergo_validation::{
        ApplyStateOutcome, BlockValidator, Parameters, StatePersistence, ValidationError,
    };
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// Test chain tip. Above the 400-id continuation cap so the fresh-peer
    /// case exercises the cap.
    const TIP: u32 = 500;

    /// Height-faithful fake id: height LE in the first 4 bytes. Unlike
    /// `[height as u8; 32]`, unambiguous above 255.
    fn id_at(height: u32) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[..4].copy_from_slice(&height.to_le_bytes());
        id
    }

    fn block_id_at(height: u32) -> BlockId {
        BlockId(ergo_chain_types::Digest32::from(id_at(height)))
    }

    /// Off-chain id at `height` — a fork branch's header. Marker byte
    /// keeps it distinct from every `id_at`.
    fn fork_block_id_at(height: u32) -> BlockId {
        let mut id = id_at(height);
        id[8] = 0xFF;
        BlockId(ergo_chain_types::Digest32::from(id))
    }

    fn test_header(height: u32, id: BlockId) -> Header {
        use ergo_chain_types::*;
        Header {
            version: 2,
            id,
            parent_id: BlockId(Digest32::zero()),
            ad_proofs_root: Digest32::zero(),
            state_root: ADDigest::zero(),
            transaction_root: Digest32::zero(),
            timestamp: 1_000_000 + height as u64,
            n_bits: 100_000,
            height,
            extension_root: Digest32::zero(),
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: vec![0; 8],
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        }
    }

    /// Height of a peer anchor if it sits on our chain (test ids are
    /// height-faithful), None for fork/alien ids.
    fn on_chain_height(id: &BlockId) -> Option<u32> {
        (1..=TIP).find(|&h| *id == block_id_at(h))
    }

    /// Chain at `TIP` whose `continuation_ids` mirrors the bridge
    /// contract: ids ascending from the newest on-chain peer anchor + 1,
    /// capped at `limit`; empty anchors serve from height 1; no common
    /// point serves nothing. Counts invocations so the ahead test can
    /// prove the handler gated the call entirely.
    struct ServeChain {
        continuation_calls: AtomicU32,
    }

    impl ServeChain {
        fn new() -> Self {
            Self {
                continuation_calls: AtomicU32::new(0),
            }
        }
    }

    impl SyncChain for ServeChain {
        async fn chain_height(&self) -> u32 {
            TIP
        }

        async fn build_sync_info(&self) -> Vec<u8> {
            // Only reached by the handler's SyncInfo response, which is
            // rate-limited away in tests (last_sync_sent = construction
            // time). Returns an empty body for the diagnostics parse.
            Vec::new()
        }

        async fn header_at(&self, height: u32) -> Option<Header> {
            (1..=TIP)
                .contains(&height)
                .then(|| test_header(height, block_id_at(height)))
        }

        async fn header_state_root(&self, _height: u32) -> Option<[u8; 33]> {
            unreachable!("not called in serve tests")
        }

        /// Test encoding: 4-byte LE anchor specs, newest first (V2 wire
        /// order). High bit set = fork id at that height, clear = our
        /// chain's id. Empty body = fresh peer.
        fn parse_sync_info(&self, body: &[u8]) -> Result<SyncInfo, ChainError> {
            let headers = body
                .chunks_exact(4)
                .map(|c| {
                    let spec = u32::from_le_bytes(c.try_into().unwrap());
                    let height = spec & 0x7fff_ffff;
                    let id = if spec & 0x8000_0000 != 0 {
                        fork_block_id_at(height)
                    } else {
                        block_id_at(height)
                    };
                    test_header(height, id)
                })
                .collect();
            Ok(SyncInfo::V2 { headers })
        }

        async fn continuation_ids(&self, peer_last_ids: &[BlockId], limit: usize) -> Vec<[u8; 32]> {
            self.continuation_calls.fetch_add(1, Ordering::Relaxed);
            let common = if peer_last_ids.is_empty() {
                0 // fresh peer — serve from height 1
            } else {
                match peer_last_ids.iter().find_map(on_chain_height) {
                    Some(h) => h,
                    None => return Vec::new(), // alien chain
                }
            };
            let to = TIP.min(common + limit as u32);
            (common + 1..=to).map(id_at).collect()
        }

        async fn active_parameters(&self) -> Parameters {
            unreachable!("not called in serve tests")
        }

        async fn is_epoch_boundary(&self, _height: u32) -> bool {
            unreachable!("not called in serve tests")
        }

        async fn compute_expected_parameters(
            &self,
            _epoch_boundary_height: u32,
            _block_proposed_update: &[u8],
        ) -> Result<Parameters, ChainError> {
            unreachable!("not called in serve tests")
        }

        async fn apply_epoch_boundary_parameters(
            &self,
            _params: Parameters,
            _proposed_update_bytes: Vec<u8>,
        ) {
            unreachable!("not called in serve tests")
        }

        async fn active_proposed_update_bytes(&self) -> Vec<u8> {
            unreachable!("not called in serve tests")
        }

        async fn verify_nipopow_envelope(
            &self,
            _envelope_body: &[u8],
        ) -> Result<Vec<Header>, ChainError> {
            unreachable!("not called in serve tests")
        }

        async fn is_better_nipopow(&self, _this: &[u8], _than: &[u8]) -> Result<bool, ChainError> {
            unreachable!("not called in serve tests")
        }

        async fn install_nipopow_suffix(
            &self,
            _suffix_head: Header,
            _suffix_tail: Vec<Header>,
        ) -> Result<(), ChainError> {
            unreachable!("not called in serve tests")
        }

        async fn voting_length(&self) -> u32 {
            1024
        }
    }

    /// Transport recording every send; never delivers events (tests call
    /// `handle_event` directly).
    struct RecordingTransport {
        sent: Arc<Mutex<Vec<(PeerId, ProtocolMessage)>>>,
    }

    impl SyncTransport for RecordingTransport {
        async fn send_to(
            &self,
            peer: PeerId,
            message: ProtocolMessage,
        ) -> Result<(), Box<dyn std::error::Error + Send>> {
            self.sent.lock().unwrap().push((peer, message));
            Ok(())
        }

        async fn outbound_peers(&self) -> Vec<PeerId> {
            Vec::new()
        }

        async fn next_event(&mut self) -> Option<ProtocolEvent> {
            std::future::pending().await
        }
    }

    struct NoopStore;

    impl SyncStore for NoopStore {
        async fn has_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> bool {
            unreachable!("not called in serve tests")
        }
        async fn get_modifier(&self, _type_id: u8, _id: &[u8; 32]) -> Option<Vec<u8>> {
            unreachable!("not called in serve tests")
        }
        async fn validated_height(&self) -> Option<u32> {
            None
        }
        async fn set_validated_height(&self, _height: u32) {}
        async fn flush(&self) {}
        async fn prune_below_height(
            &self,
            _horizon: u32,
            _type_ids: &[u8],
        ) -> Result<usize, String> {
            unreachable!("not called in serve tests")
        }
        async fn min_height_present(&self, _type_id: u8) -> Result<Option<u32>, String> {
            unreachable!("not called in serve tests")
        }
    }

    /// Never invoked — `handle_event`'s SyncInfo arm touches no validator.
    struct NoopValidator;

    impl BlockValidator for NoopValidator {
        fn apply_state(
            &mut self,
            _header: &Header,
            _block_txs: &[u8],
            _ad_proofs: Option<&[u8]>,
            _extension: &[u8],
            _preceding_headers: &[Header],
            _active_params: &Parameters,
            _expected_boundary_params: Option<&Parameters>,
            _expected_proposed_update: Option<&[u8]>,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            unreachable!("not called in serve tests")
        }
        fn validated_height(&self) -> u32 {
            unreachable!("not called in serve tests")
        }
        fn current_digest(&self) -> &ADDigest {
            unreachable!("not called in serve tests")
        }
        fn reset_to(&mut self, _height: u32, _digest: ADDigest) -> Result<(), ValidationError> {
            unreachable!("not called in serve tests")
        }
        fn state_persistence(&self) -> Option<&dyn StatePersistence> {
            None
        }
    }

    /// Messages recorded by [`RecordingTransport`], shared with the test.
    type SentLog = Arc<Mutex<Vec<(PeerId, ProtocolMessage)>>>;

    fn build_sync() -> (
        HeaderSync<RecordingTransport, ServeChain, NoopStore, NoopValidator>,
        SentLog,
    ) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (_progress_tx, progress_rx) = mpsc::channel(1);
        let (_dc_tx, delivery_control_rx) = mpsc::unbounded_channel::<DeliveryControl>();
        let (_dd_tx, delivery_data_rx) = mpsc::channel::<DeliveryData>(1);
        let sync = HeaderSync::new(
            SyncConfig::default(),
            RecordingTransport {
                sent: Arc::clone(&sent),
            },
            ServeChain::new(),
            NoopStore,
            None,
            progress_rx,
            delivery_control_rx,
            delivery_data_rx,
            None,
            None,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU64::new(0)),
            shutdown_rx,
        );
        (sync, sent)
    }

    /// The peer we're nominally syncing from in every test.
    const SYNC_PEER: PeerId = PeerId(1);

    fn sync_info_event(peer_id: PeerId, specs: &[u32]) -> ProtocolEvent {
        let body: Vec<u8> = specs.iter().flat_map(|s| s.to_le_bytes()).collect();
        ProtocolEvent::Message {
            peer_id,
            message: ProtocolMessage::SyncInfo { body },
        }
    }

    /// All header Invs recorded by the transport.
    fn sent_header_invs(
        sent: &Mutex<Vec<(PeerId, ProtocolMessage)>>,
    ) -> Vec<(PeerId, Vec<[u8; 32]>)> {
        sent.lock()
            .unwrap()
            .iter()
            .filter_map(|(p, m)| match m {
                ProtocolMessage::Inv { modifier_type, ids } if *modifier_type == HEADER_TYPE_ID => {
                    Some((*p, ids.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Recipients of every SyncInfo recorded by the transport, in send order.
    fn sent_sync_info_peers(sent: &Mutex<Vec<(PeerId, ProtocolMessage)>>) -> Vec<PeerId> {
        sent.lock()
            .unwrap()
            .iter()
            .filter_map(|(p, m)| matches!(m, ProtocolMessage::SyncInfo { .. }).then_some(*p))
            .collect()
    }

    #[tokio::test]
    async fn fresh_peer_empty_anchors_served_from_height_1_capped_at_400() {
        // The live repro: a from-genesis peer announces an empty SyncInfo.
        // JVM continuationIds serves the chain start for an empty anchor
        // list — the peer must get Inv, not silence.
        let (mut sync, sent) = build_sync();
        let fresh_peer = PeerId(7);

        let result = sync
            .handle_event(SYNC_PEER, sync_info_event(fresh_peer, &[]))
            .await;

        assert!(matches!(result, EventResult::Continue));
        let invs = sent_header_invs(&sent);
        assert_eq!(
            invs.len(),
            1,
            "fresh peer must receive exactly one header Inv"
        );
        let (to, ids) = &invs[0];
        assert_eq!(*to, fresh_peer, "Inv goes to the SyncInfo sender");
        assert_eq!(ids.len(), 400, "continuation capped at 400 (JVM size)");
        assert_eq!(ids[0], id_at(1), "continuation starts at height 1");
        assert_eq!(ids[399], id_at(400), "continuation is ascending");
    }

    #[tokio::test]
    async fn behind_peer_served_from_anchor_plus_one_even_on_caught_up_path() {
        // Peer anchored mid-chain at 450 — serve 451..=TIP. The sender is
        // our active sync peer reporting a tip below ours, so the handler
        // also flips to Synced — the serve must happen anyway (the old
        // early-return skipped everything below it).
        let (mut sync, sent) = build_sync();

        let result = sync
            .handle_event(SYNC_PEER, sync_info_event(SYNC_PEER, &[450, 434, 322]))
            .await;

        assert!(
            matches!(result, EventResult::Synced),
            "sync peer at lower tip still flips the consume side to Synced"
        );
        let invs = sent_header_invs(&sent);
        assert_eq!(invs.len(), 1, "behind peer must receive a header Inv");
        let (to, ids) = &invs[0];
        assert_eq!(*to, SYNC_PEER);
        let expected: Vec<[u8; 32]> = (451..=TIP).map(id_at).collect();
        assert_eq!(
            *ids, expected,
            "ids ascend from the newest anchor + 1 to our tip"
        );
    }

    #[tokio::test]
    async fn peer_at_our_tip_gets_no_inv() {
        // Equal chains — JVM does nothing for Equal. The continuation
        // self-gates (common point = our tip → empty → nothing sent).
        let (mut sync, sent) = build_sync();

        let result = sync
            .handle_event(SYNC_PEER, sync_info_event(SYNC_PEER, &[TIP, TIP - 16]))
            .await;

        assert!(matches!(result, EventResult::Synced));
        assert!(
            sent_header_invs(&sent).is_empty(),
            "a peer at our tip must not be served an Inv"
        );
    }

    #[tokio::test]
    async fn syncinfo_response_addressed_to_sender_not_sync_peer() {
        // A peer that finished downloading our headers (now at our tip) and
        // is NOT our active sync peer sends a final SyncInfo. Our SyncInfo
        // response is its only caught-up signal — the continuation is empty
        // (equal chains ⇒ no Inv), so its transition to `synced()` depends
        // entirely on that response coming back to IT. The handler must
        // address the response to the SENDER (`peer_id`), not our active sync
        // peer (`peer`). Misaddressing it to the sync peer stranded a
        // from-genesis rust peer in the header loop forever (2026-06-08);
        // JVM processSync replies to `remote`. See ../facts/sync.md
        // § "Serving sync".
        let (mut sync, sent) = build_sync();
        // The response is rate-limited away by default (last_sync_sent is set
        // at construction, min interval 200ms). Drop the floor so the single
        // response fires and we can assert where it was addressed.
        sync.config.min_sync_send_interval = std::time::Duration::ZERO;
        let requester = PeerId(7);
        assert_ne!(requester, SYNC_PEER, "requester must differ from sync peer");

        let result = sync
            .handle_event(SYNC_PEER, sync_info_event(requester, &[TIP]))
            .await;

        // Equal chains, sender ≠ sync peer: the caught-up flip only fires for
        // the sync peer, so the consume side stays Continue and the response
        // path is reached. No continuation Inv (common point = our tip).
        assert!(matches!(result, EventResult::Continue));
        assert!(
            sent_header_invs(&sent).is_empty(),
            "a peer at our tip is not served a continuation Inv"
        );
        assert_eq!(
            sent_sync_info_peers(&sent),
            vec![requester],
            "SyncInfo response must go to the sender, never the active sync \
             peer (regression: send_sync_info(peer) addressed it to SYNC_PEER)"
        );
    }

    #[tokio::test]
    async fn peer_ahead_gets_no_inv_and_no_continuation_call() {
        // Peer strictly ahead (JVM: Older) — never served, even though its
        // deeper anchors sit on our chain (a continuation call would
        // return ids; the handler must gate before the call).
        let (mut sync, sent) = build_sync();
        let ahead_peer = PeerId(9);

        let result = sync
            .handle_event(SYNC_PEER, sync_info_event(ahead_peer, &[550, 534, 422]))
            .await;

        assert!(
            matches!(result, EventResult::BehindPeer(p) if p == ahead_peer),
            "consume side still switches to the ahead peer"
        );
        assert!(
            sent_header_invs(&sent).is_empty(),
            "an ahead peer must not be served an Inv"
        );
        assert_eq!(
            sync.chain.continuation_calls.load(Ordering::Relaxed),
            0,
            "the handler gates ahead peers before computing a continuation"
        );
    }

    #[tokio::test]
    async fn forked_peer_served_from_common_point_plus_one() {
        // Peer at our height on a fork: tip + first anchor off-chain, a
        // deeper anchor at 372 on our chain. JVM: Fork → serve from the
        // best common point + 1.
        let (mut sync, sent) = build_sync();
        let forked_peer = PeerId(5);
        const FORK: u32 = 0x8000_0000;

        let result = sync
            .handle_event(
                SYNC_PEER,
                sync_info_event(forked_peer, &[FORK | TIP, FORK | 484, 372]),
            )
            .await;

        assert!(matches!(result, EventResult::Continue));
        let invs = sent_header_invs(&sent);
        assert_eq!(invs.len(), 1, "forked peer must receive a header Inv");
        let (to, ids) = &invs[0];
        assert_eq!(*to, forked_peer);
        let expected: Vec<[u8; 32]> = (373..=TIP).map(id_at).collect();
        assert_eq!(
            *ids, expected,
            "ids ascend from the common point (372) + 1 to our tip"
        );
    }
}

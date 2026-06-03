use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::apply_state_error::classify_apply_state_error;
use crate::delivery::{DeliveryControl, DeliveryData, DeliveryTracker};
use crate::sweep_backoff::StallDetail;
use enr_chain::{
    StateType, HEADER_TYPE_ID, BLOCK_TRANSACTIONS_TYPE_ID, AD_PROOFS_TYPE_ID, EXTENSION_TYPE_ID,
    TRANSACTION_TYPE_ID,
};

use crate::traits::{SyncChain, SyncStore, SyncTransport};
use ergo_validation::BlockValidator;

/// Number of block section requests to send per batch (per type).
/// The JVM's Akka layer can silently drop large ModifierResponse bodies via
/// backpressure, so this is capped below the JVM's `desiredInvObjects` (400).
/// 64 per type × 2 types = 128 sections per cycle = 64 blocks/cycle.
fn is_block_section_type(type_id: u8) -> bool {
    matches!(type_id, BLOCK_TRANSACTIONS_TYPE_ID | AD_PROOFS_TYPE_ID | EXTENSION_TYPE_ID)
}

/// Result of a paired state/store flush.
///
/// Discriminates three cases:
/// - `Flushed(M)`: validator flushed successfully at height `M`; modifier
///   store's `validated_height` was advanced to `M`.
/// - `NoValidator`: light-mode path; there is no state to flush, and
///   `validated_height` is not recorded.
/// - `Failed`: validator's `flush()` returned an error; modifier store's
///   `validated_height` was NOT advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlushOutcome {
    Flushed(u32),
    NoValidator,
    Failed,
}

impl FlushOutcome {
    /// Whether the caller should advance its `last_flush_height` bookkeeping.
    /// Advances on either a successful flush or no-validator (light mode);
    /// stays put on flush failure so the next `should_flush()` retries.
    pub(crate) fn advances_last_flush(self) -> bool {
        matches!(self, FlushOutcome::Flushed(_) | FlushOutcome::NoValidator)
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
        Some(v) => match v.flush() {
            Ok(()) => FlushOutcome::Flushed(v.validated_height()),
            Err(e) => {
                tracing::warn!(height = height_context, error = %e, "validator flush failed");
                FlushOutcome::Failed
            }
        },
    }
}

/// Complete the cross-DB flush pair on the store side.
///
/// Takes the outcome of [`try_flush_validator`] (validator already flushed)
/// and finishes the durability handshake on the store side. Order is
/// load-bearing: `set_validated_height(M)` (only on `Flushed`) then
/// `store.flush()`. A failed validator flush MUST NOT advance the modifier
/// store's recorded `validated_height`; light mode (no validator) skips the
/// `set_validated_height` write entirely. The store flush always runs to
/// fsync any accumulated section writes.
pub(crate) async fn complete_store_flush_pair<S: SyncStore>(
    outcome: FlushOutcome,
    store: &S,
) {
    if let FlushOutcome::Flushed(m) = outcome {
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
        Ok(n) => tracing::debug!(
            pruned = n,
            horizon,
            "pruned modifier rows"
        ),
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
    state_applied_height: u32,
    /// Highest height where evaluate_scripts() completed successfully.
    /// Advances in-order as results arrive. Persisted for startup re-eval.
    script_verified_height: u32,
    /// Number of eval tasks dispatched but not yet drained from the channel.
    evals_in_flight: u32,
    /// Channel for sending eval results from rayon pool.
    eval_tx: CrossbeamSender<(u32, Result<(), ergo_validation::ValidationError>)>,
    /// Channel for receiving eval results.
    eval_rx: CrossbeamReceiver<(u32, Result<(), ergo_validation::ValidationError>)>,
    /// Shared downloaded_height for the API (read by fastsync to avoid redundant work).
    shared_downloaded_height: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Gate controlling whether block/header/tx ModifierRequest sends actually fire.
    /// Closed (false) at construction, opened (true) by the main crate after the
    /// boot-time fastsync bootstrap decision resolves. See facts/sync.md "Bootstrap Mode".
    block_request_gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Peer's chain tip, updated on every incoming SyncInfo to the max observed.
    /// Read by the main crate to compute the bootstrap gap.
    peer_chain_tip: std::sync::Arc<std::sync::atomic::AtomicU32>,
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
    /// Exponential backoff gating the validation sweep when the applied
    /// tip fails to advance — a deterministic block failure (apply_state
    /// error OR a deferred-eval rollback; both leave `validated_height()`
    /// pinned). Derived purely from the validator's applied tip, so its
    /// stall detection is blind to which subsystem rejected the block. Also
    /// the single emitter of the contract `validation_stuck` event, fired
    /// once a frontier has stalled 5 sweeps in a row (the caller hands in
    /// the `error_kind`/`missing_key` label). Resets on real progress or a
    /// frontier change. In-memory — a restart is a legitimate reset. See
    /// [`crate::sweep_backoff`].
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
    // 13-arg constructor reflects the dependency-inversion surface (4 trait
    // objects + 6 channels/atomics + 3 config-ish args). Bundling these would
    // hide the wiring without simplifying it. A builder is reasonable future
    // work but not justified for one caller (`src/main.rs`).
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
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        let tracker = DeliveryTracker::with_config(config.delivery_timeout, config.max_delivery_checks);
        let initial_validated = validator.as_ref().map_or(0, |v| v.validated_height());
        let (eval_tx, eval_rx) = crossbeam_channel::unbounded();
        let last_flush_height = initial_validated;
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
            script_verified_height: initial_validated,
            evals_in_flight: 0,
            eval_tx,
            eval_rx,
            shared_downloaded_height,
            block_request_gate,
            peer_chain_tip,
            last_flush_height,
            at_tip_request_tx: None,
            at_tip_validator_rx: None,
            sweep_backoff: crate::sweep_backoff::SweepBackoff::default(),
            shutdown_rx,
        }
    }

    /// Configure the at-tip transition channels. When `synced()` is first
    /// entered, the existing validator is taken (releasing its AVL storage
    /// handle), its height is sent on `request_tx`, and the new validator
    /// is awaited on `validator_rx`. The integrator handles the actual
    /// storage reopen + validator rebuild on the other end of the pair.
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
            match crate::light_bootstrap::run_light_bootstrap(
                &mut self.transport,
                &self.chain,
            )
            .await
            {
                Ok(()) => {
                    let height = self.chain.chain_height().await;
                    // Light mode treats all installed headers as "validated"
                    // — the proof's PoW checks ARE the validation. There's
                    // no validator running and no block sections to download.
                    self.downloaded_height = height;
                    self.state_applied_height = height;
                    tracing::info!(height, "light bootstrap installed, entering tip-following sync");
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

        // Startup: load persisted script_verified_height.
        // If there's a gap (state applied but scripts not verified from a
        // previous unclean shutdown), accept it — the AVL digest already
        // proved state correctness. Proof boxes aren't available without
        // re-running apply_state, so re-evaluation isn't feasible here.
        if let Some(persisted_svh) = self.store.script_verified_height().await {
            self.script_verified_height = persisted_svh.min(self.state_applied_height);
            if persisted_svh < self.state_applied_height {
                let gap = self.state_applied_height - persisted_svh;
                tracing::info!(
                    persisted_svh,
                    state_applied_height = self.state_applied_height,
                    gap,
                    "startup: accepting script verification gap (AVL digest verified)"
                );
                // Advance to match — the gap blocks' state transitions are
                // already proven correct by the AVL digest check in apply_state.
                self.script_verified_height = self.state_applied_height;
                // Lock in the new floor durably so a restart before the next
                // flush doesn't re-discover the same gap. Without this the
                // persisted SVH stays stuck at the old value across restarts.
                self.store.set_script_verified_height(self.script_verified_height).await;
                self.store.flush().await;
            }
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
        if let FlushOutcome::Flushed(m) = outcome {
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

        loop {
            let until_next = self.config.sync_interval
                .saturating_sub(self.last_scheduled_sync.elapsed());
            let until_delivery = self.config.delivery_check_interval
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
            }
        }
    }

    /// Request announced modifiers from a peer and track delivery.
    ///
    /// Filters out IDs already in the store or pending in the delivery
    /// tracker before sending. Chunks into messages of at most 400 IDs
    /// to stay within the JVM's `desiredInvObjects` limit.
    async fn request_announced(&mut self, peer: PeerId, modifier_type: u8, ids: Vec<[u8; 32]>) {
        if !self.block_request_gate.load(std::sync::atomic::Ordering::Relaxed) {
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
            self.shared_downloaded_height.store(new_height, std::sync::atomic::Ordering::Relaxed);
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
        // cache that a mid-sweep deferred-eval rollback can leave stale
        // (ahead of the real tip). Deriving the start from the prover tip
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

            // Get preceding headers (up to 10) for ErgoStateContext
            let preceding_start = height.saturating_sub(10).max(1);
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
            let (expected_boundary_params, expected_proposed_update) =
                if self.chain.is_epoch_boundary(height).await {
                    let block_proposed_update =
                        match enr_chain::parse_extension_bytes(&extension) {
                            Ok((_header_id, fields)) => {
                                enr_chain::extract_disabling_rules_from_kv(&fields)
                            }
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

                    // Spawn deferred script evaluation on rayon pool
                    if let Some(eval) = outcome.deferred_eval {
                        let tx = self.eval_tx.clone();
                        self.evals_in_flight += 1;
                        rayon::spawn(move || {
                            let h = eval.height;
                            let result = ergo_validation::evaluate_scripts(&eval);
                            let _ = tx.send((h, result));
                        });
                    }

                    // Non-blocking drain of completed eval results. A
                    // deferred-eval failure inside the drain rolls the
                    // validator back (and lowers `state_applied_height`).
                    // Detect it by the validator's own tip moving backwards
                    // across the drain — NOT by `state_applied_height` dipping
                    // below its pre-sweep value, which misses a rollback that
                    // lands exactly on the pre-sweep tip (the common case: the
                    // failing block was applied earlier in THIS sweep). On any
                    // backwards move, abort: continuing would feed `apply_state`
                    // a block past the rolled-back tip and surface a misleading
                    // `HeightMismatch`.
                    let pre_drain_tip =
                        self.validator.as_ref().map_or(validated_to, |v| v.validated_height());
                    self.drain_eval_results(false).await;
                    let post_drain_tip =
                        self.validator.as_ref().map_or(pre_drain_tip, |v| v.validated_height());

                    if post_drain_tip < pre_drain_tip {
                        tracing::warn!(
                            validated_to,
                            rolled_back_to = post_drain_tip,
                            "eval failure rolled back state during sweep — aborting"
                        );
                        validated_to = post_drain_tip;
                        // If this rollback pins the frontier, the backoff's
                        // validation_stuck must name the deferred-eval mode —
                        // the path the retired apply_state tracker never saw.
                        stall_detail = StallDetail::script_eval();
                        break;
                    }

                    // Progress report every 1000 blocks during large sweeps
                    let done = height - applied_tip;
                    if done.is_multiple_of(1000) && sweep_size > 100 {
                        let elapsed = sweep_start.elapsed().as_secs().max(1);
                        let rate = done as f64 / elapsed as f64;
                        let remaining = sweep_to - height;
                        let eta_secs = if rate > 0.0 { remaining as f64 / rate } else { 0.0 };
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
                        if let FlushOutcome::Flushed(m) = outcome {
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
                    let err_msg = e.to_string();
                    // Label the stall so the backoff's validation_stuck — if
                    // this frontier wedges for 5 sweeps — names the apply_state
                    // error kind and, for the AVL missing-key case, the key.
                    let (error_kind, missing_key) = classify_apply_state_error(&err_msg);
                    stall_detail = StallDetail { error_kind, missing_key };
                    tracing::error!(height, error = %err_msg, "apply_state failed");
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
        let true_tip = self.validator.as_ref().map_or(validated_to, |v| v.validated_height());

        // Drive the sweep backoff off the authoritative applied tip. If the
        // sweep moved the tip past where it started, that is real progress —
        // clear any backoff. If it did not, the frontier is failing
        // deterministically (apply_state error, or an eval rollback that
        // pulled the tip back to where it began); arm/escalate the
        // exponential delay so the next retry is throttled, and — once the
        // frontier has stalled 5 sweeps in a row — emit the contract
        // `validation_stuck` event, labelled by `stall_detail`. The backoff
        // owns that emission for BOTH modes now (apply_state and the
        // deferred-eval rollback the retired tracker never saw). This
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
                // Cache was stale-ahead of the prover (e.g. a mid-sweep eval
                // rollback) — pulled back down to the truth.
                tracing::debug!(
                    state_applied_height = true_tip,
                    previous = prev,
                    "state applied height reconciled down to validator tip"
                );
            }
        }

        // After sweep: drain remaining eval results.
        // At chain tip (single block), drain synchronously to ensure
        // scripts are verified before accepting the next peer block.
        let blocking = sweep_size <= 1;
        self.drain_eval_results(blocking).await;

        // Durable flush at sweep end — catches the tail of blocks that
        // didn't hit a heap-threshold or max-blocks trigger mid-sweep. The
        // tip-following path (single block per sweep) always reaches this.
        let outcome =
            try_flush_validator(self.validator.as_ref(), self.state_applied_height);
        complete_store_flush_pair(outcome, &self.store).await;
        if let FlushOutcome::Flushed(m) = outcome {
            maybe_prune_at_horizon(&self.store, &self.chain, m, self.config.blocks_to_keep).await;
        }
        if outcome.advances_last_flush() {
            self.last_flush_height = self.state_applied_height;
        }
    }

    /// Drain completed script evaluation results from the channel.
    ///
    /// `blocking`: if true, block until all in-flight evals complete.
    /// If false, drain only what's immediately available (non-blocking).
    async fn drain_eval_results(&mut self, blocking: bool) {
        let mut verified: BTreeSet<u32> = BTreeSet::new();

        while self.evals_in_flight > 0 {
            let msg = if blocking {
                match self.eval_rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                }
            } else {
                match self.eval_rx.try_recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                }
            };

            self.evals_in_flight -= 1;
            let (height, result) = msg;
            match result {
                Ok(()) => {
                    verified.insert(height);
                }
                Err(e) => {
                    tracing::error!(
                        height, error = %e,
                        "script evaluation failed — rolling back"
                    );
                    self.handle_eval_failure(height).await;
                    return;
                }
            }
        }

        // Advance script_verified_height sequentially
        let prev = self.script_verified_height;
        while verified.contains(&(self.script_verified_height + 1)) {
            self.script_verified_height += 1;
            verified.remove(&self.script_verified_height);
        }
        // Persist on every advance. With Durability::None the write hits the
        // redb WAL only and gets fsynced when the paired store.flush() runs at
        // the next state-flush point — so persisted SVH always tracks
        // persisted state without an extra fsync per block.
        if self.script_verified_height > prev {
            self.store.set_script_verified_height(self.script_verified_height).await;
        }
    }

    /// Handle a deferred script evaluation failure by rolling back state.
    async fn handle_eval_failure(&mut self, failed_height: u32) {
        // Drain and discard remaining results
        while self.eval_rx.try_recv().is_ok() {}
        self.evals_in_flight = 0;

        let rollback_to = failed_height - 1;

        if let Some(v) = self.validator.as_mut() {
            if let Some(header) = self.chain.header_at(rollback_to).await {
                v.reset_to(rollback_to, header.state_root);
            } else {
                tracing::error!(rollback_to, "cannot find header for rollback");
                return;
            }
        }

        self.state_applied_height = rollback_to;
        self.script_verified_height = rollback_to;
        self.store.set_script_verified_height(self.script_verified_height).await;
        if self.downloaded_height > rollback_to {
            self.downloaded_height = rollback_to;
        }

        tracing::warn!(
            rollback_to,
            "state rolled back due to script eval failure"
        );
    }

    /// Handle a control-plane event (Reorg or NeedModifier).
    async fn handle_control_event(&mut self, ctrl: DeliveryControl, peer: PeerId) {
        match ctrl {
            DeliveryControl::NeedModifier { type_id, id } => {
                tracing::info!(type_id, "pipeline needs modifier for reorg");
                self.request_announced(peer, type_id, vec![id]).await;
            }
            DeliveryControl::Reorg { fork_point, old_tip, new_tip } => {
                tracing::info!(fork_point, old_tip, new_tip, "reorg: adjusting section queue and watermark");

                // Drain and discard in-flight eval results
                while self.eval_rx.try_recv().is_ok() {}
                self.evals_in_flight = 0;

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
                    if let Some(fork_header) = self.chain.header_at(fork_point).await {
                        if let Some(v) = self.validator.as_mut() {
                            v.reset_to(fork_point, fork_header.state_root);
                        }
                    }
                    self.state_applied_height = fork_point;
                    self.script_verified_height = fork_point;
                    self.store.set_script_verified_height(self.script_verified_height).await;
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
                ProtocolMessage::Inv { modifier_type, .. }
                    if modifier_type == HEADER_TYPE_ID =>
                {
                    let height = self.chain.chain_height().await;
                    tracing::debug!(height, "peer reports no more headers");
                    EventResult::Synced
                }

                // Inv with block section IDs: request them
                ProtocolMessage::Inv { modifier_type, ids }
                    if is_block_section_type(modifier_type) && !ids.is_empty() =>
                {
                    tracing::debug!(type_id = modifier_type, count = ids.len(), "block section Inv → requesting");
                    self.request_announced(peer_id, modifier_type, ids).await;
                    EventResult::Continue
                }

                // Peer's SyncInfo: respond with ours to keep the bidirectional
                // exchange alive. The JVM expects this — without it, per-peer
                // sync state goes stale and the JVM stops sending Inv.
                ProtocolMessage::SyncInfo { body } => {
                    if let Ok(info) = self.chain.parse_sync_info(&body) {
                        let peer_heights = C::sync_info_heights(&info);
                        let our_height = self.chain.chain_height().await;

                        if let Some(&peer_tip) = peer_heights.first() {
                            // Publish the max peer tip we've seen — main crate
                            // reads this for the bootstrap gap decision.
                            let prev = self.peer_chain_tip
                                .load(std::sync::atomic::Ordering::Relaxed);
                            if peer_tip > prev {
                                self.peer_chain_tip
                                    .store(peer_tip, std::sync::atomic::Ordering::Relaxed);
                            }

                            // "Caught up" only counts from the peer we're syncing from
                            if peer_id == peer && peer_tip <= our_height {
                                tracing::debug!(our_height, peer_tip, "caught up with peer");
                                return EventResult::Synced;
                            }
                            // Any peer ahead of us triggers a switch
                            if peer_tip > our_height + 1 {
                                tracing::debug!(our_height, peer_tip, peer = %peer_id, "peer is ahead, resuming sync");
                                return EventResult::BehindPeer(peer_id);
                            }
                        }
                    }

                    // Respond with our SyncInfo — mirrors JVM's syncSendNeeded behavior
                    let _ = self.send_sync_info(peer).await;
                    EventResult::Continue
                }

                // Transaction Inv: request unconfirmed txs from peers
                ProtocolMessage::Inv { modifier_type, ids }
                    if modifier_type == TRANSACTION_TYPE_ID && !ids.is_empty() =>
                {
                    tracing::debug!(count = ids.len(), "tx Inv → requesting transactions");
                    self.request_announced(peer_id, modifier_type, ids).await;
                    EventResult::Continue
                }

                ProtocolMessage::Inv { modifier_type, ref ids } => {
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
            && self.block_request_gate.load(std::sync::atomic::Ordering::Relaxed)
        {
            let peers = self.transport.outbound_peers().await;
            for retry in &result.retries {
                let target = peers.iter()
                    .find(|&&p| p != retry.failed_peer)
                    .copied()
                    .unwrap_or(retry.failed_peer);

                self.tracker.mark_requested(&[retry.id], target, retry.type_id);
                let _ = self.transport.send_to(
                    target,
                    ProtocolMessage::ModifierRequest {
                        modifier_type: retry.type_id,
                        ids: vec![retry.id],
                    },
                ).await;
            }
            tracing::debug!(count = result.retries.len(), "re-requested timed-out modifiers");
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
            self.request_announced(target, modifier_type, ids.to_vec()).await;
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

        // ── Storage reopen handshake ──
        if let (Some(req_tx), Some(val_rx)) =
            (self.at_tip_request_tx.take(), self.at_tip_validator_rx.take())
        {
            if let Some(validator) = self.validator.take() {
                let height = validator.validated_height();
                // Persist any pending in-memory state BEFORE dropping. Without
                // this, blocks applied since the last cold-sync flush remain in
                // the redb write tx and are GC'd with the prover. The rebuilt
                // validator would then load the older stored digest while sync
                // (and the new validator's height field) believe state is at
                // `height`, leaving an N-block gap where any later block that
                // spends an output from the gap fails with "Key does not exist".
                if let Err(e) = validator.flush() {
                    tracing::error!(
                        height,
                        error = ?e,
                        "at-tip: flush before rebuild failed; aborting rebuild and keeping old validator"
                    );
                    self.validator = Some(validator);
                    return;
                }
                drop(validator); // releases AVL storage so main can reopen it
                tracing::info!(height, "at-tip: requesting validator rebuild with synced cache");
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
                        tracing::error!("at-tip: validator rebuild channel closed; sync cannot proceed");
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

        let window_end = std::cmp::min(
            self.downloaded_height + Self::DOWNLOAD_WINDOW,
            chain_height,
        );

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
            let hex_prefix: String = body.iter().take(40)
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
        ApplyStateOutcome, BlockValidator, Parameters, ValidationError,
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

        async fn script_verified_height(&self) -> Option<u32> {
            unimplemented!("not called in flush-ordering tests")
        }

        async fn set_script_verified_height(&self, _height: u32) {
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

        fn reset_to(&mut self, _height: u32, _digest: ADDigest) {
            unimplemented!("not called in flush-ordering tests")
        }

        fn flush(&self) -> Result<(), ValidationError> {
            self.flush_result
                .map_err(|s| ValidationError::ProofVerificationFailed(s.to_string()))
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
            vec![
                StoreCall::SetValidatedHeight(1_785_000),
                StoreCall::Flush,
            ],
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

    #[test]
    fn flush_outcome_advances_last_flush_only_on_success_or_no_validator() {
        assert!(FlushOutcome::Flushed(42).advances_last_flush());
        assert!(FlushOutcome::NoValidator.advances_last_flush());
        assert!(!FlushOutcome::Failed.advances_last_flush());
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
        ApplyStateOutcome, BlockValidator, Parameters, ValidationError,
    };
    use std::sync::atomic::{AtomicBool, AtomicU32};
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
            Self { calls: Mutex::new(Vec::new()) }
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

        async fn script_verified_height(&self) -> Option<u32> {
            None
        }

        async fn set_script_verified_height(&self, _height: u32) {
            unreachable!("not called when script_verified_height returns None")
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

        fn reset_to(&mut self, _height: u32, _digest: ADDigest) {
            unreachable!("not called in shutdown-flush tests")
        }

        fn flush(&self) -> Result<(), ValidationError> {
            *self.flush_count.lock().unwrap() += 1;
            self.flush_result
                .map_err(|s| ValidationError::ProofVerificationFailed(s.to_string()))
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

        async fn is_better_nipopow(
            &self,
            _this: &[u8],
            _than: &[u8],
        ) -> Result<bool, ChainError> {
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

        shutdown_tx.send(()).expect("sync must be polling shutdown_rx");
        sync.run().await;

        assert_eq!(
            sync.validator.as_ref().unwrap().flush_count(),
            1,
            "validator.flush() must be called exactly once on shutdown"
        );
        assert_eq!(
            sync.store.calls(),
            vec![
                StoreCall::SetValidatedHeight(1_785_000),
                StoreCall::Flush,
            ],
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
    use enr_chain::{ChainError, SyncInfo};
    use ergo_chain_types::{ADDigest, Header};
    use ergo_validation::{
        ApplyStateOutcome, BlockValidator, Parameters, ValidationError,
    };
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use tracing_subscriber::fmt::MakeWriter;

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

        async fn script_verified_height(&self) -> Option<u32> {
            None
        }

        async fn set_script_verified_height(&self, _height: u32) {
            unreachable!("not called when script_verified_height returns None")
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
            horizon: u32,
            type_ids: &[u8],
        ) -> Result<usize, String> {
            self.calls.lock().unwrap().push(StoreCall::PruneBelowHeight {
                horizon,
                type_ids: type_ids.to_vec(),
            });
            Ok(0)
        }

        async fn min_height_present(
            &self,
            type_id: u8,
        ) -> Result<Option<u32>, String> {
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

        fn reset_to(&mut self, _height: u32, _digest: ADDigest) {
            unreachable!("not called in blocks_to_keep tests")
        }

        fn flush(&self) -> Result<(), ValidationError> {
            Ok(())
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

        async fn is_better_nipopow(
            &self,
            _this: &[u8],
            _than: &[u8],
        ) -> Result<bool, ChainError> {
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
            StoreCall::PruneBelowHeight { horizon, type_ids } => {
                Some((*horizon, type_ids.clone()))
            }
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
            FixedVotingLengthChain { voting_length: 1024 },
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
            FixedVotingLengthChain { voting_length: 1024 },
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
            FixedVotingLengthChain { voting_length: 1024 },
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
            fn reset_to(&mut self, _h: u32, _d: ADDigest) {
                unreachable!()
            }
            fn flush(&self) -> Result<(), ValidationError> {
                Err(ValidationError::ProofVerificationFailed("nope".into()))
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
            FixedVotingLengthChain { voting_length: 1024 },
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
            shutdown_rx,
        );

        shutdown_tx.send(()).unwrap();
        sync.run().await;

        assert!(
            last_prune_call(&sync.store.calls()).is_none(),
            "failed validator.flush() MUST NOT trigger prune"
        );
    }

    // ----- startup WARN capture -----

    #[derive(Clone, Default)]
    struct CaptureWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl CaptureWriter {
        fn captured(&self) -> String {
            String::from_utf8(self.buf.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for CaptureWriter {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // Takes the future directly rather than a closure so the caller can
    // pass `sync.maybe_warn_reclaimable_bodies()` — a future borrowing
    // `&mut sync` — without wrapping it in `move || async move {…}` to
    // satisfy capture rules. The subscriber's `_guard` outlives the
    // `await`, so emits from the future are captured.
    async fn capture_warn<Fut>(f: Fut) -> String
    where
        Fut: std::future::Future<Output = ()>,
    {
        let writer = CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer.clone())
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        f.await;
        drop(_guard);
        writer.captured()
    }

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
            FixedVotingLengthChain { voting_length: 1024 },
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
            FixedVotingLengthChain { voting_length: 1024 },
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
            FixedVotingLengthChain { voting_length: 1024 },
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
            FixedVotingLengthChain { voting_length: 1024 },
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
    //! Regression tests for the validation-sweep resume wedge.
    //!
    //! The bug: the sweep derived its start height from
    //! `state_applied_height` (a cache) instead of the validator's true
    //! applied tip (`validated_height()`). A mid-sweep deferred-eval
    //! rollback could leave the cache ahead of the prover; every later
    //! sweep then fed `apply_state` a non-consecutive block, which the
    //! validator's consecutiveness guard correctly rejected
    //! (`expected N, got N+k`), looping forever even though the skipped
    //! blocks were on disk.
    //!
    //! The fix anchors both the sweep start AND the post-sweep cache on
    //! `validated_height()`. These tests construct the desynced state
    //! directly and assert the sweep resumes at `applied_tip + 1` and
    //! applies the intervening on-disk blocks instead of wedging.
    use super::*;
    use enr_chain::{ChainError, SyncInfo};
    use ergo_chain_types::{ADDigest, Header};
    use ergo_validation::{ApplyStateOutcome, BlockValidator, Parameters, ValidationError};
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use std::sync::Arc;

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
    /// lowers the height as a successful storage rollback would. Records the
    /// exact sequence of heights it actually applied so a test can prove the
    /// sweep resumed at the right block.
    struct SweepValidator {
        validated_height: u32,
        digest: ADDigest,
        applied: Vec<u32>,
        /// When `Some(h)`, `apply_state` rejects height `h` every time with a
        /// `StateRootMismatch`, modelling a deterministic divergence that
        /// never advances the tip past `h - 1`. The backoff keys on the tip
        /// stall, not the error kind, so this stands in equally for an
        /// `apply_state` error and a deferred-eval rollback.
        fail_at: Option<u32>,
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
                fail_at: None,
                attempts: 0,
            }
        }

        fn failing_at(mut self, height: u32) -> Self {
            self.fail_at = Some(height);
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
                return Err(ValidationError::HeightMismatch { expected, got: header.height });
            }
            self.validated_height = header.height;
            self.applied.push(header.height);
            Ok(ApplyStateOutcome {
                epoch_boundary_params: None,
                epoch_boundary_proposed_update: None,
                deferred_eval: None,
            })
        }

        fn validated_height(&self) -> u32 {
            self.validated_height
        }

        fn current_digest(&self) -> &ADDigest {
            &self.digest
        }

        fn reset_to(&mut self, height: u32, _digest: ADDigest) {
            self.validated_height = height;
        }
    }

    /// Chain that hands out a header for every height and is never at an
    /// epoch boundary, so the sweep takes its plain (non-voting) path.
    struct SweepChain;

    impl SyncChain for SweepChain {
        async fn chain_height(&self) -> u32 {
            1_000_000
        }
        async fn build_sync_info(&self) -> Vec<u8> {
            unreachable!()
        }
        async fn header_at(&self, height: u32) -> Option<Header> {
            (height >= 1).then(|| fake_header(height))
        }
        async fn header_state_root(&self, _height: u32) -> Option<[u8; 33]> {
            Some([0u8; 33])
        }
        fn parse_sync_info(&self, _body: &[u8]) -> Result<SyncInfo, ChainError> {
            unreachable!()
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
        async fn script_verified_height(&self) -> Option<u32> {
            None
        }
        async fn set_script_verified_height(&self, _height: u32) {}
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

    fn build_sync(
        validator: SweepValidator,
    ) -> HeaderSync<SweepTransport, SweepChain, SweepStore, SweepValidator> {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (_progress_tx, progress_rx) = mpsc::channel(1);
        let (_dc_tx, delivery_control_rx) = mpsc::unbounded_channel::<DeliveryControl>();
        let (_dd_tx, delivery_data_rx) = mpsc::channel::<DeliveryData>(1);
        HeaderSync::new(
            SyncConfig::default(),
            SweepTransport,
            SweepChain,
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
            shutdown_rx,
        )
    }

    #[tokio::test]
    async fn sweep_resumes_at_applied_tip_when_cache_ran_ahead() {
        // The exact wedge shape: applied tip 2665, cache desynced two ahead
        // at 2667, blocks 2666..=2670 all on disk. The OLD code computed
        // `from = state_applied_height + 1 = 2668`, fed the validator a
        // non-consecutive block, and looped forever on
        // `expected 2666, got 2668`. The fix must resume at
        // `validated_height() + 1 = 2666`.
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
        assert_eq!(v.validated_height(), 2670, "validator advanced to the downloaded tip");
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
        assert!(v.applied.is_empty(), "no blocks past the applied tip — nothing to apply");
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
        sync.validator.as_mut().unwrap().reset_to(2667, ADDigest::zero());

        sync.advance_state_applied_height().await;

        let v = sync.validator.as_ref().unwrap();
        assert_eq!(
            v.applied,
            vec![2668, 2669, 2670],
            "sweep re-applies from the rolled-back tip + 1, not from the stale cache"
        );
        assert_eq!(sync.state_applied_height, 2670, "cache back in sync with the prover");
    }

    #[tokio::test]
    async fn stalled_sweep_arms_backoff_and_gate_suppresses_retry() {
        // Frontier 2665 is wedged: block 2666 fails apply_state every attempt
        // (modelling a deterministic divergence — equally an eval rollback,
        // since the backoff keys on the tip stall, not the error). Blocks
        // 2666..=2670 are all on disk, so without the gate the sweep would
        // re-run at full tilt.
        let mut sync = build_sync(SweepValidator::at(2665).failing_at(2666));
        sync.downloaded_height = 2670;

        // First sweep: one attempt at 2666, it stalls, the tip stays pinned,
        // and the backoff arms at attempt 1.
        sync.advance_state_applied_height().await;
        assert_eq!(sync.validator.as_ref().unwrap().attempts, 1, "sweep made one attempt");
        assert_eq!(
            sync.validator.as_ref().unwrap().validated_height(),
            2665,
            "deterministic failure left the tip pinned"
        );
        assert_eq!(sync.sweep_backoff.consecutive(), 1, "non-advancing sweep armed the backoff");

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
        assert_eq!(sync.sweep_backoff.consecutive(), 1, "still attempt 1 — no fresh stall recorded");
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
        assert_eq!(sync.sweep_backoff.consecutive(), 0, "progress arms no backoff");
    }
}

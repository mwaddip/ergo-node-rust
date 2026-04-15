use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::delivery::{DeliveryControl, DeliveryData, DeliveryTracker};
use enr_chain::{
    StateType, HEADER_TYPE_ID, BLOCK_TRANSACTIONS_TYPE_ID, AD_PROOFS_TYPE_ID, EXTENSION_TYPE_ID,
};

use crate::traits::{SyncChain, SyncStore, SyncTransport};
use ergo_validation::BlockValidator;

/// Ergo unconfirmed transaction modifier type.
const TRANSACTION_TYPE_ID: u8 = 2;

/// Number of block section requests to send per batch (per type).
/// The JVM's Akka layer can silently drop large ModifierResponse bodies via
/// backpressure, so this is capped below the JVM's `desiredInvObjects` (400).
/// 64 per type × 2 types = 128 sections per cycle = 64 blocks/cycle.
fn is_block_section_type(type_id: u8) -> bool {
    matches!(type_id, BLOCK_TRANSACTIONS_TYPE_ID | AD_PROOFS_TYPE_ID | EXTENSION_TYPE_ID)
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
    /// Probe returning the current live heap in bytes. Main crate wires this
    /// to `tikv_jemalloc_ctl::stats::allocated` when built with jemalloc.
    /// When `None`, flushing is purely count-based (every `flush_max_blocks`).
    pub flush_probe: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
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
            flush_probe: None,
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
}

impl<T: SyncTransport, C: SyncChain, S: SyncStore, V: BlockValidator> HeaderSync<T, C, S, V> {
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
        }
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
        let since_last = height.saturating_sub(self.last_flush_height);
        if since_last >= self.config.flush_max_blocks {
            return true;
        }
        if since_last < self.config.flush_min_blocks {
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

    /// How many blocks ahead of `downloaded_height` to request sections for.
    /// Matches JVM's `FullBlocksToDownloadAhead = 192`.
    const DOWNLOAD_WINDOW: u32 = 192;

    /// Run the sync loop. Returns only if all event sources close.
    pub async fn run(&mut self) {
        tracing::info!("header sync started");

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
            }
        }

        loop {
            // Phase 1: wait for outbound peers (skip if already targeting one)
            if self.sync_peer.is_none() && !self.pick_sync_peer().await {
                return; // event stream ended
            }

            // Phase 2: sync from the selected peer
            match self.sync_from_peer().await {
                SyncOutcome::Synced => {
                    // Snapshot bootstrap: if enabled, no validator, and channels ready
                    tracing::info!(
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
                tracing::info!(peer = %peer, height, "starting header sync");
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
    /// Chunks into messages of at most 400 IDs to stay within the JVM's
    /// `desiredInvObjects` limit. Larger requests are silently rejected.
    async fn request_announced(&mut self, peer: PeerId, modifier_type: u8, ids: Vec<[u8; 32]>) {
        if !self.block_request_gate.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.tracker.mark_requested(&ids, peer, modifier_type);
        // JVM rejects ModifierRequest with >400 elements
        for chunk in ids.chunks(400) {
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
            tracing::info!(
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
        if self.validator.is_none() {
            return; // No validator yet (snapshot bootstrap pending)
        }
        if self.state_applied_height >= self.downloaded_height {
            return;
        }

        let sweep_from = self.state_applied_height + 1;
        let sweep_to = self.downloaded_height;
        let sweep_size = sweep_to - self.state_applied_height;
        let sweep_start = Instant::now();

        if sweep_size > 100 {
            tracing::info!(
                from = sweep_from,
                to = sweep_to,
                blocks = sweep_size,
                "=== VALIDATION SWEEP STARTED ==="
            );
        }

        let mut validated_to = self.state_applied_height;

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
            let expected_boundary_params = if self.chain.is_epoch_boundary(height).await {
                match self.chain.compute_expected_parameters(height).await {
                    Ok(p) => Some(p),
                    Err(e) => {
                        tracing::error!(height, error = %e, "compute_expected_parameters failed");
                        break;
                    }
                }
            } else {
                None
            };

            let result = self.validator.as_mut().unwrap().apply_state(
                &header,
                &block_txs,
                ad_proofs.as_deref(),
                &extension,
                &preceding,
                &active_params,
                expected_boundary_params.as_ref(),
            );

            match result {
                Ok(outcome) => {
                    // If this was an epoch-boundary block, apply the new parameters
                    // to chain state. The validator already verified they match expected.
                    if let Some(new_params) = outcome.epoch_boundary_params {
                        self.chain.apply_epoch_boundary_parameters(new_params).await;
                    }
                    validated_to = height;

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

                    // Non-blocking drain of completed eval results.
                    // Save state_applied_height before drain — handle_eval_failure
                    // may lower it if a deferred eval failed. Note: state_applied_height
                    // is NOT updated during the sweep loop (only in the post-loop code),
                    // so any decrease means a rollback happened inside drain.
                    let pre_drain_height = self.state_applied_height;
                    self.drain_eval_results(false).await;

                    if self.state_applied_height < pre_drain_height {
                        tracing::warn!(
                            validated_to,
                            rolled_back_to = self.state_applied_height,
                            "eval failure rolled back state during sweep — aborting"
                        );
                        validated_to = self.state_applied_height;
                        break;
                    }

                    // Progress report every 1000 blocks during large sweeps
                    let done = height - self.state_applied_height;
                    if done % 1000 == 0 && sweep_size > 100 {
                        let elapsed = sweep_start.elapsed().as_secs().max(1);
                        let rate = done as f64 / elapsed as f64;
                        let remaining = sweep_to - height;
                        let eta_secs = if rate > 0.0 { remaining as f64 / rate } else { 0.0 };
                        tracing::info!(
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
                        if let Some(v) = self.validator.as_ref() {
                            match v.flush() {
                                Ok(()) => {
                                    self.last_flush_height = height;
                                }
                                Err(e) => {
                                    tracing::warn!(height, error = %e, "validator flush failed");
                                }
                            }
                        } else {
                            self.last_flush_height = height;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(height, error = %e, "apply_state failed");
                    break;
                }
            }
        }

        if validated_to > self.state_applied_height {
            let advanced = validated_to - self.state_applied_height;
            self.state_applied_height = validated_to;
            // validator height is now persisted atomically inside state.redb
            // by UtxoValidator::apply_state via storage.update_with_height —
            // no separate hint write needed.

            if sweep_size > 100 {
                let elapsed = sweep_start.elapsed();
                let secs = elapsed.as_secs().max(1);
                let rate = advanced as f64 / secs as f64;
                tracing::info!(
                    from = sweep_from,
                    to = validated_to,
                    blocks = advanced,
                    elapsed = format!("{}m{}s", secs / 60, secs % 60),
                    rate = format!("{rate:.0}/s"),
                    "=== VALIDATION SWEEP COMPLETE ==="
                );
            } else {
                tracing::info!(
                    state_applied_height = validated_to,
                    advanced,
                    downloaded_height = self.downloaded_height,
                    "state applied height advanced"
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
        if let Some(v) = self.validator.as_ref() {
            match v.flush() {
                Ok(()) => {
                    self.last_flush_height = self.state_applied_height;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "validator flush after sweep failed");
                }
            }
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
        // Persist periodically (every 100 blocks) to limit re-eval on restart
        if self.script_verified_height > prev
            && self.script_verified_height / 100 > prev / 100
        {
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
                    tracing::info!(height, "peer reports no more headers");
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
                                tracing::info!(our_height, peer_tip, "caught up with peer");
                                return EventResult::Synced;
                            }
                            // Any peer ahead of us triggers a switch
                            if peer_tip > our_height + 1 {
                                tracing::info!(our_height, peer_tip, peer = %peer_id, "peer is ahead, resuming sync");
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
                    tracing::info!(
                        peer = %peer_id,
                        modifier_type,
                        count = ids.len(),
                        "unhandled Inv type"
                    );
                    EventResult::Continue
                }

                other => {
                    tracing::info!(
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

    /// Synced: periodically check for new blocks, download block sections.
    async fn synced(&mut self) {
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
                        tracing::info!("no outbound peers, returning to idle");
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

                // Sliding window section download — recompute and request every 2s
                _ = section_ticker.tick() => {
                    self.advance_downloaded_height().await;
                    self.request_next_sections().await;
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
            tracing::info!(
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
            tracing::info!(
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

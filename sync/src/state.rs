use std::collections::{HashMap, HashSet, VecDeque};

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::delivery::{DeliveryEvent, DeliveryTracker};
use enr_chain::{
    StateType, HEADER_TYPE_ID, BLOCK_TRANSACTIONS_TYPE_ID, AD_PROOFS_TYPE_ID, EXTENSION_TYPE_ID,
};

use crate::traits::{SyncChain, SyncStore, SyncTransport};
use ergo_validation::BlockValidator;

/// Number of block section requests to send per batch (per type).
/// The JVM's Akka layer can silently drop large ModifierResponse bodies via
/// backpressure, so this is capped below the JVM's `desiredInvObjects` (400).
/// 64 per type × 2 types = 128 sections per cycle = 64 blocks/cycle.
const SECTION_REQUEST_BATCH_SIZE: usize = 64;

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
    validator: V,
    progress: mpsc::Receiver<u32>,
    delivery_rx: mpsc::Receiver<DeliveryEvent>,
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
    /// Block section modifier IDs queued for download.
    section_queue: VecDeque<(u8, [u8; 32])>,
    /// Highest header height for which sections have been queued.
    sections_queued_to: u32,
    /// Highest height where ALL required block sections are in the store.
    /// Blocks at or below this height are ready for validation.
    downloaded_height: u32,
    /// Highest height where block sections have been validated.
    validated_height: u32,
}

impl<T: SyncTransport, C: SyncChain, S: SyncStore, V: BlockValidator> HeaderSync<T, C, S, V> {
    pub fn new(
        config: SyncConfig,
        transport: T,
        chain: C,
        store: S,
        validator: V,
        progress: mpsc::Receiver<u32>,
        delivery_rx: mpsc::Receiver<DeliveryEvent>,
    ) -> Self {
        let tracker = DeliveryTracker::with_config(config.delivery_timeout, config.max_delivery_checks);
        let initial_validated = validator.validated_height();
        Self {
            config,
            transport,
            chain,
            store,
            validator,
            progress,
            delivery_rx,
            tracker,
            sync_peer: None,
            stalled_peers: HashSet::new(),
            last_progress: Instant::now(),
            last_scheduled_sync: Instant::now(),
            last_sync_sent: Instant::now(),
            sync_sent_count: 0,
            section_queue: VecDeque::new(),
            sections_queued_to: 0,
            downloaded_height: initial_validated,
            validated_height: initial_validated,
        }
    }

    /// Queue missing block sections for headers from `from` to `to` (inclusive).
    ///
    /// Which sections are queued depends on `config.state_type`:
    /// - UTXO mode: BlockTransactions + Extension (no AD proofs)
    /// - Digest mode: all three including AD proofs
    ///
    /// Mirrors JVM's `ToDownloadProcessor.requiredModifiersForHeader`.
    async fn queue_sections_for_range(&mut self, from: u32, to: u32) {
        let mut queued = 0u32;
        for height in from..=to {
            let header = match self.chain.header_at(height).await {
                Some(h) => h,
                None => continue,
            };
            let sections = enr_chain::required_section_ids(&header, self.config.state_type);
            for (type_id, id) in &sections {
                if !self.store.has_modifier(*type_id, id).await {
                    self.section_queue.push_back((*type_id, *id));
                    queued += 1;
                }
            }
        }
        if queued > 0 {
            tracing::info!(queued, from, to, queue_len = self.section_queue.len(), "queued block sections");
        }
        if to > self.sections_queued_to {
            self.sections_queued_to = to;
        }
    }

    /// Run the sync loop. Returns only if all event sources close.
    pub async fn run(&mut self) {
        tracing::info!("header sync started");

        // Startup catch-up: queue sections for stored headers
        let tip = self.chain.chain_height().await;
        if tip > 0 {
            self.queue_sections_for_range(1, tip).await;
            self.advance_downloaded_height().await;
        }

        loop {
            // Phase 1: wait for outbound peers (skip if already targeting one)
            if self.sync_peer.is_none() && !self.pick_sync_peer().await {
                return; // event stream ended
            }

            // Phase 2: sync from the selected peer
            match self.sync_from_peer().await {
                SyncOutcome::Synced => self.synced().await,
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

                // Pipeline delivery notifications: received / evicted modifiers
                Some(event) = self.delivery_rx.recv() => {
                    match event {
                        DeliveryEvent::Received(ids) => {
                            for id in &ids {
                                self.tracker.mark_received(id);
                            }
                            self.advance_downloaded_height().await;
                        }
                        DeliveryEvent::Evicted(ids) => {
                            self.tracker.schedule_rerequest(&ids);
                        }
                        DeliveryEvent::NeedModifier { type_id, id } => {
                            tracing::info!(type_id, "pipeline needs modifier for reorg");
                            self.request_announced(peer, type_id, vec![id]).await;
                        }
                        DeliveryEvent::Reorg { fork_point, old_tip, new_tip } => {
                            tracing::info!(fork_point, old_tip, new_tip, "reorg: adjusting section queue and watermark");

                            // Clear section queue — entries are for the old branch
                            self.section_queue.clear();
                            self.sections_queued_to = fork_point;

                            // Reset watermarks if they were above the fork point
                            if self.downloaded_height > fork_point {
                                tracing::info!(
                                    old = self.downloaded_height,
                                    new = fork_point,
                                    "resetting downloaded_height to fork point"
                                );
                                self.downloaded_height = fork_point;
                            }
                            if self.validated_height > fork_point {
                                if let Some(fork_header) = self.chain.header_at(fork_point).await {
                                    self.validator.reset_to(fork_point, fork_header.state_root);
                                }
                                self.validated_height = fork_point;
                            }

                            // Re-queue sections for the new best chain
                            self.queue_sections_for_range(fork_point + 1, new_tip).await;

                            // Re-scan watermark — some sections may already be in store
                            self.advance_downloaded_height().await;
                        }
                    }
                }

                // Pipeline progress: send ONE SyncInfo per cycle for the two-batch pattern
                Some(height) = self.progress.recv() => {
                    self.last_progress = Instant::now();
                    self.stalled_peers.clear();

                    self.queue_new_sections(height).await;

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
        self.tracker.mark_requested(&ids, peer);
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

    /// Queue block sections for newly chained headers if above the watermark.
    async fn queue_new_sections(&mut self, height: u32) {
        if height > self.sections_queued_to {
            let from = self.sections_queued_to + 1;
            self.queue_sections_for_range(from, height).await;
        }
    }

    /// Advance the full block height watermark by scanning forward.
    ///
    /// For each height above the current watermark, checks whether all
    /// required block sections (per `config.state_type`) are in the store.
    /// Advances as far as possible in one call — stops at the first gap.
    async fn advance_downloaded_height(&mut self) {
        eprintln!(">>> advance_downloaded_height called: downloaded={} validated={}", self.downloaded_height, self.validated_height);
        let chain_height = self.chain.chain_height().await;
        if self.downloaded_height >= chain_height {
            tracing::warn!(
                downloaded_height = self.downloaded_height,
                chain_height,
                "download height scan: already at or above chain height"
            );
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
                    tracing::warn!(
                        height,
                        type_id,
                        id = hex::encode(id),
                        "section missing — stopping download height scan"
                    );
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
            tracing::info!(
                downloaded_height = new_height,
                advanced,
                chain_height,
                "downloaded height advanced"
            );
            self.advance_validated_height().await;
        }
    }

    /// Advance the validated height by running the block validator on
    /// downloaded-but-not-yet-validated blocks.
    async fn advance_validated_height(&mut self) {
        if self.validated_height >= self.downloaded_height {
            return;
        }

        let start = self.validated_height + 1;
        let mut validated_to = self.validated_height;

        for height in start..=self.downloaded_height {
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

            // Get preceding headers (up to 10) for future ErgoStateContext
            let preceding_start = height.saturating_sub(10).max(1);
            let mut preceding = Vec::new();
            for h in (preceding_start..height).rev() {
                if let Some(hdr) = self.chain.header_at(h).await {
                    preceding.push(hdr);
                }
            }

            let result = self.validator.validate_block(
                &header,
                &block_txs,
                ad_proofs.as_deref(),
                &extension,
                &preceding,
            );

            match result {
                Ok(()) => {
                    validated_to = height;
                }
                Err(e) => {
                    tracing::error!(height, error = %e, "block validation failed");
                    break;
                }
            }
        }

        if validated_to > self.validated_height {
            let advanced = validated_to - self.validated_height;
            self.validated_height = validated_to;
            tracing::info!(
                validated_height = validated_to,
                advanced,
                downloaded_height = self.downloaded_height,
                "validated height advanced"
            );
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
        current_peer: PeerId,
    ) {
        // Re-request timed-out modifiers from a different peer
        if !result.retries.is_empty() {
            let peers = self.transport.outbound_peers().await;
            for retry in &result.retries {
                // Pick a peer different from the one that failed
                let target = peers.iter()
                    .find(|&&p| p != retry.failed_peer)
                    .copied()
                    .unwrap_or(current_peer);

                self.tracker.mark_requested(&[retry.id], target);
                let _ = self.transport.send_to(
                    target,
                    ProtocolMessage::ModifierRequest {
                        modifier_type: HEADER_TYPE_ID,
                        ids: vec![retry.id],
                    },
                ).await;
            }
            tracing::debug!(count = result.retries.len(), "re-requested timed-out modifiers");
        }

        // Request evicted modifiers from any available peer
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

        // Drain section queue on entry
        self.drain_section_queue().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let peers = self.transport.outbound_peers().await;
                    if let Some(&peer) = peers.first() {
                        let _ = self.send_sync_info(peer).await;
                    } else {
                        tracing::info!("no outbound peers, returning to idle");
                        self.sync_peer = None;
                        return;
                    }

                    // Periodically drain any newly queued sections
                    self.drain_section_queue().await;
                    self.advance_downloaded_height().await;
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
                    self.queue_new_sections(height).await;
                }
            }
        }
    }

    /// Send batched ModifierRequests for queued block sections.
    async fn drain_section_queue(&mut self) {
        if self.section_queue.is_empty() {
            return;
        }

        let peers = self.transport.outbound_peers().await;
        if peers.is_empty() {
            return;
        }

        // Drain from the front, group by type. Queue is in height order so
        // front-drain preserves the natural request ordering.
        let mut by_type: HashMap<u8, Vec<[u8; 32]>> = HashMap::new();
        let mut to_drain = 0usize;
        for (type_id, id) in self.section_queue.iter() {
            let batch = by_type.entry(*type_id).or_default();
            if batch.len() < SECTION_REQUEST_BATCH_SIZE {
                batch.push(*id);
                to_drain += 1;
            }
            if by_type.values().all(|v| v.len() >= SECTION_REQUEST_BATCH_SIZE) {
                break;
            }
        }

        // Remove drained entries from front
        for _ in 0..to_drain {
            self.section_queue.pop_front();
        }

        // Send to all outbound peers — we don't know which has full blocks
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
            tracing::info!(sent, peer_count = peers.len(), remaining = self.section_queue.len(), "requested block sections");
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

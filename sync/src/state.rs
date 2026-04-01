use std::collections::HashSet;

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::delivery::{DeliveryEvent, DeliveryTracker};
use crate::traits::{SyncChain, SyncTransport};

/// Header modifier type ID (NetworkObjectTypeId).
const HEADER_TYPE_ID: u8 = 101;

/// Minimum time between scheduled SyncInfo sends to the same peer.
/// The JVM enforces a hard 20-second minimum per peer in
/// `ErgoSyncTracker.MinSyncInterval`.
const MIN_SYNC_INTERVAL: Duration = Duration::from_secs(20);

/// How long without pipeline progress before rotating to a new peer.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How often to send SyncInfo when synced, to check for new blocks.
const SYNCED_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How often to check for timed-out delivery requests (JVM: 5s check cycle).
const DELIVERY_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum interval between SyncInfo sends to the same peer.
/// The JVM drops SyncInfo received within 100ms of the previous one
/// (`PerPeerSyncLockTime`). We use 200ms for safety margin.
const MIN_SYNC_SEND_INTERVAL: Duration = Duration::from_millis(200);

/// Header chain sync state machine.
///
/// Event-driven loop matching the JVM's sync exchange pattern:
/// - Sends SyncInfo, receives Inv, sends ModifierRequest
/// - On pipeline progress: sends SyncInfo again (two-batch cycle)
/// - On peer SyncInfo: responds with our SyncInfo (bidirectional exchange)
/// - On stall: rotates to a different peer
pub struct HeaderSync<T: SyncTransport, C: SyncChain> {
    transport: T,
    chain: C,
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
}

impl<T: SyncTransport, C: SyncChain> HeaderSync<T, C> {
    pub fn new(
        transport: T,
        chain: C,
        progress: mpsc::Receiver<u32>,
        delivery_rx: mpsc::Receiver<DeliveryEvent>,
    ) -> Self {
        Self {
            transport,
            chain,
            progress,
            delivery_rx,
            tracker: DeliveryTracker::new(),
            sync_peer: None,
            stalled_peers: HashSet::new(),
            last_progress: Instant::now(),
            last_scheduled_sync: Instant::now(),
            last_sync_sent: Instant::now(),
            sync_sent_count: 0,
        }
    }

    /// Run the sync loop. Returns only if all event sources close.
    pub async fn run(&mut self) {
        tracing::info!("header sync started");
        loop {
            // Phase 1: wait for outbound peers
            if !self.pick_sync_peer().await {
                return; // event stream ended
            }

            // Phase 2: sync from the selected peer
            match self.sync_from_peer().await {
                SyncOutcome::Synced => self.synced().await,
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
            let until_next = MIN_SYNC_INTERVAL
                .saturating_sub(self.last_scheduled_sync.elapsed());
            let until_delivery = DELIVERY_CHECK_INTERVAL
                .saturating_sub(last_delivery_check.elapsed());

            tokio::select! {
                // P2P events: Inv, peer SyncInfo, disconnect
                event = self.transport.next_event() => {
                    match event {
                        Some(event) => {
                            match self.handle_event(peer, event).await {
                                EventResult::Continue => {}
                                EventResult::Synced => return SyncOutcome::Synced,
                                EventResult::PeerGone => {
                                    // Re-request any modifiers that were pending from this peer
                                    let orphaned = self.tracker.purge_peer(peer);
                                    if !orphaned.is_empty() {
                                        self.rerequest_from_any(&orphaned).await;
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
                        }
                        DeliveryEvent::Evicted(ids) => {
                            self.tracker.schedule_rerequest(&ids);
                        }
                    }
                }

                // Pipeline progress: send ONE SyncInfo per cycle for the two-batch pattern
                Some(height) = self.progress.recv() => {
                    self.last_progress = Instant::now();
                    self.stalled_peers.clear();

                    if !progress_send_used {
                        progress_send_used = true;
                        tracing::debug!(height, "progress → second batch SyncInfo");
                        let _ = self.send_sync_info(peer).await;
                    }
                }

                // Delivery check: re-request timed-out modifiers
                _ = tokio::time::sleep(until_delivery) => {
                    last_delivery_check = Instant::now();
                    let result = self.tracker.check_timeouts();
                    self.handle_delivery_check(result, peer).await;
                }

                // Scheduled SyncInfo: 20-second cycle start
                _ = tokio::time::sleep(until_next) => {
                    let _ = self.send_sync_info(peer).await;
                    self.last_scheduled_sync = Instant::now();
                    progress_send_used = false; // allow one progress send next cycle

                    if self.last_progress.elapsed() > STALL_TIMEOUT {
                        return SyncOutcome::Stalled;
                    }
                }
            }
        }
    }

    /// Handle a single P2P event during sync.
    async fn handle_event(&mut self, peer: PeerId, event: ProtocolEvent) -> EventResult {
        match event {
            ProtocolEvent::Message { peer_id, message } => match message {
                // Inv with header IDs: request them and track delivery
                ProtocolMessage::Inv { modifier_type, ids }
                    if modifier_type == HEADER_TYPE_ID && !ids.is_empty() =>
                {
                    tracing::debug!(
                        count = ids.len(),
                        sync_count = self.sync_sent_count,
                        "Inv received → requesting headers"
                    );
                    self.tracker.mark_requested(&ids, peer_id);
                    let _ = self
                        .transport
                        .send_to(
                            peer_id,
                            ProtocolMessage::ModifierRequest {
                                modifier_type,
                                ids,
                            },
                        )
                        .await;
                    EventResult::Continue
                }

                // Empty Inv: peer has no more headers
                ProtocolMessage::Inv { modifier_type, ids }
                    if modifier_type == HEADER_TYPE_ID && ids.is_empty() =>
                {
                    let height = self.chain.chain_height().await;
                    tracing::info!(height, "peer reports no more headers");
                    EventResult::Synced
                }

                // Peer's SyncInfo: respond with ours to keep the bidirectional
                // exchange alive. The JVM expects this — without it, per-peer
                // sync state goes stale and the JVM stops sending Inv.
                ProtocolMessage::SyncInfo { body } => {
                    if let Ok(info) = self.chain.parse_sync_info(&body) {
                        let peer_heights = C::sync_info_heights(&info);
                        let our_height = self.chain.chain_height().await;

                        if let Some(&peer_tip) = peer_heights.first() {
                            if peer_tip <= our_height {
                                tracing::info!(our_height, peer_tip, "caught up with peer");
                                return EventResult::Synced;
                            }
                        }
                    }

                    // Respond with our SyncInfo — mirrors JVM's syncSendNeeded behavior
                    let _ = self.send_sync_info(peer).await;
                    EventResult::Continue
                }

                _ => EventResult::Continue,
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
            self.rerequest_from_any(&result.fresh).await;
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
    async fn rerequest_from_any(&mut self, ids: &[[u8; 32]]) {
        let peers = self.transport.outbound_peers().await;
        if let Some(&target) = peers.first() {
            self.tracker.mark_requested(ids, target);
            let _ = self.transport.send_to(
                target,
                ProtocolMessage::ModifierRequest {
                    modifier_type: HEADER_TYPE_ID,
                    ids: ids.to_vec(),
                },
            ).await;
            tracing::debug!(count = ids.len(), peer = %target, "re-requested modifiers");
        }
    }

    /// Synced: periodically check for new blocks.
    async fn synced(&mut self) {
        let mut ticker = tokio::time::interval(SYNCED_POLL_INTERVAL);

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
                }

                event = self.transport.next_event() => {
                    match event {
                        Some(event) => {
                            if let ProtocolEvent::Message { peer_id, message: ProtocolMessage::Inv {
                                modifier_type, ids
                            }} = event {
                                if modifier_type == HEADER_TYPE_ID && !ids.is_empty() {
                                    tracing::debug!(new_headers = ids.len(), "new headers while synced");
                                    self.tracker.mark_requested(&ids, peer_id);
                                    let _ = self.transport.send_to(
                                        peer_id,
                                        ProtocolMessage::ModifierRequest { modifier_type, ids },
                                    ).await;
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
                    // New headers validated — if we're still behind, go back to syncing
                    // (will be detected on next SyncInfo exchange)
                }
            }
        }
    }

    /// Send our current SyncInfo to a peer, respecting the JVM's PerPeerSyncLockTime.
    /// Returns Ok(()) even if rate-limited (skipped sends are not errors).
    async fn send_sync_info(
        &mut self,
        peer: PeerId,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        // Rate-limit: JVM drops SyncInfo received within 100ms of previous
        if self.last_sync_sent.elapsed() < MIN_SYNC_SEND_INTERVAL {
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
    /// Sync peer disconnected.
    PeerGone,
}

/// Outcome of a sync_from_peer session.
enum SyncOutcome {
    /// Caught up with the peer's chain tip.
    Synced,
    /// No progress for STALL_TIMEOUT — rotate peer.
    Stalled,
    /// Sync peer disconnected.
    PeerDisconnected,
    /// Event stream closed (shutdown).
    StreamEnded,
}

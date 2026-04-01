use std::collections::HashSet;

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

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
    /// Peer we're currently syncing from.
    sync_peer: Option<PeerId>,
    /// Peers that failed to produce progress — skipped during peer selection.
    stalled_peers: HashSet<PeerId>,
    /// When the last chain height increase was observed.
    last_progress: Instant,
    /// When we last sent a scheduled SyncInfo (20s floor applies to these).
    last_scheduled_sync: Instant,
    /// Total SyncInfo messages sent this session (diagnostics).
    sync_sent_count: u32,
}

impl<T: SyncTransport, C: SyncChain> HeaderSync<T, C> {
    pub fn new(transport: T, chain: C, progress: mpsc::Receiver<u32>) -> Self {
        Self {
            transport,
            chain,
            progress,
            sync_peer: None,
            stalled_peers: HashSet::new(),
            last_progress: Instant::now(),
            last_scheduled_sync: Instant::now(),
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

        loop {
            // Compute time until next scheduled SyncInfo
            let until_next = MIN_SYNC_INTERVAL
                .saturating_sub(self.last_scheduled_sync.elapsed());

            tokio::select! {
                // P2P events: Inv, peer SyncInfo, disconnect
                event = self.transport.next_event() => {
                    match event {
                        Some(event) => {
                            match self.handle_event(peer, event).await {
                                EventResult::Continue => {}
                                EventResult::Synced => return SyncOutcome::Synced,
                                EventResult::PeerGone => return SyncOutcome::PeerDisconnected,
                            }
                        }
                        None => return SyncOutcome::StreamEnded,
                    }
                }

                // Pipeline progress: chain height increased → two-batch trigger
                Some(height) = self.progress.recv() => {
                    tracing::debug!(height, "pipeline progress");
                    self.last_progress = Instant::now();
                    self.stalled_peers.clear();

                    // Send SyncInfo with updated chain state (second batch)
                    let _ = self.send_sync_info(peer).await;
                }

                // Scheduled SyncInfo: 20-second floor
                _ = tokio::time::sleep(until_next) => {
                    let _ = self.send_sync_info(peer).await;
                    self.last_scheduled_sync = Instant::now();

                    // Check for stall
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
                // Inv with header IDs: request them
                ProtocolMessage::Inv { modifier_type, ids }
                    if modifier_type == HEADER_TYPE_ID && !ids.is_empty() =>
                {
                    tracing::debug!(count = ids.len(), "requesting announced headers");
                    let _ = self
                        .transport
                        .send_to(
                            peer_id,
                            ProtocolMessage::ModifierRequest { modifier_type, ids },
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

                // Peer's SyncInfo: respond with ours (bidirectional exchange)
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

                    // Respond with our SyncInfo (JVM does this when syncSendNeeded)
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

    /// Send our current SyncInfo to a peer.
    async fn send_sync_info(
        &mut self,
        peer: PeerId,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        let body = self.chain.build_sync_info().await;
        self.sync_sent_count += 1;
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

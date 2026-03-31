use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::time::{interval, Duration, Instant};

use crate::traits::{SyncChain, SyncTransport};

/// Header modifier type ID (NetworkObjectTypeId).
const HEADER_TYPE_ID: u8 = 101;

/// How often to send SyncInfo when synced, to check for new blocks.
const SYNCED_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How long to wait for progress before considering sync stalled.
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait between SyncInfo rounds during active sync.
const SYNC_ROUND_INTERVAL: Duration = Duration::from_secs(3);

/// Sync state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for outbound peers.
    Idle,
    /// Actively syncing headers from a peer.
    Syncing,
    /// Caught up with the network.
    Synced,
}

/// Header chain sync state machine.
///
/// Drives the P2P layer to request headers by sending SyncInfo to peers.
/// The P2P router handles the Inv→ModifierRequest→ModifierResponse relay.
/// The sync machine observes chain height progress and sends more SyncInfo
/// when progress stalls.
pub struct HeaderSync<T: SyncTransport, C: SyncChain> {
    transport: T,
    chain: C,
    state: State,
    /// Peer we're currently syncing from.
    sync_peer: Option<PeerId>,
    /// Chain height at the start of the current sync round.
    round_start_height: u32,
    /// When the last progress was observed.
    last_progress: Instant,
}

impl<T: SyncTransport, C: SyncChain> HeaderSync<T, C> {
    pub fn new(transport: T, chain: C) -> Self {
        Self {
            transport,
            chain,
            state: State::Idle,
            sync_peer: None,
            round_start_height: 0,
            last_progress: Instant::now(),
        }
    }

    /// Run the sync loop. Does not return unless the event stream ends.
    pub async fn run(&mut self) {
        tracing::info!("header sync started");
        loop {
            match self.state {
                State::Idle => self.idle().await,
                State::Syncing => self.syncing().await,
                State::Synced => self.synced().await,
            }
        }
    }

    /// Idle: drain events until we have outbound peers, then start syncing.
    async fn idle(&mut self) {
        loop {
            let peers = self.transport.outbound_peers().await;
            if !peers.is_empty() {
                let peer = peers[0];
                self.sync_peer = Some(peer);
                self.round_start_height = self.chain.chain_height().await;
                self.last_progress = Instant::now();

                tracing::info!(peer = %peer, height = self.round_start_height, "starting header sync");

                if self.send_sync_info(peer).await.is_ok() {
                    self.state = State::Syncing;
                    return;
                }
                self.sync_peer = None;
            }

            // Wait for an event (peer connect, etc.)
            match self.transport.next_event().await {
                Some(_) => {} // loop back and check peers again
                None => return, // stream ended
            }
        }
    }

    /// Syncing: process events, send SyncInfo rounds, track progress.
    async fn syncing(&mut self) {
        let mut round_timer = interval(SYNC_ROUND_INTERVAL);
        round_timer.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                event = self.transport.next_event() => {
                    match event {
                        Some(event) => self.handle_event(event).await,
                        None => {
                            self.state = State::Idle;
                            return;
                        }
                    }
                }
                _ = round_timer.tick() => {
                    let height = self.chain.chain_height().await;

                    if height > self.round_start_height {
                        // Made progress — start a new round
                        tracing::debug!(height, prev = self.round_start_height, "sync progress");
                        self.round_start_height = height;
                        self.last_progress = Instant::now();

                        if let Some(peer) = self.sync_peer {
                            let _ = self.send_sync_info(peer).await;
                        }
                    } else if self.last_progress.elapsed() > SYNC_TIMEOUT {
                        // No progress for too long — stalled
                        tracing::warn!(peer = ?self.sync_peer, height, "sync stalled");
                        self.sync_peer = None;
                        self.state = State::Idle;
                        return;
                    }
                }
            }
        }
    }

    /// Handle an event during syncing.
    async fn handle_event(&mut self, event: ProtocolEvent) {
        match event {
            ProtocolEvent::Message { message, .. } => {
                match message {
                    ProtocolMessage::Inv { modifier_type, ids }
                        if modifier_type == HEADER_TYPE_ID && ids.is_empty() =>
                    {
                        // Peer has nothing more — check if we're caught up
                        let height = self.chain.chain_height().await;
                        tracing::info!(height, "peer reports no more headers");
                        self.state = State::Synced;
                    }
                    ProtocolMessage::SyncInfo { body } => {
                        // Incoming SyncInfo — check if peer is ahead
                        if let Ok(info) = self.chain.parse_sync_info(&body) {
                            let peer_heights = C::sync_info_heights(&info);
                            let our_height = self.chain.chain_height().await;
                            if let Some(&peer_tip) = peer_heights.first() {
                                if peer_tip <= our_height && self.state == State::Syncing {
                                    tracing::info!(our_height, peer_tip, "caught up with peer");
                                    self.state = State::Synced;
                                }
                            }
                        }
                    }
                    _ => {} // router handles Inv routing, ModifierRequest/Response
                }
            }
            ProtocolEvent::PeerDisconnected { peer_id, .. } => {
                if self.sync_peer == Some(peer_id) {
                    tracing::info!(peer = %peer_id, "sync peer disconnected");
                    self.sync_peer = None;
                    self.state = State::Idle;
                }
            }
            _ => {}
        }
    }

    /// Synced: periodically check for new blocks.
    async fn synced(&mut self) {
        let mut ticker = interval(SYNCED_POLL_INTERVAL);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let peers = self.transport.outbound_peers().await;
                    if let Some(&peer) = peers.first() {
                        let _ = self.send_sync_info(peer).await;
                    } else {
                        tracing::info!("no outbound peers, returning to idle");
                        self.state = State::Idle;
                        return;
                    }
                }
                event = self.transport.next_event() => {
                    match event {
                        Some(event) => {
                            // Check if we're falling behind
                            if let ProtocolEvent::Message { message: ProtocolMessage::Inv {
                                modifier_type, ids
                            }, .. } = &event {
                                if *modifier_type == HEADER_TYPE_ID && !ids.is_empty() {
                                    let height = self.chain.chain_height().await;
                                    tracing::debug!(height, new_headers = ids.len(), "new headers while synced");
                                    // Router handles requesting them via normal relay flow
                                }
                            }
                        }
                        None => {
                            self.state = State::Idle;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Send our current SyncInfo to a peer.
    async fn send_sync_info(
        &self,
        peer: PeerId,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        let body = self.chain.build_sync_info().await;
        self.transport
            .send_to(peer, ProtocolMessage::SyncInfo { body })
            .await
    }
}

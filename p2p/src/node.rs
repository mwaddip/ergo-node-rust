//! P2P node entry point.
//!
//! The caller provides a config and an optional modifier sink, then calls
//! `P2pNode::start()`. The P2P layer spawns listeners, outbound connections,
//! and the event loop as background tokio tasks. The returned `P2pNode` is a
//! handle for observing and controlling state — the caller owns the tokio runtime.

use crate::config::Config;
use crate::protocol::messages::ProtocolMessage;
use crate::protocol::peer::ProtocolEvent;
use crate::routing::router::{Action, Router};
use crate::routing::latency::LatencyStats;
use crate::transport::connection::Connection;
use crate::transport::frame::Frame;
use crate::transport::handshake::{self, HandshakeConfig};
use crate::types::{Direction, PeerId, ProxyMode, Version};
use crate::upnp::UpnpMapping;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};

type PeerSender = mpsc::Sender<Frame>;

/// Error when sending a message to a peer.
#[derive(Debug)]
pub enum SendError {
    /// The peer ID is not connected.
    UnknownPeer(PeerId),
    /// The peer's write channel is closed (peer disconnecting).
    ChannelClosed(PeerId),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::UnknownPeer(pid) => write!(f, "unknown peer: {}", pid),
            SendError::ChannelClosed(pid) => write!(f, "channel closed for peer: {}", pid),
        }
    }
}

impl std::error::Error for SendError {}

/// Handle to a running P2P node.
///
/// Created by `P2pNode::start()`. Provides observation and control of the
/// node's state. The P2P layer runs as background tokio tasks — dropping
/// this handle does not stop them. The tasks live until the tokio runtime
/// shuts down.
pub struct P2pNode {
    router: Arc<Mutex<Router>>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>>,
    upnp_mapping: Option<UpnpMapping>,
}

impl P2pNode {
    /// Start the P2P layer.
    ///
    /// Loads config, sets up listeners, outbound connections, keepalive, and
    /// the event loop as background tokio tasks. Returns immediately.
    ///
    /// # Contract
    /// - **Precondition**: Called within a tokio runtime.
    /// - **Precondition**: `config` has at least one listener and one seed peer
    ///   (enforced by `Config::load()`).
    /// - **Postcondition**: Background tasks are spawned and running.
    pub async fn start(
        config: Config,
        modifier_sink: Option<mpsc::Sender<(u8, [u8; 32], Vec<u8>, Option<u64>)>>,
        mode_config: handshake::ModeConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (ver_major, ver_minor, ver_patch) = config.version_bytes()?;
        let version = Version::new(ver_major, ver_minor, ver_patch);
        let network = config.proxy.network;

        tracing::info!(network = ?network, version = %version, "P2P layer starting");

        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(256);
        let peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let router = Arc::new(Mutex::new(Router::new()));
        let peer_counter = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>> =
            Arc::new(Mutex::new(None));

        // Discover external addresses before starting listeners.
        // UPnP for IPv4 (NAT traversal), interface enumeration for IPv6 (globally routable).
        let mut upnp_mapping: Option<UpnpMapping> = None;
        let mut ipv4_declared: Option<SocketAddr> = None;
        let mut ipv6_declared: Option<SocketAddr> = None;

        if config.upnp.enabled {
            if let Some(ref listener_cfg) = config.listen.ipv4 {
                if let Some(mapping) = crate::upnp::attempt(
                    &config.upnp,
                    listener_cfg.address.port(),
                    listener_cfg.address,
                ).await {
                    ipv4_declared = Some(mapping.external_addr);
                    upnp_mapping = Some(mapping);
                }
            }
        }

        if let Some(ref listener_cfg) = config.listen.ipv6 {
            ipv6_declared = crate::netif::find_global_ipv6(listener_cfg.address.port());
        }

        // Start listeners
        if let Some(ref listener_cfg) = config.listen.ipv6 {
            let listener = TcpListener::bind(listener_cfg.address).await?;
            tracing::info!(addr = %listener_cfg.address, mode = ?listener_cfg.mode, declared = ?ipv6_declared, "IPv6 listener started");
            let hs_config = make_handshake_config(&config.identity, version, network, listener_cfg.mode, mode_config, ipv6_declared);
            tokio::spawn(accept_loop(
                listener, hs_config, listener_cfg.mode, listener_cfg.max_inbound,
                event_tx.clone(), peer_senders.clone(), router.clone(), peer_counter.clone(),
            ));
        }

        if let Some(ref listener_cfg) = config.listen.ipv4 {
            let listener = TcpListener::bind(listener_cfg.address).await?;
            tracing::info!(addr = %listener_cfg.address, mode = ?listener_cfg.mode, declared = ?ipv4_declared, "IPv4 listener started");
            let hs_config = make_handshake_config(&config.identity, version, network, listener_cfg.mode, mode_config, ipv4_declared);
            tokio::spawn(accept_loop(
                listener, hs_config, listener_cfg.mode, listener_cfg.max_inbound,
                event_tx.clone(), peer_senders.clone(), router.clone(), peer_counter.clone(),
            ));
        }

        // Start outbound connections — prefer IPv4 declared address (most peers are IPv4)
        let outbound_declared = ipv4_declared.or(ipv6_declared);
        {
            let hs_config = make_handshake_config(&config.identity, version, network, ProxyMode::Full, mode_config, outbound_declared);
            tokio::spawn(outbound_manager(
                config.outbound.seed_peers.clone(), config.outbound.min_peers,
                hs_config, ProxyMode::Full,
                event_tx.clone(), peer_senders.clone(), router.clone(), peer_counter.clone(),
            ));
        }

        // Keepalive: send GetPeers every 2 minutes
        {
            let router = router.clone();
            let peer_senders = peer_senders.clone();
            tokio::spawn(async move {
                let mut ticker = interval(Duration::from_secs(120));
                loop {
                    ticker.tick().await;
                    let outbound = router.lock().await.outbound_peers();
                    let senders = peer_senders.lock().await;
                    let frame = ProtocolMessage::GetPeers.to_frame();
                    for pid in outbound {
                        if let Some(tx) = senders.get(&pid) {
                            let _ = tx.send(frame.clone()).await;
                        }
                    }
                }
            });
        }

        // Event loop: process protocol events through the router
        {
            let router = router.clone();
            let peer_senders = peer_senders.clone();
            let subscriber = subscriber.clone();
            tokio::spawn(async move {
                event_loop(event_rx, router, peer_senders, subscriber, modifier_sink).await;
            });
        }

        Ok(P2pNode { router, peer_senders, subscriber, upnp_mapping })
    }

    /// Number of connected peers (inbound + outbound).
    pub async fn peer_count(&self) -> usize {
        self.router.lock().await.peer_count()
    }

    /// Currently connected outbound peer IDs.
    pub async fn outbound_peers(&self) -> Vec<PeerId> {
        self.router.lock().await.outbound_peers()
    }

    /// Currently connected inbound peer IDs.
    pub async fn inbound_peers(&self) -> Vec<PeerId> {
        self.router.lock().await.inbound_peers()
    }

    /// Aggregate latency statistics across all tracked peers.
    pub async fn latency_stats(&self) -> Option<LatencyStats> {
        self.router.lock().await.latency_stats()
    }

    /// Send a protocol message to a specific peer.
    ///
    /// # Contract
    /// - **Precondition**: `peer` is a currently connected peer.
    /// - **Postcondition**: The message is queued on the peer's write channel.
    /// - Returns `SendError::UnknownPeer` if the peer is not connected.
    /// - Returns `SendError::ChannelClosed` if the peer's write channel has closed.
    pub async fn send_to(&self, peer: PeerId, message: ProtocolMessage) -> Result<(), SendError> {
        let senders = self.peer_senders.lock().await;
        let tx = senders.get(&peer).ok_or(SendError::UnknownPeer(peer))?;
        let frame = message.to_frame();
        tx.send(frame).await.map_err(|_| SendError::ChannelClosed(peer))
    }

    /// Send a protocol message to all connected outbound peers.
    ///
    /// Best-effort: silently skips peers whose write channels are full or closed.
    pub async fn broadcast_outbound(&self, message: ProtocolMessage) {
        let outbound = self.router.lock().await.outbound_peers();
        let senders = self.peer_senders.lock().await;
        let frame = message.to_frame();
        for pid in outbound {
            if let Some(tx) = senders.get(&pid) {
                let _ = tx.try_send(frame.clone());
            }
        }
    }

    /// Subscribe to protocol events.
    ///
    /// Returns a receiver that gets a copy of every `ProtocolEvent` before it
    /// reaches the router. If the subscriber falls behind (channel capacity 256),
    /// events are dropped rather than blocking the event loop.
    ///
    /// Only one subscriber at a time — calling this again replaces the previous one.
    pub async fn subscribe(&self) -> mpsc::Receiver<ProtocolEvent> {
        let (tx, rx) = mpsc::channel(256);
        *self.subscriber.lock().await = Some(tx);
        rx
    }

    /// Look up the socket address of a connected peer.
    pub async fn peer_addr(&self, peer_id: PeerId) -> Option<SocketAddr> {
        self.router.lock().await.peer_addr(peer_id)
    }

    /// REST API URLs advertised by connected peers.
    pub async fn peer_rest_urls(&self) -> Vec<(PeerId, SocketAddr, Option<String>)> {
        self.router.lock().await.peer_rest_urls()
    }

    /// Force-disconnect a peer by dropping its write channel.
    pub async fn disconnect_peer(&self, peer_id: PeerId) {
        self.peer_senders.lock().await.remove(&peer_id);
    }

    /// Register a message code as consumed by the caller's event stream.
    /// Unknown messages with this code will not be forwarded to peers.
    pub async fn register_consumed_code(&self, code: u8) {
        self.router.lock().await.register_consumed_code(code);
    }

    /// Returns the UPnP-discovered external address, if any.
    pub fn upnp_external_addr(&self) -> Option<SocketAddr> {
        self.upnp_mapping.as_ref().map(|m| m.external_addr)
    }

    /// Remove the UPnP port mapping. Call during graceful shutdown.
    pub async fn shutdown_upnp(&self) {
        if let Some(ref mapping) = self.upnp_mapping {
            mapping.remove().await;
        }
    }
}

async fn event_loop(
    mut event_rx: mpsc::Receiver<ProtocolEvent>,
    router: Arc<Mutex<Router>>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>>,
    modifier_sink: Option<mpsc::Sender<(u8, [u8; 32], Vec<u8>, Option<u64>)>>,
) {
    loop {
        match event_rx.recv().await {
            Some(event) => {
                // Tap: send to subscriber before routing (non-blocking).
                // Filter out ModifierResponse — it has its own path via modifier_sink
                // and would otherwise flood the bounded subscriber channel, causing
                // Inv and SyncInfo events to be silently dropped during heavy sync.
                {
                    let dominated_by_modifier_response = matches!(
                        &event,
                        ProtocolEvent::Message { message: ProtocolMessage::ModifierResponse { .. }, .. }
                    );
                    if !dominated_by_modifier_response {
                        let sub = subscriber.lock().await;
                        if let Some(tx) = sub.as_ref() {
                            let _ = tx.try_send(event.clone());
                        }
                    }
                }

                let actions = router.lock().await.handle_event(event);
                let senders = peer_senders.lock().await;
                for action in actions {
                    match action {
                        Action::Send { target, message } => {
                            if let Some(tx) = senders.get(&target) {
                                let frame = message.to_frame();
                                if tx.send(frame).await.is_err() {
                                    tracing::warn!(peer = %target, "Failed to send to peer");
                                }
                            }
                        }
                        Action::Validate { modifier_type, id, data, peer_id } => {
                            if let Some(ref sink) = modifier_sink {
                                if modifier_type != 101 {
                                    tracing::debug!(modifier_type, data_len = data.len(), "delivering non-header to pipeline");
                                }
                                let _ = sink.try_send((modifier_type, id, data, Some(peer_id.0)));
                            }
                        }
                    }
                }
            }
            None => {
                tracing::info!("All event senders dropped, event loop exiting");
                break;
            }
        }
    }
}

async fn run_peer(
    peer_id: PeerId,
    conn: Connection,
    direction: Direction,
    mode: ProxyMode,
    addr: SocketAddr,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
) {
    let spec = conn.peer_spec().clone();
    tracing::info!(
        peer = %peer_id,
        name = %spec.name,
        agent = %spec.agent,
        version = %spec.version,
        direction = ?direction,
        "Peer active"
    );

    // Register peer in router
    let rest_api_url = spec.rest_api_url();
    router.lock().await.register_peer(peer_id, direction, mode, addr, rest_api_url);

    // Send PeerConnected event
    let _ = event_tx.send(ProtocolEvent::PeerConnected {
        peer_id,
        spec: spec.clone(),
        direction,
        addr,
    }).await;

    // Split connection for concurrent read/write
    let (mut reader, mut writer, magic, _, _) = conn.split();

    // Create write channel
    let (write_tx, mut write_rx) = mpsc::channel::<Frame>(64);
    peer_senders.lock().await.insert(peer_id, write_tx);

    // Writer task
    let write_handle = tokio::spawn(async move {
        while let Some(frame) = write_rx.recv().await {
            if let Err(e) = crate::transport::frame::write_frame(&mut writer, &magic, &frame).await {
                tracing::warn!(peer = %peer_id, error = %e, "Write failed");
                break;
            }
        }
    });

    // Reader loop
    loop {
        match crate::transport::frame::read_frame(&mut reader, &magic, addr).await {
            Ok(frame) => {
                match ProtocolMessage::from_frame(&frame) {
                    Ok(msg) => {
                        let event = ProtocolEvent::Message { peer_id, message: msg };
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("PENALTY peer_ip={} type=misbehavior reason=\"message parse failed: {}\"", addr.ip(), e);
                    }
                }
            }
            Err(e) => {
                tracing::info!(peer = %peer_id, error = %e, "Connection lost");
                break;
            }
        }
    }

    // Cleanup
    peer_senders.lock().await.remove(&peer_id);
    write_handle.abort();

    let _ = event_tx.send(ProtocolEvent::PeerDisconnected {
        peer_id,
        reason: "connection closed".into(),
    }).await;

    tracing::info!(peer = %peer_id, "Peer removed");
}

fn make_handshake_config(
    identity: &crate::config::IdentityConfig,
    version: Version,
    network: crate::types::Network,
    mode: ProxyMode,
    mode_config: handshake::ModeConfig,
    declared_address: Option<SocketAddr>,
) -> HandshakeConfig {
    HandshakeConfig {
        agent_name: identity.agent_name.clone(),
        peer_name: identity.peer_name.clone(),
        version,
        network,
        mode,
        declared_address,
        mode_config,
    }
}

async fn accept_loop(
    listener: TcpListener,
    hs_config: HandshakeConfig,
    mode: ProxyMode,
    max_inbound: usize,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
    peer_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let remote_ip = addr.ip();
                let inbound_count = router.lock().await.inbound_peers().len();
                if inbound_count >= max_inbound {
                    tracing::warn!("PENALTY peer_ip={} type=misbehavior reason=\"connection limit exceeded\"", remote_ip);
                    continue;
                }

                let peer_id = PeerId(peer_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                tracing::info!(peer = %peer_id, ip = %remote_ip, "Inbound connection");

                let hs = HandshakeConfig {
                    agent_name: hs_config.agent_name.clone(),
                    peer_name: hs_config.peer_name.clone(),
                    version: hs_config.version,
                    network: hs_config.network,
                    mode: hs_config.mode,
                    declared_address: hs_config.declared_address,
                    mode_config: hs_config.mode_config,
                };
                let event_tx = event_tx.clone();
                let peer_senders = peer_senders.clone();
                let router = router.clone();

                tokio::spawn(async move {
                    match Connection::inbound(stream, &hs).await {
                        Ok(conn) => {
                            run_peer(peer_id, conn, Direction::Inbound, mode, addr, event_tx, peer_senders, router).await;
                        }
                        Err(e) => {
                            tracing::warn!("PENALTY peer_ip={} type=permanent reason=\"handshake failed: {}\"", addr.ip(), e);
                        }
                    }
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "Accept failed");
            }
        }
    }
}

async fn outbound_manager(
    seeds: Vec<std::net::SocketAddr>,
    min_peers: usize,
    hs_config: HandshakeConfig,
    mode: ProxyMode,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
    peer_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut backoff = Duration::from_secs(5);

    loop {
        let current_outbound = router.lock().await.outbound_peers().len();
        if current_outbound < min_peers {
            for addr in &seeds {
                let current = router.lock().await.outbound_peers().len();
                if current >= min_peers {
                    break;
                }

                let addr = *addr;
                tracing::info!(addr = %addr, "Connecting to outbound peer");
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(addr),
                ).await {
                    Ok(Ok(stream)) => {
                        let peer_id = PeerId(peer_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                        let hs = HandshakeConfig {
                            agent_name: hs_config.agent_name.clone(),
                            peer_name: hs_config.peer_name.clone(),
                            version: hs_config.version,
                            network: hs_config.network,
                            mode: hs_config.mode,
                            declared_address: hs_config.declared_address,
                            mode_config: hs_config.mode_config,
                        };
                        let event_tx = event_tx.clone();
                        let peer_senders = peer_senders.clone();
                        let router = router.clone();

                        tokio::spawn(async move {
                            match Connection::outbound(stream, &hs).await {
                                Ok(conn) => {
                                    if handshake::is_proxy(conn.peer_spec()) {
                                        tracing::info!(peer = %peer_id, addr = %addr, "Outbound peer is a proxy, skipping");
                                        return;
                                    }
                                    tracing::info!(peer = %peer_id, "Outbound handshake OK");
                                    run_peer(peer_id, conn, Direction::Outbound, mode, addr, event_tx, peer_senders, router).await;
                                }
                                Err(e) => {
                                    tracing::warn!(peer = %peer_id, addr = %addr, error = %e, "Outbound handshake failed");
                                }
                            }
                        });

                        backoff = Duration::from_secs(5);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(addr = %addr, error = %e, "Connect failed");
                    }
                    Err(_) => {
                        tracing::warn!(addr = %addr, "Connect timeout");
                    }
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_addr() -> SocketAddr {
        "127.0.0.1:9000".parse().unwrap()
    }

    /// Build a P2pNode with no background tasks — just the struct with shared state.
    fn test_node() -> (P2pNode, Arc<Mutex<Router>>, Arc<Mutex<HashMap<PeerId, PeerSender>>>) {
        let router = Arc::new(Mutex::new(Router::new()));
        let peer_senders = Arc::new(Mutex::new(HashMap::new()));
        let subscriber = Arc::new(Mutex::new(None));
        let node = P2pNode {
            router: router.clone(),
            peer_senders: peer_senders.clone(),
            subscriber,
            upnp_mapping: None,
        };
        (node, router, peer_senders)
    }

    #[tokio::test]
    async fn send_to_delivers_to_correct_peer() {
        let (node, _router, peer_senders) = test_node();
        let peer = PeerId(1);

        let (tx, mut rx) = mpsc::channel::<Frame>(64);
        peer_senders.lock().await.insert(peer, tx);

        let msg = ProtocolMessage::GetPeers;
        node.send_to(peer, msg).await.unwrap();

        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.code, 1); // GetPeers code
    }

    #[tokio::test]
    async fn send_to_unknown_peer_returns_error() {
        let (node, _router, _senders) = test_node();
        let result = node.send_to(PeerId(999), ProtocolMessage::GetPeers).await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SendError::UnknownPeer(PeerId(999))));
    }

    #[tokio::test]
    async fn send_to_closed_channel_returns_error() {
        let (node, _router, peer_senders) = test_node();
        let peer = PeerId(1);

        let (tx, rx) = mpsc::channel::<Frame>(64);
        peer_senders.lock().await.insert(peer, tx);
        drop(rx); // Close the receiving end

        let result = node.send_to(peer, ProtocolMessage::GetPeers).await;
        assert!(matches!(result.unwrap_err(), SendError::ChannelClosed(PeerId(1))));
    }

    #[tokio::test]
    async fn broadcast_outbound_sends_to_all_outbound_peers() {
        let (node, router, peer_senders) = test_node();

        let peer_a = PeerId(1);
        let peer_b = PeerId(2);
        let peer_inbound = PeerId(3);

        router.lock().await.register_peer(peer_a, Direction::Outbound, ProxyMode::Full, dummy_addr(), None);
        router.lock().await.register_peer(peer_b, Direction::Outbound, ProxyMode::Full, dummy_addr(), None);
        router.lock().await.register_peer(peer_inbound, Direction::Inbound, ProxyMode::Full, dummy_addr(), None);

        let (tx_a, mut rx_a) = mpsc::channel::<Frame>(64);
        let (tx_b, mut rx_b) = mpsc::channel::<Frame>(64);
        let (tx_in, mut rx_in) = mpsc::channel::<Frame>(64);
        {
            let mut senders = peer_senders.lock().await;
            senders.insert(peer_a, tx_a);
            senders.insert(peer_b, tx_b);
            senders.insert(peer_inbound, tx_in);
        }

        node.broadcast_outbound(ProtocolMessage::GetPeers).await;

        // Outbound peers should receive the message
        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_ok());
        // Inbound peer should NOT
        assert!(rx_in.try_recv().is_err());
    }

    #[tokio::test]
    async fn broadcast_outbound_skips_full_channel() {
        let (node, router, peer_senders) = test_node();

        let peer_ok = PeerId(1);
        let peer_full = PeerId(2);

        router.lock().await.register_peer(peer_ok, Direction::Outbound, ProxyMode::Full, dummy_addr(), None);
        router.lock().await.register_peer(peer_full, Direction::Outbound, ProxyMode::Full, dummy_addr(), None);

        let (tx_ok, mut rx_ok) = mpsc::channel::<Frame>(64);
        // Capacity 1: one fill + one broadcast = second is dropped
        let (tx_full, rx_full) = mpsc::channel::<Frame>(1);
        {
            let mut senders = peer_senders.lock().await;
            senders.insert(peer_ok, tx_ok);
            senders.insert(peer_full, tx_full);
        }

        // Fill the small channel
        node.broadcast_outbound(ProtocolMessage::GetPeers).await;
        // Channel is now at capacity — second broadcast should skip it, not block
        node.broadcast_outbound(ProtocolMessage::GetPeers).await;

        // Healthy peer got both
        assert!(rx_ok.try_recv().is_ok());
        assert!(rx_ok.try_recv().is_ok());

        // Full peer got only the first (second was dropped, not blocked)
        drop(rx_full);
    }

    #[tokio::test]
    async fn subscriber_receives_events() {
        let router = Arc::new(Mutex::new(Router::new()));
        let peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>> =
            Arc::new(Mutex::new(None));

        let node = P2pNode {
            router: router.clone(),
            peer_senders: peer_senders.clone(),
            subscriber: subscriber.clone(),
            upnp_mapping: None,
        };

        let mut events = node.subscribe().await;

        // Drive the event loop with a one-shot channel
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);

        let r = router.clone();
        let ps = peer_senders.clone();
        let sub = subscriber.clone();
        let handle = tokio::spawn(async move {
            event_loop(event_rx, r, ps, sub, None).await;
        });

        // Register a peer so the router doesn't choke
        router.lock().await.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None);

        // Send a protocol event
        event_tx.send(ProtocolEvent::Message {
            peer_id: PeerId(1),
            message: ProtocolMessage::GetPeers,
        }).await.unwrap();

        // Subscriber should see it
        let event = events.recv().await.unwrap();
        assert!(matches!(event, ProtocolEvent::Message { peer_id: PeerId(1), .. }));

        // Cleanup
        drop(event_tx);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn subscriber_drops_events_when_full() {
        let subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>> =
            Arc::new(Mutex::new(None));

        // Create a subscriber with capacity 2
        let (tx, rx) = mpsc::channel(2);
        *subscriber.lock().await = Some(tx);

        // Simulate what the event loop does: try_send
        let sub = subscriber.lock().await;
        let tx = sub.as_ref().unwrap();

        let event = ProtocolEvent::PeerDisconnected {
            peer_id: PeerId(1),
            reason: "test".into(),
        };

        // Fill the channel
        assert!(tx.try_send(event.clone()).is_ok());
        assert!(tx.try_send(event.clone()).is_ok());
        // Third should fail (channel full), not block
        assert!(tx.try_send(event).is_err());

        // Events didn't block, and rx still works
        drop(sub);
        drop(rx);
    }
}

//! P2P node entry point.
//!
//! The caller provides a config and an optional modifier sink, then calls
//! `P2pNode::start()`. The P2P layer spawns listeners, outbound connections,
//! and the event loop as background tokio tasks. The returned `P2pNode` is a
//! handle for observing and controlling state — the caller owns the tokio runtime.

use crate::blacklist::Blacklist;
use crate::config::Config;
use crate::peer_db::{PeerDb, PeerStorage, DEFAULT_CAP};
use crate::protocol::address_sanity::is_bogus_address;
use crate::protocol::counters::{self, TrafficCounters, TrafficSnapshot};
use crate::protocol::messages::ProtocolMessage;
use crate::protocol::peer::ProtocolEvent;
use crate::routing::latency::LatencyStats;
use crate::routing::router::{Action, Router};
use crate::transport::connection::Connection;
use crate::transport::frame::Frame;
use crate::transport::handshake::{self, HandshakeConfig};
use crate::types::{
    ConnectionType, Direction, Network, NetworkStatus, PeerEntry, PeerId, ProxyMode, Version,
};
use crate::upnp::UpnpMapping;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};

type PeerSender = mpsc::Sender<Frame>;

/// One modifier delivered to the validation pipeline:
/// `(modifier_type, id, raw_bytes, source_peer_id)`. The peer ID is
/// `Some(...)` for peer-delivered modifiers and `None` for
/// locally-ingested ones.
pub type ModifierDelivery = (u8, [u8; 32], Vec<u8>, Option<u64>);

/// Channel the P2P layer uses to push modifiers at the validation pipeline.
/// The receiving end lives in the main crate; the P2P side `try_send`s.
pub type ModifierSink = mpsc::Sender<ModifierDelivery>;

/// Capacity for the runtime-outbound-request channel.
///
/// Each entry is a single SocketAddr the caller wants queued for outbound
/// connection. 64 is well above the realistic burst rate from an admin
/// using `POST /peers/connect`; the channel is meant to absorb bursts, not
/// store a backlog.
const OUTBOUND_REQUEST_CAPACITY: usize = 64;

/// Shared state every background task needs handles to. Cloning a
/// `BackgroundCtx` clones the inner channel sender and `Arc`s, which is the
/// operation every spawn site wants when handing state to a child future.
#[derive(Clone)]
struct BackgroundCtx {
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
    peer_counter: Arc<AtomicU64>,
    blacklist: Arc<Blacklist>,
    peer_db: Arc<StdMutex<PeerDb>>,
    network: Network,
    counters: Arc<TrafficCounters>,
    /// Optional capture tap. `Some` when `[debug.p2p_capture]` is enabled
    /// in the operator's config. Wired into the frame I/O hot path in
    /// `peer_handler` so every successfully parsed inbound and every
    /// outbound frame is captured.
    capture_tap: Option<Arc<crate::capture::tap::Tap>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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
    /// Shared PeerDb. Same `Arc` is held by the router (for GetPeers /
    /// Peers / PeerConnected) and by the outbound manager (for the
    /// fill phase). Read here by `all_peers()`.
    peer_db: Arc<StdMutex<PeerDb>>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>>,
    upnp_mapping: Option<UpnpMapping>,
    /// Unix epoch ms of the last incoming `ProtocolEvent`. Zero means
    /// "no event seen yet"; reported as None to API consumers.
    last_incoming_ms: Arc<AtomicU64>,
    /// In-memory blacklist of permanently-penalized peer addresses. Populated
    /// by the transport layer and the accept/handshake paths.
    blacklist: Arc<Blacklist>,
    /// Sender for runtime outbound-connection requests. The outbound manager
    /// owns the matching receiver and drains it alongside its seed retry loop.
    outbound_request_tx: mpsc::Sender<SocketAddr>,
    /// Cumulative traffic counters. Same `Arc` is shared with the
    /// router and every peer task. The api adapter reads
    /// [`P2pNode::traffic_snapshot`] for `/stats/p2p`.
    counters: Arc<TrafficCounters>,
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
        modifier_sink: Option<ModifierSink>,
        mode_config: handshake::ModeConfig,
        peer_storage: Box<dyn PeerStorage>,
        capture_tap: Option<Arc<crate::capture::tap::Tap>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (ver_major, ver_minor, ver_patch) = config.version_bytes()?;
        let version = Version::new(ver_major, ver_minor, ver_patch);
        let network = config.proxy.network;
        let network_settings = config.network_settings();
        let max_peer_spec_objects = network_settings.max_peer_spec_objects as usize;

        tracing::info!(network = ?network, version = %version, "P2P layer starting");

        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(256);
        let peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let peer_counter = Arc::new(AtomicU64::new(1));
        let subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>> =
            Arc::new(Mutex::new(None));
        let last_incoming_ms = Arc::new(AtomicU64::new(0));
        let blacklist = Arc::new(Blacklist::new());
        let (outbound_request_tx, outbound_request_rx) =
            mpsc::channel::<SocketAddr>(OUTBOUND_REQUEST_CAPACITY);

        // Discover external addresses before starting listeners.
        // UPnP for IPv4 (NAT traversal), interface enumeration for IPv6 (globally routable).
        // Done before PeerDb construction so the declared addresses can
        // feed `self_addresses` and PeerDb drops self-loop gossip records.
        let mut upnp_mapping: Option<UpnpMapping> = None;
        let mut ipv4_declared: Option<SocketAddr> = None;
        let mut ipv6_declared: Option<SocketAddr> = None;

        if config.upnp.enabled {
            if let Some(ref listener_cfg) = config.listen.ipv4 {
                if let Some(mapping) = crate::upnp::attempt(
                    &config.upnp,
                    listener_cfg.address.port(),
                    listener_cfg.address,
                )
                .await
                {
                    ipv4_declared = Some(mapping.external_addr);
                    upnp_mapping = Some(mapping);
                }
            }
        }

        if let Some(ref listener_cfg) = config.listen.ipv6 {
            ipv6_declared = crate::netif::find_global_ipv6(listener_cfg.address.port());
        }

        let self_addresses: HashSet<SocketAddr> = [ipv4_declared, ipv6_declared]
            .into_iter()
            .flatten()
            .collect();

        let peer_db = PeerDb::new(peer_storage, blacklist.clone(), DEFAULT_CAP, self_addresses)
            .map_err(|e| -> Box<dyn std::error::Error> { format!("PeerDb init: {}", e).into() })?;
        let peer_db = Arc::new(StdMutex::new(peer_db));
        tracing::info!(
            loaded_peers = peer_db.lock().expect("poisoned").count(),
            "PeerDb initialised"
        );

        let router_inner = Router::with_peer_db(
            peer_db.clone(),
            blacklist.clone(),
            max_peer_spec_objects,
            network,
        );
        let counters = router_inner.counters();
        let router = Arc::new(Mutex::new(router_inner));

        let ctx = BackgroundCtx {
            event_tx,
            peer_senders: peer_senders.clone(),
            router: router.clone(),
            peer_counter,
            blacklist: blacklist.clone(),
            peer_db: peer_db.clone(),
            network,
            counters: counters.clone(),
            capture_tap,
        };

        // Start listeners
        if let Some(ref listener_cfg) = config.listen.ipv6 {
            let listener = TcpListener::bind(listener_cfg.address).await?;
            tracing::info!(addr = %listener_cfg.address, mode = ?listener_cfg.mode, declared = ?ipv6_declared, "IPv6 listener started");
            let hs_config = make_handshake_config(
                &config.identity,
                version,
                network,
                listener_cfg.mode,
                mode_config,
                ipv6_declared,
            );
            tokio::spawn(accept_loop(
                listener,
                hs_config,
                listener_cfg.mode,
                listener_cfg.max_inbound,
                ctx.clone(),
            ));
        }

        if let Some(ref listener_cfg) = config.listen.ipv4 {
            let listener = TcpListener::bind(listener_cfg.address).await?;
            tracing::info!(addr = %listener_cfg.address, mode = ?listener_cfg.mode, declared = ?ipv4_declared, "IPv4 listener started");
            let hs_config = make_handshake_config(
                &config.identity,
                version,
                network,
                listener_cfg.mode,
                mode_config,
                ipv4_declared,
            );
            tokio::spawn(accept_loop(
                listener,
                hs_config,
                listener_cfg.mode,
                listener_cfg.max_inbound,
                ctx.clone(),
            ));
        }

        // Start outbound connections — prefer IPv4 declared address (most peers are IPv4)
        let outbound_declared = ipv4_declared.or(ipv6_declared);
        {
            let hs_config = make_handshake_config(
                &config.identity,
                version,
                network,
                ProxyMode::Full,
                mode_config,
                outbound_declared,
            );
            tokio::spawn(outbound_manager(
                config.outbound.seed_peers.clone(),
                config.outbound.min_peers,
                config.outbound.max_peers,
                hs_config,
                ProxyMode::Full,
                ctx.clone(),
                outbound_request_rx,
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
            let last_incoming_ms = last_incoming_ms.clone();
            tokio::spawn(async move {
                event_loop(
                    event_rx,
                    router,
                    peer_senders,
                    subscriber,
                    modifier_sink,
                    last_incoming_ms,
                )
                .await;
            });
        }

        Ok(P2pNode {
            router,
            peer_db,
            peer_senders,
            subscriber,
            upnp_mapping,
            last_incoming_ms,
            blacklist,
            outbound_request_tx,
            counters,
        })
    }

    /// Cumulative traffic counters since process start. The api crate's
    /// `/stats/p2p` adapter consumes this snapshot.
    pub fn traffic_snapshot(&self) -> TrafficSnapshot {
        self.counters.snapshot()
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
        tx.send(frame)
            .await
            .map_err(|_| SendError::ChannelClosed(peer))
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

    /// All known peers (PeerDb entries plus any currently-connected
    /// addresses not in the PeerDb). For `GET /peers/all`.
    pub async fn all_peers(&self) -> Vec<PeerEntry> {
        // Snapshot of currently-connected peers, indexed by address.
        // The lock is dropped before we touch the PeerDb so the two
        // mutexes are never held simultaneously.
        let connected: HashMap<SocketAddr, (ConnectionType, Option<String>)> = {
            let r = self.router.lock().await;
            r.connected_summary()
                .into_iter()
                .map(|s| (s.address, (ConnectionType::from(s.direction), s.agent_name)))
                .collect()
        };

        let records = self.peer_db.lock().expect("peer_db poisoned").all();
        let now = now_ms();

        let mut out = Vec::with_capacity(records.len() + connected.len());
        let mut covered: HashSet<SocketAddr> = HashSet::with_capacity(records.len());

        for rec in records {
            // Prefer the PeerDb agent string; fall back to the live
            // connection's agent (matters when register_peer wrote a
            // stub record without an agent name, e.g. in unit tests).
            let agent = if rec.agent_name.is_empty() {
                connected.get(&rec.address).and_then(|(_, a)| a.clone())
            } else {
                Some(rec.agent_name)
            };
            out.push(PeerEntry {
                address: rec.address,
                agent_name: agent,
                last_seen_ms: Some(rec.last_seen_ms),
                connection_type: connected.get(&rec.address).map(|(ct, _)| *ct),
            });
            covered.insert(rec.address);
        }

        // Rare case: an inbound peer's observed socket differs from
        // its declared address. The declared one ends up in the PeerDb
        // entry; the observed one shows up here as a connection-only
        // overlay so the API caller sees the live connection.
        for (addr, (ct, agent)) in &connected {
            if covered.contains(addr) {
                continue;
            }
            out.push(PeerEntry {
                address: *addr,
                agent_name: agent.clone(),
                last_seen_ms: Some(now),
                connection_type: Some(*ct),
            });
        }

        out
    }

    /// Network status snapshot: last-incoming-message time and current
    /// system time, both in Unix epoch ms. For `GET /peers/status`.
    pub async fn network_status(&self) -> NetworkStatus {
        let last = self.last_incoming_ms.load(Ordering::Relaxed);
        NetworkStatus {
            last_incoming_message_ms: if last == 0 { None } else { Some(last) },
            current_network_time_ms: now_ms(),
        }
    }

    /// Addresses of peers currently permanently penalty-banned by this node.
    /// Does NOT include temporarily rate-limited peers. For
    /// `GET /peers/blacklisted`.
    pub async fn blacklisted_peers(&self) -> Vec<SocketAddr> {
        self.blacklist.list()
    }

    /// Queue an outbound connection attempt to `addr`. Returns `Ok(())`
    /// when the request is queued (not when the connection completes).
    ///
    /// Rejects:
    /// - Loopback addresses (policy: P2P does not dial back to the local node)
    /// - Addresses already in the blacklist
    /// - Addresses we are already connected to (no-op)
    /// - When the outbound-request channel is full
    ///
    /// For `POST /peers/connect`.
    pub async fn queue_outbound_connection(&self, addr: SocketAddr) -> Result<(), String> {
        if addr.ip().is_loopback() {
            return Err(format!("address {} is loopback", addr));
        }
        if self.blacklist.contains(addr) {
            return Err(format!("address {} is blacklisted", addr));
        }
        if self.router.lock().await.is_addr_connected(addr) {
            return Err(format!("already connected to {}", addr));
        }
        match self.outbound_request_tx.try_send(addr) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err("outbound request queue is full".to_string())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err("outbound manager has shut down".to_string())
            }
        }
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
    modifier_sink: Option<ModifierSink>,
    last_incoming_ms: Arc<AtomicU64>,
) {
    loop {
        match event_rx.recv().await {
            Some(event) => {
                last_incoming_ms.store(now_ms(), Ordering::Relaxed);

                // Tap: send to subscriber before routing (non-blocking).
                // Filter out ModifierResponse — it has its own path via modifier_sink
                // and would otherwise flood the bounded subscriber channel, causing
                // Inv and SyncInfo events to be silently dropped during heavy sync.
                {
                    let dominated_by_modifier_response = matches!(
                        &event,
                        ProtocolEvent::Message {
                            message: ProtocolMessage::ModifierResponse { .. },
                            ..
                        }
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
                        Action::Validate {
                            modifier_type,
                            id,
                            data,
                            peer_id,
                        } => {
                            if let Some(ref sink) = modifier_sink {
                                if modifier_type != 101 {
                                    tracing::debug!(
                                        modifier_type,
                                        data_len = data.len(),
                                        "delivering non-header to pipeline"
                                    );
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
    ctx: BackgroundCtx,
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
    let agent_name = Some(spec.agent.clone());
    ctx.router
        .lock()
        .await
        .register_peer(peer_id, direction, mode, addr, rest_api_url, agent_name);

    // Send PeerConnected event
    let _ = ctx
        .event_tx
        .send(ProtocolEvent::PeerConnected {
            peer_id,
            spec: spec.clone(),
            direction,
            addr,
        })
        .await;

    // Split connection for concurrent read/write
    let (mut reader, mut writer, magic, _, _) = conn.split();

    // Create write channel
    let (write_tx, mut write_rx) = mpsc::channel::<Frame>(64);
    ctx.peer_senders.lock().await.insert(peer_id, write_tx);

    // Writer task
    let writer_counters = ctx.counters.clone();
    let writer_tap = ctx.capture_tap.clone();
    let writer_addr = addr;
    let write_handle = tokio::spawn(async move {
        while let Some(frame) = write_rx.recv().await {
            let wire_bytes = counters::frame_wire_bytes(&frame);
            if let Err(e) = crate::transport::frame::write_frame(
                &mut writer,
                &magic,
                &frame,
                writer_addr,
                writer_tap.as_deref(),
            )
            .await
            {
                tracing::warn!(peer = %peer_id, error = %e, "Write failed");
                break;
            }
            writer_counters.record_out_frame(&frame, wire_bytes);
        }
    });

    // Reader loop
    loop {
        match crate::transport::frame::read_frame(
            &mut reader,
            &magic,
            addr,
            &ctx.blacklist,
            ctx.capture_tap.as_deref(),
        )
        .await
        {
            Ok(frame) => {
                let wire_bytes = counters::frame_wire_bytes(&frame);
                match ProtocolMessage::from_frame(&frame) {
                    Ok(msg) => {
                        ctx.counters.record_in_message(&msg, wire_bytes);
                        let event = ProtocolEvent::Message {
                            peer_id,
                            message: msg,
                        };
                        if ctx.event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            peer = %addr.ip(),
                            kind = "message_parse_failed",
                            detail = %e,
                            "PENALTY"
                        );
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
    ctx.peer_senders.lock().await.remove(&peer_id);
    write_handle.abort();

    let _ = ctx
        .event_tx
        .send(ProtocolEvent::PeerDisconnected {
            peer_id,
            reason: "connection closed".into(),
        })
        .await;

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
    ctx: BackgroundCtx,
) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let remote_ip = addr.ip();
                let inbound_count = ctx.router.lock().await.inbound_peers().len();
                if inbound_count >= max_inbound {
                    tracing::warn!(
                        peer = %remote_ip,
                        kind = "connection_limit_exceeded",
                        "PENALTY"
                    );
                    continue;
                }

                let peer_id = PeerId(ctx.peer_counter.fetch_add(1, Ordering::Relaxed));
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
                let ctx = ctx.clone();

                tokio::spawn(async move {
                    match Connection::inbound(stream, &hs, &ctx.counters).await {
                        Ok(conn) => {
                            run_peer(peer_id, conn, Direction::Inbound, mode, addr, ctx).await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer = %addr.ip(),
                                kind = "handshake_failed",
                                detail = %e,
                                "PENALTY"
                            );
                            ctx.blacklist.record_permanent(addr);
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

/// Tick period for the fill phase. JVM uses no equivalent loop (its
/// `PeerSynchronizer` is event-driven), so this is our own choice —
/// slow enough not to thrash the network, fast enough to recover from
/// peer churn in a couple of minutes.
const OUTBOUND_FILL_INTERVAL: Duration = Duration::from_secs(30);

/// How long after a dial attempt (success or failure) the fill phase
/// will skip re-trying the same address. 60s mirrors what most peers'
/// reconnect throttling tolerates.
const OUTBOUND_REDIAL_COOLDOWN: Duration = Duration::from_secs(60);

async fn outbound_manager(
    seeds: Vec<SocketAddr>,
    min_peers: usize,
    max_peers: usize,
    hs_config: HandshakeConfig,
    mode: ProxyMode,
    ctx: BackgroundCtx,
    mut request_rx: mpsc::Receiver<SocketAddr>,
) {
    /// Initial sleep between floor-phase seed bursts; doubles up to 5
    /// minutes if seeds keep refusing connections.
    const FLOOR_BACKOFF_INITIAL: Duration = Duration::from_secs(5);
    const FLOOR_BACKOFF_MAX: Duration = Duration::from_secs(300);

    let mut floor_backoff = FLOOR_BACKOFF_INITIAL;
    // Address → instant after which the fill phase may try again.
    // Expired entries are pruned on every tick.
    let mut cooldown: HashMap<SocketAddr, Instant> = HashMap::new();

    loop {
        prune_cooldown(&mut cooldown);
        let current_outbound = ctx.router.lock().await.outbound_peers().len();

        let sleep_for = if current_outbound < min_peers {
            // ---- Floor phase: aggressive seed dialing. ----
            for addr in &seeds {
                let current = ctx.router.lock().await.outbound_peers().len();
                if current >= min_peers {
                    break;
                }
                spawn_outbound_connect(*addr, &hs_config, mode, ctx.clone()).await;
                cooldown.insert(*addr, Instant::now() + OUTBOUND_REDIAL_COOLDOWN);
            }
            let s = floor_backoff;
            floor_backoff = (floor_backoff * 2).min(FLOOR_BACKOFF_MAX);
            s
        } else {
            // At or above floor: reset backoff so we restart fast if
            // peers churn back below min_peers later.
            floor_backoff = FLOOR_BACKOFF_INITIAL;
            if current_outbound < max_peers {
                // ---- Fill phase: one PeerDb candidate per tick. ----
                if let Some(candidate) =
                    pick_fill_candidate(&ctx, max_peers.saturating_sub(current_outbound), &cooldown)
                        .await
                {
                    tracing::info!(addr = %candidate, "Outbound fill: dialing PeerDb candidate");
                    spawn_outbound_connect(candidate, &hs_config, mode, ctx.clone()).await;
                    cooldown.insert(candidate, Instant::now() + OUTBOUND_REDIAL_COOLDOWN);
                }
            }
            // Same cadence whether we found a candidate or not — the
            // PeerDb may fill via gossip between now and the next tick.
            OUTBOUND_FILL_INTERVAL
        };

        // Either wait out the chosen delay or wake early on a runtime
        // request. The select biases neither way: backoff-driven seed
        // retries must keep working at scale, AND admin-triggered
        // connects must dial immediately.
        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            maybe_addr = request_rx.recv() => {
                match maybe_addr {
                    Some(addr) => {
                        spawn_outbound_connect(addr, &hs_config, mode, ctx.clone()).await;
                        cooldown.insert(addr, Instant::now() + OUTBOUND_REDIAL_COOLDOWN);
                    }
                    None => {
                        tracing::info!("Outbound request channel closed; outbound manager exiting");
                        return;
                    }
                }
            }
        }
    }
}

fn prune_cooldown(cooldown: &mut HashMap<SocketAddr, Instant>) {
    let now = Instant::now();
    cooldown.retain(|_, until| *until > now);
}

/// Returns the most-recently-seen PeerDb candidate that is not
/// currently connected, blacklisted, or in cooldown.
async fn pick_fill_candidate(
    ctx: &BackgroundCtx,
    fill_slots: usize,
    cooldown: &HashMap<SocketAddr, Instant>,
) -> Option<SocketAddr> {
    if fill_slots == 0 {
        return None;
    }
    let connected: HashSet<SocketAddr> = ctx
        .router
        .lock()
        .await
        .connected_addrs()
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    let mut exclude = connected;
    exclude.extend(cooldown.keys().copied());

    let db = ctx.peer_db.lock().expect("peer_db poisoned");
    db.recent(fill_slots, &exclude)
        .into_iter()
        .filter(|r| !is_bogus_address(r.address, ctx.network))
        .map(|r| r.address)
        .next()
}

async fn spawn_outbound_connect(
    addr: SocketAddr,
    hs_config: &HandshakeConfig,
    mode: ProxyMode,
    ctx: BackgroundCtx,
) {
    tracing::info!(addr = %addr, "Connecting to outbound peer");
    let connect = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(addr),
    )
    .await;

    match connect {
        Ok(Ok(stream)) => {
            let peer_id = PeerId(ctx.peer_counter.fetch_add(1, Ordering::Relaxed));
            let hs = HandshakeConfig {
                agent_name: hs_config.agent_name.clone(),
                peer_name: hs_config.peer_name.clone(),
                version: hs_config.version,
                network: hs_config.network,
                mode: hs_config.mode,
                declared_address: hs_config.declared_address,
                mode_config: hs_config.mode_config,
            };
            tokio::spawn(async move {
                match Connection::outbound(stream, &hs, &ctx.counters).await {
                    Ok(conn) => {
                        if handshake::is_proxy(conn.peer_spec()) {
                            tracing::info!(peer = %peer_id, addr = %addr, "Outbound peer is a proxy, skipping");
                            return;
                        }
                        tracing::info!(peer = %peer_id, "Outbound handshake OK");
                        run_peer(peer_id, conn, Direction::Outbound, mode, addr, ctx).await;
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer_id, addr = %addr, error = %e, "Outbound handshake failed");
                    }
                }
            });
        }
        Ok(Err(e)) => {
            tracing::warn!(addr = %addr, error = %e, "Connect failed");
        }
        Err(_) => {
            tracing::warn!(addr = %addr, "Connect timeout");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_addr() -> SocketAddr {
        "127.0.0.1:9000".parse().unwrap()
    }

    fn pub_addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// A P2pNode plus the shared state and outbound-request receiver. The
    /// fields are exposed for tests that need to seed the router, observe
    /// queued outbound requests, or record blacklist entries.
    struct TestHarness {
        node: P2pNode,
        router: Arc<Mutex<Router>>,
        peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
        blacklist: Arc<Blacklist>,
        outbound_request_rx: mpsc::Receiver<SocketAddr>,
    }

    fn shared_peer_db(blacklist: Arc<Blacklist>) -> Arc<StdMutex<PeerDb>> {
        let storage: Box<dyn PeerStorage> = Box::new(crate::peer_db::MemoryPeerStorage::new());
        let db = PeerDb::new(storage, blacklist, DEFAULT_CAP, HashSet::new())
            .expect("MemoryPeerStorage::load_all is infallible");
        Arc::new(StdMutex::new(db))
    }

    /// Build a P2pNode with no background tasks — just the struct with shared state.
    fn test_node() -> TestHarness {
        let blacklist = Arc::new(Blacklist::new());
        let peer_db = shared_peer_db(blacklist.clone());
        let router_inner =
            Router::with_peer_db(peer_db.clone(), blacklist.clone(), 64, Network::Mainnet);
        let counters = router_inner.counters();
        let router = Arc::new(Mutex::new(router_inner));
        let peer_senders = Arc::new(Mutex::new(HashMap::new()));
        let subscriber = Arc::new(Mutex::new(None));
        let (outbound_request_tx, outbound_request_rx) =
            mpsc::channel::<SocketAddr>(OUTBOUND_REQUEST_CAPACITY);
        let node = P2pNode {
            router: router.clone(),
            peer_db,
            peer_senders: peer_senders.clone(),
            subscriber,
            upnp_mapping: None,
            last_incoming_ms: Arc::new(AtomicU64::new(0)),
            blacklist: blacklist.clone(),
            outbound_request_tx,
            counters,
        };
        TestHarness {
            node,
            router,
            peer_senders,
            blacklist,
            outbound_request_rx,
        }
    }

    #[tokio::test]
    async fn send_to_delivers_to_correct_peer() {
        let TestHarness {
            node, peer_senders, ..
        } = test_node();
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
        let TestHarness { node, .. } = test_node();
        let result = node.send_to(PeerId(999), ProtocolMessage::GetPeers).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SendError::UnknownPeer(PeerId(999))
        ));
    }

    #[tokio::test]
    async fn send_to_closed_channel_returns_error() {
        let TestHarness {
            node, peer_senders, ..
        } = test_node();
        let peer = PeerId(1);

        let (tx, rx) = mpsc::channel::<Frame>(64);
        peer_senders.lock().await.insert(peer, tx);
        drop(rx); // Close the receiving end

        let result = node.send_to(peer, ProtocolMessage::GetPeers).await;
        assert!(matches!(
            result.unwrap_err(),
            SendError::ChannelClosed(PeerId(1))
        ));
    }

    #[tokio::test]
    async fn broadcast_outbound_sends_to_all_outbound_peers() {
        let TestHarness {
            node,
            router,
            peer_senders,
            ..
        } = test_node();

        let peer_a = PeerId(1);
        let peer_b = PeerId(2);
        let peer_inbound = PeerId(3);

        router.lock().await.register_peer(
            peer_a,
            Direction::Outbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );
        router.lock().await.register_peer(
            peer_b,
            Direction::Outbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );
        router.lock().await.register_peer(
            peer_inbound,
            Direction::Inbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );

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
        let TestHarness {
            node,
            router,
            peer_senders,
            ..
        } = test_node();

        let peer_ok = PeerId(1);
        let peer_full = PeerId(2);

        router.lock().await.register_peer(
            peer_ok,
            Direction::Outbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );
        router.lock().await.register_peer(
            peer_full,
            Direction::Outbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );

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
        let blacklist = Arc::new(Blacklist::new());
        let peer_db = shared_peer_db(blacklist.clone());
        let router_inner =
            Router::with_peer_db(peer_db.clone(), blacklist.clone(), 64, Network::Mainnet);
        let counters = router_inner.counters();
        let router = Arc::new(Mutex::new(router_inner));
        let peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>> =
            Arc::new(Mutex::new(None));
        let (outbound_request_tx, _outbound_request_rx) =
            mpsc::channel::<SocketAddr>(OUTBOUND_REQUEST_CAPACITY);
        let last_incoming_ms = Arc::new(AtomicU64::new(0));

        let node = P2pNode {
            router: router.clone(),
            peer_db,
            peer_senders: peer_senders.clone(),
            subscriber: subscriber.clone(),
            upnp_mapping: None,
            last_incoming_ms: last_incoming_ms.clone(),
            blacklist,
            outbound_request_tx,
            counters,
        };

        let mut events = node.subscribe().await;

        // Drive the event loop with a one-shot channel
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);

        let r = router.clone();
        let ps = peer_senders.clone();
        let sub = subscriber.clone();
        let last = last_incoming_ms.clone();
        let handle = tokio::spawn(async move {
            event_loop(event_rx, r, ps, sub, None, last).await;
        });

        // Register a peer so the router doesn't choke
        router.lock().await.register_peer(
            PeerId(1),
            Direction::Outbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );

        // Send a protocol event
        event_tx
            .send(ProtocolEvent::Message {
                peer_id: PeerId(1),
                message: ProtocolMessage::GetPeers,
            })
            .await
            .unwrap();

        // Subscriber should see it
        let event = events.recv().await.unwrap();
        assert!(matches!(
            event,
            ProtocolEvent::Message {
                peer_id: PeerId(1),
                ..
            }
        ));

        // Cleanup
        drop(event_tx);
        handle.await.unwrap();
    }

    // ------------------------------------------------------------------
    // New peer-query method tests
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn network_status_starts_with_none_last_incoming() {
        let TestHarness { node, .. } = test_node();
        let status = node.network_status().await;
        assert!(status.last_incoming_message_ms.is_none());
        assert!(status.current_network_time_ms > 0);
    }

    #[tokio::test]
    async fn network_status_updates_on_incoming_event() {
        // Build the same state the production start() does, then drive the
        // event loop with a synthetic Message event.
        let blacklist = Arc::new(Blacklist::new());
        let peer_db = shared_peer_db(blacklist.clone());
        let router_inner =
            Router::with_peer_db(peer_db.clone(), blacklist.clone(), 64, Network::Mainnet);
        let counters = router_inner.counters();
        let router = Arc::new(Mutex::new(router_inner));
        let peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>> =
            Arc::new(Mutex::new(None));
        let last_incoming_ms = Arc::new(AtomicU64::new(0));
        let (outbound_request_tx, _outbound_request_rx) =
            mpsc::channel::<SocketAddr>(OUTBOUND_REQUEST_CAPACITY);

        let node = P2pNode {
            router: router.clone(),
            peer_db,
            peer_senders: peer_senders.clone(),
            subscriber: subscriber.clone(),
            upnp_mapping: None,
            last_incoming_ms: last_incoming_ms.clone(),
            blacklist,
            outbound_request_tx,
            counters,
        };

        // Confirm initial state
        assert!(node
            .network_status()
            .await
            .last_incoming_message_ms
            .is_none());

        // Drive the event loop
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);
        let r = router.clone();
        let ps = peer_senders.clone();
        let sub = subscriber.clone();
        let last = last_incoming_ms.clone();
        let handle = tokio::spawn(async move {
            event_loop(event_rx, r, ps, sub, None, last).await;
        });

        router.lock().await.register_peer(
            PeerId(1),
            Direction::Outbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );
        event_tx
            .send(ProtocolEvent::Message {
                peer_id: PeerId(1),
                message: ProtocolMessage::GetPeers,
            })
            .await
            .unwrap();

        // Give the event loop a tick to drain
        for _ in 0..50 {
            if node
                .network_status()
                .await
                .last_incoming_message_ms
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let status = node.network_status().await;
        assert!(status.last_incoming_message_ms.is_some());
        assert!(status.last_incoming_message_ms.unwrap() <= status.current_network_time_ms);

        drop(event_tx);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn blacklisted_peers_empty_by_default() {
        let TestHarness { node, .. } = test_node();
        assert!(node.blacklisted_peers().await.is_empty());
    }

    #[tokio::test]
    async fn blacklisted_peers_reflects_records() {
        let TestHarness {
            node, blacklist, ..
        } = test_node();
        blacklist.record_permanent(pub_addr("203.0.113.5:9030"));
        blacklist.record_permanent(pub_addr("198.51.100.7:9030"));

        let mut listed = node.blacklisted_peers().await;
        listed.sort();
        assert_eq!(
            listed,
            vec![pub_addr("198.51.100.7:9030"), pub_addr("203.0.113.5:9030")]
        );
    }

    #[tokio::test]
    async fn queue_outbound_rejects_loopback() {
        let TestHarness { node, .. } = test_node();
        let result = node
            .queue_outbound_connection(pub_addr("127.0.0.1:9030"))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("loopback"));
    }

    #[tokio::test]
    async fn queue_outbound_rejects_blacklisted() {
        let TestHarness {
            node, blacklist, ..
        } = test_node();
        let addr = pub_addr("203.0.113.10:9030");
        blacklist.record_permanent(addr);
        let result = node.queue_outbound_connection(addr).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("blacklisted"));
    }

    #[tokio::test]
    async fn queue_outbound_rejects_already_connected() {
        let TestHarness { node, router, .. } = test_node();
        let addr = pub_addr("203.0.113.11:9030");
        router.lock().await.register_peer(
            PeerId(1),
            Direction::Outbound,
            ProxyMode::Full,
            addr,
            None,
            None,
        );
        let result = node.queue_outbound_connection(addr).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already connected"));
    }

    #[tokio::test]
    async fn queue_outbound_ok_pushes_to_channel() {
        let TestHarness {
            node,
            outbound_request_rx: mut orq,
            ..
        } = test_node();
        let addr = pub_addr("203.0.113.20:9030");
        node.queue_outbound_connection(addr).await.unwrap();
        // The address was pushed to the outbound-request channel without
        // waiting for the connection to complete.
        let received = orq.recv().await.expect("channel should have one entry");
        assert_eq!(received, addr);
    }

    #[tokio::test]
    async fn queue_outbound_returns_err_when_channel_full() {
        // Holding the harness keeps the receiver alive so try_send can saturate.
        let h = test_node();
        let node = &h.node;
        // Saturate the channel — capacity is OUTBOUND_REQUEST_CAPACITY.
        // The receiver is held alive (`_orq`), so try_send fills it.
        for i in 0..OUTBOUND_REQUEST_CAPACITY {
            let addr: SocketAddr = format!("203.0.113.30:{}", 9100 + i as u16).parse().unwrap();
            node.queue_outbound_connection(addr).await.unwrap();
        }
        let one_more = pub_addr("203.0.113.30:9999");
        let result = node.queue_outbound_connection(one_more).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("queue is full"));
    }

    #[tokio::test]
    async fn all_peers_lists_connected_with_connection_type() {
        let TestHarness { node, router, .. } = test_node();

        router.lock().await.register_peer(
            PeerId(1),
            Direction::Outbound,
            ProxyMode::Full,
            pub_addr("203.0.113.40:9030"),
            None,
            Some("ergoref".to_string()),
        );
        router.lock().await.register_peer(
            PeerId(2),
            Direction::Inbound,
            ProxyMode::Full,
            pub_addr("203.0.113.41:9030"),
            None,
            Some("nautilus".to_string()),
        );

        let peers = node.all_peers().await;
        assert_eq!(peers.len(), 2);
        let outbound = peers
            .iter()
            .find(|p| p.address == pub_addr("203.0.113.40:9030"))
            .unwrap();
        assert_eq!(outbound.agent_name.as_deref(), Some("ergoref"));
        assert!(matches!(
            outbound.connection_type,
            Some(crate::types::ConnectionType::Outgoing)
        ));
        assert!(outbound.last_seen_ms.is_some());

        let inbound = peers
            .iter()
            .find(|p| p.address == pub_addr("203.0.113.41:9030"))
            .unwrap();
        assert_eq!(inbound.agent_name.as_deref(), Some("nautilus"));
        assert!(matches!(
            inbound.connection_type,
            Some(crate::types::ConnectionType::Incoming)
        ));
    }

    #[tokio::test]
    async fn all_peers_lists_disconnected_with_no_connection_type() {
        let TestHarness { node, router, .. } = test_node();

        let addr = pub_addr("203.0.113.50:9030");
        router.lock().await.register_peer(
            PeerId(1),
            Direction::Outbound,
            ProxyMode::Full,
            addr,
            None,
            Some("ergoref".to_string()),
        );

        // Simulate disconnect via the router (the same path the event loop uses).
        let _ = router
            .lock()
            .await
            .handle_event(ProtocolEvent::PeerDisconnected {
                peer_id: PeerId(1),
                reason: "test".into(),
            });

        let peers = node.all_peers().await;
        assert_eq!(peers.len(), 1);
        let entry = &peers[0];
        assert_eq!(entry.address, addr);
        assert!(entry.connection_type.is_none());
        assert!(entry.last_seen_ms.is_some());
        assert_eq!(entry.agent_name.as_deref(), Some("ergoref"));
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

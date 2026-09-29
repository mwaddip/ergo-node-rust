//! P2P node entry point.
//!
//! The caller provides a config and a modifier sink, then calls
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::{Mutex, Notify};
use tokio::time::{interval, Duration};

/// Most frames a peer's outbound queue holds before its writer takes them
/// (`facts/p2p-node.md` § Peer write queues). JVM v6.0.6
/// `PeerConnectionHandler.MaxBufferedOutboundMessages`.
const MAX_QUEUED_FRAMES: usize = 64;

/// Most wire bytes a peer's outbound queue holds before its writer takes
/// them: one maximum-size frame, so the largest legal frame always fits and
/// a backlog beyond it means the peer is not reading. JVM v6.0.6
/// `MaxBufferedOutboundBytes = MaxMessageSize + HeaderLength + ChecksumLength`.
const MAX_QUEUED_BYTES: usize =
    crate::transport::frame::MAX_BODY_SIZE as usize + crate::transport::frame::HEADER_SIZE;

/// Why a peer connection ended. [`DisconnectReason::as_str`] is the `reason`
/// of the `peer_disconnected` journal event (`facts/journal-events.md`) and
/// of `ProtocolEvent::PeerDisconnected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisconnectReason {
    /// The peer closed the connection, or a read failed.
    ConnectionClosed,
    /// A write to the socket failed, or the writer task is gone.
    WriteFailed,
    /// The peer's outbound queue hit a bound: it stopped reading.
    OutboundQueueFull,
    /// The node aborted the connection itself (`disconnect_peer`).
    Disconnected,
}

impl DisconnectReason {
    fn as_str(self) -> &'static str {
        match self {
            DisconnectReason::ConnectionClosed => "connection_closed",
            DisconnectReason::WriteFailed => "write_failed",
            DisconnectReason::OutboundQueueFull => "outbound_queue_full",
            DisconnectReason::Disconnected => "disconnected",
        }
    }
}

/// State one connection shares between its [`PeerSender`], its reader loop
/// and its writer task.
struct PeerLink {
    /// Wire bytes of the frames queued and not yet taken by the writer.
    queued_bytes: AtomicUsize,
    /// The connection's disconnect reason, set once, by the first abort.
    reason: OnceLock<DisconnectReason>,
    /// Wakes the reader loop once `reason` is set.
    abort_signal: Notify,
}

impl PeerLink {
    fn new() -> Self {
        Self {
            queued_bytes: AtomicUsize::new(0),
            reason: OnceLock::new(),
            abort_signal: Notify::new(),
        }
    }

    /// Initiate an abort (`facts/p2p-node.md` § Aborting a connection). Never
    /// waits: the peer's own task stops the reader, closes the socket and
    /// emits `PeerDisconnected`. The first reason set is the connection's;
    /// returns whether this call set it.
    fn abort(&self, reason: DisconnectReason) -> bool {
        let mut first = false;
        self.reason.get_or_init(|| {
            first = true;
            reason
        });
        if first {
            self.abort_signal.notify_waiters();
        }
        first
    }

    fn is_aborted(&self) -> bool {
        self.reason.get().is_some()
    }

    /// Resolves once an abort has been initiated, with the first reason.
    async fn aborted(&self) -> DisconnectReason {
        loop {
            // Created before the check: `notify_waiters` reaches a
            // `Notified` from the moment it exists, so an abort landing
            // between the check and the await is not missed.
            let signal = self.abort_signal.notified();
            if let Some(reason) = self.reason.get() {
                return *reason;
            }
            signal.await;
        }
    }
}

/// A peer's outbound queue, as held in the `peer_senders` map for everything
/// that writes to the peer: the event loop, the keepalive, `send_to` and
/// `broadcast`. The peer's writer task drains the other end.
struct PeerSender {
    peer_id: PeerId,
    queue: mpsc::Sender<Frame>,
    link: Arc<PeerLink>,
}

impl PeerSender {
    /// A fresh queue for `peer_id`, and the receiving end its writer drains.
    fn new(peer_id: PeerId) -> (Self, mpsc::Receiver<Frame>) {
        let (queue, rx) = mpsc::channel(MAX_QUEUED_FRAMES);
        let sender = Self {
            peer_id,
            queue,
            link: Arc::new(PeerLink::new()),
        };
        (sender, rx)
    }

    /// Queue `frame` for the peer's writer. Never waits: a frame that would
    /// take the queue past either bound, or a queue whose writer is gone,
    /// aborts the connection instead (`facts/p2p-node.md` § Peer write
    /// queues).
    fn enqueue(&self, frame: Frame) -> Result<(), SendError> {
        if self.link.is_aborted() {
            return Err(SendError::ChannelClosed(self.peer_id));
        }
        let frame_bytes = counters::frame_wire_bytes(&frame) as usize;
        // Counted before it is queued, so a racing enqueue can see this frame
        // early but never late: the byte bound holds.
        let queued_bytes = self
            .link
            .queued_bytes
            .fetch_add(frame_bytes, Ordering::Relaxed);
        if queued_bytes + frame_bytes > MAX_QUEUED_BYTES {
            return Err(self.abort_full(queued_bytes, frame_bytes));
        }
        match self.queue.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(self.abort_full(queued_bytes, frame_bytes))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // The writer has exited: the peer can never be written to again.
                self.link.abort(DisconnectReason::WriteFailed);
                Err(SendError::ChannelClosed(self.peer_id))
            }
        }
    }

    /// Abort the connection over a full queue. `queued_bytes` excludes the
    /// rejected frame.
    fn abort_full(&self, queued_bytes: usize, frame_bytes: usize) -> SendError {
        if self.link.abort(DisconnectReason::OutboundQueueFull) {
            tracing::warn!(
                peer = %self.peer_id,
                queued_frames = self.queue.max_capacity() - self.queue.capacity(),
                queued_bytes,
                frame_bytes,
                "Outbound queue full, aborting connection"
            );
        }
        SendError::QueueFull(self.peer_id)
    }
}

/// Every connected peer's queue. A `std` mutex: it is only ever held for map
/// operations and non-blocking enqueues, never across an await
/// (`facts/p2p-node.md` § Peer write queues).
type PeerSenders = Arc<StdMutex<HashMap<PeerId, PeerSender>>>;

/// One admitted inbound connection's claim on `max_inbound`. Reserved at
/// accept, held through the handshake and, once that succeeds, by the
/// registered peer until its teardown; dropping it releases the slot. So an
/// admitted connection counts at every instant from accept to disconnect
/// (`facts/p2p-node.md` § Inbound admission).
struct InboundSlot(Arc<AtomicUsize>);

impl InboundSlot {
    /// Reserve a slot unless the `admitted` count, which spans both
    /// listeners, has reached `max_inbound`.
    fn reserve(admitted: &Arc<AtomicUsize>, max_inbound: usize) -> Option<Self> {
        admitted
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < max_inbound).then_some(n + 1)
            })
            .ok()
            .map(|_| InboundSlot(admitted.clone()))
    }
}

impl Drop for InboundSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

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
    peer_senders: PeerSenders,
    router: Arc<Mutex<Router>>,
    peer_counter: Arc<AtomicU64>,
    /// Inbound connections admitted and not yet disconnected, across both
    /// listeners: handshakes in flight plus registered inbound peers. Each
    /// one holds an [`InboundSlot`].
    inbound_admitted: Arc<AtomicUsize>,
    blacklist: Arc<Blacklist>,
    peer_db: Arc<StdMutex<PeerDb>>,
    network: Network,
    /// Gates address-sanity filtering in the outbound fill-phase candidate
    /// selection (`pick_fill_candidate`). Mirrors the router's flag so the
    /// `[network].filter_bogus_addresses = false` case can dial addresses
    /// that bypass the bogus classifier. See `facts/p2p-routing.md`.
    filter_bogus_addresses: bool,
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
    /// The peer is disconnecting: its connection is being aborted, or its
    /// writer is gone.
    ChannelClosed(PeerId),
    /// The peer's outbound queue was full. Its connection has been aborted.
    QueueFull(PeerId),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::UnknownPeer(pid) => write!(f, "unknown peer: {}", pid),
            SendError::ChannelClosed(pid) => write!(f, "channel closed for peer: {}", pid),
            SendError::QueueFull(pid) => write!(f, "outbound queue full for peer: {}", pid),
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
    peer_senders: PeerSenders,
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
    /// - Every modifier a peer delivers in a `ModifierResponse` is sent to
    ///   `modifier_sink`, without waiting: a node that drops the modifiers it
    ///   receives cannot sync, so the sink is required.
    pub async fn start(
        config: Config,
        modifier_sink: ModifierSink,
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
        let peer_senders: PeerSenders = Arc::new(StdMutex::new(HashMap::new()));
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
            network_settings.filter_bogus_addresses,
        );
        let counters = router_inner.counters();
        let router = Arc::new(Mutex::new(router_inner));

        let ctx = BackgroundCtx {
            event_tx,
            peer_senders: peer_senders.clone(),
            router: router.clone(),
            peer_counter,
            inbound_admitted: Arc::new(AtomicUsize::new(0)),
            blacklist: blacklist.clone(),
            peer_db: peer_db.clone(),
            network,
            filter_bogus_addresses: network_settings.filter_bogus_addresses,
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
                    let frame = ProtocolMessage::GetPeers.to_frame();
                    let senders = peer_senders.lock().expect("peer_senders poisoned");
                    for pid in outbound {
                        if let Some(sender) = senders.get(&pid) {
                            let _ = sender.enqueue(frame.clone());
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

    /// Send a protocol message to a specific peer.
    ///
    /// # Contract
    /// - **Precondition**: `peer` is a currently connected peer.
    /// - **Postcondition**: The message is queued for the peer's writer, or
    ///   the peer's connection is aborted.
    /// - Never waits for queue space.
    /// - Returns `SendError::UnknownPeer` if the peer is not connected.
    /// - Returns `SendError::ChannelClosed` if the peer is disconnecting.
    /// - Returns `SendError::QueueFull` if its queue is full, after aborting
    ///   the connection.
    pub async fn send_to(&self, peer: PeerId, message: ProtocolMessage) -> Result<(), SendError> {
        let frame = message.to_frame();
        let senders = self.peer_senders.lock().expect("peer_senders poisoned");
        let sender = senders.get(&peer).ok_or(SendError::UnknownPeer(peer))?;
        sender.enqueue(frame)
    }

    /// Send a protocol message to every connected peer, inbound and
    /// outbound: the JVM's `SendToNetwork(msg, Broadcast)`, which it uses to
    /// announce blocks and transactions. Every connected peer has a queue in
    /// `peer_senders`, so the broadcast goes to each queue there.
    ///
    /// Never waits: a peer whose queue is full is aborted, as for `send_to`,
    /// and the broadcast goes on to the rest. A peer that is disconnecting
    /// is skipped.
    pub async fn broadcast(&self, message: ProtocolMessage) {
        let frame = message.to_frame();
        let senders = self.peer_senders.lock().expect("peer_senders poisoned");
        for sender in senders.values() {
            let _ = sender.enqueue(frame.clone());
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

    /// Abort a peer's connection (`facts/p2p-node.md` § Aborting a
    /// connection). Returns once the abort is initiated; `PeerDisconnected`
    /// follows from the peer's own task. Unknown or already-disconnected
    /// peer: no-op.
    pub async fn disconnect_peer(&self, peer_id: PeerId) {
        let senders = self.peer_senders.lock().expect("peer_senders poisoned");
        if let Some(sender) = senders.get(&peer_id) {
            sender.link.abort(DisconnectReason::Disconnected);
        }
    }

    /// Install the router's local-serve hook for ModifierRequests: the
    /// callback answers `(modifier_type, id)` with the modifier's bytes
    /// when the node has it. The hits go back to the requester; a miss
    /// gets no answer, and no request is ever passed on to another peer.
    /// See `facts/p2p-routing.md`.
    pub async fn set_local_serve(&self, serve: crate::routing::router::LocalServeFn) {
        self.router.lock().await.set_local_serve(serve);
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
    peer_senders: PeerSenders,
    subscriber: Arc<Mutex<Option<mpsc::Sender<ProtocolEvent>>>>,
    modifier_sink: ModifierSink,
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
                // Frames are built before the map is locked. Enqueueing never
                // waits (a full queue aborts that peer instead), so no peer can
                // stall this loop.
                let mut sends: Vec<(PeerId, Frame)> = Vec::new();
                for action in actions {
                    match action {
                        Action::Send { target, message } => {
                            sends.push((target, message.to_frame()));
                        }
                        Action::Validate {
                            modifier_type,
                            id,
                            data,
                            peer_id,
                        } => {
                            if modifier_type != 101 {
                                tracing::debug!(
                                    modifier_type,
                                    data_len = data.len(),
                                    "delivering non-header to pipeline"
                                );
                            }
                            let _ =
                                modifier_sink.try_send((modifier_type, id, data, Some(peer_id.0)));
                        }
                    }
                }
                if !sends.is_empty() {
                    let senders = peer_senders.lock().expect("peer_senders poisoned");
                    for (target, frame) in sends {
                        if let Some(sender) = senders.get(&target) {
                            if let Err(e) = sender.enqueue(frame) {
                                tracing::warn!(
                                    peer = %target,
                                    error = %e,
                                    "Failed to send to peer"
                                );
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

/// Run one handshaken connection until it ends, then tear it down and emit
/// its one `PeerDisconnected` (`facts/p2p-node.md` § Aborting a connection).
/// `inbound_slot` is an inbound peer's admission slot, held until the
/// teardown is done.
async fn run_peer(
    peer_id: PeerId,
    conn: Connection,
    direction: Direction,
    mode: ProxyMode,
    addr: SocketAddr,
    ctx: BackgroundCtx,
    inbound_slot: Option<InboundSlot>,
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

    // Split connection for concurrent read/write
    let (mut reader, mut writer, magic, _, _) = conn.split();

    // The queue exists before the peer is registered or announced, so every
    // PeerId a caller can learn has a queue to send to and to abort.
    let (sender, mut write_rx) = PeerSender::new(peer_id);
    let link = sender.link.clone();
    ctx.peer_senders
        .lock()
        .expect("peer_senders poisoned")
        .insert(peer_id, sender);

    // Register peer in router
    let rest_api_url = spec.rest_api_url();
    let agent_name = Some(spec.agent.clone());
    ctx.router
        .lock()
        .await
        .register_peer(peer_id, direction, mode, addr, rest_api_url, agent_name);

    // Writer task. A write error aborts the connection; otherwise the task
    // runs until the teardown below cancels it.
    let writer_link = link.clone();
    let writer_counters = ctx.counters.clone();
    let writer_tap = ctx.capture_tap.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = write_rx.recv().await {
            let wire_bytes = counters::frame_wire_bytes(&frame);
            // Taken: the frame no longer counts against the queue's byte bound.
            writer_link
                .queued_bytes
                .fetch_sub(wire_bytes as usize, Ordering::Relaxed);
            if let Err(e) = crate::transport::frame::write_frame(
                &mut writer,
                &magic,
                &frame,
                addr,
                writer_tap.as_deref(),
            )
            .await
            {
                tracing::warn!(peer = %peer_id, error = %e, "Write failed");
                writer_link.abort(DisconnectReason::WriteFailed);
                break;
            }
            writer_counters.record_out_frame(&frame, wire_bytes);
        }
    });

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

    // Reader loop. Every exit goes through the abort latch, so the reason is
    // the first one set, whoever set it: an abort racing a read error still
    // ends the loop once, with one reason.
    let aborted = link.aborted();
    tokio::pin!(aborted);
    let reason = loop {
        let read = tokio::select! {
            biased;
            reason = &mut aborted => break reason,
            read = crate::transport::frame::read_frame(
                &mut reader,
                &magic,
                addr,
                &ctx.blacklist,
                ctx.capture_tap.as_deref(),
            ) => read,
        };
        let frame = match read {
            Ok(frame) => frame,
            Err(e) => {
                tracing::info!(peer = %peer_id, error = %e, "Connection lost");
                link.abort(DisconnectReason::ConnectionClosed);
                continue;
            }
        };
        let wire_bytes = counters::frame_wire_bytes(&frame);
        match ProtocolMessage::from_frame(&frame) {
            Ok(msg) => {
                ctx.counters.record_in_message(&msg, wire_bytes);
                let event = ProtocolEvent::Message {
                    peer_id,
                    message: msg,
                };
                // Waiting for room backpressures this peer's socket only.
                tokio::select! {
                    biased;
                    reason = &mut aborted => break reason,
                    sent = ctx.event_tx.send(event) => {
                        if sent.is_err() {
                            // The event loop is gone: the node is shutting down.
                            link.abort(DisconnectReason::Disconnected);
                        }
                    }
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
    };

    // Teardown. Unregistered first, so nothing more is queued.
    ctx.peer_senders
        .lock()
        .expect("peer_senders poisoned")
        .remove(&peer_id);
    if reason != DisconnectReason::ConnectionClosed {
        // An abort resets the connection, as the JVM's `Abort` does. With
        // SO_LINGER 0 the close below sends RST and drops unsent data,
        // instead of waiting on a peer that may never read it.
        let stream: &TcpStream = reader.get_ref().as_ref();
        if let Err(e) = stream.set_zero_linger() {
            tracing::debug!(peer = %peer_id, error = %e, "Setting SO_LINGER 0 failed");
        }
    }
    // Queued frames are discarded, not flushed. Awaiting the cancelled task
    // ensures it has released the write half.
    writer_task.abort();
    let _ = writer_task.await;
    // The last handle on the socket: this closes it.
    drop(reader);

    let _ = ctx
        .event_tx
        .send(ProtocolEvent::PeerDisconnected {
            peer_id,
            reason: reason.as_str().into(),
        })
        .await;

    tracing::info!(peer = %peer_id, reason = reason.as_str(), "Peer removed");
    drop(inbound_slot);
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
                // Handshakes in flight count against the limit as well as
                // registered peers (`facts/p2p-node.md` § Inbound admission).
                let Some(slot) = InboundSlot::reserve(&ctx.inbound_admitted, max_inbound) else {
                    tracing::warn!(
                        peer = %remote_ip,
                        kind = "connection_limit_exceeded",
                        "PENALTY"
                    );
                    // Dropping `stream` closes the connection at once.
                    continue;
                };

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
                            // The slot passes to the registered peer.
                            run_peer(
                                peer_id,
                                conn,
                                Direction::Inbound,
                                mode,
                                addr,
                                ctx,
                                Some(slot),
                            )
                            .await;
                        }
                        Err(e) => {
                            drop(slot);
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
                if let Some(candidate) = pick_fill_candidate(&ctx, &cooldown).await {
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

/// One PeerDb candidate for the fill phase: a uniformly random entry,
/// hearsay included, that is not currently connected, blacklisted, in
/// cooldown, or (when filtering is on) bogus, preferring an address group
/// no connected peer occupies. See `facts/p2p-node.md` § Fill phase and
/// `PeerDb::dial_candidate`.
async fn pick_fill_candidate(
    ctx: &BackgroundCtx,
    cooldown: &HashMap<SocketAddr, Instant>,
) -> Option<SocketAddr> {
    let connected: Vec<SocketAddr> = ctx
        .router
        .lock()
        .await
        .connected_addrs()
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    let exclude: HashSet<SocketAddr> = connected
        .iter()
        .copied()
        .chain(cooldown.keys().copied())
        .collect();

    let db = ctx.peer_db.lock().expect("peer_db poisoned");
    db.dial_candidate(&exclude, &connected, |r| {
        !(ctx.filter_bogus_addresses && is_bogus_address(r.address, ctx.network))
    })
    .map(|r| r.address)
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
                        tracing::info!(peer = %peer_id, "Outbound handshake OK");
                        run_peer(peer_id, conn, Direction::Outbound, mode, addr, ctx, None).await;
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
    use crate::protocol::messages::MessageCode;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn dummy_addr() -> SocketAddr {
        "127.0.0.1:9000".parse().unwrap()
    }

    fn pub_addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// A P2pNode plus the shared state and outbound-request receiver. The
    /// fields are exposed for tests that need to seed the router, observe
    /// queued outbound requests, or record blacklist entries. `ctx` is the
    /// background-task state over the same router, queues and blacklist, for
    /// tests that run `run_peer` or `accept_loop`; `event_rx` receives the
    /// events those emit.
    struct TestHarness {
        node: P2pNode,
        router: Arc<Mutex<Router>>,
        peer_senders: PeerSenders,
        blacklist: Arc<Blacklist>,
        outbound_request_rx: mpsc::Receiver<SocketAddr>,
        ctx: BackgroundCtx,
        event_rx: mpsc::Receiver<ProtocolEvent>,
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
        let router_inner = Router::with_peer_db(
            peer_db.clone(),
            blacklist.clone(),
            64,
            Network::Mainnet,
            true,
        );
        let counters = router_inner.counters();
        let router = Arc::new(Mutex::new(router_inner));
        let peer_senders: PeerSenders = Arc::new(StdMutex::new(HashMap::new()));
        let subscriber = Arc::new(Mutex::new(None));
        let (outbound_request_tx, outbound_request_rx) =
            mpsc::channel::<SocketAddr>(OUTBOUND_REQUEST_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(256);
        let ctx = BackgroundCtx {
            event_tx,
            peer_senders: peer_senders.clone(),
            router: router.clone(),
            peer_counter: Arc::new(AtomicU64::new(1)),
            inbound_admitted: Arc::new(AtomicUsize::new(0)),
            blacklist: blacklist.clone(),
            peer_db: peer_db.clone(),
            network: Network::Mainnet,
            filter_bogus_addresses: true,
            counters: counters.clone(),
            capture_tap: None,
        };
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
            ctx,
            event_rx,
        }
    }

    /// Register a queue for `peer` as `run_peer` does, minus the writer: the
    /// test holds the receiving end, and the link to observe aborts.
    fn add_queue(senders: &PeerSenders, peer: PeerId) -> (mpsc::Receiver<Frame>, Arc<PeerLink>) {
        let (sender, rx) = PeerSender::new(peer);
        let link = sender.link.clone();
        senders.lock().unwrap().insert(peer, sender);
        (rx, link)
    }

    /// A message whose frame body is `len` bytes.
    fn blob(len: usize) -> ProtocolMessage {
        ProtocolMessage::Unknown {
            code: 200,
            body: vec![0u8; len],
        }
    }

    #[test]
    fn queue_bounds_and_reasons_match_the_contract() {
        // `facts/p2p-node.md` § Peer write queues (JVM v6.0.6 PeerConnectionHandler).
        assert_eq!(MAX_QUEUED_FRAMES, 64);
        assert_eq!(MAX_QUEUED_BYTES, 16_388_621);
        // `facts/journal-events.md` § peer_disconnected.
        assert_eq!(
            DisconnectReason::ConnectionClosed.as_str(),
            "connection_closed"
        );
        assert_eq!(DisconnectReason::WriteFailed.as_str(), "write_failed");
        assert_eq!(
            DisconnectReason::OutboundQueueFull.as_str(),
            "outbound_queue_full"
        );
        assert_eq!(DisconnectReason::Disconnected.as_str(), "disconnected");
    }

    #[test]
    fn inbound_slots_are_bounded_and_released_on_drop() {
        let admitted = Arc::new(AtomicUsize::new(0));
        let first = InboundSlot::reserve(&admitted, 2).expect("a free slot");
        let _second = InboundSlot::reserve(&admitted, 2).expect("a free slot");
        assert!(InboundSlot::reserve(&admitted, 2).is_none());
        // The count is shared: a listener with a higher limit still admits.
        let third = InboundSlot::reserve(&admitted, 3).expect("under this listener's limit");
        assert_eq!(admitted.load(Ordering::Relaxed), 3);
        drop(third);
        drop(first);
        assert_eq!(admitted.load(Ordering::Relaxed), 1);
        assert!(InboundSlot::reserve(&admitted, 2).is_some());
    }

    #[tokio::test]
    async fn send_to_delivers_to_correct_peer() {
        let TestHarness {
            node, peer_senders, ..
        } = test_node();
        let peer = PeerId(1);

        let (mut rx, _link) = add_queue(&peer_senders, peer);

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
    async fn send_to_closed_queue_returns_error_and_aborts() {
        let TestHarness {
            node, peer_senders, ..
        } = test_node();
        let peer = PeerId(1);

        let (rx, link) = add_queue(&peer_senders, peer);
        drop(rx); // The writer is gone

        let result = node.send_to(peer, ProtocolMessage::GetPeers).await;
        assert!(matches!(
            result.unwrap_err(),
            SendError::ChannelClosed(PeerId(1))
        ));
        // A closed queue aborts the connection too.
        assert_eq!(link.reason.get(), Some(&DisconnectReason::WriteFailed));
    }

    #[tokio::test]
    async fn send_to_full_queue_returns_promptly_and_aborts() {
        let TestHarness {
            node, peer_senders, ..
        } = test_node();
        let peer = PeerId(1);
        // The writer never takes a frame, so the queue only fills.
        let (_rx, link) = add_queue(&peer_senders, peer);
        for _ in 0..MAX_QUEUED_FRAMES {
            node.send_to(peer, ProtocolMessage::GetPeers).await.unwrap();
        }
        assert!(!link.is_aborted());

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            node.send_to(peer, ProtocolMessage::GetPeers),
        )
        .await
        .expect("send_to never waits for queue space");
        assert!(matches!(result, Err(SendError::QueueFull(PeerId(1)))));
        assert_eq!(
            link.reason.get(),
            Some(&DisconnectReason::OutboundQueueFull)
        );
        // The aborted peer takes nothing more.
        assert!(matches!(
            node.send_to(peer, ProtocolMessage::GetPeers).await,
            Err(SendError::ChannelClosed(PeerId(1)))
        ));
    }

    #[tokio::test]
    async fn send_to_over_the_byte_bound_aborts() {
        let TestHarness {
            node, peer_senders, ..
        } = test_node();
        let peer = PeerId(1);
        let (rx, link) = add_queue(&peer_senders, peer);

        // With their headers, two frames of half the bound exceed it: the
        // second is refused with one frame queued, far below the frame bound.
        let half = MAX_QUEUED_BYTES / 2;
        node.send_to(peer, blob(half)).await.unwrap();
        assert!(matches!(
            node.send_to(peer, blob(half)).await,
            Err(SendError::QueueFull(PeerId(1)))
        ));
        assert_eq!(
            link.reason.get(),
            Some(&DisconnectReason::OutboundQueueFull)
        );
        assert_eq!(rx.len(), 1);
    }

    #[tokio::test]
    async fn the_largest_legal_frame_fits_an_empty_queue() {
        let TestHarness {
            node, peer_senders, ..
        } = test_node();
        let peer = PeerId(1);
        let (_rx, link) = add_queue(&peer_senders, peer);

        // One maximum-size frame is exactly the byte bound.
        let max_body = crate::transport::frame::MAX_BODY_SIZE as usize;
        node.send_to(peer, blob(max_body)).await.unwrap();
        assert!(!link.is_aborted());
        // Behind it, not even an empty-bodied frame fits.
        assert!(matches!(
            node.send_to(peer, ProtocolMessage::GetPeers).await,
            Err(SendError::QueueFull(PeerId(1)))
        ));
        assert_eq!(
            link.reason.get(),
            Some(&DisconnectReason::OutboundQueueFull)
        );
    }

    #[tokio::test]
    async fn broadcast_reaches_inbound_and_outbound_peers() {
        let TestHarness {
            node,
            router,
            peer_senders,
            ..
        } = test_node();

        let outbound_a = PeerId(1);
        let outbound_b = PeerId(2);
        let inbound = PeerId(3);
        for (peer, direction) in [
            (outbound_a, Direction::Outbound),
            (outbound_b, Direction::Outbound),
            (inbound, Direction::Inbound),
        ] {
            router.lock().await.register_peer(
                peer,
                direction,
                ProxyMode::Full,
                dummy_addr(),
                None,
                None,
            );
        }

        let (mut rx_a, _) = add_queue(&peer_senders, outbound_a);
        let (mut rx_b, _) = add_queue(&peer_senders, outbound_b);
        let (mut rx_in, _) = add_queue(&peer_senders, inbound);

        node.broadcast(ProtocolMessage::GetPeers).await;

        // Every connected peer gets one copy, whichever side dialed.
        for rx in [&mut rx_a, &mut rx_b, &mut rx_in] {
            assert_eq!(rx.try_recv().unwrap().code, MessageCode::GET_PEERS);
            assert!(rx.try_recv().is_err(), "exactly one copy");
        }
    }

    #[tokio::test]
    async fn broadcast_aborts_a_full_peer_and_goes_on() {
        let TestHarness {
            node,
            router,
            peer_senders,
            ..
        } = test_node();

        let peer_ok = PeerId(1);
        let peer_full = PeerId(2);
        for (peer, direction) in [
            (peer_ok, Direction::Outbound),
            (peer_full, Direction::Inbound),
        ] {
            router.lock().await.register_peer(
                peer,
                direction,
                ProxyMode::Full,
                dummy_addr(),
                None,
                None,
            );
        }

        let (mut rx_ok, ok_link) = add_queue(&peer_senders, peer_ok);
        // The full peer's writer never takes a frame.
        let (_rx_full, full_link) = add_queue(&peer_senders, peer_full);
        for _ in 0..MAX_QUEUED_FRAMES {
            node.send_to(peer_full, ProtocolMessage::GetPeers)
                .await
                .unwrap();
        }

        tokio::time::timeout(
            Duration::from_secs(1),
            node.broadcast(ProtocolMessage::GetPeers),
        )
        .await
        .expect("broadcast never waits for queue space");

        assert_eq!(
            full_link.reason.get(),
            Some(&DisconnectReason::OutboundQueueFull)
        );
        // The healthy peer still got it.
        assert!(rx_ok.try_recv().is_ok());
        assert!(!ok_link.is_aborted());
    }

    #[tokio::test]
    async fn disconnect_peer_initiates_an_abort() {
        let TestHarness {
            node, peer_senders, ..
        } = test_node();
        let peer = PeerId(1);
        let (_rx, link) = add_queue(&peer_senders, peer);

        node.disconnect_peer(peer).await;
        assert_eq!(link.reason.get(), Some(&DisconnectReason::Disconnected));
        // Only initiated: the peer's own task tears down and unregisters.
        assert!(peer_senders.lock().unwrap().contains_key(&peer));
        // A later abort keeps the first reason.
        assert!(!link.abort(DisconnectReason::ConnectionClosed));
        assert_eq!(link.reason.get(), Some(&DisconnectReason::Disconnected));
        // Unknown peer: no-op.
        node.disconnect_peer(PeerId(99)).await;
    }

    #[tokio::test]
    async fn subscriber_receives_events() {
        let blacklist = Arc::new(Blacklist::new());
        let peer_db = shared_peer_db(blacklist.clone());
        let router_inner = Router::with_peer_db(
            peer_db.clone(),
            blacklist.clone(),
            64,
            Network::Mainnet,
            true,
        );
        let counters = router_inner.counters();
        let router = Arc::new(Mutex::new(router_inner));
        let peer_senders: PeerSenders = Arc::new(StdMutex::new(HashMap::new()));
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
        let (modifier_sink, _modifiers) = mpsc::channel::<ModifierDelivery>(16);
        let handle = tokio::spawn(async move {
            event_loop(event_rx, r, ps, sub, modifier_sink, last).await;
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
        let router_inner = Router::with_peer_db(
            peer_db.clone(),
            blacklist.clone(),
            64,
            Network::Mainnet,
            true,
        );
        let counters = router_inner.counters();
        let router = Arc::new(Mutex::new(router_inner));
        let peer_senders: PeerSenders = Arc::new(StdMutex::new(HashMap::new()));
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
        let (modifier_sink, _modifiers) = mpsc::channel::<ModifierDelivery>(16);
        let handle = tokio::spawn(async move {
            event_loop(event_rx, r, ps, sub, modifier_sink, last).await;
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
        let before = now_ms();

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
        // The outbound registration is a completed handshake with the
        // address we dialed: stamped with the registration time and held
        // by the PeerDb as observed.
        let seen = outbound
            .last_seen_ms
            .expect("outbound entry carries last_seen");
        assert!(before <= seen && seen <= now_ms());
        let rec = node
            .peer_db
            .lock()
            .unwrap()
            .get(pub_addr("203.0.113.40:9030"))
            .cloned()
            .expect("outbound address in PeerDb");
        assert!(rec.is_observed());
        assert_eq!(rec.last_seen_ms, seen);

        let inbound = peers
            .iter()
            .find(|p| p.address == pub_addr("203.0.113.41:9030"))
            .unwrap();
        assert_eq!(inbound.agent_name.as_deref(), Some("nautilus"));
        assert!(matches!(
            inbound.connection_type,
            Some(crate::types::ConnectionType::Incoming)
        ));
        // The inbound socket is a connection-overlay entry only: its
        // last_seen is "now" and nothing was written to the PeerDb.
        let seen = inbound
            .last_seen_ms
            .expect("overlay entry carries last_seen");
        assert!(before <= seen && seen <= now_ms());
        assert!(node
            .peer_db
            .lock()
            .unwrap()
            .get(pub_addr("203.0.113.41:9030"))
            .is_none());
    }

    #[tokio::test]
    async fn all_peers_lists_disconnected_with_no_connection_type() {
        let TestHarness { node, router, .. } = test_node();
        let before = now_ms();

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
        assert_eq!(entry.agent_name.as_deref(), Some("ergoref"));
        // Disconnecting does not demote the entry: it keeps the handshake
        // stamp from registration and stays observed for GetPeers.
        let seen = entry.last_seen_ms.expect("PeerDb entry carries last_seen");
        assert!(before <= seen && seen <= now_ms());
        let rec = node
            .peer_db
            .lock()
            .unwrap()
            .get(addr)
            .cloned()
            .expect("entry survives disconnect");
        assert!(rec.is_observed());
        assert_eq!(rec.last_seen_ms, seen);
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

    // ------------------------------------------------------------------
    // Stuck peers, aborts and inbound admission (`facts/p2p-node.md`)
    // ------------------------------------------------------------------

    /// Register `peer` with the router as a full-mode outbound peer.
    async fn register_outbound(router: &Mutex<Router>, peer: PeerId) {
        router.lock().await.register_peer(
            peer,
            Direction::Outbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );
    }

    /// Run the event loop over the harness state, fed through the returned
    /// sender. The receiver gets what the loop delivers to the modifier sink.
    fn spawn_event_loop(
        h: &TestHarness,
    ) -> (
        mpsc::Sender<ProtocolEvent>,
        mpsc::Receiver<ModifierDelivery>,
    ) {
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);
        let (modifier_sink, modifiers) = mpsc::channel::<ModifierDelivery>(16);
        tokio::spawn(event_loop(
            event_rx,
            h.router.clone(),
            h.peer_senders.clone(),
            h.node.subscriber.clone(),
            modifier_sink,
            h.node.last_incoming_ms.clone(),
        ));
        (event_tx, modifiers)
    }

    fn message(peer_id: PeerId, message: ProtocolMessage) -> ProtocolEvent {
        ProtocolEvent::Message { peer_id, message }
    }

    #[tokio::test]
    async fn event_loop_serves_on_while_a_peer_is_stuck_at_the_frame_bound() {
        let h = test_node();
        let (stuck, healthy) = (PeerId(1), PeerId(2));
        register_outbound(&h.router, stuck).await;
        register_outbound(&h.router, healthy).await;
        // The stuck peer's writer never takes a frame.
        let (stuck_rx, stuck_link) = add_queue(&h.peer_senders, stuck);
        let (mut healthy_rx, _) = add_queue(&h.peer_senders, healthy);
        let (event_tx, _modifiers) = spawn_event_loop(&h);

        tokio::time::timeout(Duration::from_secs(5), async {
            // Each GetPeers from the stuck peer queues a Peers reply to it:
            // one reply more than the frame bound.
            for _ in 0..=MAX_QUEUED_FRAMES {
                event_tx
                    .send(message(stuck, ProtocolMessage::GetPeers))
                    .await
                    .unwrap();
            }
            // The other peer is still served.
            event_tx
                .send(message(healthy, ProtocolMessage::GetPeers))
                .await
                .unwrap();
            let reply = healthy_rx.recv().await.unwrap();
            assert_eq!(reply.code, MessageCode::PEERS);
        })
        .await
        .expect("the event loop waited on a stuck peer");

        assert_eq!(
            stuck_link.reason.get(),
            Some(&DisconnectReason::OutboundQueueFull)
        );
        assert_eq!(stuck_rx.len(), MAX_QUEUED_FRAMES);
    }

    #[tokio::test]
    async fn event_loop_serves_on_while_a_peer_is_stuck_at_the_byte_bound() {
        let h = test_node();
        let (stuck, healthy) = (PeerId(1), PeerId(2));
        register_outbound(&h.router, stuck).await;
        register_outbound(&h.router, healthy).await;
        // Every requested modifier is served locally, each in a response of
        // its own, of half the byte bound.
        h.router
            .lock()
            .await
            .set_local_serve(Arc::new(|_, _| Some(vec![0u8; MAX_QUEUED_BYTES / 2])));
        let (stuck_rx, stuck_link) = add_queue(&h.peer_senders, stuck);
        let (mut healthy_rx, _) = add_queue(&h.peer_senders, healthy);
        let (event_tx, _modifiers) = spawn_event_loop(&h);

        tokio::time::timeout(Duration::from_secs(5), async {
            // Two responses to the stuck peer, which together exceed the
            // byte bound.
            let request = ProtocolMessage::ModifierRequest {
                modifier_type: 102,
                ids: vec![[1u8; 32], [2u8; 32]],
            };
            event_tx.send(message(stuck, request)).await.unwrap();
            // The other peer is still served.
            event_tx
                .send(message(healthy, ProtocolMessage::GetPeers))
                .await
                .unwrap();
            let reply = healthy_rx.recv().await.unwrap();
            assert_eq!(reply.code, MessageCode::PEERS);
        })
        .await
        .expect("the event loop waited on a stuck peer");

        assert_eq!(
            stuck_link.reason.get(),
            Some(&DisconnectReason::OutboundQueueFull)
        );
        // One response queued: the byte bound refused the second, not the
        // frame bound.
        assert_eq!(stuck_rx.len(), 1);
    }

    #[tokio::test]
    async fn event_loop_delivers_received_modifiers_to_the_sink() {
        let h = test_node();
        let (source, other) = (PeerId(1), PeerId(2));
        register_outbound(&h.router, source).await;
        register_outbound(&h.router, other).await;
        let (mut source_rx, _) = add_queue(&h.peer_senders, source);
        let (mut other_rx, _) = add_queue(&h.peer_senders, other);
        let (event_tx, mut modifiers) = spawn_event_loop(&h);

        let response = ProtocolMessage::ModifierResponse {
            modifier_type: 102,
            modifiers: vec![([7u8; 32], vec![1, 2, 3]), ([8u8; 32], vec![4, 5])],
        };
        event_tx.send(message(source, response)).await.unwrap();
        for expected in [
            (102, [7u8; 32], vec![1, 2, 3], Some(source.0)),
            (102, [8u8; 32], vec![4, 5], Some(source.0)),
        ] {
            let delivered = tokio::time::timeout(Duration::from_secs(5), modifiers.recv())
                .await
                .expect("a delivery within 5 s")
                .expect("sink open");
            assert_eq!(delivered, expected);
        }

        // Validation is the only destination. Events are handled in order,
        // so the reply to this later GetPeers is the first frame `other`
        // gets unless the response sent it something.
        event_tx
            .send(message(other, ProtocolMessage::GetPeers))
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), other_rx.recv())
            .await
            .expect("a reply within 5 s")
            .unwrap();
        assert_eq!(reply.code, MessageCode::PEERS);
        assert!(source_rx.try_recv().is_err(), "nothing sent back");
    }

    /// Our side's handshake settings.
    fn local_handshake() -> HandshakeConfig {
        HandshakeConfig {
            agent_name: "ergo-node-rust".into(),
            peer_name: "local".into(),
            version: Version::new(6, 0, 3),
            network: Network::Mainnet,
            mode: ProxyMode::Full,
            declared_address: None,
            mode_config: handshake::ModeConfig::default(),
        }
    }

    /// The handshake a remote peer opens with.
    fn remote_handshake() -> Vec<u8> {
        handshake::build(&HandshakeConfig {
            agent_name: "ergoref".into(),
            peer_name: "remote".into(),
            ..local_handshake()
        })
    }

    /// A localhost connection past the handshake, our end run as `peer` by
    /// `run_peer` on its own task. Returns the remote end as a raw socket,
    /// which has not read our handshake.
    async fn run_remote_peer(
        ctx: &BackgroundCtx,
        peer: PeerId,
    ) -> (TcpStream, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut remote = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, addr) = listener.accept().await.unwrap();
        remote.write_all(&remote_handshake()).await.unwrap();
        let conn = Connection::inbound(stream, &local_handshake(), &ctx.counters)
            .await
            .unwrap();
        let task = tokio::spawn(run_peer(
            peer,
            conn,
            Direction::Inbound,
            ProxyMode::Full,
            addr,
            ctx.clone(),
            None,
        ));
        (remote, task)
    }

    async fn next_event(events: &mut mpsc::Receiver<ProtocolEvent>) -> ProtocolEvent {
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event within 5 s")
            .expect("event channel open")
    }

    async fn expect_connected(events: &mut mpsc::Receiver<ProtocolEvent>) -> PeerId {
        match next_event(events).await {
            ProtocolEvent::PeerConnected { peer_id, .. } => peer_id,
            other => panic!("expected PeerConnected, got {other:?}"),
        }
    }

    /// The `reason` of every `PeerDisconnected` for `peer` queued so far.
    fn disconnect_reasons(events: &mut mpsc::Receiver<ProtocolEvent>, peer: PeerId) -> Vec<String> {
        let mut reasons = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let ProtocolEvent::PeerDisconnected { peer_id, reason } = event {
                if peer_id == peer {
                    reasons.push(reason);
                }
            }
        }
        reasons
    }

    /// Read the remote end until the connection ends, and say how it ended.
    async fn read_to_close(remote: &mut TcpStream) -> std::io::Result<()> {
        let mut buf = vec![0u8; 1 << 16];
        let ended = async {
            while remote.read(&mut buf).await? > 0 {}
            Ok::<(), std::io::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .expect("the connection ends within 5 s")
    }

    /// Poll `cond` until it holds; panics after 5 s.
    async fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting until {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Wait until `link`'s writer has stopped taking frames with some still
    /// queued: it is blocked writing to a remote that does not read.
    async fn wait_for_stuck_writer(link: &PeerLink) {
        let queued = || link.queued_bytes.load(Ordering::Relaxed);
        let initial = queued();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut last = initial;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let now = queued();
            if now == last && 0 < now && now < initial {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the writer never got stuck"
            );
            last = now;
        }
    }

    #[tokio::test]
    async fn disconnect_peer_resets_a_remote_that_never_reads() {
        let TestHarness {
            node,
            peer_senders,
            ctx,
            mut event_rx,
            ..
        } = test_node();
        let peer = PeerId(1);
        let (mut remote, task) = run_remote_peer(&ctx, peer).await;
        assert_eq!(expect_connected(&mut event_rx).await, peer);

        // More than the socket buffers hold: the writer ends up blocked
        // mid-frame on a remote that never reads, with frames still queued,
        // and the reader on a remote that never sends.
        for _ in 0..12 {
            node.send_to(peer, blob(1 << 20)).await.unwrap();
        }
        let link = peer_senders.lock().unwrap()[&peer].link.clone();
        wait_for_stuck_writer(&link).await;

        node.disconnect_peer(peer).await;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the reader stops and the peer's task ends")
            .unwrap();

        assert_eq!(disconnect_reasons(&mut event_rx, peer), ["disconnected"]);
        assert!(!peer_senders.lock().unwrap().contains_key(&peer));
        // A reset, not an orderly close.
        let ended = read_to_close(&mut remote).await;
        assert_eq!(
            ended.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionReset
        );
    }

    #[tokio::test]
    async fn a_remote_that_never_reads_is_aborted_when_its_queue_fills() {
        let TestHarness {
            node,
            ctx,
            mut event_rx,
            ..
        } = test_node();
        let peer = PeerId(1);
        let (mut remote, task) = run_remote_peer(&ctx, peer).await;
        assert_eq!(expect_connected(&mut event_rx).await, peer);

        // Keep queueing, giving the writer its turn in between, until the
        // queue refuses a frame: by then the writer is stuck on the remote.
        let mut refused = None;
        for _ in 0..64 {
            if let Err(e) = node.send_to(peer, blob(1 << 20)).await {
                refused = Some(e);
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            matches!(refused, Some(SendError::QueueFull(p)) if p == peer),
            "{refused:?}"
        );

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the aborted peer's task ends")
            .unwrap();
        assert_eq!(
            disconnect_reasons(&mut event_rx, peer),
            ["outbound_queue_full"]
        );
        let ended = read_to_close(&mut remote).await;
        assert_eq!(
            ended.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionReset
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abort_racing_a_read_error_emits_one_disconnect() {
        for round in 0..20 {
            let TestHarness {
                node,
                ctx,
                mut event_rx,
                ..
            } = test_node();
            let peer = PeerId(1);
            let (remote, task) = run_remote_peer(&ctx, peer).await;
            assert_eq!(expect_connected(&mut event_rx).await, peer);

            // The remote leaves with our handshake unread, so its close is a
            // reset, just as the node aborts.
            let leave = tokio::spawn(async move { drop(remote) });
            node.disconnect_peer(peer).await;
            leave.await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("the peer's task ends")
                .unwrap();

            let reasons = disconnect_reasons(&mut event_rx, peer);
            assert_eq!(reasons.len(), 1, "round {round}: {reasons:?}");
            assert!(
                ["disconnected", "connection_closed"].contains(&reasons[0].as_str()),
                "round {round}: {reasons:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_frame_stops_counting_once_the_writer_takes_it() {
        let TestHarness {
            node,
            peer_senders,
            ctx,
            mut event_rx,
            ..
        } = test_node();
        let peer = PeerId(1);
        let (mut remote, _task) = run_remote_peer(&ctx, peer).await;
        assert_eq!(expect_connected(&mut event_rx).await, peer);
        let received = Arc::new(AtomicUsize::new(0));
        let counted = received.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                match remote.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        counted.fetch_add(n, Ordering::Relaxed);
                    }
                }
            }
        });

        // Two frames of half the byte bound never fit in the queue together
        // (`send_to_over_the_byte_bound_aborts`). Once the writer has taken
        // the first, the second fits.
        let half = MAX_QUEUED_BYTES / 2;
        node.send_to(peer, blob(half)).await.unwrap();
        // Frame bytes reach the remote only after the writer took the frame;
        // our handshake is well under 1 KiB.
        wait_until("the remote receives the first frame", || {
            received.load(Ordering::Relaxed) > 1024
        })
        .await;
        node.send_to(peer, blob(half)).await.unwrap();
        let link = peer_senders.lock().unwrap()[&peer].link.clone();
        assert!(!link.is_aborted());
    }

    #[tokio::test]
    async fn an_outbound_peer_advertising_an_unknown_feature_connects() {
        let TestHarness {
            router,
            ctx,
            mut event_rx,
            ..
        } = test_node();
        // A valid remote handshake plus feature 64, which this node does not
        // know. The feature is kept, not interpreted (`facts/p2p-transport.md`
        // § `parse`).
        let unknown = handshake::Feature {
            id: 64,
            body: vec![],
        };
        let mut spec = handshake::parse(&remote_handshake()).unwrap();
        spec.features.push(unknown.clone());
        let mut remote_bytes = Vec::new();
        crate::transport::vlq::write_vlq(&mut remote_bytes, now_ms());
        handshake::serialize_peer_entry(&spec, &mut remote_bytes);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let remote = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(&remote_bytes).await.unwrap();
            stream
        });
        spawn_outbound_connect(target, &local_handshake(), ProxyMode::Full, ctx.clone()).await;
        // Held open until the end, so the connection stays up.
        let _remote = remote.await.unwrap();

        match next_event(&mut event_rx).await {
            ProtocolEvent::PeerConnected {
                peer_id,
                spec,
                direction,
                ..
            } => {
                assert_eq!(direction, Direction::Outbound);
                assert!(spec.features.contains(&unknown), "{:?}", spec.features);
                assert_eq!(router.lock().await.outbound_peers(), vec![peer_id]);
            }
            other => panic!("expected PeerConnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inbound_admission_counts_handshakes_in_flight() {
        let TestHarness { ctx, .. } = test_node();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        tokio::spawn(accept_loop(
            listener,
            local_handshake(),
            ProxyMode::Full,
            2,
            ctx.clone(),
        ));
        let admitted = || ctx.inbound_admitted.load(Ordering::Relaxed);

        // Two connections that never send a handshake hold both slots.
        let first = TcpStream::connect(target).await.unwrap();
        let _second = TcpStream::connect(target).await.unwrap();
        wait_until("both are admitted", || admitted() == 2).await;

        // A third is closed at once, not after the 30 s handshake timeout.
        let mut third = TcpStream::connect(target).await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(2), third.read(&mut [0u8; 1]))
            .await
            .expect("refused at once");
        assert_eq!(read.unwrap(), 0);

        // Closing one of the two fails its handshake, which frees its slot.
        drop(first);
        wait_until("the closed one's slot is released", || admitted() == 1).await;
        let mut fourth = TcpStream::connect(target).await.unwrap();
        wait_until("the fourth is admitted", || admitted() == 2).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), fourth.read(&mut [0u8; 1]))
                .await
                .is_err(),
            "an admitted connection is not closed"
        );
    }

    #[tokio::test]
    async fn an_inbound_slot_passes_to_the_registered_peer() {
        let TestHarness {
            node,
            ctx,
            mut event_rx,
            ..
        } = test_node();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        tokio::spawn(accept_loop(
            listener,
            local_handshake(),
            ProxyMode::Full,
            1,
            ctx.clone(),
        ));
        let admitted = || ctx.inbound_admitted.load(Ordering::Relaxed);

        let mut first = TcpStream::connect(target).await.unwrap();
        first.write_all(&remote_handshake()).await.unwrap();
        let peer = expect_connected(&mut event_rx).await;
        // Registered, and still holding the one slot: another connection is
        // refused.
        assert_eq!(admitted(), 1);
        let mut second = TcpStream::connect(target).await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(2), second.read(&mut [0u8; 1]))
            .await
            .expect("refused at once");
        assert_eq!(read.unwrap(), 0);

        // The peer's disconnect releases it.
        node.disconnect_peer(peer).await;
        match next_event(&mut event_rx).await {
            ProtocolEvent::PeerDisconnected { peer_id, .. } if peer_id == peer => {}
            other => panic!("expected PeerDisconnected, got {other:?}"),
        }
        wait_until("the peer's slot is released", || admitted() == 0).await;
        let _third = TcpStream::connect(target).await.unwrap();
        wait_until("the third is admitted", || admitted() == 1).await;
    }
}

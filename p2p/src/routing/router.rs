//! Message routing: what the node does with each message a peer sends.
//!
//! The router answers peer gossip and locally served modifier requests,
//! records gossiped peers, and emits received modifiers for validation. It
//! never forwards a message from one peer to another. Everything else reaches
//! its consumer through the event subscriber (`facts/p2p-node.md`).
//!
//! # Contract (`facts/p2p-routing.md`)
//! - `handle_event`: given a `ProtocolEvent`, returns a list of `Action`s.
//!   Precondition: peer IDs in events are registered (or being disconnected).
//!   Postcondition: actions target only registered peers, and every
//!   `Action::Send` targets the source of the message being handled.
//! - `register_peer` / peer removal on disconnect: manage the peer registry.
//! - Invariant: the router's only per-peer state is the peer registry, and
//!   the registry holds exactly the registered peers.
//! - PeerDb is the canonical store of "addresses we know about"; see
//!   `facts/p2p-peerdb.md`.

use crate::blacklist::Blacklist;
use crate::peer_db::{MemoryPeerStorage, PeerDb, PeerRecord, PeerStorage};
use crate::protocol::address_sanity::is_bogus_address;
use crate::protocol::counters::{TrafficCounters, TrafficSnapshot};
use crate::protocol::messages::{build_peers_body, parse_peers_body, ProtocolMessage};
use crate::protocol::peer::ProtocolEvent;
use crate::transport::handshake::PeerSpec;
use crate::types::{ConnectionType, Direction, ModifierId, Network, PeerId, ProxyMode};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// JVM's `PeerSynchronizer.gossipPeers` sends `max/8` peers when the
/// cap is >= 16, matching its post-5.0.8 convention. With our default
/// cap of 64, that's 8.
const PEERS_PER_GOSSIP_DIVISOR: usize = 8;
const PEERS_PER_GOSSIP_MIN_CAP: usize = 16;

/// Store-blind local-serve hook that answers the ModifierRequest arm:
/// `(modifier_type, id)` → `Some(bytes)` when the integrator has the
/// modifier, `None` otherwise. A miss gets no answer. See
/// `facts/p2p-routing.md`.
pub type LocalServeFn = Arc<dyn Fn(u8, &[u8; 32]) -> Option<Vec<u8>> + Send + Sync>;

/// Serve-side cap on one encoded `ModifierResponse` body, mirroring JVM
/// `ModifiersSpec.maxMessageSize` (2_048_576). Kept below the frame
/// layer's `MAX_BODY_SIZE` (2 MiB) so a served batch is never rejected
/// by a symmetric (rust) peer's frame read cap.
const MAX_SERVE_BATCH_BYTES: usize = 2_048_576;
/// Conservative per-entry encoding overhead in a `ModifierResponse`
/// body: 32-byte id + ≤5-byte VLQ data length.
const SERVE_ENTRY_OVERHEAD: usize = 37;
/// Conservative body header overhead: 1 type byte + ≤2-byte VLQ count
/// (count is parse-capped at `MAX_INV_OBJECTS` = 400).
const SERVE_HEADER_OVERHEAD: usize = 3;

/// Block-component modifier types (JVM `NetworkObjectTypeId`):
/// Header=101, BlockTransactions=102, ADProofs=104, Extension=108.
/// Transaction=2 is mempool gossip, not block data: a Light listener's
/// request for it goes to the hook like any other. Same values as
/// enr-chain's `*_TYPE_ID` constants; restated here because p2p sits
/// below enr-chain in the layering.
fn is_block_related(modifier_type: u8) -> bool {
    matches!(modifier_type, 101 | 102 | 104 | 108)
}

/// Split locally-served modifiers into batches whose encoded
/// `ModifierResponse` bodies stay under [`MAX_SERVE_BATCH_BYTES`]. A
/// single modifier that alone exceeds the cap is shipped anyway in its
/// own batch — JVM `sendByParts` parity (it warns and sends it).
fn chunk_served(served: Vec<(ModifierId, Vec<u8>)>) -> Vec<Vec<(ModifierId, Vec<u8>)>> {
    let mut batches = Vec::new();
    let mut batch: Vec<(ModifierId, Vec<u8>)> = Vec::new();
    let mut size = SERVE_HEADER_OVERHEAD;
    for (id, data) in served {
        let entry = SERVE_ENTRY_OVERHEAD + data.len();
        if !batch.is_empty() && size + entry > MAX_SERVE_BATCH_BYTES {
            batches.push(std::mem::take(&mut batch));
            size = SERVE_HEADER_OVERHEAD;
        }
        if batch.is_empty() && size + entry > MAX_SERVE_BATCH_BYTES {
            tracing::warn!(
                data_len = data.len(),
                "serving over-cap modifier alone (JVM sendByParts parity)"
            );
        }
        size += entry;
        batch.push((id, data));
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A routing directive.
#[derive(Debug)]
pub enum Action {
    /// Send `message` to `target`, which is always the peer whose message
    /// the router was handling: the router answers, it never forwards.
    Send {
        target: PeerId,
        message: ProtocolMessage,
    },
    /// Hand modifier data to the async validation pipeline, attributed to
    /// the peer that sent it.
    Validate {
        modifier_type: u8,
        id: [u8; 32],
        data: Vec<u8>,
        peer_id: PeerId,
    },
}

struct PeerEntry {
    direction: Direction,
    mode: ProxyMode,
    addr: SocketAddr,
    rest_api_url: Option<String>,
    agent_name: Option<String>,
}

/// Snapshot of one currently-connected peer for `P2pNode::all_peers`.
#[derive(Debug, Clone)]
pub struct ConnectedPeerSummary {
    pub address: SocketAddr,
    pub direction: Direction,
    pub agent_name: Option<String>,
}

pub struct Router {
    /// The registered peers: the router's only per-peer state.
    peers: HashMap<PeerId, PeerEntry>,

    /// Shared peer database. Populated by the PeerConnected event arm,
    /// the Peers gossip arm, and `register_peer`. Read by the outbound
    /// manager's fill phase and by `P2pNode::all_peers`.
    peer_db: Arc<StdMutex<PeerDb>>,
    /// Shared blacklist. Used by the GetPeers / Peers / PeerConnected
    /// arms to filter and to permanently ban senders of malformed
    /// Peers messages.
    blacklist: Arc<Blacklist>,
    /// Cap on the `length` field of an inbound `Peers` body. Above this,
    /// the parser rejects and the source is permanently banned. Mirrors
    /// the JVM `NetworkSettings.maxPeerSpecObjects` (default 64).
    max_peer_spec_objects: usize,
    /// Network this router is operating on. Gates the network-conditional
    /// classes in `is_bogus_address` (private/CGN/ULA/documentation ranges
    /// are filtered on mainnet but allowed on testnet for LAN setups).
    network: Network,
    /// When `true`, bogus addresses are dropped from `Peers` intake and
    /// `GetPeers` response selection (JVM 6.0.3 parity). When `false`, no
    /// address-sanity filtering: every syntactically-valid address is
    /// ingested and gossiped onward. Never affects the malformed-Peers ban
    /// (a real protocol violation) or the self-address filter. See
    /// `facts/p2p-routing.md`.
    filter_bogus_addresses: bool,
    /// Cumulative traffic counters shared with peer tasks. Incremented
    /// at the framing boundary on every inbound parsed message and every
    /// outbound serialized frame. Exposed to operators via
    /// [`Router::traffic_snapshot`].
    counters: Arc<TrafficCounters>,
    /// Local-serve hook that answers ModifierRequests (see
    /// `facts/p2p-routing.md`). `None` (the construction default) means
    /// no hook, so every request goes unanswered; the integrator wires
    /// it via [`Router::set_local_serve`].
    local_serve: Option<LocalServeFn>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new(Network::Mainnet)
    }
}

impl Router {
    /// Construct a router with an internal default `PeerDb` and a
    /// fresh `Blacklist`. Useful for tests; production should use
    /// [`Router::with_peer_db`] so the PeerDb is shared with the
    /// outbound manager.
    pub fn new(network: Network) -> Self {
        let blacklist = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let peer_db = PeerDb::new(
            storage,
            blacklist.clone(),
            crate::peer_db::DEFAULT_CAP,
            HashSet::new(),
        )
        .expect("MemoryPeerStorage::load_all is infallible");
        Self::with_peer_db(
            Arc::new(StdMutex::new(peer_db)),
            blacklist,
            64,
            network,
            true,
        )
    }

    /// Construct a router with an externally-owned PeerDb + Blacklist.
    /// `max_peer_spec_objects` caps inbound `Peers` bodies.
    /// `filter_bogus_addresses` gates address-sanity filtering on `Peers`
    /// intake and `GetPeers` responses (see `facts/p2p-routing.md`).
    pub fn with_peer_db(
        peer_db: Arc<StdMutex<PeerDb>>,
        blacklist: Arc<Blacklist>,
        max_peer_spec_objects: usize,
        network: Network,
        filter_bogus_addresses: bool,
    ) -> Self {
        Self {
            peers: HashMap::new(),
            peer_db,
            blacklist,
            max_peer_spec_objects,
            network,
            filter_bogus_addresses,
            counters: Arc::new(TrafficCounters::new()),
            local_serve: None,
        }
    }

    /// Install the local-serve hook that answers the ModifierRequest arm
    /// (see `facts/p2p-routing.md`). Store-blind: the integrator wires it
    /// to what the node has. Called synchronously on the routing path —
    /// keep it to cheap single-key lookups; cost per message is bounded by
    /// the parse-layer object cap (400 ids).
    pub fn set_local_serve(&mut self, serve: LocalServeFn) {
        self.local_serve = Some(serve);
    }

    /// Clone of the shared counter store. Hand this to peer tasks so
    /// they can record traffic at the framing boundary.
    pub fn counters(&self) -> Arc<TrafficCounters> {
        self.counters.clone()
    }

    /// Plain-data snapshot of the cumulative traffic counters since
    /// process start. Intended for the api crate's `/stats/p2p`
    /// adapter; see `facts/stats.md`.
    pub fn traffic_snapshot(&self) -> TrafficSnapshot {
        self.counters.snapshot()
    }

    pub fn register_peer(
        &mut self,
        peer_id: PeerId,
        direction: Direction,
        mode: ProxyMode,
        addr: SocketAddr,
        rest_api_url: Option<String>,
        agent_name: Option<String>,
    ) {
        self.peers.insert(
            peer_id,
            PeerEntry {
                direction,
                mode,
                addr,
                rest_api_url,
                agent_name: agent_name.clone(),
            },
        );
        // Seed the PeerDb for outbound peers. `addr` is the address we
        // dialed and the handshake has completed, so this is an
        // observation in its own right (`facts/p2p-peerdb.md` § Observed
        // and hearsay). It also gives the register-only test path (no
        // event loop) an entry without driving the PeerConnected event;
        // production fills in the full handshake spec via PeerConnected,
        // both timestamps merging by max.
        //
        // Inbound peers are excluded: their observed socket is the
        // peer's ephemeral outgoing port, not a listening address worth
        // gossiping. Their listening address (declared in the
        // handshake) is recorded by PeerConnected.
        if direction == Direction::Outbound && !self.blacklist.contains(addr) {
            let now = now_ms();
            let mut db = self.peer_db.lock().expect("peer_db poisoned");
            db.record(PeerRecord {
                address: addr,
                last_seen_ms: now,
                last_handshake_ms: now,
                agent_name: agent_name.unwrap_or_default(),
                node_name: String::new(),
                version: (0, 0, 0),
                features: vec![],
            });
        }
    }

    /// Whether any currently-connected peer is bound to `addr`.
    pub fn is_addr_connected(&self, addr: SocketAddr) -> bool {
        self.peers.values().any(|p| p.addr == addr)
    }

    /// Per-connection summary for [`P2pNode::all_peers`]. The caller
    /// merges this with the PeerDb snapshot.
    pub fn connected_summary(&self) -> Vec<ConnectedPeerSummary> {
        self.peers
            .values()
            .map(|e| ConnectedPeerSummary {
                address: e.addr,
                direction: e.direction,
                agent_name: e.agent_name.clone(),
            })
            .collect()
    }

    /// Currently-connected addresses, by direction. Used by the
    /// outbound manager's fill phase to build its exclude set.
    pub fn connected_addrs(&self) -> Vec<(SocketAddr, ConnectionType)> {
        self.peers
            .values()
            .map(|e| (e.addr, ConnectionType::from(e.direction)))
            .collect()
    }

    pub fn peer_addr(&self, peer_id: PeerId) -> Option<SocketAddr> {
        self.peers.get(&peer_id).map(|e| e.addr)
    }

    /// REST API URLs for all connected peers.
    pub fn peer_rest_urls(&self) -> Vec<(PeerId, SocketAddr, Option<String>)> {
        self.peers
            .iter()
            .map(|(pid, entry)| (*pid, entry.addr, entry.rest_api_url.clone()))
            .collect()
    }

    pub fn handle_event(&mut self, event: ProtocolEvent) -> Vec<Action> {
        match event {
            ProtocolEvent::PeerConnected {
                spec,
                direction,
                addr,
                ..
            } => {
                self.record_peer_connected(&spec, direction, addr);
                vec![]
            }

            ProtocolEvent::PeerDisconnected { peer_id, .. } => {
                self.peers.remove(&peer_id);
                vec![]
            }

            ProtocolEvent::Message { peer_id, message } => self.route_message(peer_id, message),
        }
    }

    /// Record what a completed handshake proves (`facts/p2p-peerdb.md`
    /// § Observed and hearsay). `remote_addr` is the connection's remote
    /// socket: the address we dialed for an outbound peer, an ephemeral
    /// port for an inbound one.
    ///
    /// Outbound: the dialed address listens, so it is observed, always.
    /// The declared address is a port claim on that same host (observed)
    /// or a claim about another host (hearsay). Inbound: the connection
    /// proves only its remote IP, so the declared address is observed on
    /// that IP and hearsay on any other; with no declared address there
    /// is nothing to record, since the remote socket is not a listening
    /// address.
    fn record_peer_connected(
        &self,
        spec: &PeerSpec,
        direction: Direction,
        remote_addr: SocketAddr,
    ) {
        let same_host = |declared: SocketAddr| declared.ip() == remote_addr.ip();
        // (address, observed)
        let mut to_record: Vec<(SocketAddr, bool)> = Vec::with_capacity(2);
        match direction {
            Direction::Outbound => {
                to_record.push((remote_addr, true));
                if let Some(declared) = spec.address.filter(|d| *d != remote_addr) {
                    to_record.push((declared, same_host(declared)));
                }
            }
            Direction::Inbound => {
                if let Some(declared) = spec.address {
                    to_record.push((declared, same_host(declared)));
                }
            }
        }
        if to_record.is_empty() {
            return;
        }
        let now = now_ms();
        let mut db = self.peer_db.lock().expect("peer_db poisoned");
        for (address, observed) in to_record {
            if self.blacklist.contains(address) {
                continue;
            }
            db.record(PeerRecord {
                address,
                last_seen_ms: now,
                last_handshake_ms: if observed { now } else { 0 },
                agent_name: spec.agent.clone(),
                node_name: spec.name.clone(),
                version: (spec.version.major, spec.version.minor, spec.version.patch),
                features: spec
                    .features
                    .iter()
                    .map(|f| (f.id, f.body.clone()))
                    .collect(),
            });
        }
    }

    /// The router's answer to one message from `source`: a reply to the
    /// source, modifiers for validation, or nothing. Never a message to
    /// another peer (`facts/p2p-routing.md`).
    fn route_message(&self, source: PeerId, message: ProtocolMessage) -> Vec<Action> {
        let source_entry = match self.peers.get(&source) {
            Some(e) => e,
            None => return vec![],
        };
        let source_mode = source_entry.mode;
        let source_addr = source_entry.addr;

        let actions = match message {
            // Sync and the mempool task read these from the subscriber.
            ProtocolMessage::Inv { .. } | ProtocolMessage::SyncInfo { .. } => vec![],

            ProtocolMessage::ModifierRequest { modifier_type, ids } => {
                // Light listeners are gossip-only: a block-related request
                // gets nothing, and the hook is not asked.
                if source_mode == ProxyMode::Light && is_block_related(modifier_type) {
                    vec![]
                } else {
                    // The hits go back to the source, grouped into as few
                    // responses as the serve batch cap allows. A miss gets
                    // no answer, as in the JVM, whose
                    // `ErgoNodeViewSynchronizer.modifiersReq` serves what it
                    // has and ignores the rest: the requester asks another
                    // peer. The parse-layer object cap (`MAX_INV_OBJECTS`)
                    // bounds the hook's cost at 400 lookups per request.
                    let served: Vec<(ModifierId, Vec<u8>)> = match &self.local_serve {
                        Some(serve) => ids
                            .iter()
                            .filter_map(|id| serve(modifier_type, id).map(|data| (*id, data)))
                            .collect(),
                        None => Vec::new(),
                    };
                    // The request parse cap bounds `ids`, so served batches
                    // can never exceed the response object cap.
                    debug_assert!(served.len() <= crate::protocol::messages::MAX_INV_OBJECTS);
                    chunk_served(served)
                        .into_iter()
                        .map(|modifiers| Action::Send {
                            target: source,
                            message: ProtocolMessage::ModifierResponse {
                                modifier_type,
                                modifiers,
                            },
                        })
                        .collect()
                }
            }

            ProtocolMessage::ModifierResponse {
                modifier_type,
                modifiers,
            } => {
                if modifier_type != 101 {
                    tracing::debug!(
                        modifier_type,
                        count = modifiers.len(),
                        "routing non-header ModifierResponse"
                    );
                }
                // Every modifier goes to validation, attributed to the
                // source. Nothing is sent back or onward.
                modifiers
                    .into_iter()
                    .map(|(id, data)| Action::Validate {
                        modifier_type,
                        id,
                        data,
                        peer_id: source,
                    })
                    .collect()
            }

            ProtocolMessage::GetPeers => {
                let limit = self.peers_to_send();
                let mut exclude: HashSet<SocketAddr> =
                    self.peers.values().map(|p| p.addr).collect();
                exclude.insert(source_addr);

                // Observed peers only: an address we have merely heard of
                // is never vouched for to a third party
                // (`facts/p2p-peerdb.md` § Observed and hearsay).
                let specs: Vec<PeerSpec> = {
                    let db = self.peer_db.lock().expect("peer_db poisoned");
                    db.observed(limit, &exclude)
                        .into_iter()
                        .filter(|r| {
                            !(self.filter_bogus_addresses
                                && is_bogus_address(r.address, self.network))
                        })
                        .map(record_to_spec)
                        .collect()
                };
                let body = build_peers_body(&specs);
                vec![Action::Send {
                    target: source,
                    message: ProtocolMessage::Peers { body },
                }]
            }

            ProtocolMessage::Peers { body } => {
                match parse_peers_body(&body, self.max_peer_spec_objects) {
                    Ok(specs) => {
                        let mut db = self.peer_db.lock().expect("peer_db poisoned");
                        for spec in specs {
                            let Some(addr) = spec.address else { continue };
                            // Bogus addresses are silently dropped when
                            // filtering is enabled (JVM 6.0.3 parity) — the
                            // gossiper is NOT penalized. Relaying a peer list
                            // that contains CGNAT/private addresses is normal
                            // on a NAT'd network, not misbehavior. JVM
                            // PeerSynchronizer.addNewPeers → AddPeerIfEmpty
                            // never penalizes here. Non-bogus entries from the
                            // same body are recorded regardless.
                            if self.filter_bogus_addresses && is_bogus_address(addr, self.network) {
                                continue;
                            }
                            if self.blacklist.contains(addr) {
                                continue;
                            }
                            // Hearsay: a third party's claim that this
                            // address exists. `last_seen_ms` stamps the
                            // mention; the handshake stamp is left as it
                            // was (0 for a new entry).
                            db.record(PeerRecord {
                                address: addr,
                                last_seen_ms: now_ms(),
                                last_handshake_ms: 0,
                                agent_name: spec.agent.clone(),
                                node_name: spec.name.clone(),
                                version: (
                                    spec.version.major,
                                    spec.version.minor,
                                    spec.version.patch,
                                ),
                                features: spec
                                    .features
                                    .iter()
                                    .map(|f| (f.id, f.body.clone()))
                                    .collect(),
                            });
                        }
                        vec![]
                    }
                    Err(e) => {
                        // A truncated/oversized/invalid Peers body is a real
                        // protocol violation. JVM penalizes it via
                        // PeerSynchronizer.penalizeMaliciousPeer →
                        // PermanentPenalty (the Synchronizer parse-failure
                        // path), so we permanently ban the source.
                        tracing::warn!(
                            peer = %source_addr.ip(),
                            kind = "malformed_peers",
                            detail = %e,
                            "PENALTY"
                        );
                        self.blacklist.record_permanent(source_addr);
                        vec![]
                    }
                }
            }

            // Dropped, and not penalized: a newer protocol version may send
            // codes we don't know. Codes the node handles outside the typed
            // codec (UTXO snapshot 76–81, NiPoPoW 90–91) reach their
            // handlers through the subscriber.
            ProtocolMessage::Unknown { .. } => vec![],
        };
        debug_assert!(
            actions.iter().all(|action| match action {
                Action::Send { target, .. } => *target == source,
                Action::Validate { .. } => true,
            }),
            "the router answers the source of a message, never another peer"
        );
        actions
    }

    pub fn outbound_peers(&self) -> Vec<PeerId> {
        self.peers
            .iter()
            .filter(|(_, e)| e.direction == Direction::Outbound)
            .map(|(pid, _)| *pid)
            .collect()
    }

    pub fn inbound_peers(&self) -> Vec<PeerId> {
        self.peers
            .iter()
            .filter(|(_, e)| e.direction == Direction::Inbound)
            .map(|(pid, _)| *pid)
            .collect()
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    fn peers_to_send(&self) -> usize {
        if self.max_peer_spec_objects >= PEERS_PER_GOSSIP_MIN_CAP {
            self.max_peer_spec_objects / PEERS_PER_GOSSIP_DIVISOR
        } else {
            self.max_peer_spec_objects
        }
    }
}

/// Convert a `PeerRecord` back into a `PeerSpec` for serialization in a
/// `Peers` response.
fn record_to_spec(rec: PeerRecord) -> PeerSpec {
    use crate::transport::handshake::Feature;
    use crate::types::Version;
    PeerSpec {
        agent: rec.agent_name,
        version: Version::new(rec.version.0, rec.version.1, rec.version.2),
        name: rec.node_name,
        address: Some(rec.address),
        features: rec
            .features
            .into_iter()
            .map(|(id, body)| Feature { id, body })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::handshake::{Feature, PeerSpec};
    use crate::types::Version;

    fn pub_addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn spec_for(agent: &str, declared: SocketAddr) -> PeerSpec {
        PeerSpec {
            agent: agent.into(),
            version: Version::new(5, 0, 25),
            name: "node".into(),
            address: Some(declared),
            features: vec![],
        }
    }

    /// Build a router with bogus-address filtering disabled, mirroring
    /// `Router::new` but with `filter_bogus_addresses = false`.
    fn router_no_filter(network: Network) -> Router {
        let blacklist = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let peer_db = PeerDb::new(
            storage,
            blacklist.clone(),
            crate::peer_db::DEFAULT_CAP,
            HashSet::new(),
        )
        .expect("MemoryPeerStorage::load_all is infallible");
        Router::with_peer_db(
            Arc::new(StdMutex::new(peer_db)),
            blacklist,
            64,
            network,
            false,
        )
    }

    #[test]
    fn get_peers_returns_observed_excluding_source() {
        let mut router = Router::new(Network::Mainnet);
        // Five peers we handshaked with earlier, none currently connected.
        // Use a public-looking range — 203.0.113/24 was documentation
        // (now filtered by the bogus-address sanity layer).
        {
            let mut db = router.peer_db.lock().unwrap();
            for i in 1..=5 {
                db.record(PeerRecord {
                    address: pub_addr(&format!("78.46.10.{i}:9030")),
                    last_seen_ms: 1000 + i as u64 * 100,
                    last_handshake_ms: 1000 + i as u64 * 100,
                    agent_name: "ergoref".into(),
                    node_name: "node".into(),
                    version: (5, 0, 25),
                    features: vec![],
                });
            }
        }
        // Register a single connected outbound peer that will issue GetPeers.
        let source = PeerId(1);
        let source_addr = pub_addr("78.46.10.3:9030"); // also in the DB
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::GetPeers,
        });
        assert_eq!(actions.len(), 1);
        let body = match &actions[0] {
            Action::Send {
                target,
                message: ProtocolMessage::Peers { body },
            } => {
                assert_eq!(*target, source);
                body.clone()
            }
            _ => panic!("expected Peers reply"),
        };
        let specs = parse_peers_body(&body, 64).unwrap();
        // Source addr is excluded; the other four observed peers are all
        // served (peers_to_send = 64/8 = 8 > 4).
        for s in &specs {
            assert_ne!(s.address.unwrap(), source_addr);
        }
        assert_eq!(specs.len(), 4);
    }

    #[test]
    fn get_peers_empty_db_returns_zero_count_body() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(1);
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            pub_addr("198.51.100.1:9030"),
            None,
            None,
        );

        // Forget the stub PeerDb entry that register_peer just wrote so
        // we exercise the genuinely-empty case.
        router
            .peer_db
            .lock()
            .unwrap()
            .forget(pub_addr("198.51.100.1:9030"));

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::GetPeers,
        });
        match &actions[0] {
            Action::Send {
                message: ProtocolMessage::Peers { body },
                ..
            } => {
                assert_eq!(body, &vec![0x00]);
            }
            _ => panic!("expected Peers reply"),
        }
    }

    #[test]
    fn peers_message_records_specs_into_db() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(1);
        // 198.51.100/24 and 203.0.113/24 are documentation ranges and
        // would now trigger the bogus-address ban path. Use a public
        // range so this test still exercises the happy path.
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            pub_addr("78.46.20.1:9030"),
            None,
            None,
        );

        let specs = vec![
            spec_for("ergoref", pub_addr("78.46.20.10:9030")),
            spec_for("ergoref", pub_addr("78.46.20.11:9030")),
            spec_for("ergoref", pub_addr("78.46.20.12:9030")),
            spec_for("ergoref", pub_addr("78.46.20.13:9030")),
            spec_for("ergoref", pub_addr("78.46.20.14:9030")),
        ];
        let body = build_peers_body(&specs);
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::Peers { body },
        });
        assert!(actions.is_empty());

        let db = router.peer_db.lock().unwrap();
        for s in &specs {
            let addr = s.address.unwrap();
            let rec = db.get(addr).expect("recorded");
            assert_eq!(rec.agent_name, "ergoref");
        }
    }

    #[test]
    fn malformed_peers_bans_source_permanently() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(7);
        let source_addr = pub_addr("198.51.100.7:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );

        // Body declares a count above cap.
        let mut body = vec![];
        crate::transport::vlq::write_vlq(&mut body, (router.max_peer_spec_objects as u64) + 1);
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::Peers { body },
        });
        assert!(actions.is_empty());
        assert!(router.blacklist.contains(source_addr));
    }

    #[test]
    fn peer_connected_outbound_declared_other_host_is_hearsay() {
        let mut router = Router::new(Network::Mainnet);
        let dialed = pub_addr("198.51.100.20:9030");
        let declared = pub_addr("203.0.113.20:9030");
        let event = ProtocolEvent::PeerConnected {
            peer_id: PeerId(1),
            spec: PeerSpec {
                agent: "ergoref".into(),
                version: Version::new(5, 0, 25),
                name: "node20".into(),
                address: Some(declared),
                features: vec![Feature {
                    id: 16,
                    body: vec![0, 1, 0],
                }],
            },
            direction: Direction::Outbound,
            addr: dialed,
        };
        router.handle_event(event);
        let db = router.peer_db.lock().unwrap();
        assert_eq!(db.count(), 2);
        // The dialed address is what the handshake proved: observed, with
        // the full spec.
        let rec = db.get(dialed).expect("dialed address recorded");
        assert!(rec.is_observed());
        assert_eq!(rec.agent_name, "ergoref");
        assert_eq!(rec.node_name, "node20");
        assert_eq!(rec.version, (5, 0, 25));
        assert_eq!(rec.features.len(), 1);
        // The declared address names another host: a claim, kept as
        // hearsay with the same spec.
        let rec = db.get(declared).expect("declared address recorded");
        assert!(!rec.is_observed());
        assert!(rec.last_seen_ms > 0);
        assert_eq!(rec.node_name, "node20");
        assert_eq!(db.observed(8, &HashSet::new()).len(), 1);
    }

    #[test]
    fn peer_connected_outbound_no_declared_records_dialed() {
        let mut router = Router::new(Network::Mainnet);
        let dialed = pub_addr("198.51.100.21:9030");
        let event = ProtocolEvent::PeerConnected {
            peer_id: PeerId(1),
            spec: PeerSpec {
                agent: "ergoref".into(),
                version: Version::new(5, 0, 25),
                name: "node21".into(),
                address: None,
                features: vec![],
            },
            direction: Direction::Outbound,
            addr: dialed,
        };
        router.handle_event(event);
        let db = router.peer_db.lock().unwrap();
        assert_eq!(db.count(), 1);
        let rec = db.get(dialed).expect("dialed address recorded");
        assert!(rec.is_observed(), "we dialed it and it answered");
        assert_eq!(rec.node_name, "node21");
    }

    #[test]
    fn peers_with_bogus_entry_drops_bogus_no_ban() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(1);
        let source_addr = pub_addr("198.51.100.50:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );

        let good = pub_addr("78.46.1.50:9030");
        let bogus = pub_addr("169.254.0.2:9030"); // link-local APIPA
        let specs = vec![spec_for("ergoref", good), spec_for("ergoref", bogus)];
        let body = build_peers_body(&specs);
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::Peers { body },
        });
        assert!(actions.is_empty(), "Peers ingest emits no Send actions");

        // With filtering on (default), the good entry is recorded and the
        // bogus entry is silently dropped.
        let db = router.peer_db.lock().unwrap();
        assert!(db.get(good).is_some(), "good address recorded");
        assert!(db.get(bogus).is_none(), "bogus address dropped");
        drop(db);

        // Source is NOT banned — gossiping a bogus address is normal on a
        // NAT'd network, not misbehavior (JVM 6.0.3 parity).
        assert!(
            !router.blacklist.contains(source_addr),
            "source not banned for gossiping a bogus address"
        );
    }

    #[test]
    fn peers_all_bogus_drops_all_no_ban() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(2);
        let source_addr = pub_addr("78.46.1.51:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );
        // register_peer seeded the source's own address into PeerDb (as
        // an outbound stub). Capture the snapshot of addresses BEFORE
        // the bogus Peers message so we can assert no new entries land.
        let before: HashSet<SocketAddr> = router
            .peer_db
            .lock()
            .unwrap()
            .all()
            .into_iter()
            .map(|r| r.address)
            .collect();

        let bogus_specs = vec![
            spec_for("ergoref", pub_addr("169.254.0.2:9030")), // link-local
            spec_for("ergoref", pub_addr("10.0.0.1:9030")),    // RFC 1918
            spec_for("ergoref", pub_addr("127.0.0.1:9030")),   // loopback
        ];
        let body = build_peers_body(&bogus_specs);
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::Peers { body },
        });
        assert!(actions.is_empty());

        let after: HashSet<SocketAddr> = router
            .peer_db
            .lock()
            .unwrap()
            .all()
            .into_iter()
            .map(|r| r.address)
            .collect();
        assert_eq!(before, after, "PeerDb unchanged after all-bogus Peers body");

        // No penalty — an all-bogus body is filtered, not punished.
        assert!(
            !router.blacklist.contains(source_addr),
            "source not banned for an all-bogus Peers body"
        );
    }

    #[test]
    fn peers_bogus_ingested_when_filter_disabled() {
        let mut router = router_no_filter(Network::Mainnet);
        let source = PeerId(1);
        let source_addr = pub_addr("78.46.1.60:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );

        // RFC 1918 — mainnet-bogus by classification, but filtering is off,
        // so it must be ingested rather than dropped.
        let private = pub_addr("10.1.2.3:9030");
        let specs = vec![spec_for("ergoref", private)];
        let body = build_peers_body(&specs);
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::Peers { body },
        });
        assert!(actions.is_empty(), "Peers ingest emits no Send actions");

        let db = router.peer_db.lock().unwrap();
        assert!(
            db.get(private).is_some(),
            "bogus address ingested when filter disabled"
        );
        drop(db);
        assert!(
            !router.blacklist.contains(source_addr),
            "source never banned for bogus gossip"
        );
    }

    #[test]
    fn getpeers_includes_bogus_when_filter_disabled() {
        let mut router = router_no_filter(Network::Mainnet);
        let source = PeerId(4);
        let source_addr = pub_addr("78.46.1.61:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );

        // A bogus address that reached PeerDb (e.g. ingested while the
        // filter was off, or a legacy row). With filtering disabled the
        // defensive egress filter is also off, so it is gossiped onward.
        let bogus = pub_addr("192.168.1.42:9030"); // RFC 1918
        {
            let mut db = router.peer_db.lock().unwrap();
            db.record(PeerRecord {
                address: bogus,
                last_seen_ms: 3000,
                last_handshake_ms: 3000,
                agent_name: "ergoref".into(),
                node_name: "".into(),
                version: (5, 0, 25),
                features: vec![],
            });
        }

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::GetPeers,
        });
        let body = match &actions[0] {
            Action::Send {
                message: ProtocolMessage::Peers { body },
                ..
            } => body.clone(),
            _ => panic!("expected Peers reply"),
        };
        let specs = parse_peers_body(&body, 64).unwrap();
        let addrs: Vec<SocketAddr> = specs.iter().filter_map(|s| s.address).collect();
        assert!(
            addrs.contains(&bogus),
            "bogus address gossiped when filter disabled"
        );
    }

    #[test]
    fn peers_clean_no_ban() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(3);
        let source_addr = pub_addr("78.46.1.52:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );

        let clean_specs = vec![
            spec_for("ergoref", pub_addr("78.46.2.10:9030")),
            spec_for("ergoref", pub_addr("78.46.2.11:9030")),
            spec_for("ergoref", pub_addr("78.46.2.12:9030")),
        ];
        let body = build_peers_body(&clean_specs);
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::Peers { body },
        });
        assert!(actions.is_empty());

        let db = router.peer_db.lock().unwrap();
        for s in &clean_specs {
            assert!(db.get(s.address.unwrap()).is_some());
        }
        drop(db);
        assert!(
            !router.blacklist.contains(source_addr),
            "source not banned for clean Peers"
        );
    }

    #[test]
    fn getpeers_skips_bogus_in_db() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(4);
        let source_addr = pub_addr("78.46.1.53:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );

        // Preload PeerDb with one bogus + one good observed entry (bypass
        // the Peers-arm filter so we exercise the defensive egress path).
        let good = pub_addr("78.46.3.10:9030");
        let bogus = pub_addr("192.168.1.42:9030"); // RFC 1918
        {
            let mut db = router.peer_db.lock().unwrap();
            db.record(PeerRecord {
                address: good,
                last_seen_ms: 2000,
                last_handshake_ms: 2000,
                agent_name: "ergoref".into(),
                node_name: "".into(),
                version: (5, 0, 25),
                features: vec![],
            });
            db.record(PeerRecord {
                address: bogus,
                last_seen_ms: 3000,
                last_handshake_ms: 3000, // more recent than `good`
                agent_name: "ergoref".into(),
                node_name: "".into(),
                version: (5, 0, 25),
                features: vec![],
            });
        }

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::GetPeers,
        });
        let body = match &actions[0] {
            Action::Send {
                message: ProtocolMessage::Peers { body },
                ..
            } => body.clone(),
            _ => panic!("expected Peers reply"),
        };
        let specs = parse_peers_body(&body, 64).unwrap();
        let addrs: Vec<SocketAddr> = specs.iter().filter_map(|s| s.address).collect();
        assert!(addrs.contains(&good), "good address present in response");
        assert!(
            !addrs.contains(&bogus),
            "bogus address absent from response"
        );
    }

    #[test]
    fn get_peers_excludes_connected_addresses() {
        let mut router = Router::new(Network::Mainnet);
        // Outbound source. Use a public range — 198.51.100/24 and
        // 203.0.113/24 are now stripped by the bogus-address filter.
        let source = PeerId(1);
        let source_addr = pub_addr("78.46.30.40:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );
        // Another connected peer at a different address.
        let other = PeerId(2);
        let other_addr = pub_addr("78.46.30.41:9030");
        router.register_peer(
            other,
            Direction::Outbound,
            ProxyMode::Full,
            other_addr,
            None,
            None,
        );
        // A peer we handshaked with earlier and are no longer connected to.
        let disconnected = pub_addr("78.46.30.42:9030");
        {
            let mut db = router.peer_db.lock().unwrap();
            db.record(PeerRecord {
                address: disconnected,
                last_seen_ms: 5000,
                last_handshake_ms: 5000,
                agent_name: "ergoref".into(),
                node_name: "".into(),
                version: (5, 0, 25),
                features: vec![],
            });
        }

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::GetPeers,
        });
        let body = match &actions[0] {
            Action::Send {
                message: ProtocolMessage::Peers { body },
                ..
            } => body.clone(),
            _ => panic!(),
        };
        let specs = parse_peers_body(&body, 64).unwrap();
        // Source and other connected peer must be excluded.
        for s in &specs {
            let a = s.address.unwrap();
            assert_ne!(a, source_addr);
            assert_ne!(a, other_addr);
        }
        // The disconnected observed peer is present.
        assert!(specs.iter().any(|s| s.address == Some(disconnected)));
    }

    /// On testnet, the network-conditional classes (RFC 1918, CGN, ULA,
    /// documentation) are NOT bogus — a developer running a testnet
    /// inside a LAN must be able to gossip private addresses without
    /// getting their peers banned. The unconditional classes (loopback,
    /// link-local, multicast, etc.) still ban.
    #[test]
    fn testnet_accepts_private_gossiped_address() {
        let mut router = Router::new(Network::Testnet);
        let source = PeerId(1);
        let source_addr = pub_addr("192.168.50.1:9030");
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            source_addr,
            None,
            None,
        );

        let private = pub_addr("192.168.1.1:9030");
        let specs = vec![spec_for("ergoref", private)];
        let body = build_peers_body(&specs);
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::Peers { body },
        });
        assert!(actions.is_empty(), "Peers ingest emits no Send actions");

        // Private address recorded on testnet, source not banned.
        let db = router.peer_db.lock().unwrap();
        assert!(
            db.get(private).is_some(),
            "private address recorded on testnet"
        );
        drop(db);
        assert!(
            !router.blacklist.contains(source_addr),
            "testnet source not banned for gossiping private addresses"
        );
    }

    // ---- observed vs hearsay (facts/p2p-peerdb.md § Observed and hearsay) ----

    fn observed_record(address: SocketAddr, stamp: u64) -> PeerRecord {
        PeerRecord {
            address,
            last_seen_ms: stamp,
            last_handshake_ms: stamp,
            agent_name: "ergoref".into(),
            node_name: "".into(),
            version: (5, 0, 25),
            features: vec![],
        }
    }

    fn connected_event(
        declared: Option<SocketAddr>,
        direction: Direction,
        socket: SocketAddr,
    ) -> ProtocolEvent {
        ProtocolEvent::PeerConnected {
            peer_id: PeerId(1),
            spec: PeerSpec {
                agent: "ergoref".into(),
                version: Version::new(5, 0, 25),
                name: "node".into(),
                address: declared,
                features: vec![],
            },
            direction,
            addr: socket,
        }
    }

    #[test]
    fn register_peer_seeds_outbound_as_observed() {
        let mut router = Router::new(Network::Mainnet);
        let dialed = pub_addr("78.46.70.1:9030");
        router.register_peer(
            PeerId(1),
            Direction::Outbound,
            ProxyMode::Full,
            dialed,
            None,
            Some("ergoref".into()),
        );
        // An inbound registration carries the peer's ephemeral socket; it
        // is not a listening address and register_peer does not record it.
        let inbound_socket = pub_addr("78.46.70.2:51234");
        router.register_peer(
            PeerId(2),
            Direction::Inbound,
            ProxyMode::Full,
            inbound_socket,
            None,
            Some("ergoref".into()),
        );
        let db = router.peer_db.lock().unwrap();
        let rec = db.get(dialed).expect("dialed address recorded");
        assert!(
            rec.is_observed(),
            "a completed outbound handshake is an observation"
        );
        assert_eq!(rec.last_seen_ms, rec.last_handshake_ms);
        assert!(db.get(inbound_socket).is_none());
    }

    #[test]
    fn peers_intake_records_hearsay() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(1);
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            pub_addr("78.46.20.1:9030"),
            None,
            None,
        );
        // One address we handshaked with before, one we have never seen.
        let known = pub_addr("78.46.20.10:9030");
        let fresh = pub_addr("78.46.20.11:9030");
        router
            .peer_db
            .lock()
            .unwrap()
            .record(observed_record(known, 5000));

        let body = build_peers_body(&[spec_for("ergoref", known), spec_for("ergoref", fresh)]);
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::Peers { body },
        });
        assert!(actions.is_empty());

        let db = router.peer_db.lock().unwrap();
        let fresh_rec = db.get(fresh).expect("gossiped address recorded");
        assert_eq!(fresh_rec.last_handshake_ms, 0, "gossip is hearsay");
        assert!(
            fresh_rec.last_seen_ms > 5000,
            "the mention is stamped with the receipt time"
        );
        let known_rec = db.get(known).expect("still present");
        assert_eq!(
            known_rec.last_handshake_ms, 5000,
            "gossip cannot raise a handshake stamp"
        );
        assert!(
            known_rec.last_seen_ms > 5000,
            "but it does refresh last_seen"
        );
    }

    #[test]
    fn get_peers_serves_observed_only() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(1);
        router.register_peer(
            source,
            Direction::Outbound,
            ProxyMode::Full,
            pub_addr("78.46.40.1:9030"),
            None,
            None,
        );
        let observed = pub_addr("78.46.40.10:9030");
        let hearsay = pub_addr("78.46.40.11:9030");
        {
            let mut db = router.peer_db.lock().unwrap();
            db.record(observed_record(observed, 1000));
            // Hearsay mentioned far more recently than the handshake:
            // recency does not make it gossip-worthy.
            db.record(PeerRecord {
                last_handshake_ms: 0,
                ..observed_record(hearsay, 9_000_000)
            });
        }

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::GetPeers,
        });
        let body = match &actions[0] {
            Action::Send {
                message: ProtocolMessage::Peers { body },
                ..
            } => body.clone(),
            _ => panic!("expected Peers reply"),
        };
        let specs = parse_peers_body(&body, 64).unwrap();
        let addrs: Vec<SocketAddr> = specs.iter().filter_map(|s| s.address).collect();
        assert_eq!(addrs, vec![observed]);
    }

    #[test]
    fn peer_connected_outbound_is_observed() {
        let mut router = Router::new(Network::Mainnet);
        let dialed = pub_addr("78.46.50.20:9030");
        router.handle_event(connected_event(Some(dialed), Direction::Outbound, dialed));
        let db = router.peer_db.lock().unwrap();
        let rec = db.get(dialed).expect("recorded");
        assert!(rec.is_observed());
        assert_eq!(rec.last_seen_ms, rec.last_handshake_ms);
        assert_eq!(db.count(), 1, "declared == dialed is one entry");
        assert_eq!(db.observed(8, &HashSet::new()).len(), 1);
    }

    #[test]
    fn peer_connected_outbound_declared_other_port_same_host_both_observed() {
        let mut router = Router::new(Network::Mainnet);
        let dialed = pub_addr("78.46.50.20:9030");
        let declared = pub_addr("78.46.50.20:9031");
        router.handle_event(connected_event(Some(declared), Direction::Outbound, dialed));
        let db = router.peer_db.lock().unwrap();
        assert_eq!(db.count(), 2);
        assert!(db.get(dialed).expect("dialed recorded").is_observed());
        assert!(
            db.get(declared).expect("declared recorded").is_observed(),
            "a port claim on the host we just talked to"
        );
    }

    #[test]
    fn peer_connected_inbound_declared_same_host_is_observed() {
        let mut router = Router::new(Network::Mainnet);
        let socket = pub_addr("78.46.60.5:51234");
        let declared = pub_addr("78.46.60.5:9030");
        router.handle_event(connected_event(Some(declared), Direction::Inbound, socket));
        let db = router.peer_db.lock().unwrap();
        let rec = db.get(declared).expect("declared address recorded");
        assert!(
            rec.is_observed(),
            "a declared port on the connection's own IP is observed"
        );
        assert!(
            db.get(socket).is_none(),
            "the ephemeral socket is not recorded"
        );
    }

    #[test]
    fn peer_connected_inbound_declared_other_host_is_hearsay() {
        let mut router = Router::new(Network::Mainnet);
        let socket = pub_addr("78.46.60.5:51234");
        let declared = pub_addr("91.10.1.1:9030");
        router.handle_event(connected_event(Some(declared), Direction::Inbound, socket));
        let db = router.peer_db.lock().unwrap();
        let rec = db.get(declared).expect("declared address recorded");
        assert_eq!(
            rec.last_handshake_ms, 0,
            "the connection proves nothing about another host"
        );
        assert!(rec.last_seen_ms > 0);
        assert!(
            db.observed(8, &HashSet::new()).is_empty(),
            "hearsay is never served by GetPeers"
        );
    }

    #[test]
    fn peer_connected_inbound_no_declared_records_nothing() {
        let mut router = Router::new(Network::Mainnet);
        let socket = pub_addr("78.46.60.5:51234");
        let before = router.peer_db.lock().unwrap().count();
        router.handle_event(connected_event(None, Direction::Inbound, socket));
        let db = router.peer_db.lock().unwrap();
        assert_eq!(
            db.count(),
            before,
            "an ephemeral socket is not a listening address"
        );
        assert!(db.get(socket).is_none());
    }

    #[test]
    fn peer_connected_inbound_hearsay_does_not_demote_observed() {
        // An address we dialed before, now declared by an inbound peer on
        // another host: the earlier observation stands.
        let mut router = Router::new(Network::Mainnet);
        let declared = pub_addr("91.10.1.1:9030");
        router
            .peer_db
            .lock()
            .unwrap()
            .record(observed_record(declared, 5000));
        router.handle_event(connected_event(
            Some(declared),
            Direction::Inbound,
            pub_addr("78.46.60.5:51234"),
        ));
        let db = router.peer_db.lock().unwrap();
        assert_eq!(db.get(declared).unwrap().last_handshake_ms, 5000);
    }

    // ---- local-serve hook (facts/p2p-routing.md ModifierRequest arm) ----

    /// Modifier id helper: 32 bytes of `n`.
    fn mid(n: u8) -> ModifierId {
        [n; 32]
    }

    /// Hook backed by a fixed id → bytes table.
    fn serve_table(entries: Vec<(ModifierId, Vec<u8>)>) -> LocalServeFn {
        let map: HashMap<ModifierId, Vec<u8>> = entries.into_iter().collect();
        Arc::new(move |_mtype, id| map.get(id).cloned())
    }

    /// Requester + one other connected peer, which nothing the requester
    /// sends may ever reach. Returns (router, S, O).
    fn router_with_requester_and_outbound() -> (Router, PeerId, PeerId) {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(1);
        router.register_peer(
            source,
            Direction::Inbound,
            ProxyMode::Full,
            pub_addr("78.46.40.1:9030"),
            None,
            None,
        );
        let outbound = PeerId(2);
        router.register_peer(
            outbound,
            Direction::Outbound,
            ProxyMode::Full,
            pub_addr("78.46.40.2:9030"),
            None,
            None,
        );
        (router, source, outbound)
    }

    #[test]
    fn local_serve_hit_responds_to_the_source() {
        let (mut router, source, _outbound) = router_with_requester_and_outbound();
        router.set_local_serve(serve_table(vec![(mid(0xAA), b"header-bytes".to_vec())]));

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::ModifierRequest {
                modifier_type: 101,
                ids: vec![mid(0xAA)],
            },
        });
        assert_eq!(actions.len(), 1, "exactly one action: the response");
        match &actions[0] {
            Action::Send {
                target,
                message:
                    ProtocolMessage::ModifierResponse {
                        modifier_type,
                        modifiers,
                    },
            } => {
                assert_eq!(*target, source, "the response goes to the requester");
                assert_eq!(*modifier_type, 101);
                assert_eq!(
                    modifiers.as_slice(),
                    &[(mid(0xAA), b"header-bytes".to_vec())]
                );
            }
            other => panic!("expected a ModifierResponse, got {other:?}"),
        }
    }

    #[test]
    fn local_serve_miss_sends_nothing() {
        let (mut router, source, outbound) = router_with_requester_and_outbound();
        router.set_local_serve(serve_table(vec![])); // misses everything

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::ModifierRequest {
                modifier_type: 102,
                ids: vec![mid(0xBB)],
            },
        });
        assert!(
            actions.is_empty(),
            "a miss gets no answer and is asked of no other peer: {actions:?}"
        );

        // The other peer delivering that id later is validated, and the
        // requester is not sent a copy.
        let follow = router.handle_event(ProtocolEvent::Message {
            peer_id: outbound,
            message: ProtocolMessage::ModifierResponse {
                modifier_type: 102,
                modifiers: vec![(mid(0xBB), b"section".to_vec())],
            },
        });
        assert!(
            follow.iter().all(|a| matches!(a, Action::Validate { .. })),
            "a response is never forwarded: {follow:?}"
        );
    }

    #[test]
    fn local_serve_mixed_batch_answers_the_hits_only() {
        let (mut router, source, _outbound) = router_with_requester_and_outbound();
        router.set_local_serve(serve_table(vec![
            (mid(0x0A), b"mod-a".to_vec()),
            (mid(0x0C), b"mod-c".to_vec()),
        ]));

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::ModifierRequest {
                modifier_type: 102,
                ids: vec![mid(0x0A), mid(0x0B), mid(0x0C)],
            },
        });

        assert_eq!(actions.len(), 1, "one response, and nothing for the miss");
        match &actions[0] {
            Action::Send {
                target,
                message: ProtocolMessage::ModifierResponse { modifiers, .. },
            } => {
                assert_eq!(*target, source);
                assert_eq!(
                    modifiers.as_slice(),
                    &[
                        (mid(0x0A), b"mod-a".to_vec()),
                        (mid(0x0C), b"mod-c".to_vec())
                    ],
                    "both hits in one response, request order preserved"
                );
            }
            other => panic!("expected a ModifierResponse, got {other:?}"),
        }
    }

    #[test]
    fn without_a_hook_no_request_is_answered_or_passed_on() {
        let (mut router, source, _outbound) = router_with_requester_and_outbound();
        let announcer = PeerId(3);
        router.register_peer(
            announcer,
            Direction::Inbound,
            ProxyMode::Full,
            pub_addr("78.46.40.3:9030"),
            None,
            None,
        );
        // Having announced an id does not make a peer a target for
        // requests of it.
        router.handle_event(ProtocolEvent::Message {
            peer_id: announcer,
            message: ProtocolMessage::Inv {
                modifier_type: 102,
                ids: vec![mid(0x11)],
            },
        });

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::ModifierRequest {
                modifier_type: 102,
                ids: vec![mid(0x11), mid(0x22)],
            },
        });
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn light_mode_block_modifier_request_dropped() {
        let mut router = Router::new(Network::Mainnet);
        let source = PeerId(1);
        router.register_peer(
            source,
            Direction::Inbound,
            ProxyMode::Light,
            pub_addr("78.46.41.1:9030"),
            None,
            None,
        );
        let outbound = PeerId(2);
        router.register_peer(
            outbound,
            Direction::Outbound,
            ProxyMode::Full,
            pub_addr("78.46.41.2:9030"),
            None,
            None,
        );
        // A hook that would serve anything — must never be consulted for
        // block-related requests from a Light source.
        router.set_local_serve(Arc::new(|_, _| Some(b"data".to_vec())));

        for block_type in [101u8, 102, 104, 108] {
            let actions = router.handle_event(ProtocolEvent::Message {
                peer_id: source,
                message: ProtocolMessage::ModifierRequest {
                    modifier_type: block_type,
                    ids: vec![mid(0x33)],
                },
            });
            assert!(
                actions.is_empty(),
                "block-related type {block_type} dropped for Light source"
            );
        }

        // Transactions (type 2) are mempool gossip, not block data: a
        // Light source's request goes to the hook like any other.
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::ModifierRequest {
                modifier_type: 2,
                ids: vec![mid(0x44)],
            },
        });
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::Send {
                    target,
                    message: ProtocolMessage::ModifierResponse { .. }
                } if *target == source
            )),
            "transaction request from Light source still served"
        );
    }

    #[test]
    fn sync_info_from_any_peer_produces_no_actions() {
        // Sync answers every peer's SyncInfo itself, from the subscriber.
        let mut router = Router::new(Network::Mainnet);
        let peers = [
            (PeerId(1), Direction::Inbound, ProxyMode::Full),
            (PeerId(2), Direction::Inbound, ProxyMode::Light),
            (PeerId(3), Direction::Outbound, ProxyMode::Full),
        ];
        for (i, (peer, direction, mode)) in peers.into_iter().enumerate() {
            router.register_peer(
                peer,
                direction,
                mode,
                pub_addr(&format!("78.46.41.{}:9030", i + 3)),
                None,
                None,
            );
        }
        for (peer, _, _) in peers {
            let actions = router.handle_event(ProtocolEvent::Message {
                peer_id: peer,
                message: ProtocolMessage::SyncInfo {
                    body: vec![1, 2, 3],
                },
            });
            assert!(actions.is_empty(), "SyncInfo from {peer}: {actions:?}");
        }
    }

    #[test]
    fn local_serve_oversized_batch_splits_responses() {
        // Three ~900 KB modifiers: two fit under the serve batch cap
        // (2_048_576), the third forces a second response — JVM
        // sendByParts parity.
        let (mut router, source, _outbound) = router_with_requester_and_outbound();
        let big = vec![0u8; 900_000];
        router.set_local_serve(serve_table(vec![
            (mid(0x01), big.clone()),
            (mid(0x02), big.clone()),
            (mid(0x03), big.clone()),
        ]));

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::ModifierRequest {
                modifier_type: 104,
                ids: vec![mid(0x01), mid(0x02), mid(0x03)],
            },
        });

        let batches: Vec<Vec<ModifierId>> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Send {
                    target,
                    message: ProtocolMessage::ModifierResponse { modifiers, .. },
                } if *target == source => Some(modifiers.iter().map(|(id, _)| *id).collect()),
                _ => None,
            })
            .collect();
        assert_eq!(
            batches,
            vec![vec![mid(0x01), mid(0x02)], vec![mid(0x03)]],
            "split into [A,B] + [C] at the serve batch cap"
        );
        // Every batch's encoded body stays under the frame read cap, so
        // a symmetric rust peer never rejects a served response.
        for a in &actions {
            if let Action::Send { message, .. } = a {
                assert!(message.to_frame().body.len() <= 2 * 1024 * 1024);
            }
        }
    }

    #[test]
    fn local_serve_single_oversized_modifier_sent_alone() {
        // A modifier alone exceeding the batch cap is shipped anyway in
        // its own response (JVM sendByParts sends + warns).
        let (mut router, source, _outbound) = router_with_requester_and_outbound();
        router.set_local_serve(serve_table(vec![(mid(0x05), vec![0u8; 2_049_000])]));

        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: source,
            message: ProtocolMessage::ModifierRequest {
                modifier_type: 104,
                ids: vec![mid(0x05)],
            },
        });
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Send {
                target,
                message: ProtocolMessage::ModifierResponse { modifiers, .. },
            } => {
                assert_eq!(*target, source);
                assert_eq!(modifiers.len(), 1);
                assert_eq!(modifiers[0].1.len(), 2_049_000);
            }
            other => panic!("expected lone oversized response, got {other:?}"),
        }
    }
}

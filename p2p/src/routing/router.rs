//! Message routing: forwarding decisions, mode filtering, peer registry.
//!
//! # Contract
//! - `handle_event`: given a `ProtocolEvent`, returns a list of `Action`s.
//!   Precondition: peer IDs in events are registered (or being disconnected).
//!   Postcondition: actions target only registered, non-disconnected peers.
//! - `register_peer` / peer removal on disconnect: manage the peer registry.
//! - Invariant: Inv table, request tracker, and sync tracker are consistent with
//!   the peer registry — no references to unregistered peers.
//! - PeerDb is the canonical store of "addresses we know about"; see
//!   `facts/p2p-peerdb.md`.

use crate::blacklist::Blacklist;
use crate::peer_db::{MemoryPeerStorage, PeerDb, PeerRecord, PeerStorage};
use crate::protocol::address_sanity::is_bogus_address;
use crate::protocol::counters::{TrafficCounters, TrafficSnapshot};
use crate::protocol::messages::{build_peers_body, parse_peers_body, ProtocolMessage};
use crate::protocol::peer::ProtocolEvent;
use crate::routing::inv_table::InvTable;
use crate::routing::latency::{LatencyStats, LatencyTracker};
use crate::routing::tracker::{RequestTracker, SyncTracker};
use crate::transport::handshake::PeerSpec;
use crate::types::{ConnectionType, Direction, Network, PeerId, ProxyMode};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// JVM's `PeerSynchronizer.gossipPeers` sends `max/8` peers when the
/// cap is >= 16, matching its post-5.0.8 convention. With our default
/// cap of 64, that's 8.
const PEERS_PER_GOSSIP_DIVISOR: usize = 8;
const PEERS_PER_GOSSIP_MIN_CAP: usize = 16;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A routing directive.
#[derive(Debug)]
pub enum Action {
    Send {
        target: PeerId,
        message: ProtocolMessage,
    },
    /// Forward modifier data to the async validation pipeline.
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
    peers: HashMap<PeerId, PeerEntry>,
    inv_table: InvTable,
    request_tracker: RequestTracker,
    sync_tracker: SyncTracker,
    latency_tracker: LatencyTracker,
    /// Message codes handled by the main crate via the event stream.
    /// Unknown messages with these codes are not forwarded to peers.
    consumed_codes: HashSet<u8>,

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
        Self::with_peer_db(Arc::new(StdMutex::new(peer_db)), blacklist, 64, network, true)
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
            inv_table: InvTable::new(),
            request_tracker: RequestTracker::new(),
            sync_tracker: SyncTracker::new(),
            latency_tracker: LatencyTracker::new(),
            consumed_codes: HashSet::new(),
            peer_db,
            blacklist,
            max_peer_spec_objects,
            network,
            filter_bogus_addresses,
            counters: Arc::new(TrafficCounters::new()),
        }
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

    /// Register a message code as consumed by the main crate's event stream.
    /// Unknown messages with this code will not be forwarded to peers.
    pub fn register_consumed_code(&mut self, code: u8) {
        self.consumed_codes.insert(code);
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
        // Best-effort seed of PeerDb for outbound peers so the
        // register-only test path (no event-loop) gets an entry without
        // also driving the PeerConnected event. Production overwrites
        // this stub via PeerConnected's full handshake spec (merge-max
        // on last_seen).
        //
        // Inbound peers are excluded: their observed socket is the
        // peer's ephemeral outgoing port, not a listening address worth
        // gossiping. Their listening address (declared in the
        // handshake) is recorded by PeerConnected.
        if direction == Direction::Outbound && !self.blacklist.contains(addr) {
            let mut db = self.peer_db.lock().expect("peer_db poisoned");
            db.record(PeerRecord {
                address: addr,
                last_seen_ms: now_ms(),
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
            ProtocolEvent::PeerConnected { spec, addr, .. } => {
                self.record_peer_connected(&spec, addr);
                vec![]
            }

            ProtocolEvent::PeerDisconnected { peer_id, .. } => {
                self.inv_table.purge_peer(peer_id);
                self.request_tracker.purge_peer(peer_id);
                self.sync_tracker.purge_peer(peer_id);
                self.latency_tracker.purge_peer(peer_id);
                self.peers.remove(&peer_id);
                vec![]
            }

            ProtocolEvent::Message { peer_id, message } => self.route_message(peer_id, message),
        }
    }

    fn record_peer_connected(&self, spec: &PeerSpec, observed_addr: SocketAddr) {
        // Prefer the peer's declared address; fall back to the observed
        // socket so that NAT'd peers without a declared address are
        // still tracked.
        let key_addr = spec.address.unwrap_or(observed_addr);
        if self.blacklist.contains(key_addr) {
            return;
        }
        let mut db = self.peer_db.lock().expect("peer_db poisoned");
        db.record(PeerRecord {
            address: key_addr,
            last_seen_ms: now_ms(),
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

    fn route_message(&mut self, source: PeerId, message: ProtocolMessage) -> Vec<Action> {
        self.request_tracker.sweep_expired(Duration::from_secs(60));

        let source_entry = match self.peers.get(&source) {
            Some(e) => e,
            None => return vec![],
        };
        let source_direction = source_entry.direction;
        let source_mode = source_entry.mode;
        let source_addr = source_entry.addr;

        match message {
            ProtocolMessage::Inv { ids, .. } => {
                // Record which peer has which modifier so our own
                // ModifierRequest routing can pick a source. This is the
                // full-node use of an incoming Inv.
                //
                // We do NOT relay the Inv to other peers. Relaying was
                // leftover from the transparent-proxy origin of this crate
                // and is protocol-incorrect for a full node: every peer
                // talks to every other peer directly, so a relay of their
                // announcements both duplicates traffic and — critically —
                // can exceed the Inv size cap (v1 sync: 400 modifiers) if
                // an upstream peer sent us an oversized Inv, getting us
                // banned by strict peers. A full node announces its own
                // modifiers (handled by the main crate on validate /
                // mempool-accept), it does not forward others'.
                for id in &ids {
                    self.inv_table.record(*id, source);
                }
                vec![]
            }

            ProtocolMessage::ModifierRequest { modifier_type, ids } => {
                let mut actions = Vec::new();
                for id in &ids {
                    let target = if let Some(inv_target) = self.inv_table.lookup(id) {
                        if inv_target == source {
                            continue;
                        }
                        Some(inv_target)
                    } else {
                        // Fallback: pick any outbound peer that isn't the source.
                        // Enables chain sync where modifier IDs come from SyncInfo, not Inv.
                        self.peers
                            .iter()
                            .find(|(pid, entry)| {
                                **pid != source && entry.direction == Direction::Outbound
                            })
                            .map(|(pid, _)| *pid)
                    };

                    if let Some(target) = target {
                        self.request_tracker.record(*id, source);
                        self.latency_tracker.record_request(*id, target);
                        actions.push(Action::Send {
                            target,
                            message: ProtocolMessage::ModifierRequest {
                                modifier_type,
                                ids: vec![*id],
                            },
                        });
                    }
                }
                actions
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
                let mut actions = Vec::new();
                for (id, data) in &modifiers {
                    actions.push(Action::Validate {
                        modifier_type,
                        id: *id,
                        data: data.clone(),
                        peer_id: source,
                    });
                    self.latency_tracker.record_response(id);
                    if let Some(requester) = self.request_tracker.fulfill(id) {
                        actions.push(Action::Send {
                            target: requester,
                            message: ProtocolMessage::ModifierResponse {
                                modifier_type,
                                modifiers: vec![(*id, data.clone())],
                            },
                        });
                    }
                }
                actions
            }

            ProtocolMessage::SyncInfo { body } => {
                if source_mode == ProxyMode::Light {
                    return vec![];
                }

                match source_direction {
                    Direction::Inbound => {
                        if let Some((&outbound_id, _)) = self.peers.iter().find(|(pid, entry)| {
                            entry.direction == Direction::Outbound
                                && self.sync_tracker.inbound_for(pid).is_none()
                        }) {
                            self.sync_tracker.pair(source, outbound_id);
                            vec![Action::Send {
                                target: outbound_id,
                                message: ProtocolMessage::SyncInfo { body },
                            }]
                        } else {
                            vec![]
                        }
                    }
                    Direction::Outbound => {
                        if let Some(inbound) = self.sync_tracker.inbound_for(&source) {
                            vec![Action::Send {
                                target: inbound,
                                message: ProtocolMessage::SyncInfo { body },
                            }]
                        } else {
                            vec![]
                        }
                    }
                }
            }

            ProtocolMessage::GetPeers => {
                let limit = self.peers_to_send();
                let mut exclude: HashSet<SocketAddr> =
                    self.peers.values().map(|p| p.addr).collect();
                exclude.insert(source_addr);

                let specs: Vec<PeerSpec> = {
                    let db = self.peer_db.lock().expect("peer_db poisoned");
                    db.recent(limit, &exclude)
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
                            if self.filter_bogus_addresses
                                && is_bogus_address(addr, self.network)
                            {
                                continue;
                            }
                            if self.blacklist.contains(addr) {
                                continue;
                            }
                            db.record(PeerRecord {
                                address: addr,
                                last_seen_ms: now_ms(),
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

            ProtocolMessage::Unknown { code, body } => {
                if self.consumed_codes.contains(&code) {
                    return vec![];
                }

                let target_direction = match source_direction {
                    Direction::Outbound => Direction::Inbound,
                    Direction::Inbound => Direction::Outbound,
                };

                self.peers
                    .iter()
                    .filter(|(pid, entry)| **pid != source && entry.direction == target_direction)
                    .map(|(pid, _)| Action::Send {
                        target: *pid,
                        message: ProtocolMessage::Unknown {
                            code,
                            body: body.clone(),
                        },
                    })
                    .collect()
            }
        }
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

    pub fn latency_stats(&self) -> Option<LatencyStats> {
        self.latency_tracker.stats()
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
    fn get_peers_returns_recent_excluding_source() {
        let mut router = Router::new(Network::Mainnet);
        // Five known peers in the PeerDb but none currently connected.
        // Use a public-looking range — 203.0.113/24 was documentation
        // (now filtered by the bogus-address sanity layer).
        {
            let mut db = router.peer_db.lock().unwrap();
            for i in 1..=5 {
                db.record(PeerRecord {
                    address: pub_addr(&format!("78.46.10.{i}:9030")),
                    last_seen_ms: 1000 + i as u64 * 100,
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
        // Source addr is excluded.
        for s in &specs {
            assert_ne!(s.address.unwrap(), source_addr);
        }
        // At most peers_to_send (= 64/8 = 8) entries.
        assert!(specs.len() <= 8);
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
    fn peer_connected_records_with_declared_address() {
        let mut router = Router::new(Network::Mainnet);
        let observed = pub_addr("198.51.100.20:9030");
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
            addr: observed,
        };
        router.handle_event(event);
        let db = router.peer_db.lock().unwrap();
        let rec = db.get(declared).expect("declared address recorded");
        assert_eq!(rec.agent_name, "ergoref");
        assert_eq!(rec.node_name, "node20");
        assert_eq!(rec.version, (5, 0, 25));
        assert_eq!(rec.features.len(), 1);
    }

    #[test]
    fn peer_connected_falls_back_to_observed_when_no_declared() {
        let mut router = Router::new(Network::Mainnet);
        let observed = pub_addr("198.51.100.21:9030");
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
            addr: observed,
        };
        router.handle_event(event);
        let db = router.peer_db.lock().unwrap();
        assert!(db.get(observed).is_some());
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

        // Preload PeerDb with one bogus + one good entry (bypass the
        // Peers-arm filter so we exercise the defensive egress path).
        let good = pub_addr("78.46.3.10:9030");
        let bogus = pub_addr("192.168.1.42:9030"); // RFC 1918
        {
            let mut db = router.peer_db.lock().unwrap();
            db.record(PeerRecord {
                address: good,
                last_seen_ms: 2000,
                agent_name: "ergoref".into(),
                node_name: "".into(),
                version: (5, 0, 25),
                features: vec![],
            });
            db.record(PeerRecord {
                address: bogus,
                last_seen_ms: 3000, // more recent than `good`
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
        // A non-connected gossiped entry.
        let gossiped = pub_addr("78.46.30.42:9030");
        {
            let mut db = router.peer_db.lock().unwrap();
            db.record(PeerRecord {
                address: gossiped,
                last_seen_ms: 5000,
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
        // The gossiped entry should be present.
        assert!(specs.iter().any(|s| s.address == Some(gossiped)));
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
}

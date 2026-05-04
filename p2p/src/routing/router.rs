//! Message routing: forwarding decisions, mode filtering, peer registry.
//!
//! # Contract
//! - `handle_event`: given a `ProtocolEvent`, returns a list of `Action`s.
//!   Precondition: peer IDs in events are registered (or being disconnected).
//!   Postcondition: actions target only registered, non-disconnected peers.
//! - `register_peer` / peer removal on disconnect: manage the peer registry.
//! - Invariant: Inv table, request tracker, and sync tracker are consistent with
//!   the peer registry — no references to unregistered peers.

use crate::protocol::messages::ProtocolMessage;
use crate::protocol::peer::ProtocolEvent;
use crate::routing::inv_table::InvTable;
use crate::routing::latency::{LatencyTracker, LatencyStats};
use crate::routing::tracker::{RequestTracker, SyncTracker};
use crate::types::{Direction, PeerId, ProxyMode};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;

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
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            inv_table: InvTable::new(),
            request_tracker: RequestTracker::new(),
            sync_tracker: SyncTracker::new(),
            latency_tracker: LatencyTracker::new(),
            consumed_codes: HashSet::new(),
        }
    }

    /// Register a message code as consumed by the main crate's event stream.
    /// Unknown messages with this code will not be forwarded to peers.
    pub fn register_consumed_code(&mut self, code: u8) {
        self.consumed_codes.insert(code);
    }

    pub fn register_peer(&mut self, peer_id: PeerId, direction: Direction, mode: ProxyMode, addr: SocketAddr, rest_api_url: Option<String>) {
        self.peers.insert(peer_id, PeerEntry { direction, mode, addr, rest_api_url });
    }

    pub fn peer_addr(&self, peer_id: PeerId) -> Option<SocketAddr> {
        self.peers.get(&peer_id).map(|e| e.addr)
    }

    /// REST API URLs for all connected peers.
    pub fn peer_rest_urls(&self) -> Vec<(PeerId, SocketAddr, Option<String>)> {
        self.peers.iter()
            .map(|(pid, entry)| (*pid, entry.addr, entry.rest_api_url.clone()))
            .collect()
    }

    pub fn handle_event(&mut self, event: ProtocolEvent) -> Vec<Action> {
        match event {
            ProtocolEvent::PeerConnected { .. } => {
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

            ProtocolEvent::Message { peer_id, message } => {
                self.route_message(peer_id, message)
            }
        }
    }

    fn route_message(&mut self, source: PeerId, message: ProtocolMessage) -> Vec<Action> {
        self.request_tracker.sweep_expired(Duration::from_secs(60));

        let source_entry = match self.peers.get(&source) {
            Some(e) => e,
            None => return vec![],
        };
        let source_direction = source_entry.direction;
        let source_mode = source_entry.mode;

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
                        if inv_target == source { continue; }
                        Some(inv_target)
                    } else {
                        // Fallback: pick any outbound peer that isn't the source.
                        // Enables chain sync where modifier IDs come from SyncInfo, not Inv.
                        self.peers.iter()
                            .find(|(pid, entry)| **pid != source && entry.direction == Direction::Outbound)
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

            ProtocolMessage::ModifierResponse { modifier_type, modifiers } => {
                if modifier_type != 101 {
                    tracing::debug!(modifier_type, count = modifiers.len(), "routing non-header ModifierResponse");
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
                        if let Some((&outbound_id, _)) = self.peers.iter()
                            .find(|(pid, entry)| {
                                entry.direction == Direction::Outbound
                                    && self.sync_tracker.inbound_for(pid).is_none()
                            })
                        {
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
                vec![Action::Send {
                    target: source,
                    message: ProtocolMessage::Peers { body: vec![0x00] },
                }]
            }

            ProtocolMessage::Peers { .. } => {
                vec![]
            }

            ProtocolMessage::Unknown { code, body } => {
                if self.consumed_codes.contains(&code) {
                    return vec![];
                }

                let target_direction = match source_direction {
                    Direction::Outbound => Direction::Inbound,
                    Direction::Inbound => Direction::Outbound,
                };

                self.peers.iter()
                    .filter(|(pid, entry)| **pid != source && entry.direction == target_direction)
                    .map(|(pid, _)| Action::Send {
                        target: *pid,
                        message: ProtocolMessage::Unknown { code, body: body.clone() },
                    })
                    .collect()
            }
        }
    }

    pub fn outbound_peers(&self) -> Vec<PeerId> {
        self.peers.iter()
            .filter(|(_, e)| e.direction == Direction::Outbound)
            .map(|(pid, _)| *pid)
            .collect()
    }

    pub fn inbound_peers(&self) -> Vec<PeerId> {
        self.peers.iter()
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
}

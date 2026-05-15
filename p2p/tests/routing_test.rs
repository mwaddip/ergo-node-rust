use enr_p2p::routing::inv_table::InvTable;
use enr_p2p::routing::tracker::{RequestTracker, SyncTracker};
use enr_p2p::types::PeerId;
use std::net::SocketAddr;

fn dummy_addr() -> SocketAddr {
    "127.0.0.1:9000".parse().unwrap()
}

// --- Inv table tests ---

#[test]
fn inv_table_record_and_lookup() {
    let mut table = InvTable::new();
    let id = [0xaa; 32];
    table.record(id, PeerId(1));
    assert_eq!(table.lookup(&id), Some(PeerId(1)));
}

#[test]
fn inv_table_lookup_missing() {
    let table = InvTable::new();
    assert_eq!(table.lookup(&[0xbb; 32]), None);
}

#[test]
fn inv_table_latest_announcer_wins() {
    let mut table = InvTable::new();
    let id = [0xaa; 32];
    table.record(id, PeerId(1));
    table.record(id, PeerId(2));
    assert_eq!(table.lookup(&id), Some(PeerId(2)));
}

#[test]
fn inv_table_purge_peer() {
    let mut table = InvTable::new();
    table.record([0xaa; 32], PeerId(1));
    table.record([0xbb; 32], PeerId(1));
    table.record([0xcc; 32], PeerId(2));

    table.purge_peer(PeerId(1));

    assert_eq!(table.lookup(&[0xaa; 32]), None);
    assert_eq!(table.lookup(&[0xbb; 32]), None);
    assert_eq!(table.lookup(&[0xcc; 32]), Some(PeerId(2)));
}

#[test]
fn inv_table_invariant_no_disconnected_peers() {
    let mut table = InvTable::new();
    for i in 0..100 {
        let mut id = [0u8; 32];
        id[0] = i;
        table.record(id, PeerId(1));
    }
    table.purge_peer(PeerId(1));
    assert!(table.is_empty());
}

// --- Request tracker tests ---

#[test]
fn request_tracker_record_and_lookup() {
    let mut tracker = RequestTracker::new();
    let id = [0xaa; 32];
    tracker.record(id, PeerId(5));
    assert_eq!(tracker.lookup(&id), Some(PeerId(5)));
}

#[test]
fn request_tracker_fulfill_removes_entry() {
    let mut tracker = RequestTracker::new();
    let id = [0xaa; 32];
    tracker.record(id, PeerId(5));
    let requester = tracker.fulfill(&id);
    assert_eq!(requester, Some(PeerId(5)));
    assert_eq!(tracker.lookup(&id), None);
}

#[test]
fn request_tracker_fulfill_missing() {
    let mut tracker = RequestTracker::new();
    assert_eq!(tracker.fulfill(&[0xbb; 32]), None);
}

#[test]
fn request_tracker_purge_peer() {
    let mut tracker = RequestTracker::new();
    tracker.record([0xaa; 32], PeerId(1));
    tracker.record([0xbb; 32], PeerId(2));
    tracker.record([0xcc; 32], PeerId(1));

    tracker.purge_peer(PeerId(1));

    assert_eq!(tracker.lookup(&[0xaa; 32]), None);
    assert_eq!(tracker.lookup(&[0xbb; 32]), Some(PeerId(2)));
    assert_eq!(tracker.lookup(&[0xcc; 32]), None);
}

// --- Sync tracker tests ---

#[test]
fn sync_tracker_pair_and_lookup() {
    let mut tracker = SyncTracker::new();
    tracker.pair(PeerId(1), PeerId(10));
    assert_eq!(tracker.outbound_for(&PeerId(1)), Some(PeerId(10)));
    assert_eq!(tracker.inbound_for(&PeerId(10)), Some(PeerId(1)));
}

#[test]
fn sync_tracker_purge_inbound() {
    let mut tracker = SyncTracker::new();
    tracker.pair(PeerId(1), PeerId(10));
    tracker.purge_peer(PeerId(1));
    assert_eq!(tracker.outbound_for(&PeerId(1)), None);
    assert_eq!(tracker.inbound_for(&PeerId(10)), None);
}

#[test]
fn sync_tracker_purge_outbound() {
    let mut tracker = SyncTracker::new();
    tracker.pair(PeerId(1), PeerId(10));
    tracker.purge_peer(PeerId(10));
    assert_eq!(tracker.outbound_for(&PeerId(1)), None);
    assert_eq!(tracker.inbound_for(&PeerId(10)), None);
}

// --- RequestTracker expiry tests ---

#[test]
fn request_tracker_sweep_expired_removes_stale_entries() {
    use std::time::Duration;
    use std::thread;

    let mut tracker = RequestTracker::new();
    tracker.record([0xaa; 32], PeerId(1));
    tracker.record([0xbb; 32], PeerId(2));

    // Both entries should survive a sweep with a generous TTL
    tracker.sweep_expired(Duration::from_secs(60));
    assert_eq!(tracker.lookup(&[0xaa; 32]), Some(PeerId(1)));
    assert_eq!(tracker.lookup(&[0xbb; 32]), Some(PeerId(2)));

    // Wait just over the TTL, then sweep
    thread::sleep(Duration::from_millis(50));
    tracker.sweep_expired(Duration::from_millis(25));
    assert_eq!(tracker.lookup(&[0xaa; 32]), None);
    assert_eq!(tracker.lookup(&[0xbb; 32]), None);
}

#[test]
fn request_tracker_sweep_preserves_fresh_entries() {
    use std::time::Duration;
    use std::thread;

    let mut tracker = RequestTracker::new();
    tracker.record([0xaa; 32], PeerId(1));
    thread::sleep(Duration::from_millis(50));
    // Record a fresh entry after sleeping
    tracker.record([0xbb; 32], PeerId(2));

    // Sweep with TTL that kills the old one but spares the new one
    tracker.sweep_expired(Duration::from_millis(25));
    assert_eq!(tracker.lookup(&[0xaa; 32]), None);
    assert_eq!(tracker.lookup(&[0xbb; 32]), Some(PeerId(2)));
}

// --- Router tests ---

use enr_p2p::routing::router::{Router, Action};
use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::{Direction, Network, ProxyMode};

#[test]
fn router_inv_from_outbound_does_not_forward() {
    // Full-node semantics: Inv updates inv_table for our own request
    // routing, but is NOT relayed to other peers. See `router.rs` for
    // the reasoning (proxy-origin leftover, oversized-relay ban risk).
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(3), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    let event = ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![[0xaa; 32]] },
    };

    let actions = router.handle_event(event);
    assert!(actions.is_empty(), "Inv must not be forwarded to any peer");
}

#[test]
fn router_modifier_request_routes_via_inv_table() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });

    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(1)]);
}

#[test]
fn router_modifier_response_routes_to_requester() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 2,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });

    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(2)]);
}

#[test]
fn router_get_peers_handled_directly() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::GetPeers,
    });

    assert!(actions.iter().any(|a| matches!(a, Action::Send { target, message }
        if *target == PeerId(1) && matches!(message, ProtocolMessage::Peers { .. })
    )));
}

#[test]
fn router_light_mode_drops_sync_info() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Light, dummy_addr(), None, None);

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo { body: vec![1, 2, 3] },
    });
    assert!(actions.is_empty());
}

#[test]
fn router_full_mode_forwards_sync_info() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo { body: vec![1, 2, 3] },
    });
    assert!(!actions.is_empty());
}

#[test]
fn router_peer_disconnect_purges_state() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });

    router.handle_event(ProtocolEvent::PeerDisconnected {
        peer_id: PeerId(1),
        reason: "gone".into(),
    });

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });
    assert!(actions.is_empty());
}

// --- Action::Validate tests ---

#[test]
fn modifier_response_emits_validate_action() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    // Peer 2 requests via inv route
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Response arrives
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 1,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });

    // Should have both Validate and Send
    let has_validate = actions.iter().any(|a| matches!(a, Action::Validate { .. }));
    let has_send = actions.iter().any(|a| matches!(a, Action::Send { .. }));
    assert!(has_validate, "should emit Action::Validate");
    assert!(has_send, "should emit Action::Send to requester");
}

// --- ModifierRequest fallback routing tests ---

#[test]
fn router_modifier_request_no_inv_falls_back_to_outbound() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    // No Inv — peer 2 requests a modifier the router has never seen
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Should fall back to outbound peer 1
    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(1)]);
}

#[test]
fn router_modifier_request_no_inv_no_outbound_drops() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    // No outbound peers at all, no inv entry
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    assert!(actions.is_empty(), "no outbound peers means no fallback target");
}

#[test]
fn router_modifier_request_with_inv_ignores_fallback() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(3), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    // Peer 2 announces, so inv table maps to peer 2
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::Inv { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Peer 3 requests — should route to peer 2 (inv hit), not peer 1 (fallback)
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(3),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(2)], "inv table target should be used, not fallback");
}

#[test]
fn router_fallback_request_response_reaches_requester() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    // No inv — fallback routes request to peer 1
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Peer 1 responds — should route back to peer 2 (the original requester)
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 1,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });

    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(2)], "response should route back to original requester");
}

// --- Consumed-code routing tests ---

#[test]
fn router_consumed_code_suppresses_forwarding() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_consumed_code(76); // snapshot manifest

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 76, body: vec![1, 2, 3] },
    });
    assert!(actions.is_empty(), "consumed code should not produce Send actions");
}

#[test]
fn router_unregistered_code_still_forwards() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_consumed_code(76);

    // Code 99 is NOT consumed — should forward normally
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 99, body: vec![1, 2, 3] },
    });
    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(2)]);
}

#[test]
fn router_multiple_consumed_codes() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    for code in [76, 78, 80, 90, 91] {
        router.register_consumed_code(code);
    }

    // All consumed codes should be suppressed
    for code in [76, 78, 80, 90, 91] {
        let actions = router.handle_event(ProtocolEvent::Message {
            peer_id: PeerId(1),
            message: ProtocolMessage::Unknown { code, body: vec![] },
        });
        assert!(actions.is_empty(), "consumed code {} should be suppressed", code);
    }

    // Non-consumed code should still forward
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 55, body: vec![] },
    });
    assert!(!actions.is_empty(), "non-consumed code should forward");
}

// =============================================================================
// Adversarial tests — RequestTracker
// =============================================================================

/// Test 1: Fill tracker to cap, trigger eviction, then sweep.
#[test]
fn request_tracker_cap_eviction_and_sweep() {
    use std::time::Duration;
    use std::thread;

    let cap = 100;
    let mut tracker = RequestTracker::with_capacity(cap);

    // Fill to cap
    for i in 0..cap {
        let mut id = [0u8; 32];
        id[0..8].copy_from_slice(&(i as u64).to_le_bytes());
        tracker.record(id, PeerId(1));
    }
    assert_eq!(tracker.len(), cap);

    // One more — should trigger eviction of ~25%
    let overflow_id = [0xff; 32];
    tracker.record(overflow_id, PeerId(2));

    // After eviction + insert: should be around 75% of cap + 1
    let expected_max = cap - (cap / 4) + 1;
    assert!(tracker.len() <= expected_max, "len {} should be <= {}", tracker.len(), expected_max);
    assert!(tracker.len() > cap / 2, "eviction removed too many entries");
    assert_eq!(tracker.lookup(&overflow_id), Some(PeerId(2)), "new entry must survive eviction");

    // Sleep past a short TTL, then sweep — all old entries should die
    thread::sleep(Duration::from_millis(30));

    // Record a fresh entry after the sleep
    let fresh_id = [0xfe; 32];
    tracker.record(fresh_id, PeerId(3));

    tracker.sweep_expired(Duration::from_millis(15));

    // Only the entry recorded after sleep should survive
    assert_eq!(tracker.lookup(&fresh_id), Some(PeerId(3)), "fresh entry should survive sweep");
    assert_eq!(tracker.lookup(&overflow_id), None, "old entry should be swept");
    assert_eq!(tracker.len(), 1, "only the fresh entry should remain");
}

/// Test 2: Recording same modifier ID twice overwrites, doesn't duplicate.
#[test]
fn request_tracker_duplicate_id_overwrites() {
    let mut tracker = RequestTracker::new();
    let id = [0xaa; 32];

    tracker.record(id, PeerId(1));
    assert_eq!(tracker.len(), 1);

    tracker.record(id, PeerId(2));
    assert_eq!(tracker.len(), 1, "duplicate insert should not grow pending count");

    let result = tracker.fulfill(&id);
    assert_eq!(result, Some(PeerId(2)), "fulfill should return the latest peer");

    // Second fulfill proves no duplicate lurking
    assert_eq!(tracker.fulfill(&id), None, "entry should be gone after single fulfill");
}

/// Test 3: Fulfill after sweep evicted the entry.
#[test]
fn request_tracker_fulfill_after_sweep() {
    use std::time::Duration;
    use std::thread;

    let mut tracker = RequestTracker::new();
    tracker.record([0xaa; 32], PeerId(1));

    thread::sleep(Duration::from_millis(30));
    tracker.sweep_expired(Duration::from_millis(15));

    assert_eq!(tracker.fulfill(&[0xaa; 32]), None, "fulfill after sweep should return None");
}

/// Test 4: Purge a peer that has no entries — no panic.
#[test]
fn request_tracker_purge_nonexistent_peer() {
    let mut tracker = RequestTracker::new();
    tracker.record([0xaa; 32], PeerId(1));

    // Purge a peer that was never recorded
    tracker.purge_peer(PeerId(999));

    // Original entry should be untouched
    assert_eq!(tracker.lookup(&[0xaa; 32]), Some(PeerId(1)));
}

/// Test 5: Sweep on empty tracker — no panic.
#[test]
fn request_tracker_sweep_empty() {
    use std::time::Duration;

    let mut tracker = RequestTracker::new();
    tracker.sweep_expired(Duration::from_millis(10));
    assert_eq!(tracker.len(), 0);
}

/// Test 6: Fulfill on empty tracker — returns None.
#[test]
fn request_tracker_fulfill_empty() {
    let mut tracker = RequestTracker::new();
    assert_eq!(tracker.fulfill(&[0xaa; 32]), None);
}

// =============================================================================
// Adversarial tests — Router consumed codes
// =============================================================================

/// Test 7: Boundary code values 0 and 255.
#[test]
fn router_consumed_code_boundary_values() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    router.register_consumed_code(0);
    router.register_consumed_code(255);

    // Code 0 — consumed, should suppress
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 0, body: vec![1] },
    });
    assert!(actions.is_empty(), "code 0 should be suppressed");

    // Code 255 — consumed, should suppress
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 255, body: vec![2] },
    });
    assert!(actions.is_empty(), "code 255 should be suppressed");

    // Code 1 — NOT consumed, should forward to inbound
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 1, body: vec![3] },
    });
    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(2)], "non-consumed code 1 should forward");
}

/// Test 8: Register same code twice — idempotent, no panic.
#[test]
fn router_consumed_code_double_register() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    router.register_consumed_code(76);
    router.register_consumed_code(76);

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 76, body: vec![1, 2, 3] },
    });
    assert!(actions.is_empty(), "double-registered code should still suppress");

    // Non-consumed code still works
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 77, body: vec![4, 5] },
    });
    assert!(!actions.is_empty(), "non-consumed code should still forward after double-register");
}

/// Test 9: Consumed code from outbound peer — should suppress, not forward to inbound.
#[test]
fn router_consumed_code_from_outbound_suppresses() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(3), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    router.register_consumed_code(90);

    // From outbound — should suppress entirely, not forward to any inbound
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 90, body: vec![1] },
    });
    assert!(actions.is_empty(), "consumed code from outbound should suppress, not forward to inbound");
}

/// Test 10: Non-consumed unknown still forwards after registration (regression).
#[test]
fn router_non_consumed_unknown_unaffected_by_registration() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);
    router.register_peer(PeerId(3), Direction::Inbound, ProxyMode::Full, dummy_addr(), None, None);

    router.register_consumed_code(76);
    router.register_consumed_code(90);

    // Code 99 — not consumed, should forward from outbound to all inbound
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Unknown { code: 99, body: vec![7, 8, 9] },
    });
    let mut targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    targets.sort_by_key(|p| p.0);
    assert_eq!(targets, vec![PeerId(2), PeerId(3)], "non-consumed code should forward to opposite direction");

    // Also verify from inbound to outbound
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::Unknown { code: 99, body: vec![10] },
    });
    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(1)], "non-consumed code from inbound should forward to outbound");
}

// =============================================================================
// Adversarial tests — RequestTracker + sweep timing
// =============================================================================

/// Test 11: Interleaved record and sweep with precise TTL boundaries.
#[test]
fn request_tracker_interleaved_record_and_sweep() {
    use std::time::Duration;
    use std::thread;

    let mut tracker = RequestTracker::new();

    // Record entry A
    tracker.record([0xaa; 32], PeerId(1));

    thread::sleep(Duration::from_millis(30));

    // Record entry B (fresher)
    tracker.record([0xbb; 32], PeerId(2));

    // Sweep with TTL=20ms — A is ~30ms old (dead), B is ~0ms old (alive)
    tracker.sweep_expired(Duration::from_millis(20));
    assert_eq!(tracker.lookup(&[0xaa; 32]), None, "entry A should be expired");
    assert_eq!(tracker.lookup(&[0xbb; 32]), Some(PeerId(2)), "entry B should survive");

    // Sweep with TTL=0ms — everything dies
    thread::sleep(Duration::from_millis(1));
    tracker.sweep_expired(Duration::from_millis(0));
    assert_eq!(tracker.lookup(&[0xbb; 32]), None, "entry B should be expired with TTL=0");
    assert_eq!(tracker.len(), 0, "tracker should be empty after TTL=0 sweep");
}

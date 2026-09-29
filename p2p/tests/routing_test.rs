//! Router behaviour per `facts/p2p-routing.md`. The router answers the peer
//! whose message it is handling, emits received modifiers for validation, or
//! does nothing. It never forwards a message from one peer to another.

use enr_p2p::blacklist::Blacklist;
use enr_p2p::peer_db::{MemoryPeerStorage, PeerDb, DEFAULT_CAP};
use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::routing::router::{Action, Router};
use enr_p2p::types::{Direction, Network, PeerId, ProxyMode};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

fn dummy_addr() -> SocketAddr {
    "127.0.0.1:9000".parse().unwrap()
}

/// A router with one outbound peer (1) and two inbound peers (2, 3), all in
/// full mode.
fn router_with_both_directions() -> Router {
    let mut router = Router::new(Network::Mainnet);
    for (peer, direction) in [
        (PeerId(1), Direction::Outbound),
        (PeerId(2), Direction::Inbound),
        (PeerId(3), Direction::Inbound),
    ] {
        router.register_peer(peer, direction, ProxyMode::Full, dummy_addr(), None, None);
    }
    router
}

fn message(peer_id: PeerId, message: ProtocolMessage) -> ProtocolEvent {
    ProtocolEvent::Message { peer_id, message }
}

fn inv(ids: Vec<[u8; 32]>) -> ProtocolMessage {
    ProtocolMessage::Inv {
        modifier_type: 2,
        ids,
    }
}

fn request(modifier_type: u8, ids: Vec<[u8; 32]>) -> ProtocolMessage {
    ProtocolMessage::ModifierRequest { modifier_type, ids }
}

/// A local-serve hook that has exactly `id`, as `data`.
fn serve_only(id: [u8; 32], data: Vec<u8>) -> enr_p2p::routing::router::LocalServeFn {
    Arc::new(move |_, requested| (*requested == id).then(|| data.clone()))
}

/// The `Action::Send` targets among `actions`.
fn send_targets(actions: &[Action]) -> Vec<PeerId> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send { target, .. } => Some(*target),
            Action::Validate { .. } => None,
        })
        .collect()
}

// --- Inv and SyncInfo: read by sync and the mempool task, not the router ---

#[test]
fn router_inv_produces_no_actions() {
    // Sync and the mempool task read Inv from the subscriber. The router
    // neither passes it on nor remembers who announced what.
    let mut router = router_with_both_directions();
    for source in [PeerId(1), PeerId(2)] {
        let actions = router.handle_event(message(source, inv(vec![[0xaa; 32]])));
        assert!(actions.is_empty(), "Inv from {source}: {actions:?}");
    }
}

#[test]
fn router_light_mode_drops_sync_info() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(
        PeerId(1),
        Direction::Outbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );
    router.register_peer(
        PeerId(2),
        Direction::Inbound,
        ProxyMode::Light,
        dummy_addr(),
        None,
        None,
    );

    let actions = router.handle_event(message(
        PeerId(2),
        ProtocolMessage::SyncInfo {
            body: vec![1, 2, 3],
        },
    ));
    assert!(actions.is_empty());
}

#[test]
fn router_full_mode_sync_info_is_not_paired() {
    // An inbound peer's SyncInfo is not passed to an outbound peer, nor the
    // outbound peer's back: the outbound peer would see two SyncInfos from
    // us describing different chains. Sync answers each peer itself.
    let mut router = router_with_both_directions();
    for source in [PeerId(2), PeerId(1)] {
        let actions = router.handle_event(message(
            source,
            ProtocolMessage::SyncInfo {
                body: vec![1, 2, 3],
            },
        ));
        assert!(actions.is_empty(), "SyncInfo from {source}: {actions:?}");
    }
}

// --- GetPeers ---

#[test]
fn router_get_peers_handled_directly() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(
        PeerId(1),
        Direction::Outbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );

    let actions = router.handle_event(message(PeerId(1), ProtocolMessage::GetPeers));

    assert!(actions
        .iter()
        .any(|a| matches!(a, Action::Send { target, message }
            if *target == PeerId(1) && matches!(message, ProtocolMessage::Peers { .. })
        )));
}

// --- ModifierRequest: answered from the local-serve hook, never relayed ---

#[test]
fn router_modifier_request_is_not_relayed_to_the_announcer() {
    let mut router = router_with_both_directions();
    router.handle_event(message(PeerId(1), inv(vec![[0xaa; 32]])));

    let actions = router.handle_event(message(PeerId(2), request(2, vec![[0xaa; 32]])));
    assert!(actions.is_empty(), "{actions:?}");
}

#[test]
fn router_modifier_request_miss_is_not_relayed_to_an_outbound_peer() {
    // The hook has nothing the requester asks for, and an outbound peer is
    // connected: the miss gets no answer, and no request goes out.
    let mut router = router_with_both_directions();
    router.set_local_serve(serve_only([0x01; 32], vec![1]));

    let actions = router.handle_event(message(PeerId(2), request(102, vec![[0xaa; 32]])));
    assert!(actions.is_empty(), "{actions:?}");
}

#[test]
fn router_modifier_request_without_hook_gets_nothing() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(
        PeerId(1),
        Direction::Inbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );

    let actions = router.handle_event(message(PeerId(1), request(1, vec![[0xaa; 32]])));
    assert!(actions.is_empty());
}

#[test]
fn router_modifier_request_mixed_hit_and_miss_answers_the_hits_to_the_source() {
    let mut router = router_with_both_directions();
    // Peer 2 announced A; the node has only B.
    router.handle_event(message(PeerId(2), inv(vec![[0xaa; 32]])));
    router.set_local_serve(serve_only([0xbb; 32], vec![7, 7, 7]));

    let actions = router.handle_event(message(
        PeerId(3),
        request(2, vec![[0xaa; 32], [0xbb; 32], [0xcc; 32]]),
    ));

    assert_eq!(actions.len(), 1, "one response, nothing for the misses");
    match &actions[0] {
        Action::Send {
            target,
            message:
                ProtocolMessage::ModifierResponse {
                    modifier_type,
                    modifiers,
                },
        } => {
            assert_eq!(*target, PeerId(3), "the response goes to the requester");
            assert_eq!(*modifier_type, 2);
            assert_eq!(modifiers.as_slice(), &[([0xbb; 32], vec![7, 7, 7])]);
        }
        other => panic!("expected a ModifierResponse, got {other:?}"),
    }
}

// --- ModifierResponse: validated, never forwarded ---

#[test]
fn modifier_response_emits_validate_only() {
    let mut router = router_with_both_directions();

    let actions = router.handle_event(message(
        PeerId(1),
        ProtocolMessage::ModifierResponse {
            modifier_type: 102,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3]), ([0xbb; 32], vec![4])],
        },
    ));

    assert_eq!(actions.len(), 2, "one Validate per modifier: {actions:?}");
    for (action, (expected_id, expected_data)) in actions
        .iter()
        .zip([([0xaa; 32], vec![1, 2, 3]), ([0xbb; 32], vec![4])])
    {
        match action {
            Action::Validate {
                modifier_type,
                id,
                data,
                peer_id,
            } => {
                assert_eq!(*modifier_type, 102);
                assert_eq!(*id, expected_id);
                assert_eq!(*data, expected_data);
                assert_eq!(*peer_id, PeerId(1), "attributed to the sender");
            }
            other => panic!("expected Validate, got {other:?}"),
        }
    }
}

#[test]
fn router_modifier_response_is_not_forwarded_to_a_requester() {
    // Peer 2 asked for A, which the node doesn't have. When peer 1 later
    // delivers A, it is validated; peer 2 is not sent a copy.
    let mut router = router_with_both_directions();
    router.handle_event(message(PeerId(1), inv(vec![[0xaa; 32]])));
    router.handle_event(message(PeerId(2), request(2, vec![[0xaa; 32]])));

    let actions = router.handle_event(message(
        PeerId(1),
        ProtocolMessage::ModifierResponse {
            modifier_type: 2,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    ));

    assert!(send_targets(&actions).is_empty(), "{actions:?}");
    assert!(matches!(
        actions.as_slice(),
        [Action::Validate { peer_id, .. }] if *peer_id == PeerId(1)
    ));
}

// --- Unknown codes: dropped, not penalized ---

/// Codes the typed codec doesn't know: UTXO snapshot (76–81) and NiPoPoW
/// (90, 91), which reach their handlers through the subscriber, and codes
/// nobody sends today, as a newer protocol version might.
const UNKNOWN_CODES: [u8; 12] = [0, 76, 77, 78, 79, 80, 81, 90, 91, 99, 200, 255];

#[test]
fn router_unknown_code_is_dropped_from_either_direction() {
    let mut router = router_with_both_directions();
    for source in [PeerId(1), PeerId(2)] {
        for code in UNKNOWN_CODES {
            let actions = router.handle_event(message(
                source,
                ProtocolMessage::Unknown {
                    code,
                    body: vec![1, 2, 3],
                },
            ));
            assert!(
                actions.is_empty(),
                "code {code} from {source} produced {actions:?}"
            );
        }
    }
}

#[test]
fn router_unknown_code_does_not_penalize_the_source() {
    let blacklist = Arc::new(Blacklist::new());
    let peer_db = PeerDb::new(
        Box::new(MemoryPeerStorage::new()),
        blacklist.clone(),
        DEFAULT_CAP,
        HashSet::new(),
    )
    .unwrap();
    let mut router = Router::with_peer_db(
        Arc::new(Mutex::new(peer_db)),
        blacklist.clone(),
        64,
        Network::Mainnet,
        true,
    );
    let outbound: SocketAddr = "78.46.90.1:9030".parse().unwrap();
    let inbound: SocketAddr = "78.46.90.2:51234".parse().unwrap();
    router.register_peer(
        PeerId(1),
        Direction::Outbound,
        ProxyMode::Full,
        outbound,
        None,
        None,
    );
    router.register_peer(
        PeerId(2),
        Direction::Inbound,
        ProxyMode::Full,
        inbound,
        None,
        None,
    );

    for source in [PeerId(1), PeerId(2)] {
        for code in UNKNOWN_CODES {
            router.handle_event(message(
                source,
                ProtocolMessage::Unknown {
                    code,
                    body: vec![0xff; 16],
                },
            ));
        }
    }

    assert!(blacklist.list().is_empty(), "{:?}", blacklist.list());
    assert_eq!(router.peer_count(), 2, "both peers stay registered");
}

// --- Disconnect ---

#[test]
fn router_peer_disconnect_unregisters_the_peer() {
    let mut router = Router::new(Network::Mainnet);
    router.register_peer(
        PeerId(1),
        Direction::Outbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );
    router.register_peer(
        PeerId(2),
        Direction::Inbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );

    router.handle_event(ProtocolEvent::PeerDisconnected {
        peer_id: PeerId(1),
        reason: "gone".into(),
    });

    assert_eq!(router.peer_count(), 1);
    assert!(router.outbound_peers().is_empty());
    assert_eq!(router.inbound_peers(), vec![PeerId(2)]);
    // A message still in flight from the departed peer gets nothing.
    let actions = router.handle_event(message(PeerId(1), ProtocolMessage::GetPeers));
    assert!(actions.is_empty(), "{actions:?}");
}

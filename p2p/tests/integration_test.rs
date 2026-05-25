use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::routing::router::{Action, Router};
use enr_p2p::types::{Direction, Network, PeerId, ProxyMode};
use std::net::SocketAddr;

fn dummy_addr() -> SocketAddr {
    "127.0.0.1:9000".parse().unwrap()
}

#[test]
fn full_tx_relay_scenario() {
    let mut router = Router::new(Network::Mainnet);
    let outbound = PeerId(1);
    let inbound = PeerId(2);
    router.register_peer(
        outbound,
        Direction::Outbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );
    router.register_peer(
        inbound,
        Direction::Inbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );

    let tx_id = [0x42; 32];

    // Outbound announces tx — full-node semantics: router records the
    // (id, peer) entry in its inv_table but does NOT re-broadcast the
    // announcement. Peers are expected to talk to each other directly.
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: outbound,
        message: ProtocolMessage::Inv {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    assert!(actions.is_empty(), "Inv must not be relayed to other peers");

    // A request for the same tx from the inbound side still resolves via
    // the inv_table to the source peer — the record path stays intact.
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: inbound,
        message: ProtocolMessage::ModifierRequest {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, .. } if *target == outbound));

    // Outbound delivers — ModifierResponse still fans Validate + the
    // forwarded copy to the original requester (tracked by request_tracker).
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: outbound,
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 2,
            modifiers: vec![(tx_id, vec![0xde, 0xad, 0xbe, 0xef])],
        },
    });
    assert_eq!(actions.len(), 2); // Validate + Send
    assert!(actions.iter().any(|a| matches!(a, Action::Validate { .. })));
    assert!(actions
        .iter()
        .any(|a| matches!(a, Action::Send { target, .. } if *target == inbound)));
}

#[test]
fn disconnect_cleanup_scenario() {
    let mut router = Router::new(Network::Mainnet);
    let out1 = PeerId(1);
    let out2 = PeerId(2);
    let inb = PeerId(3);
    router.register_peer(
        out1,
        Direction::Outbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );
    router.register_peer(
        out2,
        Direction::Outbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );
    router.register_peer(
        inb,
        Direction::Inbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );

    let tx_id = [0x55; 32];

    router.handle_event(ProtocolEvent::Message {
        peer_id: out1,
        message: ProtocolMessage::Inv {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });

    router.handle_event(ProtocolEvent::PeerDisconnected {
        peer_id: out1,
        reason: "gone".into(),
    });

    // Inv entry for out1 was purged on disconnect. With fallback routing,
    // the request goes to out2 (the remaining outbound peer).
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: inb,
        message: ProtocolMessage::ModifierRequest {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, .. } if *target == out2));
}

#[test]
fn inv_does_not_fanout() {
    // Full-node semantics: receiving an Inv updates the router's inv_table
    // so that subsequent ModifierRequests can be routed to the announcing
    // peer, but the Inv itself is NOT relayed to any other peer. Relaying
    // was leftover from the transparent-proxy origin of this crate and
    // is protocol-incorrect for a full node (peers announce to each other
    // directly, and relayed Invs can exceed the 400-modifier cap).
    let mut router = Router::new(Network::Mainnet);
    let outbound = PeerId(1);
    router.register_peer(
        outbound,
        Direction::Outbound,
        ProxyMode::Full,
        dummy_addr(),
        None,
        None,
    );

    for i in 2..=5 {
        router.register_peer(
            PeerId(i),
            Direction::Inbound,
            ProxyMode::Full,
            dummy_addr(),
            None,
            None,
        );
    }

    let tx_id = [0xaa; 32];
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: outbound,
        message: ProtocolMessage::Inv {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });

    assert!(actions.is_empty(), "Inv must not fanout to other peers");

    // Inv was recorded — an inbound ModifierRequest for this tx still
    // routes to the announcing outbound peer via the inv_table lookup.
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, .. } if *target == outbound));
}

#[test]
fn light_mode_blocks_sync() {
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

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo {
            body: vec![1, 2, 3],
        },
    });
    assert!(actions.is_empty());
}

#[test]
fn full_mode_sync_flow() {
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

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo {
            body: vec![1, 2, 3],
        },
    });
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, .. } if *target == PeerId(1)));

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::SyncInfo {
            body: vec![4, 5, 6],
        },
    });
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, .. } if *target == PeerId(2)));
}

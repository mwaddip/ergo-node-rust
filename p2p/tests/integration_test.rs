//! Multi-message router scenarios (`facts/p2p-routing.md`): whatever peers
//! announce, request and deliver, nothing one peer sends is passed on to
//! another.

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::routing::router::{Action, Router};
use enr_p2p::types::{Direction, Network, PeerId, ProxyMode};
use std::net::SocketAddr;
use std::sync::Arc;

fn dummy_addr() -> SocketAddr {
    "127.0.0.1:9000".parse().unwrap()
}

#[test]
fn tx_announce_request_deliver_scenario() {
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

    // Outbound announces the tx. The mempool task reads the Inv from the
    // subscriber; the router does nothing with it.
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: outbound,
        message: ProtocolMessage::Inv {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    assert!(actions.is_empty(), "Inv must not be relayed to other peers");

    // The inbound peer asks us for it. We don't have it, so it gets no
    // answer, and the request is not passed to the announcer.
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: inbound,
        message: ProtocolMessage::ModifierRequest {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    assert!(actions.is_empty(), "{actions:?}");

    // Outbound delivers it (to our own request): validation only, and no
    // copy for the inbound peer.
    let tx_bytes = vec![0xde, 0xad, 0xbe, 0xef];
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: outbound,
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 2,
            modifiers: vec![(tx_id, tx_bytes.clone())],
        },
    });
    assert!(matches!(
        actions.as_slice(),
        [Action::Validate { id, peer_id, .. }] if *id == tx_id && *peer_id == outbound
    ));

    // Once the node has the tx, the hook serves it, to the requester only.
    let served = tx_bytes.clone();
    router.set_local_serve(Arc::new(move |_, id| {
        (*id == tx_id).then(|| served.clone())
    }));
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: inbound,
        message: ProtocolMessage::ModifierRequest {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    match actions.as_slice() {
        [Action::Send {
            target,
            message: ProtocolMessage::ModifierResponse { modifiers, .. },
        }] => {
            assert_eq!(*target, inbound);
            assert_eq!(modifiers.as_slice(), &[(tx_id, tx_bytes)]);
        }
        other => panic!("expected one response to the requester, got {other:?}"),
    }
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
    assert_eq!(router.outbound_peers(), vec![out2]);

    // The announcer is gone, and the remaining outbound peer is not asked
    // on the requester's behalf either.
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: inb,
        message: ProtocolMessage::ModifierRequest {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    assert!(actions.is_empty(), "{actions:?}");
}

#[test]
fn inv_does_not_fanout() {
    // Peers announce to each other directly. A relayed Inv is also
    // protocol-incorrect for a full node: relayed batches can exceed the
    // 400-modifier cap.
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

    // Nor does the announcement make the announcer a target: an inbound
    // request for the tx is not sent to it.
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest {
            modifier_type: 2,
            ids: vec![tx_id],
        },
    });
    assert!(actions.is_empty(), "{actions:?}");
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
fn full_mode_sync_exchange_is_never_paired() {
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

    // The inbound peer's SyncInfo is not sent to the outbound peer, and the
    // outbound peer's is not sent back: sync answers each peer itself.
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo {
            body: vec![1, 2, 3],
        },
    });
    assert!(actions.is_empty(), "{actions:?}");

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::SyncInfo {
            body: vec![4, 5, 6],
        },
    });
    assert!(actions.is_empty(), "{actions:?}");
}

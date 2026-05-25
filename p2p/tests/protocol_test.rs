use enr_p2p::protocol::messages::{MessageCode, ProtocolMessage};
use enr_p2p::transport::frame::Frame;
use enr_p2p::transport::vlq;

#[test]
fn parse_inv_message() {
    let mut body = Vec::new();
    body.push(2);
    vlq::write_vlq(&mut body, 1);
    body.extend_from_slice(&[0xaa; 32]);

    let frame = Frame {
        code: MessageCode::INV,
        body,
    };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();

    match msg {
        ProtocolMessage::Inv { modifier_type, ids } => {
            assert_eq!(modifier_type, 2);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0], [0xaa; 32]);
        }
        _ => panic!("Expected Inv"),
    }
}

#[test]
fn parse_modifier_request() {
    let mut body = Vec::new();
    body.push(2);
    vlq::write_vlq(&mut body, 2);
    body.extend_from_slice(&[0xbb; 32]);
    body.extend_from_slice(&[0xcc; 32]);

    let frame = Frame {
        code: MessageCode::MODIFIER_REQUEST,
        body,
    };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();

    match msg {
        ProtocolMessage::ModifierRequest { modifier_type, ids } => {
            assert_eq!(modifier_type, 2);
            assert_eq!(ids.len(), 2);
        }
        _ => panic!("Expected ModifierRequest"),
    }
}

#[test]
fn parse_modifier_response() {
    let mut body = Vec::new();
    body.push(2);
    vlq::write_vlq(&mut body, 1);
    body.extend_from_slice(&[0xdd; 32]);
    vlq::write_vlq(&mut body, 4);
    body.extend_from_slice(&[1, 2, 3, 4]);

    let frame = Frame {
        code: MessageCode::MODIFIER_RESPONSE,
        body,
    };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();

    match msg {
        ProtocolMessage::ModifierResponse {
            modifier_type,
            modifiers,
        } => {
            assert_eq!(modifier_type, 2);
            assert_eq!(modifiers.len(), 1);
            assert_eq!(modifiers[0].0, [0xdd; 32]);
            assert_eq!(modifiers[0].1, vec![1, 2, 3, 4]);
        }
        _ => panic!("Expected ModifierResponse"),
    }
}

#[test]
fn parse_get_peers() {
    let frame = Frame {
        code: MessageCode::GET_PEERS,
        body: vec![],
    };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();
    assert!(matches!(msg, ProtocolMessage::GetPeers));
}

#[test]
fn parse_sync_info_is_opaque() {
    let body = vec![0x01, 0x02, 0x03, 0x04, 0x05];
    let frame = Frame {
        code: MessageCode::SYNC_INFO,
        body: body.clone(),
    };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();
    match msg {
        ProtocolMessage::SyncInfo { body: b } => assert_eq!(b, body),
        _ => panic!("Expected SyncInfo"),
    }
}

#[test]
fn parse_unknown_code_preserved() {
    let frame = Frame {
        code: 99,
        body: vec![0xff],
    };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();
    match msg {
        ProtocolMessage::Unknown { code, body } => {
            assert_eq!(code, 99);
            assert_eq!(body, vec![0xff]);
        }
        _ => panic!("Expected Unknown"),
    }
}

#[test]
fn inv_to_frame_roundtrip() {
    let mut ids_body = Vec::new();
    ids_body.push(2);
    vlq::write_vlq(&mut ids_body, 1);
    ids_body.extend_from_slice(&[0xee; 32]);

    let frame = Frame {
        code: MessageCode::INV,
        body: ids_body,
    };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();
    let back = msg.to_frame();
    assert_eq!(back.code, frame.code);
    assert_eq!(back.body, frame.body);
}

// --- Peer state machine tests ---

use enr_p2p::protocol::peer::{PeerState, PeerStateMachine, ProtocolEvent};
use enr_p2p::transport::handshake::PeerSpec;
use enr_p2p::types::{Direction, PeerId, Version};
use std::net::SocketAddr;

fn dummy_addr() -> SocketAddr {
    "127.0.0.1:9000".parse().unwrap()
}

#[test]
fn peer_starts_connecting() {
    let peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    assert_eq!(peer.state(), PeerState::Connecting);
}

#[test]
fn peer_transitions_to_handshaking() {
    let mut peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    peer.set_handshaking();
    assert_eq!(peer.state(), PeerState::Handshaking);
}

#[test]
fn peer_transitions_to_active() {
    let mut peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    peer.set_handshaking();
    let spec = PeerSpec {
        agent: "test".into(),
        version: Version::new(6, 0, 3),
        name: "test-node".into(),
        address: None,
        features: vec![],
    };
    let event = peer.set_active(spec, dummy_addr());
    assert_eq!(peer.state(), PeerState::Active);
    match event {
        ProtocolEvent::PeerConnected {
            peer_id, direction, ..
        } => {
            assert_eq!(peer_id, PeerId(1));
            assert_eq!(direction, Direction::Outbound);
        }
        _ => panic!("Expected PeerConnected"),
    }
}

#[test]
fn peer_transitions_to_disconnected() {
    let mut peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    peer.set_handshaking();
    let spec = PeerSpec {
        agent: "test".into(),
        version: Version::new(6, 0, 3),
        name: "test-node".into(),
        address: None,
        features: vec![],
    };
    peer.set_active(spec, dummy_addr());
    let event = peer.set_disconnected("test reason".into());
    assert_eq!(peer.state(), PeerState::Disconnected);
    match event {
        ProtocolEvent::PeerDisconnected { peer_id, reason } => {
            assert_eq!(peer_id, PeerId(1));
            assert_eq!(reason, "test reason");
        }
        _ => panic!("Expected PeerDisconnected"),
    }
}

#[test]
fn peer_transitions_to_failed() {
    let mut peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    peer.set_handshaking();
    peer.set_failed("bad version".into());
    assert_eq!(peer.state(), PeerState::Failed);
}

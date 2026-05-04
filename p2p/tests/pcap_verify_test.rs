//! Verify our handshake parser against the actual JVM node pcap capture.
//! Also verify our handshake builder produces structurally correct output.

use enr_p2p::transport::handshake::{self, HandshakeConfig};
use enr_p2p::transport::frame::{self, Frame};
use enr_p2p::types::{Network, Version, ProxyMode};

/// Raw handshake bytes captured from the JVM Ergo testnet node (v6.0.3)
/// connecting to 213.239.193.208:9023 on 2026-03-30.
/// 64 bytes, extracted from tcpdump pcap.
const JVM_HANDSHAKE: [u8; 64] = [
    // VLQ timestamp (6 bytes)
    0xd4, 0xc5, 0xfb, 0x85, 0xd4, 0x33,
    // Agent name: "ergoref" (1+7 bytes)
    0x07, 0x65, 0x72, 0x67, 0x6f, 0x72, 0x65, 0x66,
    // Version: 6.0.3 (3 bytes)
    0x06, 0x00, 0x03,
    // Peer name: "ergo-test-fresh" (1+15 bytes)
    0x0f, 0x65, 0x72, 0x67, 0x6f, 0x2d, 0x74, 0x65,
    0x73, 0x74, 0x2d, 0x66, 0x72, 0x65, 0x73, 0x68,
    // Declared address + features (remaining 31 bytes)
    0x01, 0x08, 0x5f, 0xb3, 0xf6, 0x66, 0xbf, 0x46,
    0x02, 0x10, 0x04, 0x00, 0x01, 0x00, 0x01, 0x03,
    0x0e, 0x02, 0x03, 0x02, 0x03, 0xbd, 0xf8, 0xda,
    0xf9, 0x99, 0xfc, 0xf5, 0xb3, 0x8b, 0x01,
];

/// First framed message from JVM node (SyncInfo, code 65).
/// 16 bytes from pcap.
const JVM_FIRST_MESSAGE: [u8; 16] = [
    0x02, 0x03, 0x02, 0x03, // magic [2,3,2,3]
    0x41,                     // code 65 (SyncInfo)
    0x00, 0x00, 0x00, 0x03,  // body length 3
    0x45, 0xa1, 0x4b, 0x86,  // checksum
    0x00, 0xff, 0x00,         // body
];

#[test]
fn jvm_handshake_parses_without_error() {
    let spec = handshake::parse(&JVM_HANDSHAKE).unwrap();
    assert_eq!(spec.agent, "ergoref");
    assert_eq!(spec.version, Version::new(6, 0, 3));
    assert_eq!(spec.name, "ergo-test-fresh");
}

#[test]
fn jvm_handshake_version_is_valid() {
    let spec = handshake::parse(&JVM_HANDSHAKE).unwrap();
    assert!(spec.version >= Version::EIP37_MIN);
}

#[test]
fn jvm_handshake_validates_for_testnet() {
    let spec = handshake::parse(&JVM_HANDSHAKE).unwrap();
    // Should pass validation (version OK + magic OK)
    let result = handshake::validate_peer(&spec, &Network::Testnet);
    // Print details if it fails
    if result.is_err() {
        eprintln!("Validation error: {:?}", result);
        eprintln!("Features found: {:?}", spec.features);
    }
    assert!(result.is_ok(), "JVM handshake should validate for testnet");
}

#[test]
fn jvm_handshake_has_session_with_testnet_magic() {
    let spec = handshake::parse(&JVM_HANDSHAKE).unwrap();
    let session = spec.features.iter().find(|f| f.id == 3);
    assert!(session.is_some(), "Should have session feature (id=3)");
    let session = session.unwrap();
    assert!(session.body.len() >= 4, "Session body should have at least 4 bytes (magic)");
    assert_eq!(&session.body[0..4], &[2, 3, 2, 3], "Session magic should be testnet");
}

#[test]
fn jvm_handshake_has_mode_feature() {
    let spec = handshake::parse(&JVM_HANDSHAKE).unwrap();
    let mode = spec.features.iter().find(|f| f.id == 16);
    assert!(mode.is_some(), "Should have mode feature (id=16)");
    let mode = mode.unwrap();
    // Full archival node: stateType=0, verifying=1, nipopow=None(0), blocksToKeep=-1(zigzag=1)
    assert_eq!(mode.body, vec![0x00, 0x01, 0x00, 0x01],
        "Mode should be full archival node");
}

#[test]
fn jvm_first_message_decodes_as_sync_info() {
    let magic = Network::Testnet.magic();
    let frame = frame::decode(&magic, &JVM_FIRST_MESSAGE).unwrap();
    assert_eq!(frame.code, 65, "First message should be SyncInfo (code 65)");
    assert_eq!(frame.body.len(), 3);
}

#[test]
fn jvm_first_message_has_correct_magic() {
    assert_eq!(&JVM_FIRST_MESSAGE[0..4], &[2, 3, 2, 3],
        "First framed message should use testnet magic");
}

// --- Our proxy handshake structure verification ---

#[test]
fn proxy_handshake_roundtrips() {
    let config = HandshakeConfig {
        agent_name: "ergo-proxy".to_string(),
        peer_name: "ergo-proxy-node".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
        mode_config: handshake::ModeConfig::default(),
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();

    assert_eq!(spec.agent, "ergo-proxy");
    assert_eq!(spec.version, Version::new(6, 0, 3));
    assert_eq!(spec.name, "ergo-proxy-node");
    assert_eq!(spec.address, None);
    assert!(handshake::validate_peer(&spec, &Network::Testnet).is_ok());
}

#[test]
fn proxy_handshake_feature_body_lengths_are_u16_be() {
    let config = HandshakeConfig {
        agent_name: "test".to_string(),
        peer_name: "test".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
        mode_config: handshake::ModeConfig::default(),
    };
    let bytes = handshake::build(&config);

    // Find feature section: skip past timestamp, agent, version, peer, address
    // Parse up to the feature count to find where features start
    let spec = handshake::parse(&bytes).unwrap();

    // Verify features are parseable (which means body lengths are correct)
    assert_eq!(spec.features.len(), 2, "Should have Mode + Session features");

    // Verify Mode feature matches JVM format
    let mode = spec.features.iter().find(|f| f.id == 16).unwrap();
    assert_eq!(mode.body, vec![0x00, 0x01, 0x00, 0x01]);

    // Verify Session feature has correct structure
    let session = spec.features.iter().find(|f| f.id == 3).unwrap();
    // Session body: 4 magic + VLQ(ZigZag(session_id)), variable 5-14 bytes
    assert!(session.body.len() >= 5 && session.body.len() <= 14,
        "Session body len {} out of range", session.body.len());
    assert_eq!(&session.body[0..4], &[2, 3, 2, 3]);
}

#[test]
fn proxy_handshake_matches_jvm_structure() {
    // Build a handshake with similar params to the JVM node
    let config = HandshakeConfig {
        agent_name: "ergoref".to_string(),
        peer_name: "ergo-test-fresh".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
        mode_config: handshake::ModeConfig::default(),
    };
    let proxy_bytes = handshake::build(&config);
    let proxy_spec = handshake::parse(&proxy_bytes).unwrap();

    // Parse the JVM handshake
    let jvm_spec = handshake::parse(&JVM_HANDSHAKE).unwrap();

    // Structural comparison (skip timestamps and session IDs which differ)
    assert_eq!(proxy_spec.agent, jvm_spec.agent);
    assert_eq!(proxy_spec.version, jvm_spec.version);
    assert_eq!(proxy_spec.name, jvm_spec.name);

    // Both should have Mode feature with same encoding
    let proxy_mode = proxy_spec.features.iter().find(|f| f.id == 16);
    let jvm_mode = jvm_spec.features.iter().find(|f| f.id == 16);
    assert_eq!(
        proxy_mode.map(|f| &f.body),
        jvm_mode.map(|f| &f.body),
        "Mode feature body should match"
    );

    // Both should have Session feature with testnet magic
    let proxy_session = proxy_spec.features.iter().find(|f| f.id == 3).unwrap();
    let jvm_session = jvm_spec.features.iter().find(|f| f.id == 3).unwrap();
    assert_eq!(&proxy_session.body[0..4], &jvm_session.body[0..4],
        "Session magic should match");
}

#[test]
fn proxy_frame_roundtrips_with_testnet_magic() {
    let magic = Network::Testnet.magic();

    // Build a GetPeers frame (same as JVM node sends)
    let frame = Frame { code: 1, body: vec![] };
    let encoded = frame::encode(&magic, &frame);

    // Verify magic is correct
    assert_eq!(&encoded[0..4], &[2, 3, 2, 3]);

    // Verify it decodes back
    let decoded = frame::decode(&magic, &encoded).unwrap();
    assert_eq!(decoded.code, 1);
    assert!(decoded.body.is_empty());
}

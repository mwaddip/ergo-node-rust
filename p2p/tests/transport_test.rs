mod common;

use enr_p2p::transport::vlq;

#[test]
fn vlq_encode_zero() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 0);
    assert_eq!(buf, vec![0x00]);
}

#[test]
fn vlq_encode_single_byte() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 127);
    assert_eq!(buf, vec![0x7f]);
}

#[test]
fn vlq_encode_two_bytes() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 128);
    assert_eq!(buf, vec![0x80, 0x01]);
}

#[test]
fn vlq_encode_large_value() {
    // 300 = 0b100101100 → low 7: 0101100 = 0x2c | 0x80, high 7: 0000010 = 0x02
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 300);
    assert_eq!(buf, vec![0xac, 0x02]);
}

#[test]
fn vlq_roundtrip() {
    let values = [0u64, 1, 127, 128, 255, 256, 16383, 16384, 1_000_000, u64::MAX >> 1];
    for &val in &values {
        let mut buf = Vec::new();
        vlq::write_vlq(&mut buf, val);
        let mut cursor = std::io::Cursor::new(buf.as_slice());
        let decoded = vlq::read_vlq(&mut cursor).unwrap();
        assert_eq!(decoded, val, "roundtrip failed for {}", val);
    }
}

#[test]
fn vlq_read_overflow_returns_error() {
    let data = vec![0x80; 10];
    let mut cursor = std::io::Cursor::new(data.as_slice());
    assert!(vlq::read_vlq(&mut cursor).is_err());
}

#[test]
fn vlq_length_rejects_oversized() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 3_000_000);
    let mut cursor = std::io::Cursor::new(buf.as_slice());
    assert!(vlq::read_vlq_length(&mut cursor).is_err());
}

#[test]
fn vlq_length_accepts_valid() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 1024);
    let mut cursor = std::io::Cursor::new(buf.as_slice());
    assert_eq!(vlq::read_vlq_length(&mut cursor).unwrap(), 1024);
}

#[test]
fn zigzag_encode_positive() {
    assert_eq!(vlq::zigzag_encode(1), 2);
    assert_eq!(vlq::zigzag_encode(2), 4);
    assert_eq!(vlq::zigzag_encode(1440), 2880);
}

#[test]
fn zigzag_encode_negative() {
    assert_eq!(vlq::zigzag_encode(-1), 1);
    assert_eq!(vlq::zigzag_encode(-2), 3);
}

#[test]
fn zigzag_roundtrip() {
    for val in [-1000i32, -2, -1, 0, 1, 2, 1000, i32::MAX, i32::MIN] {
        let encoded = vlq::zigzag_encode(val);
        let decoded = vlq::zigzag_decode(encoded);
        assert_eq!(decoded, val, "zigzag roundtrip failed for {}", val);
    }
}

// --- Frame tests ---

use enr_p2p::transport::frame::{self, Frame};
use enr_p2p::types::Network;

#[test]
fn frame_encode_decode_roundtrip() {
    let magic = Network::Testnet.magic();
    let original = Frame { code: 55, body: vec![2, 0, 1, 2, 3] };
    let bytes = frame::encode(&magic, &original);
    assert_eq!(bytes.len(), 13 + original.body.len());
    let decoded = frame::decode(&magic, &bytes).unwrap();
    assert_eq!(decoded.code, original.code);
    assert_eq!(decoded.body, original.body);
}

#[test]
fn frame_decode_bad_magic_returns_error() {
    let magic = Network::Testnet.magic();
    let f = Frame { code: 1, body: vec![] };
    let mut bytes = frame::encode(&magic, &f);
    bytes[0] = 0xff;
    assert!(frame::decode(&magic, &bytes).is_err());
}

#[test]
fn frame_decode_bad_checksum_returns_error() {
    let magic = Network::Testnet.magic();
    let f = Frame { code: 1, body: vec![1, 2, 3] };
    let mut bytes = frame::encode(&magic, &f);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    assert!(frame::decode(&magic, &bytes).is_err());
}

#[test]
fn frame_decode_empty_body() {
    let magic = Network::Testnet.magic();
    let f = Frame { code: 1, body: vec![] };
    let bytes = frame::encode(&magic, &f);
    let decoded = frame::decode(&magic, &bytes).unwrap();
    assert_eq!(decoded.code, 1);
    assert!(decoded.body.is_empty());
}

#[test]
fn frame_encode_checksum_is_blake2b256_prefix() {
    use blake2::digest::consts::U32;
    use blake2::{Blake2b, Digest};
    type Blake2b256 = Blake2b<U32>;

    let magic = Network::Testnet.magic();
    let body = vec![0xde, 0xad, 0xbe, 0xef];
    let f = Frame { code: 55, body: body.clone() };
    let bytes = frame::encode(&magic, &f);
    let expected_checksum = &Blake2b256::digest(&body)[..4];
    assert_eq!(&bytes[9..13], expected_checksum);
}

// --- Handshake tests ---

use enr_p2p::transport::handshake::{self, HandshakeConfig};
use enr_p2p::types::{Version, ProxyMode};

#[test]
fn handshake_build_parse_roundtrip() {
    let config = HandshakeConfig {
        agent_name: "ergo-proxy".to_string(),
        peer_name: "test-node".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
        mode_config: handshake::ModeConfig::default(),
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();

    assert_eq!(spec.agent, "ergo-proxy");
    assert_eq!(spec.name, "test-node");
    assert_eq!(spec.version, Version::new(6, 0, 3));
    assert_eq!(spec.address, None);
    assert!(spec.features.len() >= 2); // Mode + Session
}

#[test]
fn handshake_self_built_is_not_proxy() {
    // We no longer advertise FEATURE_PROXY — verify our own handshake
    let config = HandshakeConfig {
        agent_name: "ergo-proxy".to_string(),
        peer_name: "test-node".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
        mode_config: handshake::ModeConfig::default(),
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();
    assert!(!handshake::is_proxy(&spec));
}

#[test]
fn handshake_full_mode_feature() {
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
    let spec = handshake::parse(&bytes).unwrap();
    let mode_feat = spec.features.iter().find(|f| f.id == 16).unwrap();
    assert_eq!(mode_feat.body, vec![0x00, 0x01, 0x00, 0x01]);
}

#[test]
fn handshake_light_mode_feature() {
    let config = HandshakeConfig {
        agent_name: "test".to_string(),
        peer_name: "test".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Light,
        declared_address: None,
        mode_config: handshake::ModeConfig::default(),
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();
    let mode_feat = spec.features.iter().find(|f| f.id == 16).unwrap();
    assert_eq!(mode_feat.body, vec![0x00, 0x01, 0x01, 0x02, 0x01]);
}

#[test]
fn handshake_session_feature_has_correct_magic() {
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
    let spec = handshake::parse(&bytes).unwrap();
    let session_feat = spec.features.iter().find(|f| f.id == 3).unwrap();
    assert_eq!(&session_feat.body[0..4], &[2, 3, 2, 3]);
    // Session body: 4 magic + VLQ(ZigZag(random_i64)), variable length 5-14 bytes
    assert!(session_feat.body.len() >= 5 && session_feat.body.len() <= 14);
}

#[test]
fn handshake_version_validation() {
    let config = HandshakeConfig {
        agent_name: "test".to_string(),
        peer_name: "test".to_string(),
        version: Version::new(3, 0, 0),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
        mode_config: handshake::ModeConfig::default(),
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();
    assert!(handshake::validate_peer(&spec, &Network::Testnet).is_err());
}

#[test]
fn handshake_valid_peer_accepted() {
    let config = HandshakeConfig {
        agent_name: "ergoref".to_string(),
        peer_name: "test-node".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
        mode_config: handshake::ModeConfig::default(),
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();
    assert!(handshake::validate_peer(&spec, &Network::Testnet).is_ok());
}

//! Typed P2P protocol messages with parse/serialize.
//!
//! # Contract
//! - `from_frame`: parses a `Frame` into a typed `ProtocolMessage`.
//!   Precondition: frame has a valid code and body.
//!   Postcondition: returns a typed message, or `Unknown` for unrecognized codes.
//!   SyncInfo body is preserved opaque. Unknown codes are preserved, not dropped.
//! - `to_frame`: serializes a `ProtocolMessage` back into a `Frame`.
//!   Postcondition: `ProtocolMessage::from_frame(&msg.to_frame()) ≈ msg` (roundtrip).
//! - Invariant: the transport layer never sees typed messages; the protocol layer
//!   never sees raw bytes.

use crate::transport::frame::Frame;
use crate::transport::handshake::{parse_peer_entry, serialize_peer_entry, PeerSpec};
use crate::transport::vlq;
use crate::types::ModifierId;
use std::io::{self, Cursor, Read};

/// Hard cap on object counts in INV / ModifierRequest / ModifierResponse
/// messages. Mirrors JVM `InvSpec.maxInvObjects = 400` (a `require` that
/// throws on parse). Bounds pre-allocation: a hostile peer cannot drive
/// `Vec::with_capacity` beyond this without us rejecting the frame first.
const MAX_INV_OBJECTS: usize = 400;

type ModifierEntries = Vec<(ModifierId, Vec<u8>)>;

/// Well-known message codes.
pub struct MessageCode;

impl MessageCode {
    pub const GET_PEERS: u8 = 1;
    pub const PEERS: u8 = 2;
    pub const MODIFIER_REQUEST: u8 = 22;
    pub const MODIFIER_RESPONSE: u8 = 33;
    pub const INV: u8 = 55;
    pub const SYNC_INFO: u8 = 65;
}

/// A typed protocol message.
#[derive(Debug, Clone)]
pub enum ProtocolMessage {
    GetPeers,
    Peers { body: Vec<u8> },
    Inv { modifier_type: u8, ids: Vec<ModifierId> },
    ModifierRequest { modifier_type: u8, ids: Vec<ModifierId> },
    ModifierResponse { modifier_type: u8, modifiers: Vec<(ModifierId, Vec<u8>)> },
    SyncInfo { body: Vec<u8> },
    Unknown { code: u8, body: Vec<u8> },
}

impl ProtocolMessage {
    /// Parse a Frame into a typed message.
    pub fn from_frame(frame: &Frame) -> io::Result<Self> {
        match frame.code {
            MessageCode::GET_PEERS => Ok(ProtocolMessage::GetPeers),

            MessageCode::PEERS => {
                Ok(ProtocolMessage::Peers { body: frame.body.clone() })
            }

            MessageCode::INV => {
                let (modifier_type, ids) = parse_inv_body(&frame.body)?;
                Ok(ProtocolMessage::Inv { modifier_type, ids })
            }

            MessageCode::MODIFIER_REQUEST => {
                let (modifier_type, ids) = parse_inv_body(&frame.body)?;
                Ok(ProtocolMessage::ModifierRequest { modifier_type, ids })
            }

            MessageCode::MODIFIER_RESPONSE => {
                let (modifier_type, modifiers) = parse_modifier_response_body(&frame.body)?;
                Ok(ProtocolMessage::ModifierResponse { modifier_type, modifiers })
            }

            MessageCode::SYNC_INFO => {
                Ok(ProtocolMessage::SyncInfo { body: frame.body.clone() })
            }

            code => {
                Ok(ProtocolMessage::Unknown { code, body: frame.body.clone() })
            }
        }
    }

    /// Serialize a typed message back into a Frame.
    pub fn to_frame(&self) -> Frame {
        match self {
            ProtocolMessage::GetPeers => Frame { code: MessageCode::GET_PEERS, body: vec![] },
            ProtocolMessage::Peers { body } => Frame { code: MessageCode::PEERS, body: body.clone() },
            ProtocolMessage::Inv { modifier_type, ids } => {
                Frame { code: MessageCode::INV, body: encode_inv_body(*modifier_type, ids) }
            }
            ProtocolMessage::ModifierRequest { modifier_type, ids } => {
                Frame { code: MessageCode::MODIFIER_REQUEST, body: encode_inv_body(*modifier_type, ids) }
            }
            ProtocolMessage::ModifierResponse { modifier_type, modifiers } => {
                Frame {
                    code: MessageCode::MODIFIER_RESPONSE,
                    body: encode_modifier_response_body(*modifier_type, modifiers),
                }
            }
            ProtocolMessage::SyncInfo { body } => Frame { code: MessageCode::SYNC_INFO, body: body.clone() },
            ProtocolMessage::Unknown { code, body } => Frame { code: *code, body: body.clone() },
        }
    }
}

fn parse_inv_body(data: &[u8]) -> io::Result<(u8, Vec<ModifierId>)> {
    let mut cursor = Cursor::new(data);
    let mut type_byte = [0u8; 1];
    cursor.read_exact(&mut type_byte)?;
    let count = vlq::read_vlq_length(&mut cursor)?;
    if count > MAX_INV_OBJECTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("INV/Request count {count} exceeds cap {MAX_INV_OBJECTS}"),
        ));
    }
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        let mut id = [0u8; 32];
        cursor.read_exact(&mut id)?;
        ids.push(id);
    }
    Ok((type_byte[0], ids))
}

fn encode_inv_body(modifier_type: u8, ids: &[ModifierId]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(modifier_type);
    vlq::write_vlq(&mut body, ids.len() as u64);
    for id in ids {
        body.extend_from_slice(id);
    }
    body
}

fn parse_modifier_response_body(data: &[u8]) -> io::Result<(u8, ModifierEntries)> {
    let body_size = data.len();
    let mut cursor = Cursor::new(data);
    let mut type_byte = [0u8; 1];
    cursor.read_exact(&mut type_byte)?;
    let count = vlq::read_vlq_length(&mut cursor)?;
    if count > MAX_INV_OBJECTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ModifierResponse count {count} exceeds cap {MAX_INV_OBJECTS}"),
        ));
    }
    let mut modifiers = Vec::with_capacity(count);
    let mut allocated = 0usize;
    for _ in 0..count {
        let mut id = [0u8; 32];
        cursor.read_exact(&mut id)?;
        let data_len = vlq::read_vlq_length(&mut cursor)?;
        // Sum of declared payload sizes cannot exceed the frame body.
        // The frame layer caps body at 2 MB; a hostile peer can still
        // declare a per-mod data_len up to the VLQ cap (also 2 MB), so
        // without this check N small mods × 2 MB each compounds beyond
        // what the frame ever delivered.
        allocated = allocated.saturating_add(data_len);
        if allocated > body_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ModifierResponse declared payload {allocated} exceeds frame body {body_size}"
                ),
            ));
        }
        let mut mod_data = vec![0u8; data_len];
        cursor.read_exact(&mut mod_data)?;
        modifiers.push((id, mod_data));
    }
    Ok((type_byte[0], modifiers))
}

fn encode_modifier_response_body(modifier_type: u8, modifiers: &[(ModifierId, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(modifier_type);
    vlq::write_vlq(&mut body, modifiers.len() as u64);
    for (id, data) in modifiers {
        body.extend_from_slice(id);
        vlq::write_vlq(&mut body, data.len() as u64);
        body.extend_from_slice(data);
    }
    body
}

/// Parse a `Peers` message body: `length: VLQ` followed by that many
/// serialized `PeerSpec` entries.
///
/// # Errors
/// `InvalidData` if `length > cap`, the body is truncated, or any
/// per-entry parse fails. The caller treats this as a permanent ban
/// signal — JVM's `PeerSynchronizer` does the same via
/// `penalizeMaliciousPeer`.
pub fn parse_peers_body(body: &[u8], cap: usize) -> io::Result<Vec<PeerSpec>> {
    let mut cursor = Cursor::new(body);
    let count = vlq::read_vlq_length(&mut cursor)?;
    if count > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Peers count {count} exceeds cap {cap}"),
        ));
    }
    let mut specs = Vec::with_capacity(count);
    for _ in 0..count {
        specs.push(parse_peer_entry(&mut cursor)?);
    }
    Ok(specs)
}

/// Build a `Peers` message body. Caller is responsible for capping the
/// list before calling — this function does not enforce a cap.
///
/// An empty `specs` produces a single-byte body `[0x00]` (VLQ-encoded
/// count = 0), matching the JVM serializer.
pub fn build_peers_body(specs: &[PeerSpec]) -> Vec<u8> {
    let mut body = Vec::new();
    vlq::write_vlq(&mut body, specs.len() as u64);
    for spec in specs {
        serialize_peer_entry(spec, &mut body);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::handshake::Feature;
    use crate::types::Version;
    use std::net::SocketAddr;

    fn spec(addr: Option<&str>, features: Vec<Feature>) -> PeerSpec {
        PeerSpec {
            agent: "ergoref".into(),
            version: Version::new(5, 0, 25),
            name: "node".into(),
            address: addr.map(|a| a.parse::<SocketAddr>().unwrap()),
            features,
        }
    }

    fn assert_specs_equal(a: &PeerSpec, b: &PeerSpec) {
        assert_eq!(a.agent, b.agent);
        assert_eq!(a.version, b.version);
        assert_eq!(a.name, b.name);
        assert_eq!(a.address, b.address);
        assert_eq!(a.features.len(), b.features.len());
        for (fa, fb) in a.features.iter().zip(b.features.iter()) {
            assert_eq!(fa.id, fb.id);
            assert_eq!(fa.body, fb.body);
        }
    }

    #[test]
    fn peers_body_roundtrip_empty() {
        let body = build_peers_body(&[]);
        assert_eq!(body, vec![0x00]);
        let parsed = parse_peers_body(&body, 64).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn peers_body_roundtrip_single_v4() {
        let s = spec(Some("203.0.113.5:9030"), vec![]);
        let body = build_peers_body(std::slice::from_ref(&s));
        let parsed = parse_peers_body(&body, 64).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_specs_equal(&parsed[0], &s);
    }

    #[test]
    fn peers_body_roundtrip_three_mixed() {
        let s1 = spec(Some("203.0.113.5:9030"), vec![Feature { id: 16, body: vec![0, 1, 0] }]);
        let s2 = spec(Some("[2001:db8::1]:9030"), vec![]);
        let s3 = spec(None, vec![
            Feature { id: 16, body: vec![0, 0, 0, 0xFE, 0x01] },
            Feature { id: 64, body: vec![] },
        ]);
        let specs = vec![s1, s2, s3];
        let body = build_peers_body(&specs);
        let parsed = parse_peers_body(&body, 64).unwrap();
        assert_eq!(parsed.len(), 3);
        for (a, b) in parsed.iter().zip(specs.iter()) {
            assert_specs_equal(a, b);
        }
    }

    #[test]
    fn peers_body_roundtrip_sixty_four_entries() {
        let specs: Vec<PeerSpec> = (0..64)
            .map(|i| {
                let addr = format!("203.0.113.{}:9030", i + 1);
                spec(Some(&addr), vec![])
            })
            .collect();
        let body = build_peers_body(&specs);
        let parsed = parse_peers_body(&body, 64).unwrap();
        assert_eq!(parsed.len(), 64);
    }

    #[test]
    fn peers_body_rejects_count_above_cap() {
        let specs: Vec<PeerSpec> = (0..10)
            .map(|i| spec(Some(&format!("203.0.113.{}:9030", i + 1)), vec![]))
            .collect();
        let body = build_peers_body(&specs);
        let err = parse_peers_body(&body, 5).expect_err("must reject oversize");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn peers_body_rejects_truncated() {
        // Declare count = 3 but only provide one entry.
        let mut body = build_peers_body(&[spec(Some("203.0.113.5:9030"), vec![])]);
        body[0] = 3; // VLQ for 3 is single byte 0x03; overwriting the declared count
        let err = parse_peers_body(&body, 64).expect_err("truncated body must error");
        assert!(matches!(
            err.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn empty_peers_body_is_single_zero_byte() {
        assert_eq!(build_peers_body(&[]), vec![0x00]);
    }

    /// Document the wire-format size for sanity-checking against pcap.
    /// Numbers come from hand-computing the encoding: 1-byte VLQ count
    /// plus per-entry: agent (1+N), version (3), name (1+N), declared
    /// address (1 option byte + 1 size byte + 4|16 IP bytes + VLQ port
    /// 9030 = 2 bytes), feature count (1 byte = 0).
    #[test]
    fn peers_body_byte_counts() {
        let v4 = |i: u8| spec(
            Some(&format!("203.0.113.{}:9030", i)),
            vec![],
        );
        let v6 = |i: u8| {
            let mut octets = [0u8; 16];
            octets[0] = 0x20; octets[1] = 0x01; octets[2] = 0x0d; octets[3] = 0xb8;
            octets[15] = i;
            let addr = std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::from(octets)),
                9030,
            );
            PeerSpec {
                agent: "ergoref".into(),
                version: Version::new(5, 0, 25),
                name: "node".into(),
                address: Some(addr),
                features: vec![],
            }
        };

        assert_eq!(build_peers_body(&[]).len(), 1, "0 peers");
        assert_eq!(build_peers_body(&[v4(1)]).len(), 26, "1 v4 peer");
        let eight_v4: Vec<_> = (1..=8).map(v4).collect();
        assert_eq!(build_peers_body(&eight_v4).len(), 1 + 8 * 25, "8 v4 peers");
        assert_eq!(build_peers_body(&[v6(1)]).len(), 38, "1 v6 peer");
        let eight_v6: Vec<_> = (1..=8).map(v6).collect();
        assert_eq!(build_peers_body(&eight_v6).len(), 1 + 8 * 37, "8 v6 peers");
    }

    #[test]
    fn parse_inv_body_rejects_count_above_cap() {
        // Body: type(1) + VLQ(MAX_INV_OBJECTS + 1). No IDs follow — the
        // cap rejection must fire before any ID-shaped allocation.
        let mut body = vec![1u8];
        vlq::write_vlq(&mut body, (MAX_INV_OBJECTS as u64) + 1);
        let err = parse_inv_body(&body).expect_err("oversized count must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parse_inv_body_accepts_count_at_cap() {
        // Hand-roll a valid INV at exactly the cap.
        let mut body = vec![1u8];
        vlq::write_vlq(&mut body, MAX_INV_OBJECTS as u64);
        body.extend_from_slice(&[0u8; 32 * MAX_INV_OBJECTS]);
        let (type_id, ids) = parse_inv_body(&body).expect("at-cap count must parse");
        assert_eq!(type_id, 1);
        assert_eq!(ids.len(), MAX_INV_OBJECTS);
    }

    #[test]
    fn parse_modifier_response_body_rejects_count_above_cap() {
        let mut body = vec![1u8];
        vlq::write_vlq(&mut body, (MAX_INV_OBJECTS as u64) + 1);
        let err = parse_modifier_response_body(&body)
            .expect_err("oversized count must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parse_modifier_response_body_rejects_payload_exceeding_body() {
        // Declare 1 modifier with data_len greater than the bytes that
        // actually fit in the (declared) body. The check must fire before
        // we allocate `vec![0u8; data_len]`.
        let mut body = vec![1u8];
        vlq::write_vlq(&mut body, 1);              // count = 1
        body.extend_from_slice(&[0u8; 32]);        // modifier id
        vlq::write_vlq(&mut body, 1_000_000);      // declared data_len
        // body length is far smaller than 1_000_000 — must reject.
        let err = parse_modifier_response_body(&body)
            .expect_err("oversized declared payload must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}

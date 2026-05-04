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

#[cfg(test)]
mod tests {
    use super::*;

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

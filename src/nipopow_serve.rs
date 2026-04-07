//! NiPoPoW serving — handles incoming P2P NiPoPoW messages.
//!
//! Two message codes (per JVM `GetNipopowProofSpec`/`NipopowProofSpec`):
//!
//! - **90 (`GetNipopowProof`)**: peer requests a NiPoPoW proof from us.
//!   We build it from the local chain via `enr_chain::build_nipopow_proof`
//!   and respond with a code 91 message.
//!
//! - **91 (`NipopowProof`)**: peer sends us a proof. We verify it via
//!   `enr_chain::verify_nipopow_proof_bytes` and log the result. The proof
//!   is NOT applied to chain state — light-client mode is a separate session.
//!
//! Both codes use VLQ-encoded integer fields with a `putUShort(0)` pad-length
//! footer for forward compatibility (the JVM convention for new message
//! types — older parsers can skip the pad without breaking).

use std::io::{Cursor, Read};

use ergo_chain_types::{BlockId, Digest32};
use sigma_ser::vlq_encode::{ReadSigmaVlqExt, WriteSigmaVlqExt};

/// Message code: peer requests a NiPoPoW proof.
pub const GET_NIPOPOW_PROOF: u8 = 90;
/// Message code: peer sends a NiPoPoW proof (or we send one in response).
pub const NIPOPOW_PROOF: u8 = 91;

/// Hard size cap on `GetNipopowProof` message bodies (mirrors JVM `SizeLimit`).
pub const GET_NIPOPOW_PROOF_MAX_SIZE: usize = 1000;
/// Hard size cap on `NipopowProof` message bodies (mirrors JVM `SizeLimit`).
pub const NIPOPOW_PROOF_MAX_SIZE: usize = 2_000_000;

/// Returns true if `code` is a NiPoPoW message that this module handles.
pub fn is_nipopow_message(code: u8) -> bool {
    matches!(code, GET_NIPOPOW_PROOF | NIPOPOW_PROOF)
}

/// Errors arising from parsing or building NiPoPoW message envelopes.
#[derive(Debug, thiserror::Error)]
pub enum NipopowError {
    #[error("body too large: {size} bytes (max {max})")]
    BodyTooLarge { size: usize, max: usize },

    #[error("invalid m or k: m={m}, k={k}")]
    InvalidParameters { m: i32, k: i32 },

    #[error("proof length out of range: {0}")]
    InvalidProofLength(usize),

    #[error("body truncated")]
    Truncated,

    #[error("VLQ decode failed: {0}")]
    Vlq(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Parsed `GetNipopowProof` request.
#[derive(Debug, Clone)]
pub struct GetNipopowProofRequest {
    /// Min superchain length parameter.
    pub m: i32,
    /// Suffix length parameter.
    pub k: i32,
    /// Optional anchor — the suffix tip header. None means "current tip".
    pub header_id: Option<BlockId>,
}

/// Parse a `GetNipopowProof` (code 90) message body.
///
/// Wire format (mirrors JVM `GetNipopowProofSpec.parse`):
/// ```text
/// m: i32 (ZigZag VLQ — putInt)
/// k: i32 (ZigZag VLQ — putInt)
/// header_id_present: u8 (raw byte: 0 or 1)
/// [if header_id_present == 1] header_id: 32 raw bytes
/// future_pad_length: u16 (VLQ — putUShort)
/// [if future_pad_length > 0 < SizeLimit] padding: future_pad_length bytes (skipped)
/// ```
pub fn parse_get_nipopow_proof(body: &[u8]) -> Result<GetNipopowProofRequest, NipopowError> {
    if body.len() > GET_NIPOPOW_PROOF_MAX_SIZE {
        return Err(NipopowError::BodyTooLarge {
            size: body.len(),
            max: GET_NIPOPOW_PROOF_MAX_SIZE,
        });
    }

    let mut cursor = Cursor::new(body);

    let m = cursor
        .get_i32()
        .map_err(|e| NipopowError::Vlq(format!("m: {e:?}")))?;
    let k = cursor
        .get_i32()
        .map_err(|e| NipopowError::Vlq(format!("k: {e:?}")))?;

    if m <= 0 || k <= 0 {
        return Err(NipopowError::InvalidParameters { m, k });
    }

    let mut present_byte = [0u8; 1];
    cursor
        .read_exact(&mut present_byte)
        .map_err(|_| NipopowError::Truncated)?;

    let header_id = if present_byte[0] == 1 {
        let mut id = [0u8; 32];
        cursor
            .read_exact(&mut id)
            .map_err(|_| NipopowError::Truncated)?;
        Some(BlockId(Digest32::from(id)))
    } else {
        None
    };

    // Pad length (forward-compat). Skip up to SizeLimit bytes.
    let pad_len = cursor
        .get_u16()
        .map_err(|e| NipopowError::Vlq(format!("pad_len: {e:?}")))? as usize;
    if pad_len > 0 && pad_len < GET_NIPOPOW_PROOF_MAX_SIZE {
        let mut pad = vec![0u8; pad_len];
        cursor
            .read_exact(&mut pad)
            .map_err(|_| NipopowError::Truncated)?;
    }

    Ok(GetNipopowProofRequest { m, k, header_id })
}

/// Parse a `NipopowProof` (code 91) message body, returning the inner proof bytes.
///
/// Wire format (mirrors JVM `NipopowProofSpec.parse`):
/// ```text
/// proof_length: u32 (VLQ — putUInt)
/// proof_bytes: [u8; proof_length]
/// future_pad_length: u16 (VLQ — putUShort)
/// [if future_pad_length > 0 < SizeLimit] padding: future_pad_length bytes (skipped)
/// ```
pub fn parse_nipopow_proof(body: &[u8]) -> Result<Vec<u8>, NipopowError> {
    if body.len() > NIPOPOW_PROOF_MAX_SIZE {
        return Err(NipopowError::BodyTooLarge {
            size: body.len(),
            max: NIPOPOW_PROOF_MAX_SIZE,
        });
    }

    let mut cursor = Cursor::new(body);

    let proof_len = cursor
        .get_u32()
        .map_err(|e| NipopowError::Vlq(format!("proof_len: {e:?}")))? as usize;

    if proof_len == 0 || proof_len >= NIPOPOW_PROOF_MAX_SIZE {
        return Err(NipopowError::InvalidProofLength(proof_len));
    }

    let mut proof_bytes = vec![0u8; proof_len];
    cursor
        .read_exact(&mut proof_bytes)
        .map_err(|_| NipopowError::Truncated)?;

    // Pad length (forward-compat). Skip up to SizeLimit bytes.
    let pad_len = cursor
        .get_u16()
        .map_err(|e| NipopowError::Vlq(format!("pad_len: {e:?}")))? as usize;
    if pad_len > 0 && pad_len < NIPOPOW_PROOF_MAX_SIZE {
        let mut pad = vec![0u8; pad_len];
        cursor
            .read_exact(&mut pad)
            .map_err(|_| NipopowError::Truncated)?;
    }

    Ok(proof_bytes)
}

/// Serialize the inner proof bytes into a `NipopowProof` (code 91) message body.
///
/// Inverse of [`parse_nipopow_proof`].
pub fn serialize_nipopow_proof(proof_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(proof_bytes.len() + 8);
    out.put_u32(proof_bytes.len() as u32)
        .expect("Vec write cannot fail");
    out.extend_from_slice(proof_bytes);
    out.put_u16(0).expect("Vec write cannot fail");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get_nipopow_proof_no_anchor() {
        // m=6, k=6, no header_id
        let mut body = Vec::new();
        body.put_i32(6).unwrap(); // m
        body.put_i32(6).unwrap(); // k
        body.push(0); // header_id_present = 0
        body.put_u16(0).unwrap(); // pad_len = 0

        let req = parse_get_nipopow_proof(&body).unwrap();
        assert_eq!(req.m, 6);
        assert_eq!(req.k, 6);
        assert!(req.header_id.is_none());
    }

    #[test]
    fn parse_get_nipopow_proof_with_anchor() {
        let id = [0xab; 32];
        let mut body = Vec::new();
        body.put_i32(10).unwrap();
        body.put_i32(20).unwrap();
        body.push(1); // present
        body.extend_from_slice(&id);
        body.put_u16(0).unwrap();

        let req = parse_get_nipopow_proof(&body).unwrap();
        assert_eq!(req.m, 10);
        assert_eq!(req.k, 20);
        assert_eq!(req.header_id.unwrap().0 .0, id);
    }

    #[test]
    fn parse_get_nipopow_proof_rejects_too_large() {
        let body = vec![0u8; GET_NIPOPOW_PROOF_MAX_SIZE + 1];
        let err = parse_get_nipopow_proof(&body).unwrap_err();
        match err {
            NipopowError::BodyTooLarge { .. } => {}
            _ => panic!("expected BodyTooLarge"),
        }
    }

    #[test]
    fn parse_get_nipopow_proof_rejects_zero_m() {
        let mut body = Vec::new();
        body.put_i32(0).unwrap();
        body.put_i32(6).unwrap();
        body.push(0);
        body.put_u16(0).unwrap();

        let err = parse_get_nipopow_proof(&body).unwrap_err();
        match err {
            NipopowError::InvalidParameters { .. } => {}
            _ => panic!("expected InvalidParameters"),
        }
    }

    #[test]
    fn parse_nipopow_proof_round_trip() {
        let original = vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03];
        let serialized = serialize_nipopow_proof(&original);
        let parsed = parse_nipopow_proof(&serialized).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn parse_nipopow_proof_rejects_too_large() {
        let body = vec![0u8; NIPOPOW_PROOF_MAX_SIZE + 1];
        let err = parse_nipopow_proof(&body).unwrap_err();
        match err {
            NipopowError::BodyTooLarge { .. } => {}
            _ => panic!("expected BodyTooLarge"),
        }
    }

    #[test]
    fn parse_nipopow_proof_rejects_zero_length() {
        let mut body = Vec::new();
        body.put_u32(0).unwrap();
        body.put_u16(0).unwrap();

        let err = parse_nipopow_proof(&body).unwrap_err();
        match err {
            NipopowError::InvalidProofLength(_) => {}
            _ => panic!("expected InvalidProofLength"),
        }
    }
}

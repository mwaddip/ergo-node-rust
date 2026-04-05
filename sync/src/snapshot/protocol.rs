//! Snapshot P2P message codec.
//!
//! Six message types for UTXO snapshot synchronization, following the Ergo P2P
//! wire format. SnapshotsInfo uses Scorex VLQ+ZigZag encoding for count and
//! heights (like all Scorex integer serialization). Manifest and chunk responses
//! use BE u32 length-prefixed payloads.

use thiserror::Error;

// ── VLQ / ZigZag helpers ────────────────────────────────────────────────────

/// Decode a VLQ (Variable-Length Quantity) unsigned integer from bytes.
/// Returns (value, bytes_consumed).
fn vlq_decode(data: &[u8]) -> Result<(u64, usize), ProtocolError> {
    let mut result: u64 = 0;
    let mut shift = 0;
    for (i, &b) in data.iter().enumerate() {
        result |= ((b & 0x7F) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        if shift >= 64 {
            return Err(ProtocolError::TooShort);
        }
    }
    Err(ProtocolError::TooShort)
}

/// Encode a u64 as VLQ bytes.
fn vlq_encode(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

/// ZigZag decode: unsigned → signed mapping.
fn zigzag_decode(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// ZigZag encode: signed → unsigned mapping.
fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

// ── Message codes ────────────────────────────────────────────────────────────

pub const GET_SNAPSHOTS_INFO: u8 = 76;
pub const SNAPSHOTS_INFO: u8 = 77;
pub const GET_MANIFEST: u8 = 78;
pub const MANIFEST: u8 = 79;
pub const GET_UTXO_SNAPSHOT_CHUNK: u8 = 80;
pub const UTXO_SNAPSHOT_CHUNK: u8 = 81;

// ── Max payload sizes ────────────────────────────────────────────────────────

pub const MAX_SNAPSHOTS_INFO_SIZE: usize = 20_000;
pub const MAX_MANIFEST_SIZE: usize = 4_000_000;
pub const MAX_CHUNK_SIZE: usize = 4_000_000;

// ── Entry in SnapshotsInfo ───────────────────────────────────────────────────

/// One (height, manifest_id) pair from a SnapshotsInfo response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub height: u32,
    pub manifest_id: [u8; 32],
}

// ── Error type ───────────────────────────────────────────────────────────────

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("message body too short")]
    TooShort,

    #[error("message body too large: {0} bytes")]
    TooLarge(usize),

    #[error("unknown snapshot message code: {0}")]
    UnknownCode(u8),
}

// ── Message enum ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotMessage {
    GetSnapshotsInfo,
    SnapshotsInfo(Vec<SnapshotEntry>),
    GetManifest([u8; 32]),
    Manifest(Vec<u8>),
    GetUtxoSnapshotChunk([u8; 32]),
    UtxoSnapshotChunk(Vec<u8>),
}

impl SnapshotMessage {
    /// Returns `true` if `code` is one of the six snapshot message codes.
    pub fn is_snapshot_code(code: u8) -> bool {
        matches!(
            code,
            GET_SNAPSHOTS_INFO
                | SNAPSHOTS_INFO
                | GET_MANIFEST
                | MANIFEST
                | GET_UTXO_SNAPSHOT_CHUNK
                | UTXO_SNAPSHOT_CHUNK
        )
    }

    /// Parse a snapshot message from a message code and raw body bytes.
    pub fn parse(code: u8, body: &[u8]) -> Result<Self, ProtocolError> {
        match code {
            GET_SNAPSHOTS_INFO => Ok(SnapshotMessage::GetSnapshotsInfo),

            SNAPSHOTS_INFO => {
                if body.len() > MAX_SNAPSHOTS_INFO_SIZE {
                    return Err(ProtocolError::TooLarge(body.len()));
                }
                // Scorex serialization: VLQ count + (ZigZag+VLQ height + 32B manifest_id) per entry
                let (count, mut offset) = vlq_decode(body)?;
                let mut entries = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let (raw_height, new_offset) = vlq_decode(&body[offset..])?;
                    let height = zigzag_decode(raw_height) as u32;
                    offset += new_offset;
                    if offset + 32 > body.len() {
                        return Err(ProtocolError::TooShort);
                    }
                    let mut manifest_id = [0u8; 32];
                    manifest_id.copy_from_slice(&body[offset..offset + 32]);
                    offset += 32;
                    entries.push(SnapshotEntry { height, manifest_id });
                }
                Ok(SnapshotMessage::SnapshotsInfo(entries))
            }

            GET_MANIFEST => {
                if body.len() < 32 {
                    return Err(ProtocolError::TooShort);
                }
                let mut id = [0u8; 32];
                id.copy_from_slice(&body[..32]);
                Ok(SnapshotMessage::GetManifest(id))
            }

            MANIFEST => {
                if body.len() > MAX_MANIFEST_SIZE {
                    return Err(ProtocolError::TooLarge(body.len()));
                }
                // Length prefix is VLQ (Scorex putUInt)
                let (len, offset) = vlq_decode(body)?;
                let len = len as usize;
                if body.len() < offset + len {
                    return Err(ProtocolError::TooShort);
                }
                Ok(SnapshotMessage::Manifest(body[offset..offset + len].to_vec()))
            }

            GET_UTXO_SNAPSHOT_CHUNK => {
                if body.len() < 32 {
                    return Err(ProtocolError::TooShort);
                }
                let mut id = [0u8; 32];
                id.copy_from_slice(&body[..32]);
                Ok(SnapshotMessage::GetUtxoSnapshotChunk(id))
            }

            UTXO_SNAPSHOT_CHUNK => {
                if body.len() > MAX_CHUNK_SIZE {
                    return Err(ProtocolError::TooLarge(body.len()));
                }
                // Length prefix is VLQ (Scorex putUInt)
                let (len, offset) = vlq_decode(body)?;
                let len = len as usize;
                if body.len() < offset + len {
                    return Err(ProtocolError::TooShort);
                }
                Ok(SnapshotMessage::UtxoSnapshotChunk(body[offset..offset + len].to_vec()))
            }

            _ => Err(ProtocolError::UnknownCode(code)),
        }
    }

    /// Encode this message into a (code, body) pair for transmission.
    pub fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            SnapshotMessage::GetSnapshotsInfo => (GET_SNAPSHOTS_INFO, Vec::new()),

            SnapshotMessage::SnapshotsInfo(entries) => {
                let mut body = Vec::new();
                body.extend_from_slice(&vlq_encode(entries.len() as u64));
                for entry in entries {
                    body.extend_from_slice(&vlq_encode(zigzag_encode(entry.height as i64)));
                    body.extend_from_slice(&entry.manifest_id);
                }
                (SNAPSHOTS_INFO, body)
            }

            SnapshotMessage::GetManifest(id) => (GET_MANIFEST, id.to_vec()),

            SnapshotMessage::Manifest(data) => {
                let mut body = Vec::with_capacity(5 + data.len());
                body.extend_from_slice(&vlq_encode(data.len() as u64));
                body.extend_from_slice(data);
                (MANIFEST, body)
            }

            SnapshotMessage::GetUtxoSnapshotChunk(id) => {
                (GET_UTXO_SNAPSHOT_CHUNK, id.to_vec())
            }

            SnapshotMessage::UtxoSnapshotChunk(data) => {
                let mut body = Vec::with_capacity(5 + data.len());
                body.extend_from_slice(&vlq_encode(data.len() as u64));
                body.extend_from_slice(data);
                (UTXO_SNAPSHOT_CHUNK, body)
            }
        }
    }
}

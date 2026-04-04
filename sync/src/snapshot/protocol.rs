//! Snapshot P2P message codec.
//!
//! Six message types for UTXO snapshot synchronization, following the Ergo P2P
//! wire format (big-endian integers, length-prefixed payloads).

use thiserror::Error;

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
                if body.len() < 4 {
                    return Err(ProtocolError::TooShort);
                }
                let count = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                let expected = 4 + count * 36; // 4B height + 32B manifest_id per entry
                if body.len() < expected {
                    return Err(ProtocolError::TooShort);
                }
                let mut entries = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = 4 + i * 36;
                    let height = i32::from_be_bytes([
                        body[offset],
                        body[offset + 1],
                        body[offset + 2],
                        body[offset + 3],
                    ]) as u32;
                    let mut manifest_id = [0u8; 32];
                    manifest_id.copy_from_slice(&body[offset + 4..offset + 36]);
                    entries.push(SnapshotEntry {
                        height,
                        manifest_id,
                    });
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
                if body.len() < 4 {
                    return Err(ProtocolError::TooShort);
                }
                let len =
                    u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                if body.len() < 4 + len {
                    return Err(ProtocolError::TooShort);
                }
                Ok(SnapshotMessage::Manifest(body[4..4 + len].to_vec()))
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
                if body.len() < 4 {
                    return Err(ProtocolError::TooShort);
                }
                let len =
                    u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                if body.len() < 4 + len {
                    return Err(ProtocolError::TooShort);
                }
                Ok(SnapshotMessage::UtxoSnapshotChunk(body[4..4 + len].to_vec()))
            }

            _ => Err(ProtocolError::UnknownCode(code)),
        }
    }

    /// Encode this message into a (code, body) pair for transmission.
    pub fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            SnapshotMessage::GetSnapshotsInfo => (GET_SNAPSHOTS_INFO, Vec::new()),

            SnapshotMessage::SnapshotsInfo(entries) => {
                let mut body = Vec::with_capacity(4 + entries.len() * 36);
                body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
                for entry in entries {
                    body.extend_from_slice(&(entry.height as i32).to_be_bytes());
                    body.extend_from_slice(&entry.manifest_id);
                }
                (SNAPSHOTS_INFO, body)
            }

            SnapshotMessage::GetManifest(id) => (GET_MANIFEST, id.to_vec()),

            SnapshotMessage::Manifest(data) => {
                let mut body = Vec::with_capacity(4 + data.len());
                body.extend_from_slice(&(data.len() as u32).to_be_bytes());
                body.extend_from_slice(data);
                (MANIFEST, body)
            }

            SnapshotMessage::GetUtxoSnapshotChunk(id) => {
                (GET_UTXO_SNAPSHOT_CHUNK, id.to_vec())
            }

            SnapshotMessage::UtxoSnapshotChunk(data) => {
                let mut body = Vec::with_capacity(4 + data.len());
                body.extend_from_slice(&(data.len() as u32).to_be_bytes());
                body.extend_from_slice(data);
                (UTXO_SNAPSHOT_CHUNK, body)
            }
        }
    }
}

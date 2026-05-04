use std::io::Cursor;

use ergo_chain_types::{BlockId, Digest32, Header};
use sigma_ser::vlq_encode::{ReadSigmaVlqExt, WriteSigmaVlqExt};
use sigma_ser::ScorexSerializable;

use crate::chain::HeaderChain;
use crate::error::ChainError;

/// Header offsets from the tip to include in a V2 SyncInfo message.
const SYNC_OFFSETS: &[u32] = &[0, 16, 128, 512];

/// Maximum header IDs in a V1 SyncInfo message.
const MAX_V1_HEADER_IDS: u16 = 1001;

/// Maximum headers in a V2 SyncInfo message.
const MAX_V2_HEADERS: u8 = 50;

/// Maximum byte size of a single serialized header in V2.
const MAX_V2_HEADER_SIZE: u16 = 1000;

/// V2 mode marker byte (0xFF = -1 as signed byte).
const V2_MODE_BYTE: u8 = 0xFF;

/// Parsed SyncInfo message — used by the sync state machine to compare
/// chain tips with peers.
#[derive(Debug)]
pub enum SyncInfo {
    /// Legacy format: list of header IDs only.
    V1 { header_ids: Vec<BlockId> },
    /// Current format (>= 4.0.16): full serialized headers.
    V2 { headers: Vec<Header> },
}

/// Build a V2 SyncInfo body from the chain's current state.
///
/// Selects headers at offsets `[0, 16, 128, 512]` from the tip, ordered
/// tip-first. Offsets below the chain's starting height are skipped.
pub fn build_sync_info(chain: &HeaderChain) -> Vec<u8> {
    let mut buf = Vec::new();

    // V2 preamble: VLQ(0) to stop V1 parsers, then 0xFF mode byte
    buf.put_u16(0).expect("write to Vec cannot fail");
    buf.push(V2_MODE_BYTE);

    if chain.is_empty() {
        buf.push(0); // zero headers
        return buf;
    }

    let tip_height = chain.height();
    let base_height = chain.header_at(tip_height - (chain.len() as u32 - 1))
        .map(|h| h.height)
        .unwrap_or(1);

    // Collect headers at the specified offsets (tip-first).
    // Post-Phase-2, `header_at` returns owned headers; iteration
    // below borrows from this Vec.
    let mut selected: Vec<Header> = Vec::new();
    for &offset in SYNC_OFFSETS {
        let target = match tip_height.checked_sub(offset) {
            Some(h) if h >= base_height => h,
            _ => continue,
        };
        if let Some(header) = chain.header_at(target) {
            selected.push(header);
        }
    }

    buf.push(selected.len() as u8);

    for header in &selected {
        let header_bytes = header
            .scorex_serialize_bytes()
            .expect("serializing a valid header cannot fail");
        buf.put_u16(header_bytes.len() as u16)
            .expect("write to Vec cannot fail");
        buf.extend_from_slice(&header_bytes);
    }

    buf
}

/// Parse a SyncInfo body, handling both V1 and V2 wire formats.
///
/// Never panics on malformed input — returns `Err(ChainError::SyncInfo)`.
pub fn parse_sync_info(body: &[u8]) -> Result<SyncInfo, ChainError> {
    if body.is_empty() {
        return Err(ChainError::SyncInfo("empty body".into()));
    }

    let mut r = Cursor::new(body);
    let count = r.get_u16().map_err(|e| ChainError::SyncInfo(e.to_string()))?;

    if count > 0 {
        parse_v1(&mut r, count)
    } else {
        parse_v2(&mut r)
    }
}

fn parse_v1(r: &mut Cursor<&[u8]>, count: u16) -> Result<SyncInfo, ChainError> {
    if count > MAX_V1_HEADER_IDS {
        return Err(ChainError::SyncInfo(format!(
            "V1 header ID count {count} exceeds max {MAX_V1_HEADER_IDS}"
        )));
    }

    let mut header_ids = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut id_bytes = [0u8; 32];
        std::io::Read::read_exact(r, &mut id_bytes)
            .map_err(|e| ChainError::SyncInfo(format!("V1 truncated: {e}")))?;
        header_ids.push(BlockId(Digest32::from(id_bytes)));
    }

    Ok(SyncInfo::V1 { header_ids })
}

fn parse_v2(r: &mut Cursor<&[u8]>) -> Result<SyncInfo, ChainError> {
    let mode = r.get_u8().map_err(|e| ChainError::SyncInfo(format!("missing V2 mode byte: {e}")))?;
    if mode != V2_MODE_BYTE {
        return Err(ChainError::SyncInfo(format!(
            "expected V2 mode byte 0xFF, got 0x{mode:02X}"
        )));
    }

    let count = r.get_u8().map_err(|e| ChainError::SyncInfo(format!("missing V2 count: {e}")))?;
    if count > MAX_V2_HEADERS {
        return Err(ChainError::SyncInfo(format!(
            "V2 header count {count} exceeds max {MAX_V2_HEADERS}"
        )));
    }

    let mut headers = Vec::with_capacity(count as usize);
    for i in 0..count {
        let size = r.get_u16().map_err(|e| {
            ChainError::SyncInfo(format!("V2 header {i} size: {e}"))
        })?;
        if size > MAX_V2_HEADER_SIZE {
            return Err(ChainError::SyncInfo(format!(
                "V2 header {i} size {size} exceeds max {MAX_V2_HEADER_SIZE}"
            )));
        }
        let mut header_bytes = vec![0u8; size as usize];
        std::io::Read::read_exact(r, &mut header_bytes)
            .map_err(|e| ChainError::SyncInfo(format!("V2 header {i} truncated: {e}")))?;
        let header = Header::scorex_parse_bytes(&header_bytes)?;
        headers.push(header);
    }

    Ok(SyncInfo::V2 { headers })
}

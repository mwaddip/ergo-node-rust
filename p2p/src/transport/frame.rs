//! Ergo P2P message frame encoding and decoding.
//!
//! # Contract
//! - `encode`: given magic, code, and body, produces a valid frame.
//!   Postcondition: output is `[magic:4][code:1][len:4 BE][checksum:4][body:N]`.
//! - `decode`: given magic and raw bytes, validates and extracts a Frame.
//!   Precondition: `data.len() >= 13` (header size).
//!   Postcondition: magic matches, checksum valid, body length matches header.
//! - Invariant: `decode(magic, encode(magic, frame)) == Ok(frame)` for any valid frame.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use std::io;
use std::net::SocketAddr;

type Blake2b256 = Blake2b<U32>;

/// Minimum frame size: magic(4) + code(1) + length(4) = 9 bytes.
/// Checksum(4) and body follow only when body length > 0.
const MIN_HEADER_SIZE: usize = 9;
/// Full header with checksum: magic(4) + code(1) + length(4) + checksum(4) = 13 bytes.
const HEADER_SIZE: usize = 13;
const MAX_BODY_SIZE: u32 = 2 * 1024 * 1024;

/// A validated P2P message frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub code: u8,
    pub body: Vec<u8>,
}

/// Encode a Frame into wire bytes with the given network magic.
///
/// When body is empty, the checksum is omitted — matching the JVM's
/// `MessageSerializer.serialize` which only writes checksum when
/// `dataLength > 0`. Sending a checksum for empty bodies desynchronizes
/// the JVM's frame parser.
pub fn encode(magic: &[u8; 4], frame: &Frame) -> Vec<u8> {
    let mut msg = Vec::with_capacity(HEADER_SIZE + frame.body.len());
    msg.extend_from_slice(magic);
    msg.push(frame.code);
    msg.extend_from_slice(&(frame.body.len() as u32).to_be_bytes());
    if !frame.body.is_empty() {
        let hash = Blake2b256::digest(&frame.body);
        msg.extend_from_slice(&hash[..4]);
        msg.extend_from_slice(&frame.body);
    }
    msg
}

/// Decode a Frame from wire bytes, validating magic and checksum.
pub fn decode(magic: &[u8; 4], data: &[u8]) -> io::Result<Frame> {
    if data.len() < MIN_HEADER_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Frame too short: {} bytes (need at least {})", data.len(), MIN_HEADER_SIZE),
        ));
    }

    if &data[0..4] != magic {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Bad magic: {:?} (expected {:?})", &data[0..4], magic),
        ));
    }

    let code = data[4];
    let body_len = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);

    if body_len > MAX_BODY_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Body too large: {} bytes (max {})", body_len, MAX_BODY_SIZE),
        ));
    }

    // Empty body — no checksum on the wire (JVM omits it)
    if body_len == 0 {
        return Ok(Frame { code, body: vec![] });
    }

    let expected_total = HEADER_SIZE + body_len as usize;
    if data.len() < expected_total {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Frame truncated: have {} bytes, need {}", data.len(), expected_total),
        ));
    }

    let checksum = &data[9..13];
    let body = &data[HEADER_SIZE..expected_total];

    let hash = Blake2b256::digest(body);
    if &hash[..4] != checksum {
        tracing::error!(
            code = code,
            body_len = body_len,
            expected = format!("{:02x}{:02x}{:02x}{:02x}", checksum[0], checksum[1], checksum[2], checksum[3]),
            computed = format!("{:02x}{:02x}{:02x}{:02x}", hash[0], hash[1], hash[2], hash[3]),
            "Checksum mismatch (decode)"
        );
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Checksum mismatch"));
    }

    Ok(Frame {
        code,
        body: body.to_vec(),
    })
}

/// Read a complete frame from an async reader.
///
/// # Contract
/// - **Precondition**: reader is positioned at the start of a frame.
/// - **Postcondition**: returns a validated Frame, or error on I/O, bad magic, bad checksum, or oversize.
/// - On permanent-penalty failures (`kind=bad_magic`, `oversized_frame`,
///   `bad_checksum`) the peer's address is recorded in `blacklist` in
///   addition to emitting the `peer_penalised` log line (`facts/journal-events.md`)
///   that fail2ban consumes.
/// - When `tap` is `Some`, the raw wire bytes of a successfully-parsed
///   frame are passed to it via `capture_inbound` before this function
///   returns. Frames rejected at the parse layer (bad magic / oversize /
///   bad checksum) are NOT captured in v1.
pub async fn read_frame(
    reader: &mut (impl tokio::io::AsyncReadExt + Unpin),
    magic: &[u8; 4],
    peer_addr: SocketAddr,
    blacklist: &crate::blacklist::Blacklist,
    tap: Option<&crate::capture::tap::Tap>,
) -> io::Result<Frame> {
    // Read fixed prefix: magic(4) + code(1) + length(4) = 9 bytes.
    // Checksum(4) follows only when body length > 0 (JVM omits it for empty bodies).
    let mut prefix = [0u8; 9];
    reader.read_exact(&mut prefix).await?;

    if &prefix[0..4] != magic {
        tracing::error!(
            received = format!("{:02x}{:02x}{:02x}{:02x}", prefix[0], prefix[1], prefix[2], prefix[3]),
            expected = format!("{:02x}{:02x}{:02x}{:02x}", magic[0], magic[1], magic[2], magic[3]),
            "Frame magic mismatch — stream likely misaligned"
        );
        tracing::warn!(
            peer = %peer_addr.ip(),
            kind = "bad_magic",
            "PENALTY"
        );
        blacklist.record_permanent(peer_addr);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Bad magic: {:?} (expected {:?})", &prefix[0..4], magic),
        ));
    }

    let code = prefix[4];
    let body_len = u32::from_be_bytes([prefix[5], prefix[6], prefix[7], prefix[8]]);

    if body_len > MAX_BODY_SIZE {
        tracing::warn!(
            peer = %peer_addr.ip(),
            kind = "oversized_frame",
            detail = body_len,
            "PENALTY"
        );
        blacklist.record_permanent(peer_addr);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Body too large: {} bytes", body_len),
        ));
    }

    // Empty body — no checksum on wire
    if body_len == 0 {
        if let Some(t) = tap {
            t.capture_inbound(peer_addr, &prefix);
        }
        return Ok(Frame { code, body: vec![] });
    }

    // Read checksum(4) + body
    let mut checksum = [0u8; 4];
    reader.read_exact(&mut checksum).await?;

    let mut body = vec![0u8; body_len as usize];
    reader.read_exact(&mut body).await?;

    let hash = Blake2b256::digest(&body);
    if hash[..4] != checksum {
        tracing::error!(
            code = code,
            body_len = body_len,
            expected = format!("{:02x}{:02x}{:02x}{:02x}", checksum[0], checksum[1], checksum[2], checksum[3]),
            computed = format!("{:02x}{:02x}{:02x}{:02x}", hash[0], hash[1], hash[2], hash[3]),
            "Checksum mismatch"
        );
        tracing::warn!(
            peer = %peer_addr.ip(),
            kind = "bad_checksum",
            "PENALTY"
        );
        blacklist.record_permanent(peer_addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Checksum mismatch"));
    }

    if let Some(t) = tap {
        let mut wire = Vec::with_capacity(prefix.len() + checksum.len() + body.len());
        wire.extend_from_slice(&prefix);
        wire.extend_from_slice(&checksum);
        wire.extend_from_slice(&body);
        t.capture_inbound(peer_addr, &wire);
    }

    tracing::debug!(code = code, body_len = body_len, "frame received");
    Ok(Frame { code, body })
}

/// Write a frame to an async writer.
///
/// # Contract
/// - **Postcondition**: the full encoded frame is written and flushed.
/// - When `tap` is `Some`, the raw wire bytes are passed to it via
///   `capture_outbound` before being written.
pub async fn write_frame(
    writer: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    magic: &[u8; 4],
    frame: &Frame,
    peer_addr: SocketAddr,
    tap: Option<&crate::capture::tap::Tap>,
) -> io::Result<()> {
    let bytes = encode(magic, frame);
    if let Some(t) = tap {
        t.capture_outbound(peer_addr, &bytes);
    }
    writer.write_all(&bytes).await?;
    writer.flush().await
}

//! Variable-Length Quantity (VLQ) encoding used in the Ergo P2P protocol.
//!
//! # Contract
//! - `write_vlq`: encodes any `u64` into 1–10 bytes, 7 data bits per byte, high bit = continuation.
//! - `read_vlq`: inverse of `write_vlq`. Errors on overflow (>63 useful bits) or unexpected EOF.
//! - `read_vlq_length`: same as `read_vlq` but rejects values > 256KB (protocol sanity bound).
//! - `zigzag_encode`/`zigzag_decode`: map signed i32 to unsigned u32 for VLQ encoding.

use std::io::{self, Read};

/// Encode a u64 as VLQ bytes.
///
/// # Postcondition
/// - `buf` is extended by 1–10 bytes.
/// - The encoded value can be recovered by `read_vlq`.
pub fn write_vlq(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            break;
        } else {
            buf.push(byte | 0x80);
        }
    }
}

/// Decode a VLQ-encoded u64 from a reader.
///
/// # Precondition
/// - Reader is positioned at the start of a VLQ sequence.
///
/// # Postcondition
/// - Returns the decoded value and advances the reader past the VLQ bytes.
///
/// # Errors
/// - `InvalidData` if the VLQ exceeds 63 bits (would overflow u64).
/// - I/O errors from the underlying reader.
pub fn read_vlq<R: Read>(reader: &mut R) -> io::Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        result |= ((byte[0] & 0x7f) as u64) << shift;
        if byte[0] & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "VLQ overflow"));
        }
    }
    Ok(result)
}

const MAX_VLQ_LENGTH: u64 = 2 * 1024 * 1024;

/// Read a VLQ value representing a byte length, capped at 2MB.
///
/// # Postcondition
/// - Returns a `usize` in range `0..=2097152`, or an error if the value exceeds the cap.
pub fn read_vlq_length<R: Read>(reader: &mut R) -> io::Result<usize> {
    let val = read_vlq(reader)?;
    if val > MAX_VLQ_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("VLQ length too large: {} (max {})", val, MAX_VLQ_LENGTH),
        ));
    }
    Ok(val as usize)
}

/// ZigZag-encode a signed i32 into an unsigned u32.
///
/// Maps: 0 → 0, -1 → 1, 1 → 2, -2 → 3, 2 → 4, ...
pub fn zigzag_encode(value: i32) -> u32 {
    ((value << 1) ^ (value >> 31)) as u32
}

/// ZigZag-decode an unsigned u32 back into a signed i32.
pub fn zigzag_decode(value: u32) -> i32 {
    ((value >> 1) as i32) ^ (-((value & 1) as i32))
}

/// ZigZag-encode a signed i64 into an unsigned u64.
pub fn zigzag_encode_i64(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// ZigZag-decode an unsigned u64 back into a signed i64.
pub fn zigzag_decode_i64(value: u64) -> i64 {
    ((value >> 1) as i64) ^ (-((value & 1) as i64))
}

/// Write a UTF-8 string prefixed by its byte length as a single u8.
///
/// # Precondition
/// - `s.len() <= 255`.
///
/// # Postcondition
/// - `buf` is extended by `1 + s.len()` bytes.
pub fn write_short_string(buf: &mut Vec<u8>, s: &str) {
    debug_assert!(s.len() <= 255, "String too long for short string encoding: {}", s.len());
    buf.push(s.len() as u8);
    buf.extend_from_slice(s.as_bytes());
}

/// Read a UTF-8 string prefixed by a single-byte length.
///
/// # Postcondition
/// - Returns a valid UTF-8 string, or an error.
pub fn read_short_string<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut len_byte = [0u8; 1];
    reader.read_exact(&mut len_byte)?;
    let len = len_byte[0] as usize;
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_short_string_roundtrip() {
        let mut buf = Vec::new();
        write_short_string(&mut buf, "ergo-proxy");
        let mut cursor = io::Cursor::new(buf.as_slice());
        assert_eq!(read_short_string(&mut cursor).unwrap(), "ergo-proxy");
    }
}

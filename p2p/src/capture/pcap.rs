//! pcap file header + per-record encoding.
//!
//! The ring file is a standard pcap file with `LINKTYPE_USER0` framing.
//! Each record is `[pcap rec hdr (16B)][Ergo metadata (24B)][frame bytes (N)]`.
//! Wireshark / tshark / `tcpdump -r` parse the file directly; the Ergo
//! metadata block sits inside `incl_len` so tools see one packet of
//! `24 + N` bytes per frame.

use std::net::IpAddr;
use std::time::SystemTime;

pub const PCAP_FILE_HEADER_LEN: usize = 24;
pub const PCAP_RECORD_HEADER_LEN: usize = 16;
pub const ERGO_METADATA_LEN: usize = 24;

pub const PCAP_MAGIC: u32 = 0xA1B2C3D4;
/// LINKTYPE_USER0 — Ergo frames carry their own framing, no link layer.
pub const LINKTYPE_USER0: u32 = 147;
/// Default snaplen — max Ergo frame body is 2 MiB.
pub const DEFAULT_SNAPLEN: u32 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Inbound = 0,
    Outbound = 1,
}

/// Write the global pcap file header. `out` must have at least
/// `PCAP_FILE_HEADER_LEN` bytes.
pub fn write_file_header(out: &mut [u8], snaplen: u32) {
    assert!(out.len() >= PCAP_FILE_HEADER_LEN);
    out[0..4].copy_from_slice(&PCAP_MAGIC.to_le_bytes());
    out[4..6].copy_from_slice(&2u16.to_le_bytes()); // version_major
    out[6..8].copy_from_slice(&4u16.to_le_bytes()); // version_minor
    out[8..12].copy_from_slice(&0u32.to_le_bytes()); // thiszone
    out[12..16].copy_from_slice(&0u32.to_le_bytes()); // sigfigs
    out[16..20].copy_from_slice(&snaplen.to_le_bytes());
    out[20..24].copy_from_slice(&LINKTYPE_USER0.to_le_bytes());
}

/// Total record size given the frame length.
pub fn record_size(frame_len: usize) -> usize {
    PCAP_RECORD_HEADER_LEN + ERGO_METADATA_LEN + frame_len
}

/// Write a complete record (pcap rec header + Ergo metadata + frame bytes)
/// to `out`. Returns total bytes written.
pub fn write_record(
    out: &mut [u8],
    ts: SystemTime,
    direction: Direction,
    peer_ip: IpAddr,
    peer_port: u16,
    frame_bytes: &[u8],
) -> usize {
    let total = record_size(frame_bytes.len());
    assert!(out.len() >= total);

    let dur = ts
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let ts_sec = dur.as_secs() as u32;
    let ts_usec = dur.subsec_micros();
    let incl_len = (ERGO_METADATA_LEN + frame_bytes.len()) as u32;

    // pcap record header
    out[0..4].copy_from_slice(&ts_sec.to_le_bytes());
    out[4..8].copy_from_slice(&ts_usec.to_le_bytes());
    out[8..12].copy_from_slice(&incl_len.to_le_bytes());
    out[12..16].copy_from_slice(&incl_len.to_le_bytes()); // orig_len same (no truncation)

    // Ergo metadata
    out[16] = direction as u8;
    out[17] = 0; // reserved
    out[18..20].copy_from_slice(&peer_port.to_be_bytes());
    let ip_bytes: [u8; 16] = match peer_ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    };
    out[20..36].copy_from_slice(&ip_bytes);
    out[36..40].copy_from_slice(&0u32.to_le_bytes()); // reserved

    // Frame bytes verbatim
    out[40..40 + frame_bytes.len()].copy_from_slice(frame_bytes);

    total
}

/// Read direction byte from a record's metadata block (offset 16 within the
/// record). Returns None if value is invalid.
pub fn read_direction(record: &[u8]) -> Option<Direction> {
    if record.len() < PCAP_RECORD_HEADER_LEN + 1 {
        return None;
    }
    match record[PCAP_RECORD_HEADER_LEN] {
        0 => Some(Direction::Inbound),
        1 => Some(Direction::Outbound),
        _ => None,
    }
}

/// Read peer IP from a record's metadata block.
pub fn read_peer_ip(record: &[u8]) -> Option<IpAddr> {
    let off = PCAP_RECORD_HEADER_LEN + 4;
    if record.len() < off + 16 {
        return None;
    }
    let mut ip_bytes = [0u8; 16];
    ip_bytes.copy_from_slice(&record[off..off + 16]);
    let v6 = std::net::Ipv6Addr::from(ip_bytes);
    // IPv4-mapped form (::ffff:a.b.c.d) → expose as Ipv4
    Some(match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    })
}

/// Read ts_sec from the pcap record header.
pub fn read_ts_sec(record: &[u8]) -> Option<u32> {
    if record.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes(record[0..4].try_into().unwrap()))
}

/// Read incl_len (24 + N) from the pcap record header. Returns None if
/// the record header is too short.
pub fn read_incl_len(record: &[u8]) -> Option<u32> {
    if record.len() < 12 {
        return None;
    }
    Some(u32::from_le_bytes(record[8..12].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn file_header_magic_and_linktype() {
        let mut buf = [0u8; PCAP_FILE_HEADER_LEN];
        write_file_header(&mut buf, DEFAULT_SNAPLEN);
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(magic, PCAP_MAGIC);
        let network = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        assert_eq!(network, LINKTYPE_USER0);
        let snaplen = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        assert_eq!(snaplen, DEFAULT_SNAPLEN);
        let vmajor = u16::from_le_bytes(buf[4..6].try_into().unwrap());
        let vminor = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        assert_eq!((vmajor, vminor), (2, 4));
    }

    #[test]
    fn record_ipv4_inbound() {
        let frame = b"hello frame";
        let mut buf = vec![0u8; record_size(frame.len())];
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234567);
        let ip = IpAddr::from_str("10.0.0.1").unwrap();
        let n = write_record(&mut buf, ts, Direction::Inbound, ip, 9030, frame);
        assert_eq!(n, buf.len());

        // pcap record header
        let ts_sec = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(ts_sec, 1234567);
        let incl_len = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(incl_len as usize, ERGO_METADATA_LEN + frame.len());
        let orig_len = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        assert_eq!(orig_len, incl_len);

        // Ergo metadata
        assert_eq!(buf[16], 0); // direction = inbound
        assert_eq!(buf[17], 0); // reserved
        let port = u16::from_be_bytes(buf[18..20].try_into().unwrap());
        assert_eq!(port, 9030);
        // IPv4 mapped to ::ffff:10.0.0.1 → bytes 20..36 contain the v4-mapped form
        assert_eq!(&buf[20 + 10..20 + 12], &[0xff, 0xff]);
        assert_eq!(&buf[20 + 12..20 + 16], &[10, 0, 0, 1]);

        // Frame bytes follow at offset 40
        assert_eq!(&buf[40..40 + frame.len()], frame);
    }

    #[test]
    fn record_ipv6_outbound() {
        let frame = b"hi6";
        let mut buf = vec![0u8; record_size(frame.len())];
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(42);
        let ip = IpAddr::from_str("2001:db8::1").unwrap();
        write_record(&mut buf, ts, Direction::Outbound, ip, 443, frame);
        assert_eq!(buf[16], 1); // direction = outbound
        let port = u16::from_be_bytes(buf[18..20].try_into().unwrap());
        assert_eq!(port, 443);
        // IPv6 bytes verbatim
        let mut want = [0u8; 16];
        if let IpAddr::V6(v6) = ip {
            want = v6.octets();
        }
        assert_eq!(&buf[20..36], &want);
        assert_eq!(&buf[40..40 + frame.len()], frame);
    }

    #[test]
    fn read_helpers_roundtrip() {
        let frame = b"payload";
        let mut buf = vec![0u8; record_size(frame.len())];
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(9001);
        let ip = IpAddr::from_str("192.0.2.42").unwrap();
        write_record(&mut buf, ts, Direction::Outbound, ip, 9030, frame);

        assert_eq!(read_ts_sec(&buf), Some(9001));
        assert_eq!(read_incl_len(&buf), Some((ERGO_METADATA_LEN + frame.len()) as u32));
        assert_eq!(read_direction(&buf), Some(Direction::Outbound));
        assert_eq!(read_peer_ip(&buf), Some(ip));
    }

    #[test]
    fn read_peer_ip_v6_preserves_non_mapped() {
        let frame = b"x";
        let mut buf = vec![0u8; record_size(frame.len())];
        let ts = SystemTime::UNIX_EPOCH;
        let ip = IpAddr::from_str("2001:db8::dead:beef").unwrap();
        write_record(&mut buf, ts, Direction::Inbound, ip, 1, frame);
        assert_eq!(read_peer_ip(&buf), Some(ip));
    }
}

//! Hot-path capture tap.
//!
//! Installed at the transport layer's frame-read and frame-write sites
//! (see `transport::frame`). When enabled, every frame the transport
//! handles is filtered by peer IP and appended to the ring as a
//! pcap-format record.
//!
//! Filter check is O(1) HashSet probe (~10 ns for typical lists). On a
//! filter pass, the per-record cost is one Vec allocation for the
//! record buffer, one memcpy into the mmap'd page, and the atomic head
//! update inside the ring's mutex. v1.1 optimization to consider:
//! ring-side `claim_slot(size) -> &mut [u8]` API to avoid the Vec.

use super::config::FilterMode;
use super::pcap::{record_size, write_record, Direction};
use super::ring::RingBuffer;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::SystemTime;

pub struct Tap {
    ring: Arc<RingBuffer>,
    filter: FilterMode,
}

impl Tap {
    pub fn new(ring: Arc<RingBuffer>, filter: FilterMode) -> Self {
        Self { ring, filter }
    }

    /// Capture an inbound frame. Called from transport after a successful
    /// frame read.
    pub fn capture_inbound(&self, peer: SocketAddr, frame: &[u8]) {
        if !self.passes_filter(peer.ip()) {
            return;
        }
        self.write(SystemTime::now(), Direction::Inbound, peer, frame);
    }

    /// Capture an outbound frame. Called from transport before writing.
    pub fn capture_outbound(&self, peer: SocketAddr, frame: &[u8]) {
        if !self.passes_filter(peer.ip()) {
            return;
        }
        self.write(SystemTime::now(), Direction::Outbound, peer, frame);
    }

    fn passes_filter(&self, ip: IpAddr) -> bool {
        match &self.filter {
            FilterMode::None => true,
            FilterMode::Include(set) => set.contains(&ip),
            FilterMode::Exclude(set) => !set.contains(&ip),
        }
    }

    fn write(&self, ts: SystemTime, dir: Direction, peer: SocketAddr, frame: &[u8]) {
        let total = record_size(frame.len());
        let mut buf = vec![0u8; total];
        write_record(&mut buf, ts, dir, peer.ip(), peer.port(), frame);
        // Append errors (record too large) are silently swallowed — capture
        // is debug-only and we don't want a degenerate input from a peer to
        // panic the transport.
        let _ = self.ring.append(&buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::pcap::{
        read_direction, read_peer_ip, ERGO_METADATA_LEN, PCAP_RECORD_HEADER_LEN,
    };
    use std::collections::HashSet;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn new_tap(filter: FilterMode) -> (tempfile::TempDir, Arc<RingBuffer>, Tap) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cap.ring");
        let ring = Arc::new(RingBuffer::open_or_create(&path, 1024 * 1024).unwrap());
        let tap = Tap::new(ring.clone(), filter);
        (dir, ring, tap)
    }

    #[test]
    fn no_filter_captures_all() {
        let (_d, ring, tap) = new_tap(FilterMode::None);
        let head0 = ring.write_head.load(std::sync::atomic::Ordering::Acquire);
        tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"hello");
        let head1 = ring.write_head.load(std::sync::atomic::Ordering::Acquire);
        assert!(head1 > head0);
    }

    #[test]
    fn include_filter_passes_listed_drops_others() {
        let mut set = HashSet::new();
        set.insert(IpAddr::from_str("10.0.0.1").unwrap());
        let (_d, ring, tap) = new_tap(FilterMode::Include(set));

        let head0 = ring.write_head.load(std::sync::atomic::Ordering::Acquire);
        tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"included");
        let head_after_include = ring.write_head.load(std::sync::atomic::Ordering::Acquire);
        assert!(head_after_include > head0);

        tap.capture_inbound("10.0.0.2:9030".parse().unwrap(), b"excluded");
        let head_after_excluded = ring.write_head.load(std::sync::atomic::Ordering::Acquire);
        assert_eq!(head_after_excluded, head_after_include); // dropped
    }

    #[test]
    fn exclude_filter_drops_listed_passes_others() {
        let mut set = HashSet::new();
        set.insert(IpAddr::from_str("192.168.1.100").unwrap());
        let (_d, ring, tap) = new_tap(FilterMode::Exclude(set));

        let head0 = ring.write_head.load(std::sync::atomic::Ordering::Acquire);
        tap.capture_inbound("192.168.1.100:9030".parse().unwrap(), b"dropped");
        assert_eq!(ring.write_head.load(std::sync::atomic::Ordering::Acquire), head0);

        tap.capture_inbound("8.8.8.8:9030".parse().unwrap(), b"kept");
        assert!(ring.write_head.load(std::sync::atomic::Ordering::Acquire) > head0);
    }

    #[test]
    fn round_trip_captured_bytes_at_expected_offset() {
        let (_d, ring, tap) = new_tap(FilterMode::None);
        let head0 = ring.write_head.load(std::sync::atomic::Ordering::Acquire);
        let peer: SocketAddr = "10.0.0.1:9030".parse().unwrap();
        let frame = b"wire frame bytes";
        tap.capture_inbound(peer, frame);

        let bytes = ring.snapshot();
        let record = &bytes[head0 as usize..];
        assert_eq!(read_direction(record), Some(Direction::Inbound));
        assert_eq!(read_peer_ip(record), Some(peer.ip()));
        // Frame bytes at offset (16 + 24) within the record
        let frame_off = PCAP_RECORD_HEADER_LEN + ERGO_METADATA_LEN;
        assert_eq!(&record[frame_off..frame_off + frame.len()], frame);
    }

    #[test]
    fn outbound_direction_distinguished() {
        let (_d, ring, tap) = new_tap(FilterMode::None);
        let head0 = ring.write_head.load(std::sync::atomic::Ordering::Acquire);
        tap.capture_outbound("10.0.0.1:9030".parse().unwrap(), b"x");
        let bytes = ring.snapshot();
        assert_eq!(
            read_direction(&bytes[head0 as usize..]),
            Some(Direction::Outbound)
        );
    }
}

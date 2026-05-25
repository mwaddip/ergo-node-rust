//! Chronological dump iterator + dump-time filters.
//!
//! Walks the ring in storage order, parses pcap-format records, skips
//! `0xFF` padding stretches, applies the operator's filter, and returns
//! the surviving records sorted by timestamp.
//!
//! Snapshotting strategy: copy the data region into an owned `Vec<u8>`
//! once (so the ring's write mutex is released before parsing). Writes
//! that land during parsing are invisible to this dump — that's the
//! design (snapshot-anchored read).

use super::config::FilterMode;
use super::pcap::{
    read_direction, read_incl_len, read_peer_ip, read_ts_sec, Direction,
    ERGO_METADATA_LEN, PCAP_RECORD_HEADER_LEN,
};
use super::ring::{RingBuffer, PAD_BYTE};
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

/// Soft cap on a single record's `incl_len` for sanity. Real frames cap
/// at 2 MiB body + 13 B framing + 24 B metadata. Padding heuristics catch
/// most garbage; this is a final guard against runaway parses on a
/// corrupted region.
const MAX_PLAUSIBLE_RECORD_BODY: usize = 2 * 1024 * 1024 + 64;

#[derive(Debug, Clone)]
pub struct DumpRecord {
    pub ts: SystemTime,
    pub direction: Direction,
    pub peer_ip: IpAddr,
    pub peer_port: u16,
    /// Frame bytes verbatim (the captured wire frame).
    pub frame: Vec<u8>,
    /// pcap rec header + Ergo metadata + frame, contiguous.
    pub raw_record: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct DumpFilter {
    pub peer: Option<IpAddr>,
    /// Only records with `ts_sec` within the last N seconds of `now`.
    pub since_secs: Option<u64>,
    pub direction: Option<Direction>,
}

impl DumpFilter {
    pub fn passes(&self, record: &DumpRecord, now: SystemTime) -> bool {
        if let Some(p) = self.peer {
            if record.peer_ip != p {
                return false;
            }
        }
        if let Some(secs) = self.since_secs {
            if let Some(cutoff) = now.checked_sub(Duration::from_secs(secs)) {
                if record.ts < cutoff {
                    return false;
                }
            }
        }
        if let Some(d) = self.direction {
            if record.direction != d {
                return false;
            }
        }
        true
    }

    /// Convert a `[debug.p2p_capture]` startup filter to a no-op dump
    /// filter. Static filtering happens at capture time; dump-time
    /// filters are operator-specified per request.
    #[allow(dead_code)]
    pub(crate) fn from_static(_: &FilterMode) -> Self {
        Self::default()
    }
}

/// Collect all records from the ring that pass the filter, sorted by
/// timestamp. Materializes a snapshot of the data region into an owned
/// buffer before parsing — the ring's write mutex is held only for the
/// snapshot copy, not for parsing.
pub fn collect_chronological(
    ring: &RingBuffer,
    now: SystemTime,
    filter: &DumpFilter,
) -> Vec<DumpRecord> {
    let (head, gen) = ring.position();
    let drs = ring.data_region_start as usize;
    let dre = ring.data_region_end as usize;

    // Snapshot the data region. The mutex is held for one memcpy.
    let snapshot: Vec<u8> = {
        let guard = ring.snapshot();
        guard[drs..dre].to_vec()
    };

    // Bound the walk: gen=0 means writes haven't wrapped yet, so anything
    // beyond `head` is uninitialized — stop at `head`. gen>0 means the
    // whole region is written content (with possible 0xFF padding before
    // the wrap point); walk all of it.
    let walk_end = if gen == 0 {
        (head as usize).saturating_sub(drs)
    } else {
        snapshot.len()
    };

    let mut records = Vec::new();
    let mut pos = 0;

    while pos < walk_end {
        // 0xFF padding: per wrap policy, padding always runs to the end
        // of the data region. Skip the rest of the snapshot.
        if snapshot[pos] == PAD_BYTE {
            // Confirm: pcap ts_sec field would be 0xFFFFFFFF, year 2106.
            if pos + 4 <= snapshot.len()
                && snapshot[pos..pos + 4] == [PAD_BYTE; 4]
            {
                break;
            }
        }

        // Need at least a pcap record header
        if pos + PCAP_RECORD_HEADER_LEN > walk_end {
            break;
        }

        let record_slice = &snapshot[pos..];
        let ts_sec = read_ts_sec(record_slice).unwrap_or(0);
        let incl_len = match read_incl_len(record_slice) {
            Some(n) => n as usize,
            None => break,
        };

        // Sanity: incl_len must cover at least the Ergo metadata block
        if incl_len < ERGO_METADATA_LEN {
            break;
        }
        let body_len = incl_len - ERGO_METADATA_LEN;
        if body_len > MAX_PLAUSIBLE_RECORD_BODY {
            break;
        }

        let total = PCAP_RECORD_HEADER_LEN + incl_len;
        if pos + total > walk_end {
            break;
        }

        let ts_usec = u32::from_le_bytes(snapshot[pos + 4..pos + 8].try_into().unwrap());
        let ts = SystemTime::UNIX_EPOCH
            + Duration::new(ts_sec as u64, ts_usec.saturating_mul(1000));

        let direction = read_direction(&snapshot[pos..pos + total]).unwrap_or(Direction::Inbound);
        let peer_ip =
            read_peer_ip(&snapshot[pos..pos + total]).unwrap_or(IpAddr::V4([0, 0, 0, 0].into()));
        let peer_port =
            u16::from_be_bytes(snapshot[pos + 18..pos + 20].try_into().unwrap());

        let frame_off = pos + PCAP_RECORD_HEADER_LEN + ERGO_METADATA_LEN;
        let frame = snapshot[frame_off..pos + total].to_vec();
        let raw_record = snapshot[pos..pos + total].to_vec();

        let record = DumpRecord {
            ts,
            direction,
            peer_ip,
            peer_port,
            frame,
            raw_record,
        };

        if filter.passes(&record, now) {
            records.push(record);
        }

        pos += total;
    }

    records.sort_by_key(|r| r.ts);
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::pcap::{record_size, write_record};
    use std::str::FromStr;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn new_ring(size: u64) -> (tempfile::TempDir, Arc<RingBuffer>) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cap.ring");
        let ring = Arc::new(RingBuffer::open_or_create(&path, size).unwrap());
        (dir, ring)
    }

    fn append(ring: &RingBuffer, ts_secs: u64, dir: Direction, ip: &str, port: u16, frame: &[u8]) {
        let mut buf = vec![0u8; record_size(frame.len())];
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(ts_secs);
        write_record(&mut buf, ts, dir, IpAddr::from_str(ip).unwrap(), port, frame);
        ring.append(&buf).unwrap();
    }

    #[test]
    fn empty_ring_yields_nothing() {
        let (_d, ring) = new_ring(64 * 1024);
        let records = collect_chronological(&ring, SystemTime::now(), &DumpFilter::default());
        assert_eq!(records.len(), 0);
    }

    #[test]
    fn three_records_in_chronological_order() {
        let (_d, ring) = new_ring(64 * 1024);
        // Insert out-of-order to verify sort
        append(&ring, 100, Direction::Inbound, "10.0.0.1", 9030, b"first");
        append(&ring, 300, Direction::Outbound, "10.0.0.2", 9030, b"third");
        append(&ring, 200, Direction::Inbound, "10.0.0.3", 9030, b"second");

        let records = collect_chronological(&ring, SystemTime::now(), &DumpFilter::default());
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].frame, b"first");
        assert_eq!(records[1].frame, b"second");
        assert_eq!(records[2].frame, b"third");
    }

    #[test]
    fn peer_filter_drops_others() {
        let (_d, ring) = new_ring(64 * 1024);
        append(&ring, 100, Direction::Inbound, "10.0.0.1", 9030, b"keep");
        append(&ring, 200, Direction::Inbound, "10.0.0.2", 9030, b"drop");

        let filter = DumpFilter {
            peer: Some(IpAddr::from_str("10.0.0.1").unwrap()),
            ..Default::default()
        };
        let records = collect_chronological(&ring, SystemTime::now(), &filter);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].frame, b"keep");
    }

    #[test]
    fn direction_filter() {
        let (_d, ring) = new_ring(64 * 1024);
        append(&ring, 100, Direction::Inbound, "10.0.0.1", 9030, b"in");
        append(&ring, 200, Direction::Outbound, "10.0.0.1", 9030, b"out");

        let filter = DumpFilter {
            direction: Some(Direction::Outbound),
            ..Default::default()
        };
        let records = collect_chronological(&ring, SystemTime::now(), &filter);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].frame, b"out");
    }

    #[test]
    fn since_secs_drops_old() {
        let (_d, ring) = new_ring(64 * 1024);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        append(&ring, 100, Direction::Inbound, "10.0.0.1", 9030, b"old");
        append(&ring, 9_950, Direction::Inbound, "10.0.0.1", 9030, b"recent");

        let filter = DumpFilter {
            since_secs: Some(100),
            ..Default::default()
        };
        let records = collect_chronological(&ring, now, &filter);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].frame, b"recent");
    }

    #[test]
    fn walk_skips_padding_after_wrap() {
        // Tiny ring; insert records until wrap, then more records post-wrap.
        let size = 4096;
        let (_d, ring) = new_ring(size);

        // Fill with chunks of distinct content until we wrap
        let mut counter = 0u64;
        let initial_gen = ring.generation.load(std::sync::atomic::Ordering::Acquire);
        while ring.generation.load(std::sync::atomic::Ordering::Acquire) == initial_gen {
            append(
                &ring,
                1_000_000 + counter,
                Direction::Inbound,
                "10.0.0.1",
                9030,
                b"x",
            );
            counter += 1;
            if counter > 1000 {
                panic!("ring failed to wrap");
            }
        }
        // Add one more post-wrap record
        append(&ring, 9_999_999, Direction::Outbound, "10.0.0.2", 9030, b"post-wrap");

        let records = collect_chronological(&ring, SystemTime::now(), &DumpFilter::default());
        // The post-wrap record should be present, with the latest timestamp
        let last = records.last().unwrap();
        assert_eq!(last.frame, b"post-wrap");
        assert_eq!(last.peer_ip, IpAddr::from_str("10.0.0.2").unwrap());
        // Some pre-wrap records should also be present (those not overwritten)
        assert!(records.len() > 1);
    }

    #[test]
    fn filter_combines_with_and() {
        let (_d, ring) = new_ring(64 * 1024);
        append(&ring, 100, Direction::Inbound, "10.0.0.1", 9030, b"a");
        append(&ring, 100, Direction::Outbound, "10.0.0.1", 9030, b"b");
        append(&ring, 100, Direction::Inbound, "10.0.0.2", 9030, b"c");

        let filter = DumpFilter {
            peer: Some(IpAddr::from_str("10.0.0.1").unwrap()),
            direction: Some(Direction::Inbound),
            ..Default::default()
        };
        let records = collect_chronological(&ring, SystemTime::now(), &filter);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].frame, b"a");
    }
}

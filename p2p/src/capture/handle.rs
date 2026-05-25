//! Public surface for the capture subsystem.
//!
//! `CaptureHandle` owns the `RingBuffer` + `Tap` + resolved config and
//! implements `CaptureAccess` — the trait the `api/` crate consumes to
//! serve `/debug/p2p-capture/{info,dump,reset}` without depending on
//! the concrete handle type. Pass `handle.tap()` to the p2p transport
//! and `Arc<dyn CaptureAccess>` (cloned from the handle) to api/.

use super::config::{FilterMode, ResolvedCaptureConfig};
use super::dump::{collect_chronological, DumpFilter};
use super::pcap::{write_file_header, DEFAULT_SNAPLEN, PCAP_FILE_HEADER_LEN};
use super::ring::{RingBuffer, RingError};
use super::tap::Tap;
use serde::Serialize;
use std::sync::Arc;
use std::time::SystemTime;

/// Public API surface consumed by `api/` and integration tests. Mockable
/// for unit tests that don't want to spin up a real ring.
pub trait CaptureAccess: Send + Sync {
    /// Lightweight metadata: enabled flag, ring path/size, write_head,
    /// generation, oldest/newest captured timestamps, filter summary.
    fn info(&self) -> CaptureInfo;

    /// Render the full pcap file (24-byte header + filtered records in
    /// chronological order) as a contiguous byte buffer.
    fn dump(&self, filter: &DumpFilter) -> Vec<u8>;

    /// Reset write_head to start-of-data-region and bump generation.
    fn reset(&self);
}

#[derive(Debug, Serialize)]
pub struct CaptureInfo {
    pub enabled: bool,
    pub path: String,
    pub size_mb: u64,
    pub write_head: u64,
    pub generation: u64,
    pub oldest_ts: Option<String>,
    pub newest_ts: Option<String>,
    /// "none" | "include" | "exclude"
    pub filter_mode: &'static str,
    pub filter_count: usize,
}

pub struct CaptureHandle {
    ring: Arc<RingBuffer>,
    tap: Arc<Tap>,
    config: ResolvedCaptureConfig,
}

impl CaptureHandle {
    /// Get the shared `Tap` handle to wire into the p2p transport.
    pub fn tap(&self) -> Arc<Tap> {
        self.tap.clone()
    }

    /// Borrow the ring (debugging / advanced use only).
    pub fn ring(&self) -> &Arc<RingBuffer> {
        &self.ring
    }
}

impl CaptureAccess for CaptureHandle {
    fn info(&self) -> CaptureInfo {
        let (head, generation) = self.ring.position();
        let (filter_mode, filter_count) = match &self.config.filter {
            FilterMode::None => ("none", 0),
            FilterMode::Include(set) => ("include", set.len()),
            FilterMode::Exclude(set) => ("exclude", set.len()),
        };

        // Compute oldest/newest from a chronological walk. For very large
        // rings this is O(ring_size) per /info call; operators using info
        // on a 1 GB ring should expect a sub-second response. Cache if
        // profiling later shows it matters.
        let records = collect_chronological(&self.ring, SystemTime::now(), &DumpFilter::default());
        let oldest_ts = records.first().map(|r| format_ts(r.ts));
        let newest_ts = records.last().map(|r| format_ts(r.ts));

        CaptureInfo {
            enabled: true,
            path: self.config.path.display().to_string(),
            size_mb: self.config.size_bytes / (1024 * 1024),
            write_head: head,
            generation,
            oldest_ts,
            newest_ts,
            filter_mode,
            filter_count,
        }
    }

    fn dump(&self, filter: &DumpFilter) -> Vec<u8> {
        let records = collect_chronological(&self.ring, SystemTime::now(), filter);

        // Pre-size: pcap header + sum of raw_record lengths
        let total: usize =
            PCAP_FILE_HEADER_LEN + records.iter().map(|r| r.raw_record.len()).sum::<usize>();
        let mut out = Vec::with_capacity(total);
        out.resize(PCAP_FILE_HEADER_LEN, 0);
        write_file_header(&mut out[0..PCAP_FILE_HEADER_LEN], DEFAULT_SNAPLEN);

        for record in records {
            out.extend_from_slice(&record.raw_record);
        }

        out
    }

    fn reset(&self) {
        self.ring.reset();
    }
}

/// Initialize the capture subsystem from a validated config.
///
/// Pass the returned handle's `tap()` to `p2p::node::P2pNode::start` and
/// the handle itself (wrapped in `Arc<dyn CaptureAccess>`) to the api
/// crate's state.
pub fn init(config: ResolvedCaptureConfig) -> Result<CaptureHandle, RingError> {
    let ring = Arc::new(RingBuffer::open_or_create(&config.path, config.size_bytes)?);
    let tap = Arc::new(Tap::new(ring.clone(), config.filter.clone()));
    Ok(CaptureHandle { ring, tap, config })
}

fn format_ts(ts: SystemTime) -> String {
    // ISO 8601 with microsecond precision. We don't pull in chrono just
    // for this — a hand-built representation suffices for an operator
    // diagnostic value.
    let dur = ts
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let micros = dur.subsec_micros();
    // Decompose secs into Y-M-D H:M:S using a minimal calendar — to avoid
    // a chrono dep we compute via days-since-epoch.
    let days = (secs / 86400) as i64;
    let time_of_day = secs % 86400;
    let (h, m, s) = (
        time_of_day / 3600,
        (time_of_day / 60) % 60,
        time_of_day % 60,
    );
    let (y, mo, d) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        y, mo, d, h, m, s, micros
    )
}

/// Convert days since 1970-01-01 to (year, month, day). Proleptic
/// Gregorian. Adapted from the Howard Hinnant date algorithms.
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::config::CaptureConfig;
    use crate::capture::pcap::PCAP_MAGIC;
    use std::net::IpAddr;
    use std::path::PathBuf;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn small_config(path: PathBuf) -> ResolvedCaptureConfig {
        CaptureConfig {
            enabled: true,
            path: Some(path),
            size_mb: 1,
            sync_interval_secs: 0,
            include_ips: vec![],
            exclude_ips: vec![],
        }
        .resolve()
        .unwrap()
        .unwrap()
    }

    #[test]
    fn init_creates_ring_and_tap() {
        let dir = tempdir().unwrap();
        let cfg = small_config(dir.path().join("cap.ring"));
        let handle = init(cfg).unwrap();
        let info = handle.info();
        assert!(info.enabled);
        assert_eq!(info.size_mb, 1);
        assert_eq!(info.generation, 0);
        assert_eq!(info.filter_mode, "none");
    }

    #[test]
    fn info_oldest_newest_after_captures() {
        let dir = tempdir().unwrap();
        let handle = init(small_config(dir.path().join("cap.ring"))).unwrap();
        let tap = handle.tap();
        tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"frame1");
        // Tiny sleep so the second timestamp is distinct in microseconds
        std::thread::sleep(std::time::Duration::from_micros(2));
        tap.capture_outbound("10.0.0.2:9030".parse().unwrap(), b"frame2");
        let info = handle.info();
        assert!(info.oldest_ts.is_some());
        assert!(info.newest_ts.is_some());
        // newest >= oldest as strings (ISO8601 is lexically ordered)
        assert!(info.newest_ts >= info.oldest_ts);
    }

    #[test]
    fn dump_returns_valid_pcap_with_captured_frames() {
        let dir = tempdir().unwrap();
        let handle = init(small_config(dir.path().join("cap.ring"))).unwrap();
        let tap = handle.tap();
        tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"frame-content");

        let bytes = handle.dump(&DumpFilter::default());
        // pcap header at front
        assert_eq!(&bytes[0..4], &PCAP_MAGIC.to_le_bytes());
        // First record begins at offset 24
        let frame_off = PCAP_FILE_HEADER_LEN + 16 /* pcap rec hdr */ + 24 /* Ergo meta */;
        assert_eq!(
            &bytes[frame_off..frame_off + b"frame-content".len()],
            b"frame-content"
        );
    }

    #[test]
    fn dump_with_peer_filter() {
        let dir = tempdir().unwrap();
        let handle = init(small_config(dir.path().join("cap.ring"))).unwrap();
        let tap = handle.tap();
        tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"keep");
        tap.capture_inbound("10.0.0.2:9030".parse().unwrap(), b"drop");

        let filter = DumpFilter {
            peer: Some(IpAddr::from_str("10.0.0.1").unwrap()),
            ..Default::default()
        };
        let bytes = handle.dump(&filter);
        let body = &bytes[PCAP_FILE_HEADER_LEN..];
        assert!(body.windows(4).any(|w| w == b"keep"));
        assert!(!body.windows(4).any(|w| w == b"drop"));
    }

    #[test]
    fn reset_bumps_generation() {
        let dir = tempdir().unwrap();
        let handle = init(small_config(dir.path().join("cap.ring"))).unwrap();
        let tap = handle.tap();
        tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"x");
        let gen_before = handle.info().generation;
        handle.reset();
        let info_after = handle.info();
        assert_eq!(info_after.generation, gen_before + 1);
        // No records remain
        assert!(info_after.oldest_ts.is_none());
    }

    #[test]
    fn capture_access_works_through_trait_object() {
        let dir = tempdir().unwrap();
        let handle = init(small_config(dir.path().join("cap.ring"))).unwrap();
        let tap = handle.tap();
        let access: Arc<dyn CaptureAccess> = Arc::new(handle);

        tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"trait-test");
        let info = access.info();
        assert_eq!(info.generation, 0);
        let bytes = access.dump(&DumpFilter::default());
        assert!(bytes.windows(10).any(|w| w == b"trait-test"));
    }

    #[test]
    fn iso8601_format_basic() {
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let s = format_ts(ts);
        // 1700000000 → 2023-11-14T22:13:20.000000Z
        assert_eq!(s, "2023-11-14T22:13:20.000000Z");
    }

    #[test]
    fn iso8601_format_leap_year() {
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_582_934_400);
        // 1582934400 = 2020-02-29 00:00:00 UTC
        let s = format_ts(ts);
        assert_eq!(s, "2020-02-29T00:00:00.000000Z");
    }
}

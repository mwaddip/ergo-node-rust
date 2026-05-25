//! End-to-end capture test: config resolve → init → tap → dump → parse.
//!
//! Goes through the same public surface that the main crate and api/
//! crate will use, so any breakage here points at the integration
//! contract rather than internal module wiring.

use enr_p2p::capture::{
    init, CaptureAccess, CaptureConfig, CaptureConfigError, Direction, DumpFilter,
};
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

const PCAP_MAGIC: u32 = 0xA1B2C3D4;
const LINKTYPE_USER0: u32 = 147;

fn resolved_config(path: PathBuf, size_mb: u64) -> enr_p2p::capture::ResolvedCaptureConfig {
    CaptureConfig {
        enabled: true,
        path: Some(path),
        size_mb,
        sync_interval_secs: 0,
        include_ips: vec![],
        exclude_ips: vec![],
    }
    .resolve()
    .unwrap()
    .unwrap()
}

#[test]
fn e2e_capture_dump_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.ring");

    let handle = init(resolved_config(path, 8)).unwrap();
    let tap = handle.tap();

    let peer1 = "10.0.0.1:9030".parse().unwrap();
    let peer2 = "[::1]:9030".parse().unwrap();
    let frame_a = b"frame a content";
    let frame_b = b"frame b content";

    tap.capture_inbound(peer1, frame_a);
    tap.capture_outbound(peer2, frame_b);

    let pcap_bytes = handle.dump(&DumpFilter::default());

    // Header sanity
    let magic = u32::from_le_bytes(pcap_bytes[0..4].try_into().unwrap());
    assert_eq!(magic, PCAP_MAGIC);
    let linktype = u32::from_le_bytes(pcap_bytes[20..24].try_into().unwrap());
    assert_eq!(linktype, LINKTYPE_USER0);

    // Walk records, count them
    let mut pos = 24;
    let mut records = Vec::new();
    while pos < pcap_bytes.len() {
        if pos + 16 > pcap_bytes.len() {
            break;
        }
        let incl_len =
            u32::from_le_bytes(pcap_bytes[pos + 8..pos + 12].try_into().unwrap()) as usize;
        if pos + 16 + incl_len > pcap_bytes.len() {
            break;
        }
        records.push((pos, incl_len));
        pos += 16 + incl_len;
    }
    assert_eq!(records.len(), 2);

    // Verify first record contains frame_a (inbound from peer1)
    let (off, _) = records[0];
    let direction = pcap_bytes[off + 16];
    assert_eq!(direction, 0); // inbound
    let frame_off = off + 16 + 24;
    assert_eq!(&pcap_bytes[frame_off..frame_off + frame_a.len()], frame_a);

    // Verify second record contains frame_b (outbound)
    let (off, _) = records[1];
    let direction = pcap_bytes[off + 16];
    assert_eq!(direction, 1); // outbound
    let frame_off = off + 16 + 24;
    assert_eq!(&pcap_bytes[frame_off..frame_off + frame_b.len()], frame_b);
}

#[test]
fn e2e_wrap_evicts_oldest_keeps_newest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.ring");

    // 1 MB ring; fill with ~12 KB frames until we wrap, then a couple more.
    let handle = init(resolved_config(path, 1)).unwrap();
    let tap = handle.tap();

    let peer = "10.0.0.1:9030".parse().unwrap();
    let big_frame = vec![0u8; 12 * 1024];

    // Append until we wrap (generation increments)
    let gen_before = handle.info().generation;
    let mut counter = 0;
    while handle.info().generation == gen_before {
        tap.capture_inbound(peer, &big_frame);
        counter += 1;
        if counter > 200 {
            panic!("ring failed to wrap after {} frames", counter);
        }
    }

    // Append a marker frame post-wrap
    let marker = b"POST-WRAP-MARKER";
    tap.capture_outbound(peer, marker);

    // Dump and verify marker is present
    let pcap_bytes = handle.dump(&DumpFilter::default());
    assert!(
        pcap_bytes.windows(marker.len()).any(|w| w == marker),
        "marker frame missing after wrap"
    );

    // info should reflect generation >= 1
    assert!(handle.info().generation >= 1);
}

#[test]
fn e2e_filter_drops_excluded_peer_at_capture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.ring");

    let cfg = CaptureConfig {
        enabled: true,
        path: Some(path),
        size_mb: 1,
        sync_interval_secs: 0,
        include_ips: vec![],
        exclude_ips: vec![IpAddr::from_str("192.168.1.100").unwrap()],
    }
    .resolve()
    .unwrap()
    .unwrap();

    let handle = init(cfg).unwrap();
    let tap = handle.tap();

    tap.capture_inbound("192.168.1.100:9030".parse().unwrap(), b"DROPPED");
    tap.capture_inbound("8.8.8.8:9030".parse().unwrap(), b"KEPT");

    let pcap_bytes = handle.dump(&DumpFilter::default());
    assert!(pcap_bytes.windows(4).any(|w| w == b"KEPT"));
    assert!(!pcap_bytes.windows(7).any(|w| w == b"DROPPED"));
}

#[test]
fn e2e_include_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.ring");

    let cfg = CaptureConfig {
        enabled: true,
        path: Some(path),
        size_mb: 1,
        sync_interval_secs: 0,
        include_ips: vec![IpAddr::from_str("10.0.0.1").unwrap()],
        exclude_ips: vec![],
    }
    .resolve()
    .unwrap()
    .unwrap();

    let handle = init(cfg).unwrap();
    let tap = handle.tap();

    tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"INCLUDED");
    tap.capture_inbound("10.0.0.2:9030".parse().unwrap(), b"EXCLUDED");

    let pcap_bytes = handle.dump(&DumpFilter::default());
    assert!(pcap_bytes.windows(8).any(|w| w == b"INCLUDED"));
    assert!(!pcap_bytes.windows(8).any(|w| w == b"EXCLUDED"));
}

#[test]
fn e2e_mutually_exclusive_filters_rejected_at_resolve() {
    let cfg = CaptureConfig {
        enabled: true,
        path: Some(PathBuf::from("/tmp/never-created.ring")),
        size_mb: 1024,
        sync_interval_secs: 0,
        include_ips: vec![IpAddr::from_str("1.2.3.4").unwrap()],
        exclude_ips: vec![IpAddr::from_str("5.6.7.8").unwrap()],
    };
    assert!(matches!(
        cfg.resolve(),
        Err(CaptureConfigError::FiltersMutuallyExclusive)
    ));
}

#[test]
fn e2e_missing_path_rejected_at_resolve() {
    let cfg = CaptureConfig {
        enabled: true,
        path: None,
        size_mb: 1024,
        sync_interval_secs: 60,
        include_ips: vec![],
        exclude_ips: vec![],
    };
    assert!(matches!(
        cfg.resolve(),
        Err(CaptureConfigError::MissingPath)
    ));
}

#[test]
fn e2e_dump_filter_by_direction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.ring");
    let handle = init(resolved_config(path, 1)).unwrap();
    let tap = handle.tap();

    tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"IN-FRAME");
    tap.capture_outbound("10.0.0.1:9030".parse().unwrap(), b"OUT-FRAME");

    let filter = DumpFilter {
        direction: Some(Direction::Outbound),
        ..Default::default()
    };
    let pcap_bytes = handle.dump(&filter);
    assert!(pcap_bytes.windows(9).any(|w| w == b"OUT-FRAME"));
    assert!(!pcap_bytes.windows(8).any(|w| w == b"IN-FRAME"));
}

#[test]
fn e2e_reset_clears_visible_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.ring");
    let handle = init(resolved_config(path, 1)).unwrap();
    let tap = handle.tap();

    tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"PRE-RESET");
    let bytes_before = handle.dump(&DumpFilter::default());
    assert!(bytes_before.windows(9).any(|w| w == b"PRE-RESET"));

    handle.reset();

    let bytes_after = handle.dump(&DumpFilter::default());
    assert!(!bytes_after.windows(9).any(|w| w == b"PRE-RESET"));

    // After capture post-reset, only the new record is visible
    tap.capture_outbound("10.0.0.1:9030".parse().unwrap(), b"POST-RESET");
    let bytes_after2 = handle.dump(&DumpFilter::default());
    assert!(bytes_after2.windows(10).any(|w| w == b"POST-RESET"));
    assert!(!bytes_after2.windows(9).any(|w| w == b"PRE-RESET"));
}

#[test]
fn e2e_trait_object_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.ring");
    let handle = init(resolved_config(path, 1)).unwrap();
    let tap = handle.tap();
    let access: Arc<dyn CaptureAccess> = Arc::new(handle);

    tap.capture_inbound("10.0.0.1:9030".parse().unwrap(), b"TRAIT-OBJ");
    let pcap_bytes = access.dump(&DumpFilter::default());
    assert!(pcap_bytes.windows(9).any(|w| w == b"TRAIT-OBJ"));

    let info = access.info();
    assert!(info.enabled);
    assert!(info.oldest_ts.is_some());
}

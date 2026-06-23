# P2P Wire-Traffic Capture — `p2p/` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the file-backed P2P frame capture ring inside the `p2p/` crate per `facts/p2p-capture.md` v1.0.0. Hot-path tap, mmap-backed ring, pcap-compatible storage, IP-array filters, `CaptureAccess` trait for the api/ crate to consume.

**Architecture:** New `p2p/src/capture/` module. The ring file is mmap'd; writes go through an atomic `write_head` with a wrap-skip policy. The transport layer's frame-read and frame-write sites call into a `Tap` handle that filters by IP and appends a pcap-formatted record. A `CaptureHandle` exposed via `capture::init()` is the public surface; api/ consumes it via the `CaptureAccess` trait. No allocations on the hot path beyond `Bytes::clone` (refcount-only) and atomic head update.

**Tech Stack:** `memmap2` (mmap), `nix` (already in workspace; for `fallocate` via `posix_fallocate`), `bytes` (Bytes for refcount-only frame handoff — already in p2p), `serde`/`toml` (config types), `blake2`/`hex` already available.

**Boundary:** `p2p/` only. Reads outside (`../facts/p2p-capture.md`, `../facts/p2p-transport.md`) are fine; writes are not. Do NOT touch `api/`, `src/main.rs`, or any other crate. The `CaptureAccess` trait you define is the seam api/ will consume; api/ implementation lives in a separate plan (`2026-05-25-p2p-capture-api.md`) and will be dispatched after this one lands.

---

## File Structure

**New** (all in `p2p/src/capture/`):

- `mod.rs` — public re-exports: `CaptureConfig`, `CaptureHandle`, `CaptureAccess`, `init`
- `config.rs` — `CaptureConfig` struct, `[debug.p2p_capture]` deserialization, validation (required path, filter exclusivity, etc.)
- `pcap.rs` — pcap file header + per-record header + Ergo metadata block encoding
- `ring.rs` — `RingBuffer`: owns mmap'd file, atomic `write_head`, `append()`, wrap-skip with `0xFF` padding, trailer maintenance
- `tap.rs` — `Tap`: filter check + record-pack + ring append (hot path)
- `dump.rs` — chronological read iterator + dump-time filters (peer, since_secs, direction), pcap output streaming
- `handle.rs` — `CaptureHandle` (the Arc-wrapped facade) implementing `CaptureAccess`

**Modified**:

- `p2p/Cargo.toml` — add `memmap2 = "0.9"`, `nix = { version = "0.29", features = ["fs"] }` (if not already present with this feature)
- `p2p/src/lib.rs` — `pub mod capture;`
- `p2p/src/transport/connection.rs` (or wherever the per-peer read/write loops live — locate at implementation time) — install `Tap::capture_inbound` after frame read, `Tap::capture_outbound` before frame write

**Tests**:

- `p2p/src/capture/{config,pcap,ring,tap,dump}.rs` — unit tests inline
- `p2p/tests/capture_e2e.rs` — integration tests (enable, generate frames, dump, parse with `etherparse` or similar)

---

## Task 1: Config types + required-path validation

**Files:**
- Create: `p2p/src/capture/mod.rs` (skeleton: `pub mod config;` for now)
- Create: `p2p/src/capture/config.rs`
- Modify: `p2p/src/lib.rs` (`pub mod capture;`)
- Modify: `p2p/Cargo.toml` (add `serde` derive if not present in p2p — should be already)

- [ ] **Step 1: Write the failing tests**

Create `p2p/src/capture/config.rs`:

```rust
use serde::Deserialize;
use std::net::IpAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    pub enabled: bool,
    pub path: Option<PathBuf>,
    #[serde(default = "default_size_mb")]
    pub size_mb: u64,
    #[serde(default = "default_sync_interval")]
    pub sync_interval_secs: u64,
    #[serde(default)]
    pub include_ips: Vec<IpAddr>,
    #[serde(default)]
    pub exclude_ips: Vec<IpAddr>,
}

fn default_size_mb() -> u64 { 1024 }
fn default_sync_interval() -> u64 { 60 }

/// Resolved + validated config. Hot-path-friendly types.
#[derive(Debug, Clone)]
pub struct ResolvedCaptureConfig {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sync_interval_secs: u64,
    pub filter: FilterMode,
}

#[derive(Debug, Clone)]
pub enum FilterMode {
    None,
    Include(std::collections::HashSet<IpAddr>),
    Exclude(std::collections::HashSet<IpAddr>),
}

impl CaptureConfig {
    /// Validate and convert to ResolvedCaptureConfig.
    /// Returns Ok(None) if `enabled = false`.
    pub fn resolve(self) -> Result<Option<ResolvedCaptureConfig>, CaptureConfigError> {
        todo!()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureConfigError {
    #[error("p2p_capture.path is required when p2p_capture.enabled = true")]
    MissingPath,
    #[error("p2p_capture.path parent directory does not exist: {0}")]
    ParentDirMissing(PathBuf),
    #[error("p2p_capture.include_ips and exclude_ips are mutually exclusive; set one or the other")]
    FiltersMutuallyExclusive,
    #[error("p2p_capture.size_mb must be >= 1 (got {0})")]
    SizeTooSmall(u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn disabled_returns_none() {
        let c = CaptureConfig {
            enabled: false,
            path: None,
            size_mb: 0,
            sync_interval_secs: 0,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        assert!(c.resolve().unwrap().is_none());
    }

    #[test]
    fn enabled_without_path_errors() {
        let c = CaptureConfig {
            enabled: true,
            path: None,
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        assert!(matches!(c.resolve(), Err(CaptureConfigError::MissingPath)));
    }

    #[test]
    fn missing_parent_dir_errors() {
        let c = CaptureConfig {
            enabled: true,
            path: Some(PathBuf::from("/nonexistent_dir_xyz/cap.ring")),
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        assert!(matches!(c.resolve(), Err(CaptureConfigError::ParentDirMissing(_))));
    }

    #[test]
    fn mutually_exclusive_filters_errors() {
        let dir = tempfile::tempdir().unwrap();
        let c = CaptureConfig {
            enabled: true,
            path: Some(dir.path().join("cap.ring")),
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![IpAddr::from_str("1.2.3.4").unwrap()],
            exclude_ips: vec![IpAddr::from_str("5.6.7.8").unwrap()],
        };
        assert!(matches!(c.resolve(), Err(CaptureConfigError::FiltersMutuallyExclusive)));
    }

    #[test]
    fn enabled_with_valid_path_resolves_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cap.ring");
        let c = CaptureConfig {
            enabled: true,
            path: Some(path.clone()),
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        let r = c.resolve().unwrap().unwrap();
        assert_eq!(r.path, path);
        assert_eq!(r.size_bytes, 1024 * 1024 * 1024);
        assert!(matches!(r.filter, FilterMode::None));
    }

    #[test]
    fn include_only_resolves_to_include_filter() {
        let dir = tempfile::tempdir().unwrap();
        let c = CaptureConfig {
            enabled: true,
            path: Some(dir.path().join("cap.ring")),
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![IpAddr::from_str("1.2.3.4").unwrap()],
            exclude_ips: vec![],
        };
        let r = c.resolve().unwrap().unwrap();
        assert!(matches!(r.filter, FilterMode::Include(_)));
    }
}
```

Add `thiserror`, `tempfile` to `p2p/Cargo.toml` if not present (likely already there for dev-dependencies).

- [ ] **Step 2: Run tests, expect fail**

```
cargo test -p ergo-p2p capture::config::tests
```

Expected: 6 tests FAIL (`not yet implemented`).

- [ ] **Step 3: Implement `resolve()`**

```rust
impl CaptureConfig {
    pub fn resolve(self) -> Result<Option<ResolvedCaptureConfig>, CaptureConfigError> {
        if !self.enabled {
            return Ok(None);
        }
        let path = self.path.ok_or(CaptureConfigError::MissingPath)?;
        let parent = path.parent().ok_or_else(|| {
            CaptureConfigError::ParentDirMissing(path.clone())
        })?;
        if !parent.exists() {
            return Err(CaptureConfigError::ParentDirMissing(parent.to_path_buf()));
        }
        if !self.include_ips.is_empty() && !self.exclude_ips.is_empty() {
            return Err(CaptureConfigError::FiltersMutuallyExclusive);
        }
        if self.size_mb == 0 {
            return Err(CaptureConfigError::SizeTooSmall(0));
        }
        let filter = if !self.include_ips.is_empty() {
            FilterMode::Include(self.include_ips.into_iter().collect())
        } else if !self.exclude_ips.is_empty() {
            FilterMode::Exclude(self.exclude_ips.into_iter().collect())
        } else {
            FilterMode::None
        };
        Ok(Some(ResolvedCaptureConfig {
            path,
            size_bytes: self.size_mb * 1024 * 1024,
            sync_interval_secs: self.sync_interval_secs,
            filter,
        }))
    }
}
```

- [ ] **Step 4: Run tests, expect pass**

```
cargo test -p ergo-p2p capture::config::tests
```

Expected: 6/6 PASS.

- [ ] **Step 5: Commit**

```
git add p2p/Cargo.toml p2p/src/lib.rs p2p/src/capture/{mod,config}.rs
git commit -m "feat(p2p): capture config types + validation"
```

---

## Task 2: pcap encoding

**Files:**
- Create: `p2p/src/capture/pcap.rs`
- Modify: `p2p/src/capture/mod.rs` (`pub mod pcap;`)

- [ ] **Step 1: Write failing tests**

```rust
use std::net::IpAddr;
use std::time::SystemTime;

pub const PCAP_FILE_HEADER_LEN: usize = 24;
pub const PCAP_RECORD_HEADER_LEN: usize = 16;
pub const ERGO_METADATA_LEN: usize = 24;

pub const PCAP_MAGIC: u32 = 0xA1B2C3D4;
pub const LINKTYPE_USER0: u32 = 147;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Inbound = 0,
    Outbound = 1,
}

/// Write the global pcap file header to `out`. Out must have at least
/// PCAP_FILE_HEADER_LEN bytes of capacity.
pub fn write_file_header(out: &mut [u8], snaplen: u32) {
    todo!()
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
    todo!()
}

/// Total record size given frame length.
pub fn record_size(frame_len: usize) -> usize {
    PCAP_RECORD_HEADER_LEN + ERGO_METADATA_LEN + frame_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn file_header_magic_and_linktype() {
        let mut buf = [0u8; PCAP_FILE_HEADER_LEN];
        write_file_header(&mut buf, 2 * 1024 * 1024);
        // magic at offset 0, little-endian
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(magic, PCAP_MAGIC);
        // network at offset 20
        let network = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        assert_eq!(network, LINKTYPE_USER0);
        // snaplen at offset 16
        let snaplen = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        assert_eq!(snaplen, 2 * 1024 * 1024);
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

        // Ergo metadata
        assert_eq!(buf[16], 0); // direction = inbound
        let port = u16::from_be_bytes(buf[18..20].try_into().unwrap());
        assert_eq!(port, 9030);
        // IPv4 mapped to ::ffff:10.0.0.1 → last 6 bytes = ffff + 10.0.0.1
        assert_eq!(&buf[20 + 10..20 + 12], &[0xff, 0xff]);
        assert_eq!(&buf[20 + 12..20 + 16], &[10, 0, 0, 1]);

        // Frame bytes follow
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
    }
}
```

- [ ] **Step 2: Run tests, expect fail**

- [ ] **Step 3: Implement encoders**

```rust
pub fn write_file_header(out: &mut [u8], snaplen: u32) {
    assert!(out.len() >= PCAP_FILE_HEADER_LEN);
    out[0..4].copy_from_slice(&PCAP_MAGIC.to_le_bytes());
    out[4..6].copy_from_slice(&2u16.to_le_bytes());   // version_major
    out[6..8].copy_from_slice(&4u16.to_le_bytes());   // version_minor
    out[8..12].copy_from_slice(&0u32.to_le_bytes());  // thiszone
    out[12..16].copy_from_slice(&0u32.to_le_bytes()); // sigfigs
    out[16..20].copy_from_slice(&snaplen.to_le_bytes());
    out[20..24].copy_from_slice(&LINKTYPE_USER0.to_le_bytes());
}

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

    let dur = ts.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let ts_sec = dur.as_secs() as u32;
    let ts_usec = dur.subsec_micros();
    let incl_len = (ERGO_METADATA_LEN + frame_bytes.len()) as u32;

    // pcap record header
    out[0..4].copy_from_slice(&ts_sec.to_le_bytes());
    out[4..8].copy_from_slice(&ts_usec.to_le_bytes());
    out[8..12].copy_from_slice(&incl_len.to_le_bytes());
    out[12..16].copy_from_slice(&incl_len.to_le_bytes()); // orig_len same

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

    // Frame
    out[40..40 + frame_bytes.len()].copy_from_slice(frame_bytes);

    total
}
```

- [ ] **Step 4: Run, expect pass**

- [ ] **Step 5: Commit**

```
git commit -m "feat(p2p): capture pcap encoding (file header + record format)"
```

---

## Task 3: Ring buffer mechanics

**Files:**
- Create: `p2p/src/capture/ring.rs`
- Modify: `p2p/Cargo.toml` (add `memmap2 = "0.9"`)
- Modify: `p2p/src/capture/mod.rs` (`pub mod ring;`)

- [ ] **Step 1: Outline the type**

```rust
use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const TRAILER_LEN: usize = 64;
pub const TRAILER_MAGIC: &[u8; 8] = b"ENRPCAPR";
pub const PAD_BYTE: u8 = 0xFF;

pub struct RingBuffer {
    mmap: MmapMut,
    size: u64,
    data_region_start: u64,   // = PCAP_FILE_HEADER_LEN (24)
    data_region_end: u64,     // = size - TRAILER_LEN
    write_head: AtomicU64,    // offset within data region (data_region_start..data_region_end)
    generation: AtomicU64,
}
```

- [ ] **Step 2: Write failing tests for the round-trip**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn new_ring(size: u64) -> (tempfile::TempDir, RingBuffer) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cap.ring");
        let ring = RingBuffer::open_or_create(&path, size).unwrap();
        (dir, ring)
    }

    #[test]
    fn create_initializes_pcap_header_and_trailer() {
        let (_d, ring) = new_ring(1024 * 1024);
        let bytes = ring.snapshot();
        // pcap magic at offset 0
        assert_eq!(&bytes[0..4], &0xA1B2C3D4u32.to_le_bytes());
        // trailer magic at end
        let tend = bytes.len();
        assert_eq!(&bytes[tend - TRAILER_LEN..tend - TRAILER_LEN + 8], TRAILER_MAGIC);
    }

    #[test]
    fn append_writes_at_current_head_and_advances() {
        let (_d, ring) = new_ring(1024 * 1024);
        let head_before = ring.write_head.load(Ordering::Acquire);
        ring.append(b"hello world").unwrap();
        let head_after = ring.write_head.load(Ordering::Acquire);
        assert_eq!(head_after - head_before, 11);
    }

    #[test]
    fn wrap_at_boundary_pads_and_restarts_at_data_region_start() {
        let size = 4096;
        let (_d, ring) = new_ring(size);
        let cap = ring.data_region_end - ring.data_region_start;
        // Append until we're close to the end
        let chunk = vec![0xAA; 1000];
        while ring.write_head.load(Ordering::Acquire) + chunk.len() as u64 <= ring.data_region_end {
            ring.append(&chunk).unwrap();
        }
        // Now append something that would overflow — should wrap
        let pre_wrap_head = ring.write_head.load(Ordering::Acquire);
        let pre_gen = ring.generation.load(Ordering::Acquire);
        let big = vec![0xBB; 500];
        ring.append(&big).unwrap();
        let post_wrap_head = ring.write_head.load(Ordering::Acquire);
        let post_gen = ring.generation.load(Ordering::Acquire);
        // Wrapped: head reset near start
        assert!(post_wrap_head < pre_wrap_head);
        assert_eq!(post_gen, pre_gen + 1);
        // Padding between pre_wrap_head and data_region_end is 0xFF
        let bytes = ring.snapshot();
        for &b in &bytes[pre_wrap_head as usize..ring.data_region_end as usize] {
            assert_eq!(b, PAD_BYTE);
        }
    }

    #[test]
    fn reset_clears_head_and_bumps_generation() {
        let (_d, ring) = new_ring(1024 * 1024);
        ring.append(b"data").unwrap();
        let gen_before = ring.generation.load(Ordering::Acquire);
        ring.reset();
        assert_eq!(
            ring.write_head.load(Ordering::Acquire),
            ring.data_region_start
        );
        assert_eq!(ring.generation.load(Ordering::Acquire), gen_before + 1);
    }
}
```

- [ ] **Step 3: Implement `open_or_create`, `append`, `reset`, `snapshot`**

Skeleton (fill in error handling + boundary conditions):

```rust
impl RingBuffer {
    pub fn open_or_create(path: &Path, size: u64) -> Result<Self, std::io::Error> {
        let file = OpenOptions::new()
            .read(true).write(true).create(true)
            .open(path)?;
        // Pre-allocate
        file.set_len(size)?;
        nix::fcntl::fallocate(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            nix::fcntl::FallocateFlags::empty(),
            0,
            size as i64,
        ).ok();  // best-effort; some filesystems don't support fallocate
        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        // Initialize pcap header if file looks fresh (magic byte at offset 0 is 0)
        if mmap[0..4] == [0u8; 4] {
            crate::capture::pcap::write_file_header(&mut mmap, 2 * 1024 * 1024);
        }

        // Initialize trailer if fresh
        let trailer_offset = (size - TRAILER_LEN as u64) as usize;
        let trailer = &mut mmap[trailer_offset..trailer_offset + TRAILER_LEN];
        let (write_head_init, gen_init) = if &trailer[0..8] != TRAILER_MAGIC {
            trailer[0..8].copy_from_slice(TRAILER_MAGIC);
            trailer[8..12].copy_from_slice(&1u32.to_le_bytes()); // version
            let head = 24u64; // = data_region_start
            trailer[16..24].copy_from_slice(&head.to_le_bytes());
            trailer[24..32].copy_from_slice(&0u64.to_le_bytes());
            (head, 0u64)
        } else {
            let h = u64::from_le_bytes(trailer[16..24].try_into().unwrap());
            let g = u64::from_le_bytes(trailer[24..32].try_into().unwrap());
            (h, g)
        };

        Ok(Self {
            mmap,
            size,
            data_region_start: 24,
            data_region_end: size - TRAILER_LEN as u64,
            write_head: AtomicU64::new(write_head_init),
            generation: AtomicU64::new(gen_init),
        })
    }

    pub fn append(&self, record: &[u8]) -> Result<(), std::io::Error> {
        let len = record.len() as u64;
        // ... atomic head update with wrap-skip and trailer write-back
        todo!()
    }

    pub fn reset(&self) {
        // ... CAS-update head to data_region_start, increment generation,
        // sync trailer
        todo!()
    }

    pub fn snapshot(&self) -> &[u8] {
        &self.mmap
    }
}
```

Implement `append` carefully — atomic `fetch_add` is the fast path, but the overflow case needs a CAS to reset head + bump generation. Write the trailer fields after each commit (cheap; 16 bytes).

- [ ] **Step 4: Run tests, expect pass**

```
cargo test -p ergo-p2p capture::ring::tests
```

- [ ] **Step 5: Commit**

```
git commit -m "feat(p2p): capture ring buffer (mmap + atomic write_head + wrap)"
```

---

## Task 4: Tap module (hot path)

**Files:**
- Create: `p2p/src/capture/tap.rs`

- [ ] **Step 1: Tap type + filter check**

```rust
use super::config::{FilterMode, ResolvedCaptureConfig};
use super::pcap::{record_size, write_record, Direction};
use super::ring::RingBuffer;
use std::net::SocketAddr;
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

    /// Hot path: called from transport after reading a frame.
    pub fn capture_inbound(&self, peer: SocketAddr, frame: &[u8]) {
        if !self.passes_filter(peer.ip()) { return; }
        self.write(SystemTime::now(), Direction::Inbound, peer, frame);
    }

    /// Hot path: called from transport before writing a frame.
    pub fn capture_outbound(&self, peer: SocketAddr, frame: &[u8]) {
        if !self.passes_filter(peer.ip()) { return; }
        self.write(SystemTime::now(), Direction::Outbound, peer, frame);
    }

    fn passes_filter(&self, ip: std::net::IpAddr) -> bool {
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
        let _ = self.ring.append(&buf);
    }
}
```

The hot path's allocation is the `vec![0u8; total]` for the record buffer. An optimization (not in v1) would be to provide a `write_into_slice` API on the ring that returns a writable slot; the tap would write directly into the mmap region. Defer that.

- [ ] **Step 2: Write tests**

Filter-pass cases (4): no filter, include hit, include miss, exclude hit, exclude miss. Round-trip case: capture a frame, snapshot the ring, verify the bytes are at the expected offset.

- [ ] **Step 3: Run + pass**

- [ ] **Step 4: Commit**

```
git commit -m "feat(p2p): capture Tap (filter + record pack + ring append)"
```

---

## Task 5: Install tap at transport

**Files:**
- Modify: `p2p/src/transport/connection.rs` (or wherever per-peer read/write loops live; locate via `grep -rn 'read_frame\|write_frame\|process_frame'` in `p2p/src/transport/`)
- Possibly modify: `p2p/src/transport/frame.rs` if framing primitives live there

- [ ] **Step 1: Locate the frame-read and frame-write sites**

Run:
```
grep -rn "read_frame\|read_exact\|process_frame\|recv_frame" p2p/src/transport/
grep -rn "write_frame\|write_all\|send_frame" p2p/src/transport/
```

Identify the points immediately AFTER a successful frame read (with the full frame bytes in scope) and immediately BEFORE a frame write (with the full frame bytes in scope including framing header).

- [ ] **Step 2: Thread an `Option<Arc<Tap>>` into the Transport / Connection struct**

The transport's constructor takes a new parameter:

```rust
pub fn new(/* ...existing... */, tap: Option<Arc<crate::capture::tap::Tap>>) -> Self { ... }
```

Store it. At read-frame site:

```rust
if let Some(tap) = &self.tap {
    tap.capture_inbound(peer_addr, &raw_frame_bytes);
}
```

At write-frame site:

```rust
if let Some(tap) = &self.tap {
    tap.capture_outbound(peer_addr, &raw_frame_bytes);
}
```

The `raw_frame_bytes` MUST be the complete frame including framing header (magic + code + length + checksum + body) per the contract.

- [ ] **Step 3: Update upstream callers**

`p2p::Node::new` and friends propagate the `tap` parameter (initially `None` — actual instantiation happens in the main crate after this lands and the api crate lands).

- [ ] **Step 4: Existing tests still pass**

```
cargo test -p ergo-p2p
```

Tap is `None` in tests; behavior unchanged.

- [ ] **Step 5: Commit**

```
git commit -m "feat(p2p): install capture tap at transport frame I/O sites"
```

---

## Task 6: Ring read iterator + dump-time filters

**Files:**
- Create: `p2p/src/capture/dump.rs`

- [ ] **Step 1: Write failing tests for chronological iteration**

```rust
use super::pcap::{Direction, record_size};
use super::ring::RingBuffer;
use std::net::IpAddr;
use std::time::SystemTime;

pub struct DumpRecord<'a> {
    pub ts: SystemTime,
    pub direction: Direction,
    pub peer_ip: IpAddr,
    pub peer_port: u16,
    pub frame: &'a [u8],
    pub raw_record: &'a [u8],  // pcap rec header + Ergo metadata + frame, contiguous
}

pub struct DumpFilter {
    pub peer: Option<IpAddr>,
    pub since_secs: Option<u64>,   // window ending at now
    pub direction: Option<Direction>,
}

pub fn iter_chronological<'a>(
    ring: &'a RingBuffer,
    now: SystemTime,
    filter: &'a DumpFilter,
) -> impl Iterator<Item = DumpRecord<'a>> + 'a {
    todo!()
}

#[cfg(test)]
mod tests {
    // Populate a ring with 3 known frames at known timestamps, iterate,
    // assert order + filter behavior. Also include a wrap scenario.
}
```

- [ ] **Step 2: Implement the walk + skip-padding logic**

Read the trailer's `write_head` + `generation` once into local state, then walk the data region. Start at `write_head` (oldest = just after where the next write will go, wrapping). End when you reach `write_head` again. Skip stretches of `0xFF` padding until you hit valid pcap-record bytes.

Apply filter inline: skip records whose metadata fails the filter.

- [ ] **Step 3: Test + commit**

```
git commit -m "feat(p2p): capture chronological dump iterator + filters"
```

---

## Task 7: `CaptureAccess` trait + `CaptureHandle` public surface

**Files:**
- Create: `p2p/src/capture/handle.rs`
- Modify: `p2p/src/capture/mod.rs` (re-exports)

- [ ] **Step 1: Trait definition**

```rust
use super::config::ResolvedCaptureConfig;
use super::dump::{DumpFilter, DumpRecord};
use super::ring::RingBuffer;
use super::tap::Tap;
use serde::Serialize;
use std::sync::Arc;

/// Public API surface that api/ consumes. Mockable for tests.
pub trait CaptureAccess: Send + Sync {
    fn info(&self) -> CaptureInfo;
    /// Returns a Vec<u8> containing the full pcap file (header + all
    /// records). For very large rings, prefer dump_filtered.
    /// (v1 implementation: synchronous; v2 may add async streaming.)
    fn dump(&self, filter: &DumpFilter) -> Vec<u8>;
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
    pub filter_mode: &'static str,   // "none" | "include" | "exclude"
    pub filter_count: usize,
}

pub struct CaptureHandle {
    ring: Arc<RingBuffer>,
    tap: Arc<Tap>,
    config: ResolvedCaptureConfig,
}

impl CaptureHandle {
    pub fn tap(&self) -> Arc<Tap> { self.tap.clone() }
}

impl CaptureAccess for CaptureHandle {
    // ... implement against the ring + dump iterator
}

/// Initialize the capture subsystem. Caller owns the returned handle;
/// pass `handle.tap()` to the p2p transport and pass the handle itself
/// to the api crate's state.
pub fn init(config: ResolvedCaptureConfig) -> Result<CaptureHandle, std::io::Error> {
    let ring = Arc::new(RingBuffer::open_or_create(&config.path, config.size_bytes)?);
    let tap = Arc::new(Tap::new(ring.clone(), config.filter.clone()));
    Ok(CaptureHandle { ring, tap, config })
}
```

- [ ] **Step 2: Test the trait against `CaptureHandle`**

E2E shape: init handle, capture some frames via tap, call info() / dump() / reset(), assert behavior.

- [ ] **Step 3: Commit**

```
git commit -m "feat(p2p): CaptureAccess trait + CaptureHandle public surface"
```

---

## Task 8: E2E tests + clippy + fmt

**Files:**
- Create: `p2p/tests/capture_e2e.rs`

- [ ] **Step 1: Write the end-to-end test**

```rust
use ergo_p2p::capture;
// ...

#[test]
fn e2e_capture_dump_parse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.ring");
    let config = capture::CaptureConfig {
        enabled: true,
        path: Some(path.clone()),
        size_mb: 8,
        sync_interval_secs: 60,
        include_ips: vec![],
        exclude_ips: vec![],
    }.resolve().unwrap().unwrap();

    let handle = capture::init(config).unwrap();
    let tap = handle.tap();

    // Synthesize a few frames at different "peers"
    let peer1 = "10.0.0.1:9030".parse().unwrap();
    let peer2 = "[::1]:9030".parse().unwrap();
    let frame_a = b"frame a content";
    let frame_b = b"frame b content";

    tap.capture_inbound(peer1, frame_a);
    tap.capture_outbound(peer2, frame_b);

    // Dump and verify
    let pcap_bytes = handle.dump(&capture::DumpFilter::default());
    // Parse the pcap, assert we see 2 records, in chronological order
    // ... use a pcap-parsing crate (etherparse, pcap-parser, or hand-roll)
}
```

Wrap-boundary test: fill ring near capacity, append one more, verify the older records get evicted and the new one is captured.

Filter tests:
- Include filter: capture frames from `peer1`, not `peer2`
- Exclude filter: capture all except `peer1`
- Mutually-exclusive filter triggers config resolve error

Missing-path startup error: `enabled=true` + `path=None` returns the expected error variant.

- [ ] **Step 2: Run all p2p tests**

```
cargo test -p ergo-p2p
cargo clippy -p ergo-p2p --all-targets -- -D warnings
cargo fmt -p ergo-p2p
```

- [ ] **Step 3: Commit any final fixes**

```
git commit -m "test(p2p): capture E2E + final clippy"
```

---

## Coordination — what happens after this plan completes

The main session is responsible for:

1. Pulling the changes and committing them.
2. Updating workspace `Cargo.toml` if new deps need workspace-level declarations.
3. **Dispatching the api-side plan** at
   `docs/superpowers/plans/2026-05-25-p2p-capture-api.md` (separate per-crate session for `api/`). That plan adds the HTTP route handlers consuming the `CaptureAccess` trait defined in Task 7 above.
4. After both p2p and api plans complete, the main session wires:
   - Read `[debug.p2p_capture]` from `ergo.toml` (config plumbing in `src/main.rs` or `src/config.rs`).
   - Call `p2p::capture::init(config)` if `enabled = true`, otherwise pass `None` for the tap.
   - Pass `Some(handle.tap())` to the p2p transport constructor.
   - Pass `Some(Arc<dyn CaptureAccess>)` (cloned from the handle) to the api crate's `ApiState`.
   - Smoke test: start the node with a debug capture config, generate some P2P traffic, GET `/debug/p2p-capture/info`.

Do NOT do step 3 from this session — that's a separate per-crate dispatch.

## Coordination back-channel (kitty)

When done (or blocked), report back to the main session window via
`kitty @ send-text --match=id:9 '...summary...' && kitty @ send-text --match=id:9 $'\r'`.

**Capture the main session's window ID at dispatch time** — see
`feedback_kitty_id_dynamic` in project memory. Do NOT hardcode any
specific id value; the dispatcher will substitute the live id when
they send the prompt instruction.

Single send-text + single `$'\r'` for the report. Multi-chunk
reports have been observed to drop the second submit.

## Self-Review

**Spec coverage** (against `facts/p2p-capture.md` v1.0.0):
- Config + required-path validation → Task 1
- pcap file + record format → Task 2
- Ergo metadata block → Task 2
- Ring buffer mmap + atomic + wrap → Task 3
- Trailer maintenance → Task 3
- Hot-path tap + filter → Task 4
- Transport integration (frame I/O sites) → Task 5
- Chronological dump iterator → Task 6
- Dump-time filters (peer / since_secs / direction) → Task 6
- `CaptureAccess` trait → Task 7
- `CaptureHandle` + `init()` → Task 7
- E2E + wrap-boundary tests → Task 8
- Mutually-exclusive filter startup error → Task 1's tests + Task 8

**API endpoints (info/dump/reset)** — NOT in this plan; they live
in the api/ plan (separate dispatch). This plan ships the
`CaptureAccess` trait that api/ will consume.

**Auto-dump triggers (on disconnect / on shutdown)** — explicitly
v2 per the contract; this plan does NOT implement them.

**Placeholder scan**: No "TODO" / "implement later" in step content
where actual code is expected. Per-record encoding is fully spelled
out. Ring `append` and `reset` are skeleton-only in the plan — the
TDD test cases drive the implementation specifics.

**Type consistency**: `RingBuffer`, `Tap`, `CaptureHandle`,
`CaptureAccess`, `DumpFilter`, `Direction` referenced uniformly
across tasks 3-8.

## Open implementation questions

Surface these when you hit them; don't pre-decide all upfront:

1. **`append` allocation**: Task 4's `Tap::write` allocates `vec![0u8; total]` per frame. Hot-path acceptable for v1? Or should the ring expose `claim_slot(size) -> &mut [u8]` to write directly into mmap'd memory? Tradeoff: zero alloc vs. more complex API. Defer optimization to v1.1.
2. **`fallocate` portability**: `nix::fcntl::fallocate` is Linux. macOS dev environments may not support it; the `.ok()` swallows the error, but worth noting in a comment that the file is still pre-sized via `set_len`.
3. **mmap remap on resize**: out of scope; size_mb changes require operator to delete the file first and restart. Worth documenting in the contract's existing language ("capture data from a prior run is lost on size change").
4. **`SystemTime::now()` per record**: ~50-100 ns; fine for v1 throughput. If profiling later shows this dominating, batch timestamping or coarser clock can be added.

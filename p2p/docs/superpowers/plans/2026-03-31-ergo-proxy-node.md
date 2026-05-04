# Ergo Proxy Node Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust/tokio P2P relay that forwards Ergo protocol messages between inbound and outbound peers, with tracked Inv-based routing and configurable full-proxy or light mode per listener.

**Architecture:** Three layers — transport (framing, handshake), protocol (message parsing, peer lifecycle), routing (Inv table, request tracking, forwarding). Each layer boundary is a Design by Contract surface with preconditions, postconditions, and invariants enforced via debug assertions and documented in `contracts/`.

**Tech Stack:** Rust, tokio 1.x, blake2 0.10, serde/toml for config, tracing for logging.

**Spec:** `docs/superpowers/specs/2026-03-31-ergo-proxy-node-design.md`

---

## File Structure

```
ergo-proxy-node/
  Cargo.toml                    — crate manifest, dependencies
  ergo-proxy.toml               — example config file
  src/
    main.rs                     — tokio runtime, listener setup, layer wiring
    config.rs                   — TOML config parsing and validation
    types.rs                    — Shared types: PeerId, ModifierId, Version, Direction
    transport/
      mod.rs                    — re-exports
      vlq.rs                    — VLQ encode/decode (used by frame + handshake)
      frame.rs                  — Frame encode/decode, blake2b checksum
      handshake.rs              — Handshake build/parse, PeerSpec, Feature, Mode
      connection.rs             — Per-connection async read/write loops over TcpStream
    protocol/
      mod.rs                    — re-exports
      messages.rs               — Typed message structs, parse from Frame, serialize to Frame
      peer.rs                   — Peer state machine, lifecycle events
    routing/
      mod.rs                    — re-exports
      inv_table.rs              — Inv table: modifier → peer mapping with cleanup
      tracker.rs                — Request tracker + SyncInfo tracker
      router.rs                 — Message routing decisions, mode filtering, peer registry
  contracts/
    transport.md                — Transport layer contract spec
    protocol.md                 — Protocol layer contract spec
    routing.md                  — Routing layer contract spec
  tests/
    common/mod.rs               — Shared test helpers: mock streams, frame builders
    transport_test.rs           — Transport layer unit tests
    protocol_test.rs            — Protocol layer unit tests
    routing_test.rs             — Routing layer unit tests
    integration_test.rs         — End-to-end proxy test with mock peers
```

---

### Task 1: Project scaffold and shared types

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/types.rs`
- Create: `src/config.rs`
- Create: `ergo-proxy.toml`

- [ ] **Step 1: Create Cargo.toml**

```toml
[package]
name = "ergo-proxy-node"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["full"] }
blake2 = { version = "0.10", features = ["std"] }
serde = { version = "1", features = ["derive"] }
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[dev-dependencies]
tokio-test = "0.4"
```

- [ ] **Step 2: Create src/types.rs with shared types**

```rust
use std::fmt;
use std::net::SocketAddr;

/// Unique identifier for a connected peer. Wraps a u64 counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub u64);

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer-{}", self.0)
    }
}

/// 32-byte modifier identifier (block, transaction, header, etc.).
pub type ModifierId = [u8; 32];

/// Protocol version as three bytes: major.minor.patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

impl Version {
    pub const EIP37_MIN: Version = Version { major: 4, minor: 0, patch: 100 };

    pub const fn new(major: u8, minor: u8, patch: u8) -> Self {
        Self { major, minor, patch }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Direction of a peer connection relative to this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// We initiated the connection (we connect to them).
    Outbound,
    /// They initiated the connection (they connect to us).
    Inbound,
}

/// Network identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    pub const fn magic(&self) -> [u8; 4] {
        match self {
            Network::Mainnet => [1, 0, 2, 4],
            Network::Testnet => [2, 3, 2, 3],
        }
    }
}

/// Proxy mode for a listener — controls handshake advertising and routing behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Full proxy: forward all messages, advertise as full archival node.
    Full,
    /// Light: gossip only, advertise as NiPoPoW-bootstrapped.
    Light,
}
```

- [ ] **Step 3: Create src/config.rs**

```rust
use crate::types::{Network, ProxyMode};
use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub listen: ListenConfig,
    pub outbound: OutboundConfig,
    pub identity: IdentityConfig,
}

#[derive(Debug, Deserialize)]
pub struct ProxyConfig {
    pub network: Network,
}

#[derive(Debug, Deserialize)]
pub struct ListenConfig {
    pub ipv6: Option<ListenerConfig>,
    pub ipv4: Option<ListenerConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ListenerConfig {
    pub address: SocketAddr,
    pub mode: ProxyMode,
    pub max_inbound: usize,
}

#[derive(Debug, Deserialize)]
pub struct OutboundConfig {
    pub min_peers: usize,
    pub max_peers: usize,
    pub seed_peers: Vec<SocketAddr>,
}

#[derive(Debug, Deserialize)]
pub struct IdentityConfig {
    pub agent_name: String,
    pub peer_name: String,
    pub protocol_version: String,
}

impl Config {
    /// Load config from a TOML file.
    ///
    /// # Contract
    /// - **Precondition**: `path` points to a readable TOML file.
    /// - **Postcondition**: Returns a valid `Config` with at least one listener
    ///   and at least one seed peer, or an error.
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;

        // Validate: at least one listener
        if config.listen.ipv4.is_none() && config.listen.ipv6.is_none() {
            return Err("At least one listener (ipv4 or ipv6) must be configured".into());
        }

        // Validate: at least one seed peer
        if config.outbound.seed_peers.is_empty() {
            return Err("At least one seed peer must be configured".into());
        }

        // Validate: min_peers <= max_peers
        if config.outbound.min_peers > config.outbound.max_peers {
            return Err("min_peers must be <= max_peers".into());
        }

        Ok(config)
    }

    /// Parse the protocol version string into (major, minor, patch).
    ///
    /// # Contract
    /// - **Precondition**: `identity.protocol_version` is "X.Y.Z" format.
    /// - **Postcondition**: Returns three u8 values.
    pub fn version_bytes(&self) -> Result<(u8, u8, u8), Box<dyn std::error::Error>> {
        let parts: Vec<&str> = self.identity.protocol_version.split('.').collect();
        if parts.len() != 3 {
            return Err(format!("Invalid version: {}", self.identity.protocol_version).into());
        }
        Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?))
    }
}
```

- [ ] **Step 4: Create ergo-proxy.toml example config**

```toml
[proxy]
network = "testnet"

[listen.ipv6]
address = "[::]:9030"
mode = "full"
max_inbound = 20

[listen.ipv4]
address = "0.0.0.0:9030"
mode = "light"
max_inbound = 20

[outbound]
min_peers = 3
max_peers = 10
seed_peers = ["213.239.193.208:9023", "128.253.41.110:9020", "176.9.15.237:9021"]

[identity]
agent_name = "ergo-proxy"
peer_name = "ergo-proxy-node"
protocol_version = "6.0.3"
```

- [ ] **Step 5: Create minimal src/main.rs**

```rust
mod config;
mod types;

use config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ergo-proxy.toml".to_string());

    let config = Config::load(&config_path)?;
    tracing::info!(network = ?config.proxy.network, "Ergo proxy node starting");

    // Layers will be wired here in later tasks.

    Ok(())
}
```

- [ ] **Step 6: Verify it compiles**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo build 2>&1`
Expected: Compiles with no errors. Warnings about unused imports are acceptable at this stage.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock ergo-proxy.toml src/main.rs src/types.rs src/config.rs
git commit -m "feat: project scaffold with shared types and config"
```

---

### Task 2: Transport — VLQ encoding

**Files:**
- Create: `src/transport/mod.rs`
- Create: `src/transport/vlq.rs`
- Create: `tests/common/mod.rs`
- Create: `tests/transport_test.rs`

- [ ] **Step 1: Create src/transport/mod.rs**

```rust
pub mod vlq;
```

- [ ] **Step 2: Add transport module to main.rs**

Add `mod transport;` to the top of `src/main.rs`, after the existing module declarations.

- [ ] **Step 3: Write failing tests for VLQ**

Create `tests/common/mod.rs`:

```rust
// Shared test helpers — populated in later tasks.
```

Create `tests/transport_test.rs`:

```rust
mod common;

use ergo_proxy_node::transport::vlq;

#[test]
fn vlq_encode_zero() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 0);
    assert_eq!(buf, vec![0x00]);
}

#[test]
fn vlq_encode_single_byte() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 127);
    assert_eq!(buf, vec![0x7f]);
}

#[test]
fn vlq_encode_two_bytes() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 128);
    assert_eq!(buf, vec![0x80, 0x01]);
}

#[test]
fn vlq_encode_large_value() {
    // 300 = 0b100101100 → low 7: 0101100 = 0x2c | 0x80, high 7: 0000010 = 0x02
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 300);
    assert_eq!(buf, vec![0xac, 0x02]);
}

#[test]
fn vlq_roundtrip() {
    let values = [0u64, 1, 127, 128, 255, 256, 16383, 16384, 1_000_000, u64::MAX >> 1];
    for &val in &values {
        let mut buf = Vec::new();
        vlq::write_vlq(&mut buf, val);
        let mut cursor = std::io::Cursor::new(buf.as_slice());
        let decoded = vlq::read_vlq(&mut cursor).unwrap();
        assert_eq!(decoded, val, "roundtrip failed for {}", val);
    }
}

#[test]
fn vlq_read_overflow_returns_error() {
    // 10 bytes all with continuation bit — would overflow u64
    let data = vec![0x80; 10];
    let mut cursor = std::io::Cursor::new(data.as_slice());
    assert!(vlq::read_vlq(&mut cursor).is_err());
}

#[test]
fn vlq_length_rejects_oversized() {
    // Encode a value > 256KB
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 300_000);
    let mut cursor = std::io::Cursor::new(buf.as_slice());
    assert!(vlq::read_vlq_length(&mut cursor).is_err());
}

#[test]
fn vlq_length_accepts_valid() {
    let mut buf = Vec::new();
    vlq::write_vlq(&mut buf, 1024);
    let mut cursor = std::io::Cursor::new(buf.as_slice());
    assert_eq!(vlq::read_vlq_length(&mut cursor).unwrap(), 1024);
}

#[test]
fn zigzag_encode_positive() {
    assert_eq!(vlq::zigzag_encode(1), 2);
    assert_eq!(vlq::zigzag_encode(2), 4);
    assert_eq!(vlq::zigzag_encode(1440), 2880);
}

#[test]
fn zigzag_encode_negative() {
    assert_eq!(vlq::zigzag_encode(-1), 1);
    assert_eq!(vlq::zigzag_encode(-2), 3);
}

#[test]
fn zigzag_roundtrip() {
    for val in [-1000i32, -2, -1, 0, 1, 2, 1000, i32::MAX, i32::MIN] {
        let encoded = vlq::zigzag_encode(val);
        let decoded = vlq::zigzag_decode(encoded);
        assert_eq!(decoded, val, "zigzag roundtrip failed for {}", val);
    }
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test transport_test 2>&1`
Expected: Compilation error — `vlq` module not found or functions not defined.

- [ ] **Step 5: Implement src/transport/vlq.rs**

```rust
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

const MAX_VLQ_LENGTH: u64 = 256 * 1024;

/// Read a VLQ value representing a byte length, capped at 256KB.
///
/// # Postcondition
/// - Returns a `usize` in range `0..=262144`, or an error if the value exceeds the cap.
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
```

- [ ] **Step 6: Add lib.rs for test access**

Create `src/lib.rs`:

```rust
pub mod config;
pub mod types;
pub mod transport;
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test transport_test 2>&1`
Expected: All tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/transport/ src/lib.rs tests/
git commit -m "feat: VLQ encoding with zigzag support"
```

---

### Task 3: Transport — Frame encoding/decoding

**Files:**
- Create: `src/transport/frame.rs`
- Modify: `src/transport/mod.rs`
- Modify: `tests/transport_test.rs`

- [ ] **Step 1: Write failing tests for Frame**

Append to `tests/transport_test.rs`:

```rust
use ergo_proxy_node::transport::frame::{self, Frame};
use ergo_proxy_node::types::Network;

#[test]
fn frame_encode_decode_roundtrip() {
    let magic = Network::Testnet.magic();
    let original = Frame { code: 55, body: vec![2, 0, 1, 2, 3] };
    let bytes = frame::encode(&magic, &original);

    // Header: 4 magic + 1 code + 4 length + 4 checksum = 13
    assert_eq!(bytes.len(), 13 + original.body.len());

    let decoded = frame::decode(&magic, &bytes).unwrap();
    assert_eq!(decoded.code, original.code);
    assert_eq!(decoded.body, original.body);
}

#[test]
fn frame_decode_bad_magic_returns_error() {
    let magic = Network::Testnet.magic();
    let f = Frame { code: 1, body: vec![] };
    let mut bytes = frame::encode(&magic, &f);
    bytes[0] = 0xff; // corrupt magic
    assert!(frame::decode(&magic, &bytes).is_err());
}

#[test]
fn frame_decode_bad_checksum_returns_error() {
    let magic = Network::Testnet.magic();
    let f = Frame { code: 1, body: vec![1, 2, 3] };
    let mut bytes = frame::encode(&magic, &f);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff; // corrupt body → checksum mismatch
    assert!(frame::decode(&magic, &bytes).is_err());
}

#[test]
fn frame_decode_empty_body() {
    let magic = Network::Testnet.magic();
    let f = Frame { code: 1, body: vec![] };
    let bytes = frame::encode(&magic, &f);
    let decoded = frame::decode(&magic, &bytes).unwrap();
    assert_eq!(decoded.code, 1);
    assert!(decoded.body.is_empty());
}

#[test]
fn frame_encode_checksum_is_blake2b256_prefix() {
    use blake2::{Blake2b, Digest};
    use blake2::digest::consts::U32;
    type Blake2b256 = Blake2b<U32>;

    let magic = Network::Testnet.magic();
    let body = vec![0xde, 0xad, 0xbe, 0xef];
    let f = Frame { code: 55, body: body.clone() };
    let bytes = frame::encode(&magic, &f);

    let expected_checksum = &Blake2b256::digest(&body)[..4];
    assert_eq!(&bytes[9..13], expected_checksum);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test transport_test frame 2>&1`
Expected: Compilation error — `frame` module not found.

- [ ] **Step 3: Implement src/transport/frame.rs**

```rust
//! Ergo P2P message frame encoding and decoding.
//!
//! # Contract
//! - `encode`: given magic, code, and body, produces a valid frame.
//!   Postcondition: output is `[magic:4][code:1][len:4 BE][checksum:4][body:N]`.
//! - `decode`: given magic and raw bytes, validates and extracts a Frame.
//!   Precondition: `data.len() >= 13` (header size).
//!   Postcondition: magic matches, checksum valid, body length matches header.
//! - Invariant: `decode(magic, encode(magic, frame)) == Ok(frame)` for any valid frame.

use blake2::{Blake2b, Digest};
use blake2::digest::consts::U32;
use std::io;

type Blake2b256 = Blake2b<U32>;

const HEADER_SIZE: usize = 13; // 4 magic + 1 code + 4 length + 4 checksum
const MAX_BODY_SIZE: u32 = 256 * 1024;

/// A validated P2P message frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub code: u8,
    pub body: Vec<u8>,
}

/// Encode a Frame into wire bytes with the given network magic.
pub fn encode(magic: &[u8; 4], frame: &Frame) -> Vec<u8> {
    let mut msg = Vec::with_capacity(HEADER_SIZE + frame.body.len());
    msg.extend_from_slice(magic);
    msg.push(frame.code);
    msg.extend_from_slice(&(frame.body.len() as u32).to_be_bytes());
    let hash = Blake2b256::digest(&frame.body);
    msg.extend_from_slice(&hash[..4]);
    msg.extend_from_slice(&frame.body);
    msg
}

/// Decode a Frame from wire bytes, validating magic and checksum.
pub fn decode(magic: &[u8; 4], data: &[u8]) -> io::Result<Frame> {
    if data.len() < HEADER_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Frame too short: {} bytes (need at least {})", data.len(), HEADER_SIZE),
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
pub async fn read_frame(
    reader: &mut (impl tokio::io::AsyncReadExt + Unpin),
    magic: &[u8; 4],
) -> io::Result<Frame> {
    let mut header = [0u8; HEADER_SIZE];
    reader.read_exact(&mut header).await?;

    if &header[0..4] != magic {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Bad magic: {:?}", &header[0..4]),
        ));
    }

    let code = header[4];
    let body_len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]);

    if body_len > MAX_BODY_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Body too large: {} bytes", body_len),
        ));
    }

    let checksum = &header[9..13];

    let mut body = vec![0u8; body_len as usize];
    if body_len > 0 {
        reader.read_exact(&mut body).await?;
    }

    let hash = Blake2b256::digest(&body);
    if &hash[..4] != checksum {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Checksum mismatch"));
    }

    Ok(Frame { code, body })
}

/// Write a frame to an async writer.
///
/// # Contract
/// - **Postcondition**: the full encoded frame is written and flushed.
pub async fn write_frame(
    writer: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    magic: &[u8; 4],
    frame: &Frame,
) -> io::Result<()> {
    let bytes = encode(magic, frame);
    writer.write_all(&bytes).await?;
    writer.flush().await
}
```

- [ ] **Step 4: Update src/transport/mod.rs**

```rust
pub mod vlq;
pub mod frame;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test transport_test 2>&1`
Expected: All VLQ and frame tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/transport/frame.rs src/transport/mod.rs tests/transport_test.rs
git commit -m "feat: frame encoding/decoding with blake2b checksum"
```

---

### Task 4: Transport — Handshake

**Files:**
- Create: `src/transport/handshake.rs`
- Modify: `src/transport/mod.rs`
- Modify: `tests/transport_test.rs`

- [ ] **Step 1: Write failing tests for handshake**

Append to `tests/transport_test.rs`:

```rust
use ergo_proxy_node::transport::handshake::{self, PeerSpec, Feature, HandshakeConfig};
use ergo_proxy_node::types::{Network, Version, ProxyMode};

#[test]
fn handshake_build_parse_roundtrip() {
    let config = HandshakeConfig {
        agent_name: "ergo-proxy".to_string(),
        peer_name: "test-node".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();

    assert_eq!(spec.agent, "ergo-proxy");
    assert_eq!(spec.name, "test-node");
    assert_eq!(spec.version, Version::new(6, 0, 3));
    assert_eq!(spec.address, None);
    assert!(spec.features.len() >= 2); // Mode + Session
}

#[test]
fn handshake_full_mode_feature() {
    let config = HandshakeConfig {
        agent_name: "test".to_string(),
        peer_name: "test".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();

    let mode_feat = spec.features.iter().find(|f| f.id == 16).unwrap();
    // Full mode: stateType=0, verifying=1, nipopow=None(0), blocksToKeep=-1(zigzag=1)
    assert_eq!(mode_feat.body, vec![0x00, 0x01, 0x00, 0x01]);
}

#[test]
fn handshake_light_mode_feature() {
    let config = HandshakeConfig {
        agent_name: "test".to_string(),
        peer_name: "test".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Light,
        declared_address: None,
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();

    let mode_feat = spec.features.iter().find(|f| f.id == 16).unwrap();
    // Light mode: stateType=0, verifying=1, nipopow=Some(1)=0x01 0x02, blocksToKeep=-1=0x01
    assert_eq!(mode_feat.body, vec![0x00, 0x01, 0x01, 0x02, 0x01]);
}

#[test]
fn handshake_session_feature_has_correct_magic() {
    let config = HandshakeConfig {
        agent_name: "test".to_string(),
        peer_name: "test".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();

    let session_feat = spec.features.iter().find(|f| f.id == 3).unwrap();
    assert_eq!(&session_feat.body[0..4], &[2, 3, 2, 3]); // testnet magic
    assert_eq!(session_feat.body.len(), 12); // 4 magic + 8 session ID
}

#[test]
fn handshake_version_validation() {
    let config = HandshakeConfig {
        agent_name: "test".to_string(),
        peer_name: "test".to_string(),
        version: Version::new(3, 0, 0), // below EIP-37 minimum
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();
    assert!(handshake::validate_peer(&spec, &Network::Testnet).is_err());
}

#[test]
fn handshake_valid_peer_accepted() {
    let config = HandshakeConfig {
        agent_name: "ergoref".to_string(),
        peer_name: "test-node".to_string(),
        version: Version::new(6, 0, 3),
        network: Network::Testnet,
        mode: ProxyMode::Full,
        declared_address: None,
    };
    let bytes = handshake::build(&config);
    let spec = handshake::parse(&bytes).unwrap();
    assert!(handshake::validate_peer(&spec, &Network::Testnet).is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test transport_test handshake 2>&1`
Expected: Compilation error — `handshake` module not found.

- [ ] **Step 3: Implement src/transport/handshake.rs**

```rust
//! Ergo P2P handshake: build and parse.
//!
//! # Contract
//! - `build`: given a `HandshakeConfig`, produces raw handshake bytes.
//!   Postcondition: bytes are parseable by `parse`, and contain the configured
//!   Mode and Session features with correct encoding.
//! - `parse`: given raw bytes, extracts a `PeerSpec`.
//!   Precondition: bytes are a valid Ergo handshake payload.
//!   Postcondition: all fields populated; features parsed but unknown IDs are preserved.
//! - `validate_peer`: checks version >= 4.0.100 and session magic matches network.

use crate::transport::vlq;
use crate::types::{Network, ProxyMode, Version};
use std::io::{self, Cursor, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

const FEATURE_MODE: u8 = 16;
const FEATURE_SESSION: u8 = 3;

/// Configuration for building a handshake.
pub struct HandshakeConfig {
    pub agent_name: String,
    pub peer_name: String,
    pub version: Version,
    pub network: Network,
    pub mode: ProxyMode,
    pub declared_address: Option<SocketAddr>,
}

/// A parsed peer feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feature {
    pub id: u8,
    pub body: Vec<u8>,
}

/// Parsed peer specification from a handshake.
#[derive(Debug, Clone)]
pub struct PeerSpec {
    pub agent: String,
    pub version: Version,
    pub name: String,
    pub address: Option<SocketAddr>,
    pub features: Vec<Feature>,
}

/// Build handshake bytes from config.
pub fn build(config: &HandshakeConfig) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);

    // Timestamp
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    vlq::write_vlq(&mut buf, now);

    // Agent name
    vlq::write_short_string(&mut buf, &config.agent_name);

    // Version
    buf.push(config.version.major);
    buf.push(config.version.minor);
    buf.push(config.version.patch);

    // Peer name
    vlq::write_short_string(&mut buf, &config.peer_name);

    // Declared address
    match &config.declared_address {
        None => buf.push(0x00),
        Some(addr) => {
            buf.push(0x01);
            let ip_bytes = match addr.ip() {
                IpAddr::V4(ip) => ip.octets().to_vec(),
                IpAddr::V6(ip) => ip.octets().to_vec(),
            };
            buf.push((ip_bytes.len() + 4) as u8);
            buf.extend_from_slice(&ip_bytes);
            buf.extend_from_slice(&(addr.port() as u32).to_be_bytes());
        }
    }

    // Features: Mode + Session
    buf.push(2); // feature count

    // Mode feature (id=16)
    buf.push(FEATURE_MODE);
    let mode_body = build_mode_body(config.mode);
    buf.extend_from_slice(&(mode_body.len() as u16).to_be_bytes());
    buf.extend_from_slice(&mode_body);

    // Session feature (id=3)
    buf.push(FEATURE_SESSION);
    let session_body = build_session_body(config.network);
    buf.extend_from_slice(&(session_body.len() as u16).to_be_bytes());
    buf.extend_from_slice(&session_body);

    buf
}

fn build_mode_body(mode: ProxyMode) -> Vec<u8> {
    match mode {
        ProxyMode::Full => {
            // stateType=UTXO(0), verifying=true(1), nipopow=None(0), blocksToKeep=-1(zigzag=1)
            vec![0x00, 0x01, 0x00, 0x01]
        }
        ProxyMode::Light => {
            // stateType=UTXO(0), verifying=true(1), nipopow=Some(0x01) value=1(zigzag=2),
            // blocksToKeep=-1(zigzag=1)
            vec![0x00, 0x01, 0x01, 0x02, 0x01]
        }
    }
}

fn build_session_body(network: Network) -> Vec<u8> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&network.magic());
    let session_id = rand_u64();
    body.extend_from_slice(&session_id.to_be_bytes());
    body
}

fn rand_u64() -> u64 {
    let mut buf = [0u8; 8];
    #[cfg(unix)]
    {
        use std::fs::File;
        let mut f = File::open("/dev/urandom").expect("/dev/urandom");
        f.read_exact(&mut buf).expect("read urandom");
    }
    #[cfg(not(unix))]
    {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        buf = t.to_le_bytes();
    }
    u64::from_le_bytes(buf)
}

/// Parse handshake bytes into a PeerSpec.
pub fn parse(data: &[u8]) -> io::Result<PeerSpec> {
    let mut cursor = Cursor::new(data);

    // Timestamp (consume but don't use)
    let _timestamp = vlq::read_vlq(&mut cursor)?;

    // Agent name
    let agent = vlq::read_short_string(&mut cursor)?;

    // Version
    let mut ver = [0u8; 3];
    cursor.read_exact(&mut ver)?;
    let version = Version::new(ver[0], ver[1], ver[2]);

    // Peer name
    let name = vlq::read_short_string(&mut cursor)?;

    // Declared address
    let mut has_addr = [0u8; 1];
    cursor.read_exact(&mut has_addr)?;
    let address = if has_addr[0] != 0 {
        let mut addr_len_byte = [0u8; 1];
        cursor.read_exact(&mut addr_len_byte)?;
        let ip_len = (addr_len_byte[0] as usize).saturating_sub(4);
        let mut ip_bytes = vec![0u8; ip_len];
        cursor.read_exact(&mut ip_bytes)?;
        let mut port_bytes = [0u8; 4];
        cursor.read_exact(&mut port_bytes)?;
        let port = u32::from_be_bytes(port_bytes) as u16;

        match ip_len {
            4 => Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3])),
                port,
            )),
            16 => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&ip_bytes);
                Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
            }
            _ => None,
        }
    } else {
        None
    };

    // Features
    let mut features = Vec::new();
    let mut feat_count_buf = [0u8; 1];
    if cursor.read_exact(&mut feat_count_buf).is_ok() {
        let feat_count = feat_count_buf[0] as usize;
        for _ in 0..feat_count {
            let mut fid = [0u8; 1];
            if cursor.read_exact(&mut fid).is_err() {
                break;
            }
            let mut flen_bytes = [0u8; 2];
            if cursor.read_exact(&mut flen_bytes).is_err() {
                break;
            }
            let flen = u16::from_be_bytes(flen_bytes) as usize;
            let mut fbody = vec![0u8; flen];
            if cursor.read_exact(&mut fbody).is_err() {
                break;
            }
            features.push(Feature { id: fid[0], body: fbody });
        }
    }

    Ok(PeerSpec { agent, version, name, address, features })
}

/// Parse a peer entry from a Peers message body.
/// Same format as handshake PeerSpec but without the timestamp prefix.
pub fn parse_peer_entry(cursor: &mut Cursor<&[u8]>) -> io::Result<PeerSpec> {
    // Agent name
    let agent = vlq::read_short_string(cursor)?;

    // Version
    let mut ver = [0u8; 3];
    cursor.read_exact(&mut ver)?;
    let version = Version::new(ver[0], ver[1], ver[2]);

    // Peer name
    let name = vlq::read_short_string(cursor)?;

    // Declared address
    let mut has_addr = [0u8; 1];
    cursor.read_exact(&mut has_addr)?;
    let address = if has_addr[0] != 0 {
        let mut addr_len_byte = [0u8; 1];
        cursor.read_exact(&mut addr_len_byte)?;
        let ip_len = (addr_len_byte[0] as usize).saturating_sub(4);
        let mut ip_bytes = vec![0u8; ip_len];
        cursor.read_exact(&mut ip_bytes)?;
        let mut port_bytes = [0u8; 4];
        cursor.read_exact(&mut port_bytes)?;
        let port = u32::from_be_bytes(port_bytes) as u16;

        match ip_len {
            4 => Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3])),
                port,
            )),
            16 => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&ip_bytes);
                Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
            }
            _ => None,
        }
    } else {
        None
    };

    // Features
    let mut features = Vec::new();
    let mut feat_count_buf = [0u8; 1];
    cursor.read_exact(&mut feat_count_buf)?;
    let feat_count = feat_count_buf[0] as usize;
    for _ in 0..feat_count {
        let mut fid = [0u8; 1];
        cursor.read_exact(&mut fid)?;
        let mut flen_bytes = [0u8; 2];
        cursor.read_exact(&mut flen_bytes)?;
        let flen = u16::from_be_bytes(flen_bytes) as usize;
        let mut fbody = vec![0u8; flen];
        cursor.read_exact(&mut fbody)?;
        features.push(Feature { id: fid[0], body: fbody });
    }

    Ok(PeerSpec { agent, version, name, address, features })
}

/// Validate a peer's handshake.
///
/// # Contract
/// - **Precondition**: `spec` was produced by `parse`.
/// - **Postcondition**: returns Ok(()) if version >= 4.0.100 and session magic matches, Err otherwise.
pub fn validate_peer(spec: &PeerSpec, network: &Network) -> Result<(), String> {
    if spec.version < Version::EIP37_MIN {
        return Err(format!(
            "Peer version {} is below minimum {} (EIP-37)",
            spec.version,
            Version::EIP37_MIN
        ));
    }

    if let Some(session) = spec.features.iter().find(|f| f.id == FEATURE_SESSION) {
        if session.body.len() >= 4 {
            let peer_magic = &session.body[0..4];
            let expected = network.magic();
            if peer_magic != expected {
                return Err(format!(
                    "Session magic mismatch: {:?} (expected {:?})",
                    peer_magic, expected
                ));
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 4: Update src/transport/mod.rs**

```rust
pub mod vlq;
pub mod frame;
pub mod handshake;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test transport_test 2>&1`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/transport/handshake.rs src/transport/mod.rs tests/transport_test.rs
git commit -m "feat: handshake build/parse with Mode and Session features"
```

---

### Task 5: Transport — Async connection

**Files:**
- Create: `src/transport/connection.rs`
- Modify: `src/transport/mod.rs`

- [ ] **Step 1: Implement src/transport/connection.rs**

```rust
//! Per-connection async read/write loops.
//!
//! # Contract
//! - `Connection::new`: performs handshake exchange, returns a `Connection` in
//!   active state or an error.
//!   Precondition: `stream` is a connected TCP stream.
//!   Postcondition: both sides have exchanged handshakes; peer's PeerSpec is available.
//! - `Connection::read_frame`: reads the next frame from the stream.
//!   Postcondition: returns a validated `Frame` or error.
//! - `Connection::write_frame`: writes a frame to the stream.
//!   Postcondition: frame is fully written and flushed.
//! - Invariant: the connection's magic is fixed at construction time and never changes.

use crate::transport::frame::{self, Frame};
use crate::transport::handshake::{self, HandshakeConfig, PeerSpec};
use crate::types::Network;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_BUF_SIZE: usize = 8192;

/// An active P2P connection with completed handshake.
pub struct Connection {
    stream: TcpStream,
    magic: [u8; 4],
    peer_spec: PeerSpec,
}

impl Connection {
    /// Establish a connection by performing the handshake as initiator (outbound).
    ///
    /// Sends our handshake, reads the peer's handshake, validates it.
    pub async fn outbound(
        mut stream: TcpStream,
        config: &HandshakeConfig,
    ) -> io::Result<Self> {
        let magic = config.network.magic();

        // Send our handshake
        let hs_bytes = handshake::build(config);
        stream.write_all(&hs_bytes).await?;
        stream.flush().await?;

        // Read peer's handshake
        let peer_spec = timeout(HANDSHAKE_TIMEOUT, read_handshake(&mut stream))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Handshake timeout"))??;

        // Validate
        handshake::validate_peer(&peer_spec, &config.network)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        Ok(Self { stream, magic, peer_spec })
    }

    /// Accept a connection by performing the handshake as responder (inbound).
    ///
    /// Reads the peer's handshake first, validates it, then sends ours.
    pub async fn inbound(
        mut stream: TcpStream,
        config: &HandshakeConfig,
    ) -> io::Result<Self> {
        let magic = config.network.magic();

        // Read peer's handshake first
        let peer_spec = timeout(HANDSHAKE_TIMEOUT, read_handshake(&mut stream))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Handshake timeout"))??;

        // Validate before sending ours
        handshake::validate_peer(&peer_spec, &config.network)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Send our handshake
        let hs_bytes = handshake::build(config);
        stream.write_all(&hs_bytes).await?;
        stream.flush().await?;

        Ok(Self { stream, magic, peer_spec })
    }

    /// Read the next message frame.
    pub async fn read_frame(&mut self) -> io::Result<Frame> {
        frame::read_frame(&mut self.stream, &self.magic).await
    }

    /// Write a message frame.
    pub async fn write_frame(&mut self, f: &Frame) -> io::Result<()> {
        frame::write_frame(&mut self.stream, &self.magic, f).await
    }

    /// Get the peer's specification from the handshake.
    pub fn peer_spec(&self) -> &PeerSpec {
        &self.peer_spec
    }

    /// Split the connection into separate read and write halves.
    /// Returns (reader, writer, magic, peer_spec).
    pub fn split(self) -> (tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf, [u8; 4], PeerSpec) {
        let (read, write) = self.stream.into_split();
        (read, write, self.magic, self.peer_spec)
    }
}

async fn read_handshake(stream: &mut TcpStream) -> io::Result<PeerSpec> {
    let mut buf = vec![0u8; HANDSHAKE_BUF_SIZE];
    // The handshake is a single variable-length message. Read what's available.
    // We may need to wait briefly for the full payload to arrive.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::ConnectionReset, "Empty handshake"));
    }
    handshake::parse(&buf[..n])
}
```

- [ ] **Step 2: Update src/transport/mod.rs**

```rust
pub mod vlq;
pub mod frame;
pub mod handshake;
pub mod connection;
```

- [ ] **Step 3: Verify it compiles**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo build 2>&1`
Expected: Compiles. No runtime test here — connection requires real TCP streams; tested in integration.

- [ ] **Step 4: Commit**

```bash
git add src/transport/connection.rs src/transport/mod.rs
git commit -m "feat: async connection with handshake exchange"
```

---

### Task 6: Protocol — Message types and parsing

**Files:**
- Create: `src/protocol/mod.rs`
- Create: `src/protocol/messages.rs`
- Create: `tests/protocol_test.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing tests**

Create `tests/protocol_test.rs`:

```rust
use ergo_proxy_node::protocol::messages::{ProtocolMessage, MessageCode};
use ergo_proxy_node::transport::frame::Frame;
use ergo_proxy_node::transport::vlq;

#[test]
fn parse_inv_message() {
    let mut body = Vec::new();
    body.push(2); // TX_TYPE_ID
    vlq::write_vlq(&mut body, 1); // count = 1
    body.extend_from_slice(&[0xaa; 32]); // one modifier ID

    let frame = Frame { code: MessageCode::INV, body };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();

    match msg {
        ProtocolMessage::Inv { modifier_type, ids } => {
            assert_eq!(modifier_type, 2);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0], [0xaa; 32]);
        }
        _ => panic!("Expected Inv"),
    }
}

#[test]
fn parse_modifier_request() {
    let mut body = Vec::new();
    body.push(2); // TX_TYPE_ID
    vlq::write_vlq(&mut body, 2); // count = 2
    body.extend_from_slice(&[0xbb; 32]);
    body.extend_from_slice(&[0xcc; 32]);

    let frame = Frame { code: MessageCode::MODIFIER_REQUEST, body };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();

    match msg {
        ProtocolMessage::ModifierRequest { modifier_type, ids } => {
            assert_eq!(modifier_type, 2);
            assert_eq!(ids.len(), 2);
            assert_eq!(ids[0], [0xbb; 32]);
            assert_eq!(ids[1], [0xcc; 32]);
        }
        _ => panic!("Expected ModifierRequest"),
    }
}

#[test]
fn parse_modifier_response() {
    let mut body = Vec::new();
    body.push(2); // TX_TYPE_ID
    vlq::write_vlq(&mut body, 1); // count = 1
    body.extend_from_slice(&[0xdd; 32]); // modifier ID
    vlq::write_vlq(&mut body, 4); // data length
    body.extend_from_slice(&[1, 2, 3, 4]); // data

    let frame = Frame { code: MessageCode::MODIFIER_RESPONSE, body };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();

    match msg {
        ProtocolMessage::ModifierResponse { modifier_type, modifiers } => {
            assert_eq!(modifier_type, 2);
            assert_eq!(modifiers.len(), 1);
            assert_eq!(modifiers[0].0, [0xdd; 32]);
            assert_eq!(modifiers[0].1, vec![1, 2, 3, 4]);
        }
        _ => panic!("Expected ModifierResponse"),
    }
}

#[test]
fn parse_get_peers() {
    let frame = Frame { code: MessageCode::GET_PEERS, body: vec![] };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();
    assert!(matches!(msg, ProtocolMessage::GetPeers));
}

#[test]
fn parse_sync_info_is_opaque() {
    let body = vec![0x01, 0x02, 0x03, 0x04, 0x05];
    let frame = Frame { code: MessageCode::SYNC_INFO, body: body.clone() };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();
    match msg {
        ProtocolMessage::SyncInfo { body: b } => assert_eq!(b, body),
        _ => panic!("Expected SyncInfo"),
    }
}

#[test]
fn parse_unknown_code_preserved() {
    let frame = Frame { code: 99, body: vec![0xff] };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();
    match msg {
        ProtocolMessage::Unknown { code, body } => {
            assert_eq!(code, 99);
            assert_eq!(body, vec![0xff]);
        }
        _ => panic!("Expected Unknown"),
    }
}

#[test]
fn inv_to_frame_roundtrip() {
    let mut ids_body = Vec::new();
    ids_body.push(2);
    vlq::write_vlq(&mut ids_body, 1);
    ids_body.extend_from_slice(&[0xee; 32]);

    let frame = Frame { code: MessageCode::INV, body: ids_body };
    let msg = ProtocolMessage::from_frame(&frame).unwrap();
    let back = msg.to_frame();
    assert_eq!(back.code, frame.code);
    assert_eq!(back.body, frame.body);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test protocol_test 2>&1`
Expected: Compilation error — `protocol` module not found.

- [ ] **Step 3: Implement src/protocol/messages.rs**

```rust
//! Typed P2P protocol messages with parse/serialize.
//!
//! # Contract
//! - `from_frame`: parses a `Frame` into a typed `ProtocolMessage`.
//!   Precondition: frame has a valid code and body.
//!   Postcondition: returns a typed message, or `Unknown` for unrecognized codes.
//!   SyncInfo body is preserved opaque. Unknown codes are preserved, not dropped.
//! - `to_frame`: serializes a `ProtocolMessage` back into a `Frame`.
//!   Postcondition: `ProtocolMessage::from_frame(&msg.to_frame()) ≈ msg` (roundtrip).
//! - Invariant: the transport layer never sees typed messages; the protocol layer
//!   never sees raw bytes.

use crate::transport::frame::Frame;
use crate::transport::vlq;
use crate::types::ModifierId;
use std::io::{self, Cursor, Read};

/// Well-known message codes.
pub struct MessageCode;

impl MessageCode {
    pub const GET_PEERS: u8 = 1;
    pub const PEERS: u8 = 2;
    pub const MODIFIER_REQUEST: u8 = 22;
    pub const MODIFIER_RESPONSE: u8 = 33;
    pub const INV: u8 = 55;
    pub const SYNC_INFO: u8 = 65;
}

/// A typed protocol message.
#[derive(Debug, Clone)]
pub enum ProtocolMessage {
    GetPeers,
    Peers { body: Vec<u8> },
    Inv { modifier_type: u8, ids: Vec<ModifierId> },
    ModifierRequest { modifier_type: u8, ids: Vec<ModifierId> },
    ModifierResponse { modifier_type: u8, modifiers: Vec<(ModifierId, Vec<u8>)> },
    SyncInfo { body: Vec<u8> },
    Unknown { code: u8, body: Vec<u8> },
}

impl ProtocolMessage {
    /// Parse a Frame into a typed message.
    pub fn from_frame(frame: &Frame) -> io::Result<Self> {
        match frame.code {
            MessageCode::GET_PEERS => Ok(ProtocolMessage::GetPeers),

            MessageCode::PEERS => {
                // Keep raw — routing layer will parse peer entries as needed
                Ok(ProtocolMessage::Peers { body: frame.body.clone() })
            }

            MessageCode::INV => {
                let (modifier_type, ids) = parse_inv_body(&frame.body)?;
                Ok(ProtocolMessage::Inv { modifier_type, ids })
            }

            MessageCode::MODIFIER_REQUEST => {
                let (modifier_type, ids) = parse_inv_body(&frame.body)?;
                Ok(ProtocolMessage::ModifierRequest { modifier_type, ids })
            }

            MessageCode::MODIFIER_RESPONSE => {
                let (modifier_type, modifiers) = parse_modifier_response_body(&frame.body)?;
                Ok(ProtocolMessage::ModifierResponse { modifier_type, modifiers })
            }

            MessageCode::SYNC_INFO => {
                Ok(ProtocolMessage::SyncInfo { body: frame.body.clone() })
            }

            code => {
                Ok(ProtocolMessage::Unknown { code, body: frame.body.clone() })
            }
        }
    }

    /// Serialize a typed message back into a Frame.
    pub fn to_frame(&self) -> Frame {
        match self {
            ProtocolMessage::GetPeers => Frame { code: MessageCode::GET_PEERS, body: vec![] },

            ProtocolMessage::Peers { body } => Frame { code: MessageCode::PEERS, body: body.clone() },

            ProtocolMessage::Inv { modifier_type, ids } => {
                Frame { code: MessageCode::INV, body: encode_inv_body(*modifier_type, ids) }
            }

            ProtocolMessage::ModifierRequest { modifier_type, ids } => {
                Frame { code: MessageCode::MODIFIER_REQUEST, body: encode_inv_body(*modifier_type, ids) }
            }

            ProtocolMessage::ModifierResponse { modifier_type, modifiers } => {
                Frame {
                    code: MessageCode::MODIFIER_RESPONSE,
                    body: encode_modifier_response_body(*modifier_type, modifiers),
                }
            }

            ProtocolMessage::SyncInfo { body } => Frame { code: MessageCode::SYNC_INFO, body: body.clone() },

            ProtocolMessage::Unknown { code, body } => Frame { code: *code, body: body.clone() },
        }
    }
}

/// Parse Inv / ModifierRequest body: [type:1][count:VLQ][ids:32*count]
fn parse_inv_body(data: &[u8]) -> io::Result<(u8, Vec<ModifierId>)> {
    let mut cursor = Cursor::new(data);
    let mut type_byte = [0u8; 1];
    cursor.read_exact(&mut type_byte)?;
    let count = vlq::read_vlq_length(&mut cursor)?;
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        let mut id = [0u8; 32];
        cursor.read_exact(&mut id)?;
        ids.push(id);
    }
    Ok((type_byte[0], ids))
}

/// Encode Inv / ModifierRequest body.
fn encode_inv_body(modifier_type: u8, ids: &[ModifierId]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(modifier_type);
    vlq::write_vlq(&mut body, ids.len() as u64);
    for id in ids {
        body.extend_from_slice(id);
    }
    body
}

/// Parse ModifierResponse body: [type:1][count:VLQ][ [id:32][len:VLQ][data:len] * count ]
fn parse_modifier_response_body(data: &[u8]) -> io::Result<(u8, Vec<(ModifierId, Vec<u8>)>)> {
    let mut cursor = Cursor::new(data);
    let mut type_byte = [0u8; 1];
    cursor.read_exact(&mut type_byte)?;
    let count = vlq::read_vlq_length(&mut cursor)?;
    let mut modifiers = Vec::with_capacity(count);
    for _ in 0..count {
        let mut id = [0u8; 32];
        cursor.read_exact(&mut id)?;
        let data_len = vlq::read_vlq_length(&mut cursor)?;
        let mut mod_data = vec![0u8; data_len];
        cursor.read_exact(&mut mod_data)?;
        modifiers.push((id, mod_data));
    }
    Ok((type_byte[0], modifiers))
}

/// Encode ModifierResponse body.
fn encode_modifier_response_body(modifier_type: u8, modifiers: &[(ModifierId, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(modifier_type);
    vlq::write_vlq(&mut body, modifiers.len() as u64);
    for (id, data) in modifiers {
        body.extend_from_slice(id);
        vlq::write_vlq(&mut body, data.len() as u64);
        body.extend_from_slice(data);
    }
    body
}
```

- [ ] **Step 4: Create src/protocol/mod.rs**

```rust
pub mod messages;
```

- [ ] **Step 5: Update src/lib.rs**

```rust
pub mod config;
pub mod types;
pub mod transport;
pub mod protocol;
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test protocol_test 2>&1`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/protocol/ src/lib.rs tests/protocol_test.rs
git commit -m "feat: typed protocol messages with parse/serialize"
```

---

### Task 7: Protocol — Peer state machine

**Files:**
- Create: `src/protocol/peer.rs`
- Modify: `src/protocol/mod.rs`
- Modify: `tests/protocol_test.rs`

- [ ] **Step 1: Write failing tests**

Append to `tests/protocol_test.rs`:

```rust
use ergo_proxy_node::protocol::peer::{PeerState, PeerStateMachine, ProtocolEvent};
use ergo_proxy_node::types::{PeerId, Direction, Version};
use ergo_proxy_node::transport::handshake::PeerSpec;

#[test]
fn peer_starts_connecting() {
    let peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    assert_eq!(peer.state(), PeerState::Connecting);
}

#[test]
fn peer_transitions_to_handshaking() {
    let mut peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    peer.set_handshaking();
    assert_eq!(peer.state(), PeerState::Handshaking);
}

#[test]
fn peer_transitions_to_active() {
    let mut peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    peer.set_handshaking();
    let spec = PeerSpec {
        agent: "test".into(),
        version: Version::new(6, 0, 3),
        name: "test-node".into(),
        address: None,
        features: vec![],
    };
    let event = peer.set_active(spec.clone());
    assert_eq!(peer.state(), PeerState::Active);
    match event {
        ProtocolEvent::PeerConnected { peer_id, direction, .. } => {
            assert_eq!(peer_id, PeerId(1));
            assert_eq!(direction, Direction::Outbound);
        }
        _ => panic!("Expected PeerConnected"),
    }
}

#[test]
fn peer_transitions_to_disconnected() {
    let mut peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    peer.set_handshaking();
    let spec = PeerSpec {
        agent: "test".into(),
        version: Version::new(6, 0, 3),
        name: "test-node".into(),
        address: None,
        features: vec![],
    };
    peer.set_active(spec);
    let event = peer.set_disconnected("test reason".into());
    assert_eq!(peer.state(), PeerState::Disconnected);
    match event {
        ProtocolEvent::PeerDisconnected { peer_id, reason } => {
            assert_eq!(peer_id, PeerId(1));
            assert_eq!(reason, "test reason");
        }
        _ => panic!("Expected PeerDisconnected"),
    }
}

#[test]
fn peer_transitions_to_failed() {
    let mut peer = PeerStateMachine::new(PeerId(1), Direction::Outbound);
    peer.set_handshaking();
    peer.set_failed("bad version".into());
    assert_eq!(peer.state(), PeerState::Failed);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test protocol_test peer 2>&1`
Expected: Compilation error — `peer` module not found.

- [ ] **Step 3: Implement src/protocol/peer.rs**

```rust
//! Peer lifecycle state machine.
//!
//! # Contract
//! - State transitions follow: Connecting → Handshaking → Active → Disconnected.
//!   Handshaking can also transition to Failed.
//! - `set_active` produces a `PeerConnected` event.
//! - `set_disconnected` produces a `PeerDisconnected` event.
//! - Invariant: a peer in any state other than `Active` cannot produce `Message` events.
//! - Invariant: state transitions are one-way; no state can transition backwards.

use crate::transport::handshake::PeerSpec;
use crate::types::{Direction, PeerId};
use crate::protocol::messages::ProtocolMessage;

/// Peer connection states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Connecting,
    Handshaking,
    Active,
    Disconnected,
    Failed,
}

/// Events produced by the protocol layer.
#[derive(Debug, Clone)]
pub enum ProtocolEvent {
    PeerConnected {
        peer_id: PeerId,
        spec: PeerSpec,
        direction: Direction,
    },
    PeerDisconnected {
        peer_id: PeerId,
        reason: String,
    },
    Message {
        peer_id: PeerId,
        message: ProtocolMessage,
    },
}

/// State machine for a single peer's lifecycle.
pub struct PeerStateMachine {
    peer_id: PeerId,
    direction: Direction,
    state: PeerState,
    spec: Option<PeerSpec>,
}

impl PeerStateMachine {
    pub fn new(peer_id: PeerId, direction: Direction) -> Self {
        Self {
            peer_id,
            direction,
            state: PeerState::Connecting,
            spec: None,
        }
    }

    pub fn state(&self) -> PeerState {
        self.state
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn spec(&self) -> Option<&PeerSpec> {
        self.spec.as_ref()
    }

    /// Transition to Handshaking.
    ///
    /// # Contract
    /// - Precondition: state is Connecting.
    pub fn set_handshaking(&mut self) {
        debug_assert_eq!(self.state, PeerState::Connecting);
        self.state = PeerState::Handshaking;
    }

    /// Transition to Active. Returns PeerConnected event.
    ///
    /// # Contract
    /// - Precondition: state is Handshaking.
    /// - Postcondition: state is Active, PeerConnected event is returned.
    pub fn set_active(&mut self, spec: PeerSpec) -> ProtocolEvent {
        debug_assert_eq!(self.state, PeerState::Handshaking);
        self.spec = Some(spec.clone());
        self.state = PeerState::Active;
        ProtocolEvent::PeerConnected {
            peer_id: self.peer_id,
            spec,
            direction: self.direction,
        }
    }

    /// Transition to Disconnected. Returns PeerDisconnected event.
    ///
    /// # Contract
    /// - Precondition: state is Active.
    /// - Postcondition: state is Disconnected, PeerDisconnected event is returned.
    pub fn set_disconnected(&mut self, reason: String) -> ProtocolEvent {
        debug_assert_eq!(self.state, PeerState::Active);
        self.state = PeerState::Disconnected;
        ProtocolEvent::PeerDisconnected {
            peer_id: self.peer_id,
            reason,
        }
    }

    /// Transition to Failed (from Handshaking).
    ///
    /// # Contract
    /// - Precondition: state is Handshaking.
    pub fn set_failed(&mut self, _reason: String) {
        debug_assert_eq!(self.state, PeerState::Handshaking);
        self.state = PeerState::Failed;
    }

    /// Wrap a parsed message into a ProtocolEvent.
    ///
    /// # Contract
    /// - Precondition: state is Active.
    pub fn message_event(&self, message: ProtocolMessage) -> ProtocolEvent {
        debug_assert_eq!(self.state, PeerState::Active);
        ProtocolEvent::Message {
            peer_id: self.peer_id,
            message,
        }
    }
}
```

- [ ] **Step 4: Update src/protocol/mod.rs**

```rust
pub mod messages;
pub mod peer;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test protocol_test 2>&1`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/protocol/peer.rs src/protocol/mod.rs tests/protocol_test.rs
git commit -m "feat: peer lifecycle state machine"
```

---

### Task 8: Routing — Inv table

**Files:**
- Create: `src/routing/mod.rs`
- Create: `src/routing/inv_table.rs`
- Create: `tests/routing_test.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing tests**

Create `tests/routing_test.rs`:

```rust
use ergo_proxy_node::routing::inv_table::InvTable;
use ergo_proxy_node::types::PeerId;

#[test]
fn inv_table_record_and_lookup() {
    let mut table = InvTable::new();
    let id = [0xaa; 32];
    table.record(id, PeerId(1));
    assert_eq!(table.lookup(&id), Some(PeerId(1)));
}

#[test]
fn inv_table_lookup_missing() {
    let table = InvTable::new();
    assert_eq!(table.lookup(&[0xbb; 32]), None);
}

#[test]
fn inv_table_latest_announcer_wins() {
    let mut table = InvTable::new();
    let id = [0xaa; 32];
    table.record(id, PeerId(1));
    table.record(id, PeerId(2));
    assert_eq!(table.lookup(&id), Some(PeerId(2)));
}

#[test]
fn inv_table_purge_peer() {
    let mut table = InvTable::new();
    table.record([0xaa; 32], PeerId(1));
    table.record([0xbb; 32], PeerId(1));
    table.record([0xcc; 32], PeerId(2));

    table.purge_peer(PeerId(1));

    assert_eq!(table.lookup(&[0xaa; 32]), None);
    assert_eq!(table.lookup(&[0xbb; 32]), None);
    assert_eq!(table.lookup(&[0xcc; 32]), Some(PeerId(2)));
}

#[test]
fn inv_table_invariant_no_disconnected_peers() {
    let mut table = InvTable::new();
    for i in 0..100 {
        let mut id = [0u8; 32];
        id[0] = i;
        table.record(id, PeerId(1));
    }
    table.purge_peer(PeerId(1));
    assert!(table.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test routing_test 2>&1`
Expected: Compilation error — `routing` module not found.

- [ ] **Step 3: Implement src/routing/inv_table.rs**

```rust
//! Inv table: maps modifier IDs to the peer that announced them.
//!
//! # Contract
//! - `record(id, peer)`: associates a modifier ID with a peer.
//!   Postcondition: `lookup(id) == Some(peer)`.
//! - `lookup(id)`: returns the peer that announced the modifier, or None.
//! - `purge_peer(peer)`: removes all entries for a peer.
//!   Postcondition: no entry maps to the purged peer.
//! - Invariant: the table never contains entries for disconnected peers
//!   (enforced by caller invoking `purge_peer` on disconnect).

use crate::types::{ModifierId, PeerId};
use std::collections::HashMap;

pub struct InvTable {
    entries: HashMap<ModifierId, PeerId>,
}

impl InvTable {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Record that `peer` has announced `modifier_id`.
    pub fn record(&mut self, modifier_id: ModifierId, peer: PeerId) {
        self.entries.insert(modifier_id, peer);
    }

    /// Look up which peer announced the given modifier.
    pub fn lookup(&self, modifier_id: &ModifierId) -> Option<PeerId> {
        self.entries.get(modifier_id).copied()
    }

    /// Remove all entries associated with a peer.
    pub fn purge_peer(&mut self, peer: PeerId) {
        self.entries.retain(|_, p| *p != peer);

        #[cfg(debug_assertions)]
        self.check_invariant_no_peer(peer);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(debug_assertions)]
    fn check_invariant_no_peer(&self, peer: PeerId) {
        debug_assert!(
            !self.entries.values().any(|p| *p == peer),
            "Invariant violated: Inv table still contains entries for purged peer {}",
            peer
        );
    }
}
```

- [ ] **Step 4: Create src/routing/mod.rs**

```rust
pub mod inv_table;
```

- [ ] **Step 5: Update src/lib.rs**

```rust
pub mod config;
pub mod types;
pub mod transport;
pub mod protocol;
pub mod routing;
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test routing_test 2>&1`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/routing/ src/lib.rs tests/routing_test.rs
git commit -m "feat: Inv table with peer purge and invariant checks"
```

---

### Task 9: Routing — Request tracker and SyncInfo tracker

**Files:**
- Create: `src/routing/tracker.rs`
- Modify: `src/routing/mod.rs`
- Modify: `tests/routing_test.rs`

- [ ] **Step 1: Write failing tests**

Append to `tests/routing_test.rs`:

```rust
use ergo_proxy_node::routing::tracker::{RequestTracker, SyncTracker};

#[test]
fn request_tracker_record_and_lookup() {
    let mut tracker = RequestTracker::new();
    let id = [0xaa; 32];
    tracker.record(id, PeerId(5));
    assert_eq!(tracker.lookup(&id), Some(PeerId(5)));
}

#[test]
fn request_tracker_fulfill_removes_entry() {
    let mut tracker = RequestTracker::new();
    let id = [0xaa; 32];
    tracker.record(id, PeerId(5));
    let requester = tracker.fulfill(&id);
    assert_eq!(requester, Some(PeerId(5)));
    assert_eq!(tracker.lookup(&id), None);
}

#[test]
fn request_tracker_fulfill_missing() {
    let mut tracker = RequestTracker::new();
    assert_eq!(tracker.fulfill(&[0xbb; 32]), None);
}

#[test]
fn request_tracker_purge_peer() {
    let mut tracker = RequestTracker::new();
    tracker.record([0xaa; 32], PeerId(1));
    tracker.record([0xbb; 32], PeerId(2));
    tracker.record([0xcc; 32], PeerId(1));

    tracker.purge_peer(PeerId(1));

    assert_eq!(tracker.lookup(&[0xaa; 32]), None);
    assert_eq!(tracker.lookup(&[0xbb; 32]), Some(PeerId(2)));
    assert_eq!(tracker.lookup(&[0xcc; 32]), None);
}

#[test]
fn sync_tracker_pair_and_lookup() {
    let mut tracker = SyncTracker::new();
    tracker.pair(PeerId(1), PeerId(10));
    assert_eq!(tracker.outbound_for(&PeerId(1)), Some(PeerId(10)));
    assert_eq!(tracker.inbound_for(&PeerId(10)), Some(PeerId(1)));
}

#[test]
fn sync_tracker_purge_inbound() {
    let mut tracker = SyncTracker::new();
    tracker.pair(PeerId(1), PeerId(10));
    tracker.purge_peer(PeerId(1));
    assert_eq!(tracker.outbound_for(&PeerId(1)), None);
    assert_eq!(tracker.inbound_for(&PeerId(10)), None);
}

#[test]
fn sync_tracker_purge_outbound() {
    let mut tracker = SyncTracker::new();
    tracker.pair(PeerId(1), PeerId(10));
    tracker.purge_peer(PeerId(10));
    assert_eq!(tracker.outbound_for(&PeerId(1)), None);
    assert_eq!(tracker.inbound_for(&PeerId(10)), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test routing_test tracker 2>&1`
Expected: Compilation error — `tracker` module not found.

- [ ] **Step 3: Implement src/routing/tracker.rs**

```rust
//! Request tracker and SyncInfo tracker.
//!
//! # RequestTracker Contract
//! - `record(id, peer)`: records that `peer` requested modifier `id`.
//! - `fulfill(id)`: returns and removes the requester for `id`.
//!   Postcondition: entry is removed after fulfill.
//! - `purge_peer(peer)`: removes all entries for a peer.
//!
//! # SyncTracker Contract
//! - `pair(inbound, outbound)`: records that `inbound`'s sync is handled by `outbound`.
//! - `outbound_for(inbound)`: returns the outbound peer handling this inbound peer's sync.
//! - `inbound_for(outbound)`: returns the inbound peer whose sync is handled by this outbound peer.
//! - `purge_peer(peer)`: removes any pairing involving this peer.
//! - Invariant: pairings are bidirectionally consistent — if A→B exists, B→A exists.

use crate::types::{ModifierId, PeerId};
use std::collections::HashMap;

pub struct RequestTracker {
    /// modifier ID → peer that requested it
    pending: HashMap<ModifierId, PeerId>,
}

impl RequestTracker {
    pub fn new() -> Self {
        Self { pending: HashMap::new() }
    }

    pub fn record(&mut self, modifier_id: ModifierId, requester: PeerId) {
        self.pending.insert(modifier_id, requester);
    }

    pub fn lookup(&self, modifier_id: &ModifierId) -> Option<PeerId> {
        self.pending.get(modifier_id).copied()
    }

    /// Fulfill a request: return the requester and remove the entry.
    pub fn fulfill(&mut self, modifier_id: &ModifierId) -> Option<PeerId> {
        self.pending.remove(modifier_id)
    }

    pub fn purge_peer(&mut self, peer: PeerId) {
        self.pending.retain(|_, p| *p != peer);
    }
}

pub struct SyncTracker {
    /// inbound peer → outbound peer handling their sync
    inbound_to_outbound: HashMap<PeerId, PeerId>,
    /// outbound peer → inbound peer whose sync they handle
    outbound_to_inbound: HashMap<PeerId, PeerId>,
}

impl SyncTracker {
    pub fn new() -> Self {
        Self {
            inbound_to_outbound: HashMap::new(),
            outbound_to_inbound: HashMap::new(),
        }
    }

    /// Pair an inbound peer with an outbound peer for sync.
    pub fn pair(&mut self, inbound: PeerId, outbound: PeerId) {
        self.inbound_to_outbound.insert(inbound, outbound);
        self.outbound_to_inbound.insert(outbound, inbound);

        #[cfg(debug_assertions)]
        self.check_invariant();
    }

    pub fn outbound_for(&self, inbound: &PeerId) -> Option<PeerId> {
        self.inbound_to_outbound.get(inbound).copied()
    }

    pub fn inbound_for(&self, outbound: &PeerId) -> Option<PeerId> {
        self.outbound_to_inbound.get(outbound).copied()
    }

    /// Remove any pairing involving this peer (either side).
    pub fn purge_peer(&mut self, peer: PeerId) {
        // Check if it's an inbound peer
        if let Some(outbound) = self.inbound_to_outbound.remove(&peer) {
            self.outbound_to_inbound.remove(&outbound);
        }
        // Check if it's an outbound peer
        if let Some(inbound) = self.outbound_to_inbound.remove(&peer) {
            self.inbound_to_outbound.remove(&inbound);
        }

        #[cfg(debug_assertions)]
        self.check_invariant();
    }

    #[cfg(debug_assertions)]
    fn check_invariant(&self) {
        for (&inb, &outb) in &self.inbound_to_outbound {
            debug_assert_eq!(
                self.outbound_to_inbound.get(&outb),
                Some(&inb),
                "SyncTracker invariant violated: inbound {} → outbound {} but reverse missing",
                inb, outb
            );
        }
        for (&outb, &inb) in &self.outbound_to_inbound {
            debug_assert_eq!(
                self.inbound_to_outbound.get(&inb),
                Some(&outb),
                "SyncTracker invariant violated: outbound {} → inbound {} but reverse missing",
                outb, inb
            );
        }
    }
}
```

- [ ] **Step 4: Update src/routing/mod.rs**

```rust
pub mod inv_table;
pub mod tracker;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test routing_test 2>&1`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/routing/tracker.rs src/routing/mod.rs tests/routing_test.rs
git commit -m "feat: request tracker and sync tracker with bidirectional invariants"
```

---

### Task 10: Routing — Router (message forwarding and mode filtering)

**Files:**
- Create: `src/routing/router.rs`
- Modify: `src/routing/mod.rs`
- Modify: `tests/routing_test.rs`

- [ ] **Step 1: Write failing tests**

Append to `tests/routing_test.rs`:

```rust
use ergo_proxy_node::routing::router::{Router, Action};
use ergo_proxy_node::protocol::messages::ProtocolMessage;
use ergo_proxy_node::protocol::peer::ProtocolEvent;
use ergo_proxy_node::types::{PeerId, Direction, ProxyMode};

fn make_router() -> Router {
    Router::new()
}

#[test]
fn router_inv_from_outbound_forwards_to_inbound() {
    let mut router = make_router();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);
    router.register_peer(PeerId(3), Direction::Inbound, ProxyMode::Full);

    let event = ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv {
            modifier_type: 2,
            ids: vec![[0xaa; 32]],
        },
    };

    let actions = router.handle_event(event);

    // Should forward to both inbound peers, not back to source
    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert!(targets.contains(&PeerId(2)));
    assert!(targets.contains(&PeerId(3)));
    assert!(!targets.contains(&PeerId(1)));
}

#[test]
fn router_modifier_request_routes_via_inv_table() {
    let mut router = make_router();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // Outbound peer announces modifier
    let inv_event = ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv {
            modifier_type: 2,
            ids: vec![[0xaa; 32]],
        },
    };
    router.handle_event(inv_event);

    // Inbound peer requests it
    let req_event = ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest {
            modifier_type: 2,
            ids: vec![[0xaa; 32]],
        },
    };
    let actions = router.handle_event(req_event);

    // Should route to PeerId(1) which announced it
    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(1)]);
}

#[test]
fn router_modifier_response_routes_to_requester() {
    let mut router = make_router();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // Set up: outbound announced, inbound requested
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });

    // Outbound responds
    let resp_event = ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 2,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    };
    let actions = router.handle_event(resp_event);

    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
        _ => None,
    }).collect();
    assert_eq!(targets, vec![PeerId(2)]);
}

#[test]
fn router_get_peers_handled_directly() {
    let mut router = make_router();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);

    let event = ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::GetPeers,
    };
    let actions = router.handle_event(event);

    // Should produce a Send with Peers message back to requester
    assert!(actions.iter().any(|a| matches!(a, Action::Send { target, message }
        if *target == PeerId(1) && matches!(message, ProtocolMessage::Peers { .. })
    )));
}

#[test]
fn router_light_mode_drops_sync_info() {
    let mut router = make_router();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Light);

    let event = ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo { body: vec![1, 2, 3] },
    };
    let actions = router.handle_event(event);
    assert!(actions.is_empty(), "Light mode should not forward SyncInfo");
}

#[test]
fn router_full_mode_forwards_sync_info() {
    let mut router = make_router();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    let event = ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo { body: vec![1, 2, 3] },
    };
    let actions = router.handle_event(event);
    assert!(!actions.is_empty(), "Full mode should forward SyncInfo");
}

#[test]
fn router_peer_disconnect_purges_state() {
    let mut router = make_router();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // Outbound announces a modifier
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });

    // Disconnect outbound peer
    router.handle_event(ProtocolEvent::PeerDisconnected {
        peer_id: PeerId(1),
        reason: "gone".into(),
    });

    // Request for that modifier should now fail to route
    let event = ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 2, ids: vec![[0xaa; 32]] },
    };
    let actions = router.handle_event(event);
    assert!(actions.is_empty(), "Should not route to disconnected peer");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test routing_test router 2>&1`
Expected: Compilation error — `router` module not found.

- [ ] **Step 3: Implement src/routing/router.rs**

```rust
//! Message routing: forwarding decisions, mode filtering, peer registry.
//!
//! # Contract
//! - `handle_event`: given a `ProtocolEvent`, returns a list of `Action`s.
//!   Precondition: peer IDs in events are registered (or being disconnected).
//!   Postcondition: actions target only registered, non-disconnected peers.
//! - `register_peer` / `unregister_peer`: manage the peer registry.
//! - Invariant: Inv table, request tracker, and sync tracker are consistent with
//!   the peer registry — no references to unregistered peers.

use crate::protocol::messages::ProtocolMessage;
use crate::protocol::peer::ProtocolEvent;
use crate::routing::inv_table::InvTable;
use crate::routing::tracker::{RequestTracker, SyncTracker};
use crate::types::{Direction, PeerId, ProxyMode};
use std::collections::HashMap;

/// A routing directive: send a message to a specific peer.
#[derive(Debug)]
pub enum Action {
    Send {
        target: PeerId,
        message: ProtocolMessage,
    },
}

struct PeerEntry {
    direction: Direction,
    mode: ProxyMode,
}

pub struct Router {
    peers: HashMap<PeerId, PeerEntry>,
    inv_table: InvTable,
    request_tracker: RequestTracker,
    sync_tracker: SyncTracker,
}

impl Router {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            inv_table: InvTable::new(),
            request_tracker: RequestTracker::new(),
            sync_tracker: SyncTracker::new(),
        }
    }

    pub fn register_peer(&mut self, peer_id: PeerId, direction: Direction, mode: ProxyMode) {
        self.peers.insert(peer_id, PeerEntry { direction, mode });
    }

    pub fn handle_event(&mut self, event: ProtocolEvent) -> Vec<Action> {
        match event {
            ProtocolEvent::PeerConnected { peer_id, direction, .. } => {
                // Peer already registered via register_peer before connection completes
                vec![]
            }

            ProtocolEvent::PeerDisconnected { peer_id, .. } => {
                self.inv_table.purge_peer(peer_id);
                self.request_tracker.purge_peer(peer_id);
                self.sync_tracker.purge_peer(peer_id);
                self.peers.remove(&peer_id);
                vec![]
            }

            ProtocolEvent::Message { peer_id, message } => {
                self.route_message(peer_id, message)
            }
        }
    }

    fn route_message(&mut self, source: PeerId, message: ProtocolMessage) -> Vec<Action> {
        let source_entry = match self.peers.get(&source) {
            Some(e) => e,
            None => return vec![],
        };
        let source_direction = source_entry.direction;
        let source_mode = source_entry.mode;

        match message {
            ProtocolMessage::Inv { modifier_type, ids } => {
                // Record in Inv table
                for id in &ids {
                    self.inv_table.record(*id, source);
                }

                // Forward to all peers of opposite direction (minus source)
                let target_direction = match source_direction {
                    Direction::Outbound => Direction::Inbound,
                    Direction::Inbound => Direction::Outbound,
                };

                self.peers.iter()
                    .filter(|(pid, entry)| **pid != source && entry.direction == target_direction)
                    .map(|(pid, _)| Action::Send {
                        target: *pid,
                        message: ProtocolMessage::Inv {
                            modifier_type,
                            ids: ids.clone(),
                        },
                    })
                    .collect()
            }

            ProtocolMessage::ModifierRequest { modifier_type, ids } => {
                let mut actions = Vec::new();
                for id in &ids {
                    if let Some(target) = self.inv_table.lookup(id) {
                        if target != source {
                            self.request_tracker.record(*id, source);
                            actions.push(Action::Send {
                                target,
                                message: ProtocolMessage::ModifierRequest {
                                    modifier_type,
                                    ids: vec![*id],
                                },
                            });
                        }
                    }
                }
                actions
            }

            ProtocolMessage::ModifierResponse { modifier_type, modifiers } => {
                let mut actions = Vec::new();
                for (id, data) in &modifiers {
                    if let Some(requester) = self.request_tracker.fulfill(id) {
                        actions.push(Action::Send {
                            target: requester,
                            message: ProtocolMessage::ModifierResponse {
                                modifier_type,
                                modifiers: vec![(*id, data.clone())],
                            },
                        });
                    }
                }
                actions
            }

            ProtocolMessage::SyncInfo { body } => {
                // Only forward in full proxy mode
                if source_mode == ProxyMode::Light {
                    return vec![];
                }

                match source_direction {
                    Direction::Inbound => {
                        // Forward to an outbound peer (pick one not already syncing)
                        if let Some((&outbound_id, _)) = self.peers.iter()
                            .find(|(pid, entry)| {
                                entry.direction == Direction::Outbound
                                    && self.sync_tracker.inbound_for(pid).is_none()
                            })
                        {
                            self.sync_tracker.pair(source, outbound_id);
                            vec![Action::Send {
                                target: outbound_id,
                                message: ProtocolMessage::SyncInfo { body },
                            }]
                        } else {
                            vec![]
                        }
                    }
                    Direction::Outbound => {
                        // Forward to the inbound peer that initiated sync
                        if let Some(inbound) = self.sync_tracker.inbound_for(&source) {
                            vec![Action::Send {
                                target: inbound,
                                message: ProtocolMessage::SyncInfo { body },
                            }]
                        } else {
                            vec![]
                        }
                    }
                }
            }

            ProtocolMessage::GetPeers => {
                // Respond directly with known peers (empty Peers body for now —
                // actual peer serialization will be added when we wire to transport)
                vec![Action::Send {
                    target: source,
                    message: ProtocolMessage::Peers { body: vec![] },
                }]
            }

            ProtocolMessage::Peers { body } => {
                // Peer discovery — handled by the connection manager (outside router)
                // Router just passes through
                vec![]
            }

            ProtocolMessage::Unknown { code, body } => {
                // Forward to all peers of opposite direction
                let target_direction = match source_direction {
                    Direction::Outbound => Direction::Inbound,
                    Direction::Inbound => Direction::Outbound,
                };

                self.peers.iter()
                    .filter(|(pid, entry)| **pid != source && entry.direction == target_direction)
                    .map(|(pid, _)| Action::Send {
                        target: *pid,
                        message: ProtocolMessage::Unknown { code, body: body.clone() },
                    })
                    .collect()
            }
        }
    }

    /// Get all outbound peer IDs (for keepalive scheduling).
    pub fn outbound_peers(&self) -> Vec<PeerId> {
        self.peers.iter()
            .filter(|(_, e)| e.direction == Direction::Outbound)
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Get all inbound peer IDs.
    pub fn inbound_peers(&self) -> Vec<PeerId> {
        self.peers.iter()
            .filter(|(_, e)| e.direction == Direction::Inbound)
            .map(|(pid, _)| *pid)
            .collect()
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}
```

- [ ] **Step 4: Update src/routing/mod.rs**

```rust
pub mod inv_table;
pub mod tracker;
pub mod router;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test --test routing_test 2>&1`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/routing/router.rs src/routing/mod.rs tests/routing_test.rs
git commit -m "feat: message router with mode filtering and tracked forwarding"
```

---

### Task 11: Main — Wire layers together

**Files:**
- Modify: `src/main.rs`

This task wires all three layers into a running proxy. The core loop: accept connections, run per-peer tasks, route messages through the router.

- [ ] **Step 1: Implement the full main.rs**

```rust
mod config;
mod types;
mod transport;
mod protocol;
mod routing;

use config::Config;
use protocol::messages::ProtocolMessage;
use protocol::peer::{PeerStateMachine, ProtocolEvent};
use routing::router::{Action, Router};
use transport::connection::Connection;
use transport::frame::Frame;
use transport::handshake::HandshakeConfig;
use types::{Direction, Network, PeerId, ProxyMode, Version};

use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};

type PeerSender = mpsc::Sender<Frame>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ergo-proxy.toml".to_string());

    let config = Config::load(&config_path)?;
    let (ver_major, ver_minor, ver_patch) = config.version_bytes()?;
    let version = Version::new(ver_major, ver_minor, ver_patch);
    let network = config.proxy.network;

    tracing::info!(network = ?network, version = %version, "Ergo proxy node starting");

    // Channel for protocol events from all peer tasks → main loop
    let (event_tx, mut event_rx) = mpsc::channel::<ProtocolEvent>(256);

    // Peer write channels: peer_id → sender for outgoing frames
    let peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Router
    let router = Arc::new(Mutex::new(Router::new()));

    // Peer ID counter
    let peer_counter = Arc::new(std::sync::atomic::AtomicU64::new(1));

    // Start listeners
    if let Some(ref listener_cfg) = config.listen.ipv6 {
        let listener = TcpListener::bind(listener_cfg.address).await?;
        tracing::info!(addr = %listener_cfg.address, mode = ?listener_cfg.mode, "IPv6 listener started");
        let mode = listener_cfg.mode;
        let max = listener_cfg.max_inbound;
        let event_tx = event_tx.clone();
        let peer_senders = peer_senders.clone();
        let router = router.clone();
        let peer_counter = peer_counter.clone();
        let hs_config = make_handshake_config(&config.identity, version, network, mode);
        tokio::spawn(accept_loop(listener, hs_config, mode, max, event_tx, peer_senders, router, peer_counter));
    }

    if let Some(ref listener_cfg) = config.listen.ipv4 {
        let listener = TcpListener::bind(listener_cfg.address).await?;
        tracing::info!(addr = %listener_cfg.address, mode = ?listener_cfg.mode, "IPv4 listener started");
        let mode = listener_cfg.mode;
        let max = listener_cfg.max_inbound;
        let event_tx = event_tx.clone();
        let peer_senders = peer_senders.clone();
        let router = router.clone();
        let peer_counter = peer_counter.clone();
        let hs_config = make_handshake_config(&config.identity, version, network, mode);
        tokio::spawn(accept_loop(listener, hs_config, mode, max, event_tx, peer_senders, router, peer_counter));
    }

    // Start outbound connections
    {
        let event_tx = event_tx.clone();
        let peer_senders = peer_senders.clone();
        let router = router.clone();
        let peer_counter = peer_counter.clone();
        let seeds = config.outbound.seed_peers.clone();
        let min_peers = config.outbound.min_peers;
        let mode = ProxyMode::Full; // outbound always full
        let hs_config = make_handshake_config(&config.identity, version, network, mode);
        tokio::spawn(outbound_manager(seeds, min_peers, hs_config, mode, event_tx, peer_senders, router, peer_counter));
    }

    // Keepalive: send GetPeers every 2 minutes
    {
        let router = router.clone();
        let peer_senders = peer_senders.clone();
        let magic = network.magic();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(120));
            loop {
                ticker.tick().await;
                let outbound = router.lock().await.outbound_peers();
                let senders = peer_senders.lock().await;
                let frame = ProtocolMessage::GetPeers.to_frame();
                for pid in outbound {
                    if let Some(tx) = senders.get(&pid) {
                        let _ = tx.send(frame.clone()).await;
                    }
                }
            }
        });
    }

    // Main event loop: receive events, route, dispatch actions
    let magic = network.magic();
    loop {
        match event_rx.recv().await {
            Some(event) => {
                let actions = router.lock().await.handle_event(event);
                let senders = peer_senders.lock().await;
                for action in actions {
                    match action {
                        Action::Send { target, message } => {
                            if let Some(tx) = senders.get(&target) {
                                let frame = message.to_frame();
                                if tx.send(frame).await.is_err() {
                                    tracing::warn!(peer = %target, "Failed to send to peer");
                                }
                            }
                        }
                    }
                }
            }
            None => {
                tracing::info!("All event senders dropped, shutting down");
                break;
            }
        }
    }

    Ok(())
}

fn make_handshake_config(
    identity: &config::IdentityConfig,
    version: Version,
    network: Network,
    mode: ProxyMode,
) -> HandshakeConfig {
    HandshakeConfig {
        agent_name: identity.agent_name.clone(),
        peer_name: identity.peer_name.clone(),
        version,
        network,
        mode,
        declared_address: None,
    }
}

async fn accept_loop(
    listener: TcpListener,
    hs_config: HandshakeConfig,
    mode: ProxyMode,
    max_inbound: usize,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
    peer_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let inbound_count = router.lock().await.inbound_peers().len();
                if inbound_count >= max_inbound {
                    tracing::warn!(addr = %addr, "Max inbound reached, rejecting");
                    continue;
                }

                let peer_id = PeerId(peer_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                tracing::info!(peer = %peer_id, addr = %addr, "Inbound connection");

                let hs = HandshakeConfig {
                    agent_name: hs_config.agent_name.clone(),
                    peer_name: hs_config.peer_name.clone(),
                    version: hs_config.version,
                    network: hs_config.network,
                    mode: hs_config.mode,
                    declared_address: hs_config.declared_address,
                };

                let event_tx = event_tx.clone();
                let peer_senders = peer_senders.clone();
                let router = router.clone();

                tokio::spawn(async move {
                    match Connection::inbound(stream, &hs).await {
                        Ok(conn) => {
                            run_peer(peer_id, conn, Direction::Inbound, mode, event_tx, peer_senders, router).await;
                        }
                        Err(e) => {
                            tracing::warn!(peer = %peer_id, error = %e, "Inbound handshake failed");
                        }
                    }
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "Accept failed");
            }
        }
    }
}

async fn outbound_manager(
    seeds: Vec<std::net::SocketAddr>,
    min_peers: usize,
    hs_config: HandshakeConfig,
    mode: ProxyMode,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
    peer_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut backoff = Duration::from_secs(5);

    loop {
        let current_outbound = router.lock().await.outbound_peers().len();
        if current_outbound < min_peers {
            for addr in &seeds {
                let current = router.lock().await.outbound_peers().len();
                if current >= min_peers {
                    break;
                }

                tracing::info!(addr = %addr, "Connecting to outbound peer");
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(addr),
                ).await {
                    Ok(Ok(stream)) => {
                        let peer_id = PeerId(peer_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                        let hs = HandshakeConfig {
                            agent_name: hs_config.agent_name.clone(),
                            peer_name: hs_config.peer_name.clone(),
                            version: hs_config.version,
                            network: hs_config.network,
                            mode: hs_config.mode,
                            declared_address: hs_config.declared_address,
                        };

                        let event_tx = event_tx.clone();
                        let peer_senders = peer_senders.clone();
                        let router = router.clone();

                        tokio::spawn(async move {
                            match Connection::outbound(stream, &hs).await {
                                Ok(conn) => {
                                    tracing::info!(peer = %peer_id, "Outbound handshake OK");
                                    run_peer(peer_id, conn, Direction::Outbound, mode, event_tx, peer_senders, router).await;
                                }
                                Err(e) => {
                                    tracing::warn!(peer = %peer_id, addr = %addr, error = %e, "Outbound handshake failed");
                                }
                            }
                        });

                        backoff = Duration::from_secs(5);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(addr = %addr, error = %e, "Connect failed");
                    }
                    Err(_) => {
                        tracing::warn!(addr = %addr, "Connect timeout");
                    }
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

async fn run_peer(
    peer_id: PeerId,
    conn: Connection,
    direction: Direction,
    mode: ProxyMode,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
) {
    let spec = conn.peer_spec().clone();
    tracing::info!(
        peer = %peer_id,
        name = %spec.name,
        agent = %spec.agent,
        version = %spec.version,
        direction = ?direction,
        "Peer active"
    );

    // Register peer in router
    router.lock().await.register_peer(peer_id, direction, mode);

    // Send PeerConnected event
    let _ = event_tx.send(ProtocolEvent::PeerConnected {
        peer_id,
        spec: spec.clone(),
        direction,
    }).await;

    // Split connection for concurrent read/write
    let (mut reader, mut writer, magic, _) = conn.split();

    // Create write channel
    let (write_tx, mut write_rx) = mpsc::channel::<Frame>(64);
    peer_senders.lock().await.insert(peer_id, write_tx);

    // Writer task
    let write_handle = tokio::spawn(async move {
        while let Some(frame) = write_rx.recv().await {
            if let Err(e) = transport::frame::write_frame(&mut writer, &magic, &frame).await {
                tracing::warn!(peer = %peer_id, error = %e, "Write failed");
                break;
            }
        }
    });

    // Reader loop
    loop {
        match transport::frame::read_frame(&mut reader, &magic).await {
            Ok(frame) => {
                match ProtocolMessage::from_frame(&frame) {
                    Ok(msg) => {
                        let event = ProtocolEvent::Message {
                            peer_id,
                            message: msg,
                        };
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer_id, error = %e, "Message parse failed");
                    }
                }
            }
            Err(e) => {
                tracing::info!(peer = %peer_id, error = %e, "Connection lost");
                break;
            }
        }
    }

    // Cleanup
    peer_senders.lock().await.remove(&peer_id);
    write_handle.abort();

    let _ = event_tx.send(ProtocolEvent::PeerDisconnected {
        peer_id,
        reason: "connection closed".into(),
    }).await;

    tracing::info!(peer = %peer_id, "Peer removed");
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo build 2>&1`
Expected: Compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire all layers into running proxy"
```

---

### Task 12: Contract documents

**Files:**
- Create: `contracts/transport.md`
- Create: `contracts/protocol.md`
- Create: `contracts/routing.md`

- [ ] **Step 1: Create contracts/transport.md**

```markdown
# Transport Layer Contract

## Module: `transport::frame`

### `encode(magic, frame) -> Vec<u8>`
- **Precondition**: `magic` is 4 bytes; `frame.body.len() <= 256KB`.
- **Postcondition**: Output is `[magic:4][code:1][length:4 BE][checksum:4][body:N]` where checksum = first 4 bytes of blake2b256(body).
- **Invariant**: `decode(magic, encode(magic, frame)) == Ok(frame)`.

### `decode(magic, data) -> Result<Frame>`
- **Precondition**: `data.len() >= 13`.
- **Postcondition**: Magic matches, checksum valid, body length matches.

### `read_frame(reader, magic) -> Result<Frame>`
- **Precondition**: Reader positioned at frame start.
- **Postcondition**: Returns validated Frame; reader advanced past frame.

### `write_frame(writer, magic, frame) -> Result<()>`
- **Postcondition**: Full encoded frame written and flushed.

## Module: `transport::handshake`

### `build(config) -> Vec<u8>`
- **Postcondition**: Raw handshake bytes containing: VLQ timestamp, agent name, version (3 bytes), peer name, declared address, Mode feature, Session feature. Feature body lengths encoded as u16 big-endian.

### `parse(data) -> Result<PeerSpec>`
- **Precondition**: Valid Ergo handshake payload.
- **Postcondition**: All fields populated. Unknown feature IDs preserved.

### `validate_peer(spec, network) -> Result<()>`
- **Postcondition**: Ok if version >= 4.0.100 AND session magic matches network.

## Module: `transport::connection`

### `Connection::outbound(stream, config) -> Result<Connection>`
- **Precondition**: `stream` is connected.
- **Postcondition**: Handshake exchanged (send then receive), peer validated.

### `Connection::inbound(stream, config) -> Result<Connection>`
- **Precondition**: `stream` is connected.
- **Postcondition**: Handshake exchanged (receive then send), peer validated.

## Invariant
The transport layer never interprets message content. It does not know what any message code means.
```

- [ ] **Step 2: Create contracts/protocol.md**

```markdown
# Protocol Layer Contract

## Module: `protocol::messages`

### `ProtocolMessage::from_frame(frame) -> Result<ProtocolMessage>`
- **Precondition**: Frame has verified envelope (valid magic, checksum).
- **Postcondition**: Returns typed message. Unknown codes → `Unknown` variant (not dropped). SyncInfo body is opaque.
- **Invariant**: `from_frame(&msg.to_frame()) ≈ msg` (roundtrip, modulo re-serialization).

### `ProtocolMessage::to_frame() -> Frame`
- **Postcondition**: Frame can be passed to transport for sending.

## Module: `protocol::peer`

### State Machine
```
Connecting → Handshaking → Active → Disconnected
                 ↓
             Failed
```

### `set_active(spec) -> ProtocolEvent::PeerConnected`
- **Precondition**: State is Handshaking.
- **Postcondition**: State is Active.

### `set_disconnected(reason) -> ProtocolEvent::PeerDisconnected`
- **Precondition**: State is Active.
- **Postcondition**: State is Disconnected.

### `message_event(msg) -> ProtocolEvent::Message`
- **Precondition**: State is Active.

## Invariant
Every ProtocolEvent is associated with a valid, handshaken peer. No events leak from non-Active peers.
```

- [ ] **Step 3: Create contracts/routing.md**

```markdown
# Routing Layer Contract

## Module: `routing::inv_table`

### `record(modifier_id, peer)`
- **Postcondition**: `lookup(modifier_id) == Some(peer)`.

### `lookup(modifier_id) -> Option<PeerId>`
- Returns the peer that most recently announced this modifier.

### `purge_peer(peer)`
- **Postcondition**: No entry maps to `peer`.
- **Invariant**: The Inv table never contains entries for disconnected peers.

## Module: `routing::tracker`

### RequestTracker

#### `record(modifier_id, requester)`
- **Postcondition**: `lookup(modifier_id) == Some(requester)`.

#### `fulfill(modifier_id) -> Option<PeerId>`
- **Postcondition**: Entry is removed. Returns the requester.

#### `purge_peer(peer)`
- **Postcondition**: No entry maps to `peer`.

### SyncTracker

#### `pair(inbound, outbound)`
- **Postcondition**: `outbound_for(inbound) == Some(outbound)` AND `inbound_for(outbound) == Some(inbound)`.
- **Invariant**: Pairings are bidirectionally consistent.

#### `purge_peer(peer)`
- **Postcondition**: No pairing references `peer` on either side.

## Module: `routing::router`

### `handle_event(event) -> Vec<Action>`
- **Precondition**: Peer IDs in events are registered (or being disconnected).
- **Postcondition**: Actions target only registered peers.
- Mode filtering: Light mode drops SyncInfo and block-related ModifierRequests.
- GetPeers: responded to directly from peer registry.
- Inv: forwarded to all peers of opposite direction.
- ModifierRequest: routed via Inv table to the announcing peer.
- ModifierResponse: routed via request tracker to the requester.
- SyncInfo: routed via sync tracker (inbound↔outbound pairing).
- Unknown: forwarded to all peers of opposite direction.

## Invariant
Inv table, request tracker, and sync tracker are consistent with the peer registry. No references to unregistered peers.
```

- [ ] **Step 4: Commit**

```bash
git add contracts/
git commit -m "docs: Design by Contract specifications for all layers"
```

---

### Task 13: Integration test

**Files:**
- Create: `tests/integration_test.rs`

- [ ] **Step 1: Write integration test**

```rust
//! Integration test: two mock peers communicating through the router.
//! This tests the routing logic end-to-end without real TCP connections.

use ergo_proxy_node::protocol::messages::ProtocolMessage;
use ergo_proxy_node::protocol::peer::ProtocolEvent;
use ergo_proxy_node::routing::router::{Action, Router};
use ergo_proxy_node::types::{Direction, PeerId, ProxyMode};

/// Full scenario: outbound announces tx, inbound requests it, outbound delivers.
#[test]
fn full_tx_relay_scenario() {
    let mut router = Router::new();

    // Register peers
    let outbound = PeerId(1);
    let inbound = PeerId(2);
    router.register_peer(outbound, Direction::Outbound, ProxyMode::Full);
    router.register_peer(inbound, Direction::Inbound, ProxyMode::Full);

    let tx_id = [0x42; 32];

    // Step 1: Outbound peer sends Inv (announces a transaction)
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: outbound,
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![tx_id] },
    });
    // Should forward Inv to inbound peer
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, message }
        if *target == inbound && matches!(message, ProtocolMessage::Inv { .. })
    ));

    // Step 2: Inbound peer sends ModifierRequest (wants the tx)
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: inbound,
        message: ProtocolMessage::ModifierRequest { modifier_type: 2, ids: vec![tx_id] },
    });
    // Should route to outbound peer
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, .. } if *target == outbound));

    // Step 3: Outbound peer sends ModifierResponse (delivers the tx)
    let tx_data = vec![0xde, 0xad, 0xbe, 0xef];
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: outbound,
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 2,
            modifiers: vec![(tx_id, tx_data.clone())],
        },
    });
    // Should route to inbound peer (the requester)
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, message }
        if *target == inbound && matches!(message, ProtocolMessage::ModifierResponse { .. })
    ));
}

/// Verify disconnect cleanup: no actions should target a disconnected peer.
#[test]
fn disconnect_cleanup_scenario() {
    let mut router = Router::new();
    let out1 = PeerId(1);
    let out2 = PeerId(2);
    let inb = PeerId(3);
    router.register_peer(out1, Direction::Outbound, ProxyMode::Full);
    router.register_peer(out2, Direction::Outbound, ProxyMode::Full);
    router.register_peer(inb, Direction::Inbound, ProxyMode::Full);

    let tx_id = [0x55; 32];

    // out1 announces
    router.handle_event(ProtocolEvent::Message {
        peer_id: out1,
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![tx_id] },
    });

    // out1 disconnects
    router.handle_event(ProtocolEvent::PeerDisconnected {
        peer_id: out1,
        reason: "gone".into(),
    });

    // inbound requests the tx — should get nothing (out1 gone, out2 never announced)
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: inb,
        message: ProtocolMessage::ModifierRequest { modifier_type: 2, ids: vec![tx_id] },
    });
    assert!(actions.is_empty());
}

/// Multiple inbound peers: Inv from outbound should fan out to all inbound peers.
#[test]
fn inv_fanout_scenario() {
    let mut router = Router::new();
    let outbound = PeerId(1);
    router.register_peer(outbound, Direction::Outbound, ProxyMode::Full);

    for i in 2..=5 {
        router.register_peer(PeerId(i), Direction::Inbound, ProxyMode::Full);
    }

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: outbound,
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });

    // Should produce 4 actions (one per inbound peer)
    assert_eq!(actions.len(), 4);
}

/// Light mode: SyncInfo from inbound should be dropped.
#[test]
fn light_mode_blocks_sync() {
    let mut router = Router::new();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Light);

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo { body: vec![1, 2, 3] },
    });
    assert!(actions.is_empty());
}

/// Full mode: SyncInfo flows through sync tracker.
#[test]
fn full_mode_sync_flow() {
    let mut router = Router::new();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // Inbound sends SyncInfo
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::SyncInfo { body: vec![1, 2, 3] },
    });
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, .. } if *target == PeerId(1)));

    // Outbound responds with SyncInfo
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::SyncInfo { body: vec![4, 5, 6] },
    });
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::Send { target, .. } if *target == PeerId(2)));
}
```

- [ ] **Step 2: Run all tests**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test 2>&1`
Expected: All tests pass (unit + integration).

- [ ] **Step 3: Commit**

```bash
git add tests/integration_test.rs
git commit -m "test: integration tests for full relay scenarios"
```

---

### Task 14: Build release and verify

- [ ] **Step 1: Build release binary**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo build --release 2>&1`
Expected: Compiles with no errors. Binary at `target/release/ergo-proxy-node`.

- [ ] **Step 2: Run all tests one final time**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && cargo test 2>&1`
Expected: All tests pass.

- [ ] **Step 3: Verify binary starts and reads config**

Run: `cd /home/mwaddip/projects/ergo-proxy-node && RUST_LOG=info timeout 3 ./target/release/ergo-proxy-node ergo-proxy.toml 2>&1 || true`
Expected: Logs showing "Ergo proxy node starting" and listener bind attempts. Will fail to connect to peers (we're not on testnet) but should not crash.

- [ ] **Step 4: Final commit**

```bash
git add -A
git commit -m "chore: verify release build"
```

# Ergo Proxy Node — Design Spec

## Overview

A lightweight Ergo P2P relay that connects to the Ergo network as a peer, accepts inbound connections, and forwards messages between them. No blockchain state, no validation, no mining — pure message routing with tracked forwarding.

Primary value: IPv6 bridge. The Ergo network is IPv4-only. This proxy listens on IPv6, enabling IPv6-only hosts (e.g., BlockHost VMs) to participate as if they were connected to the IPv4 network directly. An IPv6 peer connecting through this proxy can fully sync, not just receive gossip.

Secondary value: connection stability. Ergo testnet has few peers with aggressive rate-limiting. A persistent relay reduces connection churn for multiple clients.

Future value: the transport and protocol layers are designed as foundation components for a future non-JVM Ergo node implementation.

## Language

Rust. Chosen for: memory safety at the network boundary, zero-cost abstractions, tokio async ecosystem for concurrent connections, existing Ergo library support (sigma-rust, ergo-lib), and suitability as foundation code for a future full node.

## Architecture

Three-layer architecture with Design by Contract at each boundary.

```
┌─────────────────────────────────────────────────┐
│                   Routing                        │
│  Inv table, request tracking, mode filtering,    │
│  peer registry, forwarding decisions             │
├─────────────────────────────────────────────────┤
│                   Protocol                       │
│  Message parsing, peer state machine,            │
│  lifecycle events, version enforcement           │
├─────────────────────────────────────────────────┤
│                   Transport                      │
│  TCP streams, frame encoding/decoding,           │
│  handshake, checksum verification                │
└─────────────────────────────────────────────────┘
```

Each layer communicates via typed channels. Each layer boundary is a contract surface documented in `contracts/`.

## Layer 1: Transport

### Responsibility

Own TCP streams. Convert between raw bytes and validated message frames. Perform handshake.

### Contract

- **Precondition**: Given a connected `TcpStream` and a 4-byte network magic.
- **Postcondition (read)**: Produces `Frame { code: u8, body: Vec<u8> }` where magic is verified and blake2b256 checksum is valid, or a `TransportError`.
- **Postcondition (write)**: Given a code and body, produces correctly framed bytes on the wire: `[magic:4][code:1][length:4 BE][checksum:4][body:N]`.
- **Invariant**: The transport never interprets message content. It does not know what any message code means.

### Handshake

The handshake is a raw-bytes exchange (no message framing) at connection start. It is the one place transport touches protocol semantics, but it is a bounded, one-time operation.

**Handshake payload format** (sent raw, both directions):
```
[timestamp: VLQ u64]           — milliseconds since epoch
[agent_name: u8 len + UTF-8]   — e.g., "ergo-proxy"
[version: 3 bytes]              — e.g., [6, 0, 3]
[peer_name: u8 len + UTF-8]    — e.g., "ergo-proxy-node"
[declared_address: Option]      — 0x00 if None; if Some: 0x01 + [addr_len:1][ip:N][port:4 BE]
[feature_count: u8]
[features: repeated]            — [id:1][body_len:2 BE u16][body:N]
```

**Features sent by the proxy**:
- Mode (id=16): configurable per listener — full or NiPoPoW-light
- Session (id=3): `[magic:4][session_id:8]`

**Mode feature body encoding**:
- Full mode: `[0x00, 0x01, 0x00, 0x01]` — UTXO state, verifying, no NiPoPoW, all blocks kept
- Light mode: `[0x00, 0x01, 0x01, 0x02, 0x01]` — UTXO state, verifying, NiPoPoW-bootstrapped, all blocks kept

**Handshake validation**:
- Peer version must be >= 4.0.100 (EIP-37). Below this: reject.
- Session feature magic must match network. Mismatch: reject.

### Message frame format

```
[magic: 4 bytes]              — network identifier
[code: 1 byte]                — message type
[body_length: 4 bytes BE]     — unsigned, max 256KB
[checksum: 4 bytes]           — first 4 bytes of blake2b256(body)
[body: N bytes]               — message payload
```

### Types

```rust
struct Frame { code: u8, body: Vec<u8> }

struct PeerSpec {
    agent: String,
    version: Version,
    name: String,
    address: Option<SocketAddr>,
    features: Vec<Feature>,
}

enum TransportError {
    Io(std::io::Error),
    BadMagic { expected: [u8; 4], got: [u8; 4] },
    ChecksumMismatch,
    Oversized { size: u32, max: u32 },
    HandshakeFailed(String),
}
```

### Network magic bytes

- Mainnet: `[1, 0, 2, 4]`
- Testnet: `[2, 3, 2, 3]`

## Layer 2: Protocol

### Responsibility

Parse frame bodies into typed protocol messages. Manage peer lifecycle. Enforce protocol rules.

### Contract

- **Precondition**: Receives `Frame` from transport with verified envelope.
- **Postcondition**: Produces typed `ProtocolEvent` — a parsed message with originating peer ID, or a lifecycle event.
- **Invariant**: Every `ProtocolEvent` is associated with a valid, handshaken peer. No events leak from peers that have not completed handshake.

### Message types

| Code | Name | Parsed structure |
|------|------|-----------------|
| 1 | GetPeers | (empty) |
| 2 | Peers | `Vec<PeerSpec>` |
| 22 | ModifierRequest | `modifier_type: u8, ids: Vec<[u8; 32]>` |
| 33 | ModifierResponse | `modifier_type: u8, modifiers: Vec<(ModifierId, Vec<u8>)>` |
| 55 | Inv | `modifier_type: u8, ids: Vec<[u8; 32]>` |
| 65 | SyncInfo | `body: Vec<u8>` (opaque — proxy does not parse chain state) |
| * | Unknown | `code: u8, body: Vec<u8>` (forward-compatible pass-through) |

**SyncInfo is opaque by design.** The proxy forwards it without parsing. This avoids coupling to chain state structures and survives protocol changes.

**Unknown messages are forwarded, not dropped.** This makes the proxy resilient to protocol additions.

### Peer lifecycle state machine

```
Connecting → Handshaking → Active → Disconnected
                 ↓
             Failed
```

**State transition contracts**:
- `Connecting → Handshaking`: TCP connection established.
- `Handshaking → Active`: Handshake received, version >= 4.0.100, session magic matches.
- `Handshaking → Failed`: Version too low, magic mismatch, parse error, timeout (30s).
- `Active → Disconnected`: TCP closed, read error, or explicit disconnect.
- A peer can only produce `ProtocolEvent`s while in `Active` state.

### Protocol events

```rust
enum ProtocolEvent {
    PeerConnected { peer_id: PeerId, spec: PeerSpec, direction: Direction },
    PeerDisconnected { peer_id: PeerId, reason: String },
    Message { peer_id: PeerId, message: ProtocolMessage },
}

enum Direction { Inbound, Outbound }
```

## Layer 3: Routing

### Responsibility

Decide where messages go. Maintain the Inv table and request tracker. Manage peer sets. Apply mode-based filtering.

### Contract

- **Precondition**: Receives `ProtocolEvent` with a valid peer ID.
- **Postcondition**: Produces `Action` directives — send a message to a specific peer or set of peers.
- **Invariant**: The Inv table never contains entries for disconnected peers. Every modifier request is routed to a peer that announced having that modifier. Every modifier response is routed to the peer that requested it.

### Inv table

```rust
HashMap<ModifierId, PeerId>  // who announced this modifier
```

- `Inv` from peer A → record that peer A has these modifier IDs.
- `ModifierRequest` from peer B → look up the peer that has the requested IDs, forward request to that peer.
- Peer disconnect → purge all entries for that peer.

### Request tracker

```rust
HashMap<ModifierId, PeerId>  // who requested this modifier (awaiting response)
```

- When forwarding a `ModifierRequest` outbound: record the requesting inbound peer.
- When `ModifierResponse` arrives: look up who requested it, forward to them, remove entry.
- Peer disconnect → purge pending requests from/to that peer.

### SyncInfo tracker

```rust
HashMap<PeerId, PeerId>  // inbound peer → outbound peer handling their sync
```

- When forwarding `SyncInfo` from an inbound peer to an outbound peer: record the pairing.
- When the outbound peer sends a response (Inv with header IDs, or its own SyncInfo): forward to the paired inbound peer.
- The resulting ModifierRequest/Response flow for headers follows normal Inv table routing.
- Peer disconnect (either side) → remove the pairing.

### Mode-based filtering

Configurable per listener:

**Full proxy mode** (intended for IPv6):
- Forward all message types: Inv, ModifierRequest, ModifierResponse, SyncInfo, GetPeers/Peers, Unknown.
- Advertise Mode feature as full archival node.

**Light mode** (intended for IPv4):
- Gossip only: Inv relay, tx broadcast, GetPeers/Peers.
- Do not forward SyncInfo or block-related modifier requests.
- Advertise Mode feature as NiPoPoW-bootstrapped.

### Message handling rules

| Message | Source | Action |
|---------|--------|--------|
| Inv | Outbound peer | Record in Inv table. Forward to all inbound peers (minus source). |
| Inv | Inbound peer | Record in Inv table. Forward to all outbound peers (minus source). |
| ModifierRequest | Inbound peer | Look up Inv table for target peer. Forward to that peer. Record in request tracker. |
| ModifierRequest | Outbound peer | Look up Inv table. Forward to the peer that has it. Record in request tracker. |
| ModifierResponse | Any peer | Look up request tracker for requester. Forward to requester. Remove from tracker. |
| SyncInfo | Inbound peer (full mode) | Forward to an outbound full node. |
| SyncInfo | Outbound peer | Forward to the inbound peer that initiated sync (tracked like modifier requests). |
| GetPeers | Any peer | Respond directly from peer registry. |
| Peers | Any peer | Update peer registry with discovered peers. |
| Unknown | Any peer | Forward to all peers of opposite direction (minus source). |

### Keepalive

Send GetPeers to each outbound peer every 2 minutes. This serves dual purpose: peer discovery and preventing the 10-minute inactivity disconnect.

### Outbound connection management

- Maintain `min_peers` to `max_peers` persistent outbound connections.
- Start with `seed_peers` from config.
- Discover additional peers via GetPeers/Peers exchange.
- Reconnect on drop with exponential backoff (30s, 60s, 120s, 240s, max 300s).
- Persist discovered peers to disk for faster restart.

## Configuration

```toml
[proxy]
network = "testnet"                # or "mainnet"

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

Each listener is independently configurable. The mode setting controls both the advertised Mode feature in the handshake and the routing behavior for peers connected through that listener.

## Crate structure

```
ergo-proxy-node/
  src/
    transport/        — framing, handshake, TCP management
      mod.rs
      frame.rs        — Frame encode/decode, checksum
      handshake.rs    — Handshake build/parse, PeerSpec
      connection.rs   — Per-connection read/write loops
    protocol/         — message parsing, peer state machine
      mod.rs
      messages.rs     — Typed message structs, parse from Frame
      peer.rs         — Peer state machine, lifecycle
    routing/          — forwarding logic
      mod.rs
      inv_table.rs    — Inv table with peer-disconnect cleanup
      tracker.rs      — Request tracker (who asked for what)
      router.rs       — Message routing decisions, mode filtering
    config.rs         — TOML config parsing
    main.rs           — tokio runtime, listener setup, layer wiring
  contracts/          — layer boundary specifications (markdown)
    transport.md
    protocol.md
    routing.md
  Cargo.toml
```

## Dependencies

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
blake2 = "0.10"
serde = { version = "1", features = ["derive"] }
toml = "0.8"
tracing = "0.1"
tracing-subscriber = "0.3"
```

No ergo-specific crates needed. The proxy forwards opaque bytes — it does not need to parse transactions, evaluate scripts, or interact with the blockchain. If future needs arise (e.g., validating modifier types), sigma-rust crates can be added then.

## Error handling

- Transport errors (bad magic, checksum, oversized): drop the frame, log warning, increment penalty counter. Disconnect after threshold.
- Protocol errors (unparseable message body): drop the message, log, do not disconnect (forward-compat: might be a newer message format).
- Routing errors (modifier not in Inv table): log, do not forward. The requesting peer will timeout and retry with another peer.
- Connection errors (TCP reset, timeout): mark peer as disconnected, trigger Inv table and request tracker cleanup, schedule reconnect if outbound.

## Testing strategy

**Unit tests**: Each layer tested independently with mock inputs. Inv table invariants verified. Peer state machine transitions tested exhaustively.

**Integration test**: Run proxy locally, connect a mock inbound peer and a mock outbound peer, verify message forwarding end-to-end.

**Live test**: Deploy to testnet. Point proxy at known testnet peers as outbound. Connect a JVM node or ergo-relay as inbound through the proxy. Submit a transaction through the inbound peer. Verify it reaches the network (appears in explorer).

**Success criteria**: A transaction submitted by an IPv6-only client, relayed through the proxy to an IPv4 testnet peer, appears in the Ergo testnet explorer.

## Protocol reference

All protocol knowledge derived from:
- JVM reference node source (`ergoplatform/ergo`, v6.0.3 branch)
- ergo-relay implementation (`~/projects/blockhost-ergo/ergo-relay/src/p2p.rs`)
- tcpdump capture of JVM node handshake on testnet (2026-03-30)
- ErgoDocs P2P documentation (`docs.ergoplatform.com/dev/p2p/`)

No formal protocol specification exists. The JVM node source is the de facto spec.

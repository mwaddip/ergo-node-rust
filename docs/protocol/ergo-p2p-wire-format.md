# Ergo P2P Wire Format Specification

**Version:** 1.0 (documenting protocol version 6.0.x)
**Status:** Reverse-engineered from JVM reference node (ergoplatform/ergo v6.0.3) and verified against pcap captures. No official specification exists.
**Date:** 2026-03-31

## Overview

The Ergo P2P protocol is a TCP-based binary protocol for communication between Ergo blockchain nodes. It uses the Scorex serialization framework for all encoding. Connections are persistent, full-duplex TCP streams.

A connection begins with a handshake exchange (raw bytes, no framing), followed by framed messages for the duration of the connection.

## Scorex Serialization Primitives

All integer serialization in the Ergo P2P protocol uses the Scorex VLQ (Variable-Length Quantity) framework. This is critical and undocumented — the ErgoDocs describe fixed-width encodings, but the actual wire format uses VLQ for all integer types.

### VLQ Unsigned Integer

7 data bits per byte, MSB is continuation flag. Little-endian byte order (least significant group first).

```
Value 0:      [0x00]                          (1 byte)
Value 127:    [0x7F]                          (1 byte)
Value 128:    [0x80, 0x01]                    (2 bytes)
Value 9023:   [0xBF, 0x46]                    (2 bytes)
Value 16384:  [0x80, 0x80, 0x01]              (3 bytes)
```

Encoding algorithm:
```
while value > 0x7F:
    emit (value & 0x7F) | 0x80
    value >>= 7
emit value & 0x7F
```

Maximum: 10 bytes (encodes up to 2^63-1 safely; 2^64-1 theoretically).

### ZigZag + VLQ (Signed Integer)

Signed integers are ZigZag-encoded before VLQ encoding.

```
ZigZag(n) = (n << 1) ^ (n >> 31)     for i32
ZigZag(n) = (n << 1) ^ (n >> 63)     for i64
```

Maps: 0→0, -1→1, 1→2, -2→3, 2→4, ...

```
Value -1:   ZigZag → 1,   VLQ → [0x01]
Value 1:    ZigZag → 2,   VLQ → [0x02]
Value -2:   ZigZag → 3,   VLQ → [0x03]
```

### Scorex Type Mapping

| Scorex method | Wire encoding | Notes |
|---|---|---|
| `put(byte)` | 1 raw byte | |
| `putBytes(arr)` | raw bytes | No length prefix |
| `putShortString(s)` | `[len:1 byte][utf8:len bytes]` | Max 255 bytes |
| `putUShort(i)` | VLQ unsigned | Despite the name, NOT 2-byte BE |
| `putUInt(i)` | VLQ unsigned | Despite the name, NOT 4-byte BE |
| `putULong(i)` | VLQ unsigned | |
| `putInt(i)` | ZigZag + VLQ | |
| `putLong(i)` | ZigZag + VLQ | |
| `putOption(opt)` | `0x00` if None; `0x01` + body if Some | |

**This table is the single most important thing in this document.** The ErgoDocs and even the Scorex trait names suggest fixed-width encoding. The actual implementation uses VLQ for everything.

## Handshake

The handshake is the first data exchanged on a new TCP connection. It is sent as raw bytes (no message framing). Both sides send a handshake; the initiator sends first, the responder sends after receiving and validating.

### Handshake Format

```
[timestamp]          putULong         VLQ unsigned milliseconds since epoch
[agent_name]         putShortString   1-byte length + UTF-8
[version]            3 raw bytes      major, minor, patch
[peer_name]          putShortString   1-byte length + UTF-8
[declared_address]   putOption        see Address Format below
[feature_count]      put(byte)        raw byte, number of features
[features...]                         repeated, see Feature Format below
```

### Address Format (inside putOption)

When present (option tag = 0x01):

```
[addr_size]    put(byte)       ip_bytes.len + 4 (8 for IPv4, 20 for IPv6)
[ip_bytes]     putBytes        4 bytes (IPv4) or 16 bytes (IPv6)
[port]         putUInt         VLQ unsigned (NOT 4-byte big-endian)
```

The `addr_size` field is `ip_length + 4`. The `4` is a historical artifact — it does NOT mean 4 bytes for the port (the port is VLQ, which can be 1-5 bytes). The parser uses `ip_length = addr_size - 4` to determine how many bytes to read for the IP address.

IPv6 addresses are supported at the wire level. `InetAddress.getByAddress()` on the JVM creates the correct address type based on byte array length (4 → IPv4, 16 → IPv6).

### Feature Format

```
[feature_id]      put(byte)     raw byte, feature identifier
[body_length]     putUShort     VLQ unsigned (NOT 2-byte big-endian)
[body]            putBytes      body_length raw bytes
```

### Known Feature IDs

| ID | Name | Description |
|---|---|---|
| 2 | LocalAddress | Local/loopback address (hardcoded to IPv4 in JVM) |
| 3 | Session | Network magic + session ID for self-connection detection |
| 4 | RestApiUrl | Public REST API URL |
| 16 | Mode | Node capabilities (state type, verification, NiPoPoW, block retention) |
| 64 | Proxy | ergo-proxy-node identifier (custom, ignored by JVM nodes) |

### Session Feature (ID 3)

```
[magic]        putBytes(4)    network magic bytes
[session_id]   putLong        ZigZag + VLQ encoded random i64
```

Session body length is variable (5-14 bytes) because the session ID is ZigZag+VLQ encoded.

### Mode Feature (ID 16)

```
[state_type]              put(byte)       0x00 = UTXO, 0x01 = Digest
[verifying_transactions]  put(byte)       0x00 = false, 0x01 = true
[nipopow_bootstrapped]    putOption       0x00 = None (full headers)
                                          0x01 + putInt(value) = Some (NiPoPoW)
[blocks_to_keep]          putInt          ZigZag + VLQ signed
```

`blocks_to_keep` values:
- `-1` (ZigZag→1, VLQ→`0x01`): all blocks kept (archival)
- `-2` (ZigZag→3, VLQ→`0x03`): UTXO snapshot bootstrapped
- Positive N: pruned, keeping last N blocks

Common Mode bodies:
```
Full archival:    [0x00, 0x01, 0x00, 0x01]          (4 bytes)
NiPoPoW light:   [0x00, 0x01, 0x01, 0x02, 0x01]    (5 bytes)
UTXO snapshot:   [0x00, 0x01, 0x00, 0x03]           (4 bytes)
```

### Handshake Validation

A peer MUST be rejected if:
- Protocol version < 4.0.100 (EIP-37 hard fork minimum)
- Session feature magic does not match the expected network

Rejection results in a permanent ban (~3600 days) in the JVM reference node.

## Network Magic

```
Mainnet:  [0x01, 0x00, 0x02, 0x04]
Testnet:  [0x02, 0x03, 0x02, 0x03]    (changed Feb 2026 testnet reset)
```

The magic bytes appear in:
1. Session feature body (handshake)
2. Every framed message header

## Message Framing

After handshake, all communication uses framed messages.

### Frame Format

```
[magic]        4 raw bytes              network magic
[code]         1 raw byte               message type identifier
[body_length]  4 bytes big-endian u32   NOT VLQ (this is raw big-endian)
[checksum]     4 raw bytes              first 4 bytes of blake2b256(body)
[body]         body_length raw bytes    message payload
```

**Note:** The frame header uses fixed-width big-endian for `body_length`, unlike everything in the handshake which uses VLQ. This is because framing is implemented directly in the network layer, not through the Scorex serialization framework.

Maximum body size: 256 KB (enforced by well-behaved implementations).

### Checksum

Blake2b-256 hash of the body, truncated to the first 4 bytes. Computed over the raw body bytes. Empty bodies hash to blake2b256 of the empty byte array.

## Message Types

### GetPeers (code 1)

Request peer list. Empty body.

```
Body: (empty)
```

### Peers (code 2)

Response to GetPeers. Contains a list of peer specifications.

```
[count]        putUInt          VLQ unsigned, number of peers
[peers...]                     repeated PeerSpec (same format as handshake, without timestamp)
```

Each peer entry:
```
[agent_name]         putShortString
[version]            3 raw bytes
[peer_name]          putShortString
[declared_address]   putOption (Address Format)
[feature_count]      put(byte)
[features...]        repeated Feature Format
```

### Inv (code 55)

Announce possession of modifiers (blocks, transactions, headers).

```
[modifier_type]   put(byte)     type ID (see Modifier Types)
[count]           putUInt       VLQ unsigned, number of IDs
[ids...]          putBytes(32)  repeated 32-byte modifier IDs
```

### RequestModifier (code 22)

Request specific modifiers by ID. Same format as Inv.

```
[modifier_type]   put(byte)
[count]           putUInt       VLQ unsigned
[ids...]          putBytes(32)  repeated 32-byte modifier IDs
```

### ModifierResponse (code 33)

Deliver requested modifiers.

```
[modifier_type]   put(byte)
[count]           putUInt       VLQ unsigned
[modifiers...]                  repeated:
  [id]            putBytes(32)  32-byte modifier ID
  [data_length]   putUInt       VLQ unsigned
  [data]          putBytes      data_length raw bytes
```

### SyncInfo (code 65)

Chain synchronization state. Body format depends on protocol version and is complex (contains header IDs for chain tip comparison). Proxies should treat this as opaque and forward without parsing.

### Modifier Types

| ID | Type |
|---|---|
| 1 | Header |
| 2 | Transaction |
| 3 | BlockTransactions |
| 4 | ADProofs |
| 5 | Extension |

## Peer Management Behavior (JVM Reference Node)

These are implementation behaviors, not protocol requirements, but they affect how peers interact.

### Timeouts
- Handshake timeout: 30 seconds
- Inactivity deadline: 10 minutes (disconnect if no messages received)
- Delivery timeout: 10 seconds (for requested modifiers)

### Penalty System
- NonDeliveryPenalty: 2 points (modifier not delivered in time)
- MisbehaviorPenalty: 10 points (invalid modifier, wrong ID)
- SpamPenalty: 25 points (unrequested modifier)
- PermanentPenalty: 1,000,000,000 points (bad handshake, old version)
- Threshold: 500 points → temporary ban (60 minutes)
- Permanent penalty → ~3600 day ban

### Keepalive
GetPeers is sent every 2 minutes. This serves as keepalive to prevent the 10-minute inactivity disconnect.

### Anti-Eclipse
One random peer is disconnected every hour (if 5+ connections) to prevent eclipse attacks.

## Example: Complete Handshake (from pcap)

Captured from JVM Ergo v6.0.3 testnet node connecting to seed peer:

```
d4c5fb85d433     VLQ timestamp: 1774907744980
07               agent name length: 7
6572676f726566   "ergoref"
060003           version: 6.0.3
0f               peer name length: 15
6572676f2d746573742d6672657368   "ergo-test-fresh"
01               declared address: Some
08               addr_size: 8 (IPv4: 4 + 4)
5fb3f666         IP: 95.179.246.102
bf46             port VLQ: 9023 (0xBF=0x3F|0x80, 0x46=70 → 0x3F + 70*128 = 63+8960 = 9023)
02               feature count: 2
10               feature id: 16 (Mode)
04               body length VLQ: 4
00010001         Mode: UTXO, verifying, no NiPoPoW, all blocks
03               feature id: 3 (Session)
0e               body length VLQ: 14
02030203         magic: [2, 3, 2, 3] (testnet)
bdf8daf999fcf5b38b01   session ID: ZigZag+VLQ encoded i64

Total: 64 bytes
```

First framed message (SyncInfo):
```
02030203         magic: [2, 3, 2, 3]
41               code: 65 (SyncInfo)
00000003         body length: 3 (4-byte big-endian)
45a14b86         checksum: blake2b256([00, ff, 00])[0..4]
00ff00           body
```

## Sources

- JVM reference node: `ergoplatform/ergo` v6.0.3 branch
- Scorex serialization: `ScorexFoundation/scorex-util` v0.2.1 (decompiled bytecode)
- PeerSpec serializer: `ergo-core/src/main/scala/org/ergoplatform/network/PeerSpec.scala`
- Handshake serializer: `ergo-core/src/main/scala/org/ergoplatform/network/HandshakeSerializer.scala`
- Session feature: `ergo-core/src/main/scala/org/ergoplatform/network/peer/SessionIdPeerFeature.scala`
- Mode feature: `ergo-core/src/main/scala/org/ergoplatform/network/ModePeerFeature.scala`
- Version checks: `src/main/scala/scorex/core/network/PeerConnectionHandler.scala`
- Penalty system: `ergo-core/src/main/scala/org/ergoplatform/network/PeerSpec.scala`, `PenaltyType.scala`
- Wire capture: tcpdump of testnet handshake, 2026-03-30

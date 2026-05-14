# Protocol Layer Contract

## Module: `protocol::messages`

### `ProtocolMessage::from_frame(frame) -> Result<ProtocolMessage>`
- **Precondition**: Frame has verified envelope (valid magic, checksum).
- **Postcondition**: Returns typed message. Unknown codes produce `Unknown` variant (not dropped). SyncInfo body is opaque.
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

## Wire format: GetPeers (code 1)
- Body: empty. Any bytes in the body are a protocol violation
  (parser may accept silently for forward-compat, but JVM rejects).

## Wire format: Peers (code 2)
- Body: `length: VLQ u32` followed by `length` serialized `PeerSpec`
  entries.
- **Cap**: `length <= config.max_peer_spec_objects` (default 64).
  Bodies that declare more are a protocol violation; the parser must
  reject and the producer must trigger a permanent ban of the sender.
- Per-entry shape (same as the handshake `PeerSpec`, but with no
  leading timestamp):
  - `agentName: shortString` — VLQ length, then UTF-8 bytes.
  - `protocolVersion: 3 bytes` — `major.minor.patch`.
  - `nodeName: shortString` — VLQ length, then UTF-8 bytes.
  - `declaredAddress: Option<SocketAddr>`:
    - Option header: `0x00` for None, `0x01` for Some.
    - If Some: `addr_size_plus_4: u8` (single byte; IP byte length
      plus 4 to include the port), `addr_bytes` (4 or 16 bytes),
      `port: VLQ u32`.
  - `featuresCount: u8` (single byte; rejected if negative).
  - For each feature:
    - `featId: u8`.
    - `featBytesCount: VLQ u32` (`putUShort` is VLQ in Scorex).
    - `featChunk: bytes` of declared length. Unknown feature IDs are
      preserved as opaque (`Feature { id, body }`); parsing does not
      reject them.
- Reuse: `parse_peer_entry` in `transport::handshake` already parses
  the per-entry shape. The Peers parser wraps it in the VLQ-count
  loop and applies the cap.
- Producer-side: a paired `serialize_peer_entry` is the inverse and
  is reused for handshake output. The Peers serializer prefixes the
  VLQ count and concatenates.

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

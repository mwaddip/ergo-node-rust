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
- **Postcondition**: Raw handshake bytes containing: VLQ timestamp, agent name, version (3 bytes), peer name, declared address, Mode feature, Session feature. Feature body lengths encoded as VLQ.

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

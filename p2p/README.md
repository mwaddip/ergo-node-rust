# enr-p2p

P2P networking layer for [ergo-node-rust](https://github.com/mwaddip/ergo-node-rust), a ground-up Rust implementation of the Ergo blockchain full node.

## What this does

Handles everything between "TCP socket" and "typed protocol message":

- **Transport** -- TCP connections (inbound + outbound, IPv4 + IPv6), Ergo P2P handshake with Scorex VLQ serialization, message framing with blake2b checksums
- **Protocol** -- Message parsing into typed variants (Inv, ModifierRequest, ModifierResponse, SyncInfo, etc.), peer lifecycle state machine
- **Routing** -- Inv table, request tracking, sync pairing, message forwarding between inbound and outbound peers

The crate exposes `P2pNode` as its public API. Callers start it with a config, then use `send_to()` / `broadcast_outbound()` to send messages, and `subscribe()` to observe protocol events. The node runs as background tokio tasks.

## Role in ergo-node-rust

This is the network boundary. Every byte from a peer is untrusted until validated. The P2P layer doesn't interpret block content or validate chain state -- it routes messages and lets higher layers (chain validation, sync state machine) decide what to do with them.

```
                      ergo-node-rust (main crate)
                              |
              +---------------+---------------+
              |               |               |
           enr-p2p        enr-chain       enr-state
         (this crate)   (validation)   (UTXO/storage)
```

Started as a message-forwarding proxy (`ergo-proxy-node`). Now gradually gaining awareness of what it forwards as validation components come online. The proxy behavior is the fallback: if we can't validate it, forward it and let someone else decide.

## Architecture

Three layers, each with a documented [Design by Contract](facts/) boundary:

```
Routing    -- Inv table, request tracking, mode filtering, forwarding decisions
Protocol   -- Message parsing, peer lifecycle state machine
Transport  -- TCP streams, frame encoding, handshake, checksum verification
```

Messages flow up from transport (raw frames) through protocol (typed messages) to routing (forwarding decisions), and back down as outgoing frames.

### Proxy modes

Each listener can operate in one of two modes:

- **Full** -- forwards everything: Inv, modifiers, SyncInfo, peer exchange. Advertises as a full archival node.
- **Light** -- gossip only: Inv relay, transaction broadcast, peer exchange. Advertises as a NiPoPoW-bootstrapped node.

## Protocol notes

The Ergo P2P wire protocol was reverse-engineered from the JVM reference node and verified against pcap captures. There is no formal spec -- the wire format documentation lives in the main repo at `docs/protocol/`.

Key discoveries:
- All Scorex integer fields use VLQ encoding, not fixed-width (method names like `putUShort` are misleading)
- Handshake feature body lengths are VLQ-encoded (like all other Scorex integer fields)
- Empty-body messages omit the checksum on the wire (JVM's `MessageSerializer` only writes checksum when `dataLength > 0`)

## Building

This crate is part of the `ergo-node-rust` workspace. See the [main repo](https://github.com/mwaddip/ergo-node-rust) for build instructions and documentation.

To run tests for this crate alone:

```
cargo test
```

## Configuration

See `ergo-proxy.toml` for an example config. The `[network]` section is optional -- all fields default to values matching the JVM reference node (v6.0.3).

## License

Same as the parent project. See the [main repo](https://github.com/mwaddip/ergo-node-rust) for details.

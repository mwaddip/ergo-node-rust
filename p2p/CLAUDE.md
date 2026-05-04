# enr-p2p

P2P networking layer for the Ergo Rust node. Handles peer connections, handshake, message framing, and message routing. The network boundary — every byte from a peer is untrusted.

Started as a message-forwarding proxy. Gradually gaining awareness of what it forwards as validation components (`enr-chain`, etc.) come online. The proxy behavior is the fallback: if we can't validate it, forward it and let someone else decide.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Submodule: `p2p/` (enr-p2p)**.

Key stats: **P10** (untrusted peers send arbitrary bytes), **L9** (adversarial traffic, protocol confusion), **E9** (must survive anything).

## Contracts

Interface contracts are in `facts/`:
- `facts/p2p-transport.md` — frame encoding/decoding, handshake, connection lifecycle
- `facts/p2p-protocol.md` — message parsing, peer state machine, event guarantees
- `facts/p2p-routing.md` — inv table, request tracker, sync tracker, router behavior

Read the relevant contract before modifying any public API. If a contract needs changing, update it first, then implement.

## Scope

This crate owns:
- TCP connection management (inbound + outbound, IPv4 + IPv6)
- Ergo P2P handshake (Scorex VLQ serialization, session features, version checks)
- Message framing (magic bytes, blake2b checksums, body length)
- Message routing (inv table, request tracking, sync pairing, peer registry)
- Peer management (connect, disconnect, penalty tracking, keepalive)

This crate does NOT own:
- Header validation or chain state — that's `chain/`
- Block or transaction validation — that's `ergo-validation`
- Persistent storage — that's `store/`
- UTXO state — that's `state/`
- What to sync or when — that's `ergo-sync`

## Architecture

```
Inbound peers ──▶ ┌──────────────┐
                  │  Transport   │  frame encode/decode, handshake
                  ├──────────────┤
                  │  Protocol    │  message parsing, peer state machine
                  ├──────────────┤
                  │  Router      │  message routing, inv/request tracking
Outbound peers ◀──┘──────────────┘
```

The transport layer never interprets message content. The protocol layer parses messages into typed variants. The router decides where messages go. Validation hooks (like PoW checks on headers) are called by the router but implemented externally.

## Key Protocol Facts

- **VLQ everywhere**: Scorex method names (`putUShort`, `putUInt`) are misleading — all use VLQ encoding
- **Except frame headers**: `body_length` in message framing is 4-byte big-endian, NOT VLQ
- **Magic bytes**: Mainnet `[01,00,02,04]`, Testnet `[02,03,02,03]`
- **Modifier types**: 1=Header, 2=Transaction, 3=BlockTransactions, 4=ADProofs, 5=Extension
- Full wire format spec: `docs/protocol/ergo-p2p-wire-format.md` in the main repo

## JVM Reference

The canonical reference for P2P behavior is `~/projects/ergo-node-build` (v6.0.3):
- `ergo-core/src/main/scala/org/ergoplatform/network/` — P2P message handling
- `ergo-core/src/main/scala/org/ergoplatform/network/peer/` — peer management
- `src/main/scala/scorex/core/network/` — connection handling, penalty system

When behavior is ambiguous, the JVM source is correct. When the JVM source disagrees with observed wire traffic, the traffic wins.

## Session Boundary

This is a submodule session. Your working directory is this repo's root — **never read, write, or navigate to files outside it.** You have no access to the parent repo, sibling submodules, or any path above your root. If you need context from outside your boundary, ask the user.

You are an expert within the P2P contract boundary and a confident amateur outside it. Do not implement validation logic, storage, or chain state. If you need something from outside your boundary, define what you need as a trait or callback and let the integrator wire it.

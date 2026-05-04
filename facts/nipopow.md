# NiPoPoW Serve + Light-Client Bootstrap Contract (in-repo side)

## Scope

This contract covers the **in-repo (main crate) side** of NiPoPoW proof
handling. The chain submodule owns proof construction, verification, and
the install path (see `facts/chain.md` Phase 6: NiPoPoW Proofs). The main
crate owns the P2P message envelope, event dispatch, and the
light-client bootstrap state machine.

What ships in this contract:

1. Wire format for P2P message codes 90 (`GetNipopowProof`) and 91
   (`NipopowProof`).
2. A handler in `src/main.rs` that subscribes to `ProtocolEvent`s,
   filters for codes 90 and 91, parses the envelopes, calls into chain,
   and sends responses via `p2p.send_to`.
3. Verification of incoming proofs (logged in non-light mode; routed to
   the bootstrap state machine in `StateType::Light` mode).
4. **Outbound `GetNipopowProof` requests** for light-client bootstrap.
   The serializer for code 90 (`serialize_get_nipopow_proof`) is added
   alongside the existing code 91 serializer.
5. **Light-client bootstrap state machine** in `sync/` (see
   `facts/sync.md` "Light-Client Bootstrap" section). The main crate's
   role is to wire the bootstrap entry point into startup when
   `state_type == Light`.

What does NOT ship:

- Multi-peer best-arg proof comparison (KMZ17 §4.3) — single-peer
  bootstrap only for first release.
- In-extension parameter recovery for light clients — `active_parameters`
  stays at network defaults in light mode (documented limitation in
  `facts/chain.md`).
- Modifications to the P2P submodule (router treats codes 90/91 as
  Unknown and forwards them — wasteful, see
  `project_unknown_message_forwarding` memory; fix is a follow-up).

## SPECIAL Profile

```
S9 P10 E6 C5 I7 A7 L9
```

P is max — every byte from a peer is adversarial. The size cap on code 91
is 2 MB; the parser must enforce it before allocating. L is high — wrong
m/k handling, malformed proofs, and oversized inputs are the failure
modes.

## P2P Wire Format

Two new message codes. Both reuse the 4-byte magic + 1-byte code +
4-byte length + 4-byte checksum + body framing already implemented in
the P2P transport (no transport changes).

| Code | Name | Direction (this release) | Max body size |
|------|------|---------------------------|---------------|
| 90 | `GetNipopowProof` | Receive only | 1000 bytes |
| 91 | `NipopowProof` | Send only | 2,000,000 bytes |

JVM reference (canonical):
- `ergo-core/src/main/scala/org/ergoplatform/network/message/GetNipopowProofSpec.scala`
- `ergo-core/src/main/scala/org/ergoplatform/network/message/NipopowProofSpec.scala`

### Code 90: `GetNipopowProof` body

```
m: i32 (ZigZag VLQ — JVM putInt)
k: i32 (ZigZag VLQ — JVM putInt)
header_id_present: u8 (raw byte: 0 or 1)
[if header_id_present == 1] header_id: 32 bytes
future_pad_length: u16 (VLQ — JVM putUShort)
[if future_pad_length > 0] padding: future_pad_length bytes (skipped)
```

**Critical**: `m` and `k` use `putInt` (ZigZag VLQ), NOT plain VLQ or BE
fixed-width. The lesson from snapshot sync (`facts/snapshot.md` line 49)
applies: JVM Scorex serializers always use VLQ. Verified in `GetNipopowProofSpec.scala`.

**Validation**:
- Total body size MUST be ≤ 1000 bytes (reject before allocating).
- `m` and `k` MUST both be > 0.
- `m + k` capped at a sane upper bound (suggest 1000) to prevent unbounded
  proof construction.
- `future_pad_length` ≤ remaining body bytes (else parse error).

### Code 91: `NipopowProof` body

```
proof_length: u32 (VLQ — JVM putUInt)
proof_bytes: [u8; proof_length]
future_pad_length: u16 (VLQ — JVM putUShort)
[if future_pad_length > 0] padding: future_pad_length bytes (skipped)
```

**Validation**:
- Total body size MUST be ≤ 2,000,000 bytes (reject before allocating).
- `proof_length` MUST be > 0 AND < 2,000,000.
- `proof_bytes` is the inner NiPoPoW proof — passed verbatim to
  `chain.verify_nipopow_proof_bytes(proof_bytes)`.

## Module: `src/nipopow_handler` (or inline in main.rs)

A subscribed task that consumes `ProtocolEvent`s and responds to NiPoPoW
messages. Mirrors the snapshot sync handler structure.

### `spawn_nipopow_handler(p2p: P2pNode, chain: Arc<Mutex<HeaderChain>>, state_type: StateType)`
- **Postcondition**: Spawns a tokio task that:
  1. Subscribes to `p2p.subscribe()`.
  2. On every `ProtocolEvent::Message { from, code, body }` with `code == 90`:
     - Parses `body` as `GetNipopowProofRequest`. On parse error, log + drop.
     - Acquires the chain lock, calls `chain.build_nipopow_proof(m, k, header_id_opt)`.
     - On success, wraps the resulting bytes in the code 91 envelope and
       calls `p2p.send_to(from, ProtocolMessage::Custom(91, body))`.
     - On error, log + drop. (Do NOT send an error message — the JVM doesn't
       expect one and will time out instead.)
  3. On every `ProtocolEvent::Message { from, code, body }` with `code == 91`:
     - Parses `body` to extract the inner proof bytes.
     - Acquires the chain lock, calls `chain.verify_nipopow_proof_bytes(...)`.
     - In `StateType::Utxo`/`Digest` modes: logs the result and drops it
       (the existing serve-side observation path — verifies for visibility,
       does not mutate state).
     - In `StateType::Light` mode: routes the verified result to the
       light-client bootstrap state machine via a one-shot channel
       established at bootstrap start. The bootstrap waits for exactly
       one such verified result from its requested peer; subsequent code
       91 messages from other peers (if any) are logged and dropped.
- **Invariant**: The handler never blocks the event loop — long-running chain
  operations happen on a `tokio::task::block_in_place` boundary or in a
  spawned task, mirroring the existing pattern from snapshot sync.

### `parse_get_nipopow_proof(body: &[u8]) -> Result<GetNipopowProofRequest, NipopowError>`
### `parse_nipopow_proof(body: &[u8]) -> Result<Vec<u8>, NipopowError>`
### `serialize_nipopow_proof(proof_bytes: &[u8]) -> Vec<u8>`
### `serialize_get_nipopow_proof(req: &GetNipopowProofRequest) -> Vec<u8>`

`serialize_get_nipopow_proof` is the inverse of `parse_get_nipopow_proof`
and produces bytes ready for the code 90 message envelope. Used by the
light-client bootstrap state machine to send `GetNipopowProof` requests.
Wire format mirrors `parse_get_nipopow_proof` exactly:

```text
m: i32   (ZigZag VLQ, putInt)
k: i32   (ZigZag VLQ, putInt)
header_id_present: u8 (0 or 1)
[if present] header_id: 32 raw bytes
future_pad_length: u16 (VLQ, putUShort) — always 0 from us
```

```rust
pub struct GetNipopowProofRequest {
    pub m: i32,
    pub k: i32,
    pub header_id: Option<HeaderId>,
}

pub enum NipopowError {
    BodyTooLarge { size: usize, max: usize },
    InvalidParameters,
    Truncated,
    ChainError(String),
}
```

Note: there is no `NipopowProofResponse` struct — `parse_nipopow_proof`
returns the inner `Vec<u8>` directly because every consumer immediately
hands it to `chain.verify_nipopow_proof_bytes`. Wrapping it in a struct
adds no value.

### Sigma-rust integration

The chain submodule's `build_nipopow_proof`, `verify_nipopow_proof_bytes`,
and `install_from_nipopow_proof` wrap `ergo_nipopow::NipopowAlgos` and
`ergo_nipopow::NipopowProofSerializer`. The main crate does NOT import
`ergo-nipopow` directly — that's the chain's job. The main crate only
sees `Vec<u8>` (proof bytes), the `NipopowVerificationResult` struct
returned from `verify_nipopow_proof_bytes`, and `Header` (for installing
the suffix into the chain via `install_from_nipopow_proof`).

## Routing behavior (current limitation)

The P2P router treats unknown message codes as "forward to all peers of
opposite direction" (`facts/p2p-routing.md` line 48). This means:

- When peer A sends us `GetNipopowProof`, the router forwards a copy to
  every outbound peer in addition to delivering it to our subscriber.
- When peer A sends us `NipopowProof`, same — we get it AND every
  outbound peer gets it.

This is wasteful but harmless: we still see the events via `subscribe()`
and respond correctly. The forwarded copies are extra noise on the
network. Tracked in memory `project_unknown_message_forwarding` as a
follow-up.

For first release, **accept the waste**. Document it in the release notes.

## Configuration

Top-level toggles in `node.toml`:

```toml
[node]
state_type = "utxo"  # one of: "utxo", "digest", "light"

[node.nipopow]
serve = true       # respond to GetNipopowProof requests
verify = true      # verify incoming NipopowProof messages
max_proof_bytes = 2000000  # safety cap, mirrors JVM SizeLimit
```

When `state_type = "light"`, the node bootstraps from a NiPoPoW proof at
startup if the chain is empty (see `facts/sync.md` "Light-Client
Bootstrap"). The `[node.nipopow]` settings still control the serve side
in light mode — a light client can serve proofs to other light clients
provided it has enough chain to build one.

Defaults: `state_type = "utxo"`, both nipopow toggles `true`. The
`max_proof_bytes` cap should NOT be configurable above 2,000,000 (JVM's
hard limit). Lower values are valid for resource-constrained nodes.

## Test plan

### Unit (parser/serializer)

1. `parse_get_nipopow_proof` rejects bodies > 1000 bytes.
2. `parse_get_nipopow_proof` round-trips through `serialize_get_nipopow_proof`.
3. `parse_nipopow_proof` rejects bodies > 2,000,000 bytes.
4. VLQ parsing matches the JVM byte sequence (capture from a real JVM
   `GetNipopowProof` message via pcap, replay through our parser).
5. `serialize_get_nipopow_proof` with `m=6, k=10, header_id=None` produces
   bytes byte-identical to JVM's `GetNipopowProofSpec.toBytes` for the
   same input. Capture the JVM-generated bytes from
   `ErgoNodeViewSynchronizer.scala:1032` request once and pin them as a
   test fixture.

### Integration (serve side — already passing)

6. **Serve, JVM peer**: Have the JVM testnet peer request a NiPoPoW
   proof from our Rust node. Verify the JVM logs `is_valid = true` and
   does not disconnect.
7. **Serve, malformed input**: Inject a malformed proof request (m=0,
   k=0, oversized body). Verify the parser rejects each case.
8. **Verify, real**: Capture a real `NipopowProof` from testnet traffic,
   replay through our verifier. Must accept.

### Integration (light-client bootstrap)

9. **Bootstrap, single peer happy path**: Start a Rust node configured
   with `state_type = "light"` against a single peer (the test server's
   Rust node, which has full chain). Verify the light client:
   - Sends exactly one `GetNipopowProof(m=6, k=10, header_id=None)`.
   - Receives the response within 30s.
   - Verifies the proof.
   - Installs the suffix into `HeaderChain` (chain becomes non-empty).
   - Transitions to normal sync and starts following the tip.
10. **Bootstrap, peer stall**: Start a light client against a peer that
    accepts the request but never responds. Verify timeout fires at 30s,
    no second request to the same peer, and the bootstrap surfaces
    `LightBootstrapError::AllPeersStalled` after 3 retries (when only
    one peer is configured, all 3 attempts go to the same peer; verify
    the error is reached).
11. **Bootstrap, hostile peer**: Inject a peer that returns a malformed
    proof (mutated bytes). Verify the verifier rejects it and the
    bootstrap surfaces `LightBootstrapError::AllPeersHostile`.
12. **Bootstrap, install failure**: Inject a verified-but-self-
    inconsistent proof (parent linkage mismatch within the suffix that
    `verify_nipopow_proof_bytes` somehow accepted — this requires
    constructing a specially-crafted proof, may need to mock). Verify
    `install_from_nipopow_proof` rejects it and bootstrap surfaces
    `LightBootstrapError::InstallFailed`.
13. **Bootstrap restart idempotence**: Run a successful bootstrap, kill
    the node, restart with the same `state_type = "light"` config and
    a non-empty chain on disk. Verify bootstrap is skipped and the node
    enters normal sync immediately.

## Cross-references

- `facts/chain.md` Phase 6: NiPoPoW Proofs — the chain side
- `facts/snapshot.md` — pattern reference (codes 76-81 use the same approach)
- `facts/p2p-node.md` — `subscribe()` and `send_to` API
- `facts/p2p-routing.md` line 48 — Unknown forwarding behavior
- Memory `project_unknown_message_forwarding` — follow-up to fix waste
- JVM: `ergo-core/src/main/scala/org/ergoplatform/network/message/GetNipopowProofSpec.scala`
- JVM: `ergo-core/src/main/scala/org/ergoplatform/network/message/NipopowProofSpec.scala`

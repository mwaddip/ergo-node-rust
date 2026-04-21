# Fast Sync via chainSlice REST API — Reference Note

> **Status:** Reference material. Not our design. Documenting an architectural pattern
> used by another independent Rust Ergo node (`github.com/arkadianet/ergo`, commit
> `66e560644c7e31135b4d196be99608f4949733ce`, 2026-04-07) so we can evaluate it later.
>
> **This file describes someone else's code.** Do not implement anything here without
> re-reading the upstream source first — the constants and mechanisms may have drifted.

## Summary

arkadianet's Rust node bootstraps the header and block chain by pulling data from
peers' **REST API endpoints** instead of the P2P protocol. It downloads 2000-header
chunks over HTTPS from multiple peers in parallel, validates PoW, and feeds the
results into the same block processor that P2P sync uses. It stops within 1000
blocks of tip and hands off to normal P2P sync for the tail.

**Reported performance (from the author, 2026-04-07):** full sync from genesis to
tip in **~5 minutes**. For context: a cold JVM sync is hours-to-a-day; snapshot
sync is tens of minutes. This is an order of magnitude faster than anything else
currently available, and it moves chainSlice sync from "alternative bootstrap
path" into "the bootstrap path if you want cold-start to feel interactive."

The trick this depends on: **every JVM Ergo peer already advertises its REST API
URL in the handshake** via a standard Scorex peer feature (`FEATURE_ID_REST_API_URL`,
id `4`, length-prefixed UTF-8 string). arkadianet just reads what's already being
advertised and connects over HTTP. No new protocol, no new messages, no JVM-side
changes required.

## What arkadianet Actually Built

Two tokio tasks, reading from a shared map of peer REST URLs, writing to the same
block processor command channel that P2P sync uses:

| File (in arkadianet's repo) | Purpose |
|---|---|
| `crates/ergo-wire/src/peer_feature.rs` | `PeerFeature::RestApiUrl(String)`, id `4`, length-prefixed UTF-8. |
| `crates/ergo-node/src/fast_header_sync.rs` | Header pipeline: parallel HTTP fetch of `chainSlice`, JSON→wire conversion, PoW validation, bulk insert. |
| `crates/ergo-node/src/fast_block_sync.rs` | Block pipeline: parallel HTTP fetch of `POST /blocks/headerIds`, JSON→wire conversion per section. |
| `crates/ergo-node/src/event_loop.rs` (≈L300–440) | Wires the shared `ApiPeerUrls` map, spawns both fast-sync tasks. |

### Shared state

```rust
// fast_header_sync.rs
pub type ApiPeerUrls =
    Arc<RwLock<HashMap<u64 /*peer_id*/, String /*rest_url*/>>>;

pub type SharedHeadersHeight = Arc<AtomicU32>;
pub type SharedFastSyncActive = Arc<AtomicBool>;
```

- Event loop inserts a URL when a peer completes handshake and advertised
  `FEATURE_ID_REST_API_URL`; removes it on disconnect.
- Fast-sync task reads the map on every chunk dispatch, so new peers are picked up
  without restart.
- `SharedHeadersHeight` is written by the event loop whenever a header is applied
  (from any source) — fast-sync reads it to skip chunks that P2P has already
  covered, and to throttle itself with `MAX_LOOKAHEAD`.

### Endpoints used

| Method | URL | Purpose |
|---|---|---|
| `GET`  | `/info` | Discover peer's tip height (`headersHeight` / `bestHeaderHeight`). |
| `GET`  | `/blocks/chainSlice?fromHeight=X&toHeight=Y` | Fetch a window of headers (up to 2000). |
| `POST` | `/blocks/headerIds` | Fetch full block sections by ID batch. |

All three are endpoints the JVM Scala reference node already exposes. No custom
server-side code on the peer.

### Header flow (`fast_header_sync.rs::run_fast_sync`)

1. Wait up to 60s for at least one REST URL to appear in `ApiPeerUrls`.
2. Query `/info` on any reachable peer, get `best_height`.
3. Compute chunks `[(from, to), …]` of `chunk_size` headers up to `best_height - HANDOFF_DISTANCE`.
4. Work-stealing queue: pop a chunk, pick an idle non-failing peer, spawn a task to
   fetch `chainSlice`. One request per peer at a time.
5. On HTTP success, parse `ChainSliceHeader` structs, convert to wire format via
   `json_header_to_wire`, verify header ID matches `blake2b256(raw_wire)`, validate
   PoW in parallel, send `BulkHeaders` command to the block processor.
6. On failure or 20s timeout: increment peer failure counter (blacklist after 3),
   push chunk back to front of queue.
7. Throttle: never dispatch a chunk more than `MAX_LOOKAHEAD = 200_000` headers
   ahead of the processor. Sleep 500ms and recheck.
8. Loop until queue drained or shutdown.

### Tuning constants (arkadianet)

```rust
const HANDOFF_DISTANCE:       u32 = 1000;    // stop N blocks before tip, P2P takes over
const PEER_MAX_FAILURES:      u32 = 3;       // consecutive fails before blacklist
const PEER_FETCH_TIMEOUT_SECS: u64 = 20;     // per-chunk per-peer timeout
const MAX_LOOKAHEAD:          u32 = 200_000; // headers ahead of processor before throttling
```

Chunk size is a runtime parameter, default appears to be 2000 based on how the
Scala node serves `chainSlice`.

### JSON parsing quirks worth knowing

From `json_header_to_wire` in `fast_header_sync.rs`:

- Scala encodes the extension root as `"extensionHash"` in JSON but its own Rust
  equivalent uses `"extensionRoot"`. arkadianet accepts both via `#[serde(alias)]`.
- `pow_solutions.d` is a `BigInt` that Scala JSON-encodes as a **bare decimal
  number** (not a string). For Autolykos v1 headers this can be 60+ digits, so
  `u64`/`f64` will silently lose precision. They use
  `serde_json::Number` with the `arbitrary_precision` feature, then parse via
  `num_bigint::BigUint::from_str` and `.to_bytes_be()` with **no leading zero**
  (Scala uses `BigIntegers.asUnsignedByteArray()`).
- Header ID verification: compute `blake2b256(raw_wire_bytes)`, compare against
  the declared `"id"` field, reject on mismatch.

These are the *kind* of compatibility bugs that bite anyone going
JSON-to-consensus-critical-bytes. Worth re-reading the actual file before
reimplementing.

## Why This Pattern Is Interesting For Us

**We already receive `FEATURE_ID_REST_API_URL` from every JVM peer.**
`p2p/src/transport/handshake.rs` parses handshake features into a generic
`Feature { id: u8, body: Vec<u8> }` and preserves unknown IDs. Feature id `4` is
in `PeerSpec.features` right now, ignored but intact, with the URL sitting in
`body[1..1+body[0]]` (length-prefixed UTF-8).

```rust
// p2p/src/transport/handshake.rs, current state
const FEATURE_MODE:    u8 = 16;
const FEATURE_SESSION: u8 = 3;
pub const FEATURE_PROXY: u8 = 64;
// feature id 4 (RestApiUrl) is parsed into Feature{id:4, body:…} and passed through.
```

Adding a typed accessor (`fn rest_api_url(&self) -> Option<String>`) would be a
≈10-line change to `handshake.rs`. No wire changes. No contract changes.

## How It Compares To What We Already Have

| Dimension | Our current approach | arkadianet's chainSlice sync |
|---|---|---|
| **Bootstrap mechanism** | UTXO snapshot sync over P2P (codes 76–81) | HTTP chainSlice + headerIds |
| **Requires snapshot on peer** | Yes (snapshot must exist) | No (any REST-enabled peer works) |
| **Skips genesis→tip walk** | Yes (jumps to snapshot height) | No (walks headers from current to tip-1000) |
| **Bandwidth** | Lower (compressed state) | Higher (raw JSON) |
| **Time to first validated tip** | Tens of minutes if snapshot is available | **~5 min genesis-to-tip** (author report, 2026-04-07) |
| **Dependencies on peer capabilities** | Peer must serve snapshots (newer feature) | Peer must expose REST API (every JVM node does) |
| **P2P-independent** | No — uses P2P transport | Yes — HTTP is out-of-band |
| **Handoff to normal sync** | After snapshot applied | At `tip - HANDOFF_DISTANCE = 1000` |

**These are complementary, not competing.** Snapshot sync skips validation work
by trusting a committed state root; chainSlice sync parallelizes the validation
work across many HTTP peers. A node could reasonably do both: snapshot sync to
jump to a recent state, then chainSlice to catch up the header tail faster than
P2P would.

## Things to Be Careful About Before Adopting

1. **HTTP surface area.** Fast-sync adds a `reqwest` client, TLS handling, DNS
   resolution, and a whole new failure domain. Our current node's attack surface
   is "what comes through the P2P codec." Adding HTTP means "what comes through
   any HTTPS peer advertising a URL." A malicious handshake could advertise a
   URL we then contact — at minimum, we'd want to validate the URL structure and
   probably restrict it to the peer's known IP.

2. **Peer REST URL ≠ peer P2P IP.** A peer can advertise any URL. arkadianet
   does **not** (visibly) cross-check the URL's hostname against the peer's
   socket address. If we adopt this, we should either verify they match or at
   least log the mismatch.

3. **JSON-to-wire is a compatibility minefield.** Every field that has a
   JSON-only idiosyncrasy (camelCase aliases, BigInt-as-bare-number,
   extensionHash-vs-extensionRoot) is a place two implementations can silently
   disagree. Our JVM interop test catches P2P wire bugs because the JVM is the
   reference; fast-sync converts *away* from the reference format and toward it
   again. A bug between the two conversions would validate locally but produce
   a block ID mismatch — which arkadianet at least catches via the
   blake2b256-on-wire-bytes check. We should reuse that exact guard.

4. **`reqwest` pulls in a lot.** TLS, HTTP/2, connection pool, tokio-util. Our
   current node is deliberately minimal on dependencies. Adding reqwest is a
   policy decision, not just a feature.

5. **Per-peer rate limiting.** One-request-at-a-time-per-peer is sensible.
   Missing this would let a single peer DoS our fast-sync by queuing chunks.

6. **HANDOFF_DISTANCE = 1000 is a protocol-level assumption.** It assumes the
   last 1000 blocks are where reorgs happen and fast-sync should not touch
   them. Matches JVM's `blocks_to_keep` defaults loosely. Worth keeping as a
   tunable.

## When We'd Actually Want This

The 5-minute number changes this from "nice to have" to "probably want it."

- **Cold-start UX.** Five minutes is the threshold where "spin up a node" becomes
  an interactive action instead of a scheduled one. Any user-facing tooling
  that provisions nodes (BlockHost-style VM hosting, developer sandboxes,
  CI test fixtures, reproducible research setups) goes from unusable to
  trivial. This alone justifies the work.
- **Bootstrapping without snapshot dependencies.** Every JVM peer exposes the
  REST API; a smaller-and-growing subset serve snapshots. chainSlice works
  against the whole network.
- **Differential testing.** A validation-independent second path to the same
  data. If chainSlice and P2P produce different blocks at the same height,
  one of them is wrong. Continuous sanity check for our wire format.
- **Indexer reprocess.** Reprocessing from genesis without a snapshot
  (re-indexing on schema change, rebuilding derived data) goes from a
  full-day batch job to a coffee break.
- **CI and testing.** Integration tests that need a full chain state can stand
  up a node in 5 minutes instead of relying on pre-built fixtures that rot.

## When We Would Not

- **Phase-B feature work doesn't need it.** Voting and NiPoPoW can land first;
  fast-sync is orthogonal.
- **If we never plan to onboard new operators**, the cold-start time doesn't
  matter. Existing nodes don't re-sync.
- **If HTTP surface area is unacceptable** for the threat model (e.g. air-gapped
  or P2P-only deployments). The fallback is always the existing P2P sync, which
  continues to work.

## Minimal Adoption Path (If/When We Decide To)

1. Add typed accessor `PeerSpec::rest_api_url() -> Option<String>` in
   `p2p/src/transport/handshake.rs`. ≈10 lines. Parse the length-prefixed UTF-8
   from `Feature { id: 4, body }`.
2. Add a shared map in the main crate: `ApiPeerUrls = Arc<RwLock<HashMap<PeerId, String>>>`.
3. Populate/evict it from the event loop on peer connect/disconnect, guarded by
   a structural URL validation.
4. Write a new crate (`ergo-fast-sync`?) or put it in the main binary. Single
   tokio task, reqwest client, work-stealing queue. Port the three constants
   and the JSON types from arkadianet's files listed above.
5. Feed results into the existing validation pipeline via the same channel P2P
   sync uses — no new `ProcessorCommand` variants required if we reuse
   `PutModifier`.
6. Add a differential test: fast-sync a small height range, P2P-sync the same
   range, diff the resulting chain. Any mismatch is a conversion bug.

## References

- arkadianet repo: `github.com/arkadianet/ergo` (as of commit
  `66e560644c7e31135b4d196be99608f4949733ce`, 2026-04-07).
- Key files:
  - `crates/ergo-wire/src/peer_feature.rs` — `FEATURE_ID_REST_API_URL` definition.
  - `crates/ergo-node/src/fast_header_sync.rs` — header pipeline, work-stealing queue.
  - `crates/ergo-node/src/fast_block_sync.rs` — block pipeline, JSON-to-wire.
  - `crates/ergo-node/src/event_loop.rs` ≈L300–440 — wiring.
- Scala reference endpoints: `/info`, `/blocks/chainSlice`, `POST /blocks/headerIds`.
- Scorex `PeerFeature` with id `4` = `RestApiUrl` (upstream protocol feature, not
  an arkadianet extension).

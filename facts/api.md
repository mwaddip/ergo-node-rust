# REST API Contract

> Per-endpoint structural detail (paths, params, request and response
> bodies, status codes) lives in [`openapi.yaml`](openapi.yaml).
> This document covers cross-cutting rationale only.

## Component: `api/` (workspace crate)

External-facing HTTP interface for wallets, dApps, miners, and monitoring tools.
Thin facade over existing components — translates HTTP requests into calls against
the chain, state, mempool, and P2P subsystems. Matches JVM endpoint paths where
possible so existing tooling works without modification.

Primary consumers: Ergo wallets (Nautilus, SAFEW), mining software (GPU miners),
block explorers, dApp connectors. Primary dependencies: mempool (tx submission
and queries), chain (header lookups), state (UTXO lookups), store (block retrieval),
P2P (peer info).

## SPECIAL Profile

```
S8  P9  E6  C8  I6  A6  L5
```

External-facing (P9) — every byte from the network is untrusted. Input validation
is the primary concern. Must not leak internal state or enable DoS via expensive
queries. Clarity matters (C8) — developers stare at this API. Not a long-running
critical process (E6) — crash means API is temporarily unavailable, no state
corruption. Edge cases are low-risk (L5) — the interesting bugs live in the
components behind the API, not in the HTTP layer.

## Design Principles

- **Facade, not brain.** The API crate contains zero business logic. It validates
  input, calls a component method, serializes the result. If you're writing an
  `if` that makes a domain decision, it belongs in the component, not here.
- **JVM path compatibility.** Match the JVM node's endpoint paths exactly where
  we implement the same functionality. Wallets, miners, and explorers should
  work against the Rust node without config changes.
- **Bounded queries.** Every list endpoint has a hard maximum for `limit`.
  No unbounded iterations over chain or mempool data. Default limits match JVM.
- **No wallet, no scanning.** The Rust node does not embed a wallet. Wallet
  functionality lives in external tools (Nautilus, SAFEW, ergo-lib). The
  `/wallet/*` and `/scan/*` endpoint families are not implemented.
- **API key optional.** Authenticated endpoints (mining with custom txs, node
  control) require an API key when configured. Unauthenticated endpoints are
  always open. API key is Blake2b256-hashed in config, compared via constant-time
  equality.

## Framework

axum (tokio-native, tower-based). The project already uses tokio for async.
axum is minimal, composes well with tower middleware, and doesn't pull in an
actor system.

## Dependencies

- `axum` — HTTP framework
- `serde`, `serde_json` — request/response serialization
- `ergo-lib` — `Transaction`, `ErgoBox`, `Header` types for JSON serialization
- `ergo-chain-types` — `Header` type
- `tokio` — async runtime (already in workspace)

Does NOT depend on: `redb`, `ergo_avltree_rust`, P2P transport internals,
sync state machine internals. All access is through shared state handles
passed at construction.

## Shared State

The API server receives shared handles at construction — the same handles used
by the sync/validation pipeline, with no duplication of state. The full struct
lives at `api/src/lib.rs::ApiState`. The roles, summarized:

- Chain read access (header lookups, tip, header IDs)
- Block store read access (full block sections by header ID)
- Mempool read/write (the only mutable state the API touches, via `process()`)
- UTXO snapshot reader (box lookups by ID)
- Peer state (counts, list, blacklist, manual connect trigger)
- Current parameters (from extension voting)
- Node metadata (network, name, version, state type)
- Optional API key hash (`None` = no auth required)
- `ErgoStateContext` for transaction validation
- Validated and downloaded block heights (atomic, watched)
- Modifier-pipeline sender (for the rust-only `/ingest/modifiers` endpoint)
- Optional jemalloc probe (`None` with mimalloc or system allocator)
- Optional P2P-capture handle (`None` when `[debug.p2p_capture]` is off)
- Mining state and block submitter (`None` outside UTXO mode with a miner PK)

### Component memory attribution

`GET /debug/memory` reports per-component figures alongside the process totals:

- `chainIndexBytes` / `chainHeaderCacheBytes` / `chainScoreCacheBytes`, from
  `ChainAccess::memory_estimate()` returning an api-local
  `ChainMemory { index_bytes, header_cache_bytes, score_cache_bytes }`.
- `storeCacheBytes`, `storeCacheEvictions` and `stateCacheBytes`, from
  `redb::Database::cache_stats()` in `store/` and `state/`, reached through
  `StoreAccess::cache_bytes_used`, `StoreAccess::cache_evictions` and
  `UtxoAccess::cache_bytes_used`. redb is typically the single largest
  consumer during a genesis sync.
- `proverModifiedNodesBytes`, `proverResidentNodesBytes` and `syncWindowBytes`,
  which are **published** rather than read — see below.

**The API layer must not compute these numbers.** It reports what the owning
crate tells it. A figure derived here from a local constant describing a
structure in another crate cannot notice when that structure changes shape, and
nothing cross-checks the two.

This crate deliberately does not depend on `enr-chain`; it reaches the chain
only through `ChainAccess`. `ChainMemory` is therefore an api-local type, and
the integrator's adapter in the main crate maps `chain/`'s
`ChainMemoryEstimate` onto it. That mapping is the adapter's job as the
integration seam — it is not licence to reintroduce sizing arithmetic here.

Any future per-component memory field follows the same rule: the crate that
owns the structure computes the figure; this crate transports it.

**Every accessor returns `Option<u64>` and its field is omitted when the value
is `None`.** Reporting `0` would assert an empty cache. The traits declare these
methods **without default bodies**: a default lets an implementor silently
return a wrong value.

⚠ **These three cannot be read synchronously, unlike every field above.** The
prover lives inside the UTXO validator, which `sync/` owns and does not share
(`facts/validation.md` — the validator is moved into `HeaderSync`, not wrapped
in an `Arc<Mutex>`). The API therefore cannot call an accessor on it. They are
**published** instead: the owning crate computes its estimate, the main crate
stores it in a `PublishedGauge` — the mechanism already used for
`shared_height` — and this crate reads that. The `Option` discipline is
unchanged: a gauge that has never been written reports absent, not zero, so a
node that has applied no blocks does not claim an empty prover.

⚠ **`Arc<AtomicU64>` is the interchange type; `PublishedGauge` is a view over
one.** `api/` and `sync/` cannot name each other's types — neither depends on
the other, and their only shared crates are `enr-p2p`, `ergo-chain-types` and
`ergo-validation`, none of which is a sensible home for an observability
primitive. So the *storage* is a plain `Arc<AtomicU64>` that the main crate
allocates once and hands to both ends: the producer publishes into it, and
`PublishedGauge` adopts the same `Arc` to read it. Two views, one allocation.

`PublishedGauge` must therefore be constructible from an existing
`Arc<AtomicU64>` and able to hand its own back. A gauge that owns its
`AtomicU64` outright cannot be shared with a producer in another crate, which
is the whole requirement.

Both ends already agree on `u64::MAX` as the never-published sentinel and on
`Relaxed` ordering. Relaxed is right: this is a monotonic diagnostic gauge with
a single writer, never a synchronisation edge, and nothing downstream orders
other reads against it.

⚠ **The two cadences differ, deliberately.** `syncWindowBytes` is cheap and
publishes after each applied block. The prover figures walk the resident tree —
O(resident nodes), the same order as applying a block at mainnet scale — so they
publish on a bounded block interval with a wall-clock ceiling
(`facts/validation.md` § "Prover memory attribution"). Per-block publication
there is a measurable sync regression for no benefit: the structure grows
monotonically, so a slow gauge loses nothing. Do not "make it consistent" by
moving the prover onto the per-block path.

Do not "simplify" this into a direct accessor. A synchronous read would need
either a lock on the validator on an HTTP path or an `Arc<Mutex>` around it.

### State Context Lifecycle

`state_context` is rebuilt whenever the validated tip advances:
1. Validator confirms block at height H
2. Main crate builds `ErgoStateContext` from header H + 10 preceding headers + current parameters
3. Stores in the shared lock
4. Mempool task and API both read from this same handle

This means mempool validation always uses the state context of the latest
validated block. During sync (tip advancing rapidly), this is rebuilt
frequently — the lock ensures readers see a consistent snapshot.

During initial sync the context may be `None`; transaction submission
endpoints return 503 in that state.

## JVM Compatibility

Per-endpoint compatibility level is annotated in `openapi.yaml` as one of:

- `full` — byte-equivalent with JVM.
- `partial` — same shape, edge-case behavior may differ.
- `deviation` — Rust-specific behavior; the openapi description names what.

The overall posture: every endpoint that exists on both sides aims for `full`.
Where it can't, the gap is documented inline in `openapi.yaml`. Rust-only
endpoints (`/info/wait`, `/blocks/{id}/validation-fragments`, `/peers/api-urls`,
`/debug/*`, `/stats/p2p`, `/ingest/modifiers`) are also flagged in `openapi.yaml`
via both a `rust-only` tag and an `x-rust-only: true` extension so generated
client libraries can filter them out for portability.

Known structural deviations:

- `GET /peers/connected` — returns the same `PeerInfoEntry` shape as
  `GET /peers/all`, filtered to connected peers. JVM additionally emits
  `lastHandshake` and `restApiUrl` per entry; the latter is surfaced via
  `GET /peers/api-urls` instead.
- Transaction JSON, both ways. A rendered transaction writes its values at
  the version of its id, where the JVM writes them at its default context
  (§ Transaction JSON). A JSON submission is built at the activated version,
  with its `id` and any output `boxId` checked, where the JVM builds at its
  default context and ignores both (§ Transaction Submission Flow).

## Endpoints NOT Implemented (First Release)

| JVM Endpoint Family | Reason |
|---|---|
| `/wallet/*` | No embedded wallet. Use Nautilus, SAFEW, or ergo-lib directly. |
| `/scan/*` | No scanning/tracking subsystem. |
| `/script/*` | Script compilation/execution is a dev tool, not a node function. Add later. |
| `/blockchain/*` | Requires extra indexing infrastructure. Add later. |
| `/node/shutdown` | Operational concern. Signal-based shutdown is sufficient. |
| `/utils/*` | Utility functions. Add later if wallets need them. |
| `POST /blocks` | Block submission via API (miners use `/mining/solution`). |

## Error Model

All errors on the main listener return a JSON body matching the JVM format:

```json
{
  "error": 400,
  "reason": "Human-readable error message",
  "detail": "Optional technical detail"
}
```

**Error codes:**
- `400` — bad request (malformed JSON, invalid tx, missing params)
- `403` — authentication required but missing or invalid
- `404` — resource not found (unknown header ID, box ID, tx ID)
- `410` — resource was pruned (e.g., `/blocks/{id}/validation-fragments` after `blocks_to_keep` clipped the section)
- `413` — request body too large
- `415` — a JSON endpoint's body not sent as `application/json`
- `429` — rate limited (if HTTP-level rate limiting is added at a reverse proxy)
- `500` — internal error (component failure, should not happen)
- `503` — node is syncing or a required subsystem (mining, modifier pipeline, state context) is not yet available

The `reason` field is safe to show to users. The `detail` field may contain
internal information (stack traces, component errors) and should be omitted
in production or restricted to authenticated requests.

All endpoints, including Rust-only ones, return errors via the standard
`ApiError` shape above. That includes a request refused before any handler
logic runs: a malformed query string or path segment, or a body that isn't
valid JSON for the endpoint. It also includes a request no route matches
(404), a known path asked with a method it doesn't serve (405), and a
response that fails to serialize (500). HEAD answers keep an empty body.
Endpoints that need to expose a programmatic
dispatch key (the cross-validator harness keying off
`/blocks/{id}/validation-fragments`, the capture endpoints' on/off probe)
carry a short code string in `reason` and any context in `detail`. See
each endpoint's `openapi.yaml` entry for the codes it emits.

## Authentication

Optional API key via `api_key` header (the alias `api-key` is also accepted
for compatibility). When `api_key_hash` is configured:

1. Extract `api_key` header value
2. Compute `Blake2b256(api_key_bytes)`
3. Constant-time compare with stored `api_key_hash`
4. On mismatch: 403

**Authenticated endpoints:**
- `POST /mining/solution`
- `POST /peers/connect`
- Any future admin/control endpoints

**Unauthenticated endpoints:** everything else. Read-only queries and
transaction submission are always open (matching JVM behavior).

### Loopback-bound listeners

Two listeners are loopback-only by design and depend on the bind address as
their security boundary, not on an API key:

- `POST /ingest/modifiers` (on the main listener) — rejects any request from
  a non-loopback peer with 403, regardless of API-key state. This protects
  the modifier-pipeline shortcut used by co-located fastsync tooling. The
  loopback check is an access control, not a trust grant: ingested bytes
  enter the same pipeline as P2P deliveries and are bound the same way
  (`facts/receive-path.md`, `facts/sync.md` § Receive-path binding). A
  body whose id does not match is dropped there; `{"accepted": n}` counts
  bodies queued, not bodies stored.
- `GET /stats/p2p` (on a separate listener, default `127.0.0.1:9055`) —
  starts only when the operator config includes a `[stats]` section. A
  non-loopback bind logs a startup WARN; there is no auth.

## Configuration

```toml
[node.api]
bind_address = "0.0.0.0:9053"    # Listen address (JVM default: 9053 mainnet, 9052 testnet)
api_key_hash = ""                 # Blake2b256 hex of API key (empty = no auth)
request_timeout_ms = 5000         # Per-request timeout
max_body_bytes = 2_097_152        # Max request body (2 MB)

[stats]                           # Optional; absent = stats listener not started
bind_address = "127.0.0.1:9055"   # Loopback-only by default

[debug.p2p_capture]               # Optional; absent = capture handle is None
# See facts/p2p-capture.md for the full schema.
```

## Naming Conventions

- **Paths** use kebab-case multi-word segments (`validation-fragments`,
  `api-urls`) and lowerCamelCase for JVM-aligned segments (`lastHeaders`,
  `byTransactionId`, `getSnapshotsInfo`, `popowHeaderById`). JVM-aligned segments
  keep the JVM spelling exactly so existing tooling works; new Rust-only
  paths use kebab-case.
- **JSON keys** use camelCase. The `serde(rename_all = "camelCase")`
  attribute is applied uniformly to response structs.
- **IDs** in responses are lowercase hex; 32-byte modifier / header / box /
  transaction IDs render as 64-char hex strings without `0x` prefix.
- **Amounts** are nanoERG (1 ERG = 10^9 nanoERG), signed-or-unsigned 64-bit
  integers matching the underlying domain type. JVM compatibility forces
  integer encoding rather than decimal-string encoding even when the value
  approaches `i64::MAX`.

## Cross-cutting Concerns

### Pagination

List endpoints accept `offset` (default 0) and `limit` (default 50, hard
cap 100). Out-of-range `offset` returns an empty array rather than 404.
`limit` is silently clamped to the maximum; clients that need to know they
hit the cap must check the returned length against the requested limit.

### Ordering

- Block listings: descending by height (newest first).
- Mempool listings: descending by priority (highest fee weight first).
- Peer listings: insertion / observation order; not stable across restarts.

### Transaction JSON

Every response that renders a transaction writes its context-extension and
output register values at the version its id is written at. That is the version
it was read at: for a block's transactions, `enr_chain::block_transactions_read_version`
of the block; for a pool transaction, the version the pool read or built it at.
A client that re-serializes the JSON at that version gets the transaction's id
back. The fork's serde renders this way (fork PR #56); no handler renders a
transaction's values at another version.

An output inside a transaction's JSON follows the transaction, but its `boxId`
hashes the output written at version 0. That is how ergo computes an output's
id: its `ErgoBox.bytes`, first written in the output checks at the JVM's default
context (1, 1), below version 3. A register holding an `Upcast` of a constant
(SANTA's X15) shows the difference. Its rendered hex keeps the `Upcast`, and
the box id hashes the output without it. Re-serialize the output at 0 to get
its id; hashing the rendered hex gives another. A box rendered on its own, not
inside a transaction, writes its values at 0.

`size` in a pool transaction's JSON is the length of the bytes the pool holds,
which are the bytes it relays.

**The JVM renders every transaction at its default version context (1, 1),**
whatever version its id was computed at. For a v4 block's transaction holding
an `Upcast` of a constant, its JSON drops the `Upcast`, so its hex no longer
hashes to its own `id`, and its `size` is the length of that write, not of the
bytes read. SANTA measured both on its v6.0.6 oracle. The node doesn't copy
this. The fork's parser refuses such JVM JSON with `InvalidTxId`.

Every response that renders a transaction lists each input's
context-extension entries in the order the transaction serializes them: the
order its id is computed over. A client that re-serializes the JSON, taking
the entries in the order given, must get the transaction's id back. JSON
object keys are otherwise unordered, but this map's order is part of the
transaction. The rendering must not re-sort it: a `serde_json::Value` object
built without `preserve_order` sorts its keys as strings, so `"8"` would come
after `"4"`, and `"10"` before `"2"`.

### Synced-state semantics

`/info` reports several height fields that diverge during sync:

- `fullHeight` — last fully validated height. Most consumers should key
  off this; a wallet that submits a transaction needs `fullHeight` to be
  current, not just `headersHeight`.
- `headersHeight` — highest header in the chain regardless of full-block
  validation. Equal to `fullHeight` at the tip.
- `downloadedHeight` — highest height for which all required block sections
  exist in the store. Rust-specific addition used by fastsync; after a state
  wipe `fullHeight` resets to 0 while `downloadedHeight` reflects what is
  still on disk.
- `maxPeerHeight` — highest header height announced by **any** peer in a
  `SyncInfo` message since process start. Present for JVM parity; consumers
  compute sync progress as `headersHeight / maxPeerHeight`.

`maxPeerHeight` has three properties a consumer must account for:

1. **Monotonic within a process.** It never decreases, including when the
   announcing peer disconnects or the chain reorgs below it. It is a
   high-water mark, not a live poll of current peers.
2. **Absent until the first `SyncInfo` is parsed**, rather than reported as
   `0`. A zero would assert "the network is at height 0", which is false and
   worse than saying nothing. Consumers must treat absence as "unknown" —
   the JVM types this field optional and so do we.
3. **Peer-supplied and unverified.** A peer announcing a bogus height inflates
   it for the lifetime of the process. It is advisory display data and must
   never feed a consensus, validation, or storage decision. The value that
   *does* drive behaviour (the fastsync gap) reads the same underlying atomic
   but is bounded by a threshold check.

`launchTime` — Unix epoch milliseconds at which this process began serving.
Constant for the process lifetime. Present for JVM parity; consumers derive
uptime as `currentTime - launchTime`.

`/info/wait?after=H` long-polls until `fullHeight > H` (30s deadline, then
204).

## Invariants

- The API crate never mutates chain, state, or store. It has read-only access.
- The API crate mutates mempool only through `mempool.process()`. No direct
  pool manipulation.
- A handler that reads the pool holds the mempool lock only to copy out
  what it needs. The P2P transaction intake waits on the same lock, so
  UTXO-set reads and serialization happen after the handler releases it.
  `process()` is the exception: it validates under the lock by design.
- All box IDs and transaction IDs in responses are hex-encoded (matching JVM).
- All header IDs in responses are hex-encoded (matching JVM).
- All ERG amounts are in nanoERG (1 ERG = 10^9 nanoERG).
- Request body size is bounded by `max_body_bytes`.
- List endpoints are bounded by hard `limit` maximums.
- No endpoint blocks indefinitely — all have timeouts (`/info/wait` caps at 30s).

## Transaction Submission Flow (Full Path)

```
Client
  │ POST /transactions { ErgoTransaction JSON }
  ▼
API handler
  │ 1. Deserialize JSON → this crate's request type: id, inputs, dataInputs,
  │    and outputs read as candidates, each with its boxId where given
  │ 1a. Build the Transaction at the activated script version
  │     (Transaction::new_from_vec_at); the JSON's id ≠ the built id, or an
  │     output's boxId, where given, ≠ the built output's id → 400
  │ 2. Serialize → tx_bytes (sigma format, for P2P), at the same version
  │ 2a. tx_bytes longer than ergo_mempool::MAX_TRANSACTION_SIZE → 400
  │ 3. Acquire state_context read lock
  │ 4. Build MempoolUtxoReader (UTXO state + mempool unconfirmed outputs)
  │ 5. Acquire mempool lock
  ▼
mempool.process(tx, tx_bytes, &utxo_reader, &state_context, None)
  │ ... 13-step validation (see mempool contract) ...
  ▼
ProcessingOutcome
  │ Accepted / Replaced / AlreadyInPool → return 200 "txId"
  │ DoubleSpendLoser / Declined / Invalidated → return 400 with reason
  ▼
Response: 200 "txId" or 400 { error, reason }
```

Steps 1a and 2 build and write the transaction at the activated script
version: the `ErgoTreeVersion` equal to `block_version() − 1` of the parameters
in effect, which is 3 since 6.0. That is the version a peer reads the relayed
bytes at (`facts/mempool.md` § P2P Transaction Intake). Built there, the
transaction's id is the id of the bytes the node relays. The id the handler
answers with, the id the node announces and the id a peer reads are then the
same, at any activated version. Built or written at another version, an
`Upcast` of a constant can make them differ. `Transaction::sigma_serialize_bytes()`
writes at the fork's highest version, which isn't the activated version once
the two differ.

**This crate translates the request; the fork's transaction JSON doesn't.**
The request body is this API's contract (`facts/openapi.yaml`
`TransactionSubmission`). The handler deserializes it into a type of its own,
built from the fork's public parts (`TxId`, `Input`, `DataInput`,
`ErgoBoxCandidate`, `BoxId`), and builds with `Transaction::new_from_vec_at`.
It doesn't deserialize `Transaction`. That is the fork's wallet-facing format:
it builds at version 3, and it drops the outputs' `boxId`s. Which version to
build at, and which ids to check, are this API's decisions, so they stay in
this crate.

**The id checks.** The JVM ignores the JSON's `id` and each output's `boxId`
and computes both itself: its decoder (sigmastate
`JsonCodecs.ergoLikeTransactionDecoder`) reads only inputs, data inputs and
output candidates. The node requires `id`, and answers 400 when it differs from
the id the node builds. An output's `boxId` is optional. Where the JSON gives
one, it must equal the id of the output the node builds, or 400. So a client
that computed an id other than the node's learns it before anything is pooled
or relayed. It doesn't meet a proof failure instead, or an id it didn't expect.
Outputs are read as candidates, as the JVM reads them, so `transactionId` and
`index` may be omitted and are ignored. `POST /transactions/check` builds and
checks the same way.

**The JVM builds a JSON submission at its default version context (1, 1),**
not at the version its bytes route parses at (`withVersions(protocolVersion,
protocolVersion)`). An `Upcast` of a constant drops out of the id there. So a
transaction signed over its version-3 message is invalid through the JVM's
`POST /transactions` and valid through its `/transactions/bytes`. SANTA
measured both on its v6.0.6 oracle. The node builds at the activated version,
the version it writes and relays the bytes at.

**The handler gets the version from `ChainAccess`, not from the state
context.** The parameters in effect belong to the chain, which has them from
startup. The state context exists only after the first applied block. So steps
1a to 2a need no state context. An oversized transaction or a mismatched id
gets 400 while the node is syncing too.

```rust
// on ChainAccess:
/// The script version activated by the parameters in effect after the last
/// applied block: `enr_chain::activated_tree_version` of their block
/// version. `None` only if that block version doesn't fit `u8`, which voting
/// can't produce. The handler answers 500.
fn activated_tree_version(&self) -> Option<ErgoTreeVersion>;
```

The main crate's adapter implements it from the chain's active parameters.
This crate still doesn't depend on `enr-chain`, and any chain rule it needs
comes through `ChainAccess` like this one.
`GET /blocks/{header_id}/validation-fragments` writes each transaction with
`ergo_validation::serialize_block_transaction` (`facts/validation.md` §
Section Parsing), not with a version of its own.

Step 2a is the JVM's size check (`TransactionsApiRoute.validateTransactionAndProcess`,
v6.0.6 :165-168): a transaction over `maxTransactionSize` is refused before it
is verified. `POST /transactions/check` takes the same step. The check comes
after the JSON parse, as the JVM's does, so it bounds what enters the mempool,
not what the parser reads; the request body limit (`max_body_bytes`) bounds
that.

P2P broadcast happens separately in the mempool task (main crate), which
broadcasts Inv type 2 to all connected peers for every accepted transaction.
The API handler returns immediately after `process()` — it does not wait
for broadcast. This is the same path used by P2P transaction relay, with
`source: None` instead of a peer ID.

## Does NOT Own

- Transaction validation — that's `ergo-validation` via mempool
- UTXO state — that's `enr-state` (read-only access via `SnapshotReader`)
- Block validation — that's the validation pipeline
- P2P message routing — that's the sync machine / pipeline
- Mining candidate assembly logic — specified in `facts/mining.md`, implemented
  in the `mining/` workspace crate. The API crate calls mining methods.
- Configuration parsing — that's the main crate
- TLS termination — that's the reverse proxy (nginx, caddy)

## Testing Strategy

1. **Handler unit tests:** Mock `ApiState` components. Verify correct HTTP
   status codes, response shapes, and error messages for each endpoint.
2. **Integration tests:** Start API server on a random port with real
   (but empty) chain/state/mempool. Submit transactions, query blocks,
   verify end-to-end flow.
3. **JVM compatibility:** For each implemented endpoint, capture the JVM
   node's response and verify the Rust node produces the same JSON shape.
   Field names, nesting, encoding must match.
4. **Input validation:** Malformed JSON, oversized bodies, invalid hex
   strings, negative offsets, limit > max. Verify 400 responses.
5. **Auth:** Requests to authenticated endpoints without key → 403.
   Wrong key → 403. Correct key → 200.

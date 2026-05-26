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
- `429` — rate limited (if HTTP-level rate limiting is added at a reverse proxy)
- `500` — internal error (component failure, should not happen)
- `503` — node is syncing or a required subsystem (mining, modifier pipeline, state context) is not yet available

The `reason` field is safe to show to users. The `detail` field may contain
internal information (stack traces, component errors) and should be omitted
in production or restricted to authenticated requests.

### Endpoint-specific error shapes

A small number of Rust-only endpoints intentionally use a different error
body. `openapi.yaml` documents these per endpoint; they share the contract
that `error` is a short code string rather than a numeric HTTP status:

- `/blocks/{id}/validation-fragments` — `{ error: "<code>", headerId?, message? }`,
  keyed off short codes (`invalid-header-id`, `block-not-found`, `block-pruned`,
  `header-serialize-failed`, `block-transactions-parse-failed`,
  `tx-bytes-to-sign-failed`) so the cross-validator harness can dispatch on
  them programmatically.
- `/debug/p2p-capture/*` — `{ error: "capture-disabled" | "bad-request",
  message? }` for the same reason.

These are not retrofitted onto the JVM-compat path; existing consumers of
the JVM API never see them.

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
  the modifier-pipeline shortcut used by co-located fastsync tooling.
- `GET /stats/p2p` (on a separate listener, default `127.0.0.1:9055`) —
  starts only when the operator config includes a `[stats]` section. A
  non-loopback bind logs a startup WARN; there is no auth.

## Configuration

```toml
[node.api]
bind_address = "0.0.0.0:9052"    # Listen address (JVM default: 9052 mainnet, 9053 testnet)
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
  `byTransactionId`, `getSnapshotsInfo`, `popowHeader`). JVM-aligned segments
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

`/info/wait?after=H` long-polls until `fullHeight > H` (30s deadline, then
204).

## Invariants

- The API crate never mutates chain, state, or store. It has read-only access.
- The API crate mutates mempool only through `mempool.process()`. No direct
  pool manipulation.
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
  │ 1. Deserialize JSON → Transaction
  │ 2. Serialize → tx_bytes (sigma format, for P2P)
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

P2P broadcast happens separately in the mempool task (main crate), which
broadcasts Inv type 2 to outbound peers for every accepted transaction.
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

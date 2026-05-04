# REST API Contract

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

The API server receives shared handles at construction. These are the same
handles used by the sync/validation pipeline — no duplication of state.

```rust
pub struct ApiState {
    /// Read access to the header chain (tip, headers by height/hash).
    pub chain: Arc<RwLock<HeaderChain>>,
    /// Read access to block storage (full blocks, transactions, extensions).
    pub store: Arc<BlockStore>,
    /// Read/write access to the mempool (submit txs, query pool).
    pub mempool: Arc<Mutex<Mempool>>,
    /// Read access to UTXO state (box lookups by ID).
    pub utxo_state: Arc<SnapshotReader>,
    /// Read access to P2P peer state (connected peers, known peers).
    pub peers: Arc<RwLock<PeerManager>>,
    /// Current blockchain parameters (from extension voting).
    pub parameters: Arc<RwLock<Parameters>>,
    /// Node configuration (network type, version string, features).
    pub node_config: NodeConfig,
    /// Optional API key hash (Blake2b256). None = no auth required.
    pub api_key_hash: Option<[u8; 32]>,
    /// Builder for ErgoStateContext (tip header + preceding + params).
    /// Rebuilt on each new block; cached for mempool validation.
    pub state_context: Arc<RwLock<ErgoStateContext>>,
}
```

### State Context Lifecycle

`state_context` is rebuilt whenever the validated tip advances:
1. Validator confirms block at height H
2. Main crate builds `ErgoStateContext` from header H + 10 preceding headers + current parameters
3. Stores in `Arc<RwLock<ErgoStateContext>>`
4. Mempool task and API both read from this same handle

This means mempool validation always uses the state context of the latest
validated block. During sync (tip advancing rapidly), this is rebuilt
frequently — the `RwLock` ensures readers see a consistent snapshot.

## Endpoints

### Node Information

#### `GET /info`

Node status and chain state. No authentication.

**Response 200:**
```json
{
  "name": "ergo-node-rust",
  "appVersion": "0.1.0",
  "network": "testnet",
  "fullHeight": 271234,
  "headersHeight": 271240,
  "downloadedHeight": 271238,
  "bestFullHeaderId": "0000abcd...",
  "bestHeaderId": "0000efgh...",
  "stateRoot": "01abcdef...",
  "stateType": "utxo",
  "peersCount": 8,
  "unconfirmedCount": 42,
  "isMining": false,
  "currentTime": 1712400000000
}
```

**Source:** chain (heights, header IDs), mempool (unconfirmed count),
peers (peer count), node_config (name, version, network, state type),
sync layer (downloaded_height via shared atomic).

`downloadedHeight` is the highest height where all required block sections
are in the store. Used by fastsync to avoid re-fetching sections that
already exist. Distinct from `fullHeight` (validated) — after a state wipe,
`fullHeight` resets to 0 but `downloadedHeight` reflects what's still in the
modifiers store.

---

### Blocks

#### `GET /blocks?offset=0&limit=50`

List block header IDs from chain tip, newest first.

**Query params:**
- `offset`: skip count (default 0)
- `limit`: max results (default 50, max 100)

**Response 200:** `["headerId1", "headerId2", ...]`

**Source:** chain (header IDs by descending height).

#### `GET /blocks/at/{height}`

Block header IDs at a specific height. Returns array (multiple possible during temporary forks in JVM; we return single-element array for compatibility).

**Response 200:** `["headerId"]`
**Response 404:** height beyond chain tip.

**Source:** chain (header at height).

#### `GET /blocks/{headerId}`

Full block: header + transactions + AD proofs + extension.

**Response 200:** Full block JSON (JVM-compatible `ErgoFullBlock` shape).
**Response 404:** header not found.

**Source:** store (block sections by header ID).

#### `GET /blocks/{headerId}/header`

Block header only.

**Response 200:** Header JSON.
**Response 404:** header not found.

**Source:** chain (header by ID) or store.

#### `GET /blocks/{headerId}/transactions`

Transactions in a block.

**Response 200:** `{ "headerId": "...", "transactions": [...] }`
**Response 404:** header not found or transactions not stored.

**Source:** store (block transactions by header ID).

#### `GET /blocks/lastHeaders/{count}`

Last N block headers, newest first.

**Query params:**
- `count`: path param (max 100)

**Response 200:** `[header1, header2, ...]`

**Source:** chain (headers from tip descending).

---

### Transactions

#### `POST /transactions`

Submit a transaction to the mempool and broadcast to peers.

**Request body:** JSON `ErgoTransaction` (JVM-compatible format).

**Response 200:** `"txId"` (hex-encoded transaction ID).
**Response 400:** Validation failure — includes reason.

**Processing:**
1. Deserialize and validate JSON structure
2. Compute tx_bytes (sigma serialization for P2P)
3. Acquire mempool lock
4. Read current `state_context`
5. Call `mempool.process(tx, tx_bytes, &utxo_reader, &state_context, None)`
6. Return tx_id or error reason

P2P broadcast of accepted transactions is handled by the mempool task in the
main crate, not by the API. The mempool task broadcasts Inv messages for all
accepted transactions regardless of source (P2P or API).

**Rate limiting:** Local submissions (`source: None`) bypass mempool rate limits.
HTTP-level rate limiting (per-IP) is a deployment concern (reverse proxy), not
implemented in the API crate.

#### `POST /transactions/check`

Validate a transaction without adding to mempool or broadcasting.
Same validation as `POST /transactions` but discards the result.

**Request body:** JSON `ErgoTransaction`.
**Response 200:** `"txId"` (valid).
**Response 400:** Validation failure — includes reason.

#### `GET /transactions/unconfirmed?offset=0&limit=50`

List unconfirmed transactions from mempool, ordered by priority (highest fee weight first).

**Query params:**
- `offset`: skip count (default 0)
- `limit`: max results (default 50, max 100)

**Response 200:** `[ErgoTransaction, ...]`

**Source:** mempool (`all_prioritized()`, sliced).

#### `GET /transactions/unconfirmed/transactionIds`

All unconfirmed transaction IDs.

**Response 200:** `["txId1", "txId2", ...]`

**Source:** mempool (`tx_ids()`).

#### `GET /transactions/unconfirmed/byTransactionId/{txId}`

Single unconfirmed transaction by ID.

**Response 200:** `ErgoTransaction`
**Response 404:** not in mempool.

**Source:** mempool (`get(tx_id)`).

#### `HEAD /transactions/unconfirmed/{txId}`

Check if transaction is in mempool. Returns HTTP status only.

**Response 200:** in mempool.
**Response 404:** not in mempool.

#### `GET /transactions/getFee?waitTime=1&txSize=100`

Recommended fee for target confirmation time.

**Query params:**
- `waitTime`: target wait in blocks (default 1)
- `txSize`: transaction size in bytes (default 100)

**Response 200:** `{ "fee": 1100000 }` (nanoERG)

**Source:** mempool (`recommended_fee()`). Returns 400 if insufficient history.

#### `GET /transactions/waitTime?fee=1000000&txSize=100`

Expected confirmation wait time for a given fee.

**Query params:**
- `fee`: fee in nanoERG
- `txSize`: transaction size in bytes

**Response 200:** `{ "waitTime": 2 }` (blocks)

**Source:** mempool (`expected_wait_time()`).

#### `GET /transactions/poolHistogram?bins=10`

Fee distribution histogram of mempool contents.

**Query params:**
- `bins`: number of histogram buckets (default 10, max 50)

**Response 200:** `[{ "minFee": 1000000, "maxFee": 2000000, "count": 15 }, ...]`

**Source:** mempool (`fee_histogram()`).

---

### UTXO

#### `GET /utxo/byId/{boxId}`

Look up a box in the confirmed UTXO set only.

**Response 200:** `ErgoBox` JSON.
**Response 404:** box not found (spent or never existed).

**Source:** utxo_state (`box_by_id()`). Deserialize AVL+ value to `ErgoBox`.

#### `GET /utxo/withPool/byId/{boxId}`

Look up a box in confirmed UTXO set OR unconfirmed mempool outputs.

**Response 200:** `ErgoBox` JSON.
**Response 404:** not found in either.

**Source:** utxo_state first, then mempool (`unconfirmed_box()`).

#### `POST /utxo/withPool/byIds`

Batch lookup of boxes from UTXO set + mempool.

**Request body:** `["boxId1", "boxId2", ...]` (max 100).
**Response 200:** `[ErgoBox | null, ...]` (positional, null for missing).

**Source:** utxo_state + mempool, per box.

---

### Mining

Full specification: `facts/mining.md`. The mining module is the most complex
piece of the API — block candidate assembly, state root computation, emission
transactions, and PoW validation. Requires UTXO mode and a configured miner
public key. Returns 503 when mining is not available.

#### `GET /mining/candidate`

Get current mining candidate (block template). The miner polls this endpoint
to get work, then submits solutions via `POST /mining/solution`.

**Response 200:**
```json
{
  "msg": "0102abcd...",
  "b": "1234567890",
  "h": 271235,
  "pk": "02abcdef...",
  "proof": {
    "msgPreimage": "...",
    "txProofs": []
  }
}
```

**Response 503:** mining not configured, digest mode, or still syncing.

**Source:** chain (tip header, difficulty), mempool (tx selection), validator
(state root via `proofs_for_transactions()`), parameters (nBits, version),
ergo-nipopow (interlinks for extension).

**Caching:** Returns cached candidate if chain tip is unchanged and TTL has
not expired. Regenerates on tip change or TTL expiry. See `facts/mining.md`
for the full 9-step candidate assembly flow.

#### `POST /mining/solution`

Submit a mining solution (Autolykos v2 PoW).

**Request body:**
```json
{
  "pk": "02abcdef...",
  "w": "0256...",
  "n": "00000123...",
  "d": "12345..."
}
```

For Autolykos v2, only `n` (8-byte nonce) is required. `pk`, `w`, `d`
are optional and use protocol defaults.

**Response 200:** solution accepted, block assembled and broadcast to peers.
**Response 400:** invalid solution, stale candidate, or no cached candidate.

**Authentication:** Required when `api_key_hash` is configured.

**Processing:**
1. Validate and deserialize solution fields
2. Assemble full header from cached candidate + solution
3. Verify PoW: `pow_hit(header) < target(nBits)`
4. Assemble full block (header + transactions + extension + AD proofs)
5. Validate block via normal validation pipeline
6. Apply block (advance validator state)
7. Broadcast to P2P network
8. Invalidate cached candidate

#### `GET /mining/rewardAddress`

Miner's reward address derived from the configured public key.

**Response 200:**
```json
{ "rewardAddress": "3WwbzW..." }
```

**Response 503:** mining not configured (no miner PK).

**Source:** `MinerConfig.miner_pk` → P2S address derivation.

---

### Peers

#### `GET /peers/all`

All known peers (connected + disconnected).

**Response 200:**
```json
[
  {
    "address": "1.2.3.4:9030",
    "name": "ergo-mainnet-6.0.3",
    "lastSeen": 1712400000000,
    "connectionType": null
  }
]
```

**Source:** peers (known peer list).

#### `GET /peers/connected`

Currently connected peers.

**Response 200:**
```json
[
  {
    "address": "1.2.3.4:9030",
    "name": "ergo-mainnet-6.0.3",
    "lastSeen": 1712400000000,
    "connectionType": "Outgoing"
  }
]
```

**Source:** peers (connected peer list).

#### `GET /peers/status`

Network status summary.

**Response 200:**
```json
{
  "lastIncomingMessage": 1712400000000,
  "currentNetworkTime": 1712400001000
}
```

---

### Emission

#### `GET /emission/at/{height}`

Emission schedule data at a given height.

**Response 200:**
```json
{
  "minerReward": 67500000000,
  "totalCoinsIssued": 97739925000000000,
  "totalRemainCoins": 2260075000000000
}
```

**Source:** Computed from `EmissionRules` (already ported to sigma-rust).
Pure calculation, no state access.

---

### Debug

#### `GET /debug/memory`

Memory diagnostics for ops. Process-level (RSS, peak, anon vs file)
+ jemalloc stats (allocated, active, resident, retained, metadata) +
component-level estimates (chain header count and rough size,
mempool tx count). Useful for tracking RSS vs allocated divergence
(THP retention, fragmentation, etc.).

**Response 200:**
```json
{
  "process": {
    "rssAnonBytes": 882192384,
    "rssFileBytes": 15355904,
    "rssTotalBytes": 897548288,
    "rssPeakBytes": 1730408448,
    "vmSizeBytes": 2587922432,
    "pssBytes": 895396864
  },
  "jemalloc": {
    "allocatedBytes": 411834448,
    "activeBytes": 446201856,
    "residentBytes": 477978624,
    "retainedBytes": 1441718272,
    "metadataBytes": 18261136
  },
  "components": {
    "chainHeaderEstimateBytes": 1417688800,
    "chainHeaderCount": 1772111,
    "mempoolTxCount": 0
  }
}
```

**Notes:** `jemalloc` block is `null` when not built with the
`jemalloc` feature. `process.*` reads `/proc/self/status` and
`/proc/self/smaps_rollup` — Linux-only.

---

## Endpoints NOT Implemented (First Release)

| JVM Endpoint Family | Reason |
|---|---|
| `/wallet/*` | No embedded wallet. Use Nautilus, SAFEW, or ergo-lib directly. |
| `/scan/*` | No scanning/tracking subsystem. |
| `/script/*` | Script compilation/execution is a dev tool, not a node function. Add later. |
| `/blockchain/*` | Requires extra indexing infrastructure. Add later. |
| `/nipopow/*` | Separate roadmap item (NiPoPoW support). |
| `/node/shutdown` | Operational concern. Signal-based shutdown is sufficient. |
| `/utils/*` | Utility functions. Add later if wallets need them. |
| `POST /blocks` | Block submission via API (miners use `/mining/solution`). |

## Error Handling

All errors return a JSON body matching the JVM format:

```json
{
  "error": 400,
  "reason": "Human-readable error message",
  "detail": "Optional technical detail"
}
```

**Error codes:**
- `400` — bad request (malformed JSON, invalid tx, missing params)
- `404` — resource not found (unknown header ID, box ID, tx ID)
- `403` — authentication required but missing or invalid
- `429` — rate limited (if HTTP-level rate limiting is added)
- `500` — internal error (component failure, should not happen)

The `reason` field is safe to show to users. The `detail` field may contain
internal information (stack traces, component errors) and should be omitted
in production or restricted to authenticated requests.

## Authentication

Optional API key via `api_key` header. When `api_key_hash` is configured:

1. Extract `api_key` header value
2. Compute `Blake2b256(api_key_bytes)`
3. Constant-time compare with stored `api_key_hash`
4. On mismatch: 403

**Authenticated endpoints:**
- `POST /mining/solution`
- Any future admin/control endpoints

**Unauthenticated endpoints:** everything else. Read-only queries and
transaction submission are always open (matching JVM behavior).

## Configuration

```toml
[node.api]
bind_address = "0.0.0.0:9052"    # Listen address (JVM default: 9052 mainnet, 9053 testnet)
api_key_hash = ""                 # Blake2b256 hex of API key (empty = no auth)
request_timeout_ms = 5000         # Per-request timeout
max_body_bytes = 2_097_152        # Max request body (2 MB)
```

## Invariants

- The API crate never mutates chain, state, or store. It has read-only access.
- The API crate mutates mempool only through `mempool.process()`. No direct
  pool manipulation.
- All box IDs and transaction IDs in responses are hex-encoded (matching JVM).
- All header IDs in responses are hex-encoded (matching JVM).
- All ERG amounts are in nanoERG (1 ERG = 10^9 nanoERG).
- Request body size is bounded by `max_body_bytes`.
- List endpoints are bounded by hard `limit` maximums.
- No endpoint blocks indefinitely — all have timeouts.

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
  │ Accepted / Replaced → return 200 "txId"
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

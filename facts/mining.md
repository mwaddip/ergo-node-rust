# Mining API Contract

## Component: `mining/` (workspace crate, in-repo)

Block template assembly and solution acceptance for external GPU miners.
The mining crate orchestrates mempool, state, chain, and validation
components to produce valid block candidates, and verifies PoW solutions
to publish new blocks. The API crate is a thin HTTP facade over this —
it deserializes requests, calls mining methods, serializes responses.

Miners poll `GET /mining/candidate` for work, compute Autolykos v2 nonces
locally, and submit solutions via `POST /mining/solution`. The node does not
mine — it produces templates and validates solutions.

Requires UTXO mode. Digest-mode nodes cannot compute new state roots and
therefore cannot generate block candidates.

Primary consumers: API crate (HTTP handlers call mining methods), main crate
(wires mining state into API). Primary dependencies: chain (parent header,
difficulty), mempool (transaction selection), validation (state root
computation via UtxoValidator), parameters (protocol rules, block limits).

## SPECIAL Profile

```
S9  P9  E6  C7  I8  A7  L9
```

Consensus-critical assembly (S9) — a malformed candidate wastes miner hashpower;
a malformed block gets rejected by peers and damages reputation. External-facing
(P9) — the solution endpoint accepts data from the network. Architecture (I8) —
the candidate generator coordinates many components; wrong boundaries mean tangled
code. Edge cases (L9) — epoch boundary voting, emission box transitions, empty
mempool, zero-fee blocks, max-cost blocks. Not persistent (E6) — crash means the
current candidate is lost; miners just poll for a new one.

## Design Principles

- **UTXO mode only.** Mining requires computing the new state root after
  applying transactions. This needs the persistent AVL+ prover. Digest-mode
  nodes cannot mine. The `/mining/*` endpoints return 503 if the node is in
  digest mode.
- **Miners poll, node doesn't push.** `GET /mining/candidate` is the pull
  interface. No WebSocket, no push notifications. Miners poll at their preferred
  interval. Candidate caching avoids redundant recomputation.
- **No embedded wallet.** The miner's public key must be configured. Without it,
  the mining endpoints return 503. The node never generates or manages keys.
- **Candidate = cached snapshot.** The candidate is regenerated only when the
  chain tip changes or a configurable interval expires. Polls between
  regenerations return the cached candidate.
- **Components do the work.** The mining crate is mechanical assembly.
  Transaction validation is ergo-lib. State root computation is
  ergo_avltree_rust. PoW verification is ergo-chain-types. Emission rules
  are sigma-rust. The mining crate calls them in the right order.
- **No HTTP.** The mining crate knows nothing about HTTP, JSON, or axum.
  It exposes Rust methods that the API crate's handlers call. Same
  relationship as mempool-to-API: the crate owns the logic, the API owns
  the wire format.

## Dependencies

- `ergo-lib` — `Transaction`, `ErgoBox`, `EmissionRules`, `ErgoTreePredef`,
  `TxBuilder` (coinbase construction)
- `ergo-chain-types` — `Header`, `AutolykosPowScheme`, `AutolykosSolution`,
  `ADDigest`, header serialization
- `ergo-nipopow` — interlink vector computation for extension section
- `enr-chain` — `pow_target(n_bits)`, the single definition of the Autolykos
  mining target served as `WorkMessage.b`. The crate consumed
  `ergo-chain-types` directly and re-derived the target itself until v0.8.0,
  which is how it came to serve the difficulty instead. No cycle: `chain/` has
  no in-repo path dependencies.
- `ergo-validation` — `compute_state_changes`, `validate_single_transaction`,
  `build_state_context`, `Parameters`
- `ergo_avltree_rust` — via UtxoValidator (temporary prover operations for
  state root computation)
- `blake2` — Blake2b256 for header hash (WorkMessage `msg` field)
- `serde` — `Serialize` derives on the wire types
- `serde_json` (feature `raw_value`) — ONLY for `WorkMessage.b`'s
  `serialize_with`: `b` ranges to ~2^256, which serde's numeric data model
  cannot represent, so it is emitted as a bare JSON number via
  `serde_json::value::RawValue` wrapping the decimal digits (added
  2026-06-12 with the b-as-number fix). The API crate still does the
  HTTP-level `Json(work)` encoding; mining only owns this one
  number-shape escape hatch.

Does NOT depend on: `axum`, P2P transport, sync state machine,
block storage internals, redb. The mining crate is HTTP-agnostic — all
HTTP/routing concerns belong to the API crate (the lone `serde_json`
use above is a serialization-shape detail, not HTTP).

## Core Types

### `MinerConfig`

```rust
pub struct MinerConfig {
    /// Miner's public key (compressed group element, 33 bytes).
    /// Required for mining. Loaded from config at startup.
    pub miner_pk: ProveDlog,
    /// Miner reward delay in blocks (mainnet: 720, testnet: 720).
    /// Reward boxes are time-locked for this many blocks.
    pub reward_delay: i32,
    /// Voting preferences: 3 bytes [soft_fork_vote, param_vote_1, param_vote_2].
    /// [0, 0, 0] = no votes.
    pub votes: [u8; 3],
    /// Maximum block candidate lifetime before forced regeneration.
    pub candidate_ttl: Duration,
    /// EIP-27 re-emission rules. Carried in config (not derived inside
    /// `generate_candidate`) so the network-policy decision lives at the
    /// configuration boundary. Construct from the chain's network type at
    /// config-load time. Currently hardcoded to mainnet at the call sites
    /// in `src/main.rs` until testnet/devnet network detection is wired in.
    pub reemission_rules: ReemissionRules,
}
```

### `CandidateBlock`

```rust
/// All components needed to assemble a full block once a PoW solution arrives.
/// Cached between miner polls. Invalidated when chain tip changes.
pub struct CandidateBlock {
    /// Parent block header.
    pub parent: Header,
    /// Block version.
    pub version: u8,
    /// Compact-encoded **difficulty** for this block (not the mining target —
    /// see `WorkMessage.b`). `u32`, matching `Header.n_bits`; serialized as 4
    /// raw big-endian bytes, the one non-VLQ integer in the header.
    pub n_bits: u32,
    /// New state root after applying selected transactions.
    pub state_root: ADDigest,
    /// Serialized AD proofs for the state transition.
    pub ad_proof_bytes: Vec<u8>,
    /// Ordered transactions: [emission_tx, mempool_txs..., fee_tx].
    pub transactions: Vec<Transaction>,
    /// Block timestamp: max(now_ms, parent.timestamp + 1).
    pub timestamp: u64,
    /// Extension section (interlinks + voting).
    pub extension: ExtensionCandidate,
    /// Voting bytes (3 bytes).
    pub votes: [u8; 3],
}
```

### `WorkMessage`

```rust
/// Data sent to the miner via GET /mining/candidate.
/// The miner computes a nonce such that pow_hit(msg, nonce, height) < b.
pub struct WorkMessage {
    /// Blake2b256 hash of the serialized HeaderWithoutPow.
    pub msg: [u8; 32],
    /// Autolykos target: `q / decode_compact_bits(nBits)`, i.e.
    /// `enr_chain::pow_target(nBits)` — NOT the decoded nBits itself, which
    /// is the difficulty. Solution must satisfy hit < b.
    pub b: BigInt,
    /// Block height (used in Autolykos v2 index calculation).
    pub h: u32,
    /// Miner's public key.
    pub pk: EcPoint,
    /// Header pre-image and transaction membership proofs.
    /// Allows miners to verify they're working on a real candidate.
    pub proof: ProofOfUpcomingTransactions,
}
```

The struct above is the SEMANTIC shape. The impl (`mining/src/types.rs`)
stores the wire-ready encodings directly — `msg`/`pk`/`proof.msgPreimage`
as hex `String`s, `b` as a decimal `String` (serialized to a bare number,
below), `h` as `u32` — so it serializes straight to the JSON response
without a separate encoding pass. Do not read the semantic types
(`BigInt`/`EcPoint`/`[u8; 32]`) as the field types; they describe meaning,
the JSON block describes the wire.

JSON response (basic candidate — no mandatory tx proofs, the only case
the node produces today):

```json
{
  "msg": "hex string (32 bytes)",
  "b": 748014723576678314041035877227113663879264849498014394977645987,
  "h": 271235,
  "pk": "hex string (33 bytes compressed)"
}
```

`proof` is **OMITTED** when there are no mandatory transaction proofs (see
below). When present (a `candidateWithTxs`-style candidate), it is:

```json
  "proof": { "msgPreimage": "hex (header without PoW)", "txProofs": [...] }
```

**`b` is a bare JSON NUMBER, not a quoted string (corrected 2026-06-12 —
JVM-compat serve bug).** JVM `ExternalCandidateBlock` encodes `b` as a
`BigInt` via circe → an arbitrary-precision unquoted JSON number
(`ergo-core .../examples/LiteClientExamples.scala:27`: `"b" :
748014723576678314041035877227113663879264849498014394977645987`).
Emitting it as a string (the prior contract + impl) breaks every
JVM-compatible miner/pool: their jsmn-based parsers expect a numeric
token and reject the candidate ("Jsmn failed to parse"). `b` ranges up
to the secp256k1 group order (~2^256), so it does NOT fit u64 —
serialize it as an arbitrary-precision JSON number (serde_json
`raw_value`: a `serialize_with` that wraps the decimal digits in a
`RawValue`, emitting them verbatim/unquoted; or the `arbitrary_precision`
feature). This was invisible until the node was first driven by a real
external miner (the "untested as a mining source" serve-direction gap —
same class as the rust-peer serve bugs). `msg`/`pk` stay hex strings,
`h` stays a number.

**`proof` is OPTIONAL and OMITTED when empty (corrected 2026-06-12 — the
SECOND serve bug, same root cause).** JVM `WorkMessage` encoder
(`ergo-core .../mining/WorkMessage.scala:27-39`) builds the object then
`.collect { case (name, Some(value)) => ... }` — `proof` (and `h`) come
from `Option`s and are **dropped from the JSON entirely** when `None`, NOT
emitted as `null`. `proofsForMandatoryTransactions` is `None` for a
candidate with no mandatory transactions (our only case — the block
carries just the emission tx), so the JVM basic candidate is
`{msg, b, h, pk}`. Our node emitted a full `proof` object
unconditionally; that inflates the candidate to ~15 JSON tokens, and the
reference Autolykos2 miners parse with a fixed `#define REQ_LEN 11`
token buffer (`Autolykos2_NV_Miner secp256k1/include/definitions.h`) —
jsmn returns `NOMEM`, "Jsmn failed to parse latest block", and the miner
never mines. Fix: model `proof` as `Option`, `#[serde(skip_serializing_if
= "Option::is_none")]`, and set it `None` while there are no mandatory tx
proofs. (`h` is likewise `Option` JVM-side, omitted for v1 blocks; our
always-present `h` is correct for v2+ — the only versions the node
serves — and the miner expects it. Leave `h` present.)

### `ProofOfUpcomingTransactions`

```rust
pub struct ProofOfUpcomingTransactions {
    /// The full serialized header without PoW fields — the bytes that hash to `msg`.
    /// Miners can verify: Blake2b256(msg_preimage) == msg.
    pub msg_preimage: Vec<u8>,
    /// Merkle inclusion proofs for mandatory transactions.
    /// Empty for first release — full implementation deferred.
    pub tx_proofs: Vec<TransactionMembershipProof>,
}
```

### `ExtensionCandidate`

```rust
/// Key-value pairs for the block extension section.
pub struct ExtensionCandidate {
    /// Extension fields as (key, value) pairs.
    /// Keys: 2 bytes. Values: variable length.
    pub fields: Vec<([u8; 2], Vec<u8>)>,
}

impl ExtensionCandidate {
    /// Merkle root digest of extension fields (for header.extension_root).
    pub fn digest(&self) -> [u8; 32];
}
```

## Mining State

The mining module maintains its own state, separate from `ApiState`.
Passed to mining handlers alongside ApiState.

```rust
pub struct MiningState {
    /// Miner configuration (PK, reward delay, votes, candidate TTL).
    pub config: MinerConfig,
    /// Two-slot candidate cache + solved-block latch (see CandidateGenerator).
    /// Arc-shared with the main crate's Validator, whose post-apply hook
    /// calls `on_block_applied` for every applied block.
    pub generator: Arc<CandidateGenerator>,
    /// Access to the UTXO validator for state root computation.
    /// Same validator used by the sync pipeline. UtxoValidator specifically
    /// (not Box<dyn BlockValidator>) because proofs_for_transactions() and
    /// emission_box_id() are UtxoValidator-specific methods.
    pub validator: Arc<Mutex<UtxoValidator>>,
    /// Read access to header chain (tip, difficulty).
    pub chain: Arc<RwLock<HeaderChain>>,
    /// Read/write access to mempool (tx selection).
    pub mempool: Arc<Mutex<Mempool>>,
    /// Read access to UTXO state (emission box lookup without locking prover).
    pub utxo_reader: Arc<SnapshotReader>,
    /// Current blockchain state context (rebuilt on each validated block).
    pub state_context: Arc<RwLock<Option<ErgoStateContext>>>,
}

struct CachedCandidate {
    /// The assembled candidate block.
    block: CandidateBlock,
    /// The WorkMessage derived from the candidate.
    work: WorkMessage,
    /// Chain tip height when this candidate was generated.
    tip_height: u32,
    /// When this candidate was generated.
    created: Instant,
}

/// Stateful candidate manager. Two candidate slots + a solved-block latch,
/// mirroring the JVM's CandidateGenerator actor state
/// (cachedCandidate / cachedPreviousCandidate / solvedBlock —
/// CandidateGenerator.scala:91-101, JVM PR 2291 semantics).
pub struct CandidateGenerator {
    pub config: MinerConfig,
    /// Current candidate. None until generated, or after invalidate().
    cached: RwLock<Option<CachedCandidate>>,
    /// The immediately superseded candidate. A miner still hashing the old
    /// work message can submit its solution against this slot.
    previous: RwLock<Option<CachedCandidate>>,
    /// Solved-block latch: set when a solution is accepted and the block is
    /// handed to the (async) submitter; suppresses further solution
    /// acceptance until block application clears it. JVM: solvedBlock.
    solved: RwLock<Option<SolvedLatch>>,
}

struct SolvedLatch {
    /// Header id of the block assembled from the accepted solution.
    header_id: BlockId,
    /// Height of that block.
    height: u32,
}
```

### Lifecycle API

```rust
impl CandidateGenerator {
    /// Move current → previous, store the fresh candidate as current.
    pub fn cache_candidate(&self, block: CandidateBlock, work: WorkMessage, tip_height: u32);

    /// Poll-time read: current WorkMessage iff tip still matches and TTL fresh.
    /// `current_tip_height` MUST be the **VALIDATED** height — the same
    /// source the mining task passes to `cache_candidate` (the candidate is
    /// built on the validated tip's state root). See the height-source note
    /// under "GET /mining/candidate" below.
    pub fn cached_work(&self, current_tip_height: u32) -> Option<WorkMessage>;

    /// Current / previous CandidateBlock for solution validation.
    pub fn cached_block(&self) -> Option<CandidateBlock>;
    pub fn previous_block(&self) -> Option<CandidateBlock>;

    /// Mempool-change push invalidation: current moves to previous, current
    /// becomes None. In-flight solutions against the superseded candidate
    /// stay valid via `previous`. (JVM: forced GenerateCandidate sets
    /// cachedPreviousCandidate — CandidateGenerator.scala:182.)
    pub fn invalidate(&self);

    /// Atomically claim the latch IFF unset; false means another solution
    /// won the race and the caller must reject (400). This is the
    /// authoritative gate, taken BEFORE the block is handed to the
    /// submitter — API handlers run concurrently (the JVM actor serializes;
    /// axum does not), so a non-atomic check-then-set spanning the handler
    /// would let two simultaneously valid solutions both pass. At testnet
    /// difficulty 1 that is not a theoretical window.
    pub fn try_mark_solved(&self, header_id: BlockId, height: u32) -> bool;

    /// Release a claimed latch — ONLY for the submit-failure path (500/503):
    /// the block never left the node, so holding the latch would wedge
    /// solution acceptance until the next applied block.
    pub fn clear_solved(&self);

    /// True while a solved block awaits application. Cheap early rejection
    /// in the handler; `try_mark_solved` is the gate that counts.
    pub fn solved_pending(&self) -> bool;

    /// Block-application hook — the lifecycle counterpart of the JVM's
    /// FullBlockApplied handler (CandidateGenerator.scala:141-150). Called
    /// by the main crate for EVERY applied block (own or peer):
    ///   - drop current/previous candidates whose `parent_id != applied.id`
    ///     (they no longer build on the tip);
    ///   - clear the latch when `applied.height >= latch.height` (our block
    ///     landed, or a competitor superseded it — stale either way).
    ///
    /// KNOWN DIVERGENCE (chosen): the latch clear is height-based; the JVM's
    /// needNewSolution is parent-id-based. Outcomes match in all normal
    /// paths (incl. solve-before-parent-applies); they diverge only when a
    /// reorg re-applies blocks below the latch height, where we hold the
    /// latch longer. Conservative direction — suppresses solutions, never
    /// double-submits. Revisit only if JVM-exact behavior becomes a goal.
    pub fn on_block_applied(&self, applied_id: &BlockId, applied_height: u32);
}
```

### Validator Access

`MiningState.validator` is `Arc<Mutex<UtxoValidator>>` — the same instance
used by the sync pipeline. Mining is only available in UTXO mode; the main
crate constructs `MiningState` only when it has a `UtxoValidator`, never
for `DigestValidator`. Candidate generation acquires the lock briefly for
`proofs_for_transactions()`. During this window, block validation is blocked.

For first release, this contention is acceptable. Candidate generation
is infrequent (on tip change or TTL expiry) and proof computation is fast
(proportional to selected transaction count, not UTXO set size). If
contention becomes an issue, the solution is a dedicated read-only prover
sharing the storage backend.

## Candidate Assembly

### Regeneration triggers

A cached candidate is rebuilt when **either**:

- the validated tip advances, or
- **the cached candidate has aged past `candidate_ttl`** (default 15 s).

⚠ **The second trigger was missing until v0.8.0, and its absence made mining
unusable.** Regeneration was gated on tip change alone, while `cached_work`
invalidates on TTL — so `/mining/candidate` served work for 15 s after each
block and returned **503 for the remaining ~105 s** of a mainnet interval. A
miner could fetch work for roughly 12% of each block. The config key is
documented as "maximum candidate lifetime before forced regeneration"; only the
invalidation half existed.

Regenerating on expiry also keeps the candidate's `timestamp` current and, with
transaction selection wired, picks up transactions that arrived *after* the
last block — rather than repeatedly mining the near-empty mempool that block
left behind. **That failure mode — full mempool, empty blocks — is a caching
and trigger problem, not a selection problem**, and wiring selection without
this fix would not have produced fuller blocks.

### Ownership — `generate_candidate` is the only entry point

**Callers ask `mining` for a candidate. They do not assemble one.**
`ergo_mining::generate_candidate` performs steps 1–8 below and is the sole
supported path; constructing a `CandidateBlock` field-by-field outside this
crate is a contract violation regardless of whether the result happens to be
well-formed.

⚠ **This was violated until v0.8.0, and the violation is what made steps 3 and
4 dead code. Fixed in `3aecc7f`; the history stays because the failure was
invisible for months.** `src/main.rs` built `CandidateBlock { .. }` inline — parent,
n_bits, state root, AD proofs, its own copy of the
`max(now, parent.timestamp + 1)` rule, `transactions: vec![emission_tx]` — and
never called `generate_candidate`. So the crate's entry point had no production
caller, and `select_transactions` and `build_fee_tx` had none either: the path
that would have called them was not the path that ran. Mined blocks carried the
emission transaction alone and miners collected no fees.

The lesson generalises past mining: **a crate function with no caller outside
its own tests is not "not yet wired", it is a second implementation waiting to
diverge.** Here the timestamp rule already existed twice.

`generate_candidate` therefore needs everything steps 3–4 require, passed in
rather than reached for — the caller owns the mempool and the UTXO set, the
crate owns the assembly:

- the candidate transactions, prioritised (`mempool.all_prioritized()`), each
  with its serialized size
- the **active** `Parameters` — `stateContext.currentParameters` in JVM terms,
  not `boundary_params`, which is the epoch-boundary *proposal* and is a
  different input
- **ancestor headers**, newest first, for the upcoming-block `ErgoStateContext`
- a UTXO lookup, for validating each candidate against accumulated state

⚠ **The ancestor headers are not optional padding.** Selection validates each
transaction against a context built by `build_state_context(stub,
preceding_headers, parameters)`. Built from the parent alone that context
exposes a **one-header** `CONTEXT.headers` window, where block validation
exposes up to ten. A script reading `headers[5]` would then fail *during
selection* and the transaction would be evicted from the mempool as invalid —
a valid transaction destroyed by a selection-only artefact. Pass
`chain.headers_from(parent.height - 9, 10)` reversed, minus the parent; the
parent is prepended internally.

`generate_candidate` returns `GeneratedCandidate { block, work, invalid_txs }`.
⚠ **`invalid_txs` MUST be routed to the mempool for eviction** or Step 3.6 is
silently lost — the crate identifies unusable transactions and has no way to
remove them itself.

### Overview

```
chain tip changes (or candidate TTL expires)
  │
  ▼
1. Get parent header + parameters ───── chain, validator
  │
  ▼
2. Build emission transaction ───────── EmissionRules, UTXO state
  │
  ▼
3. Select mempool transactions ──────── mempool.all_prioritized(), validate each
  │
  ▼
4. Build fee transaction ────────────── aggregate fees from selected txs
  │
  ▼
5. Compute state root ───────────────── validator.proofs_for_transactions()
  │
  ▼
6. Build extension section ──────────── interlinks, voting
  │
  ▼
7. Assemble CandidateBlock
  │
  ▼
8. Derive WorkMessage ───────────────── serialize header, Blake2b256 hash
  │
  ▼
9. Cache candidate
```

### Step 1: Parent Header + Parameters

- Read chain tip header from `chain` handle (latest validated header).
- Read current `Parameters` from `validator.parameters()`.
- Compute nBits for the new block from the chain's difficulty adjustment.
  Uses the difficulty adjustment algorithm already implemented in the
  `chain` crate.
- Compute height: `parent.height + 1`.
- Compute timestamp: `max(system_time_ms(), parent.timestamp + 1)`.

### Step 2: Emission Transaction

Every block includes a coinbase transaction that distributes the block
reward from the emission contract. This is the first transaction in the
block.

1. **Find the emission box** in the UTXO state. The emission box is
   identified by its ErgoTree (the emission contract script). The validator
   tracks the current emission box ID as it changes each block (see
   Required Interface Additions).

2. **Compute block reward** using
   `EmissionRules::miners_reward_at_height(height)`. Returns the ERG
   amount the miner receives at this height.

3. **Build the emission transaction:**
   - **Input:** the current emission box (with empty prover result — the
     emission contract is self-validating)
   - **Output 1:** new emission box with value reduced by `reward_amount`,
     same ErgoTree script, same creation height
   - **Output 2:** miner reward box protected by
     `ErgoTreePredef::reward_output_script(reward_delay, miner_pk)`,
     containing `reward_amount` nanoERG

   Reference: JVM `CandidateGenerator.collectRewards()`.

4. **EIP-27 re-emission** (active on mainnet since height 777,217):
   - When re-emission is active at this height, the emission transaction
     includes re-emission token operations and potentially an injection box
   - Reference: JVM `CandidateGenerator.collectRewards()` lines 744-792
   - Required for mainnet. Can be deferred if targeting testnet-only for
     first release.

**If no emission box exists** (all ERG emitted): skip the emission
transaction. The block contains only mempool transactions and the fee
transaction.

### Step 3: Transaction Selection

Select transactions from the mempool to include in the block, respecting
protocol limits.

1. Read protocol limits from `Parameters`:
   - `max_block_cost` — maximum cumulative ErgoScript evaluation cost
   - `max_block_size` — maximum serialized block size in bytes

2. Build the state context for the upcoming block:
   ```rust
   let upcoming_context = build_state_context(
       &upcoming_header_stub,  // height+1, new timestamp
       &preceding_headers,
       &parameters,
   );
   ```

3. Get prioritized transactions from mempool: `mempool.all_prioritized()`.

`select_transactions` implements this sequence and is bounded by both limits.
See "Ownership" above for why it spent v0.8.0 development with no caller.

4. For each candidate transaction (in priority order):
   a. Check cumulative size: if adding this tx exceeds `max_block_size`,
      skip. Size is known from the serialized bytes without validating.
   b. Validate the transaction against the "accumulated state" — the UTXO
      set augmented with outputs from the emission tx and already-selected
      transactions. Use `validate_single_transaction()` with the upcoming
      state context. **It returns the transaction's cost** — that return
      value is the only source of the number step (d) needs.
   c. If validation fails: record tx ID for mempool elimination, skip.
   d. Check cumulative cost: if `accumulated_cost + tx_cost` would exceed
      `max_block_cost`, skip **this** transaction — do not stop the scan, a
      later cheaper transaction may still fit.
   e. Otherwise add to selected set and accumulate both cost and size.

   ⚠ **The cost check cannot precede validation**, and an earlier draft of
   this list had it first. A transaction's cost is produced *by*
   `validate_single_transaction`; there is nothing to check against before
   that call returns.

   ⚠ **The size bound is over the serialized SECTION, not the sum of
   transaction sizes.** Validators enforce `max_block_size` against the
   BlockTransactions section, which is
   `[header_id: 32B][ver_or_count: VLQ][tx_count: VLQ if ver>1][txs…]` —
   **37 bytes** for a current-version block under 128 transactions, 33 for v1.

   The middle field is not a version byte. The JVM writes
   `putUInt(MaxTransactionsInBlock + blockVersion)`, i.e. a VLQ of ~10,000,00X,
   which is **4 bytes**, and the sentinel is what tells a parser this is a
   versioned section rather than a v1 one whose first field is the transaction
   count. So the overhead is `32 + 4 + vlq_len(tx_count)`, and a "1-byte
   version" reading of the layout undercounts by three. A candidate landing within that margin
   of the limit is over it by the rule that actually decides validity.

   The JVM generator sums transaction sizes and carries the same undercount;
   we do not copy it. **This is not a safety margin** — the overhead is
   deterministic and computable, so counting it is accuracy, not headroom, and
   the no-`safeGap` decision above is untouched.

   ⚠ **An UNRESOLVABLE input is skipped, not evicted — and that applies to
   data inputs too.** A box that cannot be resolved is not evidence the
   transaction is invalid; it may be a reorg race, a fastsync gap, or the
   at-tip storage swap window, and the box may resolve on the next rebuild 15
   seconds later. Evicting there removes a valid transaction from the mempool
   for the invalidation TTL.

   This must hold for **both** input kinds. Resolving data inputs with a
   filter that silently drops misses produces a short `data_boxes`, which
   `TransactionContext::new` rejects deterministically as
   `DataInputBoxNotFound` — so the transaction is reported *invalid* rather
   than *skipped*, and the caller evicts it. The truncation is caught, so
   nothing is validated against a mismatched context; the damage is the wrong
   verdict, not a wrong evaluation.

   ⚠ **A double-spend loser is skipped, not evicted.** Two mempool
   transactions spending the same box both validate in isolation — validation
   sees one transaction at a time and the box is unspent on chain — so
   selection must reject the second against the accumulated block, before
   script evaluation (the JVM's ordering, to save time). The JVM *evicts* the
   loser; we skip it and leave it in the pool, because conflict resolution is
   the mempool's job and a candidate is a poor place to adjudicate it. The
   transaction may well be the winner next block.

   ⚠ **Bound at `accumulated + tx_cost <= max_block_cost`, with NO safety
   margin.** The JVM subtracts a `safeGap` first — 0 below a 1M limit,
   150,000 below 5M, 500,000 above (`CandidateGenerator.scala:585`) —
   because, in its own words, "different interpreter version can estimate
   cost differently due to bugs in AOT costing". That guards against *its
   own* historical costing divergence. Ours were closed and are graded by
   SANTA, so we bound exactly, and the comparator matches our validator's:
   `validation/src/tx_validation.rs` rejects on `total > max_cost`, and the
   JVM's own validation accepts on `maxCost >= startCost` — the same
   semantics. **Do not reintroduce a gap by citing the JVM generator**; its
   strict `<` at `CandidateGenerator.scala:831` is generator conservatism,
   not a consensus rule.

5. Stop when limits are reached or all mempool transactions are checked.

6. Report invalid transaction IDs back to the mempool for cleanup.

**Edge case — empty mempool:** The block contains only the emission
transaction and fee transaction (zero-fee). This is valid.

### Step 4: Fee Transaction

⚠ **Cap aggregated fee tokens by serialized BOX SIZE, not by token count.**

`MaxAssetsPerBox` (255) is **not** the binding limit and capping there does not
collect the fees. The consensus rule is `txBoxSize` —
`out.bytes.length <= MaxBoxSize` (4096), `ErgoTransaction.scala`. Measured at
`reward_delay = 720`: a miner reward box holds **121** minimal tokens at
**4087 bytes**; 122 is over. A 255-token box is **8509** bytes and can never
validate, so a count cap only moves the failure from "box will not build" to
"box will not validate": the block still ships collecting **zero** fees.

⚠ **Measure `out.bytes`, not the candidate.** `txBoxSize` applies to the
serialized `ErgoBox`, which carries 32 bytes of transaction id plus the output
index on top of the `ErgoBoxCandidate` body — 33 bytes that an
estimate-from-the-candidate misses. Earlier drafts of this section said 122 and
8476 for exactly that reason. Any implementation must grow the real box and
measure it rather than assuming a per-token width.

The JVM caps by the same wrong constant — `flatMap(_.additionalTokens).take(MaxAssetsPerBox)`,
unsorted — and loses the same fees. **We diverge deliberately.** Which fee
boxes a miner collects is miner policy, not consensus: the rule constrains the
box it produces, not the choice of what to put in it, and uncollected fee boxes
simply remain spendable later. So capping by size is legal, strictly better for
the miner, and not a parity break in any sense that matters.

⚠ **The dust rule cannot bind here, and a guard for it would only misfire.**
`value >= bytes * minValuePerByte` is already satisfied by construction: every
fee box collected has cleared that rule while carrying the same tokens under
the *larger* fee-proposition tree (105 bytes against the reward script's 54),
values add across the collected boxes while the box overhead is paid once, and
duplicate token ids collapse to a single entry. The reward box is strictly
cheaper per byte than the boxes that fed it. A guard would also have to
hardcode `minValuePerByte` rather than read the block's voted value, which
`build_fee_tx` is not given — so it could only ever fire wrongly and burn
tokens that would have validated. Recorded because it looks like an obvious
second check to add.

⚠ **The surviving tokens must be selected deterministically**, in a stable
traversal order — first-seen across the fee boxes, never hash-map iteration
order. The fee box's bytes determine its box id, which determines the
transaction id, which feeds the transactions root the miner hashes. A
seed-dependent order makes two runs over identical input produce different
work. This bit once, via `HashMap` aggregation.

Collect fees from all selected transactions and create a single fee output
for the miner.

1. **Identify fee outputs:** Scan all outputs of selected transactions for
   boxes whose ErgoTree matches the fee proposition
   (`ErgoTreePredef::fee_proposition()`). Exclude any that are spent by
   other selected transactions within the same block.

2. **Sum fee values:**
   `total_fee = sum(fee_box.value for fee_box in fee_boxes)`.

3. If `total_fee > 0`:
   - **Inputs:** all identified fee boxes (with empty prover results)
   - **Output:** single miner box with value = `total_fee`, protected by
     `ErgoTreePredef::reward_output_script(reward_delay, miner_pk)`
   - Append the fee transaction to the transaction list (after mempool txs)

4. If `total_fee == 0`: no fee transaction needed.

Reference: JVM `CandidateGenerator.collectFees()`.

### Step 5: State Root Computation

Compute the new UTXO state root and AD proofs for the complete transaction
set. This is the expensive step — it temporarily modifies the prover state.

1. Acquire the validator lock.
2. Call `validator.proofs_for_transactions(&all_transactions)` where
   `all_transactions = [emission_tx] ++ selected_txs ++ [fee_tx]`.
3. Receive `(ad_proof_bytes, new_state_root)`.
4. Release the validator lock.

The `proofs_for_transactions()` method (see Required Interface Additions):
- Computes state changes from the transactions (reusing
  `compute_state_changes()`)
- Applies operations to the prover's in-memory tree
- Captures the new digest (state root)
- Generates the AD proof bytes
- Rolls back the prover to its original state (no persistence)

### Step 6: Extension Section

Build the extension with NiPoPoW interlinks and voting data.

1. **Interlinks:** Compute updated interlink vector from the parent header
   and parent extension. Uses `ergo-nipopow`:
   ```rust
   let interlinks = update_interlinks(parent_header, parent_extension);
   let interlink_fields = interlinks_to_extension(interlinks);
   ```

2. **Voting:** The node's voting preferences from `MinerConfig.votes`:
   - Byte 0: soft-fork vote ID (0 = no vote)
   - Byte 1: parameter change vote 1 (0 = no vote)
   - Byte 2: parameter change vote 2 (0 = no vote)

3. **Epoch boundary** (height % voting_epoch_length == 0):
   - Compute new parameters from accumulated votes
   - Include parameter update fields in extension
   - Include updated validation settings
   - Reference: JVM `CandidateGenerator.createCandidate()` lines 533-569

4. **Assemble:** Merge interlink fields + voting/parameter fields into
   `ExtensionCandidate`.

### Step 7: Assemble CandidateBlock

```rust
CandidateBlock {
    parent,
    version: current_version(parameters),
    n_bits,
    state_root: new_state_root,
    ad_proof_bytes,
    transactions: all_transactions,
    timestamp,
    extension,
    votes,
}
```

### Step 8: Derive WorkMessage

Convert the candidate into the data miners need.

1. **Build HeaderWithoutPow.** Field order, sizes and encoding must match
   JVM `HeaderSerializer.serializeWithoutPow` exactly:

   | # | Field | Size | Encoding | Source |
   |---|-------|------|----------|--------|
   | 1 | version | 1 byte | raw `u8` | `candidate.version` |
   | 2 | parentId | 32 bytes | raw bytes | `candidate.parent.id` |
   | 3 | ADProofsRoot | 32 bytes | raw bytes | `Blake2b256(candidate.ad_proof_bytes)` |
   | 4 | transactionsRoot | 32 bytes | raw bytes | version-dependent Merkle root — see "transactionsRoot computation" below |
   | 5 | stateRoot | 33 bytes | raw bytes (32-byte hash + 1-byte tree height) | `candidate.state_root` |
   | 6 | timestamp | variable | VLQ `u64` | `candidate.timestamp` |
   | 7 | extensionRoot | 32 bytes | raw bytes | `candidate.extension.digest()` |
   | 8 | nBits | 4 bytes | fixed-width big-endian `u32` (NOT VLQ) | `candidate.n_bits` |
   | 9 | height | variable | VLQ `u32` | `candidate.parent.height + 1` |
   | 10 | votes | 3 bytes | raw bytes | `candidate.votes` |
   | 11 | unparsedBytes (only if version > 1) | 1 byte length + N bytes | raw `u8` length prefix + raw bytes | empty for first release |

   **transactionsRoot computation (corrected 2026-06-10 — consensus bug):**
   JVM `BlockTransactions.scala:59-63` makes the Merkle leaves
   version-dependent:
   - block version 1: leaves = the tx IDs (32 bytes each,
     `tx.serializedId` = blake2b256 of the proof-less signing bytes —
     sigma-rust `Transaction::id()`).
   - **block version ≥ 2: leaves = all tx IDs FOLLOWED BY all witness IDs**
     (`txIds ++ witnessIds` — two concatenated lists, not interleaved).
     A witness ID is
     `blake2b256(concat(tx.inputs[*].spendingProof.proof)).tail` —
     the 32-byte hash with its FIRST byte dropped, 31 bytes
     (`ErgoTransaction.scala:77-78`). Empty proofs (e.g. storage-rent
     spends) contribute zero bytes to the concatenation.

   `transactions_root(txs)` previously implemented the v1 form
   unconditionally — on v2+ networks (mainnet/testnet today) every
   assembled candidate carried a transactionsRoot no JVM peer accepts;
   mined blocks would have been orphaned network-wide. The signature is now
   `transactions_root(txs, block_version)`. Surfaced 2026-06-10 by the
   SANTA donner runner recomputing roots over real v4 testnet blocks.

   **Consensus-critical.** This serialization must match JVM
   `HeaderSerializer.bytesWithoutPow` byte-for-byte. Any divergence means
   miners compute solutions for the wrong message and blocks are rejected
   by the network. The serializer itself is delegated to sigma-rust's
   `Header::serialize_without_pow`, which is JVM-pinned upstream against
   the height-614,400 mainnet header (see
   `ergo-chain-types::autolykos_pow_scheme::tests::test_first_increase_in_big_n`).
   The mining crate's responsibility is correctly mapping `CandidateBlock`
   fields into the `Header` struct before handing it to the encoder; that
   wiring is verified by `mining/tests/work_message_wiring.rs`.

   **Note on `nBits`:** despite Scorex's general rule that integers are
   VLQ-encoded, `nBits` is the one exception in the header — it's written
   as 4 raw big-endian bytes via JVM `DifficultySerializer.serialize`, and
   sigma-rust matches this with `n_bits.to_be_bytes()`. Earlier versions
   of this contract incorrectly described `nBits` as VLQ; the code was
   always correct.

2. **Serialize** the header without PoW fields.

3. **Hash:** `msg = Blake2b256(serialized_header_without_pow)`.

4. **Target:** `b = enr_chain::pow_target(candidate.n_bits)`.

   ⚠ **Not `decode_compact_bits(candidate.n_bits)` — that is the difficulty.**
   The target is `q / difficulty` where `q` is the secp256k1 group order, and
   `pow_target` in `facts/chain.md` § "Phase 2" is the single definition of it.
   This step said `decode_compact_bits` until v0.8.0 and the implementation
   faithfully matched it, which is how a released node spent its serve path
   advertising a target ~10^58 times harder than the one its own
   `check_pow` validates against. Zero shares submitted, at any hashrate.

   The two numbers are not close enough to be confused at a glance: at testnet
   height 485,897 the difficulty was `3912040448` (10 digits) and the target
   `29598898778389163379010897437604384363675568080188445020547283242588`
   (68 digits). Anything under ~20 digits appearing in `b` is a bug, not a
   low-difficulty epoch.

5. **Assemble WorkMessage:**
   ```rust
   WorkMessage {
       msg,
       b,
       h: candidate.parent.height + 1,
       pk: miner_config.miner_pk,
       proof: ProofOfUpcomingTransactions {
           msg_preimage: serialized_header_without_pow,
           tx_proofs: vec![],  // Empty for first release
       },
   }
   ```

### Candidate Caching

The candidate is cached and served to multiple miner polls:

- **Invalidation triggers:**
  - Block applied (own or peer) → `on_block_applied` drops both slots when
    they no longer build on the new tip, then the next poll regenerates
  - Mempool content changes → `invalidate()` (push): current → previous,
    next poll regenerates with the fresh mempool snapshot
  - Candidate TTL expires → regenerate with fresh mempool snapshot
  - Node shutdown → discard

- **On GET /mining/candidate:**
  1. If cached candidate exists and `tip_height == current_tip` and
     `created + candidate_ttl > now`: return cached WorkMessage.
  2. Otherwise: regenerate candidate (steps 1-8), cache (old current
     moves to `previous`), return.

- **Two slots, not one.** Regeneration and invalidation preserve the
  superseded candidate in `previous` so a miner that fetched work before
  the swap can still submit a winning solution (JVM
  cachedPreviousCandidate, CandidateGenerator.scala:182 + the fallback at
  :213). `previous` only ever holds the immediately superseded candidate.

## Solution Submission

### POST /mining/solution

1. **Deserialize** the `AutolykosSolution` from JSON:
   ```json
   { "pk": "hex", "w": "hex", "n": "hex (8 bytes)", "d": "decimal string" }
   ```
   For Autolykos v2, `pk`, `w`, and `d` are optional (defaults apply):
   - `pk`: identity point
   - `w`: generator point
   - `d`: 0
   The only required field is `n` (the 8-byte nonce).

2. **Check the solved latch:** If `solved_pending()`, return 400
   ("solution already accepted, awaiting block application"). Both
   candidate slots are at the latched height; accepting another solution
   would submit a self-competing block. (JVM: solvedBlock latch,
   CandidateGenerator.scala:146-147.)

3. **Collect candidates:** current first, then previous. If both are
   None, return 400 ("no current candidate"). For each, check it still
   builds on the chain tip (`candidate.parent_id == tip.id`) — skip any
   that don't. The `on_block_applied` clearing makes stale candidates
   rare here; the check guards the race window between a peer block
   applying and the clearing running.

4. **Assemble header:** Combine the `HeaderWithoutPow` from the
   candidate with the submitted solution to produce a full `Header`.

5. **Verify PoW:** `AutolykosPowScheme::validate(header)`.
   - Compute `hit = pow_hit(header)` using the Autolykos v2 algorithm
   - Verify `hit < target` where `target = enr_chain::pow_target(n_bits)`,
     i.e. `q / decode_compact_bits(n_bits)` — the same value served as
     `WorkMessage.b`. Serving one number and validating against another is
     precisely the v0.8.0 defect; these two must not be allowed to drift
     apart again.
   - On failure (all candidates tried): return 400 "invalid PoW solution"

6. **Assemble full block:**
   ```
   Header (from step 3)
   BlockTransactions { header_id, version, transactions }
   Extension { header_id, fields }
   ADProofs { header_id, proof_bytes }
   ```
   The `header_id` is derived from the full header (including PoW solution).

7. **Claim the latch (atomic):** `try_mark_solved(header_id, height)`.
   On false — a concurrent solution won the race — return 400 with the
   step-2 message. This MUST precede the submitter hand-off: after
   submission the losing block has already left the node and rejection is
   too late. The candidate slots stay as-is — block application runs
   `on_block_applied`, which drops the stale slots and clears the latch
   in one place. An `invalidate()` here would be WRONG: it moves the
   just-solved candidate to `previous`, leaving it re-solvable during
   the accept→apply window.

8. **Validate block:** Submit to the normal validation pipeline. The
   assembled block must pass the same validation as any block received
   from a peer. If validation fails, there's a bug in candidate assembly
   — `clear_solved()`, return 500 and log the error.

9. **Apply block:** The validator applies the block, advancing state.
   If the submitter is unavailable (503), `clear_solved()` first — the
   block never left the node.

10. **Broadcast to P2P:** Send the full block to connected peers as
    separate modifier messages:
    - Header (type 101)
    - BlockTransactions (type 102)
    - ADProofs (type 104)
    - Extension (type 108)

11. **Return 200** on success.

## Endpoints

Detailed HTTP specification is in `api.md`. This section specifies the
mining-specific behavior that the API handlers invoke.

### `GET /mining/candidate`

No request body. Returns `WorkMessage` JSON.

**Height source — query by VALIDATED height, NOT header height (fixed
2026-06-12 — bug #3).** The handler MUST query `cached_work` with
`ApiState.validated_height` (the validator's last-fully-applied height —
the SAME `Arc<AtomicU32>` the mining task uses to key the cache, wired in
`main.rs`), NOT `chain.height()` (the HEADER-chain tip). The candidate is
built on the validated tip's state root and cached keyed to the validated
height; querying by header height fails the `cached.tip_height ==
current_tip_height` check whenever the header chain leads validation
(`full < headers`) — which is the NORMAL transient state every time a new
header arrives before its body is validated, and a persistent state when
validation trails reception (e.g. a single slow peer). The original
`chain.height()` query made the candidate `503` continuously in that
window even though a valid candidate for the validated tip sat in the
cache — the node was unservable as a mining source whenever it wasn't
perfectly synced. Found driving a real external miner; the miner grinds on
`validated_tip + 1`, racing the network for that height and switching when
the node validates the next block (own or network). Regression: a
candidate cached at validated height H must be served while header height
is H+1.

Returns 503 if:
- Mining not configured (no miner PK in config)
- Node is in digest mode (cannot compute state roots)
- No fresh candidate cached for the current VALIDATED tip (post-restart
  before the first at-tip block applies; or briefly between a block
  applying and the mining task regenerating)

### `POST /mining/solution`

Requires authentication if `api_key_hash` is configured.
Request body: `AutolykosSolution` JSON.

Returns 200 on success (block accepted and broadcast).
Returns 400 on invalid solution, no cached candidate, or stale candidate.

### `GET /mining/rewardAddress`

Returns the P2S address derived from the configured miner PK.
Simple config lookup, no state access.

```json
{ "rewardAddress": "3WwbzW..." }
```

Returns 503 if mining not configured.

### Deferred Endpoints (not first release)

| Endpoint | Reason to defer |
|---|---|
| `POST /mining/candidateWithTxs` | Pool operator feature; basic GET is sufficient |
| `POST /mining/candidateWithTxsAndPk` | Multi-miner pool feature |

## Required Interface Additions

### UtxoValidator: `proofs_for_transactions()`

New method on `UtxoValidator`:

```rust
impl UtxoValidator {
    /// Compute AD proofs and new state root for a set of transactions.
    ///
    /// Operates on the in-memory prover state WITHOUT persisting changes.
    /// The prover is rolled back to its original state after computation.
    ///
    /// Preconditions:
    ///   - Transactions are valid and ordered: [emission, mempool..., fee]
    ///   - The prover is at the current validated state
    ///
    /// Postconditions on Ok:
    ///   - Returns (serialized_ad_proof, new_state_digest)
    ///   - The prover state is unchanged (rolled back)
    ///
    /// Postconditions on Err:
    ///   - The prover state is unchanged (rolled back on best effort)
    pub fn proofs_for_transactions(
        &mut self,
        txs: &[Transaction],
    ) -> Result<(Vec<u8>, ADDigest), ValidationError>;
}
```

**Implementation approach:**
1. Save current digest: `let saved = self.prover.digest()`
2. Compute state changes: `compute_state_changes(transactions_to_summaries(txs))`
3. Build AVL operations (Lookups, Removes, Inserts — same as `validate_block`)
4. Apply operations: `self.prover.perform_one_operation(op)` for each
5. Capture new digest: `let new_root = self.prover.digest()`
6. Generate proof — use inner `BatchAVLProver::generate_proof()` if
   accessible without persistence, or call
   `generate_proof_and_update_storage()` followed by rollback (wasteful but
   correct)
7. Rollback: `self.prover.rollback(&saved)`
8. Return `(proof_bytes, new_root)`

**Note:** The exact mechanism for step 6 depends on what
`ergo_avltree_rust`'s `PersistentBatchAVLProver` exposes. If
`generate_proof()` is not accessible without persistence, the fallback is
persist + rollback. This may warrant a small addition to the avltree fork.

### UtxoValidator: `emission_box_id()`

Track the current emission box ID across block applications:

```rust
impl UtxoValidator {
    /// Current emission box ID in the UTXO state.
    /// Updated after each block validation by scanning state changes
    /// for the emission contract ErgoTree.
    /// None if all ERG has been emitted.
    pub fn emission_box_id(&self) -> Option<[u8; 32]>;
}
```

The emission box ID changes every block (the old one is spent, a new one
is created). The validator already processes all state changes — it can
track which output box matches the emission contract ErgoTree.

### Chain: difficulty for next block

The chain crate already computes difficulty adjustment. Expose the
next-block nBits:

```rust
/// Compute the required nBits (encoded difficulty) for a block
/// following `parent`.
fn required_difficulty(&self, parent: &Header) -> u64;
```

### NiPoPoW: interlink computation

From `ergo-nipopow`:

```rust
/// Compute updated interlinks from the parent header and current extension.
fn update_interlinks(parent: &Header, extension: &Extension) -> Vec<[u8; 32]>;

/// Encode interlinks as extension key-value pairs.
fn interlinks_to_extension(interlinks: &[[u8; 32]]) -> Vec<([u8; 2], Vec<u8>)>;
```

Verify that `ergo-nipopow` exposes these. If not, port from JVM's
`NipopowAlgos.updateInterlinks()` and `interlinksToExtension()`.

## Configuration

```toml
[node.mining]
# Miner public key (hex-encoded compressed group element, 33 bytes).
# Required to enable mining endpoints. Empty = mining disabled.
miner_pk = ""

# Voting preferences: 3 bytes as hex string. "000000" = no votes.
# Byte 0: soft-fork vote ID. Bytes 1-2: parameter change votes.
votes = "000000"

# Maximum candidate lifetime before forced regeneration (seconds).
candidate_ttl_secs = 15

# Miner reward maturity delay in blocks (protocol default: 720).
# Reward boxes are locked for this many blocks after mining.
reward_delay = 720
```

Mining is disabled when `miner_pk` is empty. The `/mining/*` endpoints
return 503 with `"reason": "mining not configured"`.

## Does NOT Own

- ErgoScript evaluation — that's `ergo-lib` via `validate_single_transaction()`
- UTXO state management — that's `enr-state` via the UtxoValidator
- PoW algorithm — that's `ergo-chain-types` (`AutolykosPowScheme`)
- Emission schedule — that's `ergo-lib` (`EmissionRules`)
- Difficulty adjustment — that's the `chain` crate
- Interlink computation — that's `ergo-nipopow`
- P2P block broadcast — that's the main crate's P2P task
- Block storage — that's `enr-store`
- Transaction validation — that's `ergo-validation`
- Mempool management — that's the mempool crate
- Header serialization — that's `ergo-chain-types` (must match JVM exactly)
- HTTP endpoints / JSON wire format — that's the API crate
- Key management — that's the user (configured in toml, not generated)

## Invariants

- Every `CandidateBlock`, if paired with a valid PoW solution, produces a
  block that passes the node's own validation pipeline.
- `on_block_applied` runs for EVERY applied block — own or peer — and is
  the only place that drops candidates and clears the solved latch.
  Candidates surviving it always build on the current tip.
- A solution is never accepted while the solved latch is set: at most one
  block per height leaves the node via the mining path (no
  self-competition).
- `previous` only ever holds the immediately superseded candidate, and
  solutions against it are accepted only while it still builds on the
  current tip.
- `proofs_for_transactions()` never leaves the prover in a modified state.
- The emission transaction is always the first transaction in the block.
- The fee transaction (if present) is always the last transaction.
- Transaction order in the block matches the order used for state root
  computation. Reordering transactions changes the state root.
- `WorkMessage.msg` is the Blake2b256 hash of the header serialized without
  PoW fields, using the exact same byte format as the JVM node's
  `HeaderSerializer.bytesWithoutPow()`. Byte-level divergence means miners
  produce invalid solutions.
- A 503 from any mining endpoint means the feature is unavailable (config
  or mode issue), not a transient error. The client should not retry.
- A 400 from `POST /mining/solution` means the solution is invalid or the
  candidate is stale. The miner should fetch a new candidate.

## Testing Strategy

1. **Candidate assembly — empty mempool:** Generate a candidate with only
   the emission transaction. Verify state root is correct, AD proofs verify,
   WorkMessage fields are populated.

2. **Candidate assembly — with transactions:** Add known transactions to
   mempool, generate candidate, verify selected transactions appear in
   priority order.

3. **State root verification:** Generate candidate, apply the candidate's
   transactions to a fresh prover, verify the resulting digest matches
   `candidate.state_root`.

4. **Round-trip:** Generate candidate, construct a valid header with a known
   nonce that satisfies the difficulty (use trivially low difficulty for
   testing), submit via `POST /mining/solution`, verify block is accepted.

5. **Solution rejection:** Submit an invalid nonce, verify 400 response
   with clear error message.

6. **Candidate caching:** Poll `/mining/candidate` twice rapidly, verify
   same WorkMessage returned (no regeneration).

7. **Candidate invalidation:** Generate candidate, apply a new block
   (advancing chain tip), poll `/mining/candidate`, verify new candidate
   with updated parent.

8. **Transaction selection limits:** Fill mempool with transactions
   exceeding `max_block_cost`. Verify candidate includes transactions up
   to the limit and no more.

9. **Fee collection:** Include transactions with fee outputs, verify the
   fee transaction aggregates all fees into a single miner output.

10. **Emission transaction:** Verify emission box is spent, new emission
    box has correct reduced value, miner reward box has correct amount and
    time-lock script.

11. **Extension section:** Verify interlinks are computed from parent,
    voting bytes are included. Epoch boundary produces parameter updates.

12. **JVM compatibility:** Generate a candidate from the same chain state
    as a JVM node. Compare `WorkMessage.msg` byte-for-byte — if `msg`
    matches, the header serialization is correct.

    ⚠ **`msg` matching proves header serialization and nothing else.** This
    item claimed to be "the ultimate correctness test" and it is not: a
    candidate can have a byte-perfect `msg` and still be unmineable, which is
    exactly what shipped in v0.8.0. Compare **every** served field against the
    JVM's, `b` included.

13. **Target vector (regression):** Assert the served `b` against a fixed
    known-good pair captured from a Scala node, not against our own formula —
    a test that recomputes `pow_target` the way the implementation does will
    pass on any consistent-but-wrong definition.

    ```
    n_bits     83945773            (0x0500e92d)
    difficulty 3912040448
    b          29598898778389163379010897437604384363675568080188445020547283242588
    ```

    Captured at testnet height 485,897. The `difficulty` and `b` values are
    observed — `b` was cross-checked digit for digit against a Scala node
    serving the same height, remainder `1309294913`. **`n_bits` is derived**,
    by canonical compact-bits encoding of the observed difficulty, and was not
    read off the wire. So assert the round-trip
    `decode_compact_bits(83945773) == 3912040448` **first**: if the derivation
    is wrong, that fails loudly instead of the target assertion quietly
    testing some other difficulty. The pre-fix code served `3912040448` as
    `b`.

    The three pre-existing tests over `b` asserted that it was non-empty
    (`mine_blocks.rs`), stable across polls (`candidate_generator.rs`), and
    emitted as a bare JSON number rather than a quoted string (`types.rs`).
    All three passed throughout. **A field can be present, consistent and
    well-typed while being the wrong quantity** — assert the value.

14. **Digest mode rejection:** Start node in digest mode, verify
    `/mining/candidate` returns 503.

15. **No miner PK:** Start with empty `miner_pk` config, verify 503.

16. **Prover rollback:** After `proofs_for_transactions()`, verify the
    validator's prover digest is identical to before the call.

# Block Validation Contract

## Component: `validation/` (workspace crate, in-repo)

Validates block sections to prove state transitions are correct. Composes
header checks (from `chain/`), AD proof verification (from `ergo_avltree_rust`),
and transaction validation (from `ergo-lib`). The final arbiter — if a bad block
passes, the node's state is corrupted.

Phase 4a: digest mode (AD proof verification, no persistent UTXO set).
Phase 4b: UTXO mode (persistent AVL+ tree, direct box lookup).
Both share the same validation core; only the box source differs.

## SPECIAL Profile

```
default:                S10 P8 E7 C5 I9 A7 L9
transaction validation: S9  P8 E6 C5 I8 A7 L8
```

## Design Principles

- **Two modes, one core.** DigestValidator and (future) UtxoValidator share
  section parsing, stateChanges computation, and transaction validation logic.
  Only the box source and state root verification mechanism differ.
- **Checkpoint optimization.** Below a configured checkpoint height, skip
  transaction validation (ErgoScript evaluation). The state root check alone
  is sufficient — if any box operation is wrong, the digest won't match.
- **Stateful per-block.** The validator tracks the current state root and
  validated height. Blocks must be validated in order. On reorg, the caller
  resets the validator to the fork point.

## Trait: `BlockValidator`

```rust
pub trait BlockValidator {
    /// Apply state transition: parse sections, compute state changes,
    /// apply AVL operations, verify digest, persist.
    ///
    /// Preconditions:
    ///   - `header.height == self.validated_height() + 1`
    ///   - `block_txs` is the raw BlockTransactions section (type 102)
    ///   - `ad_proofs` is the raw ADProofs section (type 104), required
    ///     for digest mode, None for UTXO mode
    ///   - `extension` is the raw Extension section (type 108)
    ///   - `preceding_headers` contains up to 10 headers before this block,
    ///     newest first (for ErgoStateContext in DeferredEval)
    ///   - `active_params` is the current chain parameters
    ///   - `expected_boundary_params` is Some iff header.height is an
    ///     epoch boundary
    ///
    /// Postconditions on Ok:
    ///   - `self.validated_height()` == header.height
    ///   - `self.current_digest()` == header.state_root
    ///   - State transition persisted (UTXO mode) or digest updated (digest mode)
    ///   - `ApplyStateOutcome.deferred_eval` is Some if scripts need
    ///     evaluation (height > checkpoint), None otherwise
    ///   - `ApplyStateOutcome.epoch_boundary_params` is Some if this was
    ///     an epoch-boundary block with verified parameters
    ///
    /// Postconditions on Err:
    ///   - State is unchanged. validated_height and current_digest are unmodified.
    ///   - The error describes which check failed.
    fn apply_state(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[Header],
        active_params: &Parameters,
        expected_boundary_params: Option<&Parameters>,
    ) -> Result<ApplyStateOutcome, ValidationError>;

    /// Current validated height. 0 means no blocks validated yet
    /// (genesis state root is set but no blocks applied).
    fn validated_height(&self) -> u32;

    /// Current state root digest (33 bytes: 32-byte hash + 1-byte tree height).
    fn current_digest(&self) -> &ADDigest;

    /// Reset to a previous state. Used on reorg and deferred eval failure.
    ///
    /// Preconditions:
    ///   - `height < self.validated_height()`
    ///   - `digest` is the state_root from the header at `height`
    ///
    /// Postconditions:
    ///   - `self.validated_height()` == height
    ///   - `self.current_digest()` == digest
    fn reset_to(&mut self, height: u32, digest: ADDigest);

    /// Compute AD proofs for transactions without modifying state.
    /// None for digest-mode validators (mining requires UTXO mode).
    fn proofs_for_transactions(&self, txs: &[Transaction])
        -> Option<Result<(Vec<u8>, ADDigest), ValidationError>>;

    /// Current emission box ID. None if digest mode or all ERG emitted.
    fn emission_box_id(&self) -> Option<[u8; 32]>;
}
```

## New Types

```rust
pub struct ApplyStateOutcome {
    /// Some if this was an epoch-boundary block with verified parameters.
    pub epoch_boundary_params: Option<Parameters>,
    /// Some if scripts need evaluation (height > checkpoint).
    pub deferred_eval: Option<DeferredEval>,
}

/// Everything needed to verify transaction spending proofs.
/// Owned, Send — can move to any thread.
pub struct DeferredEval {
    pub height: u32,
    pub transactions: Vec<Transaction>,
    pub proof_boxes: HashMap<[u8; 32], ErgoBox>,
    pub header: Header,
    pub preceding_headers: Vec<Header>,
    pub parameters: Parameters,
}
```

## Free Function

```rust
/// Verify spending proofs for all transactions in a block.
/// Pure computation — no validator state needed. Uses rayon par_iter internally.
/// On success returns the block-accumulated transaction cost.
pub fn evaluate_scripts(eval: &DeferredEval) -> Result<u64, ValidationError>;
```

### Block cost semantics (added 2026-06-10)

- The returned cost is **Σ of per-tx costs** as returned by ergo-lib
  `TransactionContext::validate` (each per-tx number already includes the
  JVM-equivalent `initialCost` components — input/data-input/output static
  costs — plus script evaluation cost; this is the tx-tier-anchored number).
- **JVM mapping:** `ErgoState.execTransactions` (`ErgoState.scala:106`) folds
  `validateStateful` from `Valid(0L)`, threading the accumulated cost through
  each tx; the running total is checked against `maxBlockCost` at each tx's
  `startCost` (`ergo-core ErgoTransaction.scala:391-396`). Block cost = the
  final fold value. There is **no block-level base term** — the sum IS the
  block cost. (SANTA keystone: testnet block 2666 = 39379.)
- **Enforcement (consensus check):** after all txs validate,
  `Σ ≤ parameters.max_block_cost()` must hold; violation →
  `BlockCostExceeded`. Verdict-equivalent to the JVM's threaded check: a
  block is accepted iff every tx is individually valid AND the total is
  within maxBlockCost. Which error fires first on a multi-fault block may
  differ from the JVM (we validate txs in parallel; the JVM stops at the
  first crossing) — error identity is diagnostic, the accept/reject verdict
  is the contract.
- Summation uses **checked arithmetic** (JVM `addExact` parity): overflow →
  reject (`BlockCostExceeded` with saturated value is acceptable), never
  wrap, never panic.
- Degenerate cases return `Ok(0)`: empty transaction list; the height-1
  no-preceding-headers guard. Blocks at or below a validator's
  `checkpoint_height` never reach evaluation (no `DeferredEval` is built),
  matching the JVM's `Valid(0L)` checkpoint shortcut.
- The per-tx sigma-rust JIT budget (`max_block_cost × 10` per tx,
  ergo-lib `tx_context.rs:202`) is unchanged — it bounds each evaluation's
  runtime; the block-level sum check is the consensus gate on top. Do NOT
  thread remaining-budget across txs: parallel evaluation is load-bearing
  for sync throughput, and verdict-equivalence makes threading unnecessary.
- Consumers: sync discards the cost today (`.map(|_| ())` at its call site —
  consumer's choice, the seam exposes it); the donner SANTA runner reports it
  on the accept arm (`runner.json: cost: true`).

## Phase 4a: DigestValidator

Validates state transitions using AD proofs. No persistent UTXO set.

### Construction

```rust
DigestValidator::new(
    genesis_digest: ADDigest,     // state root of genesis state
    checkpoint_height: u32,       // skip script validation below this
) -> Self
```

- `genesis_digest`: the ADDigest after applying genesis boxes to an empty
  AVL+ tree. Hardcoded per network (mainnet vs testnet).
- `checkpoint_height`: blocks at or below this height skip ErgoScript
  evaluation. The AD proof verification alone ensures correctness.
  Set to 0 to validate everything.

### Validation flow (per block)

1. **Parse BlockTransactions** -> `Vec<Transaction>`
   - Strip 32-byte header_id prefix
   - Read block version (VLQ sentinel: if > 10M, subtract 10M for version,
     read separate VLQ tx_count; else value IS tx_count and version = 1)
   - Parse tx_count transactions via `Transaction::sigma_parse()`

1b. **Block-version gate** (consensus check, added 2026-06-10; **narrowed to
   epoch boundaries same day** after JVM cross-reference by the implementing
   session)
   - At an **epoch-boundary block**: the newly computed boundary parameters'
     `block_version()` must equal `header.version`; violation →
     `BlockVersionMismatch { expected, got }`. (JVM `exBlockVersion`,
     `ErgoStateContext.scala:222`.)
   - At any **other block: NO version check.** `exBlockVersion` fires only at
     boundaries — `processExtension` is gated on `epochStarts`
     (`ErgoStateContext.scala:246`) — and the JVM has no header-level version
     rule anywhere; mid-epoch it ignores `header.version` entirely (script
     evaluation keys off `params.blockVersion`, `ErgoContext.scala:28`).
     Enforcing mid-epoch would be STRICTER than the reference:
     an adversarial wrong-version mid-epoch block (one block of PoW) is
     accepted by JVM nodes — rejecting it forks us off the canonical chain.
     Match JVM leniency; never add checks the reference node lacks.
   - SANTA note: the tier's oracle composes the params-vs-header check
     unconditionally and its `version-gate` mutation rides a mid-epoch donor
     (2666). With this gate JVM-exact, donner shows that one cell red until
     SANTA re-donors the mutation over a boundary block — finding relayed;
     a red cell is the runner working.

2. **Verify AD proofs digest**
   - `blake2b256(proof_bytes) == header.ad_proofs_root`
   - Proof bytes: strip 32-byte header_id prefix, read VLQ proof_size,
     remaining bytes are the raw proof

3. **Compute stateChanges** from transactions
   - Data inputs -> `Lookup(box_id)` operations (transaction order)
   - Inputs -> `Remove(box_id)` operations, BUT: if a box was created by
     an earlier tx in this block, remove from insertions instead (net-zero,
     never hits the tree). Double-spend within block is an error.
   - Outputs -> `Insert(box_id, serialized_box_bytes)` operations
     where serialized_box_bytes = `ErgoBox::sigma_serialize_bytes()`
     (full box: candidate + txId + index)
   - **CRITICAL: Removes and Inserts are sorted by box ID** (unsigned
     lexicographic byte order). The JVM uses `mutable.TreeMap[ModifierId, _]`
     which sorts by hex-encoded box ID — equivalent to byte ordering.
     Lookups preserve transaction order (data inputs don't modify the tree).
   - Final operation order: Lookups, then sorted Removes, then sorted Inserts

4. **Verify AD proof against state roots**
   - Create `AVLTree::new(label_preserving_resolver, 32, None)` — the
     resolver MUST preserve the digest label. `AVLTree::left()/right()`
     calls `resolve()` on every child access including LabelOnly sibling
     stubs. A resolver that returns `label: None` will cause panics.
   - Create `BatchAVLVerifier::new(current_digest, proof_bytes, tree,
     max_ops, max_deletes)`
   - Replay all operations from step 3 via `verifier.perform_one_operation()`
   - Each Remove/Lookup returns `Ok(Some(old_value))` — these are the
     serialized input boxes
   - Verify `verifier.digest() == header.state_root`
   - On success, `current_digest` = `header.state_root`

5. **Advance state** (immediate, before script eval)
   - `validated_height` = header.height
   - `current_digest` = header.state_root

6. **Build DeferredEval** (skipped below checkpoint_height)
   - Deserialize old values from step 4 into `ErgoBox` instances
   - Bundle transactions, proof boxes, header, preceding headers, and
     parameters into a `DeferredEval` struct
   - Returned as `ApplyStateOutcome.deferred_eval` for the sync layer
     to evaluate asynchronously via `evaluate_scripts()`
   - `evaluate_scripts` uses rayon `par_iter` for intra-block parallelism
     and returns the block-accumulated cost (see "Block cost semantics")

### Error causes

- `SectionParse` — malformed BlockTransactions, ADProofs, or Extension bytes
- `ProofDigestMismatch` — blake2b256(proof_bytes) != header.ad_proofs_root
- `StateRootMismatch` — verifier.digest() != header.state_root after replay
- `ProofVerificationFailed` — an operation failed during AD proof replay
- `IntraBlockDoubleSpend` — same box spent twice within one block
- `TransactionInvalid(index, details)` — tx validation failed (Phase 4a+)
- `BlockCostExceeded { cost, max_cost }` — Σ per-tx costs >
  `parameters.max_block_cost()` (added 2026-06-10; previously unenforced —
  each tx independently got the full block budget)
- `BlockVersionMismatch { expected, got }` — governing parameters'
  `block_version()` != `header.version` (added 2026-06-10; JVM
  `exBlockVersion`; previously unenforced)
- `MissingProof` — ad_proofs is None but validator requires proofs

## Section Parsing (internal)

### BlockTransactions (type 102)
```
[header_id: 32B] [ver_or_count: VLQ] [tx_count: VLQ if ver>1] [txs...]
```
- Each transaction: sigma-serializable with per-tx indexed token digests
- Use `Transaction::sigma_parse()` from ergo-lib for each tx
- Block version extracted but not validated here (header owns version)

### ADProofs (type 104)
```
[header_id: 32B] [proof_size: VLQ] [proof_bytes: proof_size]
```
- `proof_bytes` pass directly to `BatchAVLVerifier` — no unwrapping
- `blake2b256(proof_bytes)` must equal `header.ad_proofs_root`

### Extension (type 108)
```
[header_id: 32B] [field_count: VLQ] [fields: {key: 2B, val_len: 1B, val}...]
```
- Key prefix `0x00` = protocol parameters, `0x01` = interlinks, `0x02` = rules
- Parsed when building ErgoStateContext for script validation (Phase 4a+)
- Ignored below checkpoint_height

## Integration: Sync Machine

### Watermarks

- `state_applied_height` — AVL state advanced to here. External consumers see this.
- `script_verified_height` — scripts confirmed up to here. Internal bookkeeping.
- `downloaded_height` — all required section bytes are present in the store.

### Invariants

- `script_verified_height <= state_applied_height <= downloaded_height <= chain_height`
- `state_applied_height` is monotonically increasing (except on reorg/eval-failure reset)
- Heights at or below `script_verified_height` are fully validated (state + scripts)

### `advance_state_applied_height()`

Triggered after `downloaded_height` advances or on a timer. For each height
from `state_applied_height + 1` to `downloaded_height`:

1. Get header, sections, preceding headers, active params
2. Call `validator.apply_state(...)`
3. On Ok: advance `state_applied_height`, apply epoch boundary params,
   spawn `evaluate_scripts(deferred_eval)` on rayon pool if Some
4. On Err: stop, log error, do NOT advance watermark

Between blocks: non-blocking drain of eval result channel to advance
`script_verified_height`. On eval failure: rollback (see Failure Handling
in spec).

### SyncStore extension

The `SyncStore` trait gains one method:

```rust
fn get_modifier(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>>;
```

Reads section bytes from the store. The existing `has_modifier` checks existence;
this returns the actual data for validation.

### Startup re-evaluation

On startup, if `script_verified_height < state_applied_height`, rebuild
`DeferredEval` for gap blocks from stored sections and evaluate before
resuming normal sync.

### Reorg handling

On `DeliveryControl::Reorg { fork_point, .. }` (received via unbounded control channel):
1. Drain and discard in-flight eval results
2. Reset `downloaded_height` to fork_point
3. Get header at fork_point from chain
4. Call `validator.reset_to(fork_point, header.state_root)`
5. `state_applied_height` and `script_verified_height` reset to fork_point
6. Re-queue sections for the new branch, re-scan watermark
7. Re-validate from fork_point + 1 as sections become available

## Does NOT own

- Header validation — that's `chain/`
- Persistent storage — that's `store/`
- UTXO state persistence — that's `state/` (Phase 4b)
- Deciding when to validate — that's `sync/`
- Network I/O — that's `p2p/`

## Dependencies

- `ergo-chain-types` — Header, ADDigest, Digest32, BlockId
- `ergo-lib` — Transaction, ErgoBox, TransactionContext, ErgoStateContext
- `ergo_avltree_rust` — BatchAVLVerifier, Operation, KeyValue
- `sigma-ser` — VLQ decoding for section container formats
- `blake2` — proof digest verification

## Future: Phase 4b (UTXO mode)

`UtxoValidator` implements `BlockValidator` using a `PersistentBatchAVLProver`
instead of `BatchAVLVerifier`. Same validation core — different box source:
- Input boxes come from tree lookup, not proof output
- State root verified by tree operations, not proof replay
- AD proofs not required (not downloaded in UTXO mode)
- AD proofs generated as side effect (to serve digest-mode peers)
- `reset_to()` uses `PersistentBatchAVLProver::rollback()`

The `BlockValidator` trait is designed for both. `ad_proofs: Option<&[u8]>`
is `Some` for digest, `None` for UTXO.

## ADProof Regeneration (UTXO-mode diagnostic)

UTXO mode generates each block's ADProof as a side effect of applying state
(the prover's `generate_proof()` after the block's operations — see
`apply_state_internal`) but normally discards it: the hot path serves no
proofs ("Future: Phase 4b" above). On current testnet there is no reachable
peer that keeps ADProofs (all observed peers run UTXO mode and discard them),
so a digest client cannot obtain historical proofs from the network at all.
Regenerating them locally is the only route.

`UtxoValidator` can be configured to persist the proof at specific heights
during a genesis→target replay:

```rust
impl UtxoValidator {
    /// Persist the generated ADProof as a raw type-104 section at each height
    /// in `heights`, into `dir`. Disabled by default (empty set / `None` dir):
    /// zero overhead in normal operation. Intended for one-shot regeneration
    /// via a genesis→target replay — the prover must pass through H-1 → H for
    /// the proof at H to be correct — NOT steady-state serving.
    pub fn set_adproof_dump(&mut self, heights: HashSet<u32>, dir: PathBuf);
}
```

Behavior: when `apply_state` applies a block whose height is in `heights`, the
ADProof bytes from the apply-time `generate_proof()` (the same proof a digest
peer would verify, covering exactly that block's operations) are wrapped via
`serialize_ad_proofs(header_id, proof)` (raw type-104 section bytes) and written
to `<dir>/adproofs-<height>.104`. Logged at INFO. A write failure is logged at
WARN and MUST NOT fail block application (diagnostic, opportunistic).

Not part of the `BlockValidator` trait — UTXO-specific (digest mode has no
prover to generate from). Realizes the Phase 4b "AD proofs generated as side
effect (to serve digest-mode peers)" intent for the regeneration use case.

## Genesis State Root

The genesis state root is the ADDigest of an AVL+ tree containing only the
3 genesis boxes (emission, no-premine, founders). This is a hardcoded constant
per network, matching the JVM's `genesisStateDigestHex` in chain config.

Testnet and mainnet have different genesis digests. The node configuration
determines which one to use.

- Testnet: `cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502`
- Mainnet: `a5df145d41ab15a01e0cd3ffbab046f0d029e5412293072ad0f5827428589b9302`

## Startup: Resume from Stored Chain

When the node restarts with a stored header chain, the DigestValidator
initializes from the chain tip's `state_root` via `from_state()`, not
from genesis. The sync machine's `downloaded_height` and `validated_height`
are initialized from the validator's state. This avoids re-validating all
historical blocks.

Constraint: the store must have ADProofs for blocks above the validator's
starting height. A store populated in UTXO mode lacks ADProofs for
historical blocks — only blocks synced after switching to digest mode
will have ADProofs available for validation.

## Implementation Notes (Verified Against Testnet)

### ergo_avltree_rust Resolver

The `AVLTree` resolver is called on EVERY `left()/right()` child access,
not just for lazy-loading in the prover. `LabelOnly` sibling stubs from
the proof reconstruction are resolved too. The resolver MUST return a
`LabelOnly` node with the original digest label preserved:

```rust
fn label_preserving_resolver(digest: &[u8; 32]) -> Node {
    Node::LabelOnly(NodeHeader::new(Some(*digest), None))
}
```

A dummy resolver returning `label: None` causes panics on subsequent access
when the tree rebalancing or label computation touches the resolved stub.

### Operation Ordering

The JVM's `ErgoState.boxChanges()` uses `mutable.TreeMap[ModifierId, _]`
for both `toRemove` and `toInsert`. `ModifierId` is a hex-encoded String.
`TreeMap` sorts by natural String ordering = unsigned lexicographic byte
ordering of the raw 32-byte box IDs.

This means:
- Lookups: transaction data input order (preserved)
- Removes: **sorted by box ID bytes**
- Inserts: **sorted by box ID bytes**

Transaction output order is NOT preserved for inserts. The proof is
generated for this sorted order. Using any other order causes the verifier
to traverse wrong tree paths, hitting nodes not covered by the proof.

### Insert Values

The AVL+ tree Insert value is `ErgoBox::sigma_serialize_bytes()` — the
full box serialization including candidate fields + txId (32 bytes) +
index (u16). NOT just the candidate (without ref). Matches JVM's
`ErgoBox.bytes` via `ErgoBox.sigmaSerializer.toBytes()`.

### ADDigest Format

33 bytes: 32-byte Blake2b256 root hash + 1-byte tree height. The tree
height byte is the LAST byte. `ergo_chain_types::ADDigest` = `Digest<33>`.
`ergo_avltree_rust::ADDigest` = `bytes::Bytes`. Convert between them via
`[u8; 33]` intermediate.

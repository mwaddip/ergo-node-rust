# Parallel Validation Pipeline

**Date:** 2026-04-12
**Status:** Design approved, pending implementation

## Problem

Block validation is single-threaded. During genesis resync, the bottleneck
is ErgoScript evaluation — hundreds of milliseconds per block at higher
heights, while AVL state application takes single-digit milliseconds.
The sync loop waits for scripts to finish before advancing to the next block.

## Solution

Two layers of parallelism:

1. **Intra-block**: `rayon::par_iter` over transactions within a single
   block's script evaluation. Already implemented.
2. **Cross-block pipeline**: Split `validate_block` into a fast sequential
   state-application step and a deferred script-evaluation step. The sync
   layer advances state immediately and runs script eval in the background.

## Trait Contract

### Before

```rust
pub trait BlockValidator {
    fn validate_block(&mut self, ...) -> Result<ValidationOutcome, ValidationError>;
    fn validated_height(&self) -> u32;
    fn current_digest(&self) -> &ADDigest;
    fn reset_to(&mut self, height: u32, digest: ADDigest);
    fn proofs_for_transactions(&self, txs: &[Transaction])
        -> Option<Result<(Vec<u8>, ADDigest), ValidationError>>;
    fn emission_box_id(&self) -> Option<[u8; 32]>;
}
```

### After

```rust
pub trait BlockValidator {
    /// Apply state transition: parse sections, compute state changes,
    /// apply AVL operations, verify digest, persist.
    /// After Ok, state has advanced to this block's height.
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

    fn validated_height(&self) -> u32;
    fn current_digest(&self) -> &ADDigest;
    fn reset_to(&mut self, height: u32, digest: ADDigest);
    fn proofs_for_transactions(&self, txs: &[Transaction])
        -> Option<Result<(Vec<u8>, ADDigest), ValidationError>>;
    fn emission_box_id(&self) -> Option<[u8; 32]>;
}
```

`validate_block` is removed. `ValidationOutcome` is removed — its
`epoch_boundary_params` field moves into `ApplyStateOutcome`.

### New Types

```rust
pub struct ApplyStateOutcome {
    /// Some if this was an epoch-boundary block with verified parameters.
    pub epoch_boundary_params: Option<Parameters>,
    /// Some if scripts need evaluation (height > checkpoint).
    /// None if below checkpoint or no transactions.
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

### Free Function

```rust
/// Verify spending proofs for all transactions in a block.
/// Pure computation — no validator state needed.
pub fn evaluate_scripts(eval: &DeferredEval) -> Result<(), ValidationError> {
    validate_transactions(
        &eval.transactions,
        &eval.proof_boxes,
        &eval.header,
        &eval.preceding_headers,
        &eval.parameters,
    )
}
```

This calls the existing `validate_transactions` function, which already
uses `rayon::par_iter` for intra-block parallelism.

## Validator Implementations

### UtxoValidator

The current `validate_block` steps 1-9 split at the natural seam:

**`apply_state` does:**
1. Parse sections (BlockTransactions, Extension)
2. Epoch-boundary parameter check
3. Compute state changes from transactions
4. Build and apply AVL operations, capturing proof box bytes
5. Verify prover digest matches header state_root
6. Persist state (`generate_proof_and_update_storage`)
7. Track emission box
8. Advance internal height and digest

**Returns:** `ApplyStateOutcome` with `DeferredEval` containing deserialized
proof boxes, transactions, header, preceding headers, and parameters.
`deferred_eval` is `None` when `height <= checkpoint_height` or no
transactions exist.

The persist step (6) moves before script eval — safe because the digest
verified in step 5 proves the state transition is correct. If scripts
later fail, the sync layer calls `reset_to` to roll back.

### DigestValidator

Same split. "Apply state" verifies the AD proof via `BatchAVLVerifier`,
checks the resulting digest, updates the stored root hash. Proof boxes
come from the verifier's lookup/remove results. Returns `DeferredEval`
the same way.

### main.rs Validator Wrapper

Delegates `apply_state` to inner validator, then fires post-state side
effects: shared height update, state context publish, mempool block-applied
notification, mining proof pre-computation. These depend on state
advancement, not script completion — they trigger on `apply_state` Ok.

## Sync Layer Pipeline

### Height Watermarks

Two watermarks replace the single `validated_height`:

- **`state_applied_height`**: AVL state advanced to here. External consumers
  (API, mempool, mining) see this height.
- **`script_verified_height`**: Scripts confirmed up to here. Internal
  bookkeeping for rollback decisions. Advances in order as eval results
  arrive.

### Sweep Loop

```
for height in sweep_from..=sweep_to {
    gather sections from store
    outcome = validator.apply_state(...)
    apply epoch boundary params if present
    state_applied_height = height
    if let Some(eval) = outcome.deferred_eval {
        send eval to rayon pool via channel
    }
    drain channel (non-blocking) — advance script_verified_height
}
// after sweep: drain remaining eval results
```

### Eval Dispatch

`rayon::spawn` with `crossbeam_channel::Sender<(u32, Result<(), ValidationError>)>`.
Each eval task calls `evaluate_scripts(&deferred_eval)` and sends
`(height, result)` when done.

No backpressure, no bounded window. Memory analysis shows even worst-case
blocks (120 txs) use ~410KB per DeferredEval. At 128MB budget that's 300+
blocks, far more than the pipeline would ever queue. Typical blocks use
~25KB.

### At Chain Tip

When `sweep_size == 1` (single new block, live sync), drain the channel
synchronously after applying state — no pipelining benefit for one block,
and we want scripts verified before accepting the next peer block.

### Failure Handling

`apply_state` errors are immediate — stop sweep, no rollback needed
(state didn't advance).

`evaluate_scripts` errors are deferred. On detection:
1. Stop spawning new evals
2. Drain remaining channel results (discard)
3. Look up digest via `chain.header_at(failed_height - 1).state_root`,
   call `validator.reset_to(failed_height - 1, digest)`
4. Reset `state_applied_height` and `script_verified_height` to
   `failed_height - 1`
5. Reset `downloaded_height` to match
6. Log the error, resume sync (re-download and re-validate from failed
   height)

Eval results arrive out of order. The sync layer tracks the lowest failed
height and rolls back to there.

### Shutdown and Restart

The sync layer persists `script_verified_height` alongside
`state_applied_height`. On startup, if `script_verified_height <
state_applied_height`, the gap blocks are re-evaluated before resuming
normal sync. This is typically 0-5 blocks — a few seconds of work.

Rebuilds `DeferredEval` for each gap block from stored sections (already
in the modifier store).

## Implementation Order

1. **Update `facts/validation.md`** — new trait contract (`apply_state`
   replaces `validate_block`, new types, free function)
2. **Update `facts/sync.md`** — two-watermark model, pipeline loop,
   eval dispatch, failure/rollback semantics, startup re-eval
3. **Commit and push `facts/`** — contracts must land before any code
   touches the implementations
4. Implement validation crate changes
5. Implement main.rs wrapper changes
6. Implement sync layer pipeline

## New Dependencies

| Crate | Where | Why |
|-------|-------|-----|
| `rayon` | validation/ | Intra-block par_iter (already added) |
| `crossbeam-channel` | sync/ | Eval result channel |

No new dependencies in the validator trait itself.

## What Doesn't Change

- `proofs_for_transactions()` — mining uses it independently
- `emission_box_id()` — set during `apply_state`
- `reset_to()` — same signature
- `SyncChain`, `SyncStore`, `SyncTransport` traits
- Transaction validation logic (`validate_transactions`,
  `validate_single_transaction`)
- Section parsing, state changes computation, voting

## Performance Expectations

| Scenario | Before | After |
|----------|--------|-------|
| Intra-block (10+ tx block) | Sequential tx eval | rayon par_iter, ~Nx on N cores |
| Genesis resync (sweep) | Sequential block-by-block | AVL pipeline + background eval |
| Live sync (tip) | Same | Same (single block, no pipeline benefit) |
| Mempool validation | Unchanged | Unchanged |

The pipeline's benefit is proportional to the ratio of eval time to AVL
time. At early heights (few/no scripts), negligible. At v5+ heights with
complex scripts, substantial — the AVL path runs at ms per block while
eval takes 100s of ms.

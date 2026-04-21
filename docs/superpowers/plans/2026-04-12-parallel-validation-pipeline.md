# Parallel Validation Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `validate_block` into fast sequential `apply_state` + deferred `evaluate_scripts`, enabling cross-block pipelined validation during sync.

**Architecture:** The `BlockValidator` trait loses `validate_block`, gains `apply_state` which returns an `ApplyStateOutcome` with optional `DeferredEval`. A free function `evaluate_scripts` runs the ErgoScript evaluation (already rayon-parallelized). The sync layer calls `apply_state` sequentially, spawns script evals via `rayon::spawn` + `crossbeam-channel`, and drains results between blocks.

**Tech Stack:** Rust, rayon (already in validation/), crossbeam-channel (new in sync/)

**Spec:** `docs/superpowers/specs/2026-04-12-parallel-validation-pipeline-design.md`

---

### Task 1: Update facts/validation.md contract

**Files:**
- Modify: `facts/validation.md`

- [ ] **Step 1: Update the BlockValidator trait**

Replace the `validate_block` method and `ValidationOutcome` references with the new `apply_state` contract. In `facts/validation.md`, replace the `## Trait: BlockValidator` section (lines 33-83) with:

```markdown
## Trait: `BlockValidator`

\```rust
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

    /// Current validated height. 0 means no blocks validated yet.
    fn validated_height(&self) -> u32;

    /// Current state root digest (33 bytes).
    fn current_digest(&self) -> &ADDigest;

    /// Reset to a previous state. Used on reorg and deferred eval failure.
    fn reset_to(&mut self, height: u32, digest: ADDigest);

    /// Compute AD proofs for transactions without modifying state.
    /// None for digest-mode validators (mining requires UTXO mode).
    fn proofs_for_transactions(&self, txs: &[Transaction])
        -> Option<Result<(Vec<u8>, ADDigest), ValidationError>>;

    /// Current emission box ID. None if digest mode or all ERG emitted.
    fn emission_box_id(&self) -> Option<[u8; 32]>;
}
\```

## New Types

\```rust
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
\```

## Free Function

\```rust
/// Verify spending proofs for all transactions in a block.
/// Pure computation — no validator state needed. Uses rayon par_iter internally.
pub fn evaluate_scripts(eval: &DeferredEval) -> Result<(), ValidationError>;
\```
```

- [ ] **Step 2: Update the validation flow descriptions**

In the "Phase 4a: DigestValidator" validation flow section, update step 5 to say "Returned as `DeferredEval` in `ApplyStateOutcome` for the sync layer to evaluate asynchronously" instead of running inline.

Update step 6 ("Advance state") to clarify it happens immediately in `apply_state`, before script eval.

- [ ] **Step 3: Update the Integration: Sync Machine section**

Replace the `advance_validated_height()` description (lines 208-219) to reflect the two-watermark model:

```markdown
### Watermarks

- `state_applied_height` — AVL state advanced to here. External consumers see this.
- `script_verified_height` — scripts confirmed up to here. Internal bookkeeping.
- `downloaded_height` — all required section bytes are present in the store.

### Invariants

- `script_verified_height <= state_applied_height <= downloaded_height <= chain_height`
- `state_applied_height` is monotonically increasing (except on reorg/eval-failure reset)
- Heights at or below `script_verified_height` are fully validated (state + scripts)

### `advance_state_applied_height()`

Triggered after `downloaded_height` advances. For each height from
`state_applied_height + 1` to `downloaded_height`:

1. Get header, sections, preceding headers, active params
2. Call `validator.apply_state(...)`
3. On Ok: advance `state_applied_height`, apply epoch boundary params,
   spawn `evaluate_scripts(deferred_eval)` on rayon pool if Some
4. On Err: stop, log error, do NOT advance watermark

Between blocks: non-blocking drain of eval result channel to advance
`script_verified_height`. On eval failure: rollback (see Failure Handling
in spec).

### Startup re-evaluation

On startup, if `script_verified_height < state_applied_height`, rebuild
`DeferredEval` for gap blocks from stored sections and evaluate before
resuming normal sync.
```

- [ ] **Step 4: Commit contract update**

```bash
cd facts && git add validation.md && git commit -m "contract: split validate_block into apply_state + evaluate_scripts"
cd ..
```

---

### Task 2: Update facts/sync.md contract

**Files:**
- Modify: `facts/sync.md`

- [ ] **Step 1: Update the Block Assembly section**

In `facts/sync.md`, replace the "Block Assembly (downloaded_height / validated_height)" section (lines 329-359) with the two-watermark model:

```markdown
## Block Assembly (state_applied_height / script_verified_height)

The sync machine tracks three watermarks:

- **`downloaded_height`** — highest height where all required block sections
  are present in the store.
- **`state_applied_height`** — highest height where `apply_state()` returned Ok.
  External consumers (API, mempool, mining) see this height.
- **`script_verified_height`** — highest height where `evaluate_scripts()` has
  completed successfully. Internal bookkeeping for rollback decisions.
  Advances in-order as eval results arrive via crossbeam channel.

`downloaded_height` and `state_applied_height` are initialized from
`validator.validated_height()` on startup. `script_verified_height` is
persisted separately and loaded on startup.

### Invariants

- `script_verified_height <= state_applied_height <= downloaded_height <= chain_height`
- `state_applied_height` is monotonically increasing (except on reorg or eval failure)
- Heights at or below `script_verified_height` are fully validated (state + scripts)

### Eval dispatch

Script evaluation is dispatched to the rayon thread pool via `rayon::spawn`.
Results are sent through `crossbeam_channel::Sender<(u32, Result<(), ValidationError>)>`.
The sync layer drains the receiver non-blocking between blocks during the
sweep, and blocking after the sweep completes.

No backpressure. Memory per DeferredEval is ~25KB typical, ~410KB worst case.

### At chain tip

When `sweep_size == 1`, drain the eval channel synchronously after applying
state. No pipeline benefit for a single block during live sync.

### Eval failure handling

On eval failure detection:
1. Stop spawning new evals
2. Drain remaining channel results (discard)
3. Look up digest via `chain.header_at(failed_height - 1).state_root`
4. Call `validator.reset_to(failed_height - 1, digest)`
5. Reset `state_applied_height` and `script_verified_height` to `failed_height - 1`
6. Reset `downloaded_height` to match
7. Log the error, resume sync

### Startup re-evaluation

On startup, if `script_verified_height < state_applied_height`, rebuild
`DeferredEval` for each gap block from stored sections and evaluate before
entering the normal sync loop. Typically 0-5 blocks.
```

- [ ] **Step 2: Update the HeaderSync struct description**

In the `HeaderSync::new` section (lines 61-86), update the `validator` param
doc to reference `apply_state` instead of `validate_block`. Update the
watermark descriptions in the struct fields to use the new names.

- [ ] **Step 3: Update the Watermark scanner section**

Replace `advance_validated_height()` references with `advance_state_applied_height()`.
Mention the eval result draining between blocks.

- [ ] **Step 4: Update reorg handling**

In the "Deep reorg support" section (lines 136-152), update the sync machine
response to mention resetting `state_applied_height` and `script_verified_height`
(not just `validated_height`), and draining/discarding in-flight evals.

- [ ] **Step 5: Commit contract update**

```bash
cd facts && git add sync.md && git commit -m "contract: two-watermark model for pipelined validation"
cd ..
```

---

### Task 3: Push facts/ submodule

**Files:**
- Submodule: `facts/`

- [ ] **Step 1: Push the facts submodule**

```bash
cd facts && git push origin main && cd ..
```

- [ ] **Step 2: Stage the submodule pointer in the parent repo**

```bash
git add facts
```

Contracts are now landed. Code changes can begin.

---

### Task 4: Validation crate — new types and free function

**Files:**
- Modify: `validation/src/lib.rs`
- Modify: `validation/src/tx_validation.rs`

- [ ] **Step 1: Add new types to lib.rs**

In `validation/src/lib.rs`, replace `ValidationOutcome` (lines 28-41) with:

```rust
/// Outcome of a successful state application.
#[derive(Debug, Clone)]
pub struct ApplyStateOutcome {
    /// `Some(parsed)` if this was an epoch-boundary block AND the parsed
    /// parameters from its extension matched `expected_boundary_params`.
    pub epoch_boundary_params: Option<Parameters>,
    /// `Some` if script evaluation is needed (height > checkpoint).
    /// The caller should pass this to `evaluate_scripts()` — either
    /// inline or on a background thread.
    pub deferred_eval: Option<DeferredEval>,
}

/// Everything needed to verify transaction spending proofs.
/// Owned, `Send` — can move to any thread for background evaluation.
pub struct DeferredEval {
    /// Block height (for error reporting and result tracking).
    pub height: u32,
    /// Parsed transactions from the block.
    pub transactions: Vec<Transaction>,
    /// Input/data-input boxes extracted from state (keyed by box ID).
    pub proof_boxes: HashMap<[u8; 32], ErgoBox>,
    /// Block header.
    pub header: Header,
    /// Up to 10 preceding headers (newest first).
    pub preceding_headers: Vec<Header>,
    /// Active chain parameters.
    pub parameters: Parameters,
}
```

Add `use std::collections::HashMap;` to the imports at the top of lib.rs.

- [ ] **Step 2: Update the BlockValidator trait**

Replace the `validate_block` method signature (lines 65-75) with `apply_state`:

```rust
    #[allow(clippy::too_many_arguments)]
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
```

Update the trait doc comment to say "apply_state" instead of "validate_block".

- [ ] **Step 3: Update pub exports**

In lib.rs, update the `pub use` block to export the new types:

```rust
pub use tx_validation::{
    build_state_context, deserialize_box, evaluate_scripts, validate_single_transaction,
};
```

And ensure `ApplyStateOutcome` and `DeferredEval` are publicly accessible (they're defined at crate root, so they're already `pub`).

- [ ] **Step 4: Add evaluate_scripts to tx_validation.rs**

At the end of `validation/src/tx_validation.rs` (before the `#[cfg(test)]` block), add:

```rust
/// Verify spending proofs for all transactions in a block.
///
/// Pure computation — no validator state needed. Can run on any thread.
/// Uses rayon par_iter internally for intra-block parallelism.
pub fn evaluate_scripts(eval: &crate::DeferredEval) -> Result<(), crate::ValidationError> {
    validate_transactions(
        &eval.transactions,
        &eval.proof_boxes,
        &eval.header,
        &eval.preceding_headers,
        &eval.parameters,
    )
}
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p ergo-validation 2>&1 | tail -5`
Expected: errors from UtxoValidator/DigestValidator still implementing the old trait method. That's fine — we fix those next.

- [ ] **Step 6: Commit**

```bash
git add validation/src/lib.rs validation/src/tx_validation.rs
git commit -m "feat(validation): new trait contract — apply_state + evaluate_scripts"
```

---

### Task 5: UtxoValidator — implement apply_state

**Files:**
- Modify: `validation/src/utxo.rs`

- [ ] **Step 1: Rename and refactor validate_block to apply_state**

In `validation/src/utxo.rs`, rename `validate_block` to `apply_state` and change the return type from `Result<ValidationOutcome, ValidationError>` to `Result<ApplyStateOutcome, ValidationError>`.

The body changes:
- Steps 1-5 (parse, state changes, AVL ops, digest verify): unchanged.
- Step 6 (tx validation): instead of calling `validate_transactions` inline, build a `DeferredEval` and return it in the outcome.
- Step 7 (persist): stays, runs before returning.
- Steps 8-9 (emission tracking, advance state): unchanged.

Replace the current step 6 block (lines 168-182) and the return statement with:

```rust
        // 6. Build DeferredEval for deferred script verification
        let deferred_eval = if validate_txs {
            let mut proof_boxes = HashMap::with_capacity(proof_box_bytes.len());
            for (id, bytes) in &proof_box_bytes {
                proof_boxes.insert(*id, tx_validation::deserialize_box(bytes)?);
            }

            Some(crate::DeferredEval {
                height: header.height,
                transactions: parsed_txs.transactions,
                proof_boxes,
                header: header.clone(),
                preceding_headers: preceding_headers.to_vec(),
                parameters: active_params.clone(),
            })
        } else {
            None
        };

        // 7. Persist state changes + generate AD proof as side effect
        self.prover
            .generate_proof_and_update_storage(vec![])
            .map_err(|e| ValidationError::StateOperationFailed(
                format!("persist failed: {e}"),
            ))?;

        // 8. Track emission box: scan new outputs for emission contract
        if !self.emission_tree_bytes.is_empty() {
            self.emission_box_id = None;
            for (box_id, box_bytes) in &changes.insertions {
                if let Ok(ergo_box) = tx_validation::deserialize_box(box_bytes) {
                    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
                    if let Ok(tree_bytes) = ergo_box.ergo_tree.sigma_serialize_bytes() {
                        if tree_bytes == self.emission_tree_bytes {
                            self.emission_box_id = Some(*box_id);
                            break;
                        }
                    }
                }
            }
        }

        // 9. Advance state
        self.current_digest = header.state_root;
        self.validated_height = header.height;

        tracing::debug!(height = header.height, "state applied (UTXO mode)");

        Ok(crate::ApplyStateOutcome {
            epoch_boundary_params,
            deferred_eval,
        })
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p ergo-validation 2>&1 | tail -5`
Expected: DigestValidator error (still has old method), but UtxoValidator should be clean.

- [ ] **Step 3: Commit**

```bash
git add validation/src/utxo.rs
git commit -m "feat(validation): UtxoValidator implements apply_state"
```

---

### Task 6: DigestValidator — implement apply_state

**Files:**
- Modify: `validation/src/digest.rs`

- [ ] **Step 1: Read the current DigestValidator implementation**

Read `validation/src/digest.rs` in full to see the current `validate_block` body.

- [ ] **Step 2: Rename and refactor to apply_state**

Same pattern as UtxoValidator: rename method, change return type, build `DeferredEval` instead of calling `validate_transactions` inline. The AD proof verification and digest update happen in `apply_state`. Script eval is deferred.

The DigestValidator has no persist step (no persistent state beyond the 33-byte digest), so after AD proof verification and digest update, it returns immediately with the `DeferredEval`.

Replace the inline transaction validation block with `DeferredEval` construction:

```rust
        // Build DeferredEval for deferred script verification
        let deferred_eval = if validate_txs {
            let mut proof_boxes = HashMap::with_capacity(proof_box_bytes.len());
            for (id, bytes) in &proof_box_bytes {
                proof_boxes.insert(*id, tx_validation::deserialize_box(bytes)?);
            }

            Some(crate::DeferredEval {
                height: header.height,
                transactions: parsed_txs.transactions,
                proof_boxes,
                header: header.clone(),
                preceding_headers: preceding_headers.to_vec(),
                parameters: active_params.clone(),
            })
        } else {
            None
        };
```

And update the return:

```rust
        Ok(crate::ApplyStateOutcome {
            epoch_boundary_params,
            deferred_eval,
        })
```

- [ ] **Step 3: Verify validation crate compiles clean**

Run: `cargo check -p ergo-validation 2>&1 | tail -5`
Expected: success — both validators now implement the new trait.

- [ ] **Step 4: Commit**

```bash
git add validation/src/digest.rs
git commit -m "feat(validation): DigestValidator implements apply_state"
```

---

### Task 7: main.rs Validator wrapper — update to apply_state

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Read the Validator wrapper**

Read `src/main.rs` lines 150-350 to see the full `Validator` struct and its `BlockValidator` impl.

- [ ] **Step 2: Update the impl block**

Rename `validate_block` to `apply_state` in the `impl BlockValidator for Validator` block. Update the return type. The delegation to inner validators changes from `v.validate_block(...)` to `v.apply_state(...)`.

The post-validation side effects (shared height, state context, mempool notification, mining proofs) stay — they fire on `apply_state` Ok, same trigger as before. Update the result type handling:

```rust
impl BlockValidator for Validator {
    fn apply_state(
        &mut self,
        header: &ergo_chain_types::Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[ergo_chain_types::Header],
        active_params: &ergo_validation::Parameters,
        expected_boundary_params: Option<&ergo_validation::Parameters>,
    ) -> Result<ergo_validation::ApplyStateOutcome, ValidationError> {
        let result = match &mut self.inner {
            ValidatorInner::Digest(v) => v.apply_state(
                header, block_txs, ad_proofs, extension, preceding_headers,
                active_params, expected_boundary_params,
            ),
            ValidatorInner::Utxo(v) => v.apply_state(
                header, block_txs, ad_proofs, extension, preceding_headers,
                active_params, expected_boundary_params,
            ),
        };
        if result.is_ok() {
            // ... existing side effects unchanged ...
        }
        result
    }
    // ... rest of trait methods unchanged ...
}
```

- [ ] **Step 3: Update imports**

Replace `ValidationOutcome` with `ApplyStateOutcome` in the import line.

- [ ] **Step 4: Verify full workspace compiles**

Run: `cargo check 2>&1 | tail -10`
Expected: sync crate errors (still calls `validate_block`). main.rs and validation should be clean.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: Validator wrapper delegates apply_state"
```

---

### Task 8: Sync crate — add crossbeam-channel dependency and new fields

**Files:**
- Modify: `sync/Cargo.toml`
- Modify: `sync/src/state.rs`

- [ ] **Step 1: Add crossbeam-channel and rayon to sync/Cargo.toml**

```toml
crossbeam-channel = "0.5"
rayon = "1"
```

Add under `[dependencies]`.

- [ ] **Step 2: Add new fields to HeaderSync struct**

In `sync/src/state.rs`, add imports at the top:

```rust
use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
```

Replace the two watermark fields in the `HeaderSync` struct (lines 107-111):

```rust
    /// Highest height where ALL required block sections are in the store.
    downloaded_height: u32,
    /// Highest height where apply_state() returned Ok (state advanced).
    state_applied_height: u32,
    /// Highest height where evaluate_scripts() completed successfully.
    /// Advances in-order as results arrive. Persisted for startup re-eval.
    script_verified_height: u32,
    /// Number of eval tasks dispatched but not yet drained from the channel.
    evals_in_flight: u32,
    /// Channel for receiving eval results from rayon pool.
    eval_tx: CrossbeamSender<(u32, Result<(), ergo_validation::ValidationError>)>,
    eval_rx: CrossbeamReceiver<(u32, Result<(), ergo_validation::ValidationError>)>,
```

- [ ] **Step 3: Update HeaderSync::new()**

Initialize the new fields:

```rust
let (eval_tx, eval_rx) = crossbeam_channel::unbounded();
let initial_validated = validator.as_ref().map_or(0, |v| v.validated_height());
// ...
downloaded_height: initial_validated,
state_applied_height: initial_validated,
script_verified_height: initial_validated,
evals_in_flight: 0,
eval_tx,
eval_rx,
```

- [ ] **Step 4: Rename all validated_height references**

Search `state.rs` for `self.validated_height` and replace with `self.state_applied_height` throughout. This is a mechanical rename — the old `validated_height` becomes `state_applied_height`. `script_verified_height` is new and has no existing references to rename.

- [ ] **Step 5: Verify it compiles (expect errors from validate_block call)**

Run: `cargo check -p ergo-sync 2>&1 | tail -10`
Expected: error at the `validate_block` call site. Everything else should be clean.

- [ ] **Step 6: Commit**

```bash
git add sync/Cargo.toml sync/src/state.rs
git commit -m "feat(sync): add pipeline fields and crossbeam-channel dep"
```

---

### Task 9: Sync crate — pipelined sweep loop

**Files:**
- Modify: `sync/src/state.rs`

- [ ] **Step 1: Read the current advance_validated_height method**

Read `sync/src/state.rs` lines 500-663 to refresh context.

- [ ] **Step 2: Rename to advance_state_applied_height and rewrite**

Rename `advance_validated_height` to `advance_state_applied_height`. Replace the validation call and its surrounding logic. The new flow:

```rust
async fn advance_state_applied_height(&mut self) {
    if self.validator.is_none() {
        return;
    }
    if self.state_applied_height >= self.downloaded_height {
        return;
    }

    let sweep_from = self.state_applied_height + 1;
    let sweep_to = self.downloaded_height;
    let sweep_size = sweep_to - self.state_applied_height;
    let sweep_start = Instant::now();

    if sweep_size > 100 {
        tracing::info!(
            from = sweep_from, to = sweep_to, blocks = sweep_size,
            "=== VALIDATION SWEEP STARTED ==="
        );
    }

    let mut applied_to = self.state_applied_height;

    for height in sweep_from..=sweep_to {
        // ... existing section gathering code unchanged ...

        let result = self.validator.as_mut().unwrap().apply_state(
            &header,
            &block_txs,
            ad_proofs.as_deref(),
            &extension,
            &preceding,
            &active_params,
            expected_boundary_params.as_ref(),
        );

        match result {
            Ok(outcome) => {
                if let Some(new_params) = outcome.epoch_boundary_params {
                    self.chain.apply_epoch_boundary_parameters(new_params).await;
                }
                applied_to = height;

                // Spawn deferred script evaluation
                if let Some(eval) = outcome.deferred_eval {
                    let tx = self.eval_tx.clone();
                    self.evals_in_flight += 1;
                    rayon::spawn(move || {
                        let result = ergo_validation::evaluate_scripts(&eval);
                        let _ = tx.send((eval.height, result));
                    });
                }

                // Non-blocking drain of completed eval results
                self.drain_eval_results(false);

                // Progress reporting (same as before)
                let done = height - self.state_applied_height;
                if done % 1000 == 0 && sweep_size > 100 {
                    // ... existing progress logging ...
                }
            }
            Err(e) => {
                tracing::error!(height, error = %e, "apply_state failed");
                break;
            }
        }
    }

    if applied_to > self.state_applied_height {
        let advanced = applied_to - self.state_applied_height;
        self.state_applied_height = applied_to;
        // ... existing sweep-complete logging, updated to use state_applied_height ...
    }

    // After sweep: drain remaining results
    // If sweep_size was 1 (live sync at tip), drain synchronously
    let blocking = sweep_size <= 1;
    self.drain_eval_results(blocking);
}
```

- [ ] **Step 3: Implement drain_eval_results**

Add a new method to `HeaderSync`:

```rust
/// Drain completed script evaluation results from the channel.
///
/// `blocking`: if true, block until all in-flight evals complete
/// (used at chain tip where we want scripts verified before proceeding).
/// If false, drain only what's immediately available (non-blocking).
fn drain_eval_results(&mut self, blocking: bool) {
    // Track verified heights for in-order advancement.
    // Results may arrive out of order; we use a simple set to
    // buffer verified heights and advance sequentially.
    let mut verified: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();

    while self.evals_in_flight > 0 {
        let msg = if blocking {
            match self.eval_rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            }
        } else {
            match self.eval_rx.try_recv() {
                Ok(msg) => msg,
                Err(_) => break,
            }
        };

        self.evals_in_flight -= 1;
        let (height, result) = msg;
        match result {
            Ok(()) => {
                verified.insert(height);
            }
            Err(e) => {
                tracing::error!(
                    height, error = %e,
                    "script evaluation failed — rolling back"
                );
                self.handle_eval_failure(height);
                return;
            }
        }
    }

    // Advance script_verified_height sequentially
    while verified.contains(&(self.script_verified_height + 1)) {
        self.script_verified_height += 1;
        verified.remove(&self.script_verified_height);
    }
}
```

- [ ] **Step 4: Implement handle_eval_failure**

```rust
/// Handle a deferred script evaluation failure by rolling back state.
fn handle_eval_failure(&mut self, failed_height: u32) {
    // Drain and discard remaining results
    while self.eval_rx.try_recv().is_ok() {}
    self.evals_in_flight = 0;

    let rollback_to = failed_height - 1;

    // Reset validator state
    if let Some(v) = self.validator.as_mut() {
        // We need the digest at rollback_to. Use a blocking call
        // since this is a rare error path.
        let digest = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.chain.header_at(rollback_to).await
                    .map(|h| h.state_root)
            })
        });

        if let Some(digest) = digest {
            v.reset_to(rollback_to, digest);
        } else {
            tracing::error!(rollback_to, "cannot find header for rollback");
            return;
        }
    }

    self.state_applied_height = rollback_to;
    self.script_verified_height = rollback_to;
    if self.downloaded_height > rollback_to {
        self.downloaded_height = rollback_to;
    }

    tracing::warn!(
        rollback_to,
        "state rolled back due to script eval failure"
    );
}
```

Note: `handle_eval_failure` accesses `self.chain` which is async. Since this is called from a sync context during the sweep, use `block_in_place` + `block_on` (same pattern as the existing mining proof code in main.rs). This is a rare error path, not hot.

- [ ] **Step 5: Update all callers of the old method name**

Search `state.rs` for `advance_validated_height` and replace with `advance_state_applied_height`.

- [ ] **Step 6: Update reorg handling**

In `handle_control_event` (the `DeliveryControl::Reorg` arm), update to reset both watermarks and drain in-flight evals:

```rust
// Drain and discard any in-flight eval results
while self.eval_rx.try_recv().is_ok() {}

if self.state_applied_height > fork_point {
    // ... existing reset_to logic ...
    self.state_applied_height = fork_point;
    self.script_verified_height = fork_point;
}
if self.downloaded_height > fork_point {
    self.downloaded_height = fork_point;
}
```

- [ ] **Step 7: Verify sync crate compiles**

Run: `cargo check -p ergo-sync 2>&1 | tail -10`
Expected: success.

- [ ] **Step 8: Verify full workspace compiles**

Run: `cargo check 2>&1 | tail -10`
Expected: success (possibly with pre-existing warnings).

- [ ] **Step 9: Commit**

```bash
git add sync/src/state.rs
git commit -m "feat(sync): pipelined validation sweep with deferred script eval"
```

---

### Task 10: Startup re-evaluation of missed blocks

**Files:**
- Modify: `sync/src/state.rs`

- [ ] **Step 1: Add script_verified_height persistence**

The sync crate needs to persist `script_verified_height` across restarts. The simplest approach: add a method to `SyncStore` to read/write a u32 value by key, or store it alongside the existing state in the sync layer's startup path.

Check how `state_applied_height` is currently initialized (from `validator.validated_height()`). `script_verified_height` needs its own persistence. Add to the `SyncStore` trait in `sync/src/traits.rs`:

```rust
/// Read the persisted script_verified_height. Returns None if not set.
async fn script_verified_height(&self) -> Option<u32>;

/// Persist the script_verified_height.
async fn set_script_verified_height(&mut self, height: u32);
```

- [ ] **Step 2: Implement the SyncStore methods in the main crate**

In `src/main.rs`, the `SyncStore` implementation stores modifiers in redb. Add a small table or key for the script_verified_height. This is a single u32, stored/retrieved by fixed key.

- [ ] **Step 3: Add startup re-evaluation in HeaderSync::run()**

At the start of `HeaderSync::run()`, after validator initialization but before entering the sync loop, add:

```rust
// Re-evaluate scripts for blocks between script_verified_height
// and state_applied_height (gap from previous shutdown).
let persisted_svh = self.store.script_verified_height().await
    .unwrap_or(self.state_applied_height);
self.script_verified_height = persisted_svh;

if self.script_verified_height < self.state_applied_height {
    let gap = self.state_applied_height - self.script_verified_height;
    tracing::info!(
        from = self.script_verified_height + 1,
        to = self.state_applied_height,
        blocks = gap,
        "re-evaluating scripts for blocks missed during shutdown"
    );
    for height in (self.script_verified_height + 1)..=(self.state_applied_height) {
        // Rebuild DeferredEval from stored sections
        // (same section-gathering code as the sweep loop)
        // Call evaluate_scripts synchronously
        // On failure: handle_eval_failure and break
    }
}
```

The section-gathering code is the same as in the sweep loop (get header, get sections, build state context). Factor this into a helper if it avoids duplication, but don't over-abstract — it's two call sites.

- [ ] **Step 4: Persist script_verified_height during sweep**

In `drain_eval_results`, after advancing `self.script_verified_height`, persist it:

```rust
// Persist periodically (every 100 blocks) to avoid write overhead
if self.script_verified_height % 100 == 0 {
    // Fire and forget — this is best-effort persistence
    // The worst case is re-evaluating up to 100 blocks on restart
}
```

Or persist at the end of each sweep. The exact cadence is a tradeoff between write overhead and restart cost. Every 100 blocks is reasonable — worst case re-eval is 100 blocks (seconds).

- [ ] **Step 5: Verify full workspace compiles**

Run: `cargo check 2>&1 | tail -10`
Expected: success.

- [ ] **Step 6: Commit**

```bash
git add sync/src/traits.rs sync/src/state.rs src/main.rs
git commit -m "feat(sync): startup re-evaluation of unverified script blocks"
```

---

### Task 11: Integration test and verification

**Files:**
- No new test files — verify via existing test infrastructure and manual sync

- [ ] **Step 1: Run existing tests**

```bash
cargo test 2>&1 | tail -20
```

All existing tests must pass. The block_342964 test in tx_validation.rs exercises `validate_single_transaction` directly — it doesn't go through the trait, so it's unaffected by the rename.

- [ ] **Step 2: Build release binary**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 3: Manual verification plan**

The real test is a resync. On the test server:
1. Stop the node
2. Deploy the new binary
3. Wipe state (or let the current resync continue with the new binary)
4. Watch logs for `"state applied"` and `"script evaluation failed"` messages
5. Verify both watermarks advance and the pipeline is functioning

This is not automated — it's the same manual verification process used for all consensus changes.

- [ ] **Step 4: Commit any test fixes**

If any tests broke due to the trait change, fix them and commit.

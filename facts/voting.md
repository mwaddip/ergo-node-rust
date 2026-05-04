# Soft-Fork Voting Contract (in-repo side)

## Scope

This contract covers the **non-chain** side of soft-fork voting:

1. **Validator wiring** (`validation/`): at every block, query `chain.active_parameters()`
   for the bounds used by ErgoStateContext. At each epoch-boundary block, parse the
   parameters from the block's extension and verify they match
   `chain.compute_expected_parameters(block.height)`. On match, call
   `chain.apply_epoch_boundary_parameters(...)`. On mismatch, reject the block.

2. **Mining wiring** (`mining/` + `src/main.rs`): when assembling a candidate block,
   if the candidate height is an epoch boundary, the extension MUST include the
   parameters returned by `chain.compute_expected_parameters(candidate_height)`.
   Encode them via `pack_parameters` (mirrors JVM `Parameters.toExtensionCandidate`).

3. **Configuration**: surface miner voting preferences (which params to vote for,
   whether to vote for soft-fork) through `node.toml`. Translate into the 3-byte
   `votes` field on every mined block header.

The chain submodule owns the actual computation (`facts/chain.md` Phase 6:
Soft-Fork Voting). This contract describes how the rest of the node consumes
that computation.

## SPECIAL Profile

```
S10 P7 E7 C5 I8 A6 L10
```

Voting is consensus-critical. Strength is max — every code path must reject
malformed extensions and refuse epoch-boundary blocks that don't match expected
params. Luck is max — every edge case (genesis, reorg across boundary, missing
parent extension, voting cleanup epoch) must be handled or we fork.

## Module: `validation::voting`

New module inside `validation/`. Houses the extension-parameter serializer/parser
glue and the epoch-boundary validation hook.

### `parse_parameters_from_extension(ext: &ParsedExtension) -> Result<Parameters, ValidationError>`
- **Precondition**: `ext` is a parsed extension section from an epoch-boundary block.
- **Postcondition**: Returns the `Parameters` table extracted from extension fields
  with key prefix `0x00`. ID 124 (`SoftForkDisablingRules`) is parsed via the
  variable-length encoding from `ergo_lib::chain::parameters` (NOT as a 4-byte int).
- **Error cases**: Empty parameters table → reject. Wrong value length on a
  4-byte param ID → reject.
- **Replaces**: the orphaned `update_parameters_from_extension` helper currently
  in `validation/src/tx_validation.rs:39`. That helper merged into a current
  table; the new function returns a fresh table parsed from the extension. The
  validator compares this against `chain.compute_expected_parameters(...)`.
- **Maps to**: JVM `Parameters.parseExtension`.

### `pack_parameters(params: &Parameters) -> Vec<([u8; 2], Vec<u8>)>`
- **Postcondition**: Returns extension key-value fields encoding `params` for
  embedding in an epoch-boundary block's extension. Inverse of
  `parse_parameters_from_extension`.
- **Maps to**: JVM `Parameters.toExtensionCandidate`.
- **Used by**: mining task when assembling an epoch-boundary candidate.

## Validator wiring

### `BlockValidator::validate_block` changes

The existing trait is in `facts/validation.md`. No signature change. The
implementation in `validation/src/digest.rs` and `validation/src/utxo.rs`
must add an epoch-boundary check.

#### Per-block precondition (every block, not just epoch boundaries)

`BlockValidator` no longer carries its own `Parameters` field. The
`Parameters::default()` lines in `digest.rs:56` and `utxo.rs:68` are removed.
Instead, the validator borrows `chain.active_parameters()` when building the
`ErgoStateContext` for transaction validation.

This means `BlockValidator` needs a reference to the chain (or at least to
its active parameters). Two options, pick one in implementation:

- **Option A**: Pass `&Parameters` into `validate_block` as a new parameter.
  Caller (sync pipeline) reads from chain. Slightly more plumbing but keeps
  the validator decoupled.
- **Option B**: Pass an `Arc<HeaderChain>` (or chain handle) at validator
  construction. Validator queries on demand. Tighter coupling but no caller
  changes.

The implementer chooses. Document the choice in the implementation PR.

#### Epoch-boundary check (only when `chain.is_epoch_boundary(header.height)` is true)

Inserted between section parsing and AD-proof verification:

```rust
if chain.is_epoch_boundary(header.height) {
    let parsed_params = parse_parameters_from_extension(&parsed_extension)?;
    let block_proposed_update =
        chain::voting::extract_disabling_rules_from_kv(&parsed_extension.fields);
    let expected_params = chain.compute_expected_parameters(
        header.height,
        &block_proposed_update,
    )?;
    if parsed_params != expected_params {
        return Err(ValidationError::ParameterMismatch {
            height: header.height,
            expected: expected_params,
            actual: parsed_params,
        });
    }

    // `Parameters.matchParameters60` `proposedUpdate` check. Gated on
    // BlockVersion >= Interpreter60Version (v4 on mainnet, from
    // h=1,628,160 onward). The expected value lives on the chain
    // (`active_proposed_update_bytes()`) and tracks each accepted
    // boundary block byte-for-byte; before any boundary has been
    // applied it returns the launch default, but the gate below
    // means that seed is never the operand in production.
    if parsed_params.block_version() >= INTERPRETER_60_VERSION
        && chain.active_proposed_update_bytes() != block_proposed_update.as_slice()
    {
        return Err(ValidationError::ProposedUpdateMismatch {
            height: header.height,
            expected: chain.active_proposed_update_bytes().to_vec(),
            actual: block_proposed_update,
        });
    }

    // Note: chain.apply_epoch_boundary_parameters() is called by the caller
    // AFTER validate_block returns Ok, NOT inside the validator. Validator
    // does not mutate chain state.
}
```

`block_proposed_update` is the raw `ErgoValidationSettingsUpdate` payload
from extension key `[0x00, 124]`; it gates the `SubblocksPerBlock`
auto-insert at BlockVersion==4 activation (rule 409 in the activated
update → skip insert), and above v4 it's additionally the "actual" side of
the `matchParameters60` `proposedUpdate` byte-for-byte comparison. See
`facts/chain.md` Phase 6 for the full semantics.

`INTERPRETER_60_VERSION = 4` — JVM constant. The comparison
short-circuits for BlockVersion 1-3 because pre-v4 mainnet blocks
carry ID 124 values the current encoder does not model (statusUpdates
present on-chain since before h=1,562,624). Once the encoder is
extended to emit statusUpdates, the gate can be relaxed. Tracked as
follow-up.

The caller (sync pipeline / append-block flow) calls
`chain.apply_epoch_boundary_parameters(parsed_params, block_proposed_update)`
after the full block has been validated and persisted — both the
parameters and the ID 124 bytes advance in a single call. This keeps
the validator stateless w.r.t. chain state mutation.

### New error variants

```rust
ValidationError::ParameterMismatch {
    height: u32,
    expected: Parameters,
    actual: Parameters,
}

ValidationError::ProposedUpdateMismatch {
    height: u32,
    expected: Vec<u8>,
    actual: Vec<u8>,
}
```

Added to `validation/src/error.rs` (or wherever `ValidationError` lives).
`ProposedUpdateMismatch` is only emitted at BlockVersion >=
Interpreter60Version; before then the check short-circuits.

## Mining wiring

### `mining/src/extension.rs` changes

`build_extension` currently takes `(&Header, &[BlockId])`. It must learn
about epoch-boundary parameter emission. New signature:

```rust
pub fn build_extension(
    parent: &Header,
    parent_interlinks: &[BlockId],
    boundary_params: Option<&Parameters>,
) -> Result<ExtensionCandidate, MiningError>;
```

When `boundary_params` is `Some`, the returned `ExtensionCandidate` includes
the packed parameter fields (via `validation::voting::pack_parameters`) in
addition to the existing interlink fields. When `None`, only interlinks are
emitted (within-epoch blocks).

The mining task in `src/main.rs` decides which mode based on
`chain.is_epoch_boundary(candidate_height)`:

```rust
let candidate_height = parent.height + 1;
let boundary_params = if chain.is_epoch_boundary(candidate_height) {
    // `proposed_update_bytes` is the ID-124 payload the miner plans to emit
    // for this boundary. At a voting-driven BlockVersion==4 activation the
    // miner must emit the activated update (including rule 409) or chain
    // will diverge from JVM — see `facts/chain.md` Phase 6.
    let proposed_update_bytes: &[u8] = miner_proposed_update_for(candidate_height);
    Some(chain.compute_expected_parameters(candidate_height, proposed_update_bytes)?)
} else {
    None
};
let extension = build_extension(&parent, &parent_interlinks, boundary_params.as_ref())?;
```

### Vote-byte emission

The header `votes` field (3 bytes: 2 param vote slots + 1 fork vote slot)
is currently set from `node.toml` as a static `[u8; 3]`. This stays.

For first release, the node operator configures their votes statically;
auto-suggestion (`Parameters.suggestVotes` in JVM) is out of scope. Document
that the operator is responsible for choosing valid vote bytes.

Vote byte semantics:
- Positive byte (1-8): vote to increase that parameter
- Negative byte (`-1` to `-8`): vote to decrease that parameter
- `120`: vote for soft-fork
- `0`: no vote

### Configuration: `node.toml`

Existing:
```toml
[node.mining]
miner_pk = "..."
votes = [0, 0, 0]    # already exists
```

No new config keys for first release. The operator sets `votes` directly.
A future enhancement could add `[node.mining.voting]` with target values
that auto-translate, mirroring JVM's `voting { 1 = 1000000 }` syntax.

## Pipeline wiring

The block-application flow in `src/pipeline.rs` (or wherever the validator
is driven) must call
`chain.apply_epoch_boundary_parameters(params, block_proposed_update_bytes)`
AFTER a successful epoch-boundary block validation, BEFORE acknowledging
the block to the rest of the system. Both the parameter table and the ID
124 bytes advance atomically in a single call.

Order:
1. Validator returns `Ok` for an epoch-boundary block.
2. Block is persisted to `store/`.
3. `chain.apply_epoch_boundary_parameters(params, proposed_update_bytes)`
   is called with the parsed params and the block's ID 124 payload
   (empty `Vec` if absent).
4. The block is announced to peers / sync state advances.

Step 3 must happen before step 4 — otherwise the next block validates
against stale parameters.

### Reorg handling

When the chain reorgs across an epoch boundary, the validator's caller
must reset chain parameters to the correct value at the new tip. Two
options (chain submodule decides):

- Chain stores per-height parameter snapshots (cheap; one entry per epoch
  boundary).
- Chain re-walks back to the most recent epoch boundary on reorg.

`facts/chain.md` Phase 6 punts this to "see Startup recomputation" — same
machinery, different trigger. The validator side just needs to know that
after a reorg, `chain.active_parameters()` will be correct by the time
validation resumes.

## Cross-references

- `facts/chain.md` Phase 6: Soft-Fork Voting — the chain side
- `facts/validation.md` — `BlockValidator` trait (parameters change documented above)
- `facts/mining.md` — mining task structure
- JVM: `ergo-core/src/main/scala/org/ergoplatform/settings/Parameters.scala`

## Test plan

1. **Unit**: `parse_parameters_from_extension` round-trips through `pack_parameters`.
2. **Unit**: `parse_parameters_from_extension` rejects empty tables, wrong value lengths.
3. **Integration**: validate a known-good testnet epoch-boundary block (replay from
   testnet at the next 128-block boundary). Verify it accepts.
4. **Integration**: take that same block, mutate one parameter value, re-feed to
   validator. Verify it rejects with `ParameterMismatch`.
5. **End-to-end**: deploy to test server, observe the next epoch boundary, confirm
   the Rust node accepts and the JVM peer's acceptance matches.

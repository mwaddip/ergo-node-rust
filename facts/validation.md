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

## Traits: `BlockValidator`, `StatePersistence`, `MiningState`

**Split in v0.8.0.** Until then this was one trait whose last four methods
carried do-nothing default bodies, and that shape cost us a bug that ran
undetected for the life of the feature.

`impl BlockValidator for Validator` (the enum wrapper in `src/main.rs`) is pure
delegation: every method had to forward to the active variant. When
`resize_cache` was added, the wrapper was not updated. It compiled, because the
trait supplied a default returning `Ok(())`. So the at-tip cache resize called
into the wrapper, hit the default, did nothing, returned `Ok`, and **logged
success** — `UtxoValidator::resize_cache` was correct throughout and simply
unreachable. The resize never once reached `state.redb`. Found by @odiseusme on
2026-08-12 by reading `main.rs` rather than trusting the metrics; confirmed
independently before the fix. `flush` is the same shape and far worse: a
forgotten forward there means state is never persisted while every caller is
told it was.

The first fix considered was "declare every method, no defaults, let the
compiler catch the next wrapper that forgets." That works, but it still relies
on someone reading nine compiler errors correctly the next time a method is
added, and it costs 24 explicit no-op bodies across the workspace — 20 of them
in `sync/`'s test stubs.

**The split removes the failure mode instead of detecting it.** The four
methods had one consumer each and did not share it:

| Method | Consumer |
|---|---|
| `flush`, `resize_cache` | `sync/` — sweep flush points and at-tip cache tuning |
| `proofs_for_transactions`, `emission_box_id` | `main` — `Validator::update_mining_proofs` |

So they become two traits, each implemented **only by `UtxoValidator`**. There
is no per-method forwarding left in the wrapper to forget: a method added to
either trait needs no wrapper change at all.

One accessor does survive on `BlockValidator` — `state_persistence()`, and it
is **required, not defaulted**. `sync/` is generic over `V: BlockValidator` and
cannot name main's `Validator`, so a trait method is the only route by which it
can reach storage at all. That is a smaller surface than four defaulted methods
and a different kind of thing: it reports a capability rather than performing
work, so `None` is a truthful answer where `Ok(())` was a false claim. See
"How callers reach the split traits" below.

⚠ **The absence of an impl is the mode signal.** `DigestValidator` does not
implement either new trait. Do not reintroduce "digest mode returns a harmless
value" anywhere — that is precisely the shape that produced the bug above.

**This split also disambiguates two overloaded `None`s.**
`proofs_for_transactions` returned `Option<Result<..>>` where the outer `Option`
meant "wrong mode"; that layer is gone. `emission_box_id` returned `None` for
*either* "digest mode" *or* "all ERG emitted" — two unrelated facts wearing one
value, which `update_mining_proofs` then early-returned on identically. It now
means only **all ERG emitted**.

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
    ///     newest first (for ErgoStateContext in ScriptEvalInputs)
    ///   - `active_params` is the current chain parameters
    ///   - `expected_boundary_params` is Some iff header.height is an
    ///     epoch boundary
    ///
    /// Postconditions on Ok:
    ///   - `self.validated_height()` == header.height
    ///   - `self.current_digest()` == header.state_root
    ///   - State transition persisted (UTXO mode) or digest updated (digest mode)
    ///   - The block's scripts have been evaluated and passed. `Ok` from
    ///     `apply_state` means exactly that; there is nothing left owed.
    ///   - `ApplyStateOutcome.epoch_boundary_params` is Some if this was
    ///     an epoch-boundary block with verified parameters
    ///
    /// Postconditions on Err:
    ///   - State is unchanged: validated_height, current_digest, AND THE
    ///     PROVER are exactly as before the call. See "Err leaves the prover
    ///     clean" below — this was aspirational until 2026-08-12 and the
    ///     digest-mismatch path violated it.
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
        expected_proposed_update: Option<&[u8]>,
    ) -> Result<ApplyStateOutcome, ValidationError>;

    /// Current validated height. 0 means no blocks validated yet
    /// (genesis state root is set but no blocks applied).
    fn validated_height(&self) -> u32;

    /// Current state root digest (33 bytes: 32-byte hash + 1-byte tree height).
    fn current_digest(&self) -> &ADDigest;

    /// Reset to a previous state. Used on reorg.
    ///
    /// Preconditions:
    ///   - `height < self.validated_height()`
    ///   - `digest` is the state_root from the header at `height`
    ///
    /// Postconditions on Ok:
    ///   - `self.validated_height()` == height
    ///   - `self.current_digest()` == digest
    /// Postconditions on Err (changed 2026-06-12 — was a silent swallow):
    ///   - the underlying state rollback FAILED and the validator's
    ///     observable state is UNCHANGED: `validated_height()`,
    ///     `current_digest()`, and the prover are exactly as before the
    ///     call. No cache advance on un-rolled state — the caller decides
    ///     recovery (see facts/sync.md reconciliation arms).
    ///   - DigestValidator: infallible reset (plain field assignment),
    ///     always Ok.
    fn reset_to(&mut self, height: u32, digest: ADDigest) -> Result<(), ValidationError>;

    /// Does this validator own persistent state? `Some` hands out its storage
    /// lifecycle; `None` means it owns none (digest mode).
    ///
    /// REQUIRED — no default body. `sync/` is generic over
    /// `V: BlockValidator` (`HeaderSync<T, C, S, V>`) and never names main's
    /// `Validator`, so this is the only route by which a generic caller can
    /// reach `StatePersistence`. `MiningState` needs no such accessor because
    /// its only consumer is `main`, which does name the type.
    ///
    /// This method answers a capability question; it does NOT perform work.
    /// That is what separates it from the defaulted no-ops it replaces — a
    /// `None` return is a truthful answer, whereas `Ok(())` from a defaulted
    /// `flush` was a claim that work happened.
    fn state_persistence(&self) -> Option<&dyn StatePersistence>;
}

/// Storage lifecycle. Implemented by `UtxoValidator` only — a validator that
/// owns no persistent state does not implement it, and the caller handles
/// that case explicitly rather than being handed a successful no-op.
pub trait StatePersistence {
    /// Force a durable commit (fsync) of all outstanding storage writes.
    /// Called at sweep flush points (bounds crash data loss) and on
    /// graceful shutdown.
    ///
    /// Postconditions on Ok: every write issued before this call is durable.
    /// Postconditions on Err: durability is UNKNOWN. The caller must not
    /// advance any watermark that assumes persistence — see facts/sync.md
    /// § "Flush ordering".
    fn flush(&self) -> Result<(), ValidationError>;

    /// Resize the storage read cache at runtime (e.g. on reaching the tip).
    ///
    /// ⚠ Read cache only. `stateCacheBytes` covers read + write, so a 64 MB
    /// resize gives roughly a 128 MB envelope, not 64.
    fn resize_cache(&self, cache_bytes: usize) -> Result<(), ValidationError>;
}

/// Mining support. Implemented by `UtxoValidator` only — candidate assembly
/// requires a live UTXO set, so digest mode does not implement it and
/// `main` skips mining rather than receiving a `None` that means "wrong mode".
pub trait MiningState {
    /// Compute AD proofs and the resulting state root for a set of
    /// transactions WITHOUT modifying persistent state.
    fn proofs_for_transactions(&self, txs: &[Transaction])
        -> Result<(Vec<u8>, ADDigest), ValidationError>;

    /// Current emission box ID in the UTXO set, updated after each block.
    ///
    /// `None` means **all ERG has been emitted** — nothing else. It no
    /// longer doubles as the digest-mode signal.
    fn emission_box_id(&self) -> Option<[u8; 32]>;
}
```

### Who implements what

| Type | `BlockValidator` | `state_persistence()` returns | `StatePersistence` | `MiningState` |
|---|---|---|---|---|
| `UtxoValidator` | ✅ | `Some(self)` | ✅ | ✅ |
| `DigestValidator` | ✅ | `None` | — | — |
| `Validator` (enum wrapper, `src/main.rs`) | ✅ | match on variant | — | — |
| `sync/` stubs exercising flush (4) | ✅ | `Some(self)` | ✅ | — |
| `sync/` stubs that do not (2) | ✅ | `None` | — | — |

### How callers reach the split traits

**Two different mechanisms, because the two consumers differ.**

`sync/` is generic (`HeaderSync<T, C, S, V: BlockValidator>`) and cannot name
main's `Validator`, so it reaches storage through the trait method:

```rust
// in sync/, where V: BlockValidator
match validator.state_persistence() {
    Some(p) => p.flush(),          // real work, real Result
    None    => /* nothing to persist — see facts/sync.md */,
}
```

`main` names `Validator` directly and is `MiningState`'s only consumer, so
that one is an inherent accessor on the wrapper and needs no trait surface:

```rust
impl Validator {
    fn mining_state(&self) -> Option<&dyn MiningState>;
}
```

⚠ Do not "regularise" these into one shape. Putting `mining_state()` on
`BlockValidator` would force six `sync/` test stubs to declare a capability
they have no consumer for; making `state_persistence()` inherent would put it
out of reach of the only caller that needs it.

Both return references borrowed from `&self`, so they cannot outlive the
caller's guard — relevant because `UtxoValidator` holds `Rc`s through the AVL
prover and the wrapper carries a hand-written `Send` assertion (`src/main.rs`,
see its SAFETY comment). Check that reasoning rather than inheriting it.

⚠ **`E0034` hazard, found in step A.** With both `BlockValidator` and
`MiningState` in scope on a *concrete* `UtxoValidator`,
`proofs_for_transactions` resolves ambiguously and the call must be qualified
(`BlockValidator::proofs_for_transactions(&v, ..)`). Generic `V: BlockValidator`
call sites are unaffected. The ambiguity disappears in step E with the shims.

### No behavioural delta

The split changes no runtime behaviour. Digest mode previously returned
`Ok(())` from the defaulted `flush`/`resize_cache`; it now yields `None` from
`state_persistence()`, and **the caller treats that as the existing
"nothing to persist" arm, not as a failure** — `sync/` already models this
(`FlushOutcome`, the no-validator case). A flush that cannot happen must not
be reported as a flush that failed; nothing may be pruned or advanced on the
strength of it either way.

## Script evaluation (deferred mode removed in v0.8.0)

`apply_state` **always** evaluates the block's scripts itself, before
persisting. `Ok` therefore means the scripts passed. There is no mode, no
`ApplyStateOutcome::deferred_eval`, and no evaluation the caller still owes.

⚠ **Deferred evaluation is gone and is not returning as an option.** It let
`apply_state` return before the scripts ran — buying sync throughput at the
price of a crash-consistency window and, worse, a second source of truth for
*how far this chain is actually verified*. Every bug in that machinery came
from the same root, that verification lagged application:

- the frozen reorder-buffer watermark, which wedged a node for 190,000 blocks
- `handle_eval_failure` zeroing `evals_in_flight` while its rayon tasks were
  still running and still holding their heap — the undercount that opened the
  dispatch gate early
- the unbounded backlog that killed a 4-thread 1.8 GHz host at **anon-rss
  10.62 GiB** mid-catch-up
- the checkpoint frontier floor, needed only because heights at or below the
  checkpoint never dispatched an eval and so could never advance a frontier

Removing the lag removes the class. Anything that reintroduces "apply now,
verify later" reintroduces all of it.

**Evaluation happens before persistence, not before application.** The
JVM validates scripts before touching its AVL tree, but it can afford to: it
reads each input box via `boxById` and removes it afterwards, paying two
traversals. Ours captures the box from the removal's own return value, so a
literal copy would add a read per input for no gain.

The boundary that actually matters is **persistence**, because persistence is
what survives a crash. Everything from the first prover operation to just
before `storage.update_with_height` is in-memory only. Evaluating in that
window means no block whose **scripts** are unverified reaches `state.redb`,
which closed the startup-gap hole at the source rather than repairing it
afterwards — and then let `sync/` delete the repair machinery entirely.

Ordering: apply operations and capture boxes → verify digest → **evaluate
scripts** → persist. The digest check stays first because it is cheap and
rejects malformed blocks before the expensive step.

⚠ **This closes the script gap only. It does not make `apply_state`
crash-atomic.** The proof-digest consensus check still runs *after*
`update_with_height`, so a post-persist `Err` remains reachable and a crash in
that window can still leave a persisted block that failed a later check. Moving
proof generation earlier does not fix it and has already been tried: reverted
in `96a0186`, because `removed_nodes()` then returns empty and superseded nodes
stop being deleted — the orphan-growth bug that reached 235 GB.

So the two windows are different sizes and only one of them closes. Anyone
citing "nothing unverified is persisted" should say **scripts**, or they are
overstating it.

### Err leaves the prover clean

**Requirement:** every `Err` from `apply_state` leaves the prover byte-for-byte
as it was on entry. Script failure, digest mismatch, box deserialization, the
persist, and the post-persist proof-digest check all have to satisfy it.

**This already worked** and has since `2992645`. `apply_state` is a wrapper
around `apply_state_internal` that calls `rollback_prover_to` on any `Err`;
every bare `return`/`?` inside `_internal` undoes nothing on its own and does
not need to. Inline evaluation is one more `Err` arm under the same wrapper.

*An earlier draft of this section asserted the opposite — that the
digest-mismatch path returned `Err` with the prover dirty and needed fixing.
That was wrong: it read `apply_state_internal`'s bare `return Err` and
attributed it to the public entry point. Recorded because the mistake is
repeatable — in a file with a `_internal` split, the enclosing function is part
of what a line means, and a grep result does not carry it.*

**Why the rollback is sound before persistence**, which is the part that is
genuinely non-obvious: at the evaluation point the undo log does not run at
all. `RedbAVLStorage` sets `current_version` only after a successful commit
(`state/src/storage.rs:1002`), so it still equals the pre-block digest, and
`rollback()` takes its short-circuit branch (`storage.rs:1034-1044`) — re-read
`META_TOP_NODE_HASH` from redb, unpack, return. No write transaction, no undo
record, no version-chain mutation. It is a persisted-root re-read.

The undo-log walk runs only for the **post-persist** proof-digest check, where
the block genuinely is the newest committed version and the walk is the correct
operation. Both paths work, for different reasons; do not collapse them.

Precondition: `storage.current_version` must be `Some`, which
`UtxoValidator::new` already documents and guarantees.

**`restore_root` clears `base.modified_nodes`** as of the fork rev this
workspace pins — `b955790`, in the `[patch.crates-io]` table of the root
`Cargo.toml`. Revs before it cleared the changed-node buffers and directions
and rebased `old_top_node` but left the address-keyed map `pack_tree` gates on
holding every node the failed block touched: not a correctness bug, since each
entry owns an `Rc` and a live address cannot be recycled, but an unbounded
retention that inline mode promotes from never-happens to once per hostile
block. `rollback_prover_to` carried a local `modified_nodes.clear()` for that;
it is redundant at this rev. Anyone reading an older tree should not conclude
the clear is load-bearing.

⚠ **The upstream fix carries a precondition on `state/`.** The same defect has
a worse form that we are exempt from *by construction, not by luck*: two
provers driven to identical tree state emitting **740 vs 735 proof bytes** —
same digest, different proof. `on_node_visit` keys every **visited** node, not
only modified ones, so a storage layer whose `rollback` hands back a **live**
`NodeId` restores a root whose nodes are still keyed in the stale map, and
`pack_tree` then expands nodes it should have labelled. Both
`RedbAVLStorage::rollback` paths end at `tree.unpack` of bytes freshly copied
out of a redb read transaction (`state/src/storage.rs:1042`, `:1140`), so the
restored root is always a fresh allocation and the divergence is unreachable
here. **A node-level cache in `state/` returning live `Rc` handles from
`rollback` would make a wrong proof reachable.** Caching the *bytes* is fine;
caching the *handles* is a consensus bug.

### Open: the block cost is discarded, and always was

`evaluate_scripts` returns the block-accumulated transaction cost and
`apply_state` drops it. Nothing is unenforced — the `maxBlockCost` gate runs
inside `evaluate_scripts` regardless.

*A previous draft of this section said the figure was "observable in deferred
mode and invisible in inline". That was wrong: the deferred path discarded it
too, at `sync/src/state.rs` — `evaluate_scripts(&eval).map(|_cost| ())`, with
the binding name documenting the discard. No caller has ever consumed it in
either mode, and the only cost-shaped value in the API is `max_block_cost`,
which is the parameter limit rather than what a block actually spent.*

So this is not a regression to repair but an observable to add, and it wants a
consumer first — a `cost=` field on the sweep line, or a block-level API
field. Adding it is a field on `ApplyStateOutcome` and one assignment.

## New Types

```rust
pub struct ApplyStateOutcome {
    /// Some if this was an epoch-boundary block with verified parameters.
    pub epoch_boundary_params: Option<Parameters>,
}

/// Everything needed to verify transaction spending proofs. Built inside
/// `apply_state` and consumed by `evaluate_scripts` a few lines later.
pub struct ScriptEvalInputs {
    pub height: u32,
    pub transactions: Vec<Transaction>,
    pub proof_boxes: HashMap<[u8; 32], ErgoBox>,
    pub header: Header,
    pub preceding_headers: Vec<Header>,
    pub parameters: Parameters,
}
```

**Renamed from `DeferredEval` in v0.8.0**, along with the removal of its
`approx_heap_bytes` field and the `new()` that derived it. That weight existed
so `sync/` could bound a queue by bytes in flight rather than item count —
per-item weight varies by three orders of magnitude, since `proof_boxes` holds
every input and data-input box of the block. There is no queue now. The struct
is a plain input bundle that never leaves the stack frame that built it, so it
no longer needs to be `Send`, no longer needs to weigh itself, and no longer
carries a name describing a deferral that does not happen.

## Free Function: proofs without a validator

```rust
/// Compute AD proofs and the resulting state root for `txs` against a
/// committed tree, without a `UtxoValidator` and without touching any
/// live prover.
pub fn proofs_from_storage(
    resolver: Resolver,
    root: Option<(Digest32, usize)>,
    txs: &[Transaction],
) -> Result<(Vec<u8>, ADDigest), ValidationError>;
```

`UtxoValidator::compute_proofs` delegates to this; the logic is unchanged and
lives in one place.

**Why it exists.** Mining assembles a candidate and needs proofs for it, but the
validator is owned by `sync/` and is `!Sync`, so the mining task cannot reach
it. `compute_proofs` never needed the validator's live prover — it deliberately
builds a separate one from storage so mining cannot disturb validation — so the
dependency was always storage access, not validator access. `SnapshotReader`
now supplies both inputs (`facts/state.md`).

⚠ **The resolver must be independent.** Every node it resolves must be a fresh
handle, never shared with another prover's tree. `state/` guarantees this
structurally — no caching, a fresh read transaction per resolve — and the
guarantee is load-bearing here: a prover working over nodes still keyed in
another prover's address-keyed map emits a **different proof for identical tree
state**. Same digest, different bytes. Do not add a node cache between these.

⚠ **A missing root reports one error, not two.** The signature takes a
resolver rather than a storage handle, so the committed root is materialised by
calling the resolver rather than `get_node` + `unpack`. The unpack is
byte-identical — the resolver is that call plus a clone — but its miss path
yields `Node::LabelOnly` instead of an `Err`. `unpack` itself never produces
that variant, so it uniquely identifies the miss and is what the code gates on.
The consequence is diagnostic: the former "failed to read root node: {e}" and
"root node not found in storage" collapse into the latter. Same
`ValidationError::StateOperationFailed`; the underlying redb cause is still
logged at ERROR by `state/`, with the digest.

⚠ **This is a read path.** It computes what proofs *would* be; it applies
nothing, persists nothing, and advances no watermark. A caller that treats its
success as evidence a block is valid has misread it.

## Free Functions: state context

Two builders, and **which one you call is a correctness decision, not a style
one.**

```rust
/// Context for validating a block that HAS been mined.
/// The preheader is `header` itself. Use for block validation only.
pub fn build_state_context(
    header: &Header,
    preceding_headers: &[Header],
    parameters: &Parameters,
) -> ErgoStateContext;

/// Context for validating a transaction that is NOT yet in a block.
/// The preheader describes the *next* block, built on `last_header`.
/// Use for the mempool and for API-submitted transactions.
pub fn build_upcoming_state_context(
    last_header: &Header,
    preceding_headers: &[Header],
    parameters: &Parameters,
) -> ErgoStateContext;
```

### Why two

An unconfirmed transaction is not a member of the chain tip — it is a candidate
for the block *after* it. Wallets set `creationHeight` to the block they expect
to land in, so a transaction built against tip `H` carries `creationHeight =
H+1`. ergo-lib enforces `creationHeight <= preHeader.height`. Validate it
against a preheader at `H` and **every well-formed transaction on the network is
rejected**, with the reason `Creation height H+1 > preheader height`.

This mirrors the JVM, which keeps the same split: block validation uses the
block's own header, while `ErgoMemPool` validates against
`ErgoStateContext.simplifiedUpcoming()`
(`ergo-core/.../nodeView/state/ErgoStateContext.scala:140`).

### Field derivation for the upcoming preheader

Mirrors `simplifiedUpcoming()` composed with `PreHeader.apply` and
`AutolykosPowScheme.derivedHeaderFields` (`PreHeader.scala:49-63`):

| Field | Value | Note |
|---|---|---|
| `height` | `last_header.height + 1` | the whole point |
| `parent_id` | `last_header.id` | **not** `last_header.parent_id` |
| `version` | `last_header.version` | |
| `n_bits` | `last_header.n_bits` | difficulty carries over |
| `timestamp` | `last_header.timestamp + 1` | JVM's literal `+ 1`, not wall clock |
| `miner_pk` | `ec_point::generator()` | free fn in `ergo-chain-types`, not an assoc fn |
| `votes` | `Votes([0, 0, 0])` | see below — not equivalent to the JVM |

`miner_pk` is the secp256k1 group generator rather than a real key because the
miner of the next block is unknown. A script reading `CONTEXT.preHeader.minerPk`
therefore sees a placeholder in the mempool and the real key once mined — the
same divergence the JVM has, and the reason a transaction can pass mempool
validation and still fail in a block.

⚠ **`votes` cannot match the JVM, and the difference is observable.** The JVM
passes `Array.emptyByteArray`; `Votes` is three fixed bytes and has no empty
representation, so the upcoming preheader carries `[0, 0, 0]`. A script reading
`CONTEXT.preHeader.votes` sees a three-byte zero collection here and an empty
one on a JVM node. No known script does, and a transaction that depended on it
would fail once mined anyway — the real block carries real votes — but it is a
genuine divergence rather than an equivalent encoding, so do not record it as
parity.

**Mining does not use this builder, deliberately.** `generate_candidate` builds
a stub header at `height + 1` with a real wall-clock timestamp and feeds that to
`build_state_context` (`facts/mining.md` § Selection). Its preheader height is
therefore already correct — which is why candidate assembly kept working while
the mempool rejected everything. The timestamp is the honest difference: a miner
knows the block's real timestamp, the mempool does not, so it uses the JVM's
`last.timestamp + 1` placeholder. Do not collapse the two paths into one.

⚠ **Parameters are the caller's, and lag by one block at an epoch boundary.**
The JVM recomputes parameters for `height + 1` inside `simplifiedUpcoming()`.
We pass the parameters active for `last_header` instead. These differ only on
the single block where an epoch boundary is crossed, and only for
parameter-sensitive validation. Accepted as a bounded divergence; revisit if a
boundary-block mempool rejection is ever observed.

### Invariant

**Never validate an unconfirmed transaction against a preheader at the current
tip.** The mempool and the API validate against the upcoming context; block
validation validates against the block's own header. A caller that mixes these
is wrong even when it appears to work — the failure is silent, total, and looks
exactly like an idle network.

## Free Function

```rust
/// Verify spending proofs for all transactions in a block.
/// Pure computation — no validator state needed. Uses rayon par_iter internally.
/// On success returns the block-accumulated transaction cost.
pub fn evaluate_scripts(eval: &ScriptEvalInputs) -> Result<u64, ValidationError>;
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
  `checkpoint_height` never reach evaluation (no `ScriptEvalInputs` is built),
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

5. **Evaluate scripts** (skipped below checkpoint_height) — BEFORE persisting
   - Deserialize old values from step 4 into `ErgoBox` instances
   - Bundle transactions, proof boxes, header, preceding headers, and
     parameters into `ScriptEvalInputs`
   - Call `evaluate_scripts()`, which uses rayon `par_iter` for intra-block
     parallelism and returns the block-accumulated cost (see "Block cost
     semantics"). On `Err` the block is rejected and the prover is rolled
     back by the `apply_state` wrapper — nothing is persisted.

6. **Persist, then advance state**
   - `validated_height` = header.height
   - `current_digest` = header.state_root

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
- `downloaded_height` — all required section bytes are present in the store.

`script_verified_height` was deleted in v0.8.0 along with deferred evaluation.
It existed to track how far behind application verification had fallen; since
`apply_state` now evaluates before persisting, application *is* verification
and a second watermark can only disagree with the first.

### Invariants

- `state_applied_height <= downloaded_height <= chain_height`
- `state_applied_height` is monotonically increasing (except on reorg reset)
- Heights at or below `state_applied_height` have had their state applied and
  their scripts either verified or explicitly skipped by `checkpoint_height` —
  one watermark, both facts.

### `advance_state_applied_height()`

Triggered after `downloaded_height` advances or on a timer. For each height
from `state_applied_height + 1` to `downloaded_height`:

1. Get header, sections, preceding headers, active params
2. Call `validator.apply_state(...)`
3. On Ok: advance `state_applied_height`, apply epoch boundary params. The
   block's scripts have already passed — `Ok` is the only assertion sync
   needs, and there is no second watermark to advance.
4. On Err: stop, log error, do NOT advance watermark

### SyncStore extension

The `SyncStore` trait gains one method:

```rust
fn get_modifier(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>>;
```

Reads section bytes from the store. The existing `has_modifier` checks existence;
this returns the actual data for validation.

### Reorg handling

On `DeliveryControl::Reorg { fork_point, .. }` (received via unbounded control channel):
1. Reset `downloaded_height` to fork_point
2. Get header at fork_point from chain
4. Call `validator.reset_to(fork_point, header.state_root)` — on Err the
   validator did NOT move; sync must not perform step 5 (watermarks stay
   where they were; see facts/sync.md)
5. On Ok: `state_applied_height` resets to fork_point
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
- `reset_to()` uses `PersistentBatchAVLProver::rollback()`; a rollback
  failure surfaces as Err with validator state unchanged (2026-06-12 —
  previously logged-and-swallowed while the cache advanced onto
  un-rolled state, the latent gap-wedge hole)

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

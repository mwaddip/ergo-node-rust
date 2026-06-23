# `blocks_to_keep` Storage Pruning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. **Note:** this project uses its own per-crate dispatch model via the `dispatching-prompts` skill (kitty terminal automation) rather than the Agent tool's subagent mechanism — see "Per-Crate Dispatch Model" below.

**Goal:** Implement actual storage pruning honoring the `blocks_to_keep` config value, replacing the current handshake-only advertisement. Bodies older than the configured horizon get deleted from `modifiers.redb` at flush time, with crash recovery guaranteed by capping the flush dial.

**Architecture:** Three-crate change (`store/`, `chain/`, `sync/`) plus main-session config wiring. Pruning piggybacks on the existing `flush_pair` atomically inside the modifier-store write transaction. Flush dial guardrails cap at `min(its config, blocks_to_keep)` so the `validated_height → tip` gap never exceeds what archived bodies cover. No new persistent state — startup WARN uses derived state per `[[feedback_derive_from_state]]`.

**Tech Stack:** Rust, redb, async tokio. Spec: `docs/design/2026-05-22-blocks-to-keep.md`.

---

## Per-Crate Dispatch Model

Per `CLAUDE.md`, main session does not edit crate-internal source. All work inside `store/`, `chain/`, `sync/` is dispatched to per-crate Claude sessions via the `dispatching-prompts` skill (kitty `--type=window` + `ac` + send-text). Main session owns:

- Interface contracts (`facts/`)
- Top-level docs, `prompts/`
- `src/main.rs` integration
- Dispatch orchestration + integration of completed work

The per-crate prompts in this plan are pure work-content (50-100 lines each). Boilerplate (load OVERRIDES.md, project skill, SPECIAL.md, "YOU ARE A DISPATCHED SESSION" marker) is added by the dispatching-prompts skill at dispatch time, not in the prompt file body.

## File Structure

**Created (main session):**
- `prompts/blocks-to-keep-store.md` — dispatched-session prompt for store work
- `prompts/blocks-to-keep-chain.md` — dispatched-session prompt for chain work
- `prompts/blocks-to-keep-sync.md` — dispatched-session prompt for sync work

**Modified (main session):**
- `facts/store.md` — contract for new methods
- `facts/chain.md` — contract for new accessor
- `facts/sync.md` — document pruning step + flush dial cap + startup WARN
- `src/main.rs` — wire `blocks_to_keep` through to sync (already wired to handshake)
- `prompts/blocks-to-keep-brainstorm.md` — refresh per end-of-shift handoff

**Modified (dispatched sessions):**
- `store/src/lib.rs` (and impl module) — add `prune_below_height` + `min_height_present`
- `store/tests/` — new test coverage
- `chain/src/lib.rs` — add `voting_length()` accessor
- `chain/tests/` — accessor test
- `sync/src/lib.rs` (and flush_pair module) — `compute_prune_horizon`, dial cap, prune call, startup WARN
- `sync/tests/` — coverage

---

## Tasks

### Task 1: Update `facts/store.md` with new methods

**Files:** Modify `facts/store.md`

- [ ] **Step 1: Add `prune_below_height` to the `ModifierStore` trait block.**

Insert after the existing `put_header_score_batch` declaration:

```rust
/// Delete all modifier rows of the given `type_ids` at heights strictly
/// less than `horizon`. Atomic in a single redb write transaction.
/// Idempotent — calling twice with the same horizon is a no-op the
/// second time. Returns the count of (type_id, modifier_id) pairs
/// deleted.
///
/// Precondition: `type_ids` is non-empty and contains no 101 (headers
/// are never pruned). Returns Err if `type_ids` contains 101.
fn prune_below_height(
    &self,
    horizon: u32,
    type_ids: &[u8],
) -> Result<usize, Self::Error>;

/// Returns the lowest height present in HEIGHT_INDEX for `type_id`,
/// or None if no entries exist. Mirror of `tip(type_id)`. Single
/// forward range query, O(log N). For `type_id == 101` routes to
/// BEST_CHAIN's lowest entry.
fn min_height_present(
    &self,
    type_id: u8,
) -> Result<Option<u32>, Self::Error>;
```

- [ ] **Step 2: Add preconditions** under the "Preconditions" section:

```markdown
- **`prune_below_height`**: `horizon > 0`. `type_ids` is non-empty and
  does not contain 101 (the store rejects header pruning).
- **`min_height_present`**: no preconditions beyond valid `type_id`.
```

- [ ] **Step 3: Add postconditions** under the "Postconditions" section:

```markdown
- **`prune_below_height`**: For every `(t, h, id)` previously in HEIGHT_INDEX
  with `t ∈ type_ids` and `h < horizon`: PRIMARY row removed, HEIGHT_INDEX
  row removed, subsequent `get(t, &id)` returns `None`. For every
  `(t, h, id)` with `h >= horizon` or `t ∉ type_ids`: untouched. On Err:
  no writes (atomic).
- **`min_height_present`**: For `type_id != 101`, returns the smallest
  `h` with `(type_id, h)` in HEIGHT_INDEX, or `None` if no entry. For
  `type_id == 101`, returns BEST_CHAIN's lowest height entry, or `None`
  if empty.
```

- [ ] **Step 4: Add an invariant** under "Invariants":

```markdown
- `prune_below_height` is idempotent: re-running with the same `(horizon,
  type_ids)` produces no additional writes and returns 0.
```

- [ ] **Step 5: Commit**

```bash
git add facts/store.md
git commit -m "docs(facts/store): contract for prune_below_height + min_height_present"
```

### Task 2: Update `facts/chain.md` with `voting_length()` accessor

**Files:** Modify `facts/chain.md`

- [ ] **Step 1: Add the accessor** in the "Phase 6: Soft-Fork Voting" section, after the `is_epoch_boundary` block:

```markdown
### `voting_length() -> u32`
- **Postcondition**: Returns the voting epoch length for this network.
  Mainnet: 1024. Testnet: 128.
- **Pure accessor** over the chain's `VotingConfig`. Used by sync to
  align the `blocks_to_keep` prune horizon to voting-epoch boundaries
  (so the current epoch's extensions stay intact for parameter
  recomputation).
```

- [ ] **Step 2: Commit**

```bash
git add facts/chain.md
git commit -m "docs(facts/chain): voting_length() accessor for sync's prune horizon"
```

### Task 3: Update `facts/sync.md` with pruning integration

**Files:** Modify `facts/sync.md`

- [ ] **Step 1: Find the flush_pair section** and add a "Pruning at flush time" subsection:

```markdown
### Pruning at flush time

When `blocks_to_keep >= 0`, the flush_pair's modifier-store write
transaction also prunes non-header section bodies (type IDs 102, 104,
108) at heights below the retention horizon:

```rust
horizon = compute_prune_horizon(validated_height, blocks_to_keep, voting_length);
if horizon > 0 {
    store.prune_below_height(horizon, &[102, 104, 108])?;
}
```

Horizon is computed against `validated_height` (state.redb's
just-flushed height), not the chain tip — this guarantees crash
recovery can always re-apply `(validated_height, tip]` from on-disk
bodies. Voting-epoch alignment pulls the horizon back to the start of
the current voting epoch so the current epoch's extensions stay
intact for `recompute_active_parameters_from_storage`.

Pruning failure is logged at WARN and does not abort the flush — the
next flush retries with the same (idempotent) horizon.
```

- [ ] **Step 2: Add the flush dial cap subsection:**

```markdown
### Flush dial cap

When `blocks_to_keep >= 0`, the flush dial's min/max block guardrails
are capped at `blocks_to_keep`:

```
effective_min = min(flush_min_blocks_config, blocks_to_keep)
effective_max = min(flush_max_blocks_config, blocks_to_keep)
```

This ensures the `validated_height → tip` gap never exceeds
`blocks_to_keep`, so pruning never deletes bodies still needed for
crash recovery. For `blocks_to_keep = 0`: effective_max = 0 → flush
on every block (Durability::Immediate per block).
```

- [ ] **Step 3: Add the startup WARN subsection:**

```markdown
### Startup WARN (lazy migration)

On startup, if `blocks_to_keep > 0` and the on-disk archive contains
bodies older than what would be retained going forward, sync emits a
one-shot WARN pointing at `sharpen prune` for reclamation:

```rust
if blocks_to_keep > 0 {
    if let Some(actual_min) = store.min_height_present(102)? {
        let configured_horizon = compute_prune_horizon(
            validated_height, blocks_to_keep, voting_length);
        if actual_min < configured_horizon {
            warn!("{N} historical blocks reclaimable; run `sharpen prune`");
        }
    }
}
```

Self-correcting: after the first prune sweep, `actual_min` advances
to `configured_horizon` and the WARN stops firing. No persistent
sentinel — derived from store state per
`[[feedback_derive_from_state]]`.
```

- [ ] **Step 4: Commit**

```bash
git add facts/sync.md
git commit -m "docs(facts/sync): blocks_to_keep pruning integration"
```

### Task 4: Write per-crate dispatch prompts

**Files:** Create three new prompt files.

- [ ] **Step 1: Create `prompts/blocks-to-keep-store.md`** with this content:

````markdown
# enr-store: implement blocks_to_keep pruning support

## Context

Design doc: `docs/design/2026-05-22-blocks-to-keep.md`.
Contract: `facts/store.md` (the contract section now declares
`prune_below_height` and `min_height_present`; the main session
landed those declarations in advance of this prompt).

These two methods support the new pruning feature: sync will call
them inside the existing flush_pair to delete non-header section
bodies older than a horizon computed from `blocks_to_keep`.

## What to implement

Add to the `ModifierStore` trait and `RedbModifierStore` impl:

```rust
fn prune_below_height(
    &self,
    horizon: u32,
    type_ids: &[u8],
) -> Result<usize, Self::Error>;

fn min_height_present(
    &self,
    type_id: u8,
) -> Result<Option<u32>, Self::Error>;
```

Full preconditions / postconditions / invariants are in `../facts/store.md`.

Key implementation notes:

- `prune_below_height` MUST reject `type_ids` containing 101 (return
  Err; headers are never pruned). Debug-assert + release-error.
- `prune_below_height` uses a HEIGHT_INDEX range query `(t, 0)..(t,
  horizon)` per type to find rows; for each, delete PRIMARY and
  HEIGHT_INDEX in the same write transaction. Atomic by redb's tx
  guarantee.
- `prune_below_height` is idempotent — re-running with the same
  horizon should produce 0 deletes (rows already gone).
- `min_height_present(101)` routes to BEST_CHAIN's lowest entry
  (mirrors how `tip(101)` routes to `best_header_tip`).
- `min_height_present(non-101)` is a forward range query on
  HEIGHT_INDEX for that type; first entry's height. O(log N).

## Tests

Add tests covering:

- `prune_below_height` removes matching rows, leaves higher-height
  rows untouched, leaves other type_ids untouched
- `prune_below_height` is idempotent (run twice, second returns 0)
- `prune_below_height` rejects type_ids containing 101
- `prune_below_height` on empty store returns 0 with no error
- `min_height_present` on empty store returns None
- `min_height_present` on single-entry store returns that height
- `min_height_present` after a prune sweep returns the new minimum
- `min_height_present(101)` returns BEST_CHAIN's lowest height when
  populated

## Scope boundary

You are in `store/`. Do NOT edit:

- `../facts/store.md` (main session owns contract docs)
- Any sibling crate (`chain/`, `sync/`, etc.)

You MAY update `store/README.md` if it currently lists methods you've
added.

## Verification

- `cargo test --package enr-store` passes (all existing tests + new)
- `cargo clippy --package enr-store -- -D warnings` passes
- `grep -n "prune_below_height\|min_height_present" src/` shows
  declarations and uses match the contract

## Return

Bullets covering:
- Files touched (path + brief description)
- Test count added vs. existing baseline
- Anything surprising while implementing (e.g. existing redb helper
  that simplified the range query, an unexpected interaction with
  the chain_meta table, etc.)
- Confirmation that `cargo test` and `cargo clippy` pass
````

- [ ] **Step 2: Create `prompts/blocks-to-keep-chain.md`** with this content:

````markdown
# enr-chain: expose voting_length() accessor

## Context

Design doc: `docs/design/2026-05-22-blocks-to-keep.md`.
Contract: `facts/chain.md` (the `voting_length()` declaration was
added by the main session in advance of this prompt).

Sync needs the voting epoch length to compute the prune horizon for
`blocks_to_keep`-driven pruning. Pulling it via a pure accessor on
`HeaderChain` keeps voting-epoch knowledge out of sync's call site.

## What to implement

Add a single public accessor:

```rust
impl HeaderChain {
    /// Returns the voting epoch length for this network.
    /// Mainnet: 1024. Testnet: 128.
    pub fn voting_length(&self) -> u32 {
        self.voting_config.voting_length
    }
}
```

That's the entire change. No new internal state, no logic, just
expose the existing `VotingConfig.voting_length` field through a
named accessor.

## Tests

Add a test in the chain crate verifying:

- `HeaderChain::new` with mainnet config returns `voting_length() == 1024`
- `HeaderChain::new` with testnet config returns `voting_length() == 128`

## Scope boundary

You are in `chain/`. Do NOT edit:

- `../facts/chain.md` (main session owns contract docs)
- Any sibling crate

## Verification

- `cargo test --package enr-chain` passes
- `cargo clippy --package enr-chain -- -D warnings` passes

## Return

Bullets: file touched, test added, and confirmation tests + clippy pass.
````

- [ ] **Step 3: Create `prompts/blocks-to-keep-sync.md`** with this content:

````markdown
# enr-sync: implement blocks_to_keep pruning + flush dial cap + startup WARN

## Context

Design doc: `docs/design/2026-05-22-blocks-to-keep.md`.
Contract: `facts/sync.md` (the main session has landed the pruning,
flush dial cap, and startup WARN sections in advance of this prompt).

Store's new API (`prune_below_height`, `min_height_present`) is
landed. Chain's new `voting_length()` accessor is landed. Wire them
into the existing flush_pair + flush dial + startup paths.

## What to implement

### 1. `compute_prune_horizon` helper

In sync's flush module (or a new small module if cleaner):

```rust
fn compute_prune_horizon(flushed: u32, keep: u32, voting_length: u32) -> u32 {
    let raw = flushed.saturating_sub(keep).saturating_add(1);
    if raw > voting_length {
        let epoch_start = (raw / voting_length) * voting_length;
        raw.min(epoch_start)
    } else {
        raw
    }
}
```

Pure function; unit-testable.

### 2. Prune call inside flush_pair

Locate the existing flush_pair logic (modifier-store side, after
state.redb's Immediate flush commits, inside the modifier-store
write transaction). Add:

```rust
if blocks_to_keep >= 0 {
    let horizon = compute_prune_horizon(
        validated_height,
        blocks_to_keep as u32,
        chain.voting_length(),
    );
    if horizon > 0 {
        match store.prune_below_height(horizon, &[102, 104, 108]) {
            Ok(n) => debug!("pruned {n} modifier rows below height {horizon}"),
            Err(e) => warn!("prune at horizon {horizon} failed: {e}"),
        }
    }
}
```

Pruning failure is non-fatal — log and continue. Next flush retries.

### 3. Flush dial cap

In the flush dial's config-construction path:

```rust
let effective_min = match blocks_to_keep {
    n if n < 0 => flush_min_blocks_config,
    n => (n as u32).min(flush_min_blocks_config),
};
let effective_max = match blocks_to_keep {
    n if n < 0 => flush_max_blocks_config,
    n => (n as u32).min(flush_max_blocks_config),
};
```

Use `effective_min` / `effective_max` in place of the unmodified
config values when the dial decides whether to flush.

For `blocks_to_keep = 0`: effective_max = 0, meaning every block
triggers a flush.

### 4. Startup WARN

After store open and chain reconstruction (sync's startup path):

```rust
if blocks_to_keep > 0 {
    if let Some(actual_min) = store.min_height_present(102)
        .ok()
        .flatten()
    {
        let configured_horizon = compute_prune_horizon(
            validated_height,
            blocks_to_keep as u32,
            chain.voting_length(),
        );
        if actual_min < configured_horizon {
            let reclaimable = configured_horizon - actual_min;
            warn!(
                "{reclaimable} historical blocks reclaimable; \
                 run `sharpen prune --keep={blocks_to_keep}` to free disk"
            );
        }
    }
}
```

Single-line WARN. No retry, no persistence — derived state per
`[[feedback_derive_from_state]]`.

## Tests

- Unit tests for `compute_prune_horizon`:
  - `flushed < voting_length` (no alignment)
  - `flushed > voting_length`, raw inside an epoch (pulled back to
    epoch start)
  - `flushed > voting_length`, raw at exact epoch boundary (no change)
  - `keep > flushed` (saturating_sub returns 0)
  - `keep = 0` (raw = flushed + 1)
- Unit tests for flush dial cap with `blocks_to_keep` values -1, 0,
  small (e.g. 5), large (1e6).
- Integration test (with fixture store + chain): set `blocks_to_keep
  = 50`, apply 200 blocks, trigger flush, verify
  `min_height_present(102)` advances accordingly.
- Integration test: configure `blocks_to_keep` such that the
  raw horizon lands mid-epoch on a known voting epoch, verify the
  current epoch's bodies (including extensions) are retained.
- Integration test for startup WARN: pre-populate fixture store with
  bodies older than `configured_horizon`, run startup path, verify
  WARN appears in captured logs (use `tracing-test` with
  `no-env-filter` per `[[feedback_tracing_test_no_env_filter]]`).

## Scope boundary

You are in `sync/`. Do NOT edit:

- `../facts/sync.md` (main session owns contract docs)
- `../src/main.rs` (main session wires `blocks_to_keep` through to sync)
- Sibling crates

You MAY add helpers or modules within `sync/src/`.

## Verification

- `cargo test --package enr-sync` passes
- `cargo clippy --package enr-sync -- -D warnings` passes
- New tests cover all four behaviors (horizon math, dial cap, prune
  at flush, startup WARN)

## Return

Bullets covering:
- Files touched
- Where `compute_prune_horizon` lives (module path)
- How the prune call integrates with the existing flush_pair (which
  function, which transaction)
- How the flush dial cap is wired (where the effective values flow)
- Test count added
- Anything surprising about the existing flush_pair architecture that
  required a non-obvious wiring choice
````

- [ ] **Step 4: Confirm the three files exist on disk; no commit.**

`prompts/` is in `.gitignore` per project convention
(`[[feedback_prompts_gitignored]]`) — per-crate dispatch prompts are
disposable runtime artifacts written by main session, dispatched, and
deleted after the work is done. The canonical record of their
contents is embedded in this plan file (above) — recreate from there
if any are lost.

```bash
ls -la prompts/blocks-to-keep-store.md prompts/blocks-to-keep-chain.md prompts/blocks-to-keep-sync.md
```

Expected: all three files present.

### Task 5: Dispatch store + chain prompts (parallel)

These two crates have no dependency on each other and can be dispatched in parallel.

- [ ] **Step 1: Dispatch `prompts/blocks-to-keep-store.md`** via the dispatching-prompts skill.

The skill handles: opening a new kitty window, running `ac` (the launcher), `cd store/`, sending the standard boilerplate ("read OVERRIDES.md, then CLAUDE.md, then SETTINGS.md, then the ergo-node-development skill, then MEMORY.md; YOU ARE A DISPATCHED SESSION; here is your work:"), then the prompt file path, then `$'\r'`.

- [ ] **Step 2: Dispatch `prompts/blocks-to-keep-chain.md`** in parallel (separate kitty window, separate `ac` invocation, `cd chain/`).

- [ ] **Step 3: Wait for completion reports** from both sessions via the coordination channel.

Expected return (per the prompt's "Return" section): file list, test count, `cargo test` + `cargo clippy` confirmation.

- [ ] **Step 4: Verify integrated state** from main session:

```bash
cargo test --package enr-store --package enr-chain
cargo clippy --workspace -- -D warnings 2>&1 | head -40
```

Expected: both packages' tests pass; clippy clean.

- [ ] **Step 5: Commit any incidental main-session-visible changes** (e.g. if the dispatched session updated `Cargo.lock`, that gets committed by main session).

```bash
git status
# If only Cargo.lock changed:
git add Cargo.lock
git commit -m "chore: update Cargo.lock after blocks_to_keep store + chain"
```

### Task 6: Dispatch sync prompt

Sync depends on store + chain being complete, so this runs after Task 5.

- [ ] **Step 1: Dispatch `prompts/blocks-to-keep-sync.md`** via the dispatching-prompts skill (new kitty window, `cd sync/`).

- [ ] **Step 2: Wait for completion report.**

- [ ] **Step 3: Verify integrated state:**

```bash
cargo test --package enr-sync
cargo clippy --workspace -- -D warnings 2>&1 | head -40
```

- [ ] **Step 4: Workspace-wide build:**

```bash
cargo build --workspace
```

Expected: clean build. If main crate doesn't compile because sync's new public surface needs additional wiring at the call site, that's Task 7.

### Task 7: Main session wiring

Wire `blocks_to_keep` from config through to sync's pruning logic. Already wired into the handshake; this adds the second consumer.

**Files:** Modify `src/main.rs`

- [ ] **Step 1: Find the existing `blocks_to_keep` read site** (per the spec, around `src/main.rs:769-770, 887, 924, 1280-1284, 1530, 1540`). Confirm where the value is currently passed to handshake construction.

- [ ] **Step 2: Find where sync is constructed.** Pass `blocks_to_keep` to whichever sync constructor / config struct the dispatched sync session created or modified for this purpose.

- [ ] **Step 3: Build the workspace:**

```bash
cargo build --workspace
```

Expected: clean build.

- [ ] **Step 4: Run the full test suite:**

```bash
cargo test --workspace
```

Expected: all tests pass, including the new ones from each dispatched session.

- [ ] **Step 5: Commit:**

```bash
git add src/main.rs
git commit -m "feat(main): wire blocks_to_keep config to sync pruning"
```

### Task 8: Integration test on laptop validator

Optional but strongly recommended given that this touches durability-sensitive code paths. The laptop validator (per `[[project_v0_6_3_release]]`) is the canonical live test bed.

- [ ] **Step 1: Build the release binary:**

```bash
cargo build --release
```

- [ ] **Step 2: Pre-test snapshot of laptop state:**

```bash
ssh laptop "du -sh /var/lib/ergo-node/data/modifiers.redb /var/lib/ergo-node/data/state.redb && systemctl status ergo-node | head -5"
```

Record the modifiers.redb size for the before-pruning baseline.

- [ ] **Step 3: Deploy with `blocks_to_keep = -1` (no pruning, baseline behavior):**

Per `[[feedback_deploy_binary_race]]`: `rm` before `cp`. Per `[[feedback_no_bulldoze_live_config]]`: patch config lines, don't `cp` over the whole config.

```bash
ssh laptop "sudo systemctl stop ergo-node && sudo rm /usr/bin/ergo-node && sudo cp /tmp/ergo-node /usr/bin/ && sudo systemctl start ergo-node"
```

Watch journalctl for clean start. No new WARN expected because `blocks_to_keep = -1` skips the migration check.

- [ ] **Step 4: Switch config to `blocks_to_keep = 100000` and restart:**

```bash
ssh laptop "sudo sed -i 's/^blocks_to_keep = -1/blocks_to_keep = 100000/' /etc/ergo-node/ergo.toml && sudo systemctl restart ergo-node"
```

Watch journalctl. Expected: one-shot WARN "1689xxxx historical blocks reclaimable; run `sharpen prune --keep=100000` to free disk". The exact number depends on the chain tip at deploy time (currently ~1.789M minus 100000 = ~1.689M).

- [ ] **Step 5: Let it run for an hour at-tip and verify:**

```bash
ssh laptop "du -sh /var/lib/ergo-node/data/modifiers.redb"
ssh laptop "curl -s http://localhost:9053/info | jq .fullHeight"
```

Expected:
- modifiers.redb size SAME as baseline (only newly-pruned bodies removed; the historical archive is left untouched per lazy migration).
- New blocks pruning into the rolling window — verifiable via the debug log line "pruned N modifier rows below height H".

- [ ] **Step 6: Restart and verify WARN behavior:**

```bash
ssh laptop "sudo systemctl restart ergo-node"
ssh laptop "journalctl -u ergo-node -n 200 | grep reclaimable"
```

WARN should fire once more (the archive is still > configured_horizon since no `sharpen prune` has run).

- [ ] **Step 7: Restore baseline config (optional, for clean test cleanup):**

```bash
ssh laptop "sudo sed -i 's/^blocks_to_keep = 100000/blocks_to_keep = -1/' /etc/ergo-node/ergo.toml && sudo systemctl restart ergo-node"
```

### Task 9: Refresh brainstorm prompt with decided design

Per the original prompt's end-of-shift handoff instruction: rewrite `prompts/blocks-to-keep-brainstorm.md` with the decided design so the next session has clear orientation.

**Files:** Modify `prompts/blocks-to-keep-brainstorm.md`

- [ ] **Step 1: Replace the file contents** with a pointer to the spec + plan + summary of decisions:

```markdown
# blocks_to_keep storage pruning — handoff (post-brainstorm)

The brainstorm converged on 2026-05-22. Implementation plan + spec are:

- Design: `docs/design/2026-05-22-blocks-to-keep.md`
- Plan: `docs/design/2026-05-22-blocks-to-keep-plan.md`
- Per-crate prompts: `prompts/blocks-to-keep-{store,chain,sync}.md`

## Summary of decisions

[short bullet list mirroring the spec's "Decisions" table]

## Status

[update based on what's been landed at handoff time — e.g. "Tasks
1-4 landed; Task 5 dispatch in flight; Task 6+ pending"]
```

- [ ] **Step 2: Commit:**

```bash
git add prompts/blocks-to-keep-brainstorm.md
git commit -m "docs(prompts): refresh blocks-to-keep handoff with decided design"
```

---

## Self-Review

**Spec coverage:** Every section of `docs/design/2026-05-22-blocks-to-keep.md` maps to at least one task here:
- Architecture / Components touched → Tasks 1-7 distribute the changes per the spec's component table
- Store contract addition → Task 1 (contract) + Task 4 / Task 5 (impl)
- Sync wiring → Task 3 (contract) + Task 4 / Task 6 (impl) + Task 7 (main integration)
- Chain addition → Task 2 (contract) + Task 4 / Task 5 (impl)
- Migration / startup WARN → covered in Task 4's sync prompt content + Task 8 verifies live
- `sharpen` subcommand → explicitly out of scope (spec confirms), no task
- Re-fetch behavior → unchanged, no task
- Light mode interaction → silently ignored, no task (verification implicit in test coverage)
- Error handling → addressed in each per-crate prompt's "what to implement" guidance
- Testing → embedded in per-crate prompts + Task 8 integration

**Placeholder scan:** Each task step has actual content. The per-crate prompts in Task 4 use the existing project convention of "read the contract for full preconditions/postconditions" rather than restating them — that's not a placeholder, it's intentional DRY.

**Type consistency:** Method signatures match across spec / contract / prompts. `prune_below_height`, `min_height_present`, `voting_length` are used identically everywhere they appear.

**Open items deliberately left to dispatched-session judgment:**
- Exact module organization within each crate (where to put `compute_prune_horizon`, etc.)
- Existing flush_pair architecture details (the dispatched sync session will read its own code and decide where the prune call fits cleanly)
- Test fixture utilities (the dispatched sessions reuse whatever fixture infrastructure each crate already has)

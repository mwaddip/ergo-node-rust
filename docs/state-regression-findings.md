# State Regression Findings — 2026-04-15

## Summary

State.redb is intact. The bug is in `src/main.rs` at the validator-resume
scan: it only walks DOWN from the persisted hint, so when the actual
state lives at a height ABOVE the hint, the scan finds nothing,
resolves to `height=0`, and then overwrites the hint with 0 — amplifying
the corruption on the next startup.

No resync is needed. Changing the scan direction (and, ideally, the way
state-vs-hint gets out of sync in the first place) is sufficient.

## Reproduction

Opened a copy of `/var/lib/ergo-node/data/state.redb` (11 GB, btrfs
sandbox snapshot) via `RedbAVLStorage::open` and constructed a
`PersistentBatchAVLProver`. Inspector: `examples/state_inspect.rs`.

```
NODES_TABLE entries:   18,292,093
UNDO_TABLE entries:    201
version_chain:         201 entries, LSNs 1,307,679 .. 1,307,879
META_CURRENT_VERSION:  4b1254ad20ec0243c7d3bc9b0f3a8c9399b926b23d3f8bfec2ee02e72986f99f1a
prover.digest():       4b1254ad20ec0243c7d3bc9b0f3a8c9399b926b23d3f8bfec2ee02e72986f99f1a
```

Cross-referenced the prover digest against mainnet headers in
`modifiers.redb` (copy, not live):

```
header[1,307,866].state_root != prover.digest()
header[1,307,878].state_root == prover.digest()   ✓ MATCH
header[1,307,879].state_root != prover.digest()
```

The storage is 100 % self-consistent: META metadata, NODES_TABLE,
UNDO_TABLE, version chain, and the prover's computed root digest all
agree. The state corresponds to the UTXO set after block 1,307,878 was
applied.

LSN 1,307,879 = 1 + block_height because the genesis bootstrap path
stamps LSN=1 before applying block 1.

## The bug

`src/main.rs:1289-1307` — the slow-path scan on UTXO resume:

```rust
if height == 0 && hint != Some(0) {
    scanned = true;
    let scan_from = hint.unwrap_or(chain_height).min(chain_height);
    for h in (0..=scan_from).rev() {
        ...
    }
}
```

`scan_from` is clamped to the hint. The iteration is
`(0..=scan_from).rev()` — strictly downward. If the actual state
height is ABOVE the hint, no iteration of this loop ever sees the
right header.

Concretely: hint was `Some(1,307,866)`, state is at 1,307,878. Scan
walks `1,307,866, 1,307,865, ..., 0`, never reaches 1,307,878, finds
nothing, leaves `height` at its initialization value of 0.

Then `src/main.rs:1310` persists that resolved 0 back to the store:

```rust
ergo_node_rust::write_validator_height(&store, height);
```

On the next startup, `hint=Some(0)` short-circuits the slow-path guard
(`hint != Some(0)` is false), so scan doesn't even run. `height`
stays 0, bootstrap thinks state is empty, the validator tries to
apply block 1 against a tree that already reflects 1,307,878, and
fails immediately on a missing genesis-box key.

## Why state ended up above the hint

Two separate ordering issues in `sync/src/state.rs` let state get
durable AHEAD of the persisted hint:

**1. In-loop flush without hint update (line 716-724).**
During a sweep, `v.flush()` runs every 100 blocks, promoting all
Durability::None writes to durable. But the hint is only written in
the post-loop branch at line 736. If the process is killed after an
in-loop flush but before the sweep completes, state is durable at the
last flush boundary while hint still reflects the last completed
sweep's `validated_to`.

**2. Post-sweep ordering: hint write BEFORE final drain/flush
(lines 733-773).**
```
if validated_to > state_applied_height { write_hint(validated_to) }
drain_eval_results()       // may trigger rollback(), which is durable
v.flush()                  // unconditional
```
If `drain_eval_results` triggers `handle_eval_failure`, it calls
`reset_to` which does `prover.rollback()` — a Durability::Immediate
commit. State becomes durable at `rollback_to`, but the hint was
already written as the higher `validated_to`. This flavor is hint
AHEAD of state (the scan still finds it going down), so it's not the
current symptom — but it's a related integrity gap.

**3. Silent failures in hint write (bridge.rs:182-191).**
`write_validator_height` only logs on failure:
```rust
if let Err(e) = store.put(...) {
    tracing::warn!(height, "failed to persist validator_height: {e}");
}
```
`set_validator_height` in bridge.rs:254-261 awaits via `.await.ok()`,
swallowing join errors. If the redb put ever fails transiently, the
hint silently stays at its previous value while state advances.

The exact trigger for this specific instance isn't provable from the
artifacts alone (the 7-hour run ended in a hardware crash, logs don't
record which path fired). All three are real bugs; fix them together.

## Recovery

State is fully usable. Three non-exclusive options, cheapest first:

**Option A — Manually fix the hint, restart.**
Write `1,307,878` into `modifiers.redb` under the validator-height
key. Node resumes from block 1,307,879, sync continues. No code
changes required. Fastest path to a running node.

**Option B — Fix the scan first, then restart.**
Change `src/main.rs:1289-1307` to scan BOTH directions, or just
scan the full chain from tip. At 1.76 M headers with in-memory
chain access this is single-digit seconds even without bounds. No
hint fix needed — the next startup resolves the height from the
digest.

**Option C — Close the hint/state divergence at the source.**
Either persist the hint INSIDE `state.redb`'s META table atomic
with the update, or arrange the sweep so hint and state are always
updated in the same durable commit. Proper fix; bigger change; needs
state-submodule coordination.

Recommended: B immediately (makes the node self-healing on stale
hints). Then C as a follow-up (makes stale hints impossible in the
first place).

## Smallest fix that prevents recurrence

In `src/main.rs:1289-1307`, replace the downward-only scan with a
full-chain scan. Rough shape:

```rust
if height == 0 {
    scanned = true;
    // Genesis check first (cheap).
    let genesis_root: [u8; 33] = genesis_digest.into();
    if prover_digest_arr == genesis_root {
        height = 0;
    } else {
        // Scan from tip down. Digests are unique per block, so the
        // first match is the correct height.
        for h in (1..=chain_height).rev() {
            if let Some(header) = chain_guard.header_at(h) {
                let header_root: [u8; 33] = header.state_root.into();
                if prover_digest_arr == header_root {
                    height = h;
                    break;
                }
            }
        }
    }
}
```

Remove the `hint != Some(0)` guard — it's the amplification mechanism
that turns a one-time scan miss into a permanent 0-hint. The hint is
still useful as a fast path, but as an OPTIMIZATION, not a
CONSTRAINT on the scan's search space.

This alone makes the node self-healing. The scan is O(chain_height)
worst case, but headers are already in memory and the compare is a
32-byte memcmp, so even a full mainnet scan is sub-second.

## Related integrity work

Follow-ups (separate work, not required for recovery):

- **Fix sweep ordering**: do the final drain/flush before the hint
  write, so the hint reflects the actually-durable state after any
  rollback path.
- **Collapse hint + state into one durable commit**: either move the
  hint inside state.redb's META table, or stamp a block-height key
  as part of `update()`.
- **Surface hint-write failures**: change bridge.rs to propagate
  errors from `write_validator_height` / `set_validator_height`
  instead of logging and continuing. A failed hint write should
  either retry or terminate the sweep, not pretend to succeed.
- **SIGTERM handler should explicitly flush**: `src/main.rs:2195-2220`
  drops P2P and relies on the sync task reaching its end-of-sweep
  flush during `SHUTDOWN_GRACE`. Explicit `validator.flush()` in the
  handler is cheaper insurance than the current race.

## Artifacts

- `examples/state_inspect.rs` — read-only inspector. Usage:
  `cargo run --example state_inspect -- <state.redb> [modifiers.redb] [target_height]`
- Sandbox: `/mnt/state-sandbox/pristine/state.redb` (from loopback
  btrfs img at `/home/mwaddip/ergo-state-sandbox.img`). Read-only
  snapshot at `/mnt/state-sandbox/pristine-ro`. To experiment:
  `btrfs subvolume snapshot /mnt/state-sandbox/pristine-ro work-N`.
- Modifiers copy: `/home/mwaddip/modifiers-inspection.redb` (outside
  sandbox; 19 GB).

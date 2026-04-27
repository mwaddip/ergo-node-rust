# Session 23 — Pipeline height gap on restart

> **Archived session handoff** — pipeline gap fixes landed across
> v0.3.x and v0.4.x (synced-drain, SVH persist, at-tip handshake).

Load CLAUDE.md, ~/projects/OVERRIDES.md, ergo-node-development skill,
and memory. Read `project_parallel_validation_todo.md` from memory.

## Context

Session 22b added a parallel validation pipeline with two watermarks:
- `state_applied_height` — AVL state advanced (sequential, fast)
- `script_verified_height` — scripts confirmed (background, slow)

On restart, the node resumes the sweep from `script_verified_height + 1`.
But the UTXO state (persistent AVL prover) is at `state_applied_height`,
which may be ahead.

## Observable problem

After v0.2.0 deployed and the first validation sweep completed up to
942,665, the node restarted. Now it's stuck:

```
ERROR ergo_sync::state: apply_state failed height=942666
      error=unexpected block height: expected 942664, got 942666
```

The UTXO state expects block 942,664 (it's at 942,663), but the sweep
starts from 942,666 (script_verified_height was 942,665). Blocks
942,664 and 942,665 were state-applied but never script-verified before
shutdown. Now:
- The state can't accept 942,666 (it needs 942,664 first)
- The sweep won't go back to 942,664 (it thinks those are done)

## What to investigate

1. How does the sweep determine its start height on restart?
   Look at `sync/src/state.rs` for the `VALIDATION SWEEP STARTED` log.

2. How does the persistent prover report its current height?
   The prover digest is matched against header state_roots to find
   the actual state height (see `src/main.rs` around line 1200).

3. The gap: sweep starts from `script_verified_height + 1` but state
   is at `state_applied_height`. If state_applied > script_verified,
   the sweep skips blocks the state hasn't processed yet.

## Expected fix

On restart, the sweep start height should be
`min(state_applied_height, script_verified_height) + 1` — or more
precisely, the prover's actual height + 1. The prover's digest tells
us where the state actually is. The sweep should resume from there,
not from the script-verified watermark.

Alternatively: on startup, if there's a gap between prover height and
script_verified_height, roll back the prover to match, or re-apply
the missing blocks.

## Files

- `sync/src/state.rs` — sweep logic, watermarks
- `src/main.rs:1200+` — validator startup, prover height detection
- `validation/src/lib.rs` — `BlockValidator` trait, `apply_state`

## Verification

After the fix:
- Restart the node (state is at 942,663)
- The sweep should resume from 942,664 (prover height + 1)
- No "unexpected block height" errors
- Validation continues to tip

## Notes

- The node is currently deployed with v0.2.0 tag (`ab2fb54`)
- sigma-rust is at rev `6c765c41`
- The parallel pipeline design spec is at
  `docs/superpowers/specs/2026-04-12-parallel-validation-pipeline-design.md`
- The `[patch]` override has been removed from Cargo.toml

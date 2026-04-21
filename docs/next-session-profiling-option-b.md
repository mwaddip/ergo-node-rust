# Next session: heap profile the running node, then decide on option B

## Goal

Get a usable heap profile of the mainnet node during active sync, figure out
where the 13-15 GB steady-state + 60 MB/1000-blocks peak creep actually lives,
then make an evidence-based decision on whether to do **option B** (chain
lazy-loading refactor).

## Context: what already shipped

Recent commits on `main` (pushed):

- `67a5410` — durable state flush + A-fix (bulk-deserialize header restore).
  Startup peak 37 GB → 1.95 GB, restore wall time -26%.
- `0b38dc8` — silenced per-modifier INFO log spam. Log volume 734 MB/hour → 0.3 MB/hour.

Submodule pointers (all on origin):

- `state` @ `eca6d44` — flush() trait method
- `p2p` @ `be3d518` — log-level downgrade
- `chain` @ `8d7a570` — unchanged
- `store` @ `26671e4` — unchanged

## What's still observable

During this session's monitor window (31-36k blocks of sync):

- Steady-state RSS: 12-14 GB, oscillating in ~1.5 GB band
- Peak RSS grew **steadily from 14.4 GB → 16.5 GB** at roughly
  **60 MB per 1000 blocks validated**
- Linear extrapolation to tip (464k blocks remaining): ~43 GB peak, close to
  the pre-A-fix 37 GB

Chain's `by_height: Vec<Header>` at 1.76M × 800B = ~1.4 GB accounts for part
of steady-state but cannot explain the per-block peak creep or the 8+ GB of
unknown heap during active sync.

## Task 1: heap profile

### Why three prior attempts failed

All under heaptrack. The tool works fine in user context (verified with a
trivial malloc test) but fails under the service's systemd sandboxing:

1. **Attempt 1** (PrivateTmp=yes, just wrap ExecStart): trace file 0 bytes.
   Root cause: default `KillMode=control-group` SIGTERMs the whole cgroup on
   service stop — heaptrack's interpreter+compressor pipeline gets killed
   before it can drain and write the zst.
2. **Attempt 2** (PrivateTmp=no + Environment=TMPDIR=/var/log/ergo-node):
   `ERROR: failed to open /tmp/heaptrack_fifo...: Read-only file system`.
   Root cause: `/usr/bin/heaptrack` line 243 hardcodes `pipe=/tmp/heaptrack_fifo$$`
   — TMPDIR is not respected. Under `ProtectSystem=strict`, /tmp is read-only
   without `PrivateTmp`.
3. **Attempt 3** (run heaptrack outside systemd via nohup as ergo-node user):
   background script exited at the stop-service step for unclear reasons
   before launching heaptrack. No diagnostics captured; service auto-recovered.

### Recommended fix (simplest path)

Patch the heaptrack script to move its FIFO to a writable path:

```bash
sudo sed -i.bak 's|pipe=/tmp/heaptrack_fifo|pipe=/var/log/ergo-node/heaptrack_fifo|' /usr/bin/heaptrack
```

Then install this drop-in:

```
# /etc/systemd/system/ergo-node-rust.service.d/heaptrack.conf
[Service]
KillMode=mixed
TimeoutStopSec=120
ExecStart=
ExecStart=/usr/bin/heaptrack --output /var/log/ergo-node/heaptrack.ergo /usr/bin/ergo-node-rust /etc/ergo-node/ergo.toml
```

`KillMode=mixed` sends SIGTERM only to MainPID (the heaptrack shell) and lets
its trap cleanup run before SIGKILL hits the rest of the cgroup after 120s.
Our node's SIGTERM handler flushes state, heaptrack's trap propagates and lets
the interpreter drain.

### Fallback: dhat

If heaptrack continues to misbehave under systemd, switch to
[`dhat`](https://docs.rs/dhat/) — a Rust heap profiler:

1. Add `dhat = "0.3"` to `Cargo.toml`
2. Add a `dhat-heap` cargo feature
3. In `src/main.rs`, under `#[cfg(feature = "dhat-heap")]`, set the global
   allocator to `dhat::Alloc` and initialize the profiler
4. Build with `--features dhat-heap`, deploy, run for 15-20 min
5. `dhat-heap.json` output loads in https://nnethercote.github.io/dh_view/dh_view.html

Trade-offs: needs code change + rebuild + feature flag. But works anywhere the
binary runs, no systemd sandbox issues, no FIFO drama.

### Profile window

Target 15-20 minutes of active sync (blocks being downloaded AND validated).
Key thing to capture: the steady-state allocation distribution + peak events.

### What to look for in the report

`heaptrack_print --print-leaks --print-histogram <trace>` is the starting
point. Top questions:

1. **What type holds the most bytes at peak?** If it's `Vec<Header>` in
   chain, option B is confirmed as the right fix. If it's something in
   `ergo_avltree_rust::batch_avl_prover` or similar, option B is the wrong
   target and we should refactor the prover's state retention instead.
2. **Is there a growing allocation with no matching free?** Leak candidate.
3. **How much is accounted for by redb's page cache?** We configured
   `cache_mb=4096` — should be ~4 GB file-backed.
4. **How much is sigma-rust?** Prior experience (per memory) suggests
   sigma-rust's allocator churn was unexpectedly large. Worth verifying here.

## Task 2: option B decision (after profiling lands)

**Option B** = refactor `enr-chain` so the full chain isn't kept in memory.
Per the chain session's prior pushback (see `feedback_verify_before_dispatch.md`
in memory), this is a real refactor:

- Contract change in `facts/chain.md` (signatures for `header_at`,
  `headers_from`, `tip` must change from `Option<&Header>` to
  `Option<Arc<Header>>` or similar to return owned/lazy values)
- New `HeaderLoader` callback analogous to the existing `ExtensionLoader`
- Rework every internal caller that indexes `by_height` directly:
  `try_reorg`, `try_reorg_deep`, `install_from_nipopow_proof`, difficulty
  recalc (walks `use_last_epochs * epoch_length` headers per incoming header)
- Update the 2709-line chain test file
- Must be done in phases per OVERRIDES rule 4 (max 5 files per phase)

### Decide B is worth it IFF

The heap profile shows chain's `Vec<Header>` is a dominant or near-dominant
share of steady-state working set. If chain is ~10% of 13 GB and something
else is the 60%, B is the wrong lever.

### If B is confirmed

Process (per established multi-session workflow):

1. Update `facts/chain.md` with new contract (new HeaderLoader, new return
   types). Commit + push facts submodule pointer.
2. Write `prompts/chain-lazy-by-height.md` scoped to **one phase**, e.g.,
   "add HeaderLoader trait and wire genesis/tip access through it without
   removing Vec<Header> yet." Send to chain session (window_id=3 per
   `~/projects/ac/sessions`).
3. Verify integration after each phase lands. Run full test suite. Watch
   memory under the new regime.
4. Next phase: "replace `by_height: Vec<Header>` with an LRU cache + lazy
   load through HeaderLoader." Etc.

## Setup for the session

1. Read the boot load directive (CLAUDE.md, OVERRIDES.md, SETTINGS.md,
   facts/chain.md, ergo-node-development skill, and this file)
2. Check the live service state: `systemctl status ergo-node-rust.service`
   and `curl -s localhost:9052/info | jq .`
3. Check the monitor: should still be persistent from prior session if the
   box hasn't been rebooted. `TaskList` shows it. If gone, rearm from
   `.claude/watch-mem.sh` equivalent.
4. Read `.claude/heaptrack-v3.sh` and prior attempts if useful; ignore and
   start fresh if easier.

## Session output target

A markdown report at `docs/profiling-results.md` that says:

- What's actually in the 13-15 GB working set (top 10 alloc sites)
- Is the peak-creep a leak or bounded growth, and what causes it
- Recommendation: do option B (yes/no), and if yes, with what scope

Once that's delivered, we can either commit the profiling findings as
documentation and/or dispatch the contract change + first phase of B.

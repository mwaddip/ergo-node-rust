# Memory Budget — Interface Contract

Owner: **main crate** (`src/main.rs`). The crates that consume memory —
`enr-state`, `enr-store`, `ergo-sync` — already receive byte counts as
constructor parameters and need no API change for this. Derivation is
orchestration, which is main's job.

## Problem

The node sizes its caches from `cache_mb` in `ergo.toml` and is otherwise blind
to how much memory it may actually use. On 2026-08-13 a test node was run under
`systemd-run -p MemoryMax=1500M` and had **no idea** — it sized itself from a
TOML file, and had that number been wrong for the ceiling, the first indication
would have been the OOM killer. The kernel knew the answer exactly the whole
time.

Every value the operator is asked for is one the system can supply:

- the ceiling is in the cgroup, or in `MemTotal`
- the untunable floor is measurable at runtime and **grows with the chain** —
  `chainIndexBytes` was 9.7 MB at 201k headers and 148 MB at 1.85M, so any
  number written into a config at install time is correct on the day and drifts
  thereafter
- absolute defaults are wrong at the edges: `flush_heap_threshold_mb = 4096`
  sits above total RAM on any box small enough to care, so the trigger it
  governs is inert exactly where it is needed

## Rule: autosize unless told otherwise

**An option absent from the config is derived. An option present in the config
is obeyed, exactly, with no adjustment.**

⚠ **This requires the config fields to be `Option<T>`.** Today they are
`#[serde(default = "default_cache_mb")]`, so by the time `main` reads
`cache_mb = 1024` it cannot distinguish "the operator wrote 1024" from "serde
filled it in". Both must be representable, because the failure mode otherwise is
the worst kind: an operator sets a value, derivation silently overrides it, and
nothing says so.

The `default_cache_mb` and `default_flush_heap_threshold_mb` functions are
**deleted**, not repurposed as fallbacks: derivation cannot fail. If every
source is unreadable it assumes a small box (1 GB) rather than a large one, so
there is always a number and a second default would only be a second answer to
the same question.

Fields under this rule: `cache_mb`, `cache_store_pct`,
`flush_heap_threshold_mb`, `synced_cache_mb`,
`synced_flush_heap_threshold_mb`.

⚠ **`flush_max_blocks` and `flush_min_blocks` are NOT derived**, and keep their
existing defaults (100 / 5). They bound crash-recovery work in *blocks*, and
nothing measured relates a block count to a memory budget. They do affect
memory — fewer blocks between flushes means fewer accumulated dirty pages — so
a constrained profile may well want them lower, but picking that number without
a measurement would be exactly the invention this contract exists to avoid.

Setting **any** of them disables derivation for that field only; the rest are
still derived. Partial configuration is the normal case, not an error.

## The effective limit, in priority order

1. **`memory_budget_mb`** in `[node]`, if set — an explicit statement, used as
   given.
2. **The cgroup limit.** cgroup v2 `memory.max` (the literal string `max` means
   no limit), else cgroup v1 `memory.limit_in_bytes` (a huge sentinel means no
   limit). Read the node's own cgroup via `/proc/self/cgroup`.
3. **`MemTotal`** from `/proc/meminfo`.

Take `min()` of any that apply — a cgroup limit above physical RAM is not a
licence to use more than exists.

**The fraction taken differs by source, and deliberately:**

| Source | Fraction | Why |
|---|---|---|
| `memory_budget_mb` | 100% | The operator stated a budget. Obey it. |
| cgroup limit | high (~90%) | A cgroup limit is an explicit statement of intent — something already decided this node gets this much. |
| `MemTotal` | conservative (~50%) | Nothing said the node owns the box. Log that setting `MemoryMax` or `memory_budget_mb` would let it use more. |

## Budget against anon, not against the limit

⚠ **The cgroup counts page cache toward `memory.max`, but page cache is
reclaimable and anonymous memory is not.** On 2026-08-13 the test node showed
`MemoryPeak` at 1.36 GB against a 1.5 GB ceiling — 86%, alarming — while
`memory.stat` reported `anon` 578 MB and `file` 292 MB, and `memory.events` said
`max 0`: the limit had never once been touched. Nearly 300 MB of that "peak" was
cache the kernel drops on demand.

So the budget is spent against **anon**, and page cache is left to the kernel.
Sizing against `MemoryCurrent`/`MemoryPeak` over-provisions by roughly the cache
and is the single easiest mistake to make here — it was made during the
measurement that produced this contract.

## Derivation

```
limit      = effective limit × source fraction   (above)
floor      = chain_index_bytes                   (measured, grows with the chain)
           + baseline_anon                       (calibration, below)
available  = max(0, limit − floor)

cache_total            = available × CACHE_SHARE
flush_heap_threshold   = available × FLUSH_SHARE
synced_cache_total     = cache_total × SYNCED_RATIO
```

`cache_total` is then split by `cache_store_pct` exactly as today.

**Calibration constants, with provenance.** These come from mainnet at 1.85M
blocks on a 32-core box, `cache_mb = 1024` / `synced_cache_mb = 128`,
inline-only evaluation:

- `baseline_anon` ≈ **300 MB** — allocated-but-unattributed heap at tip
  (jemalloc `allocated` 703 MB against 402 MB of tracked components). Reproduces
  the "~200 MB of at-tip growth that cache does not account for" reported
  independently from the field. Excludes jemalloc's own overhead, a further
  ~260 MB between `allocated` and `resident`.
- Cold-sync peak reached **3.02 GB RSS** with a 1 GB cache budget, against
  950 MB at tip — so the cold-sync phase, not the at-tip phase, sets the
  ceiling.

⚠ **`CACHE_SHARE` and `FLUSH_SHARE` are not yet calibrated for a constrained
box.** The run that settles them is in progress; until it lands, these are the
one part of this contract resting on a fast machine's numbers. Do not present
derived values for a small box as measured.

## When it runs

⚠ **Derivation happens in two phases, because of an ordering constraint that is
not negotiable.** `modifiers.redb` must be opened before the header chain can be
restored — the chain is restored *from* it — and redb fixes its cache size at
open with no resize path. So `chain_index_bytes`, part of the floor, is not
knowable when the first cache must be sized.

- **Phase 1, before the store opens.** Floor is `baseline_anon` alone; the index
  is not yet knowable. Derives the **store cache** only. The budget is
  over-estimated by whatever the index turns out to be — bounded by ~150 MB at
  today's chain length, so single-digit percent of any sane budget.
- **Phase 2, after the header chain is restored.** `chain_index_bytes` is real.
  Derives the **state cache and the flush thresholds**, and logs the phase-1
  estimate against the measured floor so the error is visible rather than
  assumed small.
- **Once at the at-tip transition**, reusing the existing `synced_*` swap. The
  machinery already exists; derivation supplies its numbers.

Do not "fix" phase 1 by opening the store with a small bootstrap cache and
reopening later: a small cache makes the startup chain walk roughly 70× slower,
which is a known and measured cost.

**It does NOT re-derive periodically.** The floor moves over months, not
minutes, and a restart picks up the drift. A periodic re-derive trades a real
churn risk for an imperceptible gain.

## Observability

Derivation MUST be logged at startup at INFO, as a parseable journal event, with
every input and every output. Auto-sizing's failure mode is not being wrong — it
is being wrong **invisibly**, leaving the operator no line to point at. See
`facts/journal-events.md` § `memory_budget_derived`.

An operator must be able to answer "why is my cache this size" from the log
alone, without reading this document.

## Non-goals

- **Not a memory limiter.** Derivation sizes caches and flush thresholds; it
  does not enforce a ceiling, and cannot prevent an OOM caused by something it
  does not size.
- **Not adaptive under pressure.** The node does not shrink caches in response
  to memory pressure. That is a larger design and is not this.
- **Does not size `blocks_to_keep`.** Retention is the biggest lever on at-tip
  footprint and is a *policy* decision — archival versus pruned changes what the
  node is, not just what it costs.

# Contract: `ergo-fastsync` (addons/fastsync)

Version 1.0.0 — 2026-08-11

HTTP-based cold-sync accelerator. Downloads headers and block sections from
JVM peers' REST APIs and pushes them into the local node, typically 5-10x
faster than P2P for a cold start.

**Ownership**: main session, source included. `addons/*` has no per-crate
session or scaffolding — see `CLAUDE.md` § Per-Crate Dispatch.

## Position

```
JVM peers  --GET /blocks/chainSlice-->  fastsync  --POST /ingest/modifiers-->  local node
           --POST /blocks/headerIds-->            (localhost only)
```

fastsync is a separate binary, `exclude`d from the workspace with its own
lockfile, versioned independently (currently 0.1.7). The node spawns it when
`fastsync = true` and the gap exceeds `fastsync_threshold_blocks`.

## Lifecycle

Spawned by the node at startup when
`peer_tip - downloaded_height > fastsync_threshold_blocks` (default 25,000).

| Phase | Source | Target |
|---|---|---|
| 1 — headers | `GET /blocks/chainSlice` | `peer_tip - 25000` (handoff margin) |
| 2 — blocks | `POST /blocks/headerIds` | same |

It exits after phase 2; the node's P2P sync covers the remaining 25,000-block
handoff margin and everything after. **It is NOT short-lived**: phase 2 covers
~98% of the chain, so on a cold start fastsync is resident for the entire sync
— the whole window in which memory is scarce.

## Interface: fastsync → node

### `POST /ingest/modifiers`

Binary body, repeated records, no framing header and no version byte:

```
(type_id: u8, modifier_id: [u8; 32], data_len: u32 BE, data: [u8; data_len])*
```

Producer: `wire.rs::encode_ingest_body`.
Consumer: `api/src/handlers.rs::post_ingest_modifiers`.

Invariants:

- **Localhost only.** The node rejects non-loopback peers with 403. This is
  the only access control; there is no authentication.
- **The node buffers the entire body** (`axum::body::Bytes`) before parsing.
  Batch size therefore has a hard ceiling.
- Response is `{"accepted": u32}`; fastsync retries once after 1 s on failure,
  then aborts the run.
- The format carries **no version or magic bytes**. Both sides must change
  together; there is no negotiation and no way to detect a mismatch other
  than a parse failure — and a changed `type_id` would not even produce one.

### ⚠ Unresolved: body limit is undocumented and uncoordinated

`facts/api.md` documents `max_body_bytes = 2_097_152` as a config option and
asserts "request body size is bounded by `max_body_bytes`" as an invariant.
**Neither the config field nor any explicit limit exists in the code** — no
`max_body_bytes`, no `DefaultBodyLimit`, no `RequestBodyLimitLayer`. The
effective ceiling is axum's built-in default (2 MB in current versions), which
no operator can configure.

Meanwhile fastsync pushes batches of `BLOCK_BATCH_SIZE = 32` blocks' worth of
sections per request, with no awareness of any ceiling. Nothing coordinates
the two numbers. It works today, so real batches evidently fit; the dense end
of the chain is where they would stop.

Resolving this means picking one: implement the documented knob, or document
the real limit and bound `BLOCK_BATCH_SIZE` against it.

## Interface: peers → fastsync

### `GET /blocks/chainSlice?fromHeight=A&toHeight=B`

Returns header JSON. **The lower bound is EXCLUSIVE**: `(A, B]`. Verified
against a live endpoint — `fromHeight=0 toHeight=3` yields heights 1,2,3.

The name says otherwise, and treating it as inclusive drops one header per
chunk. On a cold start that header is genesis, after which nothing chains and
every subsequent header sits in the orphan buffer forever. This was live for
months and only surfaced when a wiped `modifiers.redb` finally exercised the
cold path — every prior run logged "headers already synced — skipping phase 1".

### `POST /blocks/headerIds`

Batches of `BLOCK_BATCH_SIZE = 32` IDs. Returns full blocks.

### `GET /info`

Peer chain state, used for tip discovery and health.

### `GET /peers/api-urls` (local node)

Mid-fetch peer discovery, rate-limited to once per `REFRESH_INTERVAL` (30 s).

## Memory characteristics

Measured 2026-08-11 during a genesis resync: **+416 MB/hr, ~40% of the
combined node+fastsync footprint, no plateau observed over 54 minutes.**

fastsync has **no memory observability** — no `/debug/memory` equivalent, no
jemalloc profiling feature. The figures below are code-read estimates, not
profiles.

| Structure | Estimate at 1.82M headers | Lifetime |
|---|---|---|
| `header_ids: Vec<(u32, String)>` (`pool.rs:281`) | ~167 MiB | entire run |
| `queue` clone (`pool.rs:546`) | ~153 MiB | all of phase 2 |
| out-of-order `buffer` (`pool.rs:278`) | ~41 MB at 33 peers | transient |

The first two are **simultaneously live**: `fetch_blocks` takes
`header_ids: &[(u32, String)]` by reference and clones every string into
`queue` up front, so both copies exist for the whole of phase 2.

IDs are stored as 64-char hex `String`s — 88 bytes of allocation to carry 32
bytes of data.

## Invariants

- fastsync never writes to the node's databases directly. All state changes go
  through `POST /ingest/modifiers` and are subject to the node's normal
  validation pipeline.
- fastsync is advisory and always optional. A node with `fastsync = false`, or
  with the binary absent from `PATH`, syncs correctly over P2P — only slower.
- Phase 1 must complete before phase 2 begins; block fetching is keyed on the
  header IDs phase 1 collected.
- Failure is non-fatal to the node: fastsync exiting non-zero leaves the node
  to continue over P2P.

## Does NOT own

- Validation of anything it fetches — the node validates every modifier it
  ingests, exactly as if it had arrived over P2P.
- Chain state, storage, or reorg handling.
- The 25,000-block handoff margin, which is P2P sync's job.

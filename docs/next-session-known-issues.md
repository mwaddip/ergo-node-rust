# Session 17 — Known Issues Sweep

> **Archived session handoff** — known-issues sweep completed; the
> tracked items have either been fixed or are documented in current
> CLAUDE.md/README.

## 2026-04-15 — State regression: resume scan direction bug

**See** `docs/state-regression-findings.md` for full writeup.

Summary: state.redb is intact (18.3 M nodes, prover digest matches
mainnet header at height 1,307,878). Resume scan walks down from the
persisted hint only, so when state is ABOVE the hint the scan misses,
resolves to 0, and overwrites the hint with 0 — amplifying on each
restart. Fix: scan full chain (or both directions) in
`src/main.rs:1289-1307`, and drop the `hint != Some(0)` guard so the
node is self-healing. Follow-ups: sweep ordering (final drain/flush
must happen BEFORE the hint write), silent hint-write failures, explicit
`validator.flush()` in the SIGTERM handler. No resync needed.

---

Load CLAUDE.md, memory, SESSION_CONTEXT.md, and the ergo-node-development skill.

## Goal

Fix the 7 known issues from the v0.1.5 release notes, one at a time.
Tackle in priority order (most user-visible first).

## Issue 1: API fullHeight bug

**Problem**: `GET /info` returns `chain.height()` for both
`headersHeight` and `fullHeight`. During sync, these are different —
headers are ahead of validated blocks. The JVM node reports them
separately: `headersHeight` = chain tip, `fullHeight` = last validated
block. Our node collapses both to the chain tip, masking validator
stalls.

**Where**: `src/main.rs` — the `ApiState` construction and the
`HeaderChainAdapter`. The `shared_validated_height` atomic already
exists and is updated by the validator. The API just doesn't read it.

**Fix**: Pass `shared_validated_height` into `ApiState`, use it for
`full_height` in `get_info`. Keep `chain.height()` for
`headers_height`.

## Issue 2: Stale request tracker entries

**Problem**: When the pipeline rejects a modifier (duplicate,
out-of-order, wrong type), the P2P `RequestTracker` entry for that
modifier ID is never cleared. The new 50K cap prevents OOM but
orphan entries still waste memory and could cause wrong routing if
the same ID is re-requested later.

**Where**: `p2p/src/routing/tracker.rs` (RequestTracker),
`src/pipeline.rs` (rejection path).

**Fix options**:
- Pipeline reports rejected IDs back to the router for cleanup
- RequestTracker adds timestamp-based expiry (e.g., 60s)
- Both (belt and suspenders)

This touches p2p (submodule) — needs a prompt if changing tracker.rs.

## Issue 3: Unknown message forwarding waste

**Problem**: The P2P router forwards messages with unrecognized codes
to all peers. Snapshot messages (76-81) and NiPoPoW messages (90-91)
are handled by specific consumers in the main crate, but the router
doesn't know about them. Every snapshot or NiPoPoW message gets
copied to every peer.

**Where**: `p2p/src/routing/router.rs` — the default case in the
message routing match.

**Fix**: Add a handler registry to the router — consumers register
which message codes they handle. Unknown codes with no registered
handler are dropped. This is a p2p submodule change.

## Issue 4: recompute_active_parameters header_id discarding

**Problem**: `HeaderChain::recompute_active_parameters_from_storage`
uses the extension loader to read extension bytes by height. The
loader returns raw bytes from the store, which include the extension's
embedded header_id. The recomputation discards this header_id without
verifying it matches the expected block — same pattern the session 15
`ChainPopowReader` corruption fix addressed.

**Where**: `chain/src/chain.rs` around
`recompute_active_parameters_from_storage`.

**Risk**: If the store's height index has holes (previously a bug,
fixed in store `96051da`), the extension bytes could be from the
wrong block. Parameters would be silently wrong — consensus-critical.

**Fix**: Validate the embedded header_id matches the header at that
height before parsing. Chain submodule fix.

## Issue 5: Stale p2p/facts submodule pointer

**Problem**: The `p2p/facts` nested submodule pointer references
commit `88c99ad9` which was pruned. Harmless because CI uses
`submodules: true` not `recursive`, and the p2p crate doesn't read
from facts/ at build time.

**Fix**: In the p2p session, `cd facts && git checkout main && cd ..
&& git add facts && git commit`. Quick.

## Issue 6: Multi-peer NiPoPoW comparison

**Problem**: The light bootstrap state machine sends
`GetNipopowProof` to one peer and accepts the first valid response.
KMZ17 §4.3 says: request proofs from multiple peers, compare them
using `NipopowProof::is_better_than`, and install the best.

**Where**: `sync/src/light_bootstrap.rs`.

**Fix**: Send to all outbound peers, collect responses within a
timeout window, compare, install the best. The comparison function
already exists in sigma-rust (`NipopowProof::is_better_than`).

## Issue 7: sigma-rust PR #852

**Problem**: Upstream review pending for the
`has_valid_connections` fix. Nothing to do on our side — just check
status periodically.

## Approach

Work through these in order. Issues 2, 3, 4, 5 touch submodules —
write prompts and dispatch. Issues 1, 6 are in-repo. Issue 7 is
external.

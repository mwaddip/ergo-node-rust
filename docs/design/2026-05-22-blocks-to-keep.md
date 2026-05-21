# Design: `blocks_to_keep` storage pruning

**Date:** 2026-05-22
**Status:** Approved for implementation
**Brainstorm transcript:** `prompts/blocks-to-keep-brainstorm.md` (refresh this with the implementation hand-off)

## Summary

Make `blocks_to_keep` a real retention dial that actually deletes block sections from `modifiers.redb`, rather than just a value advertised in the P2P handshake. Headers are always retained. Non-header sections (BlockTransactions, ADProofs, Extension) are pruned at flush time, with the current voting epoch's extensions preserved. The flush dial adapts to honor the retention horizon so crash recovery is never starved for bodies.

## Motivation

`blocks_to_keep` is currently read from `[node]` in `ergo.toml`, logged at startup, and advertised in the P2P handshake's Mode feature. **That's the entire current implementation.** No references in `store/`, `chain/`, or `state/`. A node configured with `blocks_to_keep = 1000` advertises "pruned" to peers but holds the full archive on disk.

Three concrete reasons to ship a real implementation:

- **Disk economy.** Mainnet block bodies dominate `modifiers.redb` size. At ~1.8M blocks and growing, `blocks_to_keep = 100_000` would shrink the modifier store by an order of magnitude for operators who don't need the historical archive.
- **Honest advertising.** Telling peers we're a pruned node while keeping everything is dishonest in a small but real way. The mismatch isn't currently exploitable but is a smell, and being a good network citizen matters more for us than for JVM since we're a fraction of the peer set — a handful of misbehaving Rust nodes wouldn't need to coordinate to get the implementation flagged.
- **Light-client variants.** `state_type = Light` exists in the code (NiPoPoW bootstrap path); without persistent block-body pruning, the practical disk cost of a light node is the same as a full node. Not a useful "light" mode.

Disk economy is the primary scope. The other two fall out naturally.

## Decisions

| # | Decision | Rationale |
|---|---|---|
| Floor / coupling | **No hard floor.** Flush dial caps at `min(its config, blocks_to_keep)` | Crash recovery needs bodies for `validated_height..tip`; capping the flush gap to `blocks_to_keep` makes any value (including 0) safe without a config-time rejection |
| What's pruned | All 3 non-header section types together (102 BlockTransactions, 104 ADProofs, 108 Extension). Headers (101) retained forever. Voting-epoch alignment preserves the current epoch's extensions | Matches JVM `FullBlockPruningProcessor`; current epoch's extensions are required for `recompute_active_parameters_from_storage` |
| When pruning fires | Piggyback on existing flush_pair; deletes happen in the same modifier-store write transaction as the flush's bookkeeping | Composes with the existing cross-DB durability handshake (`validated_height` in `chain_meta`) one-for-one; no new schedule |
| Migration | **Lazy.** Self-correcting startup WARN fires only when the on-disk archive contains more history than `blocks_to_keep` will retain going forward, pointing at `sharpen prune` for reclamation | Per `[[feedback_derive_from_state]]` — derive the WARN trigger from store state, not a config-history sentinel |
| `sharpen` extension | `sharpen prune --keep=N` / `sharpen prune --before=H` — deferred to a follow-up cut | Mechanical wrapper around the same store API; no new contract surface |
| `state_type = Light` + `blocks_to_keep > 0` | Silently ignored | Light mode is a dead-end code path for body retention; no advisory output |
| Handshake Mode feature | Advertise the configured value, not the on-disk reality | The advertisement is the going-forward policy claim; the archive may temporarily exceed it during lazy migration |
| Re-fetch | Silently drop body requests we can't satisfy | Unchanged from today; handshake tells well-behaved peers not to ask |
| Store API shape | Thick / height-based bulk: `prune_below_height(horizon, type_ids)` + `min_height_present(type_id)`. Caller computes horizon, including voting-epoch alignment | Keeps voting-epoch knowledge out of `facts/store.md`'s "bytes in, bytes out" boundary; reuses existing HEIGHT_INDEX range queries |
| Reorg interaction | Independent. Reorgs use peer-supplied bodies for the new branch; our archive is not consulted. `keep_versions` stays as state.redb's rollback-depth constraint | Refutes the prompt's framing of `blocks_to_keep` as a reorg horizon |

## Architecture

`blocks_to_keep` becomes a real retention dial in addition to its existing handshake-advertisement role. The configured value is honored by two cooperating mechanisms in `sync/`:

1. **Pruning at flush time.** When sync's flush_pair fires, modifier-store rows for the three non-header section types below the retention horizon are deleted in the same write transaction as the flush's modifier-store bookkeeping.
2. **Flush dial cap.** The existing memory-aware flush dial's min/max block guardrails are capped at `blocks_to_keep`, so the `validated_height → tip` gap never exceeds what archived bodies cover.

Headers (101) and the `peer_db` / `chain_meta` tables are untouched. Reorgs are unaffected (they use peer-supplied bodies for the new branch). `state.redb` is unaffected; `keep_versions` remains an independent rollback-depth constraint.

## Components touched

| Crate | Change |
|---|---|
| `store/` | Add `prune_below_height` + `min_height_present` to `ModifierStore` trait + redb impl |
| `chain/` | Add `voting_length()` accessor (math lives in sync) |
| `sync/` | Compute prune horizon; cap flush dial; call prune inside flush_pair; emit startup WARN |
| `main` | Wire `blocks_to_keep` from config into sync (already wired into handshake) |

Contract docs (`facts/store.md`, `facts/sync.md`, `facts/chain.md`) get matching updates. Three dispatched per-crate prompts (`store`, `chain`, `sync`) plus main-session coordination, per the project's per-crate dispatch rule.

## Store contract addition

```rust
/// Delete all modifier rows of the given `type_ids` at heights strictly less
/// than `horizon`. Atomic in a single redb write transaction. Idempotent —
/// calling twice with the same horizon is a no-op the second time.
/// Returns the count of (type_id, modifier_id) pairs deleted.
///
/// Precondition: `type_ids` is non-empty and contains no 101 (headers are
/// never pruned). Returns Err if `type_ids` contains 101.
fn prune_below_height(
    &self,
    horizon: u32,
    type_ids: &[u8],
) -> Result<usize, Self::Error>;

/// Returns the lowest height present in HEIGHT_INDEX for `type_id`, or None
/// if no entries exist. Mirror of `tip(type_id)`. Single forward range
/// query, O(log N). For `type_id == 101` routes to BEST_CHAIN's lowest entry.
fn min_height_present(
    &self,
    type_id: u8,
) -> Result<Option<u32>, Self::Error>;
```

**Postconditions for `prune_below_height`:**
- For every `(t, h, id)` with `t ∈ type_ids` and `h < horizon` previously in HEIGHT_INDEX: `get(t, &id) == None` after the call; the HEIGHT_INDEX row is removed.
- For every `(t, h, id)` with `h >= horizon`: untouched.
- For `t ∉ type_ids` or `t == 101`: untouched.
- On Err: no writes happened (atomic).

**Postconditions for `min_height_present`:**
- For `type_id != 101`: returns the smallest `h` such that HEIGHT_INDEX contains `(type_id, h)`, or `None` if no entry.
- For `type_id == 101`: returns BEST_CHAIN's lowest height entry, or `None` if empty.

## Sync wiring

In the existing flush_pair, after state.redb's `Immediate` flush commits, inside the modifier-store flush write transaction:

```rust
if blocks_to_keep >= 0 {
    let horizon = compute_prune_horizon(
        validated_height,            // state.redb's just-flushed height
        blocks_to_keep as u32,
        chain.voting_length(),
    );
    if horizon > 0 {
        let n = store.prune_below_height(horizon, &[102, 104, 108])?;
        debug!("pruned {n} modifier rows below height {horizon}");
    }
}

fn compute_prune_horizon(flushed: u32, keep: u32, voting_length: u32) -> u32 {
    let raw = flushed.saturating_sub(keep).saturating_add(1);
    // JVM-style: pull horizon back to start of current voting epoch so the
    // current epoch's extensions stay intact for parameter recomputation.
    if raw > voting_length {
        let epoch_start = (raw / voting_length) * voting_length;
        raw.min(epoch_start)
    } else {
        raw
    }
}
```

**Critical detail:** horizon is computed against `validated_height`, not chain tip. Bodies in `(validated_height, tip]` are always retained, so crash recovery can always re-apply.

**Flush dial cap** (sync's existing flush dial config):

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

For `blocks_to_keep = 0`: `effective_max = 0` → flush on every block (Durability::Immediate per block, JVM-equivalent throughput cost). For `blocks_to_keep = -1`: unchanged from today.

## Chain addition

```rust
impl HeaderChain {
    /// Returns the voting epoch length for this network.
    /// Mainnet: 1024. Testnet: 128.
    pub fn voting_length(&self) -> u32 { self.voting_config.voting_length }
}
```

That's the entire chain change. The alignment math lives in `sync/` to keep voting-epoch knowledge out of the store crate and to keep chain's surface minimal.

## Migration / startup WARN

After store open and chain reconstruction:

```rust
if blocks_to_keep > 0 {
    if let Some(actual_min) = store.min_height_present(102)? {
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

Self-correcting via derived state (per `[[feedback_derive_from_state]]`). After the first prune sweep, `actual_min` advances to `configured_horizon` and the WARN stops firing. If the operator shrinks `blocks_to_keep` further, the gap re-opens and the WARN fires once on the next start.

No `chain_meta` sentinel. The store's HEIGHT_INDEX is the source of truth.

## `sharpen` subcommand (deferred to a follow-up cut)

`sharpen prune --keep=N` and `sharpen prune --before=H` are documented here for clarity but explicitly **not** in the first cut. Both are mechanical wrappers around the same store API: they compute a horizon from the CLI args and call `prune_below_height`. No new store surface required.

The first cut ships with the WARN message pointing at `sharpen prune` even though the subcommand doesn't yet exist. Operators who hit the WARN can ignore it, wait for the follow-up cut, or open an issue.

## Re-fetch behavior

Unchanged from today. P2P's existing request-handling silently drops requests for bodies we don't have. The handshake advertisement tells well-behaved peers not to ask. No new "pruned vs. never had" signaling — peers should treat any non-response as "not available", regardless of cause.

## Light mode interaction

`state_type = Light` + `blocks_to_keep > 0`: **silently ignored**. Light mode downloads no bodies, so retention is a no-op in that mode. No advisory output, no startup warning, no `--checkconfig` flag — the operator's config knob is in a dead-end code path and that's their problem to notice.

## Error handling

- `prune_below_height` returning Err: log at WARN, continue. Pruning is opportunistic; failure to delete doesn't break sync or validation. Next flush retries with the same horizon.
- `min_height_present` returning Err at startup: log error, skip the WARN computation. Non-critical.
- Crash mid-prune: redb's atomic write transaction prevents partial visibility. Next flush re-runs with the same horizon; the second run no-ops on already-deleted rows.

## Testing

**Unit:**
- `compute_prune_horizon` with edge cases: `flushed < voting_length`, `blocks_to_keep > flushed`, raw horizon at vs. inside a voting epoch boundary, `blocks_to_keep = 0`, `blocks_to_keep = -1` (skip path).
- `prune_below_height` on a fixture store: verify removed rows gone, retained rows intact, idempotent on re-run, rejects `type_ids` containing 101.
- `min_height_present` on empty store, single entry, after a sweep, for non-existent type_id.
- Flush dial cap across `blocks_to_keep` values from 0 to large-N.

**Integration:**
- Sync to N blocks with `blocks_to_keep = N/2`, verify bodies below horizon gone, current voting epoch's extensions retained, headers all present.
- Crash recovery: kill -9 mid-sync at multiple `blocks_to_keep` values, verify resume catches up to tip with no missing bodies.
- Voting-epoch alignment: configure `blocks_to_keep` such that raw horizon lands mid-epoch, verify the current epoch's extensions are preserved.
- Lazy migration WARN: start with `blocks_to_keep = -1` archive, restart with `blocks_to_keep = 100_000`, verify WARN fires once on startup pointing at `sharpen prune`.

## Out of scope (explicit)

- A protocol-level "I serve historical blocks via fastsync, you can still get pre-window blocks from me" extension. Separate, larger feature.
- Changes to `state_type` semantics. Block-body retention only; state.redb's UTXO retention is governed by `keep_versions` on an independent axis.
- `--checkconfig` CLI flag. Originally on the table; dropped as paternalistic.
- Header pruning. Not in JVM, not in scope here. The store rejects `prune_below_height` calls that include type_id 101.
- The actual `sharpen prune` subcommand. Designed-for, not built.

## Open items for the implementation prompt

- Exact flush dial parameter names — the per-crate `sync/` dispatch prompt should grep `sync/` for the current flush dial config struct.
- Interaction with the `apply_state_tracker` `validation_stuck` WARN from v0.6.2 (`[[project_v0_6_2_release]]`). Probably independent — pruning is post-flush, validation_stuck is per-apply — but worth a sanity-check.
- Startup WARN format: assumed single-line to keep log parsing simple. Confirm during implementation.
- `--keep=N` vs `--keep N` for the eventual `sharpen` subcommand CLI surface — defer to the sharpen follow-up.

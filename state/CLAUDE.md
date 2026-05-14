# enr-state

UTXO state management for the Ergo Rust node — persistent AVL+ authenticated tree over redb, rollback, crash recovery, genesis bootstrap.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Crate: `state/` (enr-state)**.

## Contract

The interface contract is `facts/state.md`. Read it before modifying any public API. If the contract needs changing, update it first, then implement.

## Scope

This crate owns:
- `RedbAVLStorage` — `VersionedAVLStorage` impl backed by redb
- AVL tree node persistence (`nodes` table) keyed by digest label
- Version chain (`versions` table) for rollback within `keep_versions` window
- Block-height metadata (`meta` table — `META_BLOCK_HEIGHT` key)
- Genesis bootstrap (empty storage → genesis digest commit)
- Crash recovery: any half-applied update rolls back to the previous version on next open

This crate does NOT own:
- ErgoScript evaluation — that's `ergo-validation` via `ergo-lib`
- Transaction validation rules — that's `ergo-validation`
- Block validation — that's `ergo-validation`
- The AVL algorithm itself — that's the `ergo_avltree_rust` crate (forked at `mwaddip/ergo_avltree_rust`)
- Network I/O — that's P2P

## Dependencies

- `redb` — storage backend
- `ergo_avltree_rust` (fork) — AVL+ tree algorithm; we provide its `VersionedAVLStorage` trait impl

## Durability

- `update()` uses `Durability::None` per call; the caller invokes the dedicated `flush()` (or any `Durability::Immediate` write) periodically and on shutdown.
- All write transactions should use `set_quick_repair(true)` so redb saves the allocator state per commit. Without quick-repair, `Database::open` after an unclean shutdown does a full file scan to reconstruct the allocator — measured ~2 min on a full mainnet state.redb (and that's on top of OS page cache being warm).

## JVM Reference

The JVM node uses LevelDB with a comparable schema (`avldb` module in `ergoplatform/ergo`). Behavior parity is required for the consensus-critical paths (rollback semantics, version metadata).

## Directory Boundary

This is a per-crate session in a single-repo workspace. Your working directory is `state/` — **do not edit files outside it.** Reads outside the directory are allowed when needed (e.g. `../facts/state.md` for the contract, `../Cargo.toml` for workspace config), but writes belong to either the main session or a session dispatched into a different crate's directory.

You are an expert within the `state/` contract boundary and a confident amateur outside it. Do not implement logic that belongs to `chain/`, `p2p/`, `store/`, `validation/`, or the main orchestration crate. If you need something from outside your boundary, define what you need in the contract and let the integrator wire it. Cross-crate coordination commits (workspace `Cargo.toml`, `README.md`, etc.) are the main session's job — surface what you need in your completion summary.

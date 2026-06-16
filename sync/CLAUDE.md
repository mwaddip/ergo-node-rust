# ergo-sync

Chain sync state machine for the Ergo Rust node. Coordinates P2P requests, storage writes, and validation across full / digest / UTXO-snapshot sync modes. Must survive network partitions, reorgs, and restarts at any point — if it loses track of where it is, the node is stuck.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Workspace crate: `ergo-sync`** — Endurance is maxed (crash/restart/reorg survival) and Architecture is high (the sync modes share significant logic).

## Contract

The interface contract is `facts/sync.md`. Read it before modifying any public API. If the contract needs changing, the main session updates it first, then implement.

## Scope

This crate owns:
- The sync state machine and mode logic (full / digest / UTXO-snapshot)
- Request/download coordination (the sliding download window, delivery tracking)
- Reorg handling and restart/resume (watermark tracking, gap recovery, sweep/backoff)
- At-tip behavior (e.g. runtime AVL cache resize on `synced()`)

This crate does NOT own:
- Header validation (`chain/`), block validation (`validation/`)
- State persistence (`state/`), block/modifier storage (`store/`)
- P2P I/O primitives, framing, and routing (`p2p/`) — sync *drives* the router, it does not implement it

## Dependencies

- `enr-chain`, `ergo-validation`, `enr-state`, `enr-store`, `enr-p2p` — the components sync orchestrates
- `ergo-chain-types` — Header / modifier types

## JVM Reference

The JVM node (`~/projects/ergo-node-build`, v6.0.3) is the reference for sync behavior:
- `ergo-core/.../nodeView/` — NodeViewSynchronizer, modifier download/apply
- network sync info and the request/response cycle

When behavior is ambiguous, the JVM source is correct. When the JVM source disagrees with observed network behavior, the network wins.

## Directory Boundary

This is a per-crate session in a single-repo workspace. Your working directory is `sync/` — **do not edit files outside it.** Reads outside the directory are allowed when needed (`../facts/sync.md` for the contract, `../Cargo.toml` for workspace config), but writes belong to either the main session or a session dispatched into a different crate's directory.

You are an expert within the `sync/` contract boundary and a confident amateur outside it. Do not implement logic that belongs to `chain/`, `validation/`, `state/`, `store/`, or `p2p/`. If you need something from outside your boundary, define what you need in the contract and let the integrator wire it. Cross-crate coordination commits (workspace `Cargo.toml`, `README.md`, the `facts/` contracts) are the main session's job — surface what you need in your completion summary.

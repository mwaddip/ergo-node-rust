# ergo-mempool

Transaction pool for the Ergo Rust node. Validate-on-entry, replace-by-fee, family weighting, fee statistics, rate limiting. Not persistent — a crash means an empty mempool, which is acceptable.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Workspace crate: `ergo-mempool`** — Agility (throughput) and Luck (conflicting-tx / replacement edge cases) carry the extra attention; not persistent, so Endurance is standard.

## Contract

The interface contract is `facts/mempool.md`. Read it before modifying any public API. If the contract needs changing, the main session updates it first, then implement.

## Scope

This crate owns:
- The transaction pool: insertion, ordering, eviction policies
- Double-spend / conflict detection, replace-by-fee, family weighting
- Fee statistics (histogram), and rate limiting

This crate does NOT own:
- Stateful transaction validation (that's `ergo-lib`, invoked via `validation/` on entry)
- Persistence — the mempool is intentionally in-memory

## Dependencies

- `ergo-lib` — Transaction types, validation primitives
- `ergo-chain-types` — chain types

## JVM Reference

The JVM node (`~/projects/ergo-node-build`, v6.0.3) is the reference for mempool behavior:
- `ergo-core/.../nodeView/mempool/` — ErgoMemPool, OrderedTxPool, eviction and weighting

When behavior is ambiguous, the JVM source is correct. When the JVM source disagrees with observed network behavior, the network wins.

## Directory Boundary

This is a per-crate session in a single-repo workspace. Your working directory is `mempool/` — **do not edit files outside it.** Reads outside the directory are allowed when needed (`../facts/mempool.md` for the contract, `../Cargo.toml` for workspace config), but writes belong to either the main session or a session dispatched into a different crate's directory.

You are an expert within the `mempool/` contract boundary and a confident amateur outside it. Do not implement validation, consensus, or persistence logic that belongs to `validation/`, `chain/`, `state/`, or `store/`. If you need something from outside your boundary, define what you need in the contract and let the integrator wire it. Cross-crate coordination commits (workspace `Cargo.toml`, `README.md`, the `facts/` contracts) are the main session's job — surface what you need in your completion summary.

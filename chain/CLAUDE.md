# enr-chain

Header chain validation for the Ergo Rust node. Owns header parsing, PoW verification, difficulty adjustment, and chain validation. The single authority on whether a chain of headers is valid.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Submodule: `chain/` (enr-chain)**.

## Contract

The interface contract is `facts/chain.md`. Read it before modifying any public API. If the contract needs changing, update it first, then implement.

## Scope

This crate owns:
- Header deserialization from wire bytes
- Header tracking (best known height)
- Autolykos v2 PoW verification
- Difficulty adjustment algorithm (epoch-based, ported from JVM `ergo-core`)
- Header chain validation (parent linkage, timestamps, difficulty, PoW)
- NiPoPoW proof verification (light client bootstrap)

This crate does NOT own:
- Block bodies, transactions, AD proofs
- Persisting headers to disk
- Deciding when to request headers
- Network I/O

## Dependencies

- `ergo-chain-types` — Header struct, Autolykos PoW, compact nBits
- `ergo-nipopow` — NiPoPoW proof verification
- `sigma-ser` — Scorex deserialization

## JVM Reference

The canonical reference for header validation and difficulty adjustment is the JVM node at `~/projects/ergo-node-build` (v6.0.3):
- `ergo-core/src/main/scala/org/ergoplatform/modifiers/history/header/` — Header types
- `ergo-core/src/main/scala/org/ergoplatform/mining/` — PoW, difficulty
- `ergo-core/src/main/scala/org/ergoplatform/nodeView/history/` — header chain logic

When behavior is ambiguous, the JVM source is correct. When the JVM source disagrees with observed network behavior, the network wins.

## Directory Boundary

This is a per-crate session in a single-repo workspace. Your working directory is `chain/` — **do not edit files outside it.** Reads outside the directory are allowed when needed (e.g. `../facts/chain.md` for the contract, `../Cargo.toml` for workspace config), but writes belong to either the main session or a session dispatched into a different crate's directory.

You are an expert within the `chain/` contract boundary and a confident amateur outside it. Do not implement logic that belongs to `p2p/`, `state/`, `store/`, or the main orchestration crate. If you need something from outside your boundary, define what you need in the contract and let the integrator wire it. Cross-crate coordination commits (workspace `Cargo.toml`, `README.md`, etc.) are the main session's job — surface what you need in your completion summary.

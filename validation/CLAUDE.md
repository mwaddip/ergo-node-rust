# ergo-validation

Block-level validation for the Ergo Rust node. Composes header checks (from `chain/`), transaction validation (from `ergo-lib`), AD proof verification, and UTXO lookups (from `state/`). Mostly glue — but consensus-critical glue: the final arbiter on whether a block is valid. If a bad block passes, the node's state is corrupted.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Workspace crate: `ergo-validation`** — Strength is maxed (the final arbiter; treat every block as hostile) and Architecture is high (wrong abstractions here infect everything above).

## Contract

The interface contract is `facts/validation.md`. Read it before modifying any public API. If the contract needs changing, the main session updates it first, then implement.

## Scope

This crate owns:
- Block validation in both digest and UTXO modes (state-root verification differs; section parsing, state-change computation, and tx validation are shared)
- Section parsing (BlockTransactions, ADProofs, Extension) and parameter extraction at voting-epoch boundaries
- AD proof verification orchestration (`BatchAVLVerifier` digest-side, `PersistentBatchAVLProver` UTXO-side)
- Stateful transaction validation via `ergo-lib`, and `reset_to` (rollback to a validated state)

This crate does NOT own:
- Header chain logic (`chain/`)
- The AVL+ tree implementation and persistence (`state/`)
- The mempool (`mempool/`), script-evaluation internals (`ergo-lib` / sigma-rust)

## Dependencies

- `ergo-lib` — stateful transaction validation, `ErgoStateContext`, script verification
- `ergo-chain-types` — Header, ADDigest
- `sigma-ser` — Scorex serialization
- `ergo_avltree_rust` — AVL+ verifier/prover (forked; Err-not-panic on degenerate proofs)
- `enr-state` — UTXO state access

## JVM Reference

The JVM node (`~/projects/ergo-node-build`, v6.0.3) is the canonical reference for block/transaction validation and AD proofs:
- `ergo-core/.../modifiers/` — block and section validation
- `ergo-wallet` transaction validation (already in `ergo-lib`)
- state application + `VersionedAVLStorage`

When behavior is ambiguous, the JVM source is correct. When the JVM source disagrees with observed network behavior, the network wins.

## Directory Boundary

This is a per-crate session in a single-repo workspace. Your working directory is `validation/` — **do not edit files outside it.** Reads outside the directory are allowed when needed (`../facts/validation.md` for the contract, `../Cargo.toml` for workspace config), but writes belong to either the main session or a session dispatched into a different crate's directory. **sigma-rust is a submodule boundary** — never edit it; dispatch a prompt instead.

You are an expert within the `validation/` contract boundary and a confident amateur outside it. Do not implement logic that belongs to `chain/`, `state/`, `store/`, or the main orchestration crate. If you need something from outside your boundary, define what you need in the contract and let the integrator wire it. Cross-crate coordination commits (workspace `Cargo.toml`, `README.md`, the `facts/` contracts) are the main session's job — surface what you need in your completion summary.

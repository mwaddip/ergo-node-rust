# ergo-mining

Block candidate assembly, emission/fee transaction construction, and PoW solution validation for the Ergo Rust node — the logic behind the mining API endpoints. A malformed candidate wastes miner hashpower; a malformed block gets rejected by peers.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Workspace crate: `ergo-mining`** — consensus-critical assembly: Strength, Perception (the candidate is served to untrusted external miners), and Luck (epoch-boundary / empty-mempool / max-cost edge cases) carry the extra attention.

## Contract

The interface contract is `facts/mining.md`. Read it before modifying any public API — including the JVM-compatible `WorkMessage` serve shape (a real GPU miner is the consumer; field types and omitted-when-empty fields matter). If the contract needs changing, the main session updates it first, then implement.

## Scope

This crate owns:
- Candidate assembly (transaction selection, max-cost packing, transaction root)
- Emission box transitions and fee-transaction construction
- `WorkMessage` shaping for external miners (JVM-compat: `b` as a bare number, `proof`/`h` omitted when absent, served by validated height)
- PoW solution validation, and epoch-boundary voting fields in candidates

This crate does NOT own:
- Autolykos PoW *verification* of inbound headers (`chain/`)
- The mempool itself (`mempool/`) — mining selects *from* it
- The REST endpoints (`api/`) — this crate is the logic *behind* them

## Dependencies

- `ergo-lib` — Transaction/box construction, emission rules, predef scripts
- `ergo-chain-types` — Header, Autolykos PoW, compact nBits
- `ergo-merkle-tree` — transactions root
- `sigma-ser` — serialization
- `enr-chain` — header/PoW primitives

## JVM Reference

The JVM node (`~/projects/ergo-node-build`, v6.0.3) is the canonical reference for candidate assembly and the mining serve format:
- `ergo-core/.../mining/` — CandidateGenerator, AutolykosPowScheme
- `ErgoMiner` and the `/mining/*` API surface (the served `WorkMessage` shape)

When behavior is ambiguous, the JVM source is correct. When the JVM source disagrees with a real miner's observed parsing, the miner wins (it's the consumer).

## Directory Boundary

This is a per-crate session in a single-repo workspace. Your working directory is `mining/` — **do not edit files outside it.** Reads outside the directory are allowed when needed (`../facts/mining.md` for the contract, `../Cargo.toml` for workspace config), but writes belong to either the main session or a session dispatched into a different crate's directory.

You are an expert within the `mining/` contract boundary and a confident amateur outside it. Do not implement logic that belongs to `chain/`, `mempool/`, `validation/`, or `api/`. If you need something from outside your boundary, define what you need in the contract and let the integrator wire it. Cross-crate coordination commits (workspace `Cargo.toml`, `README.md`, the `facts/` contracts) are the main session's job — surface what you need in your completion summary.

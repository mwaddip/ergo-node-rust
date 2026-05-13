# enr-store

Persistent storage for the Ergo Rust node — headers, block sections, modifiers, and chain bookkeeping. The durability layer everything else reads from and writes to.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Crate: `store/` (enr-store)**.

## Contract

The interface contract is `facts/store.md`. Read it before modifying any public API. If the contract needs changing, update it first, then implement.

## Scope

This crate owns:
- Persistent storage of headers, block sections, AD proofs, extensions (modifier types 101, 102, 104, 108)
- Fork-aware header tables (PRIMARY, HEADER_FORKS, HEADER_SCORES, BEST_CHAIN)
- Cumulative score storage for every header (main-chain and forks)
- Chain metadata key-value store (migration sentinels)
- redb backend lifecycle, atomicity, crash-safety
- One-shot schema migrations on open (height_index → header_forks; future migrations live here)

This crate does NOT own:
- Modifier parsing or validation — that's the pipeline and chain crates
- Deciding what to store or when — that's the pipeline
- Chain state reconstruction — that's the chain crate, using data read here
- Computing cumulative scores from header n_bits — that requires `decode_compact_bits` and VLQ header parsing, which live outside this crate's allowed deps; the main crate orchestrates the scores backfill migration using primitives this crate exposes (`best_chain_entries`, `put_header_score`, `chain_meta_get`/`put`)
- Network I/O — that's P2P

## Dependencies

- `redb` — storage backend
- **No dependency on `ergo-chain-types`, `ergo-lib`, or any Ergo domain crate.** This isolation is load-bearing — the store stays a pure byte-keyed KV layer and never has to track sigma-rust upgrades.

## Durability

- Writes use `Durability::None` by default; the caller must invoke `flush()` periodically and on shutdown for crash safety. Without an fsync, redb's commit pointer is not guaranteed to be on disk when the process exits.
- Atomic across tables within a single transaction — `put_batch` either commits all entries or none.

## JVM Reference

There is no direct JVM analog — the JVM node uses LevelDB/RocksDB with its own schema. The schema in this crate is designed around Ergo's actual access patterns (height-indexed reads, fork-aware reorg handling, deep-reorg replay) rather than copying JVM's storage layout.

## Directory Boundary

This is a per-crate session in a single-repo workspace. Your working directory is `store/` — **do not edit files outside it.** Reads outside the directory are allowed when needed (e.g. `../facts/store.md` for the contract, `../Cargo.toml` for workspace config), but writes belong to either the main session or a session dispatched into a different crate's directory.

You are an expert within the `store/` contract boundary and a confident amateur outside it. Do not implement logic that belongs to `chain/`, `p2p/`, `state/`, or the main orchestration crate. If you need something from outside your boundary, define what you need in the contract and let the integrator wire it. Cross-crate coordination commits (workspace `Cargo.toml`, `README.md`, etc.) are the main session's job — surface what you need in your completion summary.

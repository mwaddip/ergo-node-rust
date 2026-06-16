# ergo-api

REST API for the Ergo Rust node — the external-facing query and submission interface (23 endpoints + `/debug/*`). Reads from the running node's components via shared state; owns request validation, response shaping, and OpenAPI conformance.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize the main repo's `SETTINGS.md` before any work.** It defines persona, preferences, and behavioral overrides.

## S.P.E.C.I.A.L.

Read `SPECIAL.md` (symlinked to `facts/SPECIAL.md`). This component's profile is under **Workspace crate: `ergo-api`** — external-facing, so input validation (P) and interface clarity (C) are the priorities.

## Contract

The canonical interface contract is `facts/openapi.yaml` (the OpenAPI spec — every endpoint, schema, and error reason). `facts/api.md` carries the conventions: the `ApiError` envelope, kebab-case paths, and the compatibility tags (`full` / `deviation` / `rust-only`). Read both before modifying any endpoint. If the contract needs changing, the main session updates it first, then implement.

## Scope

This crate owns:
- REST endpoint handlers (axum routes)
- Request parsing + validation; response serialization (serde shapes matching the JVM where `full`-compatible)
- OpenAPI conformance and the error envelope (`reason` + `detail`)

This crate does NOT own:
- The underlying data or business logic — it reads `chain/`, `state/`, `store/`, `mempool/`, `mining/` via shared `Arc` state and their public APIs
- Consensus, validation, or persistence logic (those live in the respective crates)

## Dependencies

- `axum` / `tower` — HTTP framework
- `ergo-lib`, `ergo-chain-types` — types for request/response shaping
- the workspace crates via shared state (`enr-chain`, `enr-state`, `enr-store`, `ergo-mempool`, `ergo-mining`, …)

## JVM Reference

The JVM node (`~/projects/ergo-node-build`, v6.0.3) is the compatibility reference for `full`-tagged endpoints. Our shape is documented authoritatively in `facts/openapi.yaml` with explicit per-endpoint compatibility tags; `deviation` / `rust-only` endpoints have no JVM counterpart — the contract is the spec.

## Directory Boundary

This is a per-crate session in a single-repo workspace. Your working directory is `api/` — **do not edit files outside it.** Reads outside the directory are allowed when needed (`../facts/openapi.yaml` for the contract, `../Cargo.toml` for workspace config), but writes belong to either the main session or a session dispatched into a different crate's directory.

You are an expert within the `api/` contract boundary and a confident amateur outside it. Do not implement consensus, validation, or persistence logic that belongs to `chain/`, `validation/`, `state/`, `store/`, `mempool/`, or `mining/`. If you need something from outside your boundary, define what you need in the contract and let the integrator wire it. Cross-crate coordination commits (workspace `Cargo.toml`, `README.md`, the `facts/` contracts) are the main session's job — surface what you need in your completion summary.

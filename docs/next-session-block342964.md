# Session 22 — COMPLETED

Block 342,964 investigation → full mainnet validation from genesis.

## Results

- **4 sigma-rust consensus bugs found and fixed** (PRs #855, #857, #859)
- **966k+ mainnet blocks validated** from genesis, no checkpoint, full ErgoScript evaluation
- **Parallel validation pipeline** built and deployed (session 22b)
- **checkpoint_height removed** from mainnet config permanently
- **v0.2.0 tagged**

## Bugs found

1. **selfBoxIndex** (block 342,964) — JVM returned -1 for pre-v2 trees
2. **BigInt modulo** (block 670,557) — Rust remainder vs Java mathematical modulo
3. **BoolToSigmaProp** (block 680,692) — SigmaProp passthrough for pre-v2 trees
4. **xorOf** — distinct.length==2 bug for pre-v2 trees (found by audit, not by failure)

## Key learnings

- Pre-JIT leniency gates on `tree_version()` (per-script), NOT `activated_script_version()` (per-block)
- Cargo caches git dep builds by commit hash — editing .cargo/git/checkouts has no effect
- Use `[patch]` override for local sigma-rust testing
- sigma-rust is a submodule boundary — dispatch only, no direct edits

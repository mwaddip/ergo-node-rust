# enr-store

Persistent modifier storage for [ergo-node-rust](https://github.com/mwaddip/ergo-node-rust). Stores block headers, block sections, and other modifiers as pre-validated, pre-serialized bytes in a [redb](https://github.com/cberner/redb) embedded database.

## What it does

- **Primary storage** keyed by `(type_id, modifier_id)` for raw modifier bytes
- **Height index** for non-header modifiers (`type_id, height` lookups)
- **Fork-aware header storage** supporting multiple headers per block height, cumulative difficulty scores, and atomic best-chain switching for chain reorganization
- **Migration** from single-header-per-height schema to fork-aware schema on first open

## Trait: `ModifierStore`

The public API is the `ModifierStore` trait in `src/lib.rs`. The redb implementation (`RedbModifierStore`) is the only backend. The trait exists so upstream components depend on the contract, not the storage engine.

Key method groups:

| Group | Methods |
|-------|---------|
| Generic modifiers | `put`, `put_batch`, `get`, `get_id_at`, `contains`, `tip` |
| Fork-aware headers | `put_header`, `header_ids_at_height`, `header_score`, `best_header_at`, `best_header_tip` |

## Header writes go through the fork-aware tables

Headers are `type_id == 101`. Whether you call the generic `put` / `put_batch` API or the explicit `put_header` API, the bytes land in the same place:

| Caller intent | Methods | Tables written |
|---|---|---|
| Main-chain header (caller has certified it as best chain) | `put`, `put_batch` with `type_id=101` | `primary` + `header_forks (h, 0)` + `header_scores` (empty) + `best_chain` (unconditional) |
| Fork header at the same height as a known header | `put_header` with `fork>0` | `primary` + `header_forks (h, fork)` + `header_scores` + `best_chain` (only if absent) |

For headers, `put` / `put_batch` **never write to `height_index`**. The `(101, h)` slot in `height_index` is the legacy schema and is cleaned up by the one-shot migration on first open. As a consequence, `get_id_at(101, h)` and `tip(101)` are routed to `best_header_at(h)` and `best_header_tip()` respectively, so the legacy lookup APIs continue to work after the schema change.

`put_batch`'s insert into `best_chain` for `type_id=101` is **unconditional**: main-chain headers are authoritative for their height slot. This overwrites any stale fork-first-arrival entry that an earlier `put_header` may have left, and overwrites the previous main-chain entry on a same-height reorg replacement.

For non-header modifiers (block sections, extensions, etc.), `put` / `put_batch` continue to write to `primary` + `height_index` as before.

## Tables

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `primary` | `(type_id, modifier_id)` | raw bytes | All modifier data |
| `height_index` | `(type_id, height)` | modifier_id | Height lookup for non-header types only |
| `header_forks` | `(height, fork)` | header_id | All known headers per height; `fork=0` reserved for the best chain |
| `header_scores` | `header_id` | BigUint bytes | Cumulative difficulty per header (empty placeholder for main-chain) |
| `best_chain` | `height` | header_id | Current best chain (one per height) |

## Building and testing

See the [main ergo-node-rust repo](https://github.com/mwaddip/ergo-node-rust) for build instructions and project documentation.

```
cargo test
```

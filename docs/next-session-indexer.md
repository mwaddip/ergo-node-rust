# Session 21 — Indexer Addon

Load CLAUDE.md, ~/projects/OVERRIDES.md, ergo-node-development skill, and
memory. Read `project_plugin_architecture.md` and `project_fastsync_design.md`
from memory for the addon pattern.

## Context

Session 20 built the fastsync addon (`addons/fastsync/`), which established
the pattern: separate crate with its own `[workspace]`, excluded from the
main workspace, communicates with the node via REST API. Heavy deps (reqwest,
TLS) stay out of the core binary.

The node is syncing mainnet with `checkpoint_height = 417792`. Fastsync is
pushing headers. The mainnet config is at `/etc/ergo-node/ergo.toml`.

## Pre-check: mainnet sync

```bash
curl -s http://127.0.0.1:9052/info | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(f'headers={d[\"headersHeight\"]} validated={d[\"fullHeight\"]}')"
```

If past 417,792 cleanly, tag v0.2.0.

## What to build

An indexer addon (`addons/indexer/`) that:

1. Subscribes to the node's block feed (poll `/info` + `/blocks/at/{height}`)
2. Parses full blocks (transactions, boxes, tokens)
3. Indexes into a local SQL database (SQLite or PostgreSQL)
4. Serves rich query endpoints that the core REST API doesn't provide

### Why

The core node's REST API is intentionally minimal — it serves what miners,
wallets, and peers need. An indexer serves what explorers, dApps, and
analytics tools need:

- Box lookup by address (not just by ID)
- Transaction history for an address
- Token metadata and holder lists
- Unspent boxes by ErgoTree/template hash
- Balance history over time

These queries require full-table scans or specialized indexes that don't
belong in the core node's redb store.

### Architecture

Same pattern as fastsync:

```
addons/indexer/
  Cargo.toml        # [workspace], excluded from main workspace
  src/
    main.rs         # CLI entry point, poller
    schema.rs       # SQL schema definitions
    parser.rs       # Block → SQL inserts
    api.rs          # Query endpoints (axum)
```

**Key decisions to make:**
- SQLite vs PostgreSQL (SQLite is simpler to ship, PG is better for concurrent queries)
- Poll-based vs webhook/SSE (node doesn't have SSE yet — poll `/info` for now)
- Which indexes to build (address lookup is the minimum viable set)
- Schema design (normalized vs. denormalized for query performance)

### Reference

Check what the existing Ergo explorer (explorer.ergoplatform.com) provides
for API surface area. Also look at:
- JVM's `/blockchain/*` endpoints for the query patterns users expect
- EIP-1 (UTXO-set scanning) for the box-by-address pattern

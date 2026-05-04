# enr-state

Persistent, versioned, crash-safe AVL+ authenticated dictionary over [redb](https://github.com/cberner/redb). This crate implements the `VersionedAVLStorage` trait from [`ergo_avltree_rust`](https://github.com/mwaddip/ergo_avltree_rust), providing the storage backend that `PersistentBatchAVLProver` needs to persist an AVL+ tree to disk with rollback support.

## Role in ergo-node-rust

This is the **UTXO state persistence layer** for [ergo-node-rust](https://github.com/mwaddip/ergo-node-rust), a ground-up Rust implementation of the Ergo blockchain full node. It sits between the AVL+ tree algorithm (provided by `ergo_avltree_rust`) and the block validation layer, which applies state transitions and verifies authenticated state roots.

```
Block validation (ergo-validation)
        |
        v
  State management (enr-state)  <-- this crate
        |
        v
  AVL+ tree algorithm (ergo_avltree_rust)
```

The crate is **Ergo-agnostic** — it stores arbitrary `(ADKey, ADValue)` byte pairs in an authenticated tree. It does not depend on `ergo-lib`, `ergo-chain-types`, or any blockchain-specific types.

## Features

- **Single-file ACID** — one redb database with atomic write transactions. No partial writes, ever.
- **Undo-log rollback** — modified and removed nodes recorded per version; rollback replays undo records in reverse.
- **Lazy node loading** — the full UTXO tree (~4M+ entries on mainnet) never needs to be in memory. Nodes are loaded from redb on demand via the `Resolver` callback.
- **Configurable version retention** — `keep_versions` controls rollback depth. Set to 0 during initial sync for zero undo overhead.
- **Snapshot bootstrap** — `load_snapshot()` bulk-inserts packed nodes without undo records.

## Building

See the [ergo-node-rust](https://github.com/mwaddip/ergo-node-rust) repository for build instructions and project documentation.

## License

Same as the parent project.

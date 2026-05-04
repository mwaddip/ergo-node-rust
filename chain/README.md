# enr-chain

Header chain validation for [ergo-node-rust](https://github.com/mwaddip/ergo-node-rust) — the single authority on whether a chain of Ergo block headers is valid.

## What it does

`enr-chain` owns everything between receiving raw header bytes from the P2P layer and answering "is this chain of headers valid?" It does not deal with block bodies, transactions, persistent storage, or network I/O.

### Capabilities

- **Header parsing** — deserialize Scorex-encoded headers from wire bytes
- **Header tracking** — observe headers and track the best known network height
- **PoW verification** — verify Autolykos v2 proof-of-work solutions
- **Difficulty adjustment** — epoch-based recalculation matching the JVM reference node (pre- and post-EIP-37)
- **Chain validation** — parent linkage, timestamps, difficulty, cumulative score tracking
- **Chain reorganization** — 1-deep tip replacement and deep fork switching with full rollback on failure
- **SyncInfo messages** — build and parse V1/V2 SyncInfo for peer chain comparison
- **Block section IDs** — derive modifier IDs for BlockTransactions, ADProofs, and Extension sections

### Public API

| Export | Purpose |
|---|---|
| `parse_header` | Deserialize a header from wire bytes |
| `HeaderTracker` | Stateless best-height observer |
| `verify_pow` | Standalone PoW check |
| `HeaderChain` | Validated chain with append, reorg, and score tracking |
| `AppendResult` | Distinguishes chain extension from fork detection |
| `ChainConfig` | Network parameters (testnet/mainnet) |
| `build_sync_info` / `parse_sync_info` | SyncInfo V1/V2 wire format |
| `section_ids` / `required_section_ids` | Block section modifier IDs |
| `StateType` | UTXO vs Digest mode |
| `decode_compact_bits` | nBits to difficulty conversion |

## Role in ergo-node-rust

This crate is a submodule of the main `ergo-node-rust` workspace. It is consumed by the sync state machine (for chain management and peer comparison) and by the block validation layer (for header membership checks).

It depends on the [sigma-rust](https://github.com/ergoplatform/sigma-rust) ecosystem for cryptographic primitives (`ergo-chain-types` for the `Header` struct and Autolykos PoW, `sigma-ser` for Scorex serialization).

## Building and testing

See the [main repository](https://github.com/mwaddip/ergo-node-rust) for workspace-level build instructions.

To build and test this crate in isolation:

```sh
cargo test
cargo clippy
```

## License

Same as the parent repository.

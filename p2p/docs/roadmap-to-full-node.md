# From Proxy to Full Node: Roadmap

## What Exists in Rust

The ecosystem is more complete than expected. Here's the inventory:

### Ready to Use

| Component | Crate | What it does |
|---|---|---|
| ErgoTree interpreter | `ergotree-interpreter` | Full script evaluator, 70+ opcodes, sigma protocols |
| Transaction validation | `ergo-lib` | Stateful validation: ERG/token preservation, script verification, storage rent |
| Transaction signing | `ergo-lib` | Wallet, multi-sig, BIP-39/44, coin selection, tx builder |
| Box/UTXO primitives | `ergo-lib` | ErgoBox, registers, tokens, ErgoStateContext |
| Block header types | `ergo-chain-types` | Full Header struct with Autolykos solution |
| Autolykos v2 PoW | `ergo-chain-types` | `pow_hit()` calculation, compact bits, table size growth |
| NiPoPoW verification | `ergo-nipopow` | Full KMZ17 algorithm, proof comparison, best chain selection |
| AVL+ authenticated tree | `ergo_avltree_rust` | Prover + verifier, batch operations, versioned storage trait |
| Merkle proofs | `ergo-merkle-tree` | Tree, proof, batch multiproof |
| ErgoScript compiler | `ergoscript-compiler` | Source to ErgoTree |
| Scorex serialization | `sigma-ser` | VLQ, ZigZag, binary encoding |
| **P2P networking** | **ergo-proxy-node** | **Handshake, framing, message routing (this project)** |

### Missing (Must Build)

| Component | Difficulty | Reference |
|---|---|---|
| Header chain validation | Medium | `ergo-core` sub-module |
| Difficulty adjustment | Medium | Autolykos2 epoch recalculation, documented algorithm |
| UTXO set management | Hard | Apply block → update AVL tree, rollback |
| AD proofs verification | Medium | Batch AVL verifier (crate exists, orchestration missing) |
| Block validation (full) | Hard | Combine header + tx + AD proof + UTXO checks |
| Extension section handling | Easy | Parameters, interlinks, voting |
| Mempool | Medium | Tx ordering, eviction, double-spend detection |
| Block/modifier storage | Medium | LevelDB or similar persistent backend |
| Chain sync state machine | Hard | Full/digest/UTXO-snapshot modes |
| Emission schedule | Easy | Fixed rate period, epoch reduction, documented formula |
| Soft-fork voting | Easy | Parameter voting, rule activation |
| Autolykos v1 PoW | Low priority | Only needed for pre-hardfork blocks, could skip if bootstrapping from snapshot |

### Dead or Superseded

| Crate | Status |
|---|---|
| `ergo-utilities-rust` | Abandoned, pinned to ergo-lib 0.13 (current: 0.28) |
| sigma-rust `ergo-p2p` | Architecture only, codec is `todo!()`, no message types. Our proxy supersedes this. |
| `ogre` (TypeScript) | Abandoned light node attempt (April 2023) |

## Key Insight

**No one has attempted a Rust full node before.** The sigma-rust `ergo-p2p` crate was started but never completed — the encode method is literally `todo!()`. Our ergo-proxy-node is now the most functional Rust P2P implementation for Ergo.

The transaction validation in `ergo-lib` is surprisingly complete — it does stateful validation including script evaluation. The main gap is everything at the block level and above: header validation, UTXO state management, chain sync.

## Natural Progression

The proxy already has the hardest-to-get-right piece: a working P2P layer that real nodes accept. Here's the order of what to bolt on, from easiest/most obvious to hardest:

### Phase 1: Header Awareness (easiest first step)

**What:** Instead of treating SyncInfo as opaque, parse it. Track the header chain. Know what height the network is at.

**Why:** The proxy currently forwards SyncInfo blindly. If it understood headers, it could answer "what height are you?" without forwarding, reducing load on outbound peers. It also enables health monitoring — the proxy knows if it's seeing fresh blocks.

**Uses:** `ergo-chain-types` Header struct (exists), header serialization (exists).

**Builds toward:** Header chain validation.

### Phase 2: PoW Verification

**What:** Verify that block headers have valid proof of work before forwarding them.

**Why:** Right now the proxy forwards everything, including potentially invalid blocks. PoW verification is cheap (one hash computation) and filters out garbage without needing any chain state.

**Uses:** `ergo-chain-types::AutolykosPowScheme::pow_hit()` (exists). Just need to compare against nBits target.

**Builds toward:** Full header validation.

### Phase 3: Header Chain Validation

**What:** Validate headers form a valid chain: parent hash matches, timestamps are reasonable, difficulty adjusts correctly, PoW is valid for the claimed difficulty.

**Why:** This makes the proxy a validating relay for headers. It can reject invalid chains without storing full blocks or UTXO state. Combined with NiPoPoW (`ergo-nipopow` exists), it becomes a light client that can verify chain quality.

**Uses:** Header types (exists), PoW (exists), NiPoPoW (exists). Missing: difficulty adjustment algorithm (needs porting from `ergo-core`).

**Builds toward:** SPV-level security.

### Phase 4: Transaction Validation

**What:** Validate transactions before forwarding them, given their input boxes.

**Why:** The proxy becomes a validating mempool relay. It can reject invalid transactions at the network boundary, reducing spam. `ergo-lib` already implements full stateful transaction validation.

**Uses:** `ergo-lib::TransactionContext::validate()` (exists). Needs a way to look up input boxes — either from an outbound full node's REST API or from local state.

**Builds toward:** Full mempool.

### Phase 5: UTXO State Management

**What:** Maintain the UTXO set using the AVL+ tree. Apply blocks to update state, support rollbacks.

**Why:** This is the core of a full node. With UTXO state, the node can validate blocks end-to-end and serve state queries to peers.

**Uses:** `ergo_avltree_rust` (exists, needs persistence backend), `ergo-lib` box/UTXO types (exists). Missing: the orchestration layer (apply block → insert/remove UTXOs → compute new state root → verify AD proofs).

**Builds toward:** Full validating node.

### Phase 6: Full Node

**What:** Block storage, chain sync state machine, mempool, REST API.

**Why:** Complete replacement for the JVM node.

**Uses:** Everything above, plus new storage layer and sync protocol.

## The First Bolt-On: Header Awareness

After the proxy is solid and stable, the most obvious next step is Phase 1. It requires:

1. Parse `ergo-chain-types::Header` from ModifierResponse bodies (when modifier_type = 1)
2. Store latest known header (height, id, timestamp) in the router
3. Optionally parse SyncInfo to extract the peer's chain tip
4. Add a simple `/status` endpoint or log line showing current network height

This is a small change that uses an existing crate, doesn't break the proxy's "forward everything" model, and gives immediate operational visibility. It's also the foundation for everything else — you can't validate what you can't parse.

## Dependency Versions

If integrating sigma-rust crates, pin to `ergo-lib = "0.28.0"` (latest as of Feb 2026). The crate has stabilized significantly. Key transitive dependencies:

- `k256` (secp256k1) for curve operations
- `blake2` for hashing (we already use this)
- `num-bigint` for Autolykos arithmetic
- `bounded-vec` for size-constrained collections

The proxy currently has zero ergo crate dependencies (forwards opaque bytes). Each phase adds dependencies incrementally — no need to pull in the full sigma-rust workspace until Phase 4.

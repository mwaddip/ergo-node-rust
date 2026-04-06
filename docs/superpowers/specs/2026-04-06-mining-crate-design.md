# Mining Crate Design

**Date:** 2026-04-06
**Status:** Approved
**Contract:** `facts/mining.md`

## Goal

Standalone `mining/` workspace crate that assembles block candidates for
external GPU miners and validates PoW solutions. The API crate is a thin
HTTP facade — the mining crate owns all logic.

## Scope

Mainnet-ready. Includes EIP-27 re-emission from the start. Deferred:
`candidateWithTxs`, `candidateWithTxsAndPk` (pool operator features).

## Architecture

```
  Miner (GPU software)
    │ GET /mining/candidate ──────► API crate ──► mining::generate_candidate()
    │ POST /mining/solution  ──────► API crate ──► mining::validate_solution()
    ▼
  mining/ crate
    ├── CandidateGenerator         (owns cached candidate, assembly logic)
    ├── emission tx construction   (EmissionRules, ErgoTreePredef, EIP-27)
    ├── fee tx construction        (aggregate fee proposition outputs)
    ├── tx selection               (mempool priority, cost/size limits)
    ├── extension building         (interlinks via ergo-nipopow, voting)
    ├── header serialization       (HeaderWithoutPow → Blake2b256 → msg)
    └── solution validation        (AutolykosPowScheme::validate)
```

The mining crate does not know about HTTP, JSON, or axum. It exposes Rust
methods. The API crate handles wire format.

## Shared State

```rust
pub struct MiningState {
    pub config: MinerConfig,
    pub candidate: RwLock<Option<CachedCandidate>>,
    pub validator: Arc<Mutex<UtxoValidator>>,
    pub chain: Arc<RwLock<HeaderChain>>,
    pub mempool: Arc<Mutex<Mempool>>,
    pub utxo_reader: Arc<SnapshotReader>,
    pub state_context: Arc<RwLock<Option<ErgoStateContext>>>,
}
```

The validator is `UtxoValidator` specifically (not `Box<dyn BlockValidator>`)
because `proofs_for_transactions()` and `emission_box_id()` are
UtxoValidator-specific methods, not on the BlockValidator trait. This means
mining is only available when the node runs in UTXO mode — the main crate
constructs `MiningState` only when it has a `UtxoValidator`, never for
`DigestValidator`.

The validator `Arc<Mutex<>>` is the same one used by the sync pipeline.
Candidate generation briefly locks it for `proofs_for_transactions()`.
The `SnapshotReader` is used for emission box lookups without locking the
prover.

## Required Interface Additions

### UtxoValidator

Two new methods:

1. `proofs_for_transactions(&mut self, txs: &[Transaction]) → Result<(Vec<u8>, ADDigest)>`
   - Save digest → apply state changes → capture new digest + proof → rollback
   - Must not persist changes
   - Implementation depends on ergo_avltree_rust prover API (dependency risk)

2. `emission_box_id(&self) → Option<[u8; 32]>`
   - Track current emission box ID across block applications
   - Match outputs against emission contract ErgoTree after each validate_block()

### Chain crate

- `required_difficulty(&self, parent: &Header) → u64` — next-block nBits
  (may already exist or be derivable from existing difficulty adjustment code)

### ergo-nipopow

- `update_interlinks(parent, extension) → Vec<[u8; 32]>`
- `interlinks_to_extension(interlinks) → Vec<([u8; 2], Vec<u8>)>`
- If not exposed: port from JVM's `NipopowAlgos`

## Implementation Approach: Vertical Slice

Build the minimum valid block first (empty mempool), proving the three
hardest unknowns (prover access, emission tx, header serialization).
Then layer on complexity.

### Phase 1: Crate scaffold + types + interface additions

- `mining/Cargo.toml`, `mining/src/lib.rs`
- Core types: `MinerConfig`, `CandidateBlock`, `WorkMessage`,
  `ProofOfUpcomingTransactions`, `ExtensionCandidate`
- Public struct: `CandidateGenerator`
- Add `proofs_for_transactions()` and `emission_box_id()` to UtxoValidator
- **Dependency check:** read ergo_avltree_rust `PersistentBatchAVLProver` API
  to determine proof generation mechanism (with or without persistence)

### Phase 2: Emission transaction (with EIP-27)

- Look up emission box ID via `validator.emission_box_id()`
- Read box bytes from UTXO state via `SnapshotReader::lookup_key()`
  (read-only, doesn't lock the prover)
- Compute reward: `EmissionRules::miners_reward_at_height(height)`
- Build outputs: new emission box (reduced value) + miner reward box
  (time-locked via `ErgoTreePredef::reward_output_script`)
- EIP-27: re-emission token deduction, injection box on activation height
- **Dependency check:** verify sigma-rust fork (rev 38d38ece) exposes
  EIP-27 re-emission APIs
- **Test:** construct emission tx for a known mainnet height, compare
  output values/scripts against the JVM

### Phase 3: State root computation

- Implement `proofs_for_transactions()` on UtxoValidator
- Compute state changes from `[emission_tx]`
- Apply operations to prover, capture digest + proof, rollback
- **Test:** generate state root for emission-only block, verify against
  known block's `header.state_root`

### Phase 4: Header serialization + WorkMessage

- Serialize `HeaderWithoutPow` matching JVM's
  `HeaderSerializer.bytesWithoutPow()` byte-for-byte
- Field layout: version(1) | parentId(32) | ADProofsRoot(32) |
  stateRoot(33) | transactionsRoot(32) | timestamp(VLQ) | nBits(VLQ) |
  height(VLQ) | extensionRoot(32) | votes(3)
- `msg = Blake2b256(serialized_bytes)`
- `b = decode_compact_bits(n_bits)`
- Extension: interlinks from ergo-nipopow + voting bytes from config
- **Dependency check:** verify ergo-nipopow exposes interlink functions
- **Cross-verification gate:** generate WorkMessage from same chain state
  as JVM node on test server. Compare `msg` byte-for-byte. Must pass
  before proceeding.

### Phase 5: Solution validation + block assembly

- Deserialize `AutolykosSolution` (only `n` required for v2)
- Combine with cached `HeaderWithoutPow` → full `Header`
- Verify PoW: `AutolykosPowScheme::validate(header)`
- Assemble `ErgoFullBlock`: Header + BlockTransactions + Extension + ADProofs
- Submit through normal validation pipeline
- On success: apply block, broadcast to P2P, invalidate candidate

### Phase 6: Transaction selection + fee collection

- Select from `mempool.all_prioritized()` in priority order
- Validate each against accumulated state (UTXO + emission + selected outputs)
  using `validate_single_transaction()` with upcoming state context
- Track cumulative cost/size against `Parameters` limits
- Report failed txs to mempool for elimination
- Fee collection: scan outputs for fee proposition ErgoTree, sum values,
  build fee tx with single miner output
- Re-run `proofs_for_transactions()` with full tx set:
  `[emission, selected..., fee]`

### Phase 7: Candidate caching + API wiring

- `CachedCandidate`: block + WorkMessage + tip height + creation timestamp
- Invalidation: tip change or TTL expiry
- API handlers (in api/ crate): thin wrappers over mining methods
  - `GET /mining/candidate` → `generate_candidate()` or return cached
  - `POST /mining/solution` → `validate_solution()`
  - `GET /mining/rewardAddress` → config lookup
  - All return 503 when mining unconfigured or digest mode
- Main crate: construct `MiningState`, pass to API server

## Dependency Risks

All three are checked in Phases 1-4, before significant code depends on
the answers:

| Risk | When checked | Fallback |
|------|-------------|----------|
| `ergo_avltree_rust` proof generation without persistence | Phase 1 | persist + rollback, or fork addition |
| sigma-rust EIP-27 re-emission APIs at rev 38d38ece | Phase 2 | build emission tx manually from primitives |
| `ergo-nipopow` interlink computation functions | Phase 4 | port from JVM `NipopowAlgos` |

## Key Invariants

- Every `CandidateBlock` + valid PoW solution → block that passes own
  validation pipeline
- `proofs_for_transactions()` never leaves prover in modified state
- Emission tx is always first, fee tx (if present) always last
- Transaction order matches state root computation order
- `WorkMessage.msg` matches JVM `HeaderSerializer.bytesWithoutPow()` byte-for-byte
- 503 = feature unavailable (config/mode), 400 = bad input

## Configuration

```toml
[node.mining]
miner_pk = ""              # hex compressed group element (33 bytes), empty = disabled
votes = "000000"           # 3 bytes hex: [soft_fork, param_1, param_2]
candidate_ttl_secs = 15    # max candidate lifetime before regeneration
reward_delay = 720         # miner reward maturity delay in blocks
```

## Testing Strategy

1. Emission tx against known mainnet block values
2. State root for emission-only block against known header
3. Header serialization byte-for-byte against JVM node
4. Round-trip: generate candidate → forge solution (low difficulty) → submit → block accepted
5. Solution rejection: invalid nonce → 400
6. Candidate caching: two rapid polls return same WorkMessage
7. Candidate invalidation: new block → new candidate
8. Tx selection limits: exceed max_block_cost, verify truncation
9. Fee aggregation: multiple fee outputs → single miner output
10. Digest mode / no PK → 503

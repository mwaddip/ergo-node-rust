# Mining API Contract

## Component: `mining/` (workspace crate, in-repo)

Block template assembly and solution acceptance for external GPU miners.
The mining crate orchestrates mempool, state, chain, and validation
components to produce valid block candidates, and verifies PoW solutions
to publish new blocks. The API crate is a thin HTTP facade over this —
it deserializes requests, calls mining methods, serializes responses.

Miners poll `GET /mining/candidate` for work, compute Autolykos v2 nonces
locally, and submit solutions via `POST /mining/solution`. The node does not
mine — it produces templates and validates solutions.

Requires UTXO mode. Digest-mode nodes cannot compute new state roots and
therefore cannot generate block candidates.

Primary consumers: API crate (HTTP handlers call mining methods), main crate
(wires mining state into API). Primary dependencies: chain (parent header,
difficulty), mempool (transaction selection), validation (state root
computation via UtxoValidator), parameters (protocol rules, block limits).

## SPECIAL Profile

```
S9  P9  E6  C7  I8  A7  L9
```

Consensus-critical assembly (S9) — a malformed candidate wastes miner hashpower;
a malformed block gets rejected by peers and damages reputation. External-facing
(P9) — the solution endpoint accepts data from the network. Architecture (I8) —
the candidate generator coordinates many components; wrong boundaries mean tangled
code. Edge cases (L9) — epoch boundary voting, emission box transitions, empty
mempool, zero-fee blocks, max-cost blocks. Not persistent (E6) — crash means the
current candidate is lost; miners just poll for a new one.

## Design Principles

- **UTXO mode only.** Mining requires computing the new state root after
  applying transactions. This needs the persistent AVL+ prover. Digest-mode
  nodes cannot mine. The `/mining/*` endpoints return 503 if the node is in
  digest mode.
- **Miners poll, node doesn't push.** `GET /mining/candidate` is the pull
  interface. No WebSocket, no push notifications. Miners poll at their preferred
  interval. Candidate caching avoids redundant recomputation.
- **No embedded wallet.** The miner's public key must be configured. Without it,
  the mining endpoints return 503. The node never generates or manages keys.
- **Candidate = cached snapshot.** The candidate is regenerated only when the
  chain tip changes or a configurable interval expires. Polls between
  regenerations return the cached candidate.
- **Components do the work.** The mining crate is mechanical assembly.
  Transaction validation is ergo-lib. State root computation is
  ergo_avltree_rust. PoW verification is ergo-chain-types. Emission rules
  are sigma-rust. The mining crate calls them in the right order.
- **No HTTP.** The mining crate knows nothing about HTTP, JSON, or axum.
  It exposes Rust methods that the API crate's handlers call. Same
  relationship as mempool-to-API: the crate owns the logic, the API owns
  the wire format.

## Dependencies

- `ergo-lib` — `Transaction`, `ErgoBox`, `EmissionRules`, `ErgoTreePredef`,
  `TxBuilder` (coinbase construction)
- `ergo-chain-types` — `Header`, `AutolykosPowScheme`, `AutolykosSolution`,
  `ADDigest`, header serialization
- `ergo-nipopow` — interlink vector computation for extension section
- `ergo-validation` — `compute_state_changes`, `validate_single_transaction`,
  `build_state_context`, `Parameters`
- `ergo_avltree_rust` — via UtxoValidator (temporary prover operations for
  state root computation)
- `blake2` — Blake2b256 for header hash (WorkMessage `msg` field)
- `serde` — serialization of types (JSON encoding is the API crate's job)

Does NOT depend on: `axum`, `serde_json`, P2P transport, sync state machine,
block storage internals, redb. The mining crate is HTTP-agnostic — all wire
format concerns belong to the API crate.

## Core Types

### `MinerConfig`

```rust
pub struct MinerConfig {
    /// Miner's public key (compressed group element, 33 bytes).
    /// Required for mining. Loaded from config at startup.
    pub miner_pk: ProveDlog,
    /// Miner reward delay in blocks (mainnet: 720, testnet: 720).
    /// Reward boxes are time-locked for this many blocks.
    pub reward_delay: i32,
    /// Voting preferences: 3 bytes [soft_fork_vote, param_vote_1, param_vote_2].
    /// [0, 0, 0] = no votes.
    pub votes: [u8; 3],
    /// Maximum block candidate lifetime before forced regeneration.
    pub candidate_ttl: Duration,
    /// EIP-27 re-emission rules. Carried in config (not derived inside
    /// `generate_candidate`) so the network-policy decision lives at the
    /// configuration boundary. Construct from the chain's network type at
    /// config-load time. Currently hardcoded to mainnet at the call sites
    /// in `src/main.rs` until testnet/devnet network detection is wired in.
    pub reemission_rules: ReemissionRules,
}
```

### `CandidateBlock`

```rust
/// All components needed to assemble a full block once a PoW solution arrives.
/// Cached between miner polls. Invalidated when chain tip changes.
pub struct CandidateBlock {
    /// Parent block header.
    pub parent: Header,
    /// Block version.
    pub version: u8,
    /// Encoded difficulty target for this block.
    pub n_bits: u64,
    /// New state root after applying selected transactions.
    pub state_root: ADDigest,
    /// Serialized AD proofs for the state transition.
    pub ad_proof_bytes: Vec<u8>,
    /// Ordered transactions: [emission_tx, mempool_txs..., fee_tx].
    pub transactions: Vec<Transaction>,
    /// Block timestamp: max(now_ms, parent.timestamp + 1).
    pub timestamp: u64,
    /// Extension section (interlinks + voting).
    pub extension: ExtensionCandidate,
    /// Voting bytes (3 bytes).
    pub votes: [u8; 3],
}
```

### `WorkMessage`

```rust
/// Data sent to the miner via GET /mining/candidate.
/// The miner computes a nonce such that pow_hit(msg, nonce, height) < b.
pub struct WorkMessage {
    /// Blake2b256 hash of the serialized HeaderWithoutPow.
    pub msg: [u8; 32],
    /// Target value derived from nBits. Solution must satisfy hit < b.
    pub b: BigInt,
    /// Block height (used in Autolykos v2 index calculation).
    pub h: u32,
    /// Miner's public key.
    pub pk: EcPoint,
    /// Header pre-image and transaction membership proofs.
    /// Allows miners to verify they're working on a real candidate.
    pub proof: ProofOfUpcomingTransactions,
}
```

JSON response:

```json
{
  "msg": "hex string (32 bytes)",
  "b": "decimal string (BigInt)",
  "h": 271235,
  "pk": "hex string (33 bytes compressed)",
  "proof": {
    "msgPreimage": "hex string (serialized header without PoW)",
    "txProofs": []
  }
}
```

### `ProofOfUpcomingTransactions`

```rust
pub struct ProofOfUpcomingTransactions {
    /// The full serialized header without PoW fields — the bytes that hash to `msg`.
    /// Miners can verify: Blake2b256(msg_preimage) == msg.
    pub msg_preimage: Vec<u8>,
    /// Merkle inclusion proofs for mandatory transactions.
    /// Empty for first release — full implementation deferred.
    pub tx_proofs: Vec<TransactionMembershipProof>,
}
```

### `ExtensionCandidate`

```rust
/// Key-value pairs for the block extension section.
pub struct ExtensionCandidate {
    /// Extension fields as (key, value) pairs.
    /// Keys: 2 bytes. Values: variable length.
    pub fields: Vec<([u8; 2], Vec<u8>)>,
}

impl ExtensionCandidate {
    /// Merkle root digest of extension fields (for header.extension_root).
    pub fn digest(&self) -> [u8; 32];
}
```

## Mining State

The mining module maintains its own state, separate from `ApiState`.
Passed to mining handlers alongside ApiState.

```rust
pub struct MiningState {
    /// Miner configuration (PK, reward delay, votes, candidate TTL).
    pub config: MinerConfig,
    /// The current cached candidate. None if not yet generated or invalidated.
    pub candidate: RwLock<Option<CachedCandidate>>,
    /// Access to the UTXO validator for state root computation.
    /// Same validator used by the sync pipeline. UtxoValidator specifically
    /// (not Box<dyn BlockValidator>) because proofs_for_transactions() and
    /// emission_box_id() are UtxoValidator-specific methods.
    pub validator: Arc<Mutex<UtxoValidator>>,
    /// Read access to header chain (tip, difficulty).
    pub chain: Arc<RwLock<HeaderChain>>,
    /// Read/write access to mempool (tx selection).
    pub mempool: Arc<Mutex<Mempool>>,
    /// Read access to UTXO state (emission box lookup without locking prover).
    pub utxo_reader: Arc<SnapshotReader>,
    /// Current blockchain state context (rebuilt on each validated block).
    pub state_context: Arc<RwLock<Option<ErgoStateContext>>>,
}

struct CachedCandidate {
    /// The assembled candidate block.
    block: CandidateBlock,
    /// The WorkMessage derived from the candidate.
    work: WorkMessage,
    /// Chain tip height when this candidate was generated.
    tip_height: u32,
    /// When this candidate was generated.
    created: Instant,
}
```

### Validator Access

`MiningState.validator` is `Arc<Mutex<UtxoValidator>>` — the same instance
used by the sync pipeline. Mining is only available in UTXO mode; the main
crate constructs `MiningState` only when it has a `UtxoValidator`, never
for `DigestValidator`. Candidate generation acquires the lock briefly for
`proofs_for_transactions()`. During this window, block validation is blocked.

For first release, this contention is acceptable. Candidate generation
is infrequent (on tip change or TTL expiry) and proof computation is fast
(proportional to selected transaction count, not UTXO set size). If
contention becomes an issue, the solution is a dedicated read-only prover
sharing the storage backend.

## Candidate Assembly

### Overview

```
chain tip changes (or candidate TTL expires)
  │
  ▼
1. Get parent header + parameters ───── chain, validator
  │
  ▼
2. Build emission transaction ───────── EmissionRules, UTXO state
  │
  ▼
3. Select mempool transactions ──────── mempool.all_prioritized(), validate each
  │
  ▼
4. Build fee transaction ────────────── aggregate fees from selected txs
  │
  ▼
5. Compute state root ───────────────── validator.proofs_for_transactions()
  │
  ▼
6. Build extension section ──────────── interlinks, voting
  │
  ▼
7. Assemble CandidateBlock
  │
  ▼
8. Derive WorkMessage ───────────────── serialize header, Blake2b256 hash
  │
  ▼
9. Cache candidate
```

### Step 1: Parent Header + Parameters

- Read chain tip header from `chain` handle (latest validated header).
- Read current `Parameters` from `validator.parameters()`.
- Compute nBits for the new block from the chain's difficulty adjustment.
  Uses the difficulty adjustment algorithm already implemented in the
  `chain` crate.
- Compute height: `parent.height + 1`.
- Compute timestamp: `max(system_time_ms(), parent.timestamp + 1)`.

### Step 2: Emission Transaction

Every block includes a coinbase transaction that distributes the block
reward from the emission contract. This is the first transaction in the
block.

1. **Find the emission box** in the UTXO state. The emission box is
   identified by its ErgoTree (the emission contract script). The validator
   tracks the current emission box ID as it changes each block (see
   Required Interface Additions).

2. **Compute block reward** using
   `EmissionRules::miners_reward_at_height(height)`. Returns the ERG
   amount the miner receives at this height.

3. **Build the emission transaction:**
   - **Input:** the current emission box (with empty prover result — the
     emission contract is self-validating)
   - **Output 1:** new emission box with value reduced by `reward_amount`,
     same ErgoTree script, same creation height
   - **Output 2:** miner reward box protected by
     `ErgoTreePredef::reward_output_script(reward_delay, miner_pk)`,
     containing `reward_amount` nanoERG

   Reference: JVM `CandidateGenerator.collectRewards()`.

4. **EIP-27 re-emission** (active on mainnet since height 777,217):
   - When re-emission is active at this height, the emission transaction
     includes re-emission token operations and potentially an injection box
   - Reference: JVM `CandidateGenerator.collectRewards()` lines 744-792
   - Required for mainnet. Can be deferred if targeting testnet-only for
     first release.

**If no emission box exists** (all ERG emitted): skip the emission
transaction. The block contains only mempool transactions and the fee
transaction.

### Step 3: Transaction Selection

Select transactions from the mempool to include in the block, respecting
protocol limits.

1. Read protocol limits from `Parameters`:
   - `max_block_cost` — maximum cumulative ErgoScript evaluation cost
   - `max_block_size` — maximum serialized block size in bytes

2. Build the state context for the upcoming block:
   ```rust
   let upcoming_context = build_state_context(
       &upcoming_header_stub,  // height+1, new timestamp
       &preceding_headers,
       &parameters,
   );
   ```

3. Get prioritized transactions from mempool: `mempool.all_prioritized()`.

4. For each candidate transaction (in priority order):
   a. Check cumulative cost: if adding this tx exceeds `max_block_cost`,
      skip.
   b. Check cumulative size: if adding this tx exceeds `max_block_size`,
      skip.
   c. Validate the transaction against the "accumulated state" — the UTXO
      set augmented with outputs from the emission tx and already-selected
      transactions. Use `validate_single_transaction()` with the upcoming
      state context.
   d. If validation fails: record tx ID for mempool elimination, skip.
   e. If valid: add to selected set, accumulate cost and size.

5. Stop when limits are reached or all mempool transactions are checked.

6. Report invalid transaction IDs back to the mempool for cleanup.

**Edge case — empty mempool:** The block contains only the emission
transaction and fee transaction (zero-fee). This is valid.

### Step 4: Fee Transaction

Collect fees from all selected transactions and create a single fee output
for the miner.

1. **Identify fee outputs:** Scan all outputs of selected transactions for
   boxes whose ErgoTree matches the fee proposition
   (`ErgoTreePredef::fee_proposition()`). Exclude any that are spent by
   other selected transactions within the same block.

2. **Sum fee values:**
   `total_fee = sum(fee_box.value for fee_box in fee_boxes)`.

3. If `total_fee > 0`:
   - **Inputs:** all identified fee boxes (with empty prover results)
   - **Output:** single miner box with value = `total_fee`, protected by
     `ErgoTreePredef::reward_output_script(reward_delay, miner_pk)`
   - Append the fee transaction to the transaction list (after mempool txs)

4. If `total_fee == 0`: no fee transaction needed.

Reference: JVM `CandidateGenerator.collectFees()`.

### Step 5: State Root Computation

Compute the new UTXO state root and AD proofs for the complete transaction
set. This is the expensive step — it temporarily modifies the prover state.

1. Acquire the validator lock.
2. Call `validator.proofs_for_transactions(&all_transactions)` where
   `all_transactions = [emission_tx] ++ selected_txs ++ [fee_tx]`.
3. Receive `(ad_proof_bytes, new_state_root)`.
4. Release the validator lock.

The `proofs_for_transactions()` method (see Required Interface Additions):
- Computes state changes from the transactions (reusing
  `compute_state_changes()`)
- Applies operations to the prover's in-memory tree
- Captures the new digest (state root)
- Generates the AD proof bytes
- Rolls back the prover to its original state (no persistence)

### Step 6: Extension Section

Build the extension with NiPoPoW interlinks and voting data.

1. **Interlinks:** Compute updated interlink vector from the parent header
   and parent extension. Uses `ergo-nipopow`:
   ```rust
   let interlinks = update_interlinks(parent_header, parent_extension);
   let interlink_fields = interlinks_to_extension(interlinks);
   ```

2. **Voting:** The node's voting preferences from `MinerConfig.votes`:
   - Byte 0: soft-fork vote ID (0 = no vote)
   - Byte 1: parameter change vote 1 (0 = no vote)
   - Byte 2: parameter change vote 2 (0 = no vote)

3. **Epoch boundary** (height % voting_epoch_length == 0):
   - Compute new parameters from accumulated votes
   - Include parameter update fields in extension
   - Include updated validation settings
   - Reference: JVM `CandidateGenerator.createCandidate()` lines 533-569

4. **Assemble:** Merge interlink fields + voting/parameter fields into
   `ExtensionCandidate`.

### Step 7: Assemble CandidateBlock

```rust
CandidateBlock {
    parent,
    version: current_version(parameters),
    n_bits,
    state_root: new_state_root,
    ad_proof_bytes,
    transactions: all_transactions,
    timestamp,
    extension,
    votes,
}
```

### Step 8: Derive WorkMessage

Convert the candidate into the data miners need.

1. **Build HeaderWithoutPow.** Field order, sizes and encoding must match
   JVM `HeaderSerializer.serializeWithoutPow` exactly:

   | # | Field | Size | Encoding | Source |
   |---|-------|------|----------|--------|
   | 1 | version | 1 byte | raw `u8` | `candidate.version` |
   | 2 | parentId | 32 bytes | raw bytes | `candidate.parent.id` |
   | 3 | ADProofsRoot | 32 bytes | raw bytes | `Blake2b256(candidate.ad_proof_bytes)` |
   | 4 | transactionsRoot | 32 bytes | raw bytes | Merkle root of transaction IDs |
   | 5 | stateRoot | 33 bytes | raw bytes (32-byte hash + 1-byte tree height) | `candidate.state_root` |
   | 6 | timestamp | variable | VLQ `u64` | `candidate.timestamp` |
   | 7 | extensionRoot | 32 bytes | raw bytes | `candidate.extension.digest()` |
   | 8 | nBits | 4 bytes | fixed-width big-endian `u32` (NOT VLQ) | `candidate.n_bits` |
   | 9 | height | variable | VLQ `u32` | `candidate.parent.height + 1` |
   | 10 | votes | 3 bytes | raw bytes | `candidate.votes` |
   | 11 | unparsedBytes (only if version > 1) | 1 byte length + N bytes | raw `u8` length prefix + raw bytes | empty for first release |

   **Consensus-critical.** This serialization must match JVM
   `HeaderSerializer.bytesWithoutPow` byte-for-byte. Any divergence means
   miners compute solutions for the wrong message and blocks are rejected
   by the network. The serializer itself is delegated to sigma-rust's
   `Header::serialize_without_pow`, which is JVM-pinned upstream against
   the height-614,400 mainnet header (see
   `ergo-chain-types::autolykos_pow_scheme::tests::test_first_increase_in_big_n`).
   The mining crate's responsibility is correctly mapping `CandidateBlock`
   fields into the `Header` struct before handing it to the encoder; that
   wiring is verified by `mining/tests/work_message_wiring.rs`.

   **Note on `nBits`:** despite Scorex's general rule that integers are
   VLQ-encoded, `nBits` is the one exception in the header — it's written
   as 4 raw big-endian bytes via JVM `DifficultySerializer.serialize`, and
   sigma-rust matches this with `n_bits.to_be_bytes()`. Earlier versions
   of this contract incorrectly described `nBits` as VLQ; the code was
   always correct.

2. **Serialize** the header without PoW fields.

3. **Hash:** `msg = Blake2b256(serialized_header_without_pow)`.

4. **Target:** `b = decode_compact_bits(candidate.n_bits)`.

5. **Assemble WorkMessage:**
   ```rust
   WorkMessage {
       msg,
       b,
       h: candidate.parent.height + 1,
       pk: miner_config.miner_pk,
       proof: ProofOfUpcomingTransactions {
           msg_preimage: serialized_header_without_pow,
           tx_proofs: vec![],  // Empty for first release
       },
   }
   ```

### Candidate Caching

The candidate is cached and served to multiple miner polls:

- **Invalidation triggers:**
  - Chain tip changes (new block validated) → regenerate
  - Candidate TTL expires → regenerate with fresh mempool snapshot
  - Node shutdown → discard

- **On GET /mining/candidate:**
  1. If cached candidate exists and `tip_height == current_tip` and
     `created + candidate_ttl > now`: return cached WorkMessage.
  2. Otherwise: regenerate candidate (steps 1-8), cache, return.

- **No push invalidation from mempool.** New transactions don't immediately
  invalidate the cached candidate. The TTL-based refresh picks them up.
  This is simpler and matches the JVM's behavior with its
  `blockCandidateGenerationInterval`.

## Solution Submission

### POST /mining/solution

1. **Deserialize** the `AutolykosSolution` from JSON:
   ```json
   { "pk": "hex", "w": "hex", "n": "hex (8 bytes)", "d": "decimal string" }
   ```
   For Autolykos v2, `pk`, `w`, and `d` are optional (defaults apply):
   - `pk`: identity point
   - `w`: generator point
   - `d`: 0
   The only required field is `n` (the 8-byte nonce).

2. **Check cached candidate:** If no candidate is cached, return 400
   ("no current candidate").

3. **Assemble header:** Combine the `HeaderWithoutPow` from the cached
   candidate with the submitted solution to produce a full `Header`.

4. **Verify PoW:** `AutolykosPowScheme::validate(header)`.
   - Compute `hit = pow_hit(header)` using the Autolykos v2 algorithm
   - Verify `hit < target` where `target = decode_compact_bits(n_bits)`
   - On failure: return 400 "invalid PoW solution"

5. **Assemble full block:**
   ```
   Header (from step 3)
   BlockTransactions { header_id, version, transactions }
   Extension { header_id, fields }
   ADProofs { header_id, proof_bytes }
   ```
   The `header_id` is derived from the full header (including PoW solution).

6. **Validate block:** Submit to the normal validation pipeline. The
   assembled block must pass the same validation as any block received
   from a peer. If validation fails, there's a bug in candidate assembly
   — return 500 and log the error.

7. **Apply block:** The validator applies the block, advancing state.

8. **Broadcast to P2P:** Send the full block to connected peers as
   separate modifier messages:
   - Header (type 101)
   - BlockTransactions (type 102)
   - ADProofs (type 104)
   - Extension (type 108)

9. **Invalidate candidate:** Clear the cached candidate (chain tip
   changed — step 7 advanced it).

10. **Return 200** on success.

## Endpoints

Detailed HTTP specification is in `api.md`. This section specifies the
mining-specific behavior that the API handlers invoke.

### `GET /mining/candidate`

No request body. Returns `WorkMessage` JSON.

Returns 503 if:
- Mining not configured (no miner PK in config)
- Node is in digest mode (cannot compute state roots)
- Node is still syncing (validator height far below chain tip)

### `POST /mining/solution`

Requires authentication if `api_key_hash` is configured.
Request body: `AutolykosSolution` JSON.

Returns 200 on success (block accepted and broadcast).
Returns 400 on invalid solution, no cached candidate, or stale candidate.

### `GET /mining/rewardAddress`

Returns the P2S address derived from the configured miner PK.
Simple config lookup, no state access.

```json
{ "rewardAddress": "3WwbzW..." }
```

Returns 503 if mining not configured.

### Deferred Endpoints (not first release)

| Endpoint | Reason to defer |
|---|---|
| `POST /mining/candidateWithTxs` | Pool operator feature; basic GET is sufficient |
| `POST /mining/candidateWithTxsAndPk` | Multi-miner pool feature |

## Required Interface Additions

### UtxoValidator: `proofs_for_transactions()`

New method on `UtxoValidator`:

```rust
impl UtxoValidator {
    /// Compute AD proofs and new state root for a set of transactions.
    ///
    /// Operates on the in-memory prover state WITHOUT persisting changes.
    /// The prover is rolled back to its original state after computation.
    ///
    /// Preconditions:
    ///   - Transactions are valid and ordered: [emission, mempool..., fee]
    ///   - The prover is at the current validated state
    ///
    /// Postconditions on Ok:
    ///   - Returns (serialized_ad_proof, new_state_digest)
    ///   - The prover state is unchanged (rolled back)
    ///
    /// Postconditions on Err:
    ///   - The prover state is unchanged (rolled back on best effort)
    pub fn proofs_for_transactions(
        &mut self,
        txs: &[Transaction],
    ) -> Result<(Vec<u8>, ADDigest), ValidationError>;
}
```

**Implementation approach:**
1. Save current digest: `let saved = self.prover.digest()`
2. Compute state changes: `compute_state_changes(transactions_to_summaries(txs))`
3. Build AVL operations (Lookups, Removes, Inserts — same as `validate_block`)
4. Apply operations: `self.prover.perform_one_operation(op)` for each
5. Capture new digest: `let new_root = self.prover.digest()`
6. Generate proof — use inner `BatchAVLProver::generate_proof()` if
   accessible without persistence, or call
   `generate_proof_and_update_storage()` followed by rollback (wasteful but
   correct)
7. Rollback: `self.prover.rollback(&saved)`
8. Return `(proof_bytes, new_root)`

**Note:** The exact mechanism for step 6 depends on what
`ergo_avltree_rust`'s `PersistentBatchAVLProver` exposes. If
`generate_proof()` is not accessible without persistence, the fallback is
persist + rollback. This may warrant a small addition to the avltree fork.

### UtxoValidator: `emission_box_id()`

Track the current emission box ID across block applications:

```rust
impl UtxoValidator {
    /// Current emission box ID in the UTXO state.
    /// Updated after each block validation by scanning state changes
    /// for the emission contract ErgoTree.
    /// None if all ERG has been emitted.
    pub fn emission_box_id(&self) -> Option<[u8; 32]>;
}
```

The emission box ID changes every block (the old one is spent, a new one
is created). The validator already processes all state changes — it can
track which output box matches the emission contract ErgoTree.

### Chain: difficulty for next block

The chain crate already computes difficulty adjustment. Expose the
next-block nBits:

```rust
/// Compute the required nBits (encoded difficulty) for a block
/// following `parent`.
fn required_difficulty(&self, parent: &Header) -> u64;
```

### NiPoPoW: interlink computation

From `ergo-nipopow`:

```rust
/// Compute updated interlinks from the parent header and current extension.
fn update_interlinks(parent: &Header, extension: &Extension) -> Vec<[u8; 32]>;

/// Encode interlinks as extension key-value pairs.
fn interlinks_to_extension(interlinks: &[[u8; 32]]) -> Vec<([u8; 2], Vec<u8>)>;
```

Verify that `ergo-nipopow` exposes these. If not, port from JVM's
`NipopowAlgos.updateInterlinks()` and `interlinksToExtension()`.

## Configuration

```toml
[node.mining]
# Miner public key (hex-encoded compressed group element, 33 bytes).
# Required to enable mining endpoints. Empty = mining disabled.
miner_pk = ""

# Voting preferences: 3 bytes as hex string. "000000" = no votes.
# Byte 0: soft-fork vote ID. Bytes 1-2: parameter change votes.
votes = "000000"

# Maximum candidate lifetime before forced regeneration (seconds).
candidate_ttl_secs = 15

# Miner reward maturity delay in blocks (protocol default: 720).
# Reward boxes are locked for this many blocks after mining.
reward_delay = 720
```

Mining is disabled when `miner_pk` is empty. The `/mining/*` endpoints
return 503 with `"reason": "mining not configured"`.

## Does NOT Own

- ErgoScript evaluation — that's `ergo-lib` via `validate_single_transaction()`
- UTXO state management — that's `enr-state` via the UtxoValidator
- PoW algorithm — that's `ergo-chain-types` (`AutolykosPowScheme`)
- Emission schedule — that's `ergo-lib` (`EmissionRules`)
- Difficulty adjustment — that's the `chain` crate
- Interlink computation — that's `ergo-nipopow`
- P2P block broadcast — that's the main crate's P2P task
- Block storage — that's `enr-store`
- Transaction validation — that's `ergo-validation`
- Mempool management — that's the mempool crate
- Header serialization — that's `ergo-chain-types` (must match JVM exactly)
- HTTP endpoints / JSON wire format — that's the API crate
- Key management — that's the user (configured in toml, not generated)

## Invariants

- Every `CandidateBlock`, if paired with a valid PoW solution, produces a
  block that passes the node's own validation pipeline.
- The cached candidate is invalidated whenever the chain tip advances.
- `proofs_for_transactions()` never leaves the prover in a modified state.
- The emission transaction is always the first transaction in the block.
- The fee transaction (if present) is always the last transaction.
- Transaction order in the block matches the order used for state root
  computation. Reordering transactions changes the state root.
- `WorkMessage.msg` is the Blake2b256 hash of the header serialized without
  PoW fields, using the exact same byte format as the JVM node's
  `HeaderSerializer.bytesWithoutPow()`. Byte-level divergence means miners
  produce invalid solutions.
- A 503 from any mining endpoint means the feature is unavailable (config
  or mode issue), not a transient error. The client should not retry.
- A 400 from `POST /mining/solution` means the solution is invalid or the
  candidate is stale. The miner should fetch a new candidate.

## Testing Strategy

1. **Candidate assembly — empty mempool:** Generate a candidate with only
   the emission transaction. Verify state root is correct, AD proofs verify,
   WorkMessage fields are populated.

2. **Candidate assembly — with transactions:** Add known transactions to
   mempool, generate candidate, verify selected transactions appear in
   priority order.

3. **State root verification:** Generate candidate, apply the candidate's
   transactions to a fresh prover, verify the resulting digest matches
   `candidate.state_root`.

4. **Round-trip:** Generate candidate, construct a valid header with a known
   nonce that satisfies the difficulty (use trivially low difficulty for
   testing), submit via `POST /mining/solution`, verify block is accepted.

5. **Solution rejection:** Submit an invalid nonce, verify 400 response
   with clear error message.

6. **Candidate caching:** Poll `/mining/candidate` twice rapidly, verify
   same WorkMessage returned (no regeneration).

7. **Candidate invalidation:** Generate candidate, apply a new block
   (advancing chain tip), poll `/mining/candidate`, verify new candidate
   with updated parent.

8. **Transaction selection limits:** Fill mempool with transactions
   exceeding `max_block_cost`. Verify candidate includes transactions up
   to the limit and no more.

9. **Fee collection:** Include transactions with fee outputs, verify the
   fee transaction aggregates all fees into a single miner output.

10. **Emission transaction:** Verify emission box is spent, new emission
    box has correct reduced value, miner reward box has correct amount and
    time-lock script.

11. **Extension section:** Verify interlinks are computed from parent,
    voting bytes are included. Epoch boundary produces parameter updates.

12. **JVM compatibility:** Generate a candidate from the same chain state
    as a JVM node. Compare `WorkMessage.msg` byte-for-byte. This is the
    ultimate correctness test — if `msg` matches, the header serialization
    is correct.

13. **Digest mode rejection:** Start node in digest mode, verify
    `/mining/candidate` returns 503.

14. **No miner PK:** Start with empty `miner_pk` config, verify 503.

15. **Prover rollback:** After `proofs_for_transactions()`, verify the
    validator's prover digest is identical to before the call.

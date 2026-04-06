# Mining Crate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Standalone `mining/` workspace crate that assembles block candidates for external GPU miners and validates PoW solutions. Mainnet-ready with EIP-27 re-emission.

**Architecture:** Vertical slice approach — build the minimum valid block first (empty mempool), proving the hardest unknowns (prover access, emission tx, header serialization), then layer on transaction selection, fee collection, and caching. The mining crate owns all logic; the API crate is a thin HTTP facade.

**Tech Stack:** ergo-lib (Transaction, EmissionRules, ErgoTreePredef), ergo-chain-types (Header, AutolykosPowScheme, AutolykosSolution), ergo-nipopow (interlinks), ergo_avltree_rust (BatchAVLProver::generate_proof_for_operations), ergo-merkle-tree (transaction root), blake2 (header hash).

**Spec:** `docs/superpowers/specs/2026-04-06-mining-crate-design.md`
**Contract:** `facts/mining.md`

---

## File Structure

**Create:**

| File | Responsibility |
|------|---------------|
| `mining/Cargo.toml` | Crate manifest — depends on ergo-lib, ergo-chain-types, ergo-nipopow, ergo-validation, ergo-merkle-tree, blake2 |
| `mining/src/lib.rs` | Public API: `CandidateGenerator`, re-exports from submodules |
| `mining/src/types.rs` | `MinerConfig`, `CandidateBlock`, `WorkMessage`, `CachedCandidate`, `ExtensionCandidate` |
| `mining/src/emission.rs` | `ReemissionRules`, `build_emission_tx()` — EIP-27 aware |
| `mining/src/fee.rs` | `build_fee_tx()` — aggregate fee proposition outputs |
| `mining/src/extension.rs` | `build_extension()` — NiPoPoW interlinks + voting |
| `mining/src/solution.rs` | `validate_solution()`, `assemble_block()` |

**Modify:**

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add `mining` to workspace members |
| `validation/src/lib.rs` | Export `compute_state_changes`, `TxSummary`, add `transactions_to_summaries` for `&[Transaction]` |
| `validation/src/state_changes.rs` | Add `pub fn transaction_to_summary(tx: &Transaction) -> Result<TxSummary>` |
| `validation/src/utxo.rs` | Add `proofs_for_transactions()`, `emission_box_id()` |
| `validation/Cargo.toml` | Add ergo-merkle-tree if needed for transaction root |
| `api/src/handlers.rs` | Add mining endpoint handlers |
| `api/src/lib.rs` | Add mining routes, `MiningState` to shared state |
| `api/Cargo.toml` | Add `mining` dependency |
| `src/main.rs` | Construct `MiningState`, wire to API, delegate validator mining methods |

---

### Task 1: Mining crate scaffold + core types

**Files:**
- Create: `mining/Cargo.toml`
- Create: `mining/src/lib.rs`
- Create: `mining/src/types.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Create mining/Cargo.toml**

```toml
[package]
name = "ergo-mining"
version = "0.1.0"
edition = "2021"

[dependencies]
ergo-lib = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "38d38ec", features = ["json"] }
ergo-chain-types = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "38d38ec" }
ergo-nipopow = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "38d38ec" }
ergo-merkle-tree = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "38d38ec" }
ergo-validation = { path = "../validation" }
blake2 = "0.10"
hex = "0.4"
num-bigint = "0.4"
serde = { version = "1", features = ["derive"] }
thiserror = "1"
tracing = "0.1"

[dev-dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Note: verify the exact sigma-rust git rev matches the workspace. Check `Cargo.toml` root or `validation/Cargo.toml` for the current rev hash.

- [ ] **Step 2: Create mining/src/types.rs**

```rust
use std::time::{Duration, Instant};

use ergo_chain_types::{ADDigest, Header};
use ergo_lib::chain::ergo_state_context::ErgoStateContext;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::sigma_protocol::dlog_group::EcPoint;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use num_bigint::BigUint;
use serde::Serialize;

/// Miner configuration loaded from node config.
pub struct MinerConfig {
    /// Miner's public key (required for mining).
    pub miner_pk: ProveDlog,
    /// Miner reward maturity delay in blocks (720 on mainnet/testnet).
    pub reward_delay: i32,
    /// Voting preferences: 3 bytes [soft_fork, param_1, param_2].
    pub votes: [u8; 3],
    /// Maximum candidate lifetime before forced regeneration.
    pub candidate_ttl: Duration,
}

/// Extension section key-value pairs for a new block.
pub struct ExtensionCandidate {
    /// Fields as (2-byte key, variable-length value).
    pub fields: Vec<([u8; 2], Vec<u8>)>,
}

/// All components needed to assemble a full block once a PoW solution arrives.
pub struct CandidateBlock {
    /// Parent block header.
    pub parent: Header,
    /// Block version.
    pub version: u8,
    /// Encoded difficulty target (compact bits).
    pub n_bits: u32,
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
    /// Serialized header-without-PoW bytes (cached for WorkMessage).
    pub header_bytes: Vec<u8>,
}

/// Data sent to the miner. The miner finds nonce n such that pow_hit(msg, n, h) < b.
#[derive(Serialize)]
pub struct WorkMessage {
    /// Blake2b256(serialized HeaderWithoutPow) — hex-encoded.
    pub msg: String,
    /// Target value from nBits — decimal string.
    pub b: String,
    /// Block height.
    pub h: u32,
    /// Miner public key — hex-encoded compressed point.
    pub pk: String,
    /// Header pre-image for miner verification.
    pub proof: ProofOfUpcomingTransactions,
}

#[derive(Serialize)]
pub struct ProofOfUpcomingTransactions {
    /// Serialized header-without-PoW — hex-encoded.
    #[serde(rename = "msgPreimage")]
    pub msg_preimage: String,
    /// Merkle proofs for mandatory txs (empty for first release).
    #[serde(rename = "txProofs")]
    pub tx_proofs: Vec<()>,
}

/// Cached candidate with metadata for invalidation.
pub struct CachedCandidate {
    pub block: CandidateBlock,
    pub work: WorkMessage,
    pub tip_height: u32,
    pub created: Instant,
}
```

- [ ] **Step 3: Create mining/src/lib.rs**

```rust
pub mod emission;
pub mod extension;
pub mod fee;
pub mod solution;
pub mod types;

pub use types::*;

use ergo_chain_types::ADDigest;
use ergo_lib::chain::transaction::Transaction;

/// Errors from mining operations.
#[derive(Debug, thiserror::Error)]
pub enum MiningError {
    #[error("mining not available: {0}")]
    Unavailable(String),

    #[error("no cached candidate")]
    NoCachedCandidate,

    #[error("candidate is stale (tip changed)")]
    StaleCandidate,

    #[error("invalid PoW solution: {0}")]
    InvalidSolution(String),

    #[error("block assembly failed: {0}")]
    AssemblyFailed(String),

    #[error("validation error: {0}")]
    Validation(#[from] ergo_validation::ValidationError),

    #[error("emission error: {0}")]
    Emission(String),

    #[error("state root computation failed: {0}")]
    StateRoot(String),
}
```

- [ ] **Step 4: Add mining to workspace**

In the root `Cargo.toml`, add `"mining"` to the workspace members list. Check the current members first — they should include validation, mempool, sync, api, etc.

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p ergo-mining`
Expected: compiles with no errors (types only, no logic yet).

- [ ] **Step 6: Commit**

```
feat(mining): scaffold mining crate with core types
```

---

### Task 2: BlockValidator trait additions + UtxoValidator proofs_for_transactions

**Files:**
- Modify: `validation/src/lib.rs`
- Modify: `validation/src/state_changes.rs`
- Modify: `validation/src/utxo.rs`

This task adds `proofs_for_transactions()` to the `BlockValidator` trait with a default returning `None`, and implements it on `UtxoValidator` using `BatchAVLProver::generate_proof_for_operations()`.

Key discovery: the inner `BatchAVLProver` has `generate_proof_for_operations(&self, ops) → Result<(SerializedAdProof, ADDigest)>` which creates a temporary prover, applies operations, and returns the result WITHOUT modifying the original. No persist, no rollback.

- [ ] **Step 1: Add transaction_to_summary in state_changes.rs**

The existing `transactions_to_summaries` takes parsed raw bytes. Mining has `Transaction` objects from ergo-lib. Add a conversion function.

In `validation/src/state_changes.rs`, add:

```rust
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

/// Convert ergo-lib Transaction objects to TxSummary for state change computation.
/// Used by the mining crate for candidate assembly.
pub fn transactions_to_summaries_from_lib(
    txs: &[Transaction],
) -> Result<Vec<TxSummary>, ValidationError> {
    let mut summaries = Vec::with_capacity(txs.len());
    for tx in txs {
        let inputs: Vec<[u8; 32]> = tx
            .inputs
            .iter()
            .map(|input| {
                let bytes: Vec<u8> = input.box_id.into();
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                arr
            })
            .collect();

        let data_inputs: Vec<[u8; 32]> = tx
            .data_inputs
            .as_ref()
            .map(|dis| {
                dis.iter()
                    .map(|di| {
                        let bytes: Vec<u8> = di.box_id.into();
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&bytes);
                        arr
                    })
                    .collect()
            })
            .unwrap_or_default();

        let outputs: Vec<([u8; 32], Vec<u8>)> = tx
            .outputs
            .iter()
            .map(|output| {
                let id_bytes: Vec<u8> = output.box_id().into();
                let mut id = [0u8; 32];
                id.copy_from_slice(&id_bytes);
                let value = output.sigma_serialize_bytes()
                    .map_err(|e| ValidationError::SectionParse {
                        section_type: 102,
                        reason: format!("output serialization failed: {e}"),
                    })?;
                Ok((id, value))
            })
            .collect::<Result<Vec<_>, ValidationError>>()?;

        summaries.push(TxSummary {
            inputs,
            data_inputs,
            outputs,
        });
    }
    Ok(summaries)
}
```

Note: verify the exact conversion from `BoxId` to `[u8; 32]`. `BoxId` wraps `Digest32` which wraps `[u8; 32]`. The `Into<Vec<u8>>` conversion should exist. If not, use `.0.0` to access the inner array.

- [ ] **Step 2: Export new functions from validation/src/lib.rs**

Add to the exports in `validation/src/lib.rs`:

```rust
pub use state_changes::{compute_state_changes, TxSummary, transactions_to_summaries_from_lib};
```

- [ ] **Step 3: Add proofs_for_transactions to BlockValidator trait**

In `validation/src/lib.rs`, add to the `BlockValidator` trait:

```rust
    /// Compute AD proofs and new state root for a set of transactions
    /// without modifying persistent state. Returns None for digest-mode
    /// validators (mining requires UTXO mode).
    fn proofs_for_transactions(
        &self,
        txs: &[Transaction],
    ) -> Option<Result<(Vec<u8>, ADDigest), ValidationError>> {
        let _ = txs;
        None
    }
```

- [ ] **Step 4: Implement proofs_for_transactions on UtxoValidator**

In `validation/src/utxo.rs`, add to the `BlockValidator` impl:

```rust
    fn proofs_for_transactions(
        &self,
        txs: &[Transaction],
    ) -> Option<Result<(Vec<u8>, ADDigest), ValidationError>> {
        Some(self.compute_proofs(txs))
    }
```

And add a private helper method on `UtxoValidator`:

```rust
impl UtxoValidator {
    /// Internal: compute proofs using BatchAVLProver::generate_proof_for_operations.
    /// This creates a temporary prover copy — the real prover is not modified.
    fn compute_proofs(
        &self,
        txs: &[Transaction],
    ) -> Result<(Vec<u8>, ADDigest), ValidationError> {
        use crate::state_changes::{compute_state_changes, transactions_to_summaries_from_lib};

        // 1. Convert transactions to state changes
        let summaries = transactions_to_summaries_from_lib(txs)?;
        let changes = compute_state_changes(summaries)?;

        // 2. Build AVL operations (same order as validate_block)
        let mut operations: Vec<Operation> = Vec::new();
        for lookup_id in &changes.lookups {
            operations.push(Operation::Lookup(Bytes::copy_from_slice(lookup_id)));
        }
        for removal_id in &changes.removals {
            operations.push(Operation::Remove(Bytes::copy_from_slice(removal_id)));
        }
        for (insert_id, insert_value) in &changes.insertions {
            operations.push(Operation::Insert(KeyValue {
                key: Bytes::copy_from_slice(insert_id),
                value: Bytes::copy_from_slice(insert_value),
            }));
        }

        // 3. Generate proof WITHOUT modifying state.
        //    generate_proof_for_operations creates a temp prover internally.
        let (proof_bytes, new_digest) = self
            .prover
            .prover
            .generate_proof_for_operations(&operations)
            .map_err(|e| {
                ValidationError::StateOperationFailed(format!(
                    "proof generation for mining failed: {e}"
                ))
            })?;

        let ad_digest = bytes_to_ad_digest(&new_digest);
        Ok((proof_bytes.to_vec(), ad_digest))
    }
}
```

Note: `self.prover` is `PersistentBatchAVLProver`, `self.prover.prover` is the inner `BatchAVLProver` (pub field). `generate_proof_for_operations` takes `&self` so no mutable borrow is needed. Verify this compiles — if the field isn't accessible, expose it via a getter on PersistentBatchAVLProver.

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p ergo-validation`
Expected: compiles. If `generate_proof_for_operations` isn't accessible, check the actual `PersistentBatchAVLProver` API and adjust.

- [ ] **Step 6: Commit**

```
feat(validation): add proofs_for_transactions for mining candidate assembly
```

---

### Task 3: Emission box ID tracking in UtxoValidator

**Files:**
- Modify: `validation/src/utxo.rs`
- Modify: `validation/src/lib.rs`

The emission box ID changes every block (spent and recreated). The validator already processes all state changes — track which new output matches the emission contract ErgoTree.

- [ ] **Step 1: Add emission tracking fields to UtxoValidator**

In `validation/src/utxo.rs`, add to the `UtxoValidator` struct:

```rust
pub struct UtxoValidator {
    prover: PersistentBatchAVLProver,
    validated_height: u32,
    checkpoint_height: u32,
    current_digest: ADDigest,
    parameters: Parameters,
    /// Current emission box ID (changes every block). None if all ERG emitted.
    emission_box_id: Option<[u8; 32]>,
    /// The emission contract ErgoTree bytes for matching outputs.
    emission_tree_bytes: Vec<u8>,
}
```

- [ ] **Step 2: Initialize emission tracking in UtxoValidator::new**

Update the constructor to compute and store the emission contract ErgoTree:

```rust
use ergo_lib::chain::ergo_tree_predef::ErgoTreePredef;
use ergo_lib::chain::parameters::MonetarySettings;

impl UtxoValidator {
    pub fn new(
        prover: PersistentBatchAVLProver,
        height: u32,
        checkpoint_height: u32,
    ) -> Self {
        let digest_bytes = prover.digest();
        let digest = bytes_to_ad_digest(&digest_bytes);

        // Compute emission contract ErgoTree for box matching
        let emission_tree_bytes = ErgoTreePredef::emission_box_prop(
            &MonetarySettings::default(),
        )
        .map(|tree| tree.sigma_serialize_bytes().unwrap_or_default())
        .unwrap_or_default();

        Self {
            prover,
            validated_height: height,
            checkpoint_height,
            current_digest: digest,
            parameters: Parameters::default(),
            emission_box_id: None, // Discovered during first block validation
            emission_tree_bytes,
        }
    }
}
```

Note: `MonetarySettings::default()` returns mainnet settings. For testnet, the settings may differ. Check if `Parameters` carries the monetary settings or if we need to parameterize this. For now, mainnet defaults are correct.

- [ ] **Step 3: Update emission box tracking in validate_block**

At the end of `validate_block()`, after state changes are applied, scan the insertions for the emission box:

```rust
        // After step 7 (persist) and before step 9 (advance state):

        // Track emission box: scan new outputs for emission contract
        self.emission_box_id = None;
        for (box_id, box_bytes) in &changes.insertions {
            if let Ok(ergo_box) = tx_validation::deserialize_box(box_bytes) {
                let tree_bytes = ergo_box
                    .ergo_tree
                    .sigma_serialize_bytes()
                    .unwrap_or_default();
                if tree_bytes == self.emission_tree_bytes {
                    self.emission_box_id = Some(*box_id);
                    break;
                }
            }
        }
```

- [ ] **Step 4: Add emission_box_id to BlockValidator trait**

In `validation/src/lib.rs`, add to the trait:

```rust
    /// Current emission box ID in the UTXO set. None if digest mode or all ERG emitted.
    fn emission_box_id(&self) -> Option<[u8; 32]> {
        None
    }
```

And implement in `UtxoValidator`:

```rust
    fn emission_box_id(&self) -> Option<[u8; 32]> {
        self.emission_box_id
    }
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p ergo-validation`
Expected: compiles. The `ErgoTreePredef::emission_box_prop` and `SigmaSerializable` imports need to be verified.

- [ ] **Step 6: Commit**

```
feat(validation): track emission box ID across block applications
```

---

### Task 4: ReemissionRules (EIP-27 schedule)

**Files:**
- Create: `mining/src/emission.rs` (first part — schedule only)

EIP-27 re-emission is NOT in sigma-rust. We implement the schedule here. Reference: JVM `ReemissionRules` and EIP-27 specification.

- [ ] **Step 1: Write the test first**

In `mining/src/emission.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_reemission_before_activation() {
        let rules = ReemissionRules::mainnet();
        // Before activation height 777_217: no re-emission
        assert_eq!(rules.reemission_for_height(777_216, 67_500_000_000), 0);
    }

    #[test]
    fn test_reemission_at_activation() {
        let rules = ReemissionRules::mainnet();
        // At activation: miner reward is 67.5 ERG (> 15 ERG), re-emission = 12 ERG
        assert_eq!(
            rules.reemission_for_height(777_217, 67_500_000_000),
            12_000_000_000
        );
    }

    #[test]
    fn test_reemission_high_reward() {
        let rules = ReemissionRules::mainnet();
        // When reward >= 15 ERG: re-emission = 12 ERG
        assert_eq!(
            rules.reemission_for_height(800_000, 15_000_000_000),
            12_000_000_000
        );
    }

    #[test]
    fn test_reemission_medium_reward() {
        let rules = ReemissionRules::mainnet();
        // When 3 < reward < 15: re-emission = reward - 3 ERG
        assert_eq!(
            rules.reemission_for_height(1_500_000, 6_000_000_000),
            3_000_000_000
        );
    }

    #[test]
    fn test_reemission_min_reward() {
        let rules = ReemissionRules::mainnet();
        // When reward <= 3 ERG: re-emission = 0 (from normal emission)
        // But re-emission contract provides 3 ERG
        assert_eq!(
            rules.reemission_for_height(2_500_000, 3_000_000_000),
            0
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ergo-mining`
Expected: FAIL — `ReemissionRules` not defined.

- [ ] **Step 3: Implement ReemissionRules**

In `mining/src/emission.rs`:

```rust
/// EIP-27 re-emission schedule.
///
/// After the initial emission period, miners receive additional ERG from
/// the re-emission contract. The amount depends on the current miner reward
/// from the standard emission schedule.
///
/// Reference: JVM `ReemissionRules`, EIP-27 specification.
pub struct ReemissionRules {
    /// Height at which re-emission activates (mainnet: 777_217).
    pub activation_height: u32,
    /// Re-emission amount when miner reward >= 15 ERG (12 ERG in nanoERG).
    pub reemission_amount: i64,
    /// Minimum miner reward threshold for re-emission (15 ERG in nanoERG).
    pub reemission_threshold: i64,
    /// Floor miner reward below which no re-emission occurs (3 ERG in nanoERG).
    pub min_reward: i64,
}

impl ReemissionRules {
    /// Mainnet EIP-27 parameters.
    pub fn mainnet() -> Self {
        Self {
            activation_height: 777_217,
            reemission_amount: 12_000_000_000,     // 12 ERG
            reemission_threshold: 15_000_000_000,   // 15 ERG
            min_reward: 3_000_000_000,              // 3 ERG
        }
    }

    /// Testnet EIP-27 parameters (same schedule, different activation height).
    /// Check JVM testnet config for the exact activation height.
    pub fn testnet() -> Self {
        Self {
            activation_height: 777_217, // Verify against testnet config
            ..Self::mainnet()
        }
    }

    /// Compute re-emission amount at a given height.
    ///
    /// `miner_reward` is the standard emission reward in nanoERG
    /// (from `EmissionRules::miners_reward_at_height`).
    ///
    /// Returns additional nanoERG from re-emission, or 0 if not active.
    pub fn reemission_for_height(&self, height: u32, miner_reward: i64) -> i64 {
        if height < self.activation_height {
            return 0;
        }

        if miner_reward >= self.reemission_threshold {
            // High reward period: fixed 12 ERG re-emission
            self.reemission_amount
        } else if miner_reward > self.min_reward {
            // Tapering period: reward - 3 ERG
            miner_reward - self.min_reward
        } else {
            // Below floor: no re-emission from this path
            // (re-emission contract itself provides 3 ERG, handled separately)
            0
        }
    }
}
```

Note: verify the exact re-emission parameters against JVM source at `~/projects/ergo-node-build`. The activation height, thresholds, and token IDs should be cross-referenced. The testnet activation height may differ.

- [ ] **Step 4: Run tests**

Run: `cargo test -p ergo-mining`
Expected: all 5 tests PASS.

- [ ] **Step 5: Commit**

```
feat(mining): ReemissionRules — EIP-27 re-emission schedule
```

---

### Task 5: Emission transaction construction

**Files:**
- Modify: `mining/src/emission.rs`

Build the coinbase transaction: spend emission box → new emission box (reduced value) + miner reward box (time-locked). Reference: JVM `CandidateGenerator.collectRewards()`.

- [ ] **Step 1: Write the test**

Add to `mining/src/emission.rs` tests:

```rust
    #[test]
    fn test_emission_tx_basic_structure() {
        // A valid emission tx has:
        // - 1 input (the emission box)
        // - 2 outputs (new emission box + miner reward box)
        // - Output 0 value = old emission box value - reward
        // - Output 1 value = reward
        //
        // Exact test requires a real emission box from chain state.
        // This test verifies the builder produces the right structure.
        // Full correctness is verified in Task 9 against a real chain state.
    }
```

Note: the emission tx requires a real emission box (with correct value, tokens, script). A full unit test needs mock data from the JVM. For now, the test validates the structure. The real validation comes in Task 9 when we test against chain state.

- [ ] **Step 2: Implement build_emission_tx**

Add to `mining/src/emission.rs`:

```rust
use ergo_chain_types::ADDigest;
use ergo_lib::chain::emission::EmissionRules;
use ergo_lib::chain::ergo_tree_predef::ErgoTreePredef;
use ergo_lib::chain::parameters::MonetarySettings;
use ergo_lib::chain::transaction::unsigned::UnsignedTransaction;
use ergo_lib::chain::transaction::{Input, Transaction, TxId};
use ergo_lib::ergotree_ir::chain::ergo_box::{
    BoxId, ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters,
};
use ergo_lib::ergotree_ir::chain::token::{Token, TokenAmount, TokenId};
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_lib::wallet::signing::TransactionContext;

use crate::MiningError;

/// Build the emission (coinbase) transaction for a new block.
///
/// Spends the current emission box and creates:
/// - A new emission box with reduced value
/// - A miner reward box protected by the miner's PK
///
/// Returns the transaction and the new emission box ID.
pub fn build_emission_tx(
    emission_box: &ErgoBox,
    height: u32,
    miner_pk: &ProveDlog,
    reward_delay: i32,
    reemission_rules: &ReemissionRules,
) -> Result<Transaction, MiningError> {
    let settings = MonetarySettings::default();
    let emission = EmissionRules::new(settings.clone());

    let miner_reward = emission.miners_reward_at_height(height as i64);
    if miner_reward <= 0 && emission_box.value.as_i64() <= 0 {
        return Err(MiningError::Emission("no emission remaining".into()));
    }

    let reemission_amount = reemission_rules.reemission_for_height(height, miner_reward);
    let total_reward = miner_reward + reemission_amount;

    // New emission box: same script, reduced value
    let new_emission_value = emission_box.value.as_i64() - miner_reward;
    if new_emission_value < 0 {
        return Err(MiningError::Emission(
            format!("emission box value {} < reward {}", emission_box.value.as_i64(), miner_reward),
        ));
    }

    // Build miner reward output script (time-locked)
    let reward_script = ErgoTreePredef::reward_output_script(reward_delay, miner_pk.clone())
        .map_err(|e| MiningError::Emission(format!("reward script: {e}")))?;

    // TODO: Handle EIP-27 token operations on the emission box.
    // When re-emission is active, the emission box contains re-emission tokens
    // that decrease each block. The token handling depends on the emission box's
    // current token state. Cross-reference JVM CandidateGenerator.collectRewards()
    // lines 744-792 for the exact token operations.
    //
    // For the initial implementation, build the basic emission tx structure.
    // EIP-27 token handling is added after verifying the basic flow works.

    // Construct the transaction using ergo-lib primitives.
    // The emission box input has an empty prover result (the emission contract
    // is self-validating — its script checks the output structure).
    //
    // The exact construction depends on ergo-lib's Transaction builder API.
    // Reference: ergo-lib TxBuilder or direct UnsignedTransaction construction.

    todo!(
        "Construct emission Transaction from emission_box, new_emission_value, \
         total_reward, reward_script. This requires verifying ergo-lib's \
         Transaction construction API. The structure is: \
         Input(emission_box.box_id, empty_proof) → \
         Output(new_emission_box) + Output(miner_reward_box)"
    )
}
```

Note: this step intentionally uses `todo!()` for the transaction construction because ergo-lib's `Transaction` construction API needs verification. The builder may use `UnsignedTransaction` + signing, or direct construction. Cross-reference `ergo-lib/src/chain/transaction.rs` and `ergo-lib/src/wallet/tx_builder.rs` for the exact API. The key insight: the emission transaction uses `ProverResult::empty()` for its input (the emission contract validates itself).

- [ ] **Step 3: Fill in transaction construction**

After verifying ergo-lib's API, replace the `todo!()` with actual construction. The pattern will be similar to:

```rust
    let input = Input::new(emission_box.box_id(), ProverResult::empty());
    // Build output candidates, then create Transaction
```

The exact API depends on what ergo-lib exposes. This step requires reading `ergo-lib` source to find the right constructors.

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p ergo-mining`
Expected: compiles (with the todo! producing a warning).

- [ ] **Step 5: Commit**

```
feat(mining): emission transaction construction (basic structure)
```

---

### Task 6: Fee transaction construction

**Files:**
- Create: `mining/src/fee.rs`

Collect fee outputs from selected transactions, aggregate into a single miner output.

- [ ] **Step 1: Write the test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_fee_tx_when_zero_fees() {
        // When no fee outputs exist, build_fee_tx returns None
        let result = build_fee_tx(&[], &[], 720, todo!("miner_pk"));
        assert!(result.unwrap().is_none());
    }
}
```

- [ ] **Step 2: Implement build_fee_tx**

```rust
use ergo_lib::chain::ergo_tree_predef::ErgoTreePredef;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use crate::MiningError;

/// Build the fee collection transaction.
///
/// Scans outputs of `selected_txs` for boxes matching the fee proposition.
/// Aggregates all fee values into a single miner output.
///
/// Returns None if total fee is zero.
pub fn build_fee_tx(
    selected_txs: &[Transaction],
    spent_box_ids: &[[u8; 32]],
    reward_delay: i32,
    miner_pk: &ProveDlog,
) -> Result<Option<Transaction>, MiningError> {
    // Get fee proposition ErgoTree bytes for matching
    let fee_tree = ErgoTreePredef::fee_proposition(reward_delay)
        .map_err(|e| MiningError::Emission(format!("fee proposition: {e}")))?;
    let fee_tree_bytes = fee_tree
        .sigma_serialize_bytes()
        .map_err(|e| MiningError::Emission(format!("fee tree serialize: {e}")))?;

    // Collect fee outputs from selected transactions
    let mut fee_boxes: Vec<&ErgoBox> = Vec::new();
    for tx in selected_txs {
        for output in tx.outputs.iter() {
            let output_tree_bytes = output
                .ergo_tree
                .sigma_serialize_bytes()
                .unwrap_or_default();
            if output_tree_bytes == fee_tree_bytes {
                // Check this output isn't spent by another selected tx
                let id_bytes: Vec<u8> = output.box_id().into();
                let mut id = [0u8; 32];
                id.copy_from_slice(&id_bytes);
                if !spent_box_ids.contains(&id) {
                    fee_boxes.push(output);
                }
            }
        }
    }

    if fee_boxes.is_empty() {
        return Ok(None);
    }

    let total_fee: i64 = fee_boxes.iter().map(|b| b.value.as_i64()).sum();
    if total_fee <= 0 {
        return Ok(None);
    }

    // Build fee tx: inputs = fee boxes, output = single miner reward box
    let reward_script = ErgoTreePredef::reward_output_script(reward_delay, miner_pk.clone())
        .map_err(|e| MiningError::Emission(format!("reward script: {e}")))?;

    // Construct transaction — same pattern as emission tx.
    // Fee box inputs use empty prover results (the fee proposition
    // allows spending in the same block the fee box was created).
    todo!(
        "Construct fee Transaction: inputs from fee_boxes with empty proofs, \
         single output with total_fee value and reward_script"
    )
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p ergo-mining`

- [ ] **Step 4: Commit**

```
feat(mining): fee transaction construction
```

---

### Task 7: Extension section building

**Files:**
- Create: `mining/src/extension.rs`
- Modify: `mining/Cargo.toml` (ergo-nipopow should already be listed)

Build the extension with NiPoPoW interlinks and voting parameters.

- [ ] **Step 1: Implement build_extension**

```rust
use ergo_chain_types::Header;
use ergo_nipopow::NipopowAlgos;

use crate::types::ExtensionCandidate;
use crate::MiningError;

/// Build the extension section for a new block.
///
/// Contains NiPoPoW interlinks (updated from parent) and voting data.
pub fn build_extension(
    parent: &Header,
    parent_interlinks: &[ergo_chain_types::BlockId],
    votes: [u8; 3],
) -> Result<ExtensionCandidate, MiningError> {
    // 1. Update interlinks from parent
    let algos = NipopowAlgos::default();
    let updated_interlinks =
        NipopowAlgos::update_interlinks(parent.clone(), parent_interlinks.to_vec())
            .map_err(|e| MiningError::AssemblyFailed(format!("interlinks: {e}")))?;

    // 2. Pack interlinks into extension format
    let interlink_fields = NipopowAlgos::pack_interlinks(updated_interlinks);

    // 3. Combine interlink fields with voting
    // Voting is encoded in the header's votes field, not in the extension
    // (except at epoch boundaries where parameter updates go in extension).
    // For non-epoch blocks, extension = just interlinks.
    let fields = interlink_fields;

    Ok(ExtensionCandidate { fields })
}

/// Compute the Merkle root digest of extension fields.
///
/// Uses the same Merkle tree construction as the JVM node's Extension.rootHash.
pub fn extension_digest(extension: &ExtensionCandidate) -> Result<[u8; 32], MiningError> {
    use ergo_merkle_tree::{MerkleNode, MerkleTree};

    if extension.fields.is_empty() {
        // Empty extension — use a default/zero digest
        // Check JVM behavior for empty extensions
        return Ok([0u8; 32]);
    }

    // Each extension field becomes a leaf: key ++ value
    let nodes: Vec<MerkleNode> = extension
        .fields
        .iter()
        .map(|(key, value)| {
            let mut leaf_data = Vec::with_capacity(2 + value.len());
            leaf_data.extend_from_slice(key);
            leaf_data.extend_from_slice(value);
            MerkleNode::from_bytes(leaf_data)
        })
        .collect();

    let tree = MerkleTree::new(nodes)
        .map_err(|e| MiningError::AssemblyFailed(format!("extension merkle: {e}")))?;

    Ok(*tree.root_hash())
}
```

Note: `NipopowAlgos::update_interlinks` and `pack_interlinks` are the exact function names from the ergo-nipopow crate. Verify the method signatures — they may be methods on `NipopowAlgos` instance or associated functions. The extension Merkle root computation may need to match the JVM's `Extension.rootHash` exactly. Cross-verify.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p ergo-mining`

- [ ] **Step 3: Commit**

```
feat(mining): extension section building with NiPoPoW interlinks
```

---

### Task 8: Header construction + WorkMessage derivation

**Files:**
- Modify: `mining/src/lib.rs`

Build a candidate Header (without PoW) and derive the WorkMessage that miners receive.

- [ ] **Step 1: Implement header construction + WorkMessage**

Add to `mining/src/lib.rs` (or a new `mining/src/candidate.rs` if lib.rs gets large):

```rust
use blake2::digest::Digest as Blake2Digest;
use blake2::Blake2b256;
use ergo_chain_types::{
    AutolykosSolution, BlockId, Digest, Digest32, Header,
    autolykos_pow_scheme::decode_compact_bits,
};
use ergo_lib::ergotree_ir::sigma_protocol::dlog_group::EcPoint;
use ergo_merkle_tree::{MerkleNode, MerkleTree};

use crate::extension::extension_digest;
use crate::types::*;

/// Compute the Merkle root of transaction IDs for the header's transaction_root.
pub fn transactions_root(txs: &[Transaction]) -> Result<Digest32, MiningError> {
    let nodes: Vec<MerkleNode> = txs
        .iter()
        .map(|tx| {
            let id_bytes: Vec<u8> = tx.id().into();
            MerkleNode::from_bytes(id_bytes)
        })
        .collect();

    if nodes.is_empty() {
        return Err(MiningError::AssemblyFailed("no transactions".into()));
    }

    let tree = MerkleTree::new(nodes)
        .map_err(|e| MiningError::AssemblyFailed(format!("tx merkle: {e}")))?;

    Ok(Digest::from(*tree.root_hash()))
}

/// Build the candidate header and derive the WorkMessage.
///
/// The header is constructed with a placeholder PoW solution.
/// `serialize_without_pow()` excludes the solution, producing the bytes
/// that the miner hashes.
pub fn build_work_message(
    candidate: &CandidateBlock,
    miner_pk: &EcPoint,
) -> Result<(Vec<u8>, WorkMessage), MiningError> {
    let height = candidate.parent.height + 1;

    // AD proofs root = Blake2b256(ad_proof_bytes)
    let ad_proofs_root = {
        let mut hasher = Blake2b256::new();
        hasher.update(&candidate.ad_proof_bytes);
        let hash: [u8; 32] = hasher.finalize().into();
        Digest::from(hash)
    };

    // Transaction root = Merkle root of tx IDs
    let tx_root = transactions_root(&candidate.transactions)?;

    // Extension root
    let ext_root = extension_digest(&candidate.extension)?;

    // Build header with placeholder solution (excluded from serialization)
    let header = Header {
        version: candidate.version,
        id: BlockId(Digest::from([0u8; 32])), // Computed after PoW
        parent_id: candidate.parent.id,
        ad_proofs_root,
        state_root: candidate.state_root,
        transaction_root: tx_root,
        timestamp: candidate.timestamp,
        n_bits: candidate.n_bits,
        height,
        extension_root: Digest::from(ext_root),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(EcPoint::default()),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: candidate.votes.into(), // Verify Votes type conversion
        unparsed_bytes: Box::new([]),
    };

    // Serialize header without PoW
    let header_bytes = header
        .serialize_without_pow()
        .map_err(|e| MiningError::AssemblyFailed(format!("header serialize: {e}")))?;

    // msg = Blake2b256(header_bytes)
    let msg = {
        let mut hasher = Blake2b256::new();
        hasher.update(&header_bytes);
        let hash: [u8; 32] = hasher.finalize().into();
        hash
    };

    // b = target from nBits
    let target = decode_compact_bits(candidate.n_bits);

    let work = WorkMessage {
        msg: hex::encode(msg),
        b: target.to_string(),
        h: height,
        pk: hex::encode(/* miner_pk compressed bytes — verify encoding */
            &[0u8; 33] // placeholder — need EcPoint → compressed bytes
        ),
        proof: ProofOfUpcomingTransactions {
            msg_preimage: hex::encode(&header_bytes),
            tx_proofs: vec![],
        },
    };

    Ok((header_bytes, work))
}
```

Note: several details need verification during implementation:
1. `Votes` type — may be `[u8; 3]` or a newtype. Check ergo-chain-types.
2. `EcPoint` to compressed hex — check if ergo-chain-types has a serialization method.
3. `TxId::into()` for Vec<u8> — verify the conversion path.
4. `Digest::from([u8; 32])` — verify constructor.
5. `decode_compact_bits` returns `BigInt` not `BigUint` — verify and adjust the `b` field type.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p ergo-mining`

- [ ] **Step 3: Commit**

```
feat(mining): header construction and WorkMessage derivation
```

---

### Task 9: CandidateGenerator — generate_candidate (empty mempool)

**Files:**
- Modify: `mining/src/lib.rs`

Orchestrate all pieces into a complete candidate. Start with empty mempool (emission tx only) — the simplest valid block.

- [ ] **Step 1: Implement generate_candidate**

```rust
use std::time::{SystemTime, UNIX_EPOCH};

/// Generate a block candidate from the current chain state.
///
/// For now: emission tx only (empty mempool). Transaction selection
/// is added in Task 11.
pub fn generate_candidate(
    config: &MinerConfig,
    parent: &Header,
    n_bits: u32,
    parent_interlinks: &[BlockId],
    emission_box: &ErgoBox,
    validator_proofs: &dyn Fn(&[Transaction]) -> Option<Result<(Vec<u8>, ADDigest), ValidationError>>,
) -> Result<(CandidateBlock, WorkMessage), MiningError> {
    let height = parent.height + 1;
    let timestamp = {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::cmp::max(now, parent.timestamp + 1)
    };

    // 1. Build emission transaction
    let reemission_rules = ReemissionRules::mainnet(); // TODO: parameterize for testnet
    let emission_tx = emission::build_emission_tx(
        emission_box,
        height,
        &config.miner_pk,
        config.reward_delay,
        &reemission_rules,
    )?;

    // 2. Transaction list: [emission_tx] (no mempool txs yet, no fee tx)
    let transactions = vec![emission_tx];

    // 3. Compute state root
    let (ad_proof_bytes, state_root) = validator_proofs(&transactions)
        .ok_or(MiningError::Unavailable("UTXO mode required".into()))?
        .map_err(MiningError::Validation)?;

    // 4. Build extension
    let extension = extension::build_extension(parent, parent_interlinks, config.votes)?;

    // 5. Assemble candidate
    let mut candidate = CandidateBlock {
        parent: parent.clone(),
        version: parent.version, // Same version as parent (unless voting changes it)
        n_bits,
        state_root,
        ad_proof_bytes,
        transactions,
        timestamp,
        extension,
        votes: config.votes,
        header_bytes: vec![], // Filled by build_work_message
    };

    // 6. Build WorkMessage
    let (header_bytes, work) = build_work_message(&candidate, &config.miner_pk.h)?;
    candidate.header_bytes = header_bytes;

    Ok((candidate, work))
}
```

Note: the `validator_proofs` parameter is a closure that calls `validator.proofs_for_transactions()`. This avoids the mining crate needing to know the validator's concrete type. The main crate provides the closure when wiring things together.

- [ ] **Step 2: Cross-verification gate — compare msg with JVM**

This is the most critical test. Generate a WorkMessage from the Rust node and compare `msg` byte-for-byte with the JVM node's output for the same chain state.

Steps:
1. On the test server, query the JVM node's mining candidate: `curl http://localhost:9033/mining/candidate`
2. Note the `msg` and `h` (height) from the response
3. In the Rust node, load the same chain state (same parent header) and generate a candidate
4. Compare `msg` values

If they don't match, the header serialization diverges from the JVM. Debug by comparing the `proof.msgPreimage` bytes field-by-field.

This test is manual for now — automated once both nodes are running side by side.

- [ ] **Step 3: Commit**

```
feat(mining): CandidateGenerator — empty mempool candidate assembly
```

---

### Task 10: Solution validation + block assembly

**Files:**
- Create: `mining/src/solution.rs`

Verify PoW and assemble the full block from candidate + solution.

- [ ] **Step 1: Implement validate_solution**

```rust
use ergo_chain_types::{
    AutolykosPowScheme, AutolykosSolution, BlockId, Digest, Header,
    autolykos_pow_scheme::decode_compact_bits,
};
use num_bigint::BigUint;

use crate::types::CandidateBlock;
use crate::MiningError;

/// Validate a PoW solution against a cached candidate.
///
/// Returns the assembled Header on success.
pub fn validate_solution(
    candidate: &CandidateBlock,
    solution: AutolykosSolution,
) -> Result<Header, MiningError> {
    let height = candidate.parent.height + 1;

    // Build the full header by injecting the solution
    let header = Header {
        version: candidate.version,
        id: BlockId(Digest::from([0u8; 32])), // Recomputed below
        parent_id: candidate.parent.id,
        ad_proofs_root: { /* same as in build_work_message */ todo!("compute ad_proofs_root") },
        state_root: candidate.state_root,
        transaction_root: crate::transactions_root(&candidate.transactions)?,
        timestamp: candidate.timestamp,
        n_bits: candidate.n_bits,
        height,
        extension_root: Digest::from(
            crate::extension::extension_digest(&candidate.extension)?
        ),
        autolykos_solution: solution,
        votes: candidate.votes.into(),
        unparsed_bytes: Box::new([]),
    };

    // Compute header ID from the full serialization (including PoW)
    // The id field should be derived from the full header hash.
    // Check if ergo-chain-types computes this automatically or if we must set it.

    // Verify PoW
    let pow = AutolykosPowScheme::default();
    let hit = pow
        .pow_hit(&header)
        .map_err(|e| MiningError::InvalidSolution(format!("pow_hit failed: {e}")))?;

    let target = decode_compact_bits(candidate.n_bits);
    // decode_compact_bits returns BigInt; pow_hit returns BigUint
    // Need to compare: hit < target
    let target_uint = target
        .to_biguint()
        .ok_or(MiningError::InvalidSolution("negative target".into()))?;

    if hit >= target_uint {
        return Err(MiningError::InvalidSolution(format!(
            "hit {hit} >= target {target_uint}"
        )));
    }

    Ok(header)
}
```

Note: the header `id` field computation needs verification. In the JVM, the header ID is derived from the serialized header bytes (including PoW). Check if `Header` in ergo-chain-types computes `id` automatically or if it must be set explicitly. Also, `decode_compact_bits` returns `BigInt` — verify the comparison with `BigUint` from `pow_hit`.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p ergo-mining`

- [ ] **Step 3: Commit**

```
feat(mining): solution validation and PoW verification
```

---

### Task 11: Transaction selection from mempool

**Files:**
- Modify: `mining/src/lib.rs`

Add transaction selection to `generate_candidate`. Select from mempool in priority order, validate each, respect block cost/size limits.

- [ ] **Step 1: Add select_transactions function**

```rust
use ergo_lib::chain::ergo_state_context::ErgoStateContext;
use ergo_validation::validate_single_transaction;

/// Select transactions from mempool for block inclusion.
///
/// Returns (selected_txs, invalid_tx_ids).
/// Selected txs are in priority order, validated against the upcoming state.
/// Invalid tx IDs should be reported to the mempool for cleanup.
pub fn select_transactions(
    candidates: &[(Transaction, Vec<u8>)], // (tx, tx_bytes) from mempool.all_prioritized()
    emission_outputs: &[ErgoBox],          // Outputs from the emission tx (available as inputs)
    state_context: &ErgoStateContext,
    max_block_cost: u64,
    max_block_size: usize,
    utxo_lookup: &dyn Fn(&[u8; 32]) -> Option<ErgoBox>,
) -> (Vec<Transaction>, Vec<[u8; 32]>) {
    let mut selected = Vec::new();
    let mut invalid_ids = Vec::new();
    let mut accumulated_cost: u64 = 0;
    let mut accumulated_size: usize = 0;
    // Track outputs from selected txs (available as inputs for later txs)
    let mut available_outputs: HashMap<[u8; 32], ErgoBox> = HashMap::new();

    // Seed with emission tx outputs
    for output in emission_outputs {
        let id_bytes: Vec<u8> = output.box_id().into();
        let mut id = [0u8; 32];
        id.copy_from_slice(&id_bytes);
        available_outputs.insert(id, output.clone());
    }

    for (tx, tx_bytes) in candidates {
        // Check size limit
        if accumulated_size + tx_bytes.len() > max_block_size {
            continue; // Skip, might fit smaller txs later
        }

        // Resolve input boxes
        let mut input_boxes = Vec::new();
        let mut inputs_found = true;
        for input in tx.inputs.iter() {
            let id_bytes: Vec<u8> = input.box_id.into();
            let mut id = [0u8; 32];
            id.copy_from_slice(&id_bytes);
            if let Some(b) = available_outputs.get(&id).cloned().or_else(|| utxo_lookup(&id)) {
                input_boxes.push(b);
            } else {
                inputs_found = false;
                break;
            }
        }
        if !inputs_found {
            continue; // Inputs not available, skip
        }

        // Resolve data input boxes
        let data_boxes: Vec<ErgoBox> = tx
            .data_inputs
            .as_ref()
            .map(|dis| {
                dis.iter()
                    .filter_map(|di| {
                        let id_bytes: Vec<u8> = di.box_id.into();
                        let mut id = [0u8; 32];
                        id.copy_from_slice(&id_bytes);
                        available_outputs.get(&id).cloned().or_else(|| utxo_lookup(&id))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Validate
        match validate_single_transaction(tx, &input_boxes, &data_boxes, state_context) {
            Ok(cost) => {
                if accumulated_cost + cost as u64 > max_block_cost {
                    continue; // Over cost limit
                }
                accumulated_cost += cost as u64;
                accumulated_size += tx_bytes.len();

                // Track outputs for later txs
                for output in tx.outputs.iter() {
                    let id_bytes: Vec<u8> = output.box_id().into();
                    let mut id = [0u8; 32];
                    id.copy_from_slice(&id_bytes);
                    available_outputs.insert(id, output.clone());
                }

                selected.push(tx.clone());
            }
            Err(_) => {
                let tx_id: Vec<u8> = tx.id().into();
                let mut id = [0u8; 32];
                id.copy_from_slice(&tx_id);
                invalid_ids.push(id);
            }
        }
    }

    (selected, invalid_ids)
}
```

- [ ] **Step 2: Integrate into generate_candidate**

Update `generate_candidate` to call `select_transactions` and `build_fee_tx`, inserting the selected transactions between the emission tx and fee tx. The `invalid_ids` returned by `select_transactions` should be returned from `generate_candidate` so the caller (main crate's mining task or API handler) can call `mempool.invalidate()` for each.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p ergo-mining`

- [ ] **Step 4: Commit**

```
feat(mining): transaction selection from mempool with cost/size limits
```

---

### Task 12: Candidate caching + API handlers + main crate wiring

**Files:**
- Modify: `mining/src/lib.rs`
- Modify: `api/src/handlers.rs`
- Modify: `api/src/lib.rs`
- Modify: `api/Cargo.toml`
- Modify: `src/main.rs`

Wire everything together.

- [ ] **Step 1: Add CandidateGenerator struct to mining/src/lib.rs**

```rust
use std::sync::RwLock;
use std::time::Instant;

pub struct CandidateGenerator {
    pub config: MinerConfig,
    cached: RwLock<Option<CachedCandidate>>,
}

impl CandidateGenerator {
    pub fn new(config: MinerConfig) -> Self {
        Self {
            config,
            cached: RwLock::new(None),
        }
    }

    /// Get or generate a mining candidate.
    ///
    /// Returns cached candidate if still valid (same tip, within TTL).
    /// Otherwise generates a fresh candidate.
    pub fn get_candidate(
        &self,
        current_tip_height: u32,
        // ... other params needed for generation
    ) -> Result<WorkMessage, MiningError> {
        // Check cache
        if let Some(cached) = self.cached.read().unwrap().as_ref() {
            if cached.tip_height == current_tip_height
                && cached.created.elapsed() < self.config.candidate_ttl
            {
                return Ok(cached.work.clone());
            }
        }

        // Generate fresh candidate
        // ... call generate_candidate with all required params ...
        // ... store in cache ...

        todo!("Wire generate_candidate with all required handles")
    }

    /// Invalidate the cached candidate (called when chain tip changes).
    pub fn invalidate(&self) {
        *self.cached.write().unwrap() = None;
    }
}
```

- [ ] **Step 2: Add mining dependency to api/Cargo.toml**

```toml
ergo-mining = { path = "../mining" }
```

- [ ] **Step 3: Add mining handlers to api/src/handlers.rs**

```rust
pub async fn get_mining_candidate(
    State(state): State<ApiState>,
) -> Result<Json<WorkMessage>, ApiError> {
    let mining = state.mining.as_ref()
        .ok_or(ApiError::service_unavailable("mining not configured"))?;

    let tip_height = state.chain.read().await.tip_height();

    let work = mining.get_candidate(tip_height /* , other params */)
        .map_err(|e| match e {
            MiningError::Unavailable(msg) => ApiError::service_unavailable(&msg),
            other => ApiError::internal(&other.to_string()),
        })?;

    Ok(Json(work))
}

pub async fn post_mining_solution(
    State(state): State<ApiState>,
    Json(solution): Json<AutolykosSolution>,
) -> Result<StatusCode, ApiError> {
    let mining = state.mining.as_ref()
        .ok_or(ApiError::service_unavailable("mining not configured"))?;

    // ... validate solution, assemble block, broadcast ...

    Ok(StatusCode::OK)
}

pub async fn get_mining_reward_address(
    State(state): State<ApiState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mining = state.mining.as_ref()
        .ok_or(ApiError::service_unavailable("mining not configured"))?;

    // Derive P2S address from miner PK
    // ... address derivation ...

    Ok(Json(serde_json::json!({
        "rewardAddress": "address_here"
    })))
}
```

- [ ] **Step 4: Add mining routes to api/src/lib.rs**

Add to the axum router:

```rust
    .route("/mining/candidate", get(handlers::get_mining_candidate))
    .route("/mining/solution", post(handlers::post_mining_solution))
    .route("/mining/rewardAddress", get(handlers::get_mining_reward_address))
```

- [ ] **Step 5: Delegate mining methods on Validator wrapper in src/main.rs**

The `Validator` struct in main.rs wraps `ValidatorInner` (enum of Digest/Utxo). The new `BlockValidator` trait methods need delegation:

```rust
impl BlockValidator for Validator {
    // ... existing method delegations ...

    fn proofs_for_transactions(
        &self,
        txs: &[Transaction],
    ) -> Option<Result<(Vec<u8>, ADDigest), ValidationError>> {
        match &self.inner {
            ValidatorInner::Utxo(v) => v.proofs_for_transactions(txs),
            ValidatorInner::Digest(_) => None,
        }
    }

    fn emission_box_id(&self) -> Option<[u8; 32]> {
        match &self.inner {
            ValidatorInner::Utxo(v) => v.emission_box_id(),
            ValidatorInner::Digest(_) => None,
        }
    }
}
```

- [ ] **Step 6: Wire MiningState in src/main.rs**

In the main crate, construct `CandidateGenerator` when mining is configured and the node is in UTXO mode:

```rust
    // After validator construction:
    let mining = if let Some(miner_pk) = config.mining.miner_pk.as_ref() {
        match &validator_inner {
            ValidatorInner::Utxo(_) => {
                let config = MinerConfig {
                    miner_pk: parse_miner_pk(miner_pk)?,
                    reward_delay: 720,
                    votes: parse_votes(&config.mining.votes)?,
                    candidate_ttl: Duration::from_secs(config.mining.candidate_ttl_secs),
                };
                Some(Arc::new(CandidateGenerator::new(config)))
            }
            ValidatorInner::Digest(_) => {
                tracing::warn!("mining configured but node is in digest mode — mining disabled");
                None
            }
        }
    } else {
        None
    };

    // Pass to ApiState
```

- [ ] **Step 7: Add mining config parsing**

Add to the config parsing in `src/main.rs`:

```toml
[node.mining]
miner_pk = ""
votes = "000000"
candidate_ttl_secs = 15
reward_delay = 720
```

- [ ] **Step 8: Verify everything compiles**

Run: `cargo check --workspace`
Expected: full workspace compiles.

- [ ] **Step 9: Run all tests**

Run: `cargo test --workspace`
Expected: all existing tests still pass + new mining tests pass.

- [ ] **Step 10: Commit**

```
feat: wire mining crate into API and main crate
```

---

## Dependency Risk Verification

These checks should be done at the START of implementation, before writing significant code:

### Check 1: BatchAVLProver::generate_proof_for_operations (Task 2)

```bash
# Find the actual source
find ~/.cargo/git/checkouts/ -path "*/ergo_avltree_rust*/batch_avl_prover.rs" | head -1
# Read the method — verify it takes &self and returns (SerializedAdProof, ADDigest)
```

If the method doesn't exist or has a different signature, fall back to: persist + rollback (call `generate_proof_and_update_storage()` then `rollback()`).

### Check 2: sigma-rust EIP-27 APIs (Task 5)

```bash
# Check if EmissionRules has re-emission methods
grep -r "reemission\|re_emission\|ReemissionRules" ~/.cargo/git/checkouts/sigma-rust-*/
```

Already confirmed: NOT present. We build `ReemissionRules` ourselves.

### Check 3: ergo-nipopow API (Task 7)

```bash
# Verify function signatures
grep -n "pub fn update_interlinks\|pub fn pack_interlinks" ~/.cargo/git/checkouts/sigma-rust-*/ergo-nipopow/src/
```

Already confirmed: present as `NipopowAlgos::update_interlinks()` and `NipopowAlgos::pack_interlinks()`.

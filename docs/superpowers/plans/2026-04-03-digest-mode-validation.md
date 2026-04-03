# Digest Mode Block Validation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Validate block state transitions using AD proofs (digest mode), proving each block correctly transforms the UTXO state root without maintaining the full UTXO set.

**Architecture:** A `validation/` workspace crate implements the `BlockValidator` trait with a `DigestValidator` that parses block sections, computes AVL+ tree operations from transactions, and verifies AD proofs against the state roots in consecutive headers. The sync machine drives validation via a new `validated_height` watermark.

**Tech Stack:** `ergo-lib` (transaction parsing), `ergo_avltree_rust` (AD proof verification), `ergo-chain-types` (Header, ADDigest), `sigma-ser` (VLQ decoding), `blake2` (proof digest checks)

**Contract:** `facts/validation.md`

---

## File Structure

```
validation/
  Cargo.toml                    — workspace crate, depends on ergo-lib, ergo_avltree_rust, etc.
  src/
    lib.rs                      — BlockValidator trait, ValidationError, DigestValidator, re-exports
    sections.rs                 — parse BlockTransactions, ADProofs, Extension from raw bytes
    state_changes.rs            — convert Vec<Transaction> to AVL+ operations (Lookup/Remove/Insert)
    digest.rs                   — DigestValidator: AD proof verification via BatchAVLVerifier

sync/src/traits.rs              — add SyncStore::get_modifier()
sync/src/state.rs               — rename full_block_height → downloaded_height, add validated_height
sync/Cargo.toml                 — add ergo-validation dependency

src/bridge.rs                   — add SharedStore::get_modifier() impl
src/main.rs                     — create DigestValidator, pass to sync machine
Cargo.toml                      — add ergo-validation workspace member + dependency
```

---

### Task 1: Scaffold the validation crate

**Files:**
- Create: `validation/Cargo.toml`
- Create: `validation/src/lib.rs`
- Modify: `Cargo.toml` (root)

- [ ] **Step 1: Create validation/Cargo.toml**

```toml
[package]
name = "ergo-validation"
version = "0.1.0"
edition = "2021"
description = "Block validation for the Ergo Rust node"

[dependencies]
ergo-lib = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "6e76e82" }
ergo-chain-types = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "6e76e82" }
sigma-ser = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "6e76e82" }
ergo_avltree_rust = "0.1.1"
blake2 = "0.10"
bytes = "1"
tracing = "0.1"
thiserror = "2"

[dev-dependencies]
hex = "0.4"
```

- [ ] **Step 2: Create validation/src/lib.rs with trait and error types**

```rust
mod digest;
mod sections;
mod state_changes;

use ergo_chain_types::{ADDigest, Header};

pub use digest::DigestValidator;
pub use sections::{ParsedAdProofs, ParsedBlockTransactions, ParsedExtension};
pub use state_changes::StateChanges;

/// Validates block sections against the current UTXO state.
///
/// Two implementations: DigestValidator (AD proof based, no persistent UTXO set)
/// and UtxoValidator (persistent AVL+ tree, future Phase 4b).
pub trait BlockValidator {
    /// Validate a block's sections against the current state.
    ///
    /// `header.height` must equal `self.validated_height() + 1`.
    /// `ad_proofs` is required for digest mode, None for UTXO mode.
    /// `preceding_headers` contains up to 10 headers before this block (newest first).
    fn validate_block(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[Header],
    ) -> Result<(), ValidationError>;

    /// Height of the last validated block. 0 = genesis state set but no blocks applied.
    fn validated_height(&self) -> u32;

    /// Current state root digest (33 bytes).
    fn current_digest(&self) -> &ADDigest;

    /// Reset to a previous state after reorg.
    fn reset_to(&mut self, height: u32, digest: ADDigest);
}

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("section parse failed (type {section_type}): {reason}")]
    SectionParse { section_type: u8, reason: String },

    #[error("header ID mismatch in section type {section_type}")]
    HeaderIdMismatch { section_type: u8, expected: [u8; 32], got: [u8; 32] },

    #[error("AD proofs digest mismatch")]
    ProofDigestMismatch { expected: [u8; 32], got: [u8; 32] },

    #[error("state root mismatch after AD proof verification")]
    StateRootMismatch { expected: Vec<u8>, got: Vec<u8> },

    #[error("AD proof verification failed: {0}")]
    ProofVerificationFailed(String),

    #[error("intra-block double spend: box {0}")]
    IntraBlockDoubleSpend(String),

    #[error("transaction {index} invalid: {reason}")]
    TransactionInvalid { index: usize, reason: String },

    #[error("AD proofs required but not provided")]
    MissingProof,

    #[error("unexpected block height: expected {expected}, got {got}")]
    HeightMismatch { expected: u32, got: u32 },
}
```

- [ ] **Step 3: Create stub modules**

Create `validation/src/sections.rs`:
```rust
//! Parse raw block section bytes into typed structures.

/// Parsed ADProofs section (type 104).
pub struct ParsedAdProofs {
    pub header_id: [u8; 32],
    pub proof_bytes: Vec<u8>,
}

/// Parsed BlockTransactions section (type 102).
pub struct ParsedBlockTransactions {
    pub header_id: [u8; 32],
    pub block_version: u32,
    pub transactions: Vec<ergo_lib::chain::transaction::Transaction>,
}

/// A single key-value field from the Extension section.
pub struct ExtensionField {
    pub key: [u8; 2],
    pub value: Vec<u8>,
}

/// Parsed Extension section (type 108).
pub struct ParsedExtension {
    pub header_id: [u8; 32],
    pub fields: Vec<ExtensionField>,
}
```

Create `validation/src/state_changes.rs`:
```rust
//! Convert transactions to AVL+ tree operations.
```

Create `validation/src/digest.rs`:
```rust
//! DigestValidator: AD proof verification via BatchAVLVerifier.
```

- [ ] **Step 4: Add validation to workspace**

In root `Cargo.toml`, add under `[dependencies]`:
```toml
ergo-validation = { path = "validation" }
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p ergo-validation`
Expected: compiles with no errors (stub modules have no code yet)

- [ ] **Step 6: Commit**

```bash
git add validation/ Cargo.toml
git commit -m "scaffold validation crate with BlockValidator trait and error types"
```

---

### Task 2: Section parsing — ADProofs

**Files:**
- Modify: `validation/src/sections.rs`
- Test: `validation/src/sections.rs` (inline tests)

ADProofs is the simplest section: `[header_id: 32B] [proof_size: VLQ] [proof_bytes]`.

- [ ] **Step 1: Write failing test**

Add to `validation/src/sections.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ad_proofs_basic() {
        // Minimal valid ADProofs: 32-byte header_id + VLQ proof_size(3) + 3 proof bytes
        let mut data = Vec::new();
        let header_id = [0xAA; 32];
        data.extend_from_slice(&header_id);
        // VLQ encode 3 (single byte since < 128)
        data.push(3);
        data.extend_from_slice(&[0x01, 0x02, 0x03]);

        let parsed = parse_ad_proofs(&data).unwrap();
        assert_eq!(parsed.header_id, header_id);
        assert_eq!(parsed.proof_bytes, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn parse_ad_proofs_too_short() {
        let data = [0u8; 31]; // less than 32-byte header_id
        assert!(parse_ad_proofs(&data).is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ergo-validation -- parse_ad_proofs`
Expected: FAIL — `parse_ad_proofs` not defined

- [ ] **Step 3: Implement parse_ad_proofs**

In `validation/src/sections.rs`:

```rust
use sigma_ser::vlq_encode;
use std::io::Cursor;

use crate::ValidationError;

fn section_parse_err(section_type: u8, reason: impl Into<String>) -> ValidationError {
    ValidationError::SectionParse {
        section_type,
        reason: reason.into(),
    }
}

/// Parse an ADProofs section (type 104) from raw modifier bytes.
///
/// Wire format: [header_id: 32B] [proof_size: VLQ u32] [proof_bytes: proof_size B]
pub fn parse_ad_proofs(data: &[u8]) -> Result<ParsedAdProofs, ValidationError> {
    const TYPE_ID: u8 = 104;
    if data.len() < 33 {
        return Err(section_parse_err(TYPE_ID, "too short for header_id + proof_size"));
    }
    let header_id: [u8; 32] = data[..32].try_into().unwrap();
    let mut cursor = Cursor::new(&data[32..]);
    let proof_size = vlq_encode::ReadSigmaVlqExt::get_u32(&mut cursor)
        .map_err(|e| section_parse_err(TYPE_ID, format!("proof_size VLQ: {e}")))?
        as usize;
    let pos = 32 + cursor.position() as usize;
    let remaining = data.len() - pos;
    if remaining < proof_size {
        return Err(section_parse_err(
            TYPE_ID,
            format!("proof_size={proof_size} but only {remaining} bytes remain"),
        ));
    }
    let proof_bytes = data[pos..pos + proof_size].to_vec();
    Ok(ParsedAdProofs {
        header_id,
        proof_bytes,
    })
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p ergo-validation -- parse_ad_proofs`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add validation/src/sections.rs
git commit -m "implement ADProofs section parser"
```

---

### Task 3: Section parsing — BlockTransactions

**Files:**
- Modify: `validation/src/sections.rs`

BlockTransactions format: `[header_id: 32B] [ver_or_count: VLQ] [tx_count: VLQ if ver>1] [txs...]`

The version/count hack: if first VLQ > 10,000,000 then `block_version = value - 10,000,000` and read a second VLQ for tx_count. Otherwise the first VLQ IS the tx_count and block_version = 1.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn parse_block_transactions_empty_block() {
    // Block version 2, 0 transactions
    let mut data = Vec::new();
    let header_id = [0xBB; 32];
    data.extend_from_slice(&header_id);
    // VLQ encode 10_000_002 (version 2 sentinel)
    let mut sentinel_buf = Vec::new();
    vlq_encode::WriteSigmaVlqExt::put_u32(&mut sentinel_buf, 10_000_002).unwrap();
    data.extend_from_slice(&sentinel_buf);
    // VLQ encode 0 (tx_count)
    data.push(0);

    let parsed = parse_block_transactions(&data).unwrap();
    assert_eq!(parsed.header_id, header_id);
    assert_eq!(parsed.block_version, 2);
    assert!(parsed.transactions.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ergo-validation -- parse_block_transactions`
Expected: FAIL — `parse_block_transactions` not defined

- [ ] **Step 3: Implement parse_block_transactions**

```rust
use ergo_lib::chain::transaction::Transaction;
use sigma_ser::ScorexSerializable;

/// Version sentinel threshold for BlockTransactions encoding.
const BLOCK_VERSION_SENTINEL: u32 = 10_000_000;

/// Parse a BlockTransactions section (type 102) from raw modifier bytes.
///
/// Wire format: [header_id: 32B] [ver_or_count: VLQ] [tx_count: VLQ if ver>1] [txs...]
/// Version hack: if first VLQ > 10M, subtract 10M for block_version, read separate tx_count.
/// Otherwise first VLQ IS the tx_count and block_version = 1.
pub fn parse_block_transactions(data: &[u8]) -> Result<ParsedBlockTransactions, ValidationError> {
    const TYPE_ID: u8 = 102;
    if data.len() < 33 {
        return Err(section_parse_err(TYPE_ID, "too short for header_id"));
    }
    let header_id: [u8; 32] = data[..32].try_into().unwrap();
    let mut cursor = Cursor::new(&data[32..]);

    let ver_or_count = vlq_encode::ReadSigmaVlqExt::get_u32(&mut cursor)
        .map_err(|e| section_parse_err(TYPE_ID, format!("ver_or_count VLQ: {e}")))?;

    let (block_version, tx_count) = if ver_or_count > BLOCK_VERSION_SENTINEL {
        let version = ver_or_count - BLOCK_VERSION_SENTINEL;
        let count = vlq_encode::ReadSigmaVlqExt::get_u32(&mut cursor)
            .map_err(|e| section_parse_err(TYPE_ID, format!("tx_count VLQ: {e}")))?;
        (version, count as usize)
    } else {
        (1, ver_or_count as usize)
    };

    // Parse individual transactions from the remaining byte stream
    let pos = 32 + cursor.position() as usize;
    let mut tx_reader = Cursor::new(&data[pos..]);
    let mut transactions = Vec::with_capacity(tx_count);

    for i in 0..tx_count {
        let tx = Transaction::scorex_parse(&mut tx_reader)
            .map_err(|e| section_parse_err(TYPE_ID, format!("transaction {i}: {e}")))?;
        transactions.push(tx);
    }

    Ok(ParsedBlockTransactions {
        header_id,
        block_version,
        transactions,
    })
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p ergo-validation -- parse_block_transactions`
Expected: PASS

- [ ] **Step 5: Add test with real block fixture**

Extract a real BlockTransactions section from the test server store. SSH into `216.128.144.28` and run a small binary or script that reads a section from redb and hex-encodes it. Pick a low height (e.g., height 1 or 2) for a small fixture.

The test should parse the real section and verify the transaction count and that no parsing errors occur. The exact assertion values come from the fixture extraction. Example structure:

```rust
#[test]
fn parse_real_block_transactions() {
    let data = hex::decode(include_str!("../tests/fixtures/block_txs_height_1.hex")).unwrap();
    let parsed = parse_block_transactions(&data).unwrap();
    assert!(parsed.block_version >= 2);
    assert!(!parsed.transactions.is_empty());
    // Verify first transaction has expected structure
    assert!(!parsed.transactions[0].inputs.is_empty());
}
```

Store the hex fixture at `validation/tests/fixtures/block_txs_height_1.hex`.

- [ ] **Step 6: Run tests**

Run: `cargo test -p ergo-validation`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add validation/
git commit -m "implement BlockTransactions section parser"
```

---

### Task 4: Section parsing — Extension

**Files:**
- Modify: `validation/src/sections.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn parse_extension_basic() {
    let mut data = Vec::new();
    let header_id = [0xCC; 32];
    data.extend_from_slice(&header_id);
    // VLQ encode field_count = 2
    data.push(2);
    // Field 1: key=[0x00, 0x01], value_len=4, value=[0xDE, 0xAD, 0xBE, 0xEF]
    data.extend_from_slice(&[0x00, 0x01]);
    data.push(4);
    data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    // Field 2: key=[0x01, 0x00], value_len=1, value=[0xFF]
    data.extend_from_slice(&[0x01, 0x00]);
    data.push(1);
    data.push(0xFF);

    let parsed = parse_extension(&data).unwrap();
    assert_eq!(parsed.header_id, header_id);
    assert_eq!(parsed.fields.len(), 2);
    assert_eq!(parsed.fields[0].key, [0x00, 0x01]);
    assert_eq!(parsed.fields[0].value, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(parsed.fields[1].key, [0x01, 0x00]);
    assert_eq!(parsed.fields[1].value, vec![0xFF]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ergo-validation -- parse_extension`
Expected: FAIL

- [ ] **Step 3: Implement parse_extension**

```rust
/// Parse an Extension section (type 108) from raw modifier bytes.
///
/// Wire format: [header_id: 32B] [field_count: VLQ] [fields: {key: 2B, val_len: 1B, val}...]
pub fn parse_extension(data: &[u8]) -> Result<ParsedExtension, ValidationError> {
    const TYPE_ID: u8 = 108;
    if data.len() < 33 {
        return Err(section_parse_err(TYPE_ID, "too short for header_id"));
    }
    let header_id: [u8; 32] = data[..32].try_into().unwrap();
    let mut cursor = Cursor::new(&data[32..]);

    let field_count = vlq_encode::ReadSigmaVlqExt::get_u32(&mut cursor)
        .map_err(|e| section_parse_err(TYPE_ID, format!("field_count VLQ: {e}")))?
        as usize;

    let mut pos = 32 + cursor.position() as usize;
    let mut fields = Vec::with_capacity(field_count);

    for i in 0..field_count {
        if pos + 3 > data.len() {
            return Err(section_parse_err(TYPE_ID, format!("field {i}: truncated")));
        }
        let key: [u8; 2] = data[pos..pos + 2].try_into().unwrap();
        let value_len = data[pos + 2] as usize;
        pos += 3;
        if pos + value_len > data.len() {
            return Err(section_parse_err(
                TYPE_ID,
                format!("field {i}: value_len={value_len} but only {} bytes remain", data.len() - pos),
            ));
        }
        let value = data[pos..pos + value_len].to_vec();
        pos += value_len;
        fields.push(ExtensionField { key, value });
    }

    Ok(ParsedExtension {
        header_id,
        fields,
    })
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p ergo-validation -- parse_extension`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add validation/src/sections.rs
git commit -m "implement Extension section parser"
```

---

### Task 5: State changes computation

**Files:**
- Modify: `validation/src/state_changes.rs`

Convert `Vec<Transaction>` into AVL+ operations: Lookups (data inputs), Removes (inputs), Inserts (outputs). Intra-block netting: if tx2 spends an output created by tx1 in the same block, neither hits the tree.

- [ ] **Step 1: Write failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_changes_no_intra_block_spend() {
        // Two transactions, no intra-block dependencies.
        // tx1: 1 input (box_a), 1 output (box_c)
        // tx2: 1 input (box_b), 1 output (box_d)
        // Expected: removes=[box_a, box_b], inserts=[box_c, box_d], lookups=[]
        //
        // We can't easily construct Transaction objects without real data,
        // so this test uses the extract_changes helper with pre-built vecs.

        let box_a = [0xAA; 32];
        let box_b = [0xBB; 32];
        let box_c_id = [0xCC; 32];
        let box_d_id = [0xDD; 32];
        let box_c_bytes = vec![1, 2, 3]; // placeholder serialized box
        let box_d_bytes = vec![4, 5, 6];

        let tx_summaries = vec![
            TxSummary {
                input_ids: vec![box_a],
                data_input_ids: vec![],
                output_entries: vec![(box_c_id, box_c_bytes.clone())],
            },
            TxSummary {
                input_ids: vec![box_b],
                data_input_ids: vec![],
                output_entries: vec![(box_d_id, box_d_bytes.clone())],
            },
        ];

        let changes = compute_state_changes(tx_summaries).unwrap();
        assert_eq!(changes.lookups.len(), 0);
        assert_eq!(changes.removals.len(), 2);
        assert_eq!(changes.insertions.len(), 2);
        assert_eq!(changes.removals[0], box_a);
        assert_eq!(changes.removals[1], box_b);
    }

    #[test]
    fn state_changes_intra_block_netting() {
        // tx1 creates box_x, tx2 spends box_x.
        // box_x should not appear in removals or insertions (net-zero).
        let box_a = [0xAA; 32];
        let box_x_id = [0xCC; 32];
        let box_x_bytes = vec![1, 2, 3];
        let box_d_id = [0xDD; 32];
        let box_d_bytes = vec![4, 5, 6];

        let tx_summaries = vec![
            TxSummary {
                input_ids: vec![box_a],
                data_input_ids: vec![],
                output_entries: vec![(box_x_id, box_x_bytes)],
            },
            TxSummary {
                input_ids: vec![box_x_id], // spends tx1's output
                data_input_ids: vec![],
                output_entries: vec![(box_d_id, box_d_bytes.clone())],
            },
        ];

        let changes = compute_state_changes(tx_summaries).unwrap();
        assert_eq!(changes.removals.len(), 1); // only box_a
        assert_eq!(changes.removals[0], box_a);
        assert_eq!(changes.insertions.len(), 1); // only box_d (box_x netted out)
        assert_eq!(changes.insertions[0].0, box_d_id);
    }

    #[test]
    fn state_changes_double_spend_rejected() {
        let box_a = [0xAA; 32];
        let box_b_id = [0xBB; 32];
        let box_b_bytes = vec![1, 2, 3];
        let box_c_id = [0xCC; 32];
        let box_c_bytes = vec![4, 5, 6];

        let tx_summaries = vec![
            TxSummary {
                input_ids: vec![box_a],
                data_input_ids: vec![],
                output_entries: vec![(box_b_id, box_b_bytes)],
            },
            TxSummary {
                input_ids: vec![box_a], // double spend!
                data_input_ids: vec![],
                output_entries: vec![(box_c_id, box_c_bytes)],
            },
        ];

        let result = compute_state_changes(tx_summaries);
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ergo-validation -- state_changes`
Expected: FAIL — types and functions not defined

- [ ] **Step 3: Implement state changes**

In `validation/src/state_changes.rs`:

```rust
use std::collections::{HashMap, HashSet};

use crate::ValidationError;

/// Summary of a transaction's inputs and outputs for state change computation.
/// Decoupled from ergo-lib's Transaction type to allow isolated testing.
pub struct TxSummary {
    /// BoxIds of inputs being spent.
    pub input_ids: Vec<[u8; 32]>,
    /// BoxIds of data inputs (read-only lookups).
    pub data_input_ids: Vec<[u8; 32]>,
    /// (BoxId, serialized_box_bytes) for each output.
    pub output_entries: Vec<([u8; 32], Vec<u8>)>,
}

/// AVL+ tree operations derived from a block's transactions.
pub struct StateChanges {
    /// Data input lookups (box_id). Applied first.
    pub lookups: Vec<[u8; 32]>,
    /// Input removals (box_id). Applied second.
    pub removals: Vec<[u8; 32]>,
    /// Output insertions (box_id, serialized_box_bytes). Applied third.
    pub insertions: Vec<([u8; 32], Vec<u8>)>,
}

/// Compute AVL+ tree operations from transaction summaries.
///
/// Handles intra-block netting: if tx2 spends an output created by tx1 in
/// the same block, neither a Remove nor Insert hits the tree (net-zero).
/// Rejects intra-block double-spends.
///
/// Operation order matches JVM: lookups, then removals, then insertions.
pub fn compute_state_changes(
    tx_summaries: Vec<TxSummary>,
) -> Result<StateChanges, ValidationError> {
    let mut pending_inserts: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
    let mut spent: HashSet<[u8; 32]> = HashSet::new();
    let mut removals: Vec<[u8; 32]> = Vec::new();
    let mut lookups: Vec<[u8; 32]> = Vec::new();

    for summary in &tx_summaries {
        for &input_id in &summary.input_ids {
            if !spent.insert(input_id) {
                return Err(ValidationError::IntraBlockDoubleSpend(
                    hex::encode(input_id),
                ));
            }
            if pending_inserts.remove(&input_id).is_none() {
                // Not an intra-block output — must be removed from tree
                removals.push(input_id);
            }
            // else: netted out (created and spent within this block)
        }

        for entry in &summary.output_entries {
            pending_inserts.insert(entry.0, entry.1.clone());
        }

        for &data_input_id in &summary.data_input_ids {
            lookups.push(data_input_id);
        }
    }

    // Remaining pending_inserts are outputs that weren't spent intra-block
    let insertions: Vec<([u8; 32], Vec<u8>)> = pending_inserts.into_iter().collect();

    Ok(StateChanges {
        lookups,
        removals,
        insertions,
    })
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p ergo-validation -- state_changes`
Expected: PASS (3 tests)

- [ ] **Step 5: Add helper to convert Transaction → TxSummary**

This bridges ergo-lib's Transaction type to our TxSummary:

```rust
use ergo_lib::chain::transaction::Transaction;
use sigma_ser::ScorexSerializable;

/// Convert ergo-lib Transactions into TxSummaries for state change computation.
pub fn transactions_to_summaries(
    transactions: &[Transaction],
) -> Result<Vec<TxSummary>, ValidationError> {
    let mut summaries = Vec::with_capacity(transactions.len());

    for (i, tx) in transactions.iter().enumerate() {
        let input_ids: Vec<[u8; 32]> = tx
            .inputs
            .iter()
            .map(|input| {
                let bytes: Vec<u8> = input.box_id.into();
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                arr
            })
            .collect();

        let data_input_ids: Vec<[u8; 32]> = tx
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

        let output_entries: Vec<([u8; 32], Vec<u8>)> = tx
            .outputs
            .iter()
            .map(|output| {
                let id_bytes: Vec<u8> = output.box_id().into();
                let mut id_arr = [0u8; 32];
                id_arr.copy_from_slice(&id_bytes);
                let box_bytes = output
                    .sigma_serialize_bytes()
                    .map_err(|e| ValidationError::SectionParse {
                        section_type: 102,
                        reason: format!("output serialization: {e}"),
                    })?;
                Ok((id_arr, box_bytes))
            })
            .collect::<Result<Vec<_>, ValidationError>>()?;

        summaries.push(TxSummary {
            input_ids,
            data_input_ids,
            output_entries,
        });
    }

    Ok(summaries)
}
```

Note: the exact conversions from `BoxId` to `[u8; 32]` depend on ergo-lib's API. `BoxId` implements `Into<Vec<u8>>` and has a `SIZE = 32`. If the API differs slightly (e.g., `BoxId` exposes `.0` as `Digest32`), adjust the conversion accordingly. Check `ergotree-ir/src/chain/ergo_box/box_id.rs` for the exact API.

- [ ] **Step 6: Run all tests**

Run: `cargo test -p ergo-validation`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add validation/src/state_changes.rs
git commit -m "implement state changes computation with intra-block netting"
```

---

### Task 6: DigestValidator — AD proof verification

**Files:**
- Modify: `validation/src/digest.rs`
- Create: `validation/tests/fixtures/` (test data from testnet)

This is the core: wire `BatchAVLVerifier` with parsed sections and state changes to verify state root transitions.

- [ ] **Step 1: Extract test fixtures from the testnet node**

SSH to the test server (`216.128.144.28`) and fetch raw section bytes for a few consecutive blocks. Use the JVM API to get header data and the Rust store for raw section bytes.

Fetch headers from JVM API:
```bash
# On test server:
# Get block IDs at height 1, 2, 3
curl -s http://127.0.0.1:9063/blocks/at/1 | python3 -m json.tool
curl -s http://127.0.0.1:9063/blocks/at/2 | python3 -m json.tool
# Get full header for each
curl -s http://127.0.0.1:9063/blocks/<id>/header | python3 -m json.tool
```

For raw section bytes, write a small Rust helper or read from the redb store directly. Save as hex files in `validation/tests/fixtures/`:
- `header_1.json` — header at height 1 (JSON from JVM API)
- `header_2.json` — header at height 2
- `block_txs_1.hex` — BlockTransactions section raw bytes (hex)
- `ad_proofs_1.hex` — ADProofs section raw bytes (hex)
- `extension_1.hex` — Extension section raw bytes (hex)
- Same for height 2

Also record the genesis state root (testnet):
```
cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502
```

- [ ] **Step 2: Implement DigestValidator**

In `validation/src/digest.rs`:

```rust
use blake2::digest::Digest as Blake2Digest;
use blake2::Blake2b256;
use bytes::Bytes;
use ergo_avltree_rust::batch_avl_verifier::BatchAVLVerifier;
use ergo_avltree_rust::batch_node::AVLTree;
use ergo_avltree_rust::operation::{ADKey, ADValue, KeyValue, Operation};
use ergo_chain_types::{ADDigest, Header};

use crate::sections::{parse_ad_proofs, parse_block_transactions};
use crate::state_changes::{compute_state_changes, transactions_to_summaries};
use crate::{BlockValidator, ValidationError};

/// Key length for Ergo's UTXO AVL+ tree (BoxId = 32 bytes).
const KEY_LENGTH: usize = 32;

/// Dummy resolver for the AVL tree (verifier doesn't need to load nodes from storage).
fn dummy_resolver(_digest: &[u8; 32]) -> ergo_avltree_rust::batch_node::Node {
    ergo_avltree_rust::batch_node::Node::LabelOnly(
        ergo_avltree_rust::batch_node::NodeHeader::new(
            None,
            vec![0u8; 32],
            None,
        ),
    )
}

/// Digest-mode block validator.
///
/// Verifies state transitions using AD proofs and BatchAVLVerifier.
/// No persistent UTXO set — just tracks the current state root.
pub struct DigestValidator {
    current_digest: ADDigest,
    validated_height: u32,
    checkpoint_height: u32,
}

impl DigestValidator {
    /// Create a new DigestValidator starting from genesis.
    ///
    /// `genesis_digest`: ADDigest of the genesis UTXO state (33 bytes, per-network constant).
    /// `checkpoint_height`: skip script validation at or below this height (0 = validate all).
    pub fn new(genesis_digest: ADDigest, checkpoint_height: u32) -> Self {
        Self {
            current_digest: genesis_digest,
            validated_height: 0,
            checkpoint_height,
        }
    }

    /// Create a DigestValidator resuming from a known state.
    pub fn from_state(digest: ADDigest, height: u32, checkpoint_height: u32) -> Self {
        Self {
            current_digest: digest,
            validated_height: height,
            checkpoint_height,
        }
    }
}

impl BlockValidator for DigestValidator {
    fn validate_block(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        _preceding_headers: &[Header],
    ) -> Result<(), ValidationError> {
        // Height check
        let expected_height = self.validated_height + 1;
        if header.height != expected_height {
            return Err(ValidationError::HeightMismatch {
                expected: expected_height,
                got: header.height,
            });
        }

        // AD proofs required in digest mode
        let proof_data = ad_proofs.ok_or(ValidationError::MissingProof)?;

        // 1. Parse sections
        let parsed_proofs = parse_ad_proofs(proof_data)?;
        let parsed_txs = parse_block_transactions(block_txs)?;
        // Extension is parsed but not used until script validation is enabled
        let _parsed_ext = crate::sections::parse_extension(extension)?;

        // 2. Verify AD proofs digest matches header
        let proof_digest: [u8; 32] = Blake2b256::digest(&parsed_proofs.proof_bytes).into();
        let expected_digest: [u8; 32] = header.ad_proofs_root.into();
        if proof_digest != expected_digest {
            return Err(ValidationError::ProofDigestMismatch {
                expected: expected_digest,
                got: proof_digest,
            });
        }

        // 3. Compute state changes from transactions
        let summaries = transactions_to_summaries(&parsed_txs.transactions)?;
        let changes = compute_state_changes(summaries)?;

        // 4. Build AVL operations
        let mut operations: Vec<Operation> = Vec::new();

        for lookup_id in &changes.lookups {
            operations.push(Operation::Lookup(ADKey::from(
                Bytes::copy_from_slice(lookup_id),
            )));
        }
        for removal_id in &changes.removals {
            operations.push(Operation::Remove(ADKey::from(
                Bytes::copy_from_slice(removal_id),
            )));
        }
        for (insert_id, insert_value) in &changes.insertions {
            operations.push(Operation::Insert(KeyValue {
                key: Bytes::copy_from_slice(insert_id),
                value: Bytes::copy_from_slice(insert_value),
            }));
        }

        // 5. Verify AD proof via BatchAVLVerifier
        let starting_digest = Bytes::copy_from_slice(
            &<[u8; 33]>::from(self.current_digest.clone()),
        );
        let proof_bytes = Bytes::copy_from_slice(&parsed_proofs.proof_bytes);

        let tree = AVLTree::new(dummy_resolver, KEY_LENGTH, None);
        let mut verifier = BatchAVLVerifier::new(
            &starting_digest,
            &proof_bytes,
            tree,
            Some(operations.len()),
            None,
        )
        .map_err(|e| ValidationError::ProofVerificationFailed(format!("{e}")))?;

        for op in &operations {
            verifier
                .perform_one_operation(op)
                .map_err(|e| ValidationError::ProofVerificationFailed(format!("{e}")))?;
        }

        // 6. Check resulting digest matches header.state_root
        let verifier_digest = verifier.digest();
        let expected_state_root: Vec<u8> = self.current_digest_to_expected(header);

        match &verifier_digest {
            Some(d) if d.as_ref() == expected_state_root.as_slice() => {}
            Some(d) => {
                return Err(ValidationError::StateRootMismatch {
                    expected: expected_state_root,
                    got: d.to_vec(),
                });
            }
            None => {
                return Err(ValidationError::ProofVerificationFailed(
                    "verifier digest is None after operations".to_string(),
                ));
            }
        }

        // 7. Advance state
        self.current_digest = header.state_root.clone();
        self.validated_height = header.height;

        tracing::debug!(
            height = header.height,
            "block validated (digest mode)"
        );

        Ok(())
    }

    fn validated_height(&self) -> u32 {
        self.validated_height
    }

    fn current_digest(&self) -> &ADDigest {
        &self.current_digest
    }

    fn reset_to(&mut self, height: u32, digest: ADDigest) {
        self.validated_height = height;
        self.current_digest = digest;
        tracing::info!(height, "validator reset to fork point");
    }
}

impl DigestValidator {
    fn current_digest_to_expected(&self, header: &Header) -> Vec<u8> {
        let arr: [u8; 33] = header.state_root.clone().into();
        arr.to_vec()
    }
}
```

**Important implementation notes:**
- The `dummy_resolver` is needed because `AVLTree::new` requires a resolver function, but the verifier never calls it (it reconstructs the partial tree from the proof).
- The exact `NodeHeader::new` constructor may differ — check `ergo_avltree_rust::batch_node` for the actual API. If `Node::LabelOnly` requires different parameters, adjust accordingly.
- The `ADDigest` conversion between ergo-chain-types (`Digest<33>`) and ergo_avltree_rust (`Bytes`) is via `[u8; 33]` intermediary.
- The operation order MUST be: lookups, removals, insertions (matching the JVM).

- [ ] **Step 3: Write test with real block fixtures**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_single_block() {
        // Load fixtures (header, sections, genesis digest)
        // These come from the testnet — see Task 6 Step 1 for extraction.
        //
        // The test constructs a DigestValidator with the genesis digest,
        // then validates block at height 1, and checks that:
        // - validated_height becomes 1
        // - current_digest matches header_1.state_root

        let genesis_digest = ADDigest::try_from(
            hex::decode("cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502")
                .unwrap()
        ).unwrap();

        let mut validator = DigestValidator::new(genesis_digest, 0);
        assert_eq!(validator.validated_height(), 0);

        // Load real block data from fixtures
        // let header_1 = ... (parse from fixture)
        // let block_txs_1 = hex::decode(include_str!("../../tests/fixtures/block_txs_1.hex")).unwrap();
        // let ad_proofs_1 = hex::decode(include_str!("../../tests/fixtures/ad_proofs_1.hex")).unwrap();
        // let extension_1 = hex::decode(include_str!("../../tests/fixtures/extension_1.hex")).unwrap();

        // validator.validate_block(&header_1, &block_txs_1, Some(&ad_proofs_1), &extension_1, &[]).unwrap();
        // assert_eq!(validator.validated_height(), 1);

        // TODO: uncomment when fixtures are extracted from testnet
    }
}
```

The actual assertions depend on the extracted fixtures. Once extracted, fill in the real data and remove the TODO.

- [ ] **Step 4: Run tests and iterate**

Run: `cargo test -p ergo-validation`

This is the most likely step to require debugging. Common issues:
- `ADDigest` conversion: the 33-byte digest from ergo-chain-types vs the `Bytes` type in ergo_avltree_rust
- Operation ordering: must match JVM's Lookups → Removes → Inserts
- Box serialization: the serialized bytes for Insert operations must match what the original prover used
- `dummy_resolver` construction: the API may require specific parameters

Fix issues as they arise, re-run tests. The AD proof verifier is deterministic — if the inputs are correct, verification succeeds.

- [ ] **Step 5: Validate multiple consecutive blocks**

Once single-block validation works, extend the test to validate 2-3 consecutive blocks to confirm the state root chains correctly:

```rust
#[test]
fn validate_consecutive_blocks() {
    // Validate blocks 1, 2, 3 in sequence.
    // After each, check validated_height and current_digest.
    // This confirms state roots chain correctly across blocks.
}
```

- [ ] **Step 6: Commit**

```bash
git add validation/
git commit -m "implement DigestValidator with AD proof verification"
```

---

### Task 7: Sync machine — rename full_block_height to downloaded_height

**Files:**
- Modify: `sync/src/state.rs`

Mechanical rename. No behavior change.

- [ ] **Step 1: Rename the field and all references**

In `sync/src/state.rs`, rename every occurrence of `full_block_height` to `downloaded_height`. This includes:
- The struct field (line 97)
- The initialization in `new()` (line 126)
- The `advance_full_block_height` method → `advance_downloaded_height` (line 400)
- All call sites (~12 occurrences total — see grep output)
- All log messages and comments

- [ ] **Step 2: Verify compilation**

Run: `cargo check -p ergo-sync`
Expected: compiles with no errors

- [ ] **Step 3: Run existing tests**

Run: `cargo test -p ergo-sync`
Expected: all existing tests pass (name change only)

- [ ] **Step 4: Commit**

```bash
git add sync/src/state.rs
git commit -m "rename full_block_height to downloaded_height"
```

---

### Task 8: Sync machine — add validated_height and SyncStore::get_modifier

**Files:**
- Modify: `sync/Cargo.toml`
- Modify: `sync/src/traits.rs`
- Modify: `sync/src/state.rs`

- [ ] **Step 1: Add ergo-validation dependency to sync**

In `sync/Cargo.toml`:
```toml
ergo-validation = { path = "../validation" }
```

- [ ] **Step 2: Add get_modifier to SyncStore trait**

In `sync/src/traits.rs`:
```rust
/// Retrieve raw modifier bytes from the store.
/// Returns None if the modifier doesn't exist.
fn get_modifier(
    &self,
    type_id: u8,
    id: &[u8; 32],
) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send;
```

- [ ] **Step 3: Add validated_height field and validator to HeaderSync**

In `sync/src/state.rs`, add a type parameter for the validator and the new field:

```rust
use ergo_validation::BlockValidator;

pub struct HeaderSync<T: SyncTransport, C: SyncChain, S: SyncStore, V: BlockValidator> {
    // ... existing fields ...
    validator: V,
    /// Highest height where block sections have been validated.
    validated_height: u32,
}
```

Update `new()` to accept the validator:
```rust
pub fn new(
    config: SyncConfig,
    transport: T,
    chain: C,
    store: S,
    progress: mpsc::Receiver<u32>,
    delivery_rx: mpsc::Receiver<DeliveryEvent>,
    validator: V,
) -> Self {
    // ... existing init ...
    // Add:
    validator,
    validated_height: 0,
}
```

- [ ] **Step 4: Implement advance_validated_height**

```rust
/// Advance the validated height by running the block validator on
/// downloaded-but-not-yet-validated blocks.
async fn advance_validated_height(&mut self) {
    if self.validated_height >= self.downloaded_height {
        return;
    }

    let start = self.validated_height + 1;
    let mut validated_to = self.validated_height;

    for height in start..=self.downloaded_height {
        let header = match self.chain.header_at(height).await {
            Some(h) => h,
            None => break,
        };

        // Get required section IDs for this header
        let sections = enr_chain::required_section_ids(&header, self.config.state_type);

        // Fetch section bytes from store
        let mut block_txs = None;
        let mut ad_proofs = None;
        let mut extension = None;

        for (type_id, id) in &sections {
            let data = match self.store.get_modifier(*type_id, id).await {
                Some(d) => d,
                None => {
                    tracing::warn!(height, type_id, "section bytes missing during validation");
                    return; // stop — section not available
                }
            };
            match *type_id {
                enr_chain::BLOCK_TRANSACTIONS_TYPE_ID => block_txs = Some(data),
                enr_chain::AD_PROOFS_TYPE_ID => ad_proofs = Some(data),
                enr_chain::EXTENSION_TYPE_ID => extension = Some(data),
                _ => {}
            }
        }

        let block_txs = block_txs.expect("BlockTransactions required");
        let extension = extension.expect("Extension required");

        // Get preceding headers (up to 10) for future ErgoStateContext
        let preceding_start = height.saturating_sub(10);
        let mut preceding = Vec::new();
        for h in (preceding_start..height).rev() {
            if h == 0 { break; }
            if let Some(hdr) = self.chain.header_at(h).await {
                preceding.push(hdr);
            }
        }

        // Validate
        let result = self.validator.validate_block(
            &header,
            &block_txs,
            ad_proofs.as_deref(),
            &extension,
            &preceding,
        );

        match result {
            Ok(()) => {
                validated_to = height;
            }
            Err(e) => {
                tracing::error!(height, error = %e, "block validation failed");
                break;
            }
        }
    }

    if validated_to > self.validated_height {
        let advanced = validated_to - self.validated_height;
        self.validated_height = validated_to;
        tracing::info!(
            validated_height = validated_to,
            advanced,
            downloaded_height = self.downloaded_height,
            "validated height advanced"
        );
    }
}
```

- [ ] **Step 5: Call advance_validated_height after downloaded_height advances**

In `advance_downloaded_height()`, after the height is updated, add a call:
```rust
if new_height > self.downloaded_height {
    // ... existing logging ...
    self.downloaded_height = new_height;
    self.advance_validated_height().await;
}
```

Also call it from the reorg handler, after resetting:
```rust
// In the Reorg handler:
self.validator.reset_to(fork_point, header.state_root.clone());
self.validated_height = fork_point;
// Then after re-downloading and advancing downloaded_height:
self.advance_validated_height().await;
```

- [ ] **Step 6: Verify compilation**

Run: `cargo check -p ergo-sync`

This will fail because existing code that constructs `HeaderSync` doesn't pass a validator yet. That's expected — fixed in Task 9.

- [ ] **Step 7: Commit**

```bash
git add sync/
git commit -m "add validated_height watermark and SyncStore::get_modifier to sync machine"
```

---

### Task 9: Bridge + main — wire DigestValidator into the node

**Files:**
- Modify: `src/bridge.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add get_modifier to SharedStore bridge**

In `src/bridge.rs`, add the `get_modifier` implementation:

```rust
impl SyncStore for SharedStore {
    // ... existing has_modifier ...

    async fn get_modifier(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>> {
        let store = self.store.clone();
        let type_id = type_id;
        let id = *id;
        tokio::task::spawn_blocking(move || {
            match store.get(type_id, &id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("store.get failed: {e}");
                    None
                }
            }
        })
        .await
        .unwrap_or(None)
    }
}
```

- [ ] **Step 2: Create DigestValidator in main and pass to sync**

In `src/main.rs`, after chain config setup:

```rust
use ergo_validation::DigestValidator;
use ergo_chain_types::ADDigest;

// Genesis state root (testnet)
let genesis_digest = ADDigest::try_from(
    hex::decode("cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502")
        .unwrap()
        .as_slice()
).expect("invalid genesis digest");

// Create validator
// checkpoint_height 0 = validate everything (no skip)
let validator = DigestValidator::new(genesis_digest, 0);
```

Choose the genesis digest based on network (testnet vs mainnet):
```rust
let genesis_digest_hex = match config.proxy.network {
    enr_p2p::types::Network::Testnet =>
        "cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502",
    enr_p2p::types::Network::Mainnet =>
        "a5df145d41ab15a01e0cd3ffbab046f0d029e5412293072ad0f5827428589b9302",
};
```

Pass the validator to `HeaderSync::new()`.

- [ ] **Step 3: Add hex dependency to root Cargo.toml**

```toml
hex = "0.4"
```

- [ ] **Step 4: Update lib.rs exports if needed**

Ensure `ValidationPipeline` and any new public types are exported.

- [ ] **Step 5: Build the full binary**

Run: `cargo build`
Expected: compiles with no errors

- [ ] **Step 6: Commit**

```bash
git add src/ Cargo.toml
git commit -m "wire DigestValidator into node startup and sync machine"
```

---

### Task 10: Integration test — validate real chain data

**Files:**
- Modify: `validation/src/digest.rs` (fill in fixture-based tests)

- [ ] **Step 1: Deploy to test server and run**

Build and deploy the binary to the test server:
```bash
cargo build --release
scp target/release/ergo-node-rust admin@216.128.144.28:/tmp/
```

On the test server, run with the existing config:
```bash
sudo systemctl stop ergo-node
/tmp/ergo-node-rust /etc/ergo-node/ergo.toml
```

Watch the logs. After header sync completes and block sections are downloaded, the validation should start. You should see log messages like:
```
validated height advanced validated_height=100 advanced=100
```

- [ ] **Step 2: Monitor for validation errors**

If validation fails at a specific height, the log will show:
```
block validation failed height=N error="..."
```

Common failures and fixes:
- **StateRootMismatch**: operation ordering is wrong, or box serialization differs
- **ProofVerificationFailed**: the proof bytes aren't being passed correctly
- **ProofDigestMismatch**: the proof bytes extraction from the section is off

Debug by comparing with the JVM at that height:
```bash
curl -s http://127.0.0.1:9063/blocks/at/N | python3 -m json.tool
```

- [ ] **Step 3: Verify N consecutive blocks validate successfully**

Let the node run until `validated_height` reaches at least 1000 (or as many blocks as have been downloaded). This confirms the state root chains correctly across many blocks.

- [ ] **Step 4: Restore systemd service**

```bash
sudo systemctl start ergo-node
```

Or update the systemd service to use the new binary if testing is successful.

- [ ] **Step 5: Commit any fixes discovered during integration**

```bash
git add -A
git commit -m "fix: integration issues found during testnet validation"
```

---

## Implementation Notes

### Type conversions

The most likely pain point is converting between ergo-chain-types and ergo_avltree_rust types:
- `ergo_chain_types::ADDigest` = `Digest<33>` = `[u8; 33]` wrapper
- `ergo_avltree_rust::ADDigest` = `bytes::Bytes` (variable length)
- Convert via: `Bytes::copy_from_slice(&<[u8; 33]>::from(digest))`
- Convert back: `ADDigest::try_from(bytes.as_ref())`

### Box serialization format

The Insert operations need serialized box bytes matching what the original prover used. This is `ErgoBox::sigma_serialize_bytes()` — the standard Scorex serialization including tx_id and index. If the verification fails with `StateRootMismatch`, the first thing to check is whether the serialization matches.

### JVM reference for debugging

When a specific block fails validation, cross-reference with the JVM:
```bash
# Get block at height N
curl -s http://127.0.0.1:9063/blocks/at/N
# Get full block details
curl -s http://127.0.0.1:9063/blocks/<header_id>
# Get AD proofs
curl -s http://127.0.0.1:9063/blocks/<header_id>/proofFor/<tx_id>
```

### Checkpoint optimization

The plan implements with `checkpoint_height = 0` (validate everything). For faster sync, set checkpoint to a known-good height. The validation logic already supports this — script validation (step 5 in the contract) is skipped below checkpoint. Since we haven't implemented script validation yet, this is effectively always in checkpoint mode.

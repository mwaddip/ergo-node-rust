//! Canonical per-block blake2b256 digest.
//!
//! Backend-agnostic encoding: bytes derived from SQLite vs. PostgreSQL, once
//! parsed into the same `BlockData`, MUST hash identically. The hash is the
//! consensus check between source and target during migration — if encoding
//! drifts, the migrator reports false mismatches forever.
//!
//! Encoding contract (per facts/indexer-migration.md § Hash verification):
//!   1. blocks row at H
//!   2. transactions sorted by tx_index
//!   3. created_boxes sorted by box_id
//!   4. box_registers in (box_id, register_id) order
//!   5. box_tokens in (box_id, token_id) order
//!   6. minted_tokens sorted by token_id
//!   7. spent_box_updates sorted by box_id
//!
//! Determinism rules within each row:
//!   - fixed-width integers → `to_le_bytes()` then FIELD_SEP
//!   - fixed-width arrays (`[u8; 32]`) → raw bytes then FIELD_SEP
//!   - variable-length blobs (`Vec<u8>`) → 4-byte LE length prefix, then bytes,
//!     then FIELD_SEP
//!   - strings → 4-byte LE length prefix on UTF-8 bytes, then bytes, then FIELD_SEP
//!   - `Option<T>` → 1-byte discriminator (0x00 None, 0x01 Some), then (if Some)
//!     inner encoding, then FIELD_SEP
//!
//! Each row ends with ROW_SEP (0xFF). Length prefixes inside fields make ROW_SEP
//! redundant for unambiguous parsing but the spec keeps it for visual sanity.
//!
//! Note on sort-key tie-breaking: a single box can carry multiple registers
//! (R4–R9) and multiple tokens, so `box_id` alone is not unique within
//! `box_registers`/`box_tokens`. Composite `(box_id, register_id)` /
//! `(box_id, token_id)` ordering is used. This is a stability extension of the
//! "in box_id order" spec phrasing — the spec key was inherently incomplete.

use super::{
    BlockData, BlockRow, BoxRegisterRow, BoxRow, BoxSpentUpdate, BoxTokenRow, TokenRow,
    TransactionRow,
};
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};

pub type BlockHash = [u8; 32];

type Hasher = Blake2b<U32>;

const FIELD_SEP: u8 = 0x00;
const ROW_SEP: u8 = 0xFF;

/// Compute the canonical blake2b256 digest of a block's content.
///
/// Pure: same `BlockData` → same hash. The input may arrive with rows in any
/// order — sorting is applied before hashing so backend-side row ordering
/// never leaks into the digest.
pub fn hash_block(data: &BlockData) -> BlockHash {
    let mut hasher = Hasher::new();

    // 1. blocks row
    write_block_row(&mut hasher, &data.block);
    hasher.update([ROW_SEP]);

    // 2. transactions sorted by tx_index
    let mut txs: Vec<&TransactionRow> = data.transactions.iter().collect();
    txs.sort_by_key(|t| t.tx_index);
    for tx in txs {
        write_tx_row(&mut hasher, tx);
        hasher.update([ROW_SEP]);
    }

    // 3. created_boxes sorted by box_id
    let mut boxes: Vec<&BoxRow> = data.created_boxes.iter().collect();
    boxes.sort_by(|a, b| a.box_id.cmp(&b.box_id));
    for b in boxes {
        write_box_row(&mut hasher, b);
        hasher.update([ROW_SEP]);
    }

    // 4. box_registers in (box_id, register_id) order
    let mut regs: Vec<&BoxRegisterRow> = data.box_registers.iter().collect();
    regs.sort_by(|a, b| {
        a.box_id
            .cmp(&b.box_id)
            .then_with(|| a.register_id.cmp(&b.register_id))
    });
    for r in regs {
        write_box_register_row(&mut hasher, r);
        hasher.update([ROW_SEP]);
    }

    // 5. box_tokens in (box_id, token_id) order
    let mut tkns: Vec<&BoxTokenRow> = data.box_tokens.iter().collect();
    tkns.sort_by(|a, b| {
        a.box_id
            .cmp(&b.box_id)
            .then_with(|| a.token_id.cmp(&b.token_id))
    });
    for t in tkns {
        write_box_token_row(&mut hasher, t);
        hasher.update([ROW_SEP]);
    }

    // 6. minted_tokens sorted by token_id
    let mut mts: Vec<&TokenRow> = data.minted_tokens.iter().collect();
    mts.sort_by(|a, b| a.token_id.cmp(&b.token_id));
    for t in mts {
        write_token_row(&mut hasher, t);
        hasher.update([ROW_SEP]);
    }

    // 7. spent_box_updates sorted by box_id
    let mut spent: Vec<&BoxSpentUpdate> = data.spent_box_updates.iter().collect();
    spent.sort_by(|a, b| a.box_id.cmp(&b.box_id));
    for s in spent {
        write_spent_update_row(&mut hasher, s);
        hasher.update([ROW_SEP]);
    }

    hasher.finalize().into()
}

// ---------------------------------------------------------------------------
// Field encoders
// ---------------------------------------------------------------------------

/// Variable-length blob: 4-byte LE length prefix, then bytes.
fn write_blob(h: &mut Hasher, b: &[u8]) {
    let len = b.len() as u32;
    h.update(len.to_le_bytes());
    h.update(b);
}

/// UTF-8 string: same shape as blob (4-byte LE len prefix + bytes).
fn write_string(h: &mut Hasher, s: &str) {
    write_blob(h, s.as_bytes());
}

/// `Option<[u8; 32]>`: 0x00 None, or 0x01 followed by 32 raw bytes.
fn write_opt_id(h: &mut Hasher, v: &Option<[u8; 32]>) {
    match v {
        None => h.update([0u8]),
        Some(id) => {
            h.update([1u8]);
            h.update(id);
        }
    }
}

/// `Option<u32>`: 0x00 None, or 0x01 followed by 4 LE bytes.
fn write_opt_u32(h: &mut Hasher, v: &Option<u32>) {
    match v {
        None => h.update([0u8]),
        Some(n) => {
            h.update([1u8]);
            h.update(n.to_le_bytes());
        }
    }
}

/// `Option<i32>`: 0x00 None, or 0x01 followed by 4 LE bytes.
fn write_opt_i32(h: &mut Hasher, v: &Option<i32>) {
    match v {
        None => h.update([0u8]),
        Some(n) => {
            h.update([1u8]);
            h.update(n.to_le_bytes());
        }
    }
}

/// `Option<String>`: 0x00 None, or 0x01 followed by length-prefixed UTF-8.
fn write_opt_string(h: &mut Hasher, v: &Option<String>) {
    match v {
        None => h.update([0u8]),
        Some(s) => {
            h.update([1u8]);
            write_string(h, s);
        }
    }
}

// ---------------------------------------------------------------------------
// Row encoders
// ---------------------------------------------------------------------------

fn write_block_row(h: &mut Hasher, b: &BlockRow) {
    h.update(b.height.to_le_bytes());
    h.update([FIELD_SEP]);
    h.update(b.header_id);
    h.update([FIELD_SEP]);
    h.update(b.timestamp.to_le_bytes());
    h.update([FIELD_SEP]);
    h.update(b.difficulty.to_le_bytes());
    h.update([FIELD_SEP]);
    write_blob(h, &b.miner_pk);
    h.update([FIELD_SEP]);
    h.update(b.block_size.to_le_bytes());
    h.update([FIELD_SEP]);
    h.update(b.tx_count.to_le_bytes());
    h.update([FIELD_SEP]);
}

fn write_tx_row(h: &mut Hasher, t: &TransactionRow) {
    h.update(t.tx_id);
    h.update([FIELD_SEP]);
    h.update(t.header_id);
    h.update([FIELD_SEP]);
    h.update(t.height.to_le_bytes());
    h.update([FIELD_SEP]);
    h.update(t.tx_index.to_le_bytes());
    h.update([FIELD_SEP]);
    h.update(t.size.to_le_bytes());
    h.update([FIELD_SEP]);
}

fn write_box_row(h: &mut Hasher, b: &BoxRow) {
    h.update(b.box_id);
    h.update([FIELD_SEP]);
    h.update(b.tx_id);
    h.update([FIELD_SEP]);
    h.update(b.header_id);
    h.update([FIELD_SEP]);
    h.update(b.height.to_le_bytes());
    h.update([FIELD_SEP]);
    h.update(b.output_index.to_le_bytes());
    h.update([FIELD_SEP]);
    write_blob(h, &b.ergo_tree);
    h.update([FIELD_SEP]);
    h.update(b.ergo_tree_hash);
    h.update([FIELD_SEP]);
    write_string(h, &b.address);
    h.update([FIELD_SEP]);
    h.update(b.value.to_le_bytes());
    h.update([FIELD_SEP]);
    write_opt_id(h, &b.spent_tx_id);
    h.update([FIELD_SEP]);
    write_opt_u32(h, &b.spent_height);
    h.update([FIELD_SEP]);
}

fn write_box_register_row(h: &mut Hasher, r: &BoxRegisterRow) {
    h.update(r.box_id);
    h.update([FIELD_SEP]);
    h.update([r.register_id]);
    h.update([FIELD_SEP]);
    write_blob(h, &r.serialized);
    h.update([FIELD_SEP]);
}

fn write_box_token_row(h: &mut Hasher, t: &BoxTokenRow) {
    h.update(t.box_id);
    h.update([FIELD_SEP]);
    h.update(t.token_id);
    h.update([FIELD_SEP]);
    h.update(t.amount.to_le_bytes());
    h.update([FIELD_SEP]);
}

fn write_token_row(h: &mut Hasher, t: &TokenRow) {
    h.update(t.token_id);
    h.update([FIELD_SEP]);
    h.update(t.minting_tx_id);
    h.update([FIELD_SEP]);
    h.update(t.minting_height.to_le_bytes());
    h.update([FIELD_SEP]);
    write_opt_string(h, &t.name);
    h.update([FIELD_SEP]);
    write_opt_string(h, &t.description);
    h.update([FIELD_SEP]);
    write_opt_i32(h, &t.decimals);
    h.update([FIELD_SEP]);
}

fn write_spent_update_row(h: &mut Hasher, s: &BoxSpentUpdate) {
    h.update(s.box_id);
    h.update([FIELD_SEP]);
    h.update(s.spent_tx_id);
    h.update([FIELD_SEP]);
    h.update(s.spent_height.to_le_bytes());
    h.update([FIELD_SEP]);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic synthetic block data keyed by a seed byte. Different
    /// seeds MUST produce different `BlockData` (verified by
    /// `differing_data_produces_differing_hash`); same seed MUST reproduce
    /// byte-identical structures.
    fn synth_block_data(seed: u8) -> BlockData {
        let header_id = [seed; 32];

        let block = BlockRow {
            height: 100u32.wrapping_add(seed as u32),
            header_id,
            timestamp: 1_700_000_000_000i64 + seed as i64,
            difficulty: 1_000_000i64 + seed as i64,
            // miner_pk is a deterministic 33-byte sequence; not a real Ergo
            // miner_pk but the hasher only cares about bytes.
            miner_pk: (0u8..33).map(|i| seed.wrapping_add(i)).collect(),
            block_size: 1000u32 + seed as u32,
            tx_count: 2,
        };

        // 2 transactions
        let mut transactions = Vec::with_capacity(2);
        for i in 0u32..2 {
            transactions.push(TransactionRow {
                tx_id: [seed.wrapping_add(i as u8); 32],
                header_id,
                height: block.height,
                tx_index: i,
                size: 200u32 + i + seed as u32,
            });
        }

        // 2 created boxes (one per tx)
        let mut created_boxes = Vec::with_capacity(2);
        for i in 0u32..2 {
            created_boxes.push(BoxRow {
                box_id: [seed.wrapping_mul(2).wrapping_add(i as u8); 32],
                tx_id: transactions[i as usize].tx_id,
                header_id,
                height: block.height,
                output_index: i,
                ergo_tree: vec![0x10u8, 0x01, 0x04, seed],
                ergo_tree_hash: [seed.wrapping_add(99); 32],
                address: format!("9addr_{}_{}", seed, i),
                value: 1_000_000i64 + i as i64 + seed as i64,
                // Created-this-block rows are unspent here; spent updates
                // live in spent_box_updates.
                spent_tx_id: None,
                spent_height: None,
            });
        }

        // 1 register per box (register_id 4 = R4)
        let mut box_registers = Vec::with_capacity(2);
        for b in &created_boxes {
            box_registers.push(BoxRegisterRow {
                box_id: b.box_id,
                register_id: 4,
                serialized: vec![0x05u8, seed, 0x42],
            });
        }

        // 1 token per box
        let mut box_tokens = Vec::with_capacity(2);
        for b in &created_boxes {
            box_tokens.push(BoxTokenRow {
                box_id: b.box_id,
                token_id: [seed.wrapping_add(50); 32],
                amount: 100i64 + seed as i64,
            });
        }

        // 1 minted token
        let minted_tokens = vec![TokenRow {
            token_id: [seed.wrapping_add(77); 32],
            minting_tx_id: transactions[0].tx_id,
            minting_height: block.height,
            name: Some(format!("token_{}", seed)),
            description: None,
            decimals: Some(seed as i32),
        }];

        // 1 spent box update (some prior-height box spent at this H)
        let spent_box_updates = vec![BoxSpentUpdate {
            box_id: [seed.wrapping_add(200); 32],
            spent_tx_id: transactions[1].tx_id,
            spent_height: block.height,
        }];

        BlockData {
            block,
            transactions,
            created_boxes,
            box_registers,
            box_tokens,
            minted_tokens,
            spent_box_updates,
        }
    }

    #[test]
    fn identical_data_produces_identical_hash() {
        let a = synth_block_data(0x01);
        let b = synth_block_data(0x01);
        assert_eq!(hash_block(&a), hash_block(&b));
    }

    #[test]
    fn differing_data_produces_differing_hash() {
        let a = synth_block_data(0x01);
        let b = synth_block_data(0x02);
        assert_ne!(hash_block(&a), hash_block(&b));
    }

    #[test]
    fn hash_is_deterministic_across_runs() {
        let d = synth_block_data(0x42);
        let h1 = hash_block(&d);
        let h2 = hash_block(&d);
        let h3 = hash_block(&d);
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);
    }

    /// Critical for backend-agnosticism: input-row order MUST NOT affect the
    /// digest. SQLite and PG may return rows in different orders; the hash
    /// must be identical after the in-function sort.
    #[test]
    fn input_order_does_not_affect_hash() {
        let mut a = synth_block_data(0x07);
        let baseline = hash_block(&a);

        // Reverse every vec — this is the worst-case input order skew.
        a.transactions.reverse();
        a.created_boxes.reverse();
        a.box_registers.reverse();
        a.box_tokens.reverse();
        a.minted_tokens.reverse();
        a.spent_box_updates.reverse();

        assert_eq!(
            hash_block(&a),
            baseline,
            "row-order skew leaked into the digest — sort step is missing or weak"
        );
    }

    /// Guard against a regression where `Option::None` and `Option::Some([0; N])`
    /// could collide if the discriminator byte is absent.
    #[test]
    fn option_discriminator_separates_none_from_some_zero() {
        let mut a = synth_block_data(0x10);
        let mut b = a.clone();

        // Set the first box's spent_tx_id to Some([0; 32]); leave the other as
        // None. If the discriminator is dropped, this could collide with
        // None encoded as 32 zero bytes.
        a.created_boxes[0].spent_tx_id = None;
        b.created_boxes[0].spent_tx_id = Some([0u8; 32]);

        assert_ne!(hash_block(&a), hash_block(&b));
    }

    /// Length-prefix guard: two adjacent variable-length blobs whose
    /// concatenation matches a different split must not collide.
    #[test]
    fn length_prefixed_blobs_do_not_collide_under_concatenation() {
        let mut a = synth_block_data(0x20);
        let mut b = a.clone();

        // a.created_boxes[0].ergo_tree = [0x01, 0x02], address suffix encodes
        //   nothing relevant here — separate fields.
        // The real concatenation risk is within a single Vec<u8>; if the
        // length prefix is missed, [1,2] then [3,4] could equal [1,2,3] then [4].
        a.created_boxes[0].ergo_tree = vec![0x01, 0x02];
        a.box_registers[0].serialized = vec![0x03, 0x04];

        b.created_boxes[0].ergo_tree = vec![0x01, 0x02, 0x03];
        b.box_registers[0].serialized = vec![0x04];

        assert_ne!(
            hash_block(&a),
            hash_block(&b),
            "blob length prefix is missing — concatenation collision"
        );
    }
}

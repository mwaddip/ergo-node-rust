//! Transaction selection from mempool for block inclusion.

use std::collections::{HashMap, HashSet};

use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_validation::{validate_single_transaction, ErgoStateContext};

/// A transaction together with the two numbers the block limits are counted
/// in: the script cost `validate_single_transaction` charged for it, and the
/// serialized size it contributes to the block transactions section.
///
/// Selection returns these rather than bare transactions because the caller
/// has to keep counting after selection finishes — the fee transaction is
/// itself a transaction in the block, and bounding the assembled set needs
/// the per-transaction numbers that produced the selection.
#[derive(Clone, Debug)]
pub struct CostedTx {
    pub tx: Transaction,
    /// Script evaluation cost in block cost units.
    pub cost: u64,
    /// Serialized size in bytes.
    pub size: usize,
}

/// Sentinel the BlockTransactions serializer adds to the block version, so a
/// reader can tell a versioned section from a pre-v2 one where the first VLQ
/// *was* the transaction count (JVM `BlockTransactionsSerializer`,
/// `MaxTransactionsInBlock`).
const BLOCK_VERSION_SENTINEL: u64 = 10_000_000;

/// Framing bytes of the BlockTransactions section — everything the section
/// carries that is not a transaction:
///
/// ```text
/// [header_id: 32B][ver_or_count: VLQ][tx_count: VLQ if ver > 1][txs…]
/// ```
///
/// These count against `max_block_size`, because the limit is enforced over
/// the **section** and not over a sum of transactions: the JVM validates
/// `fb.blockTransactions.size <= currentParameters.maxBlockSize`
/// (`ErgoStateContext.scala:308-310`, rule `bsBlockTransactionsSize`), and
/// `.size` there is the serialized section. A candidate landing within this of
/// the limit is over it by the rule that decides validity, and gets rejected by
/// every peer. Comes to 37 bytes for a current-version block carrying fewer
/// than 128 transactions.
///
/// ⚠ **Not a safety margin, and must not become one.** The overhead is
/// deterministic and computable, so counting it is accuracy — the no-`safeGap`
/// decision (see the cost bound below) is untouched. The JVM's own generator
/// sums transaction sizes and carries the undercount
/// (`CandidateGenerator.correctLimits`); we do not copy that.
pub fn block_transactions_overhead(block_version: u8, tx_count: usize) -> usize {
    // JVM `BlockTransactionsSerializer.serialize`: 32 header-id bytes, then
    // `putUInt(MaxTransactionsInBlock + blockVersion)` for a versioned section,
    // then `putUInt(txs.size)`. A version-1 section omits the first field
    // entirely — the reader recognises the difference by the sentinel.
    let version_bytes = if block_version > 1 {
        vlq_len(BLOCK_VERSION_SENTINEL + u64::from(block_version))
    } else {
        0
    };
    32 + version_bytes + vlq_len(tx_count as u64)
}

/// Encoded length of `v` under Scorex's unsigned VLQ: 7 payload bits per byte,
/// high bit as the continuation flag.
///
/// Computed rather than encoded-and-measured, so the size accounting carries no
/// error path to swallow. `vlq_len_matches_the_encoder` pins it to `sigma_ser`'s
/// writer — the one that produces the bytes being counted.
fn vlq_len(v: u64) -> usize {
    let mut len = 1;
    let mut rest = v >> 7;
    while rest != 0 {
        len += 1;
        rest >>= 7;
    }
    len
}

/// Select transactions from mempool for block inclusion.
///
/// Takes prioritized candidates (highest-fee-rate first) and validates
/// each against the upcoming block state. Transactions whose inputs can't
/// be resolved or that fail validation are reported as invalid.
///
/// `committed` is what the block already carries before selection starts —
/// the emission transaction. Its cost and size seed the accumulators, its
/// outputs become spendable by candidates (intra-block chaining), and its
/// inputs are already consumed. The JVM feeds the emission transaction
/// through the same selection loop (`CandidateGenerator.scala:894`,
/// `emissionTxs ++ prioritizedTransactions ++ poolTxs`); passing it as
/// pre-committed gets the same accounting without the loop being able to
/// *skip* it, which for us — skipping where the JVM stops — would silently
/// produce a block with no coinbase.
///
/// Bounded by both protocol limits from `Parameters`: the serialized
/// BlockTransactions **section** against `max_block_size`, and cumulative
/// ErgoScript evaluation cost against `max_block_cost`. Cost is bounded at
/// exactly `accumulated + tx_cost <= max_block_cost` with **no safety margin** —
/// the JVM's `safeGap` guards its own AOT costing divergence, not ours (see
/// `../facts/mining.md`, Step 3). Exceeding either limit skips that one
/// transaction and the scan continues, so a later cheaper or smaller
/// transaction can still use the remaining budget.
///
/// `block_version` is the candidate's version, and it is a size input, not
/// decoration: the section's framing bytes depend on it
/// (`block_transactions_overhead`), and those bytes are inside the limit.
///
/// ⚠ The fee transaction is NOT accounted for here — it does not exist until
/// the selection is known. Bounding the assembled set is the caller's job;
/// `generate_candidate` does it.
///
/// Returns `(selected_txs, invalid_tx_ids)`. Invalid IDs should be
/// reported to the mempool for cleanup. A transaction skipped for exceeding
/// a limit, for conflicting with one already selected, or for inputs that
/// don't resolve is *not* invalid — it stays in the mempool for a later
/// block.
#[allow(clippy::too_many_arguments)]
pub fn select_transactions(
    candidates: &[(Transaction, usize)],
    committed: &[CostedTx],
    state_context: &ErgoStateContext,
    block_version: u8,
    max_block_size: usize,
    max_block_cost: u64,
    utxo_lookup: &dyn Fn(&[u8; 32]) -> Option<ErgoBox>,
) -> (Vec<CostedTx>, Vec<[u8; 32]>) {
    let mut selected: Vec<CostedTx> = Vec::new();
    let mut invalid_ids = Vec::new();
    // Transaction bytes only. The section's framing bytes are added at each
    // bound check rather than seeded here, because one of them — the VLQ
    // transaction count — grows with the selection.
    let mut accumulated_tx_bytes: usize = 0;
    let mut accumulated_cost: u64 = 0;

    // Outputs of the emission tx and of already-selected txs — spendable as
    // inputs by later candidates.
    let mut available_outputs: HashMap<[u8; 32], ErgoBox> = HashMap::new();
    // Every box already consumed by a committed or selected transaction.
    //
    // Without this, two conflicting mempool transactions both get selected:
    // the UTXO lookup still resolves the contested box (it is unspent on
    // chain), and `validate_single_transaction` evaluates one transaction at a
    // time and has no idea the box was already taken by an earlier one. The
    // candidate would carry a double-spend and the assembled block would be
    // invalid. The JVM runs the same check — `doublespend(current, tx)`,
    // deliberately before script validation "to save time"
    // (`CandidateGenerator.scala:875-880`).
    let mut spent_ids: HashSet<[u8; 32]> = HashSet::new();

    for committed_tx in committed {
        accumulated_cost = accumulated_cost.saturating_add(committed_tx.cost);
        accumulated_tx_bytes = accumulated_tx_bytes.saturating_add(committed_tx.size);
        record(&committed_tx.tx, &mut available_outputs, &mut spent_ids);
    }

    for (tx, tx_size) in candidates {
        // Cheap rejections first, in the JVM's order: script evaluation is by
        // far the most expensive step here and candidate assembly now runs
        // every 15 s (TTL regeneration) rather than once a block.

        // Size limit, measured over the section this candidate would produce:
        // its framing bytes, every transaction already accounted for, and this
        // one. The framing is recomputed per candidate because its VLQ
        // transaction count is a function of the selection so far.
        let section_size =
            block_transactions_overhead(block_version, committed.len() + selected.len() + 1)
                .saturating_add(accumulated_tx_bytes)
                .saturating_add(*tx_size);
        if section_size > max_block_size {
            continue; // Skip, might fit smaller txs later
        }

        // Conflict with what is already in the block.
        if tx
            .inputs
            .iter()
            .any(|input| spent_ids.contains(&id_bytes(input.box_id.as_ref())))
        {
            // A mempool conflict is not evidence that this transaction is
            // invalid — the mempool owns conflict resolution, and the loser
            // here is perfectly valid in a block where the winner is absent.
            // The JVM evicts it (`invalidTxs :+ tx.id`); we skip, matching the
            // choice already made for unresolvable inputs below.
            continue;
        }

        // Resolve input boxes
        let mut input_boxes = Vec::with_capacity(tx.inputs.len());
        let mut inputs_found = true;
        for input in tx.inputs.iter() {
            let id = id_bytes(input.box_id.as_ref());
            if let Some(b) = available_outputs.get(&id).cloned().or_else(|| utxo_lookup(&id)) {
                input_boxes.push(b);
            } else {
                inputs_found = false;
                break;
            }
        }
        if !inputs_found {
            continue; // Inputs not available, skip (not necessarily invalid — might appear later)
        }

        // Resolve data input boxes
        let data_boxes: Vec<ErgoBox> = tx
            .data_inputs
            .as_ref()
            .map(|dis| {
                dis.iter()
                    .filter_map(|di| {
                        let id = id_bytes(di.box_id.as_ref());
                        available_outputs.get(&id).cloned().or_else(|| utxo_lookup(&id))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Validate against upcoming state
        match validate_single_transaction(tx, input_boxes, data_boxes, state_context) {
            Ok(tx_cost) => {
                // The cost check cannot precede validation — validation is what
                // produces the number. `checked_add` reads an overflowing sum as
                // "does not fit" instead of wrapping into a spurious accept.
                let Some(total_cost) = accumulated_cost
                    .checked_add(tx_cost)
                    .filter(|total| *total <= max_block_cost)
                else {
                    // Over budget, but still a valid transaction: leave it in
                    // the mempool and keep scanning — a cheaper one may fit.
                    continue;
                };

                accumulated_cost = total_cost;
                accumulated_tx_bytes += tx_size;

                record(tx, &mut available_outputs, &mut spent_ids);

                selected.push(CostedTx {
                    tx: tx.clone(),
                    cost: tx_cost,
                    size: *tx_size,
                });
            }
            Err(_) => {
                invalid_ids.push(id_bytes(tx.id().as_ref()));
            }
        }
    }

    (selected, invalid_ids)
}

/// Fold a transaction into the accumulated block state: its inputs become
/// unavailable, its outputs become spendable by later transactions.
fn record(
    tx: &Transaction,
    available_outputs: &mut HashMap<[u8; 32], ErgoBox>,
    spent_ids: &mut HashSet<[u8; 32]>,
) {
    for input in tx.inputs.iter() {
        spent_ids.insert(id_bytes(input.box_id.as_ref()));
    }
    for output in tx.outputs.iter() {
        available_outputs.insert(id_bytes(output.box_id().as_ref()), output.clone());
    }
}

/// Box/transaction ids are Blake2b256 digests — always 32 bytes.
pub(crate) fn id_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut id = [0u8; 32];
    id.copy_from_slice(bytes);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigma_ser::vlq_encode::WriteSigmaVlqExt;

    /// `vlq_len` counts what the encoder writes. Computing the length instead
    /// of encoding keeps an error path out of the size accounting, which is
    /// only safe while the two agree — so pin them together, at every byte
    /// boundary and either side of it.
    #[test]
    fn vlq_len_matches_the_encoder() {
        let mut cases: Vec<u64> = vec![0, 1, 127, 128, 16_383, 16_384, 10_000_002, u64::MAX];
        // Every 7-bit boundary and its neighbours.
        for shift in 1..=9 {
            let boundary = 1u64 << (7 * shift);
            cases.extend([boundary - 1, boundary, boundary + 1]);
        }

        for v in cases {
            let mut encoded: Vec<u8> = Vec::new();
            encoded.put_u64(v).expect("writing to a Vec cannot fail");
            assert_eq!(
                vlq_len(v),
                encoded.len(),
                "vlq_len({v}) disagrees with the encoder"
            );
        }
    }

    /// The 32 + 4 + 1 that the sum of transaction sizes misses on a
    /// current-version block: header id, the sentinel-offset version, and the
    /// transaction count.
    #[test]
    fn section_overhead_is_37_bytes_for_a_normal_block() {
        assert_eq!(block_transactions_overhead(3, 1), 37);
        assert_eq!(block_transactions_overhead(3, 127), 37);
        // 128 transactions push the count VLQ to two bytes.
        assert_eq!(block_transactions_overhead(3, 128), 38);
        assert_eq!(block_transactions_overhead(3, 16_384), 39);
        // Every plausible block version sits inside the same 24-bit VLQ band
        // as the sentinel, so the version field is 4 bytes throughout.
        for version in 2..=u8::MAX {
            assert_eq!(block_transactions_overhead(version, 1), 37);
        }
        // A version-1 section has no version field at all: the first VLQ is
        // the transaction count.
        assert_eq!(block_transactions_overhead(1, 1), 33);
    }
}

use std::time::Instant;

use crate::process;
use crate::types::UtxoReader;
use crate::CLEANUP_COST_LIMIT;
use ergo_validation::{validate_single_transaction, ErgoStateContext};

impl super::Mempool {
    /// Revalidate pool transactions against current state: the JVM's
    /// `CleanupWorker.validatePool`.
    ///
    /// Iterates transactions in priority order, highest weight first, so a
    /// parent comes before its children. For each:
    /// - Skip if no more than `cleanup_interval` has passed since `last_checked`
    /// - Stop the pass if its accumulated cost has reached `CLEANUP_COST_LIMIT`
    /// - Resolve inputs from utxo_reader + unconfirmed
    /// - Re-run validate_single_transaction()
    /// - If valid: record the measured cost as `validation_cost` and now as
    ///   `last_checked` (the JVM's `UnconfirmedTransaction.withCost`), and add
    ///   the measured cost to the pass's
    /// - Remove if invalid or inputs missing, adding the transaction's
    ///   previous `validation_cost` to the pass's cost, 0 when it has none
    ///
    /// A skipped transaction adds nothing. Returns IDs of removed transactions.
    pub fn revalidate(
        &mut self,
        utxo_reader: &dyn UtxoReader,
        state_context: &ErgoStateContext,
    ) -> Vec<[u8; 32]> {
        self.revalidate_at(Instant::now(), utxo_reader, state_context)
    }

    /// [`Self::revalidate`] as of `now`, which decides whether
    /// `cleanup_interval` has passed since each `last_checked`, and becomes
    /// the `last_checked` of each transaction that passes.
    pub fn revalidate_at(
        &mut self,
        now: Instant,
        utxo_reader: &dyn UtxoReader,
        state_context: &ErgoStateContext,
    ) -> Vec<[u8; 32]> {
        self.revalidate_within(now, CLEANUP_COST_LIMIT, utxo_reader, state_context)
    }

    /// One pass on a budget of `limit`. Production passes only
    /// `CLEANUP_COST_LIMIT`; the tests pass limits a few validations reach.
    pub(crate) fn revalidate_within(
        &mut self,
        now: Instant,
        limit: u64,
        utxo_reader: &dyn UtxoReader,
        state_context: &ErgoStateContext,
    ) -> Vec<[u8; 32]> {
        let mut removed = Vec::new();
        let mut spent: u64 = 0;

        // Taken once, since the pass removes as it goes.
        let prioritized: Vec<[u8; 32]> = self.pool.ordered.keys().map(|w| w.tx_id).collect();

        for tx_id in prioritized {
            let utx = match self.pool.get(&tx_id) {
                Some(u) => u,
                None => continue,
            };

            // Skip recently checked. Exactly `cleanup_interval` is recent: the
            // JVM re-checks only past its `TimeLimit`.
            if now.saturating_duration_since(utx.last_checked) <= self.config.cleanup_interval {
                continue;
            }

            if spent >= limit {
                break;
            }

            // Same transient guard as step 6a — a reorg can move the preheader
            // backwards, making a pooled tx look early. Leave it pooled; it is
            // early, not invalid.
            if let Some(height) = process::output_above_preheader(&utx.tx, state_context) {
                tracing::debug!(
                    tx_id = %hex::encode(tx_id),
                    creation_height = height,
                    preheader_height = state_context.pre_header.height,
                    "revalidation: skipping transaction built ahead of our tip"
                );
                continue;
            }

            // What a removal adds to the pass: the cost the transaction's last
            // validation measured.
            let previous_cost = utx.validation_cost.unwrap_or(0);

            // Resolve inputs
            let input_ids = process::input_box_ids(&utx.tx);
            let input_boxes: Option<Vec<_>> = input_ids
                .iter()
                .map(|id| {
                    utxo_reader
                        .box_by_id(id)
                        .or_else(|| self.pool.unconfirmed_box(id).cloned())
                })
                .collect();

            let input_boxes = match input_boxes {
                Some(boxes) => boxes,
                None => {
                    self.pool.remove(&tx_id);
                    removed.push(tx_id);
                    spent = spent.saturating_add(previous_cost);
                    continue;
                }
            };

            let data_boxes: Vec<_> = utx
                .tx
                .data_inputs
                .as_ref()
                .map(|dis| {
                    dis.iter()
                        .filter_map(|di| {
                            let id = process::input_box_id_raw(&di.box_id);
                            utxo_reader
                                .box_by_id(&id)
                                .or_else(|| self.pool.unconfirmed_box(&id).cloned())
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Clone what we need before the mutable borrow
            let tx_clone = utx.tx.clone();

            match validate_single_transaction(&tx_clone, input_boxes, data_boxes, state_context) {
                Ok(measured) => {
                    if let Some(utx) = self.pool.get_mut(&tx_id) {
                        utx.validation_cost = Some(measured);
                        utx.last_checked = now;
                    }
                    spent = spent.saturating_add(measured);
                }
                Err(e) => {
                    tracing::info!(
                        tx_id = %hex::encode(tx_id),
                        reason = %e,
                        "revalidation: evicting invalid tx"
                    );
                    self.invalidated.insert(tx_id);
                    self.pool.remove(&tx_id);
                    removed.push(tx_id);
                    spent = spent.saturating_add(previous_cost);
                }
            }
        }

        removed
    }

    /// Select transactions for rebroadcast.
    /// Returns up to rebroadcast_count txs whose inputs exist in the UTXO set.
    pub fn select_for_rebroadcast(
        &self,
        utxo_reader: &dyn UtxoReader,
    ) -> Vec<&crate::types::UnconfirmedTx> {
        let mut valid: Vec<&crate::types::UnconfirmedTx> = Vec::new();

        for utx in self.pool.all_prioritized() {
            if valid.len() >= self.config.rebroadcast_count {
                break;
            }
            let all_inputs_exist = process::input_box_ids(&utx.tx)
                .iter()
                .all(|id| utxo_reader.box_by_id(id).is_some());
            if all_inputs_exist {
                valid.push(utx);
            }
        }

        valid
    }
}

/// The pass's order and budget, through the crate-private
/// `revalidate_within`: a real pass reaches `CLEANUP_COST_LIMIT` only across
/// hundreds of transactions. Validation is real. Every transaction spends one
/// `sigmaProp(true)` box, which costs 10,000 to start the interpreter, 2,000
/// for the input, 100 per output and 1 for the script: 12,101 into one
/// output.
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ergo_lib::chain::transaction::Transaction;
    use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

    use crate::test_common::{
        fee_tree, make_box, sigma_bool_tree, spend_tx, spend_tx_to, state_context_at, tx_id_bytes,
        StaticUtxo, TIP_HEIGHT,
    };
    use crate::types::{MempoolConfig, ProcessingOutcome, UnconfirmedTx};
    use crate::Mempool;

    /// A pool entry spending `input` into one output, paying `fee`, carrying
    /// `validation_cost`, entered and last checked at `checked`.
    fn entry(
        input: &ErgoBox,
        fee: u64,
        validation_cost: Option<u64>,
        checked: Instant,
    ) -> UnconfirmedTx {
        let tx = spend_tx(std::slice::from_ref(input), TIP_HEIGHT);
        let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
        UnconfirmedTx {
            cost: tx_bytes.len() as u32,
            tx,
            tx_bytes: tx_bytes.into(),
            fee,
            validation_cost,
            created: checked,
            last_checked: checked,
            source: None,
        }
    }

    /// Pools `entries` and returns their ids, asserting that their fees put
    /// them in priority order as given.
    fn pool(mempool: &mut Mempool, entries: Vec<UnconfirmedTx>) -> Vec<[u8; 32]> {
        let ids: Vec<[u8; 32]> = entries.iter().map(|utx| tx_id_bytes(&utx.tx)).collect();
        mempool.return_to_pool(entries);
        assert_eq!(prioritized(mempool), ids, "setup: priority order");
        ids
    }

    fn prioritized(mempool: &Mempool) -> Vec<[u8; 32]> {
        mempool
            .all_prioritized()
            .iter()
            .map(|utx| tx_id_bytes(&utx.tx))
            .collect()
    }

    /// Whether the pass at `now` validated `tx_id` and kept it.
    fn passed(mempool: &Mempool, tx_id: &[u8; 32], now: Instant) -> bool {
        mempool
            .get(tx_id)
            .is_some_and(|utx| utx.last_checked == now)
    }

    /// Spends `input` into its change and a fee output of `fee`.
    fn paying(input: &ErgoBox, fee: u64) -> Transaction {
        spend_tx_to(
            std::slice::from_ref(input),
            &[
                (*input.value.as_u64() - fee, sigma_bool_tree(true)),
                (fee, fee_tree()),
            ],
            TIP_HEIGHT,
        )
    }

    /// An hour on: past `cleanup_interval` for anything checked at `t0`.
    fn an_hour_after(t0: Instant) -> Instant {
        t0 + Duration::from_secs(3600)
    }

    /// P pays 2,000,000 and its child C, spending P's change, 8,000,000; X,
    /// on its own, 4,000,000. By their own fees C comes first and P last.
    /// Family weighting adds C's weight to P's, so P comes first. On a
    /// budget one validation spends, P is checked and neither C nor X is.
    #[test]
    fn the_pass_goes_heaviest_first_so_a_parent_before_its_child() {
        let mut mempool = Mempool::new(MempoolConfig::default());
        let p_input = make_box(true, 1, TIP_HEIGHT - 1);
        let x_input = make_box(true, 2, TIP_HEIGHT - 1);
        let utxo = StaticUtxo::new(&[p_input.clone(), x_input.clone()]);
        let context = state_context_at(TIP_HEIGHT);

        let p = paying(&p_input, 2_000_000);
        let change = p.outputs.as_slice().first().expect("change output").clone();
        let c = paying(&change, 8_000_000);
        let x = paying(&x_input, 4_000_000);
        for tx in [&x, &p, &c] {
            let bytes = tx.sigma_serialize_bytes().expect("tx serialization");
            let outcome = mempool.process(tx.clone(), bytes, &utxo, &context, None);
            assert!(
                matches!(outcome, ProcessingOutcome::Accepted { .. }),
                "setup: expected the tx to enter the pool, got {outcome:?}"
            );
        }
        let [p, c, x] = [&p, &c, &x].map(tx_id_bytes);
        assert_eq!(prioritized(&mempool), [p, c, x], "setup: P, C, X");

        let now = an_hour_after(Instant::now());
        assert!(mempool
            .revalidate_within(now, 1, &utxo, &context)
            .is_empty());

        assert!(passed(&mempool, &p, now), "P is checked");
        assert!(!passed(&mempool, &c, now), "C is not");
        assert!(!passed(&mempool, &x, now), "X is not");
    }

    /// A, B and C are valid, heaviest in that order. A limit of exactly two
    /// validations, 24,202, is reached after B, and the pass stops before C:
    /// at the limit, not after it. One more, 24,203, and C is checked too.
    #[test]
    fn the_pass_stops_when_its_measured_costs_reach_the_limit() {
        for (limit, c_checked) in [(24_202, false), (24_203, true)] {
            let mut mempool = Mempool::new(MempoolConfig::default());
            let inputs = [1, 2, 3].map(|seed| make_box(true, seed, TIP_HEIGHT - 1));
            let t0 = Instant::now();
            let ids = pool(
                &mut mempool,
                vec![
                    entry(&inputs[0], 3_000_000, None, t0),
                    entry(&inputs[1], 2_000_000, None, t0),
                    entry(&inputs[2], 1_000_000, None, t0),
                ],
            );

            let now = an_hour_after(t0);
            let removed = mempool.revalidate_within(
                now,
                limit,
                &StaticUtxo::new(&inputs),
                &state_context_at(TIP_HEIGHT),
            );

            assert!(removed.is_empty());
            let checked: Vec<bool> = ids.iter().map(|id| passed(&mempool, id, now)).collect();
            assert_eq!(checked, [true, true, c_checked], "on a limit of {limit}");
        }
    }

    /// R, the heaviest, is removed; V1 and V2 after it are valid, on a limit
    /// of two validations, 24,202. R adds its previous `validation_cost`,
    /// whether its input is gone or its script no longer passes:
    /// - 24,202 is the limit on its own: neither V is checked.
    /// - 12,101: V1 is checked, which reaches 24,202, and V2 isn't.
    /// - None adds 0: both are.
    #[test]
    fn a_removal_adds_the_cost_its_last_validation_measured() {
        let cases = [
            (Some(24_202), [false, false]),
            (Some(12_101), [true, false]),
            (None, [true, true]),
        ];
        for input_gone in [true, false] {
            for (previous, checked) in cases {
                let mut mempool = Mempool::new(MempoolConfig::default());
                // Gone from the UTXO set, or there under a script that fails.
                let r_input = make_box(input_gone, 1, TIP_HEIGHT - 1);
                let v_inputs = [2, 3].map(|seed| make_box(true, seed, TIP_HEIGHT - 1));
                let t0 = Instant::now();
                let ids = pool(
                    &mut mempool,
                    vec![
                        entry(&r_input, 3_000_000, previous, t0),
                        entry(&v_inputs[0], 2_000_000, None, t0),
                        entry(&v_inputs[1], 1_000_000, None, t0),
                    ],
                );
                let mut utxo = v_inputs.to_vec();
                if !input_gone {
                    utxo.push(r_input);
                }

                let now = an_hour_after(t0);
                let removed = mempool.revalidate_within(
                    now,
                    24_202,
                    &StaticUtxo::new(&utxo),
                    &state_context_at(TIP_HEIGHT),
                );

                let case = format!("input gone: {input_gone}, previous: {previous:?}");
                assert_eq!(removed, [ids[0]], "{case}");
                assert_eq!(mempool.is_invalidated(&ids[0]), !input_gone, "{case}");
                assert_eq!(
                    [
                        passed(&mempool, &ids[1], now),
                        passed(&mempool, &ids[2], now)
                    ],
                    checked,
                    "{case}"
                );
            }
        }
    }

    /// S, the heaviest, was checked a second before the pass: skipped. It
    /// carries a cost of 12,101, the whole limit, which would stop the pass
    /// had the skip added it. V1 is checked, reaching the limit; V2 isn't.
    #[test]
    fn a_skipped_transaction_adds_nothing() {
        let mut mempool = Mempool::new(MempoolConfig::default());
        let inputs = [1, 2, 3].map(|seed| make_box(true, seed, TIP_HEIGHT - 1));
        let t0 = Instant::now();
        let now = an_hour_after(t0);
        let just_checked = now - Duration::from_secs(1);
        let ids = pool(
            &mut mempool,
            vec![
                entry(&inputs[0], 3_000_000, Some(12_101), just_checked),
                entry(&inputs[1], 2_000_000, None, t0),
                entry(&inputs[2], 1_000_000, None, t0),
            ],
        );

        let removed = mempool.revalidate_within(
            now,
            12_101,
            &StaticUtxo::new(&inputs),
            &state_context_at(TIP_HEIGHT),
        );

        assert!(removed.is_empty());
        let s = mempool.get(&ids[0]).expect("still pooled");
        assert_eq!(
            (s.last_checked, s.validation_cost),
            (just_checked, Some(12_101)),
            "S is left as it was"
        );
        assert!(passed(&mempool, &ids[1], now), "V1 is checked");
        assert!(!passed(&mempool, &ids[2], now), "V2 is not");
    }
}

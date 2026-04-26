use std::collections::{BTreeMap, HashMap};
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;

use crate::weight::TxWeight;
use crate::types::UnconfirmedTx;

/// Core ordered transaction pool with secondary indexes.
pub struct OrderedPool {
    /// Primary store: ordered by TxWeight (highest fee first).
    pub(crate) ordered: BTreeMap<TxWeight, UnconfirmedTx>,
    /// tx_id → TxWeight
    pub(crate) by_id: HashMap<[u8; 32], TxWeight>,
    /// input_box_id → TxWeight (for double-spend detection)
    pub(crate) by_input: HashMap<[u8; 32], TxWeight>,
    /// output_box_id → (TxWeight, ErgoBox) (for chained tx resolution)
    pub(crate) by_output: HashMap<[u8; 32], (TxWeight, ErgoBox)>,
    /// Maximum capacity
    capacity: usize,
}

impl OrderedPool {
    pub fn new(capacity: usize) -> Self {
        Self {
            ordered: BTreeMap::new(),
            by_id: HashMap::new(),
            by_input: HashMap::new(),
            by_output: HashMap::new(),
            capacity,
        }
    }

    pub fn len(&self) -> usize { self.ordered.len() }

    pub fn is_empty(&self) -> bool { self.ordered.is_empty() }

    pub fn is_full(&self) -> bool { self.len() >= self.capacity }

    /// Insert a transaction with its pre-computed weight.
    pub fn insert(
        &mut self,
        weight: TxWeight,
        utx: UnconfirmedTx,
        input_ids: &[[u8; 32]],
        output_boxes: Vec<([u8; 32], ErgoBox)>,
    ) {
        let tx_id = weight.tx_id;
        self.by_id.insert(tx_id, weight.clone());
        for id in input_ids {
            self.by_input.insert(*id, weight.clone());
        }
        for (box_id, ebox) in output_boxes {
            self.by_output.insert(box_id, (weight.clone(), ebox));
        }
        self.ordered.insert(weight, utx);
    }

    /// Remove a transaction by ID. Returns the removed tx if found.
    pub fn remove(&mut self, tx_id: &[u8; 32]) -> Option<UnconfirmedTx> {
        let weight = self.by_id.remove(tx_id)?;
        let utx = self.ordered.remove(&weight)?;

        // Clean input index
        self.by_input.retain(|_, w| w.tx_id != *tx_id);
        // Clean output index
        self.by_output.retain(|_, (w, _)| w.tx_id != *tx_id);

        Some(utx)
    }

    /// Get a transaction by ID.
    pub fn get(&self, tx_id: &[u8; 32]) -> Option<&UnconfirmedTx> {
        let weight = self.by_id.get(tx_id)?;
        self.ordered.get(weight)
    }

    /// Check if tx ID is in pool.
    pub fn contains(&self, tx_id: &[u8; 32]) -> bool { self.by_id.contains_key(tx_id) }

    /// Find which pool tx spends a given input box (for double-spend check).
    pub fn spending_tx(&self, input_box_id: &[u8; 32]) -> Option<&TxWeight> {
        self.by_input.get(input_box_id)
    }

    /// Get unconfirmed output box by box ID (for chained tx validation).
    pub fn unconfirmed_box(&self, box_id: &[u8; 32]) -> Option<&ErgoBox> {
        self.by_output.get(box_id).map(|(_, b)| b)
    }

    /// Top N transactions by weight.
    pub fn top(&self, limit: usize) -> Vec<&UnconfirmedTx> {
        self.ordered.values().take(limit).collect()
    }

    /// All transactions in priority order.
    pub fn all_prioritized(&self) -> Vec<&UnconfirmedTx> {
        self.ordered.values().collect()
    }

    /// Evict the lowest-weight transaction. Returns its tx_id.
    pub fn evict_lowest(&mut self) -> Option<[u8; 32]> {
        let lowest_weight = self.ordered.keys().next_back()?.clone();
        let tx_id = lowest_weight.tx_id;
        self.remove(&tx_id);
        Some(tx_id)
    }

    /// Get the lowest weight in the pool (for admission check).
    pub fn lowest_weight(&self) -> Option<u64> {
        self.ordered.keys().last().map(|w| w.weight)
    }

    /// Update a transaction's weight (for family propagation).
    pub fn update_weight(&mut self, tx_id: &[u8; 32], new_weight: u64) {
        let old_weight = match self.by_id.get(tx_id) {
            Some(w) => w.clone(),
            None => return,
        };

        if let Some(utx) = self.ordered.remove(&old_weight) {
            let mut updated = old_weight.clone();
            updated.weight = new_weight;

            // Update all indexes that hold the weight
            self.by_id.insert(*tx_id, updated.clone());
            for (_, w) in self.by_input.iter_mut() {
                if w.tx_id == *tx_id {
                    *w = updated.clone();
                }
            }
            for (_, (w, _)) in self.by_output.iter_mut() {
                if w.tx_id == *tx_id {
                    *w = updated.clone();
                }
            }

            self.ordered.insert(updated, utx);
        }
    }

    /// All input box IDs spent by pool transactions.
    pub fn spent_inputs(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.by_input.keys()
    }

    /// All transaction IDs.
    pub fn tx_ids(&self) -> Vec<[u8; 32]> {
        self.by_id.keys().copied().collect()
    }
}

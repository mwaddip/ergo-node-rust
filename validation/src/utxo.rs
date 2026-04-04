//! UtxoValidator: persistent AVL+ tree state verification via BatchAVLProver.

use std::collections::HashMap;

use bytes::Bytes;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::persistent_batch_avl_prover::PersistentBatchAVLProver;
use ergo_chain_types::{ADDigest, Header};
use ergo_lib::chain::parameters::Parameters;

use crate::sections::{parse_block_transactions, parse_extension};
use crate::state_changes::{compute_state_changes, transactions_to_summaries};
use crate::tx_validation;
use crate::{BlockValidator, ValidationError};

/// UTXO-mode block validator.
///
/// Verifies state transitions by applying operations to a persistent AVL+ tree
/// (PersistentBatchAVLProver backed by VersionedAVLStorage). Boxes come from
/// the tree's Lookup/Remove results, not from AD proofs.
///
/// Shares section parsing, state change computation, and transaction validation
/// with DigestValidator — only the state root verification mechanism differs.
pub struct UtxoValidator {
    prover: PersistentBatchAVLProver,
    validated_height: u32,
    checkpoint_height: u32,
    current_digest: ADDigest,
    parameters: Parameters,
}

impl UtxoValidator {
    /// Create a UtxoValidator from a fully constructed PersistentBatchAVLProver.
    ///
    /// The caller builds the prover (RedbAVLStorage + BatchAVLProver) and passes
    /// it in. The validator doesn't know about the storage backend.
    pub fn new(
        prover: PersistentBatchAVLProver,
        height: u32,
        checkpoint_height: u32,
    ) -> Self {
        let digest_bytes = prover.digest();
        let digest = bytes_to_ad_digest(&digest_bytes);
        Self {
            prover,
            validated_height: height,
            checkpoint_height,
            current_digest: digest,
            parameters: Parameters::default(),
        }
    }
}

impl BlockValidator for UtxoValidator {
    fn validate_block(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        _ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[Header],
    ) -> Result<(), ValidationError> {
        let expected_height = self.validated_height + 1;
        if header.height != expected_height {
            return Err(ValidationError::HeightMismatch {
                expected: expected_height,
                got: header.height,
            });
        }

        // 1. Parse sections (AD proofs not needed in UTXO mode)
        let parsed_txs = parse_block_transactions(block_txs)?;
        let parsed_ext = parse_extension(extension)?;

        // 2. Compute state changes from transactions
        let summaries = transactions_to_summaries(&parsed_txs.transactions)?;
        let changes = compute_state_changes(summaries)?;

        // 3. Build AVL operations: Lookups, Removes, Inserts (sorted per JVM order)
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

        // 4. Apply operations to the persistent prover, capturing old values
        let validate_txs = header.height > self.checkpoint_height;
        let mut proof_box_bytes: HashMap<[u8; 32], Vec<u8>> = HashMap::new();

        for (i, op) in operations.iter().enumerate() {
            let result = self
                .prover
                .perform_one_operation(op)
                .map_err(|e| ValidationError::StateOperationFailed(
                    format!("operation {i} failed: {e}"),
                ))?;

            if validate_txs {
                if let Some(value) = result {
                    match op {
                        Operation::Lookup(key) | Operation::Remove(key) => {
                            let mut id = [0u8; 32];
                            id.copy_from_slice(key);
                            proof_box_bytes.insert(id, value.to_vec());
                        }
                        _ => {}
                    }
                }
            }
        }

        // 5. Verify resulting digest matches header.state_root
        let expected_state_root: [u8; 33] = header.state_root.into();
        let prover_digest = self.prover.digest();
        if prover_digest.as_ref() != expected_state_root.as_slice() {
            return Err(ValidationError::StateRootMismatch {
                expected: expected_state_root.to_vec(),
                got: prover_digest.to_vec(),
            });
        }

        // 6. Transaction validation (above checkpoint — ErgoScript evaluation)
        if validate_txs {
            let mut proof_boxes = HashMap::with_capacity(proof_box_bytes.len());
            for (id, bytes) in &proof_box_bytes {
                proof_boxes.insert(*id, tx_validation::deserialize_box(bytes)?);
            }

            tx_validation::validate_transactions(
                &parsed_txs.transactions,
                &proof_boxes,
                header,
                preceding_headers,
                &self.parameters,
            )?;
        }

        // 7. Persist state changes + generate AD proof as side effect
        self.prover
            .generate_proof_and_update_storage(vec![])
            .map_err(|e| ValidationError::StateOperationFailed(
                format!("persist failed: {e}"),
            ))?;

        // 8. Update parameters from extension
        self.parameters =
            tx_validation::update_parameters_from_extension(&self.parameters, &parsed_ext);

        // 9. Advance state
        self.current_digest = header.state_root;
        self.validated_height = header.height;

        tracing::debug!(height = header.height, "block validated (UTXO mode)");

        Ok(())
    }

    fn validated_height(&self) -> u32 {
        self.validated_height
    }

    fn current_digest(&self) -> &ADDigest {
        &self.current_digest
    }

    fn reset_to(&mut self, height: u32, digest: ADDigest) {
        let digest_bytes: [u8; 33] = digest.into();
        let avl_digest = Bytes::copy_from_slice(&digest_bytes);

        if let Err(e) = self.prover.rollback(&avl_digest) {
            tracing::error!(height, error = %e, "UTXO state rollback failed");
            return;
        }

        self.validated_height = height;
        self.current_digest = digest;
        tracing::info!(height, "UTXO validator reset to fork point");
    }
}

fn bytes_to_ad_digest(bytes: &Bytes) -> ADDigest {
    let mut arr = [0u8; 33];
    arr.copy_from_slice(bytes);
    ADDigest::from(arr)
}

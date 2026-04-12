//! DigestValidator: AD proof verification via BatchAVLVerifier.

use std::collections::HashMap;
use std::sync::Arc;

use blake2::Digest;
use bytes::Bytes;
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_verifier::BatchAVLVerifier;
use ergo_avltree_rust::batch_node::{AVLTree, Node, NodeHeader, Resolver};
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_chain_types::{ADDigest, Header};
use ergo_lib::chain::parameters::Parameters;

use crate::sections::{parse_ad_proofs, parse_block_transactions, parse_extension};
use crate::state_changes::{compute_state_changes, transactions_to_summaries};
use crate::tx_validation;
use crate::voting;
use crate::{ApplyStateOutcome, BlockValidator, ValidationError};

/// Key length for Ergo's UTXO AVL+ tree (BoxId = 32 bytes).
pub(crate) const KEY_LENGTH: usize = 32;

/// Resolver for the AVL verifier — returns a LabelOnly node preserving the digest.
///
/// The verifier's partial tree has LabelOnly sibling stubs with labels from the proof.
/// `AVLTree::left()/right()` calls `resolve()` on every child access, including
/// LabelOnly stubs. The resolver must preserve the label so the stub remains valid
/// for subsequent accesses (label computation, rebalancing checks).
pub(crate) fn label_preserving_resolver() -> Resolver {
    Arc::new(|digest: &[u8; 32]| Node::LabelOnly(NodeHeader::new(Some(*digest), None)))
}

/// Digest-mode block validator.
///
/// Verifies state transitions using AD proofs and BatchAVLVerifier.
/// No persistent UTXO set — just tracks the current state root.
///
/// Stateless w.r.t. blockchain parameters: the caller passes `active_params`
/// on every `validate_block` call. The validator does not own a `Parameters`
/// field — chain submodule is the single source of truth.
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
    fn apply_state(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[Header],
        active_params: &Parameters,
        expected_boundary_params: Option<&Parameters>,
    ) -> Result<ApplyStateOutcome, ValidationError> {
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
        let parsed_ext = parse_extension(extension)?;

        // 1a. Epoch-boundary parameter check (consensus-critical).
        // Uses JVM v6 matchParameters60 semantics: local can have fewer
        // entries than received, every entry in local must match received.
        let epoch_boundary_params = match expected_boundary_params {
            Some(expected) => {
                let parsed = voting::parse_parameters_from_extension(&parsed_ext)?;
                voting::check_parameters_v6(expected, &parsed, header.height)?;
                Some(parsed)
            }
            None => None,
        };

        // 2. Verify AD proofs digest matches header
        let proof_digest: [u8; 32] = blake2::Blake2b::<blake2::digest::typenum::U32>::digest(
            &parsed_proofs.proof_bytes,
        )
        .into();
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

        // 4. Build AVL operations in JVM order: Lookups, Removes, Inserts
        //    (Removes and Inserts are sorted by box ID — see state_changes.rs)
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

        // 5. Verify AD proof via BatchAVLVerifier, capturing old values
        //    for transaction validation (Lookup/Remove return the serialized box)
        let validate_txs = header.height > self.checkpoint_height;
        let starting_digest_bytes: [u8; 33] = self.current_digest.into();
        let starting_digest = Bytes::copy_from_slice(&starting_digest_bytes);
        let proof_bytes = Bytes::copy_from_slice(&parsed_proofs.proof_bytes);

        let tree = AVLTree::new(label_preserving_resolver(), KEY_LENGTH, None);
        let mut verifier = BatchAVLVerifier::new(
            &starting_digest,
            &proof_bytes,
            tree,
            Some(operations.len()),
            None,
        )
        .map_err(|e| ValidationError::ProofVerificationFailed(format!("{e}")))?;

        let mut proof_box_bytes: HashMap<[u8; 32], Vec<u8>> = HashMap::new();

        for (i, op) in operations.iter().enumerate() {
            let result = verifier
                .perform_one_operation(op)
                .map_err(|e| ValidationError::ProofVerificationFailed(
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

        // 6. Check resulting digest matches header.state_root
        let expected_state_root: [u8; 33] = header.state_root.into();
        let verifier_digest: Option<Bytes> = verifier.digest();
        match verifier_digest {
            Some(ref d) if d.as_ref() == expected_state_root.as_slice() => {}
            Some(d) => {
                return Err(ValidationError::StateRootMismatch {
                    expected: expected_state_root.to_vec(),
                    got: d.to_vec(),
                });
            }
            None => {
                return Err(ValidationError::ProofVerificationFailed(
                    "verifier digest is None after operations".to_string(),
                ));
            }
        }

        // 7. Build DeferredEval for deferred script verification
        let deferred_eval = if validate_txs {
            let mut proof_boxes = HashMap::with_capacity(proof_box_bytes.len());
            for (id, bytes) in &proof_box_bytes {
                proof_boxes.insert(*id, tx_validation::deserialize_box(bytes)?);
            }

            Some(crate::DeferredEval {
                height: header.height,
                transactions: parsed_txs.transactions,
                proof_boxes,
                header: header.clone(),
                preceding_headers: preceding_headers.to_vec(),
                parameters: active_params.clone(),
            })
        } else {
            None
        };

        // 8. Advance state
        self.current_digest = header.state_root;
        self.validated_height = header.height;

        tracing::debug!(height = header.height, "state applied (digest mode)");

        Ok(ApplyStateOutcome { epoch_boundary_params, deferred_eval })
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

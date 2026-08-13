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
use crate::{
    ApplyStateOutcome, BlockValidator, ScriptEvalMode, StatePersistence, ValidationError,
};

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
    /// Whether `apply_state` evaluates this block's scripts itself or hands
    /// the caller a `DeferredEval` to do it. Fixed at construction.
    ///
    /// Digest mode has no persistence step to sit in front of — the whole
    /// apply is in memory and ends in two field assignments — so "inline"
    /// here buys no crash-consistency the mode does not already have. It
    /// exists so the two validators do not diverge in observable behaviour:
    /// a caller keying on `deferred_eval` must not have to ask which one it
    /// is holding.
    script_eval_mode: ScriptEvalMode,
}

impl DigestValidator {
    /// Create a new DigestValidator starting from genesis.
    ///
    /// `genesis_digest`: ADDigest of the genesis UTXO state (33 bytes, per-network constant).
    /// `checkpoint_height`: skip script validation at or below this height (0 = validate all).
    /// `script_eval_mode`: see [`ScriptEvalMode`] — must agree with the sync
    /// layer's configuration.
    pub fn new(
        genesis_digest: ADDigest,
        checkpoint_height: u32,
        script_eval_mode: ScriptEvalMode,
    ) -> Self {
        Self {
            current_digest: genesis_digest,
            validated_height: 0,
            checkpoint_height,
            script_eval_mode,
        }
    }

    /// Create a DigestValidator resuming from a known state.
    pub fn from_state(
        digest: ADDigest,
        height: u32,
        checkpoint_height: u32,
        script_eval_mode: ScriptEvalMode,
    ) -> Self {
        Self {
            current_digest: digest,
            validated_height: height,
            checkpoint_height,
            script_eval_mode,
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
        expected_proposed_update: Option<&[u8]>,
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
        // At v4+ the proposedUpdate byte-for-byte comparison also runs.
        let (epoch_boundary_params, epoch_boundary_proposed_update) =
            match expected_boundary_params {
                Some(expected) => {
                    let parsed = voting::parse_parameters_from_extension(&parsed_ext)?;
                    let parsed_pu = voting::extract_proposed_update(&parsed_ext);
                    let expected_pu = expected_proposed_update.unwrap_or(&[]);
                    voting::check_parameters_v6(
                        expected,
                        &parsed,
                        header.height,
                        header.version,
                        expected_pu,
                        &parsed_pu,
                    )?;
                    (Some(parsed), Some(parsed_pu))
                }
                None => (None, None),
            };

        // 1b. Block-version gate (consensus check — JVM exBlockVersion).
        // Boundary-only: the JVM checks header.version against the newly
        // computed boundary parameters inside processExtension, which runs
        // only at epoch boundaries (epochStarts gate). Mid-epoch the JVM
        // has no version rule at all — checking there would reject blocks
        // the reference accepts.
        if let Some(boundary) = expected_boundary_params {
            voting::check_block_version(boundary, header.version, header.height)?;
        }

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

        let tree = AVLTree::with_resolver(label_preserving_resolver(), KEY_LENGTH, None);
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

        // 7. Script evaluation — bundled for the caller, or run right here.
        //
        // Same ordering rule as UTXO mode (state-root check first, it is
        // cheap and rejects malformed blocks before the expensive step), and
        // the same single entry point through `evaluate_scripts` so the two
        // modes cannot drift on a consensus path. Nothing to roll back on the
        // Err path: step 8 below is the only mutation in the whole function,
        // and it has not happened yet.
        let deferred_eval = if validate_txs {
            let mut proof_boxes = HashMap::with_capacity(proof_box_bytes.len());
            // Summed inside the loop that already runs — the serialized size
            // feeds DeferredEval::new's heap estimate at no extra walk.
            let mut serialized_box_bytes = 0usize;
            for (id, bytes) in &proof_box_bytes {
                serialized_box_bytes = serialized_box_bytes.saturating_add(bytes.len());
                proof_boxes.insert(*id, tx_validation::deserialize_box(bytes)?);
            }

            let eval = crate::DeferredEval::new(
                header.height,
                parsed_txs.transactions,
                proof_boxes,
                header.clone(),
                preceding_headers.to_vec(),
                active_params.clone(),
                block_txs,
                serialized_box_bytes,
            );

            match self.script_eval_mode {
                ScriptEvalMode::Deferred => Some(eval),
                ScriptEvalMode::Inline => {
                    // Cost discarded, not unchecked — the maxBlockCost gate
                    // runs inside evaluate_scripts.
                    tx_validation::evaluate_scripts(&eval)?;
                    None
                }
            }
        } else {
            None
        };

        // 8. Advance state
        self.current_digest = header.state_root;
        self.validated_height = header.height;

        tracing::debug!(height = header.height, "state applied (digest mode)");

        Ok(ApplyStateOutcome {
            epoch_boundary_params,
            epoch_boundary_proposed_update,
            deferred_eval,
        })
    }

    fn validated_height(&self) -> u32 {
        self.validated_height
    }

    fn current_digest(&self) -> &ADDigest {
        &self.current_digest
    }

    fn reset_to(&mut self, height: u32, digest: ADDigest) -> Result<(), ValidationError> {
        // Plain field assignment — no persistent state to roll back, so
        // this reset is infallible (contract: DigestValidator always Ok).
        self.validated_height = height;
        self.current_digest = digest;
        tracing::info!(height, "validator reset to fork point");
        Ok(())
    }

    /// Digest mode owns no persistent state — there is nothing to flush and
    /// no cache to resize, so there is no [`StatePersistence`] to hand out.
    ///
    /// ⚠ The absence of a [`StatePersistence`] impl is the mode signal. Do
    /// not "helpfully" give `DigestValidator` a no-op one — a defaulted
    /// `resize_cache` on `BlockValidator` is exactly what let the enum
    /// wrapper in `src/main.rs` drop the at-tip cache resize for the life of
    /// the feature while logging success.
    fn state_persistence(&self) -> Option<&dyn StatePersistence> {
        None
    }
}

#[cfg(test)]
mod tests {
    //! Digest-mode script evaluation modes. The blocks here are real: a
    //! `BatchAVLProver` builds the tree and emits the block's AD proof, which
    //! is what the validator's `BatchAVLVerifier` replays — the same
    //! prover→verifier path a digest-mode node walks against a serving peer.

    use super::*;
    use crate::test_support::*;
    use crate::ScriptEvalMode;
    use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
    use ergo_avltree_rust::operation::{KeyValue, Operation};
    use ergo_chain_types::blake2b256_hash;
    use ergo_lib::chain::transaction::Transaction;
    use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;

    /// A block ready to feed `apply_state`, with the AD proof that backs it.
    struct Fixture {
        pre_digest: ADDigest,
        header: Header,
        txs: Vec<u8>,
        proofs: Vec<u8>,
        extension: Vec<u8>,
        preceding: Vec<Header>,
    }

    /// Seed a tree with `boxes`, then run `transactions` over it, keeping the
    /// resulting proof. The trailing `generate_proof` after seeding is what
    /// makes the block's proof cover the block's operations and nothing else.
    fn fixture(boxes: &[ErgoBox], transactions: &[Transaction]) -> Fixture {
        let tree = AVLTree::with_resolver(label_preserving_resolver(), KEY_LENGTH, None);
        let mut prover = BatchAVLProver::new(tree, false);

        for b in boxes {
            prover
                .perform_one_operation(&Operation::Insert(KeyValue {
                    key: Bytes::copy_from_slice(&box_key(b)),
                    value: Bytes::from(serialized_box(b)),
                }))
                .expect("seed insert");
        }
        let _ = prover.generate_proof();
        let pre_digest = ad_digest(&prover.digest().expect("seeded tree has a root"));

        for op in block_operations(transactions) {
            prover.perform_one_operation(&op).expect("block operation");
        }
        let post_digest = ad_digest(&prover.digest().expect("post-block root"));
        let proof = prover.generate_proof();

        let (txs, extension) = sections(transactions);
        Fixture {
            pre_digest,
            header: make_header(BLOCK_HEIGHT, post_digest, blake2b256_hash(proof.as_ref())),
            txs,
            proofs: crate::sections::serialize_ad_proofs(&HEADER_ID, proof.as_ref()),
            extension,
            preceding: preceding_headers(),
        }
    }

    impl Fixture {
        fn validator(&self, mode: ScriptEvalMode) -> DigestValidator {
            DigestValidator::from_state(self.pre_digest, SEED_HEIGHT, 0, mode)
        }

        fn apply(
            &self,
            validator: &mut DigestValidator,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            validator.apply_state(
                &self.header,
                &self.txs,
                Some(&self.proofs),
                &self.extension,
                &self.preceding,
                &Parameters::default(),
                None,
                None,
            )
        }
    }

    /// The digest-mode half of the mode contract: identical acceptance,
    /// differing only in who is left holding the evaluation.
    #[test]
    fn a_valid_block_differs_only_in_who_owes_the_evaluation() {
        let input = make_box(true, 11);
        let tx = spend_tx(std::slice::from_ref(&input));
        let f = fixture(std::slice::from_ref(&input), std::slice::from_ref(&tx));

        let mut deferred = f.validator(ScriptEvalMode::Deferred);
        let deferred_outcome = f.apply(&mut deferred).expect("valid block applies");
        assert!(
            deferred_outcome.deferred_eval.is_some(),
            "deferred mode must hand the evaluation back"
        );
        assert_eq!(deferred.validated_height(), BLOCK_HEIGHT);
        assert_eq!(*deferred.current_digest(), f.header.state_root);

        let mut inline = f.validator(ScriptEvalMode::Inline);
        let inline_outcome = f.apply(&mut inline).expect("valid block applies");
        assert!(
            inline_outcome.deferred_eval.is_none(),
            "inline mode already evaluated; Ok is the verdict"
        );
        assert_eq!(inline.validated_height(), BLOCK_HEIGHT);
        assert_eq!(*inline.current_digest(), f.header.state_root);

        crate::evaluate_scripts(&deferred_outcome.deferred_eval.unwrap())
            .expect("the block's scripts are satisfied");
    }

    /// Digest mode has no prover and no persistence — its whole state is two
    /// fields written at the very end — so "Err leaves nothing behind" means
    /// those two fields never move.
    #[test]
    fn inline_script_failure_leaves_state_unchanged() {
        let input = make_box(false, 12);
        let tx = spend_tx(std::slice::from_ref(&input));
        let f = fixture(std::slice::from_ref(&input), std::slice::from_ref(&tx));

        let mut inline = f.validator(ScriptEvalMode::Inline);
        let err = f
            .apply(&mut inline)
            .expect_err("an unsatisfied script must be rejected inline");
        assert!(
            matches!(err, ValidationError::TransactionInvalid { .. }),
            "expected a script failure, got: {err:?}"
        );
        assert_eq!(inline.validated_height(), SEED_HEIGHT);
        assert_eq!(*inline.current_digest(), f.pre_digest);

        // Same block, deferred: accepted here, rejected by the caller later.
        let mut deferred = f.validator(ScriptEvalMode::Deferred);
        let outcome = f
            .apply(&mut deferred)
            .expect("deferred mode does not evaluate scripts");
        assert_eq!(deferred.validated_height(), BLOCK_HEIGHT);
        let eval = outcome
            .deferred_eval
            .expect("deferred mode owes the caller an evaluation");
        assert!(
            matches!(
                crate::evaluate_scripts(&eval),
                Err(ValidationError::TransactionInvalid { .. })
            ),
            "the deferred verdict must be the same rejection, just later"
        );
    }

    /// Below the checkpoint neither mode looks at a script — the AD proof
    /// replay alone is the guarantee.
    #[test]
    fn the_checkpoint_still_outranks_the_mode() {
        let input = make_box(false, 13);
        let tx = spend_tx(std::slice::from_ref(&input));
        let f = fixture(std::slice::from_ref(&input), std::slice::from_ref(&tx));

        for mode in [ScriptEvalMode::Deferred, ScriptEvalMode::Inline] {
            let mut validator =
                DigestValidator::from_state(f.pre_digest, SEED_HEIGHT, BLOCK_HEIGHT, mode);
            let outcome = f
                .apply(&mut validator)
                .expect("checkpointed block applies without script evaluation");
            assert!(
                outcome.deferred_eval.is_none(),
                "{mode:?}: nothing is owed below the checkpoint"
            );
        }
    }
}

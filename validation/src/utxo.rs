//! UtxoValidator: persistent AVL+ tree state verification via BatchAVLProver.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use bytes::Bytes;
use enr_state::RedbAVLStorage;
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use ergo_chain_types::{ADDigest, Header};
use ergo_lib::chain::parameters::Parameters;

use crate::sections::{parse_block_transactions, parse_extension};
use crate::state_changes::{compute_state_changes, transactions_to_summaries};
use crate::tx_validation;
use crate::voting;
use crate::{ApplyStateOutcome, BlockValidator, ValidationError};

/// UTXO-mode block validator.
///
/// Verifies state transitions by applying operations to a persistent AVL+ tree
/// (BatchAVLProver over RedbAVLStorage). Boxes come from the tree's
/// Lookup/Remove results, not from AD proofs. Storage and prover are held as
/// separate fields so the validator can call the storage's inherent
/// `update_with_height` to commit block_height atomically with state — the
/// `VersionedAVLStorage` trait only exposes the block-height-unaware `update`.
///
/// Stateless w.r.t. blockchain parameters: the caller passes `active_params`
/// on every `validate_block` call. The validator does not own a `Parameters`
/// field — chain submodule is the single source of truth.
///
/// Shares section parsing, state change computation, and transaction validation
/// with DigestValidator — only the state root verification mechanism differs.
pub struct UtxoValidator {
    storage: RedbAVLStorage,
    prover: BatchAVLProver,
    validated_height: u32,
    checkpoint_height: u32,
    current_digest: ADDigest,
    /// Current emission box ID (changes every block). None if all ERG emitted.
    emission_box_id: Option<[u8; 32]>,
    /// Emission contract ErgoTree bytes for matching outputs.
    emission_tree_bytes: Vec<u8>,
    /// Heights at which to persist the apply-time ADProof. Empty = disabled.
    adproof_dump_heights: HashSet<u32>,
    /// Directory for dumped `adproofs-<height>.104` sections. None = disabled.
    adproof_dump_dir: Option<PathBuf>,
}

impl UtxoValidator {
    /// Create a UtxoValidator from initialized storage and prover.
    ///
    /// The caller is responsible for arranging the prover's in-memory tree to
    /// match `storage.version()`: either by calling `storage.rollback(&version)`
    /// and installing the returned root, or by performing the genesis-bootstrap
    /// insertions plus a first `storage.update_with_height(&mut prover, vec![], 0)`.
    pub fn new(
        storage: RedbAVLStorage,
        prover: BatchAVLProver,
        height: u32,
        checkpoint_height: u32,
    ) -> Self {
        let digest_bytes = prover.digest().expect("prover has no root");
        let digest = bytes_to_ad_digest(&digest_bytes);

        // Compute emission contract ErgoTree bytes for box matching.
        // Uses mainnet MonetarySettings — the emission contract is the same
        // across mainnet/testnet (same script, different genesis boxes).
        use ergo_lib::chain::ergo_tree_predef;
        use ergo_lib::chain::emission::MonetarySettings;
        use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

        let emission_tree_bytes =
            if let Ok(tree) = ergo_tree_predef::emission_box_prop(&MonetarySettings::default()) {
                tree.sigma_serialize_bytes().unwrap_or_default()
            } else {
                Vec::new()
            };

        Self {
            storage,
            prover,
            validated_height: height,
            checkpoint_height,
            current_digest: digest,
            emission_box_id: None,
            emission_tree_bytes,
            adproof_dump_heights: HashSet::new(),
            adproof_dump_dir: None,
        }
    }

    /// Persist the generated ADProof as a raw type-104 section at each height
    /// in `heights`, into `dir`. Disabled by default (empty set / `None` dir):
    /// zero overhead in normal operation. Intended for one-shot regeneration
    /// via a genesis→target replay — the prover must pass through H-1 → H for
    /// the proof at H to be correct — NOT steady-state serving.
    pub fn set_adproof_dump(&mut self, heights: HashSet<u32>, dir: PathBuf) {
        self.adproof_dump_heights = heights;
        self.adproof_dump_dir = Some(dir);
    }
}

impl BlockValidator for UtxoValidator {
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
        // The op loop in apply_state_internal mutates the in-memory prover.
        // An early-return after partial mutation leaves the prover dirty;
        // sync's retry then re-enters with a half-applied tree and surfaces
        // a different error on a different op number, burying the original
        // failure cause. Roll the prover back to pre-block state on any
        // failure so retries are deterministic and the original error
        // survives.
        let saved_digest = self.current_digest;

        match self.apply_state_internal(
            header,
            block_txs,
            ad_proofs,
            extension,
            preceding_headers,
            active_params,
            expected_boundary_params,
            expected_proposed_update,
        ) {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                self.rollback_prover_to(saved_digest);
                Err(e)
            }
        }
    }

    fn validated_height(&self) -> u32 {
        self.validated_height
    }

    fn current_digest(&self) -> &ADDigest {
        &self.current_digest
    }

    fn reset_to(&mut self, height: u32, digest: ADDigest) -> Result<(), ValidationError> {
        let digest_bytes: [u8; 33] = digest.into();
        let avl_digest = Bytes::copy_from_slice(&digest_bytes);

        // The rollback is the only fallible step and nothing may mutate
        // before it succeeds: on Err the contract guarantees
        // validated_height/current_digest/prover are exactly as before.
        let (root, tree_height) = self.storage.rollback(&avl_digest).map_err(|e| {
            ValidationError::StateOperationFailed(format!(
                "rollback to height {height} failed: {e}"
            ))
        })?;
        self.prover.base.tree.root = Some(root);
        self.prover.base.tree.height = tree_height;

        // Drop stale dirty-node bookkeeping from the pre-rollback chain —
        // the freshly-unpacked tree is the ground truth. Without this, the
        // next flush's undo record would list nodes from the demoted branch
        // as "inserted", and a subsequent rollback would delete them.
        self.prover.base.tree.reset();
        self.prover.base.changed_nodes_buffer.clear();
        self.prover.base.changed_nodes_buffer_to_check.clear();

        self.validated_height = height;
        self.current_digest = digest;
        tracing::info!(height, "UTXO validator reset to fork point");
        Ok(())
    }

    fn flush(&self) -> Result<(), ValidationError> {
        self.storage.flush().map_err(|e| {
            ValidationError::StateOperationFailed(format!("flush: {e}"))
        })
    }

    fn proofs_for_transactions(
        &self,
        txs: &[ergo_lib::chain::transaction::Transaction],
    ) -> Option<Result<(Vec<u8>, ADDigest), ValidationError>> {
        Some(self.compute_proofs(txs))
    }

    fn emission_box_id(&self) -> Option<[u8; 32]> {
        self.emission_box_id
    }
}

impl UtxoValidator {
    /// Apply a block, mutating the prover's in-memory tree and committing
    /// state changes to storage. Wrapped by `apply_state` (the trait impl)
    /// to handle rollback-on-failure — call sites should always go through
    /// the trait method.
    #[allow(clippy::too_many_arguments)]
    fn apply_state_internal(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        _ad_proofs: Option<&[u8]>,
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

        // 1. Parse sections (AD proofs not needed in UTXO mode)
        let parsed_txs = parse_block_transactions(block_txs)?;
        let parsed_ext = parse_extension(extension)?;

        // 1a. Epoch-boundary parameter check (consensus-critical).
        // Uses JVM v6 matchParameters60 semantics: local can have fewer
        // entries than received, every entry in local must match received.
        // At v4+ the proposedUpdate byte-for-byte comparison also runs.
        let (epoch_boundary_params, epoch_boundary_proposed_update) = match expected_boundary_params {
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

        // 4. Apply operations to the prover, capturing old values
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
        let prover_digest = self
            .prover
            .digest()
            .ok_or_else(|| ValidationError::StateOperationFailed(
                "prover has no root after operations".to_string(),
            ))?;
        if prover_digest.as_ref() != expected_state_root.as_slice() {
            return Err(ValidationError::StateRootMismatch {
                expected: expected_state_root.to_vec(),
                got: prover_digest.to_vec(),
            });
        }

        // 6. Build DeferredEval for deferred script verification
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

        // 7. Persist state changes atomically with block_height, then
        //    generate the AD proof. generate_proof() also flushes the prover's
        //    tree-local state (resets visited/new flags, directions, and
        //    old_top_node) for the next block — that side effect is why the
        //    call is kept even when the proof is dropped. The proof returned
        //    here is the same one a digest-mode peer would verify: it covers
        //    exactly this block's operations and replays cleanly from the
        //    H-1 state root (confirmed by tests/adproof_dump_ordering_diag.rs,
        //    which proves it survives the storage-side tree.reset()). Steady-
        //    state serving is Phase 6; here it can optionally be dumped at
        //    configured heights for ADProof regeneration (set_adproof_dump).
        self.storage
            .update_with_height(&mut self.prover, vec![], header.height)
            .map_err(|e| ValidationError::StateOperationFailed(
                format!("persist failed: {e}"),
            ))?;
        let proof = self.prover.generate_proof();
        if let Some(dir) = &self.adproof_dump_dir {
            if self.adproof_dump_heights.contains(&header.height) {
                // Raw type-104 section: [header_id:32][proof_size:VLQ][proof].
                let section =
                    crate::sections::serialize_ad_proofs(&header.id.0 .0, proof.as_ref());
                let path = dir.join(format!("adproofs-{}.104", header.height));
                match std::fs::write(&path, &section) {
                    Ok(()) => tracing::info!(
                        height = header.height,
                        bytes = section.len(),
                        path = %path.display(),
                        "dumped ADProof (type-104 section)"
                    ),
                    Err(e) => tracing::warn!(
                        height = header.height,
                        error = %e,
                        "ADProof dump write failed (non-fatal)"
                    ),
                }
            }
        }

        // 8. Track emission box: scan new outputs for emission contract
        if !self.emission_tree_bytes.is_empty() {
            self.emission_box_id = None;
            for (box_id, box_bytes) in &changes.insertions {
                if let Ok(ergo_box) = tx_validation::deserialize_box(box_bytes) {
                    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
                    if let Ok(tree_bytes) = ergo_box.ergo_tree.sigma_serialize_bytes() {
                        if tree_bytes == self.emission_tree_bytes {
                            self.emission_box_id = Some(*box_id);
                            break;
                        }
                    }
                }
            }
        }

        // 9. Advance state
        self.current_digest = header.state_root;
        self.validated_height = header.height;

        tracing::debug!(height = header.height, "state applied (UTXO mode)");

        Ok(ApplyStateOutcome {
            epoch_boundary_params,
            epoch_boundary_proposed_update,
            deferred_eval,
        })
    }

    /// Restore the in-memory prover to match the on-disk state at `digest`.
    /// Called after a failed `apply_state` to clear half-applied operations
    /// from the prover's tree before sync retries the block.
    ///
    /// Storage's `current_version` typically equals `digest` after a
    /// failure (we haven't called `update_with_height` yet), so
    /// `storage.rollback` hits its short-circuit path and just unpacks
    /// the on-disk root — fast.
    fn rollback_prover_to(&mut self, digest: ADDigest) {
        let digest_bytes: [u8; 33] = digest.into();
        let avl_digest = Bytes::copy_from_slice(&digest_bytes);

        match self.storage.rollback(&avl_digest) {
            Ok((root, tree_height)) => {
                self.prover.base.tree.root = Some(root);
                self.prover.base.tree.height = tree_height;
                // Mirrors reset_to — feedback_avl_prover_flush_reset.md.
                self.prover.base.tree.reset();
                self.prover.base.changed_nodes_buffer.clear();
                self.prover.base.changed_nodes_buffer_to_check.clear();
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "prover rollback after apply_state failure also failed; \
                     validator state may be inconsistent — operator restart required"
                );
            }
        }
    }

    /// Compute AD proofs and new state root for a set of transactions
    /// without modifying persistent state.
    ///
    /// Uses BatchAVLProver::generate_proof_for_operations which creates
    /// a temporary prover copy internally. The real prover is untouched.
    fn compute_proofs(
        &self,
        txs: &[ergo_lib::chain::transaction::Transaction],
    ) -> Result<(Vec<u8>, ADDigest), ValidationError> {
        use crate::state_changes::{compute_state_changes, transactions_to_summaries};

        // 1. Convert transactions to state changes
        let summaries = transactions_to_summaries(txs)?;
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

fn bytes_to_ad_digest(bytes: &Bytes) -> ADDigest {
    let mut arr = [0u8; 33];
    arr.copy_from_slice(bytes);
    ADDigest::from(arr)
}

//! UtxoValidator: persistent AVL+ tree state verification via BatchAVLProver.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use bytes::Bytes;
use enr_state::RedbAVLStorage;
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::AVLTree;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use ergo_chain_types::{blake2b256_hash, ADDigest, Header};
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
        self.prover.restore_root(root, tree_height);

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

    fn resize_cache(&self, cache_bytes: usize) -> Result<(), ValidationError> {
        self.storage.resize_cache(cache_bytes);
        Ok(())
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
            // Summed inside the loop that already runs — the serialized size
            // feeds DeferredEval::new's heap estimate at no extra walk.
            let mut serialized_box_bytes = 0usize;
            for (id, bytes) in &proof_box_bytes {
                serialized_box_bytes = serialized_box_bytes.saturating_add(bytes.len());
                proof_boxes.insert(*id, tx_validation::deserialize_box(bytes)?);
            }

            Some(crate::DeferredEval::new(
                header.height,
                parsed_txs.transactions,
                proof_boxes,
                header.clone(),
                preceding_headers.to_vec(),
                active_params.clone(),
                block_txs,
                serialized_box_bytes,
            ))
        } else {
            None
        };

        // 7. Persist state changes atomically with block_height.
        //    Must precede generate_proof(): update_internal builds its delete
        //    list from prover.removed_nodes(), and generate_proof() clears the
        //    changed-node buffers that back it. With the proof first (the
        //    v0.7.5 order, commit 2c0811f) removed_nodes() returned empty on
        //    every block, so superseded nodes were never deleted from
        //    NODES_TABLE — unbounded orphan growth (235 GB at height ~1.66M,
        //    ~658 KB/block, zero deletions).
        self.storage
            .update_with_height(&mut self.prover, vec![], header.height)
            .map_err(|e| ValidationError::StateOperationFailed(
                format!("persist failed: {e}"),
            ))?;

        // 8. Generate the AD proof AFTER persisting — the canonical order,
        //    matching the JVM's
        //    PersistentBatchAVLProver.generateProofAndUpdateStorage
        //    (storage.update, then generateProof).
        //
        //    Safe as of ergo_avltree_rust #18 (in pinned rev 2941396):
        //    pack_tree now gates on was_modified() — an Rc::as_ptr scan of
        //    modified_nodes — instead of the tree's visited flags, and
        //    modified_nodes survives update_with_height's tree.reset().
        //    Before #18 this order collapsed Lookup-bearing proofs to a
        //    single root label (block 28474, isolated by SANTA); on the
        //    pinned rev both orders yield byte-identical proof bytes.
        let proof = self.prover.generate_proof();

        // TEMP: 28474 Lookup divergence investigation — remove after SANTA resolves
        if header.height == 28474 {
            let internal_digest: [u8; 32] = blake2b256_hash(proof.as_ref()).0;
            let expected_digest: [u8; 32] = header.ad_proofs_root.into();
            tracing::info!(
                height = 28474,
                proof_digest = %hex::encode(internal_digest),
                ad_proofs_root = %hex::encode(expected_digest),
                proof_bytes_hex = %hex::encode(proof.as_ref()),
                "28474 diagnostics"
            );
            for (i, op) in operations.iter().enumerate() {
                match op {
                    Operation::Lookup(key) => {
                        tracing::info!(height=28474, op_index=i, op_kind="Lookup", box_id=%hex::encode(key.as_ref()));
                    }
                    Operation::Remove(key) => {
                        tracing::info!(height=28474, op_index=i, op_kind="Remove", box_id=%hex::encode(key.as_ref()));
                    }
                    Operation::Insert(kv) => {
                        tracing::info!(height=28474, op_index=i, op_kind="Insert", box_id=%hex::encode(kv.key.as_ref()));
                    }
                    _ => {}
                }
            }
        }

        // Consensus: the JVM checks that internally-generated proof bytes hash
        // to the header's declared ad_proofs_root, even for proofless blocks.
        // Without this, a Rust-mined block whose AVL proof serialization differs
        // from the JVM's gets accepted by Rust nodes but rejected by the JVM
        // reference node (live testnet fork at height 431,367).
        let internal_proof_digest = blake2b256_hash(proof.as_ref()).0;
        let expected_proof_digest: [u8; 32] = header.ad_proofs_root.into();
        if internal_proof_digest != expected_proof_digest {
            tracing::error!(
                height = header.height,
                expected = %hex::encode(expected_proof_digest),
                got = %hex::encode(internal_proof_digest),
                "proof-digest mismatch"
            );
            return Err(ValidationError::ProofDigestMismatch {
                expected: expected_proof_digest,
                got: internal_proof_digest,
            });
        }

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

        // 9. Track emission box: scan new outputs for emission contract
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

        // 10. Advance state
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
                self.prover.restore_root(root, tree_height);
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
    /// Builds a **separate** prover from storage (fresh tree, no shared Rc
    /// nodes).  The main prover's tree is never touched — the old path of
    /// calling `prover.generate_proof_for_operations()` cloned the tree
    /// shallowly (`Rc<RefCell<Node>>`) and operations on the clone mutated
    /// shared nodes (visited flags + children pointers during restructuring),
    /// corrupting the main prover for the next block apply (state-root
    /// mismatch on the first attempt; self-healing retry after rollback).
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

        // 3. Build a separate prover from storage — do NOT touch self.prover.
        //    generate_proof_for_operations clones the tree shallowly (Rc)
        //    and operations on the clone mutate shared nodes, corrupting the
        //    main prover.  Instead, load the current root from storage into a
        //    fresh tree with its own resolver — every resolved node is a new
        //    Rc<RefCell<Node>>, independent of the main prover.
        let (root_label, tree_height) = self
            .storage
            .root_state()
            .ok_or_else(|| {
                ValidationError::StateOperationFailed(
                    "no root state for mining proof computation".to_string(),
                )
            })?;
        let root_bytes = self
            .storage
            .get_node(&root_label)
            .map_err(|e| {
                ValidationError::StateOperationFailed(format!(
                    "mining proof: failed to read root node: {e}"
                ))
            })?
            .ok_or_else(|| {
                ValidationError::StateOperationFailed(
                    "mining proof: root node not found in storage".to_string(),
                )
            })?;

        let mut tree = AVLTree::with_resolver(self.storage.resolver(), 32, None);
        let root_node = tree.unpack(&root_bytes);
        tree.root = Some(root_node);
        tree.height = tree_height;

        let mut temp_prover = BatchAVLProver::new(tree, false);
        for op in &operations {
            temp_prover
                .perform_one_operation(op)
                .map_err(|e| {
                    ValidationError::StateOperationFailed(format!(
                        "mining proof operation failed: {e}"
                    ))
                })?;
        }
        let proof_bytes = temp_prover.generate_proof();
        let new_digest = temp_prover.digest().ok_or_else(|| {
            ValidationError::StateOperationFailed(
                "temp prover has no root after mining operations".to_string(),
            )
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

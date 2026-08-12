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
use crate::{ApplyStateOutcome, BlockValidator, ScriptEvalMode, ValidationError};

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
    /// Whether `apply_state` evaluates this block's scripts itself or hands
    /// the caller a `DeferredEval` to do it. Fixed at construction.
    script_eval_mode: ScriptEvalMode,
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
    ///
    /// `script_eval_mode` is spelled out at every call site on purpose rather
    /// than defaulted: it must agree with the sync layer's own configuration,
    /// and a knob that can be forgotten silently is how the `resize_cache`
    /// no-op survived a whole feature's lifetime (facts/validation.md).
    pub fn new(
        storage: RedbAVLStorage,
        prover: BatchAVLProver,
        height: u32,
        checkpoint_height: u32,
        script_eval_mode: ScriptEvalMode,
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
            script_eval_mode,
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
        // Every Err leaves the prover byte-for-byte as it was on entry
        // (facts/validation.md, "Err leaves the prover clean"). This wrapper
        // is the only thing that makes that true: apply_state_internal
        // mutates the prover in step 4 and every Err return below it — the
        // digest check, box deserialization, inline script evaluation, the
        // persist, the proof-digest check — is a bare `return`/`?` that undoes
        // nothing on its own.
        //
        // Two failure regions, one mechanism:
        //   - before `update_with_height`: storage's current_version is still
        //     `saved_digest`, so `rollback` short-circuits to a re-read of the
        //     persisted root (state/src/storage.rs, the current_version ==
        //     version branch). No undo log involved — this block was never a
        //     version.
        //   - after it (the proof-digest check): the block *is* the newest
        //     version, and the same call walks the undo log back one step.
        //
        // Without it, sync's retry re-enters on a half-applied tree and fails
        // on a different op with a different error, burying the real cause —
        // and in inline mode, where a rejected block is routine rather than
        // near-unreachable, the tree would carry a rejected block's mutations
        // into the next candidate.
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

        // 6. Script evaluation — bundled for the caller, or run right here.
        //
        // Inline evaluation sits between the digest check and persistence,
        // not before the prover mutations. The JVM checks scripts before
        // touching its AVL tree, but it reads each input box via boxById and
        // pays a second traversal to remove it; step 4 above captures the box
        // from the removal's own return value, so copying that ordering would
        // buy a read per input and nothing else. Persistence is the boundary
        // that matters, because persistence is what survives a crash:
        // everything up to `update_with_height` below is in memory, and an
        // Err from here rewinds the prover (see `apply_state`), so no
        // unverified state ever reaches state.redb.
        //
        // Both modes go through `evaluate_scripts`, which means both modes
        // evaluate identically — the alternative, calling the underlying
        // validator directly here, is a second entry point that can drift on
        // a consensus path. The bundle's `approx_heap_bytes` is dead weight
        // inline (nothing queues the value; it is dropped at the end of this
        // expression), and that is the price of the single path.
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
                    // The cost is discarded, not unchecked: the maxBlockCost
                    // consensus gate runs inside evaluate_scripts. Sync
                    // discards it on the deferred path too.
                    tx_validation::evaluate_scripts(&eval)?;
                    None
                }
            }
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

                // `restore_root` clears the changed-node buffers and the
                // directions and rebases `old_top_node`, but leaves
                // `modified_nodes` — the address-keyed map `pack_tree` gates
                // on — holding every node the failed block touched. Nothing
                // is misidentified afterwards, because each entry owns an Rc
                // and a live address cannot be recycled; the cost is that
                // those entries pin the touched subtree for the life of the
                // process. Deferred mode reaches this path about never.
                // Inline mode reaches it once per rejected block, i.e. as
                // often as a peer cares to send one, which turns an
                // accounting wart into an unbounded retention.
                //
                // The proper home for this is `restore_root` itself, next to
                // the other three clears it already does — an upstream change
                // in the ergo_avltree_rust fork. Until that lands, clearing
                // here is what makes "the prover is as it was on entry" true
                // of the whole prover rather than just its tree.
                self.prover.base.modified_nodes.clear();
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

#[cfg(test)]
mod tests {
    //! UTXO-mode script evaluation modes, and the Err postcondition that
    //! makes a rejected block survivable: *the prover* is byte-for-byte as it
    //! was on entry (facts/validation.md, "Err leaves the prover clean").
    //!
    //! These live inside the crate rather than in `tests/` for one reason: the
    //! postcondition is about `self.prover`, and from outside the only view of
    //! the tree is `proofs_for_transactions`, which builds its own prover from
    //! *storage*. Storage is untouched on the pre-persist failure paths, so an
    //! out-of-crate test would report "unchanged" no matter how dirty the
    //! in-memory tree was — a test that cannot fail. Here we read
    //! `prover.digest()` directly.

    use super::*;
    use crate::test_support::*;
    use crate::{ErgoBox, ScriptEvalMode};
    use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage};
    use ergo_chain_types::Digest32;
    use ergo_lib::chain::transaction::Transaction;
    use tempfile::TempDir;

    const KEY_LEN: usize = 32;

    fn open_storage(dir: &TempDir) -> RedbAVLStorage {
        RedbAVLStorage::open(
            &dir.path().join("state.redb"),
            AVLTreeParams {
                key_length: KEY_LEN,
                value_length: None,
            },
            10,
            CacheSize::default(),
        )
        .expect("fresh redb opens")
    }

    /// Seed a storage/prover pair with `boxes` committed at `SEED_HEIGHT`,
    /// left in the cycle state a steady-state validator is in: the storage
    /// commit clears the dirty-node bookkeeping, and the trailing
    /// `generate_proof` rebases the proof baseline exactly as step 8 of a
    /// completed `apply_state` does. Without that rebase the next block's
    /// internally generated proof would be packed from a stale root and every
    /// test here would fail on the proof-digest check for the wrong reason.
    fn seed(storage: &mut RedbAVLStorage, boxes: &[ErgoBox]) -> BatchAVLProver {
        let tree = AVLTree::with_resolver(storage.resolver(), KEY_LEN, None);
        let mut prover = BatchAVLProver::new(tree, true);
        for b in boxes {
            prover
                .perform_one_operation(&Operation::Insert(KeyValue {
                    key: Bytes::copy_from_slice(&box_key(b)),
                    value: Bytes::from(serialized_box(b)),
                }))
                .expect("seed insert");
        }
        storage
            .update_with_height(&mut prover, vec![], SEED_HEIGHT)
            .expect("seed commit");
        let _ = prover.generate_proof();
        prover
    }

    fn seeded_validator(boxes: &[ErgoBox], mode: ScriptEvalMode) -> (UtxoValidator, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let prover = seed(&mut storage, boxes);
        (
            UtxoValidator::new(storage, prover, SEED_HEIGHT, 0, mode),
            dir,
        )
    }

    /// What the block under test must claim to be accepted: the post-block
    /// state root, and the digest of the proof the validator will generate
    /// internally. Computed by replaying the identical sequence — seed,
    /// commit, rebase, block operations, commit, proof — on an independent
    /// storage/prover pair, in the same order `apply_state_internal` uses.
    fn oracle(boxes: &[ErgoBox], ops: &[Operation]) -> (ADDigest, Digest32) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let mut prover = seed(&mut storage, boxes);

        for op in ops {
            prover.perform_one_operation(op).expect("oracle operation");
        }
        let state_root = bytes_to_ad_digest(&prover.digest().expect("oracle has a root"));
        storage
            .update_with_height(&mut prover, vec![], BLOCK_HEIGHT)
            .expect("oracle commit");
        let proof = prover.generate_proof();

        (state_root, blake2b256_hash(proof.as_ref()))
    }

    fn prover_digest(validator: &UtxoValidator) -> ADDigest {
        bytes_to_ad_digest(&validator.prover.digest().expect("prover has a root"))
    }

    /// Everything an `apply_state` call needs for a one-transaction block.
    struct Block {
        header: Header,
        txs: Vec<u8>,
        extension: Vec<u8>,
        preceding: Vec<Header>,
    }

    impl Block {
        fn new(transactions: &[Transaction], state_root: ADDigest, ad_root: Digest32) -> Self {
            let (txs, extension) = sections(transactions);
            Self {
                header: make_header(BLOCK_HEIGHT, state_root, ad_root),
                txs,
                extension,
                preceding: preceding_headers(),
            }
        }

        fn apply(&self, validator: &mut UtxoValidator) -> Result<ApplyStateOutcome, ValidationError> {
            validator.apply_state(
                &self.header,
                &self.txs,
                None,
                &self.extension,
                &self.preceding,
                &Parameters::default(),
                None,
                None,
            )
        }
    }

    /// A rejected block must leave nothing of itself behind. The digest check
    /// fires after step 4 has applied every operation, so this is the path
    /// where "nothing undoes them" would show up.
    #[test]
    fn digest_mismatch_leaves_the_prover_clean() {
        let input = make_box(true, 1);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));

        let (mut validator, _dir) = seeded_validator(
            std::slice::from_ref(&input),
            ScriptEvalMode::Deferred,
        );
        let before = prover_digest(&validator);

        // Not a vacuous test: this block really does move the tree, so an
        // un-rewound prover would be observably different afterwards.
        let (real_state_root, _) = oracle(std::slice::from_ref(&input), &ops);
        assert_ne!(
            real_state_root, before,
            "fixture is degenerate — the block changes nothing, so no rewind is needed"
        );

        // The block claims the tree did not change. Every operation is applied
        // before that claim is checked.
        let block = Block::new(std::slice::from_ref(&tx), before, Digest32::zero());
        let err = block
            .apply(&mut validator)
            .expect_err("a wrong state root must be rejected");
        assert!(
            matches!(err, ValidationError::StateRootMismatch { .. }),
            "unexpected error variant: {err:?}"
        );

        assert_eq!(validator.validated_height(), SEED_HEIGHT);
        assert_eq!(*validator.current_digest(), before);
        assert_eq!(
            prover_digest(&validator),
            before,
            "the prover kept the rejected block's mutations"
        );
        assert!(
            validator.prover.base.modified_nodes.is_empty(),
            "the rejected block's touched nodes are still pinned in the proof-cycle map"
        );
    }

    /// Inline mode's own rejection path — the one that turns the case above
    /// from near-unreachable into routine. The block is otherwise perfectly
    /// well-formed; only its script fails.
    #[test]
    fn inline_script_failure_leaves_the_prover_clean_and_storage_untouched() {
        let input = make_box(false, 2);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);

        let (mut validator, _dir) =
            seeded_validator(std::slice::from_ref(&input), ScriptEvalMode::Inline);
        let before = prover_digest(&validator);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);

        let err = block
            .apply(&mut validator)
            .expect_err("an unsatisfied script must be rejected inline");
        assert!(
            matches!(err, ValidationError::TransactionInvalid { .. }),
            "expected a script failure, got: {err:?}"
        );

        assert_eq!(validator.validated_height(), SEED_HEIGHT);
        assert_eq!(*validator.current_digest(), before);
        assert_eq!(
            prover_digest(&validator),
            before,
            "the prover kept the rejected block's mutations"
        );
        assert!(
            validator.prover.base.modified_nodes.is_empty(),
            "the rejected block's touched nodes are still pinned in the proof-cycle map"
        );

        // The whole point of evaluating before `update_with_height`: the
        // rejected block never reached storage. `proofs_for_transactions`
        // reads the tree back out of storage, so this observes the durable
        // side specifically, not the in-memory one asserted above.
        let (_, storage_digest) = validator
            .proofs_for_transactions(&[])
            .expect("UTXO mode computes proofs")
            .expect("empty-operation proof");
        assert_eq!(
            storage_digest, before,
            "an unverified block reached state.redb"
        );
    }

    /// The same failing block under `Deferred`: applied and persisted, with
    /// the verdict handed to the caller instead. Proves the two modes differ
    /// in *who evaluates* and that the block above fails for a script reason
    /// rather than something incidental to the fixture.
    #[test]
    fn deferred_defers_the_same_failing_block() {
        let input = make_box(false, 2);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);

        let (mut validator, _dir) =
            seeded_validator(std::slice::from_ref(&input), ScriptEvalMode::Deferred);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);

        let outcome = block
            .apply(&mut validator)
            .expect("deferred mode does not evaluate scripts, so this block applies");
        assert_eq!(validator.validated_height(), BLOCK_HEIGHT);

        let eval = outcome
            .deferred_eval
            .expect("deferred mode owes the caller an evaluation");
        let err = crate::evaluate_scripts(&eval)
            .expect_err("the deferred verdict is the same rejection, just later");
        assert!(
            matches!(err, ValidationError::TransactionInvalid { .. }),
            "expected a script failure, got: {err:?}"
        );
    }

    /// A block that is valid in every respect: both modes accept it, advance
    /// to the same state, and differ only in whether they hand back an
    /// evaluation to run. `sync/` keys on exactly this.
    #[test]
    fn a_valid_block_differs_only_in_who_owes_the_evaluation() {
        let input = make_box(true, 3);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);

        let (mut deferred, _d1) =
            seeded_validator(std::slice::from_ref(&input), ScriptEvalMode::Deferred);
        let deferred_outcome = block.apply(&mut deferred).expect("valid block applies");
        assert!(
            deferred_outcome.deferred_eval.is_some(),
            "deferred mode must hand the evaluation back"
        );
        assert_eq!(deferred.validated_height(), BLOCK_HEIGHT);
        assert_eq!(*deferred.current_digest(), state_root);

        let (mut inline, _d2) =
            seeded_validator(std::slice::from_ref(&input), ScriptEvalMode::Inline);
        let inline_outcome = block.apply(&mut inline).expect("valid block applies");
        assert!(
            inline_outcome.deferred_eval.is_none(),
            "inline mode already evaluated; Ok is the verdict"
        );
        assert_eq!(inline.validated_height(), BLOCK_HEIGHT);
        assert_eq!(*inline.current_digest(), state_root);

        // And the evaluation deferred mode handed back really is the one
        // inline mode ran: it passes.
        crate::evaluate_scripts(&deferred_outcome.deferred_eval.unwrap())
            .expect("the block's scripts are satisfied");
    }

    /// Below the checkpoint neither mode evaluates — the state-root check
    /// alone is the guarantee, and `deferred_eval` is None in both.
    #[test]
    fn the_checkpoint_still_outranks_the_mode() {
        let input = make_box(false, 4);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);

        for mode in [ScriptEvalMode::Deferred, ScriptEvalMode::Inline] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut storage = open_storage(&dir);
            let prover = seed(&mut storage, std::slice::from_ref(&input));
            // Checkpoint at the block's own height: `height > checkpoint` is
            // false, so the unsatisfiable script is never looked at.
            let mut validator =
                UtxoValidator::new(storage, prover, SEED_HEIGHT, BLOCK_HEIGHT, mode);

            let outcome = block
                .apply(&mut validator)
                .expect("checkpointed block applies without script evaluation");
            assert!(
                outcome.deferred_eval.is_none(),
                "{mode:?}: nothing is owed below the checkpoint"
            );
        }
    }
}

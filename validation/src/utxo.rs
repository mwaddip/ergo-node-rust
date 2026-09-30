//! UtxoValidator: persistent AVL+ tree state verification via BatchAVLProver.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use bytes::Bytes;
use enr_state::RedbAVLStorage;
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::{AVLTree, Node, Resolver};
use ergo_avltree_rust::operation::{Digest32, KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use ergo_chain_types::{blake2b256_hash, ADDigest, Header};
use ergo_lib::chain::parameters::Parameters;

use crate::block_rules::validate_block_rules;
use crate::sections::{parse_block_transactions, parse_extension};
use crate::state_changes::{compute_state_changes, transactions_to_summaries, Insertion};
use crate::tx_validation;
use crate::voting;
use crate::{
    ApplyStateOutcome, BlockValidator, MiningState, ProverMemoryEstimate, ScriptEvalInputs,
    StatePersistence, ValidationError,
};

/// Where a freshly constructed [`UtxoValidator`] recovers `emission_box_id`
/// from.
///
/// [`UtxoValidator::new`] takes one of these by value, so a caller cannot
/// build a validator without saying where the emission box comes from.
/// `emission_box_id` is derived state maintained by `apply_state`;
/// constructors must provide an initial value via this type
/// (facts/validation.md, "Recovering `emission_box_id` on resume").
///
/// The variants are deliberately not an `Option`. "No block exists yet"
/// (genesis) and "I could not read the block" are different facts with
/// different consequences, and collapsing them into one `None` is the exact
/// overload this type exists to remove.
pub enum EmissionSource<'a> {
    /// Raw BlockTransactions (type-102) section bytes of the block at the
    /// validator's resume height.
    ///
    /// The coinbase spends and recreates the emission box in every block that
    /// has one, so this block's insertions contain the current emission box.
    /// One block to parse — **not** a UTXO-set scan, which would be O(millions
    /// of boxes) on the startup path.
    TipBlock(&'a [u8]),

    /// The boxes a genesis bootstrap just inserted, for a validator starting
    /// at height 0 where no block exists yet. One of them carries the emission
    /// contract.
    GenesisBoxes(&'a [Insertion]),

    /// Neither is obtainable — e.g. UTXO snapshot bootstrap, which loads a
    /// state tree at a height whose block transactions were never downloaded.
    ///
    /// `emission_box_id` stays `None` and `reason` is logged at WARN. Stating
    /// a reason is the price of this variant: an unexplained `None` here is
    /// indistinguishable from the legitimate one, which is how the defect this
    /// type addresses stayed invisible.
    Unavailable(&'a str),
}

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
    ///
    /// `checkpoint_height` is the only thing that changes whether a block's
    /// scripts are evaluated: at or below it they are skipped, above it they
    /// always run. There is no mode to configure and no way for a caller to
    /// end up holding an evaluation this validator did not perform.
    ///
    /// `emission_source` is required, not optional — see [`EmissionSource`].
    /// The returned validator's `emission_box_id()` is valid immediately.
    pub fn new(
        storage: RedbAVLStorage,
        prover: BatchAVLProver,
        height: u32,
        checkpoint_height: u32,
        emission_source: EmissionSource<'_>,
    ) -> Self {
        let digest_bytes = prover.digest().expect("prover has no root");
        let digest = bytes_to_ad_digest(&digest_bytes);

        // Compute emission contract ErgoTree bytes for box matching.
        // Uses mainnet MonetarySettings — the emission contract is the same
        // across mainnet/testnet (same script, different genesis boxes).
        use ergo_lib::chain::emission::MonetarySettings;
        use ergo_lib::chain::ergo_tree_predef;
        use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

        let emission_tree_bytes =
            if let Ok(tree) = ergo_tree_predef::emission_box_prop(&MonetarySettings::default()) {
                tree.sigma_serialize_bytes().unwrap_or_default()
            } else {
                Vec::new()
            };

        let emission_box_id =
            recover_emission_box_id(&emission_source, &emission_tree_bytes, height);

        Self {
            storage,
            prover,
            validated_height: height,
            checkpoint_height,
            current_digest: digest,
            emission_box_id,
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

/// Which of `insertions` carries the emission contract, if any.
///
/// **The single implementation of "which box is the emission box."** Step 9 of
/// `apply_state_internal` and construction-time recovery both call it, and
/// neither may grow its own copy: two implementations of this match is
/// precisely the divergence facts/validation.md exists to prevent, and if they
/// ever disagreed the node would mine against the wrong box. The equivalence
/// is asserted in `recovery_at_the_tip_matches_applying_the_same_block`, not
/// merely asserted here in prose.
///
/// `None` means no insertion carries the contract. Read against a block that
/// was parsed successfully, that means emission has ended — see
/// [`recover_emission_box_id`] for why the caller must not treat every `None`
/// alike.
fn find_emission_box(insertions: &[Insertion], emission_tree_bytes: &[u8]) -> Option<[u8; 32]> {
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

    insertions.iter().find_map(|(box_id, box_bytes)| {
        let ergo_box = tx_validation::deserialize_box(box_bytes).ok()?;
        let tree_bytes = ergo_box.ergo_tree.sigma_serialize_bytes().ok()?;
        (tree_bytes == emission_tree_bytes).then_some(*box_id)
    })
}

/// The insertions a block's transactions produce, by the same route
/// `apply_state_internal` takes to them.
///
/// Routed through `compute_state_changes` rather than reading `tx.outputs`
/// directly, so intra-block netting and ordering are identical to the apply
/// path by construction. A second derivation here could disagree with the
/// first and nothing would notice.
fn insertions_of_block(section_bytes: &[u8]) -> Result<Vec<Insertion>, ValidationError> {
    let parsed = parse_block_transactions(section_bytes)?;
    let summaries = transactions_to_summaries(&parsed.transactions)?;
    Ok(compute_state_changes(summaries)?.insertions)
}

/// Resolve `emission_box_id` for a validator that has applied no block yet.
///
/// Both outcomes are `None`-shaped and only one of them is benign, so they are
/// logged differently:
///
/// - **Read the source, found no emission output** → emission has ended. The
///   documented meaning of `None`; logged at INFO.
/// - **Could not read the source** → unknown. Logged at WARN, because a silent
///   `None` here is byte-identical to the legitimate answer above, and that
///   indistinguishability is how the original defect survived: during sync a
///   block lands within seconds and the field fills itself, so only an at-tip
///   restart ever exposed it.
///
/// It does not fail construction. An unreadable tip block costs mining until
/// the next applied block; it is not a reason to refuse to start a node that
/// can still validate, serve, and sync.
fn recover_emission_box_id(
    source: &EmissionSource<'_>,
    emission_tree_bytes: &[u8],
    height: u32,
) -> Option<[u8; 32]> {
    // No contract bytes means the match can never succeed, so every answer
    // below would be a `None` that says "emission ended" about a scan that
    // never ran. Same trap, one level earlier.
    if emission_tree_bytes.is_empty() {
        tracing::warn!(
            height,
            "emission contract ErgoTree could not be built — emission_box_id \
             cannot be tracked at all and mining will produce no candidates"
        );
        return None;
    }

    let found = match source {
        EmissionSource::TipBlock(section_bytes) => match insertions_of_block(section_bytes) {
            Ok(insertions) => {
                let found = find_emission_box(&insertions, emission_tree_bytes);
                if found.is_none() {
                    tracing::info!(
                        height,
                        "block at resume height created no emission output — \
                         all ERG has been emitted"
                    );
                }
                found
            }
            Err(e) => {
                tracing::warn!(
                    height,
                    error = %e,
                    "emission_box_id recovery failed: the block at the resume \
                     height could not be read; mining is unavailable until the \
                     next block is applied"
                );
                return None;
            }
        },
        EmissionSource::GenesisBoxes(boxes) => {
            let found = find_emission_box(boxes, emission_tree_bytes);
            if found.is_none() {
                // Emission cannot have ended at genesis, so this is a broken
                // bootstrap rather than the legitimate `None`.
                tracing::warn!(
                    height,
                    box_count = boxes.len(),
                    "emission_box_id recovery failed: no genesis box carries the \
                     emission contract"
                );
            }
            found
        }
        EmissionSource::Unavailable(reason) => {
            tracing::warn!(
                height,
                reason,
                "emission_box_id could not be recovered; mining is unavailable \
                 until the next block is applied"
            );
            return None;
        }
    };

    if let Some(id) = found {
        tracing::info!(
            height,
            emission_box_id = %hex::encode(id),
            "recovered emission box at construction"
        );
    }
    found
}

impl BlockValidator for UtxoValidator {
    fn apply_state(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        parent_extension: Option<&[u8]>,
        preceding_headers: &[Header],
        active_params: &Parameters,
        expected_boundary_params: Option<&Parameters>,
        expected_proposed_update: Option<&[u8]>,
    ) -> Result<ApplyStateOutcome, ValidationError> {
        // Every Err leaves the prover byte-for-byte as it was on entry
        // (facts/validation.md, "Err leaves the prover clean"). This wrapper
        // is the only thing that makes that true: apply_state_internal
        // mutates the prover in step 4 and every Err return below it — the
        // digest check, box deserialization, script evaluation, the persist,
        // the proof-digest check — is a bare `return`/`?` that undoes nothing
        // on its own.
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
        // and since script evaluation now runs here, a rejected block is
        // routine rather than near-unreachable, so the tree would carry that
        // block's mutations into the next candidate.
        let saved_digest = self.current_digest;

        match self.apply_state_internal(
            header,
            block_txs,
            ad_proofs,
            extension,
            parent_extension,
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

    /// UTXO mode owns an AVL+ tree over redb, so it hands out its own
    /// storage lifecycle.
    fn state_persistence(&self) -> Option<&dyn StatePersistence> {
        Some(self)
    }
}

impl StatePersistence for UtxoValidator {
    fn flush(&self) -> Result<(), ValidationError> {
        self.storage
            .flush()
            .map_err(|e| ValidationError::StateOperationFailed(format!("flush: {e}")))
    }

    fn resize_cache(&self, cache_bytes: usize) -> Result<(), ValidationError> {
        self.storage.resize_cache(cache_bytes);
        Ok(())
    }

    /// Measured from the live prover — see [`ProverMemoryEstimate`] for what
    /// each figure covers and which way each one is wrong.
    fn prover_memory_estimate(&self) -> Option<ProverMemoryEstimate> {
        crate::prover_memory::estimate(&self.prover)
    }
}

impl MiningState for UtxoValidator {
    fn proofs_for_transactions(
        &self,
        txs: &[ergo_lib::chain::transaction::Transaction],
    ) -> Result<(Vec<u8>, ADDigest), ValidationError> {
        self.compute_proofs(txs)
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
        parent_extension: Option<&[u8]>,
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
        let (epoch_boundary_params, epoch_boundary_proposed_update) = match expected_boundary_params
        {
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

        // 1c. Block rules 306 and 400–406 (JVM `appendFullBlock`), before any
        // state changes. Runs at every height, checkpoint or not — matching
        // the JVM's `execTransactions` skip-only semantics.
        let parent_parsed_ext = parent_extension.map(parse_extension).transpose()?;
        validate_block_rules(
            header,
            preceding_headers.first(),
            &parsed_ext,
            parent_parsed_ext.as_ref(),
            block_txs.len(),
            extension.len(),
            active_params,
        )?;

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
            let result = self.prover.perform_one_operation(op).map_err(|e| {
                ValidationError::StateOperationFailed(format!("operation {i} failed: {e}"))
            })?;

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
        let prover_digest = self.prover.digest().ok_or_else(|| {
            ValidationError::StateOperationFailed("prover has no root after operations".to_string())
        })?;
        if prover_digest.as_ref() != expected_state_root.as_slice() {
            return Err(ValidationError::StateRootMismatch {
                expected: expected_state_root.to_vec(),
                got: prover_digest.to_vec(),
            });
        }

        // 6. Script evaluation. Always here, never the caller's — the block's
        // scripts are checked before anything about it is persisted, so an Ok
        // from this function already means they passed.
        //
        // Evaluation sits between the digest check and persistence, not
        // before the prover mutations. The JVM checks scripts before touching
        // its AVL tree, but it reads each input box via boxById and pays a
        // second traversal to remove it; step 4 above captures the box from
        // the removal's own return value, so copying that ordering would buy
        // a read per input and nothing else. Persistence is the boundary that
        // matters, because persistence is what survives a crash: everything
        // up to `update_with_height` below is in memory, and an Err from here
        // rewinds the prover (see `apply_state`), so no block whose scripts
        // are unverified ever reaches state.redb.
        //
        // The digest check stays ahead of it because it is cheap and rejects
        // malformed blocks before the expensive step.
        let mut block_cost = None;
        if validate_txs {
            let mut proof_boxes = HashMap::with_capacity(proof_box_bytes.len());
            for (id, bytes) in &proof_box_bytes {
                proof_boxes.insert(*id, tx_validation::deserialize_box(bytes)?);
            }

            // A block's transactions are validated with the parameters in
            // effect **after** the block. Everywhere but an epoch boundary
            // those are `active_params`; at a boundary they are the just-
            // parsed `epoch_boundary_params` (facts/validation.md §
            // "Parameters a block's transactions are validated with").
            //
            // Rule 306 keeps `active_params` — bounding a block's own
            // section size by the parameters it introduces would be a
            // liveness footgun and is not what the JVM does.
            let tx_params = epoch_boundary_params
                .as_ref()
                .cloned()
                .unwrap_or_else(|| active_params.clone());

            let eval = ScriptEvalInputs {
                height: header.height,
                transactions: parsed_txs.transactions,
                proof_boxes,
                header: header.clone(),
                preceding_headers: preceding_headers.to_vec(),
                parameters: tx_params,
            };

            block_cost = Some(tx_validation::evaluate_scripts(&eval)?);
        }

        // 7. Persist state changes atomically with block_height.
        //    Must precede generate_proof(): update_internal builds its delete
        //    list from prover.removed_nodes(), and generate_proof() clears the
        //    changed-node buffers that back it.
        self.storage
            .update_with_height(&mut self.prover, vec![], header.height)
            .map_err(|e| ValidationError::StateOperationFailed(format!("persist failed: {e}")))?;

        // 8. Generate the AD proof AFTER persisting — the canonical order,
        //    matching the JVM's
        //    PersistentBatchAVLProver.generateProofAndUpdateStorage
        //    (storage.update, then generateProof).
        //
        //    pack_tree gates on was_modified(), so this order is safe.
        let proof = self.prover.generate_proof();

        // Consensus: the JVM checks that internally-generated proof bytes hash
        // to the header's declared ad_proofs_root, even for proofless blocks.
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
                let section = crate::sections::serialize_ad_proofs(&header.id.0 .0, proof.as_ref());
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

        // 9. Track emission box: scan new outputs for the emission contract.
        //    [`find_emission_box`] is shared with construction-time recovery —
        //    the two paths must not drift, or a resumed node mines against a
        //    different box than an applying one.
        if !self.emission_tree_bytes.is_empty() {
            self.emission_box_id =
                find_emission_box(&changes.insertions, &self.emission_tree_bytes);
        }

        // 10. Advance state
        self.current_digest = header.state_root;
        self.validated_height = header.height;

        tracing::debug!(height = header.height, "state applied (UTXO mode)");

        Ok(ApplyStateOutcome {
            epoch_boundary_params,
            epoch_boundary_proposed_update,
            block_cost,
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
                // `restore_root` handles the entire cleanup: it installs the
                // on-disk root and clears `modified_nodes`. Do not reinstate
                // a manual `modified_nodes` clear.
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
    /// The work is [`proofs_from_storage`]'s; this only supplies the two
    /// storage accessors it takes. The validator was never the dependency —
    /// see that function's documentation.
    fn compute_proofs(
        &self,
        txs: &[ergo_lib::chain::transaction::Transaction],
    ) -> Result<(Vec<u8>, ADDigest), ValidationError> {
        proofs_from_storage(self.storage.resolver(), self.storage.root_state(), txs)
    }
}

/// Compute AD proofs and the resulting state root for `txs` against a
/// committed tree, without a [`UtxoValidator`] and without touching any live
/// prover.
///
/// `resolver` and `root` are exactly what `RedbAVLStorage` and
/// `SnapshotReader` both hand out (facts/state.md). Mining holds the latter,
/// which is the point: the validator is owned by `sync/` and is `!Sync`, so
/// the mining task cannot reach it — but this path never needed it. It
/// builds a **separate** prover from storage because the live prover is
/// `!Sync` and shared-`Rc` mutation would corrupt it. See facts/state.md.
///
/// ⚠ **The resolver must hand out fresh node handles.** `state/` guarantees
/// that structurally — no cache, a fresh read transaction per resolve — and
/// nothing may be inserted between it and the prover here. A prover working
/// over nodes still keyed in another prover's address-keyed map emits a
/// *different proof for identical tree state*: same digest, different bytes
/// (facts/state.md, under `rollback()`).
///
/// ⚠ **Read path.** It reports what proofs *would* be. It applies nothing,
/// persists nothing, and advances no watermark; success is not evidence that
/// a block is valid.
pub fn proofs_from_storage(
    resolver: Resolver,
    root: Option<(Digest32, usize)>,
    txs: &[ergo_lib::chain::transaction::Transaction],
) -> Result<(Vec<u8>, ADDigest), ValidationError> {
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

    // 3. Load the committed root into a fresh tree with its own resolver —
    //    every node it resolves is a new Rc<RefCell<Node>>, independent of
    //    any other prover's tree.
    let (root_label, tree_height) = root.ok_or_else(|| {
        ValidationError::StateOperationFailed(
            "no root state for mining proof computation".to_string(),
        )
    })?;

    // The resolver is the way in: it unpacks the stored bytes exactly as
    // `AVLTree::unpack` does, and `unpack` itself never yields a LabelOnly —
    // so that variant IS the miss. A redb read error arrives here the same
    // way (`state/` logs the cause at ERROR with the digest), which is why one
    // message covers both.
    let root_node = (resolver)(&root_label);
    if matches!(root_node, Node::LabelOnly(_)) {
        return Err(ValidationError::StateOperationFailed(
            "mining proof: root node not found in storage".to_string(),
        ));
    }

    let mut tree = AVLTree::with_resolver(resolver, 32, None);
    tree.root = Some(Rc::new(RefCell::new(root_node)));
    tree.height = tree_height;

    let mut temp_prover = BatchAVLProver::new(tree, false);
    for op in &operations {
        temp_prover.perform_one_operation(op).map_err(|e| {
            ValidationError::StateOperationFailed(format!("mining proof operation failed: {e}"))
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

fn bytes_to_ad_digest(bytes: &Bytes) -> ADDigest {
    let mut arr = [0u8; 33];
    arr.copy_from_slice(bytes);
    ADDigest::from(arr)
}

#[cfg(test)]
mod tests {
    //! UTXO-mode script evaluation, and the Err postcondition that makes a
    //! rejected block survivable: *the prover* is byte-for-byte as it was on
    //! entry (facts/validation.md, "Err leaves the prover clean").
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
    use crate::ErgoBox;
    use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage};
    use ergo_chain_types::Digest32;
    use ergo_lib::chain::transaction::Transaction;
    use ergo_lib::ergotree_ir::ergo_tree::{ErgoTree, ErgoTreeVersion};
    use tempfile::TempDir;
    use tracing_test::traced_test;

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

    fn seeded_validator(boxes: &[ErgoBox]) -> (UtxoValidator, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let prover = seed(&mut storage, boxes);
        (
            UtxoValidator::new(
                storage,
                prover,
                SEED_HEIGHT,
                0,
                EmissionSource::Unavailable("seeded fixture: no tip block"),
            ),
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

        fn apply(
            &self,
            validator: &mut UtxoValidator,
        ) -> Result<ApplyStateOutcome, ValidationError> {
            validator.apply_state(
                &self.header,
                &self.txs,
                None,
                &self.extension,
                None,
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

        let (mut validator, _dir) = seeded_validator(std::slice::from_ref(&input));
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

    /// The script-failure rejection path — the one that turns the case above
    /// from near-unreachable into routine. The block is otherwise perfectly
    /// well-formed; only its script fails.
    #[test]
    fn script_failure_leaves_the_prover_clean_and_storage_untouched() {
        let input = make_box(false, 2);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);

        let (mut validator, _dir) = seeded_validator(std::slice::from_ref(&input));
        let before = prover_digest(&validator);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);

        let err = block
            .apply(&mut validator)
            .expect_err("an unsatisfied script must be rejected");
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
            .expect("empty-operation proof");
        assert_eq!(
            storage_digest, before,
            "an unverified block reached state.redb"
        );
    }

    /// The accept arm. Both rejection tests above prove the validator says no
    /// to something; this proves it still says yes to a block that deserves
    /// it, and that an Ok really does advance both height and digest.
    #[test]
    fn a_valid_block_is_accepted_and_advances_state() {
        let input = make_box(true, 3);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);

        let (mut validator, _dir) = seeded_validator(std::slice::from_ref(&input));
        block.apply(&mut validator).expect("valid block applies");

        assert_eq!(validator.validated_height(), BLOCK_HEIGHT);
        assert_eq!(*validator.current_digest(), state_root);
    }

    /// At or below the checkpoint nothing is evaluated — the state-root check
    /// alone is the guarantee. This block carries the same unsatisfiable
    /// script the rejection test above uses and is accepted anyway, which is
    /// the whole observable: the skip is what separates the two outcomes.
    #[test]
    fn the_checkpoint_skips_evaluation_entirely() {
        let input = make_box(false, 4);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);

        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let prover = seed(&mut storage, std::slice::from_ref(&input));
        // Checkpoint at the block's own height: `height > checkpoint` is
        // false, so the unsatisfiable script is never looked at.
        let mut validator = UtxoValidator::new(
            storage,
            prover,
            SEED_HEIGHT,
            BLOCK_HEIGHT,
            EmissionSource::Unavailable("checkpoint fixture: no tip block"),
        );

        block
            .apply(&mut validator)
            .expect("checkpointed block applies without script evaluation");

        assert_eq!(validator.validated_height(), BLOCK_HEIGHT);
        assert_eq!(*validator.current_digest(), state_root);
    }

    /// The extraction's whole contract: the free function and the method agree
    /// on the proof *bytes*, not merely on the digest. Same-digest-different-
    /// bytes is precisely the hazard this path lives in (facts/state.md, under
    /// `rollback()`), so a digest-only assertion would pass straight through
    /// the failure it exists to catch.
    ///
    /// Not tautological despite the delegation: the free function is reached
    /// here through a `SnapshotReader` — a second handle on the same database,
    /// and the route mining will actually take — while the method goes through
    /// the validator's own `RedbAVLStorage`. Two accessor pairs, one answer.
    #[test]
    fn proofs_from_storage_matches_the_validator_byte_for_byte() {
        let input = make_box(true, 5);
        let tx = spend_tx(std::slice::from_ref(&input));

        let (validator, _dir) = seeded_validator(std::slice::from_ref(&input));
        let before = prover_digest(&validator);

        let (method_proof, method_root) = validator
            .compute_proofs(std::slice::from_ref(&tx))
            .expect("the validator computes proofs");

        let reader = validator.storage.snapshot_reader();
        let (free_proof, free_root) = proofs_from_storage(
            reader.resolver(),
            reader.root_state(),
            std::slice::from_ref(&tx),
        )
        .expect("the free function computes proofs");

        assert_eq!(
            free_proof, method_proof,
            "same tree, same transactions, different proof bytes — the two \
             provers are not seeing independent nodes"
        );
        assert_eq!(free_root, method_root, "the two paths disagree on the root");

        // Not a vacuous fixture: this transaction really does move the tree,
        // so an equality that held for a no-op would not hold here.
        assert_ne!(
            free_root, before,
            "fixture is degenerate — the transaction changes nothing"
        );

        // And neither call disturbed the live prover, which is the reason
        // this path is safe to run from another task at all.
        assert_eq!(
            prover_digest(&validator),
            before,
            "computing mining proofs moved the validator's own prover"
        );
    }

    // ── Tree versions ────────────────────────────────────────────────────
    //
    // A v4 block whose output's R4 holds SANTA X15, `Tuple(1, Upcast(1,
    // Long))`. Its transaction is read at tree version 3, so the transaction
    // id keeps the `Upcast`. Its output enters the tree as `ErgoBox::bytes()`,
    // written at 0, so the value does not (facts/validation.md § Insert
    // Values). Written at 3, the value is two bytes longer and the state root
    // another: the block would not apply and sync would stall on it. The
    // values below were computed from the wire layout, apart from the code
    // under test.

    /// [`make_box`]`(true, 5)`'s id: the box the transaction spends.
    const X15_INPUT_ID: &str = "8166424ca5266319d6ddd27bdef3a3107fccfdd1c2fc2a44608bbd540fa9fb01";
    /// The transaction, written at 3: [`X15_INPUT_ID`] spent with an empty
    /// proof into one 1 ERG `sigmaProp(true)` output at [`BLOCK_HEIGHT`], with
    /// X15 as R4. Its id at 3 is `55c8b854…`.
    const X15_TX: &str = "018166424ca5266319d6ddd27bdef3a3107fccfdd1c2fc2a44608bbd540fa9fb01\
                          00000000018094ebdc030008d3e9070001860204027e040205";
    /// The output as the tree stores it, written at 0: X15's `Upcast` as `1`,
    /// then the transaction's id at 3 and index 0.
    const X15_OUTPUT: &str = "8094ebdc030008d3e9070001860204020402\
                              55c8b854992e5a898dee58b17eefac5874e3339f3fd31567d7b38cee6f67e45200";
    /// The same output written at 3, the `Upcast` kept.
    const X15_OUTPUT_AT_3: &str = "8094ebdc030008d3e9070001860204027e040205\
                                   55c8b854992e5a898dee58b17eefac5874e3339f3fd31567d7b38cee6f67e45200";
    /// blake2b256 of [`X15_OUTPUT`]: the output's box id.
    const X15_OUTPUT_ID: &str = "d7f2c12cf1e29154436ae7fe5ab9dafe6df97ad947f8ece98123b03be8789490";

    fn hex32(s: &str) -> [u8; 32] {
        hex::decode(s)
            .expect("fixture hex")
            .try_into()
            .expect("32 bytes")
    }

    /// The block's operations with its output inserted as `value`, written
    /// by hand rather than through `transactions_to_summaries`.
    fn x15_operations(input: &ErgoBox, value: &str) -> [Operation; 2] {
        [
            Operation::Remove(Bytes::copy_from_slice(&box_key(input))),
            Operation::Insert(KeyValue {
                key: Bytes::copy_from_slice(&hex32(X15_OUTPUT_ID)),
                value: Bytes::from(hex::decode(value).expect("fixture hex")),
            }),
        ]
    }

    #[test]
    fn a_v4_block_inserts_an_upcast_output_as_its_version_0_bytes() {
        let input = make_box(true, 5);
        assert_eq!(hex::encode(box_key(&input)), X15_INPUT_ID);
        let (state_root, ad_root) = oracle(
            std::slice::from_ref(&input),
            &x15_operations(&input, X15_OUTPUT),
        );
        // Not vacuous: the output written at 3 makes another tree.
        let (state_root_at_3, _) = oracle(
            std::slice::from_ref(&input),
            &x15_operations(&input, X15_OUTPUT_AT_3),
        );
        assert_ne!(state_root, state_root_at_3);

        let mut header = make_header(BLOCK_HEIGHT, state_root, ad_root);
        header.version = 4;
        let block = Block {
            header,
            txs: raw_section(4, &[X15_TX]),
            // One dummy non-interlink field so rule 406 is satisfied (a
            // non-genesis block must have at least one field). Same as the
            // shared `sections` helper.
            extension: crate::sections::serialize_extension(
                &HEADER_ID,
                &[([0x02, 0x00], vec![0xff])],
            )
            .expect("extension section serialization"),
            preceding: preceding_headers(),
        };

        let (mut validator, _dir) = seeded_validator(std::slice::from_ref(&input));
        let outcome = block
            .apply(&mut validator)
            .expect("the v4 block applies under the root its output written at 0 gives");
        assert!(outcome.block_cost.is_some(), "its scripts were evaluated");
        assert_eq!(validator.validated_height(), BLOCK_HEIGHT);
        assert_eq!(*validator.current_digest(), state_root);
    }

    /// The other two routes to an insert write the output the same way: the
    /// emission scan's `insertions_of_block` and mining's
    /// `proofs_for_transactions`.
    #[test]
    fn every_other_route_to_an_insert_writes_the_output_at_0() {
        assert_eq!(
            insertions_of_block(&raw_section(4, &[X15_TX])).expect("the v4 section parses"),
            vec![(
                hex32(X15_OUTPUT_ID),
                hex::decode(X15_OUTPUT).expect("fixture hex")
            )]
        );

        let input = make_box(true, 5);
        let (state_root, ad_root) = oracle(
            std::slice::from_ref(&input),
            &x15_operations(&input, X15_OUTPUT),
        );
        let (validator, _dir) = seeded_validator(std::slice::from_ref(&input));
        let (proof, root) = validator
            .proofs_for_transactions(&[tx_read_at(X15_TX, ErgoTreeVersion::V3)])
            .expect("mining proofs for the transaction");
        assert_eq!(root, state_root);
        assert_eq!(blake2b256_hash(&proof), ad_root);
    }

    // ── Prover memory attribution ────────────────────────────────────────
    //
    // These read `validator.prover` through `prover_memory_estimate`, so they
    // belong in here for the same reason the rewind tests do: from outside the
    // crate the in-memory tree is invisible, and a test that cannot see it
    // cannot fail.

    fn estimate(validator: &UtxoValidator) -> ProverMemoryEstimate {
        validator
            .prover_memory_estimate()
            .expect("a prover with a root always has something to measure")
    }

    /// The literal ask: a validator that has applied a block names a tree.
    /// The bytes floor is what separates a measurement from a placeholder —
    /// every counted node is at least one `Rc<RefCell<Node>>` allocation, so a
    /// figure that satisfies this cannot have been a `node_count` with the
    /// bytes stubbed out.
    #[test]
    fn a_validator_that_applied_a_block_reports_a_resident_tree() {
        let input = make_box(true, 6);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);

        let (mut validator, _dir) = seeded_validator(std::slice::from_ref(&input));
        block.apply(&mut validator).expect("valid block applies");

        let measured = estimate(&validator);
        assert!(
            measured.node_count > 0,
            "an applied block left no resident nodes"
        );
        assert!(
            measured.resident_nodes_bytes
                >= measured.node_count * crate::prover_memory::node_allocation_bytes(),
            "{} bytes across {} nodes is below one allocation per node",
            measured.resident_nodes_bytes,
            measured.node_count
        );
    }

    /// The estimate must be a measurement, not a multiplier. Two validators
    /// differing only in how much state they hold must report two different
    /// answers, and both figures must move together. `AVG_HEADER_BYTES`
    /// (`facts/api.md`) passed every test anyone wrote for four months because
    /// nobody made it answer a question whose answer it could get wrong.
    #[test]
    fn the_estimate_follows_the_tree_it_is_measuring() {
        let few: Vec<ErgoBox> = (100u8..102).map(|s| make_box(true, s)).collect();
        let many: Vec<ErgoBox> = (110u8..134).map(|s| make_box(true, s)).collect();

        let (small, _small_dir) = seeded_validator(&few);
        let (large, _large_dir) = seeded_validator(&many);

        let small = estimate(&small);
        let large = estimate(&large);

        assert!(
            large.node_count > small.node_count,
            "twelve times the boxes, {} nodes against {} — the walk is not \
             seeing the tree",
            large.node_count,
            small.node_count
        );
        assert!(
            large.resident_nodes_bytes > small.resident_nodes_bytes,
            "node_count moved and bytes did not: {} against {}",
            large.resident_nodes_bytes,
            small.resident_nodes_bytes
        );
    }

    /// Two claims at once, and the second is the dangerous one.
    ///
    /// `reset_to` reinstalls the root unpacked from storage — one internal
    /// node and two `LabelOnly` children — so a walk that resolved would
    /// report the whole tree here instead of three nodes, having just pulled
    /// the entire UTXO set into RAM in order to measure it. That is the
    /// failure this asserts against.
    ///
    /// What it demonstrates: nothing here mutates state. Reads alone resolve
    /// nodes in place and the resident tree grows permanently, which is the
    /// mechanism behind the unattributed heap being hunted.
    #[test]
    fn resolving_from_storage_grows_the_resident_tree() {
        let boxes: Vec<ErgoBox> = (140u8..164).map(|s| make_box(true, s)).collect();
        let (mut validator, _dir) = seeded_validator(&boxes);

        let seeded = estimate(&validator);
        let digest = prover_digest(&validator);

        validator
            .reset_to(SEED_HEIGHT, digest)
            .expect("rollback to the version storage is already at");
        let frontier = estimate(&validator);
        assert!(
            frontier.node_count < seeded.node_count,
            "the frontier reports {} of {} nodes — the walk is resolving \
             LabelOnly children from storage",
            frontier.node_count,
            seeded.node_count
        );

        // The frontier is a structure small enough to state exactly, which is
        // what pins the payload accounting: `unpack` builds one internal node
        // carrying a 32-byte key and two `LabelOnly` children carrying none.
        // A `node_count × constant` — the `AVG_HEADER_BYTES` shape — cannot
        // land on this by construction.
        assert_eq!(
            frontier.node_count, 3,
            "an unpacked root is one internal node and two labels"
        );
        assert_eq!(
            frontier.resident_nodes_bytes,
            3 * crate::prover_memory::node_allocation_bytes() + KEY_LEN as u64,
            "three allocations plus one key is the whole frontier; this figure \
             is not counting what the nodes actually own"
        );

        for ergo_box in &boxes {
            let key = Bytes::copy_from_slice(&box_key(ergo_box));
            assert!(
                validator.prover.unauthenticated_lookup(&key).is_some(),
                "a seeded box is missing from the tree"
            );
        }

        let resolved = estimate(&validator);
        assert!(
            resolved.node_count > frontier.node_count,
            "resolving every leaf left the node count at {}",
            resolved.node_count
        );
        assert!(
            resolved.resident_nodes_bytes > frontier.resident_nodes_bytes,
            "node_count grew and bytes did not: {} against {}",
            resolved.resident_nodes_bytes,
            frontier.resident_nodes_bytes
        );
    }

    /// `modified_nodes_bytes` must read the live buffers rather than report a
    /// fixed shape.
    ///
    /// ⚠ The contract calls that buffer "cleared on flush"
    /// (`facts/validation.md`). It is not: `StatePersistence::flush` is redb's
    /// fsync and never touches the prover. The clear is the *proof-cycle*
    /// boundary — `generate_proof()` for all three buffers, plus
    /// `update_internal` for the two `Vec`s — and both of those sit inside
    /// `apply_state`. So this drives that seam directly; going through a
    /// completed `apply_state` would sample only after the clear and leave
    /// nothing to observe.
    #[test]
    fn the_cycle_buffers_are_measured_and_released() {
        let boxes: Vec<ErgoBox> = (170u8..178).map(|s| make_box(true, s)).collect();
        let (mut validator, _dir) = seeded_validator(&boxes);

        let idle = estimate(&validator);

        // Mid-cycle: operations performed, no proof generated yet — where an
        // `apply_state` sits between applying its operations and step 8.
        for ergo_box in &boxes {
            validator
                .prover
                .perform_one_operation(&Operation::Lookup(Bytes::copy_from_slice(&box_key(
                    ergo_box,
                ))))
                .expect("lookup of a seeded box");
        }
        let mid_cycle = estimate(&validator);
        assert!(
            mid_cycle.modified_nodes_bytes > idle.modified_nodes_bytes,
            "the touched nodes did not register: {} against an idle {}",
            mid_cycle.modified_nodes_bytes,
            idle.modified_nodes_bytes
        );

        let _ = validator.prover.generate_proof();
        let released = estimate(&validator);
        assert!(
            released.modified_nodes_bytes < mid_cycle.modified_nodes_bytes,
            "the figure did not move across the clear — it is measuring \
             something other than the buffers"
        );
        assert_eq!(
            released.modified_nodes_bytes, idle.modified_nodes_bytes,
            "the cycle ended but the figure did not return to idle"
        );

        // The tree itself is untouched by a lookup, so the *other* figure must
        // not have moved. If it did, the two are not separated and summing
        // them at the endpoint would double-count.
        assert_eq!(
            released.resident_nodes_bytes, idle.resident_nodes_bytes,
            "lookups changed the resident figure"
        );
    }

    // ── Emission box recovery at construction ────────────────────────────
    //
    // `emission_box_id` is derived state that step 9 of `apply_state`
    // maintains. What these cover is the value a validator reports *before*
    // it has applied anything — the state a node restarted at the chain tip
    // is in, where `None` reads as "all ERG has been emitted" and mining
    // therefore never starts.

    fn emission_tree() -> ErgoTree {
        use ergo_lib::chain::emission::MonetarySettings;
        use ergo_lib::chain::ergo_tree_predef;
        ergo_tree_predef::emission_box_prop(&MonetarySettings::default())
            .expect("the emission contract builds")
    }

    /// A block at `BLOCK_HEIGHT` whose single transaction recreates the
    /// emission box, plus the input it spends — the shape of every real
    /// coinbase, which is what makes the tip block a sufficient source.
    fn emission_block(seed: u8) -> (ErgoBox, Transaction, Block) {
        let input = make_box(true, seed);
        let tx = spend_tx_to(std::slice::from_ref(&input), emission_tree());
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);
        let block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);
        (input, tx, block)
    }

    /// The whole point of the change, and the structural assertion that both
    /// paths run one implementation: a validator that has applied nothing must
    /// already name **the same box** applying the block would have named.
    ///
    /// Asserting only `is_some()` would pass on a second, divergent scan that
    /// picked a different output — the failure mode where a resumed node mines
    /// against the wrong box and every block it produces is rejected.
    #[test]
    fn recovery_at_the_tip_matches_applying_the_same_block() {
        let (input, tx, block) = emission_block(7);

        // The applying path.
        let (mut applied, _applied_dir) = seeded_validator(std::slice::from_ref(&input));
        assert_eq!(
            applied.emission_box_id(),
            None,
            "fixture is degenerate — this validator already knew the answer \
             before applying anything"
        );
        block.apply(&mut applied).expect("valid block applies");
        let after_apply = applied
            .emission_box_id()
            .expect("the applied block recreated the emission box");

        // The resume path: a fresh validator over the post-block box set,
        // handed that block's section bytes and nothing else. (`seed` commits
        // at SEED_HEIGHT while the validator resumes at BLOCK_HEIGHT —
        // immaterial, because recovery reads the section bytes, never the
        // tree. Which is exactly why it costs one block to parse instead of a
        // UTXO-set scan.)
        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let prover = seed(&mut storage, tx.outputs.as_vec());
        let resumed = UtxoValidator::new(
            storage,
            prover,
            BLOCK_HEIGHT,
            0,
            EmissionSource::TipBlock(&block.txs),
        );

        assert_eq!(
            resumed.validated_height(),
            BLOCK_HEIGHT,
            "the resumed validator is not where the tip block left it"
        );
        assert_eq!(
            resumed.emission_box_id(),
            Some(after_apply),
            "a validator resumed at the tip names a different emission box \
             than one that applied the same block"
        );
    }

    /// `None` from a source that read cleanly is the documented answer rather
    /// than a failure: the tip block created no emission output, so emission
    /// has ended.
    #[test]
    fn a_tip_block_without_an_emission_output_recovers_none() {
        let input = make_box(true, 8);
        let tx = spend_tx(std::slice::from_ref(&input));
        let (txs, _extension) = sections(std::slice::from_ref(&tx));

        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let prover = seed(&mut storage, std::slice::from_ref(&input));
        let validator = UtxoValidator::new(
            storage,
            prover,
            BLOCK_HEIGHT,
            0,
            EmissionSource::TipBlock(&txs),
        );

        assert_eq!(
            validator.emission_box_id(),
            None,
            "a block whose outputs carry no emission contract reported one"
        );
    }

    /// The case that must never be silent. `None` here is byte-identical to
    /// the legitimate answer above, so without the WARN an operator cannot
    /// tell "emission ended" from "I could not read the block" — which is how
    /// the original defect survived to reach the field.
    #[traced_test]
    #[test]
    fn an_unreadable_tip_block_recovers_none_and_warns() {
        // A well-formed header id followed by a claim of five transactions and
        // no transaction bytes: parses far enough to be a real section and
        // then fails, rather than being rejected on length alone.
        let mut malformed = vec![0u8; 32];
        malformed.push(5);

        let input = make_box(true, 9);
        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let prover = seed(&mut storage, std::slice::from_ref(&input));
        let validator = UtxoValidator::new(
            storage,
            prover,
            BLOCK_HEIGHT,
            0,
            EmissionSource::TipBlock(&malformed),
        );

        assert_eq!(validator.emission_box_id(), None);
        assert!(
            logs_contain("emission_box_id recovery failed"),
            "an unreadable tip block left `None` silently"
        );
    }

    /// The caller that cannot supply a source at all — UTXO snapshot
    /// bootstrap, which loads a tree at a height whose block transactions were
    /// never downloaded. It must say why, and the why must reach the log.
    #[traced_test]
    #[test]
    fn an_unavailable_source_recovers_none_and_names_its_reason() {
        let input = make_box(true, 10);
        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let prover = seed(&mut storage, std::slice::from_ref(&input));
        let validator = UtxoValidator::new(
            storage,
            prover,
            BLOCK_HEIGHT,
            0,
            EmissionSource::Unavailable("snapshot bootstrap: no block at the snapshot height"),
        );

        assert_eq!(validator.emission_box_id(), None);
        assert!(
            logs_contain("snapshot bootstrap: no block at the snapshot height"),
            "the caller's reason never reached the log"
        );
    }

    /// The genesis arm. At height 0 there is no tip block, and the emission box
    /// is one of the three boxes the bootstrap just inserted. Left uncovered, a
    /// node mining a fresh chain would deadlock at height 1 for exactly the
    /// reason an at-tip restart did.
    ///
    /// Built from `ergo-lib`'s own `genesis_boxes`, which is what `main` calls,
    /// so this also pins the assumption underneath the whole file: that the
    /// genesis emission box carries the same `emission_box_prop` tree
    /// `UtxoValidator::new` computes.
    #[test]
    fn genesis_boxes_recover_the_emission_box() {
        use ergo_lib::chain::emission::MonetarySettings;
        use ergo_lib::chain::genesis::genesis_boxes;
        use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;

        let founders = vec![ProveDlog::new(ergo_chain_types::EcPoint::default())];
        let (emission, no_premine, founders_box) =
            genesis_boxes(&MonetarySettings::default(), &founders, 1, &[])
                .expect("genesis boxes construct");

        let boxes: Vec<([u8; 32], Vec<u8>)> = [&emission, &no_premine, &founders_box]
            .into_iter()
            .map(|b| (box_key(b), serialized_box(b)))
            .collect();

        let input = make_box(true, 11);
        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = open_storage(&dir);
        let prover = seed(&mut storage, std::slice::from_ref(&input));
        let validator =
            UtxoValidator::new(storage, prover, 0, 0, EmissionSource::GenesisBoxes(&boxes));

        assert_eq!(
            validator.emission_box_id(),
            Some(box_key(&emission)),
            "the genesis emission box was not recognised — `emission_box_prop` \
             and the genesis bootstrap have diverged"
        );
    }

    // ── Item #25: transactions validated with the block's own boundary
    //             parameters, not the pre-block ones ─────────────────────────
    //
    // At an epoch-boundary block, the JVM validates that block's transactions
    // with the *newly computed* parameters the block carries in its extension
    // — the block's `newStateContext`, in JVM terms
    // (`ErgoStateContext.scala:138-139`). A parameter that changed at the
    // boundary decides the block's own validity by its new value, not its
    // old one. `MaxBlockCost` is the observable path here: it gates
    // acceptance and it can grow at a boundary.

    /// The load-bearing test: an old `maxBlockCost` too low for the block, a
    /// new one high enough. If the transactions were validated with
    /// `active_params`, the block is rejected as over-budget; if they are
    /// validated with the block's own boundary parameters, it passes.
    ///
    /// Two facts are pinned. First, the same block against `active_params`
    /// (no boundary) is rejected — a positive control that the cost really
    /// exceeds the low limit. Second, with the boundary set attached, the
    /// same block is accepted, and its reported cost is the same regardless
    /// of which set is passed — so we know the accept arm did not merely
    /// weaken the fold.
    #[test]
    fn epoch_boundary_block_validates_txs_with_boundary_max_block_cost() {
        use ergo_lib::chain::parameters::Parameter;

        // A block with one transaction — a real script eval, real cost.
        let input = make_box(true, 200);
        let tx = spend_tx(std::slice::from_ref(&input));
        let ops = block_operations(std::slice::from_ref(&tx));
        let (state_root, ad_root) = oracle(std::slice::from_ref(&input), &ops);

        // Measure the block's real cost via a permissive-active run — the
        // cost the validator would surface at accept. That way the "old
        // maxBlockCost" is chosen relative to a measurement, not a guess.
        let (mut probe_validator, _probe_dir) = seeded_validator(std::slice::from_ref(&input));
        let permissive_block = Block::new(std::slice::from_ref(&tx), state_root, ad_root);
        let probe_outcome = permissive_block
            .apply(&mut probe_validator)
            .expect("probe block applies with default parameters");
        let block_cost = probe_outcome
            .block_cost
            .expect("script evaluation reports a cost above checkpoint");
        assert!(block_cost > 0, "the probe transaction has zero cost");

        // Build the "old" and "new" parameters. Old cost cap is below the
        // block's cost; new cap is above it. Everything else is identical so
        // the `matchParameters60` subset check sees a legal set.
        let mut boundary_params = Parameters::default();
        boundary_params
            .parameters_table
            .insert(Parameter::MaxBlockCost, (block_cost + 1_000) as i32);

        let mut active_params = boundary_params.clone();
        active_params
            .parameters_table
            .insert(Parameter::MaxBlockCost, (block_cost - 1) as i32);

        // Pack the boundary parameters into an extension exactly as
        // `check_parameters_v6` expects (Parameters.toExtensionCandidate
        // parity). At the boundary, the block's extension carries them.
        let mut fields = crate::voting::pack_parameters(&boundary_params);
        // `pack_parameters` orders by param id. The block rules (405) run on
        // top of this; they need no interlink fields at height BLOCK_HEIGHT
        // — this test uses `parent_extension = None` (JVM
        // exIlUnableToValidate) so 401/402 are skipped.
        // The extension does need a field for rule 406, which pack_parameters
        // already satisfies (Parameters::default() has entries).
        assert!(
            !fields.is_empty(),
            "packed boundary parameters must be non-empty for rule 406"
        );
        let extension_bytes = crate::sections::serialize_extension(&HEADER_ID, &fields)
            .expect("extension serialization");

        let block_txs_bytes =
            crate::sections::serialize_block_transactions(&HEADER_ID, 3, std::slice::from_ref(&tx))
                .expect("tx section serialization");

        // Positive control: same block, active_params only (no boundary set,
        // as at any mid-epoch height) — rejected as over-budget. This proves
        // the cost really exceeds the low limit, so the boundary accept
        // below is a genuine flip, not a vacuous pass.
        //
        // ergo-lib enforces the block cost per input, so the reject arm may
        // surface as `TransactionInvalid` (per-input CostLimitExceeded) or
        // `BlockCostExceeded` (post-fold check). Both are the same fact —
        // the tx does not fit under active_params.
        let (mut old_only_validator, _old_only_dir) =
            seeded_validator(std::slice::from_ref(&input));
        let old_header = make_header(BLOCK_HEIGHT, state_root, ad_root);
        let err = old_only_validator
            .apply_state(
                &old_header,
                &block_txs_bytes,
                None,
                &extension_bytes,
                None,
                &preceding_headers(),
                &active_params,
                None,
                None,
            )
            .expect_err("with only the low active_params, the block must be over-budget");
        assert!(
            matches!(
                err,
                ValidationError::BlockCostExceeded { .. }
                    | ValidationError::TransactionInvalid { .. }
            ),
            "expected an over-cost rejection, got: {err:?}"
        );

        // The accept arm: active_params still says the block is over-budget,
        // but the boundary parameters override at a boundary block, and the
        // block passes.
        let (mut boundary_validator, _boundary_dir) =
            seeded_validator(std::slice::from_ref(&input));
        // The boundary path also runs `check_block_version`; its
        // `Parameters::default()` inserts BlockVersion = 1, and `make_header`
        // sets version = 3, so we need the extension to declare the header's
        // version too. `Parameters::default()` starts with BlockVersion = 1;
        // update the boundary set to match the header, then re-pack.
        let mut boundary_params = boundary_params;
        boundary_params
            .parameters_table
            .insert(Parameter::BlockVersion, i32::from(old_header.version));
        fields = crate::voting::pack_parameters(&boundary_params);
        let extension_bytes = crate::sections::serialize_extension(&HEADER_ID, &fields)
            .expect("extension serialization");

        let outcome = boundary_validator
            .apply_state(
                &old_header,
                &block_txs_bytes,
                None,
                &extension_bytes,
                None,
                &preceding_headers(),
                &active_params,
                Some(&boundary_params),
                None,
            )
            .expect("with boundary params supplying a higher maxBlockCost, the block is accepted");
        assert_eq!(
            outcome.block_cost,
            Some(block_cost),
            "the cost surfaced under boundary params must match the probe"
        );
        assert_eq!(
            boundary_validator.validated_height(),
            BLOCK_HEIGHT,
            "an accepted boundary block advances state"
        );
    }
}

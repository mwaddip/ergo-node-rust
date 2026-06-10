//! DIAGNOSTIC — not a test of validation logic. Isolates a single question:
//! does `BatchAVLProver::generate_proof()` produce a verifier-replayable
//! ADProof when it is called *after* a storage-style `tree.reset()`?
//!
//! This mirrors the exact ordering in `UtxoValidator::apply_state_internal`
//! (validation/src/utxo.rs:316-321):
//!   1. perform this block's operations on the prover
//!   2. `storage.update_with_height(...)`  ── which, in our
//!      `RedbAVLStorage::update_internal` (state/src/storage.rs:939-941),
//!      calls `prover.base.tree.reset()` + clears the changed-node buffers
//!   3. `let _ = self.prover.generate_proof();`  ── the proof the
//!      adproof-dump task (prompts/validation-adproof-dump.md) wants to keep
//!
//! The canonical avltree flow (PersistentBatchAVLProver::
//! generate_proof_and_update_storage) also does update-then-generate_proof,
//! but its in-memory storage.update() does NOT reset the prover — our redb
//! storage added that reset because "in UTXO mode we never call generate_proof
//! on the main prover" (a comment that contradicts utxo.rs:321).
//!
//! `generate_proof`'s `pack_tree` emits a full node only when `visited(node)`
//! is true, otherwise just a label stub. `tree.reset()` clears every `visited`
//! flag. So the open question is whether the proof at step 3 still covers the
//! block's touched paths. We answer it empirically instead of reasoning about
//! copy-on-write node reachability.

use bytes::Bytes;
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_avl_verifier::BatchAVLVerifier;
use ergo_avltree_rust::batch_node::{AVLTree, Node, NodeHeader};
use ergo_avltree_rust::operation::{Digest32, KeyValue, Operation};

/// Label-preserving resolver (matches facts/validation.md guidance). Never
/// actually invoked for a freshly built in-memory tree, but the verifier may
/// resolve LabelOnly stubs while replaying.
fn resolver(digest: &Digest32) -> Node {
    Node::LabelOnly(NodeHeader::new(Some(*digest), None))
}

fn key(i: u8) -> Bytes {
    Bytes::from(vec![i; 32])
}
fn val(i: u8) -> Bytes {
    Bytes::from(vec![i; 8])
}

fn make_prover() -> BatchAVLProver {
    // collect_changed_nodes = true mirrors the node's prover (storage's
    // collect_changed_nodes path depends on it).
    BatchAVLProver::new(AVLTree::new(resolver, 32, None), true)
}

fn block_ops() -> Vec<Operation> {
    // 20 inserts → a multi-level tree with internal nodes and rebalancing,
    // so the visited-vs-reset distinction actually matters.
    (1..=20u8)
        .map(|i| {
            Operation::Insert(KeyValue {
                key: key(i),
                value: val(i),
            })
        })
        .collect()
}

/// Replay `ops` through a fresh verifier seeded at `start_digest` with `proof`,
/// and report whether it reconstructs `expected_post`.
fn verify(
    start_digest: &Bytes,
    proof: &Bytes,
    ops: &[Operation],
    expected_post: &Bytes,
) -> Result<bool, String> {
    let tree = AVLTree::new(resolver, 32, None);
    let mut v = BatchAVLVerifier::new(start_digest, proof, tree, Some(ops.len() + 1), None)
        .map_err(|e| format!("verifier construction rejected the proof: {e}"))?;
    for (i, op) in ops.iter().enumerate() {
        v.perform_one_operation(op)
            .map_err(|e| format!("verifier replay failed at op {i}: {e}"))?;
    }
    let got = v.digest().ok_or_else(|| "verifier produced no digest".to_string())?;
    Ok(got == *expected_post)
}

/// CONTROL: canonical ordering (generate_proof immediately after the ops, no
/// storage reset in between). Must verify — proves the test harness is sound.
#[test]
fn control_canonical_ordering_verifies() {
    let mut prover = make_prover();
    let start = prover.digest().unwrap();
    let ops = block_ops();
    for op in &ops {
        prover.perform_one_operation(op).unwrap();
    }
    let post = prover.digest().unwrap();

    let proof = prover.generate_proof();
    let ok = verify(&start, &proof, &ops, &post)
        .expect("control: canonical proof should construct + replay");
    assert!(
        ok,
        "control: canonical proof must reconstruct the post-block digest"
    );
    println!("CONTROL ok: proof_len={} verified=true", proof.len());
}

/// REPRO: the node's actual ordering — storage-style `tree.reset()` + buffer
/// clears (state/src/storage.rs:939-941) BEFORE `generate_proof()`
/// (utxo.rs:321). This is the proof the adproof-dump task would persist.
#[test]
fn repro_reset_before_generate_proof() {
    let mut prover = make_prover();
    let start = prover.digest().unwrap();
    let ops = block_ops();
    for op in &ops {
        prover.perform_one_operation(op).unwrap();
    }
    let post = prover.digest().unwrap();

    // ── mimic RedbAVLStorage::update_internal (state/src/storage.rs:939-941) ──
    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();
    // ── then UtxoValidator generates (and currently discards) the proof ──
    let proof = prover.generate_proof();

    let result = verify(&start, &proof, &ops, &post);
    println!(
        "REPRO: proof_len={} result={:?}",
        proof.len(),
        result
    );
    match &result {
        Ok(true) => println!(
            "REPRO VERDICT: proof VERIFIED — apply-time proof is valid; dump task premise HOLDS"
        ),
        Ok(false) => println!(
            "REPRO VERDICT: proof replayed but digest MISMATCH — proof is INVALID for dumping"
        ),
        Err(e) => println!("REPRO VERDICT: proof REJECTED ({e}) — proof is INVALID for dumping"),
    }

    // Document the finding as an executable assertion: the dump task is only
    // sound if this proof verifies. If it does not, the test fails loudly and
    // the verdict above explains why.
    assert!(
        result.unwrap_or(false),
        "apply-time proof (post storage-reset) does NOT verify — see REPRO VERDICT above"
    );
}

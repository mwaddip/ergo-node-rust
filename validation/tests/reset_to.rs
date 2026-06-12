//! `reset_to` contract tests (facts/validation.md, postconditions dated
//! 2026-06-12):
//!   Ok  → `validated_height()` == height, `current_digest()` == digest,
//!         prover rolled to the target tree.
//!   Err → the underlying rollback FAILED and observable state is
//!         UNCHANGED — no cache advance onto un-rolled state.
//!   DigestValidator: plain field assignment, always Ok.

use bytes::Bytes;
use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage};
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::AVLTree;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_chain_types::ADDigest;
use ergo_validation::{BlockValidator, DigestValidator, UtxoValidator, ValidationError};
use tempfile::TempDir;

const KEY_LEN: usize = 32;

fn make_key(seed: u8) -> Bytes {
    // Strictly between negative_infinity [0;32] and positive_infinity [0xFF;32].
    let mut key = vec![0u8; KEY_LEN];
    key[0] = 0x01;
    key[1] = seed;
    Bytes::from(key)
}

fn prover_ad_digest(prover: &BatchAVLProver) -> ADDigest {
    let bytes = prover.digest().expect("prover has a root");
    let mut arr = [0u8; 33];
    arr.copy_from_slice(&bytes);
    ADDigest::from(arr)
}

/// Storage with two committed versions (heights 1 and 2); the validator
/// sits at height 2 with both digests rollback-reachable.
fn utxo_validator_with_history() -> (UtxoValidator, ADDigest, ADDigest, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let mut storage = RedbAVLStorage::open(
        &path,
        AVLTreeParams {
            key_length: KEY_LEN,
            value_length: None,
        },
        10,
        CacheSize::default(),
    )
    .unwrap();

    let tree = AVLTree::with_resolver(storage.resolver(), KEY_LEN, None);
    let mut prover = BatchAVLProver::new(tree, true);

    // Height 1. update_with_height resets the prover's dirty-node
    // bookkeeping after commit, so no manual reset between batches.
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(1),
            value: Bytes::from(vec![1u8; 16]),
        }))
        .unwrap();
    storage.update_with_height(&mut prover, vec![], 1).unwrap();
    let digest_h1 = prover_ad_digest(&prover);

    // Height 2.
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(2),
            value: Bytes::from(vec![2u8; 16]),
        }))
        .unwrap();
    storage.update_with_height(&mut prover, vec![], 2).unwrap();
    let digest_h2 = prover_ad_digest(&prover);

    let validator = UtxoValidator::new(storage, prover, 2, 0);
    (validator, digest_h1, digest_h2, dir)
}

/// The prover's actual root digest, observed through the public API
/// (an empty-ops proof generation reports the current tree digest).
fn observed_prover_digest(validator: &UtxoValidator) -> ADDigest {
    let (_, digest) = validator
        .proofs_for_transactions(&[])
        .expect("UTXO mode computes proofs")
        .expect("empty-ops proof generation succeeds");
    digest
}

#[test]
fn utxo_reset_to_ok_postconditions() {
    let (mut validator, digest_h1, digest_h2, _dir) = utxo_validator_with_history();
    assert_eq!(validator.validated_height(), 2);
    assert_eq!(validator.current_digest(), &digest_h2);

    validator
        .reset_to(1, digest_h1)
        .expect("rollback to a committed version succeeds");

    assert_eq!(validator.validated_height(), 1);
    assert_eq!(validator.current_digest(), &digest_h1);
    // The prover really moved to the height-1 tree, not just the cache.
    assert_eq!(observed_prover_digest(&validator), digest_h1);
}

#[test]
fn utxo_reset_to_err_leaves_state_unchanged() {
    let (mut validator, _digest_h1, digest_h2, _dir) = utxo_validator_with_history();

    // A digest that was never committed — not in storage's version chain.
    let bogus = ADDigest::from([0x42u8; 33]);
    let err = validator
        .reset_to(1, bogus)
        .expect_err("rollback to an unknown version must fail");
    assert!(
        matches!(err, ValidationError::StateOperationFailed(_)),
        "unexpected error variant: {err:?}"
    );

    // Postconditions on Err: observable state exactly as before the call.
    assert_eq!(validator.validated_height(), 2);
    assert_eq!(validator.current_digest(), &digest_h2);
    assert_eq!(observed_prover_digest(&validator), digest_h2);
}

#[test]
fn digest_reset_to_always_ok() {
    let genesis = ADDigest::from([0x01u8; 33]);
    let mut validator = DigestValidator::from_state(genesis, 10, 0);

    let target = ADDigest::from([0x07u8; 33]);
    validator
        .reset_to(3, target)
        .expect("digest-mode reset is infallible");

    assert_eq!(validator.validated_height(), 3);
    assert_eq!(validator.current_digest(), &target);
}

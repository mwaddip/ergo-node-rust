//! Tests for the version-dependent `transactions_root` Merkle computation.
//!
//! JVM `BlockTransactions.scala:59-63`: block version 1 commits to the tx
//! IDs only; version >= 2 commits to `txIds ++ witnessIds`, where a witness
//! ID is `blake2b256(concat(inputs[*].spendingProof.proof)).tail` — 31
//! bytes (`ErgoTransaction.scala:77-78`).
//!
//! The v2+ path is pinned against a REAL testnet block: height 2666,
//! header version 4, three transactions with a mix of empty and non-empty
//! spending proofs (fixture extracted from the SANTA vector suite). The
//! tx-IDs-only root the code used to produce for v2+ networks is asserted
//! to differ — that was the consensus bug: every candidate assembled on a
//! v2+ network carried a transactionsRoot no JVM peer accepts.

use blake2::Digest as Blake2Digest;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_merkle_tree::{MerkleNode, MerkleTree};
use ergo_mining::candidate::transactions_root;
use ergo_mining::emission::{build_emission_tx, ReemissionRules};

type Blake2b256 = blake2::Blake2b<blake2::digest::typenum::U32>;

const FIXTURE: &str = include_str!("fixtures/block-2666-txs.json");

/// Parse the fixture into (block_version, expected_root_hex, transactions).
fn load_fixture() -> (u8, String, Vec<Transaction>) {
    let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let block_version = v["blockVersion"].as_u64().unwrap() as u8;
    let expected_root = v["transactionsRoot"].as_str().unwrap().to_string();
    let txs: Vec<Transaction> = v["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tx| serde_json::from_value(tx.clone()).unwrap())
        .collect();
    (block_version, expected_root, txs)
}

/// The v2+ rule against a real testnet block: the three transactions of
/// block 2666 (header version 4) must reproduce the header's
/// transactionsRoot exactly. The block mixes fully-empty proofs (txs 0 and
/// 2) with a tx carrying one empty and one non-empty proof, so the witness
/// concatenation rule is exercised in all combinations that occur on chain.
#[test]
fn v2_root_matches_real_testnet_block_2666() {
    let (block_version, expected_root, txs) = load_fixture();
    assert_eq!(block_version, 4);
    assert_eq!(txs.len(), 3);

    // Guard: the JSON round-trip must reconstruct the exact on-chain txs.
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    for (tx, json_tx) in txs.iter().zip(fixture["transactions"].as_array().unwrap()) {
        assert_eq!(
            hex::encode(tx.id().as_ref()),
            json_tx["id"].as_str().unwrap(),
            "parsed tx id diverged from the on-chain id — fixture decode is broken"
        );
    }

    let root = transactions_root(&txs, block_version).unwrap();
    assert_eq!(
        hex::encode(root.0),
        expected_root,
        "v2+ transactionsRoot must match the real block 2666 header"
    );

    // The pre-fix rule (tx IDs only) must NOT reproduce the on-chain root.
    let v1_style_root = transactions_root(&txs, 1).unwrap();
    assert_ne!(
        hex::encode(v1_style_root.0),
        expected_root,
        "tx-IDs-only root unexpectedly matches — the witness leaves would be \
         dead code and the original bug report wrong"
    );
}

/// Version 1 commits to the tx IDs only — pinned against a Merkle tree
/// built from explicit tx-ID leaves right here in the test.
#[test]
fn v1_root_is_tx_ids_only() {
    let (_, _, txs) = load_fixture();

    let expected = MerkleTree::new(
        txs.iter()
            .map(|tx| MerkleNode::from_bytes(tx.id().as_ref().to_vec()))
            .collect::<Vec<_>>(),
    )
    .root_hash();

    let root = transactions_root(&txs, 1).unwrap();
    assert_eq!(root, expected);
}

/// An input with an empty spending proof (emission/storage-rent style)
/// contributes zero bytes to the witness concatenation: the witness leaf of
/// a tx whose only input has an empty proof is `blake2b256(b"")[1..]`.
#[test]
fn empty_proof_input_contributes_nothing() {
    // The emission tx's input spends the emission box with ProofBytes::Empty.
    let emission_box = mainnet_emission_box();
    let miner_pk = test_miner_pk();
    let tx = build_emission_tx(&emission_box, 1, &miner_pk, 720, &ReemissionRules::mainnet())
        .unwrap();
    assert!(
        tx.inputs.iter().all(|i| i.spending_proof.proof.as_ref().is_empty()),
        "test premise: emission tx inputs carry empty proofs"
    );

    let empty_witness: Vec<u8> = {
        let hash: [u8; 32] = Blake2b256::new().finalize().into();
        hash[1..].to_vec()
    };
    let expected = MerkleTree::new(vec![
        MerkleNode::from_bytes(tx.id().as_ref().to_vec()),
        MerkleNode::from_bytes(empty_witness),
    ])
    .root_hash();

    let root = transactions_root(&[tx], 2).unwrap();
    assert_eq!(root, expected);
}

#[test]
fn empty_tx_list_is_an_error() {
    assert!(transactions_root(&[], 1).is_err());
    assert!(transactions_root(&[], 2).is_err());
}

fn test_miner_pk() -> ProveDlog {
    let bytes =
        hex::decode("039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647")
            .unwrap();
    let point =
        ergo_chain_types::EcPoint::sigma_parse_bytes(&bytes).unwrap();
    ProveDlog::new(point)
}

fn mainnet_emission_box() -> ErgoBox {
    use ergo_lib::chain::emission::MonetarySettings;
    use ergo_lib::chain::genesis;

    let founder_pks: Vec<ProveDlog> = [
        "039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647",
        "031fb52cf6e805f80d97cde289f4f757d49accf0c83fb864b27d2cf982c37f9a8b",
        "0352ac2a471339b0d23b3d2c5ce0db0e81c969f77891b9edf0bda7fd39a78184e7",
    ]
    .iter()
    .map(|hex_str| {
        let bytes = hex::decode(hex_str).unwrap();
        let point = ergo_chain_types::EcPoint::sigma_parse_bytes(&bytes).unwrap();
        ProveDlog::new(point)
    })
    .collect();

    let (emission_box, _, _) = genesis::genesis_boxes(
        &MonetarySettings::default(),
        &founder_pks,
        2,
        &["p1", "p2", "p3", "p4", "p5"],
    )
    .unwrap();
    emission_box
}

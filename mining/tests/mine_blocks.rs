//! Integration test: bootstrap genesis, generate candidates, mine blocks.
//!
//! Uses trivial difficulty (initial_n_bits = 16842752, decodes to 1) so that
//! nearly any nonce produces a valid PoW solution. This tests the full mining
//! pipeline without needing real hashpower.

use std::time::Duration;

use ergo_chain_types::{
    autolykos_pow_scheme::{decode_compact_bits, AutolykosPowScheme},
    ADDigest, AutolykosSolution, BlockId, Digest, Digest32, EcPoint, Header, Votes,
};
use ergo_lib::chain::emission::{EmissionRules, MonetarySettings};
use ergo_lib::chain::genesis;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_mining::candidate::build_work_message;
use ergo_mining::emission::{build_emission_tx, ReemissionRules};
use ergo_mining::extension::{build_extension, extension_digest};
use ergo_mining::solution::validate_solution;
use ergo_mining::types::*;
use ergo_mining::MiningError;
use num_bigint::BigInt;

/// Initial difficulty for testnet/mainnet — decodes to 1.
/// Target = order / 1 ≈ 2^256, so any nonce is valid.
const INITIAL_N_BITS: u32 = 16842752;

/// Testnet no-premine proof strings.
const PROOFS: &[&str] = &[
    "test-proof-1",
    "test-proof-2",
    "test-proof-3",
    "test-proof-4",
    "test-proof-5",
];

/// Testnet founders PKs (from main.rs).
const FOUNDER_PKS: &[&str] = &[
    "039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647",
    "031fb52cf6e805f80d97cde289f4f757d49accf0c83fb864b27d2cf982c37f9a8b",
    "0352ac2a471339b0d23b3d2c5ce0db0e81c969f77891b9edf0bda7fd39a78184e7",
];

/// Build founder ProveDlog keys from hex strings.
fn founder_pks() -> Vec<ProveDlog> {
    FOUNDER_PKS
        .iter()
        .map(|hex_str| {
            let bytes = hex::decode(hex_str).unwrap();
            let point = EcPoint::sigma_parse_bytes(&bytes).unwrap();
            ProveDlog::new(point)
        })
        .collect()
}

/// Build a miner PK for testing (reuses first founder key).
fn test_miner_pk() -> ProveDlog {
    let bytes = hex::decode(FOUNDER_PKS[0]).unwrap();
    let point = EcPoint::sigma_parse_bytes(&bytes).unwrap();
    ProveDlog::new(point)
}

/// Construct a genesis header (height 1) with placeholder fields.
///
/// In Ergo, genesis is at height 1 (not 0). This is the "parent" for
/// the first mined block (height 2). Its PoW doesn't need to be valid —
/// it just needs the right structural fields so that the next block can
/// reference it.
fn genesis_header() -> Header {
    Header {
        version: 2,
        id: BlockId(Digest::from([1u8; 32])),
        parent_id: BlockId(Digest::from([0u8; 32])),
        ad_proofs_root: Digest32::from([0u8; 32]),
        state_root: ADDigest::from([0u8; 33]),
        transaction_root: Digest32::from([0u8; 32]),
        timestamp: 1000,
        n_bits: INITIAL_N_BITS,
        height: 1, // genesis is height 1 in Ergo
        extension_root: Digest32::from([0u8; 32]),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new((*test_miner_pk().h).clone()),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes([0, 0, 0]),
        unparsed_bytes: Box::new([]),
    }
}

/// CPU mine: find a nonce that makes pow_hit < target.
///
/// With difficulty 1 this should succeed on the first attempt.
fn cpu_mine(candidate: &CandidateBlock, max_attempts: u64) -> Result<Header, String> {
    let pow = AutolykosPowScheme::default();
    let target_bigint = order_bigint() / decode_compact_bits(candidate.n_bits);

    for nonce in 0u64..max_attempts {
        let solution = AutolykosSolution {
            miner_pk: Box::new((*test_miner_pk().h).clone()),
            pow_onetime_pk: None,
            nonce: nonce.to_be_bytes().to_vec(),
            pow_distance: None,
        };

        match validate_solution(candidate, solution) {
            Ok(header) => return Ok(header),
            Err(MiningError::InvalidSolution(_)) => continue,
            Err(e) => return Err(format!("unexpected error: {e}")),
        }
    }

    Err(format!("no solution found in {max_attempts} attempts"))
}

/// Get the secp256k1 group order as BigInt.
fn order_bigint() -> BigInt {
    // secp256k1 group order
    let order_hex = "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141";
    BigInt::parse_bytes(order_hex.as_bytes(), 16).unwrap()
}

#[test]
fn build_emission_tx_produces_valid_structure() {
    let settings = MonetarySettings::default();
    let pks = founder_pks();
    let (emission_box, _, _) =
        genesis::genesis_boxes(&settings, &pks, 2, PROOFS).unwrap();

    let miner_pk = test_miner_pk();
    let reemission_rules = ReemissionRules::mainnet();

    let tx = build_emission_tx(&emission_box, 2, &miner_pk, 720, &reemission_rules).unwrap();

    // Emission tx should have 1 input (the emission box)
    assert_eq!(tx.inputs.len(), 1);
    // And 2 outputs (new emission box + miner reward box)
    assert_eq!(tx.outputs.len(), 2);

    // Output 0: new emission box — value reduced by miner reward
    let emission_rules = EmissionRules::new(settings);
    let reward = emission_rules.miners_reward_at_height(2);
    assert!(reward > 0, "miner reward at height 1 should be positive");

    let new_emission_value = tx.outputs.get(0).unwrap().value.as_i64();
    let original_value = emission_box.value.as_i64();
    assert_eq!(new_emission_value, original_value - reward);

    // Output 1: miner reward box — value equals miner reward
    let reward_value = tx.outputs.get(1).unwrap().value.as_i64();
    assert_eq!(reward_value, reward);
}

#[test]
fn generate_candidate_and_mine_block() {
    let settings = MonetarySettings::default();
    let pks = founder_pks();
    let (emission_box, _, _) =
        genesis::genesis_boxes(&settings, &pks, 2, PROOFS).unwrap();

    let miner_pk = test_miner_pk();
    let config = MinerConfig {
        miner_pk: miner_pk.clone(),
        reward_delay: 720,
        votes: [0, 0, 0],
        candidate_ttl: Duration::from_secs(15),
    };

    let parent = genesis_header();

    // Mock validator proofs — returns fake proof bytes and state root.
    // Real state root computation requires a full UTXO validator.
    // This test verifies the mining pipeline: candidate → header → PoW.
    let mock_proofs =
        |_txs: &[ergo_lib::chain::transaction::Transaction]|
         -> Option<Result<(Vec<u8>, ADDigest), ergo_validation::ValidationError>> {
            Some(Ok((vec![0u8; 64], ADDigest::from([0u8; 33]))))
        };

    // Generate candidate for block 1
    let (candidate, work) = ergo_mining::generate_candidate(
        &config,
        &parent,
        INITIAL_N_BITS,
        &[], // empty interlinks for genesis
        &emission_box,
        &mock_proofs,
    )
    .expect("candidate generation failed");

    // Verify WorkMessage fields
    assert_eq!(work.h, 2, "block height should be 2 (genesis is 1)");
    assert!(!work.msg.is_empty(), "msg should be non-empty hex");
    assert!(!work.b.is_empty(), "b (target) should be non-empty");
    assert!(!work.pk.is_empty(), "pk should be non-empty hex");
    assert!(!work.proof.msg_preimage.is_empty(), "preimage should be non-empty");

    // Verify candidate structure
    assert_eq!(candidate.transactions.len(), 1, "should have 1 tx (emission only)");
    assert_eq!(candidate.version, 2);
    assert_eq!(candidate.n_bits, INITIAL_N_BITS);
    assert_eq!(candidate.votes, [0, 0, 0]);

    // CPU mine — with difficulty 1, first nonce should work
    let header = cpu_mine(&candidate, 100).expect("mining should succeed with trivial difficulty");

    // Verify the mined header
    assert_eq!(header.height, 2);
    assert_eq!(header.n_bits, INITIAL_N_BITS);
    assert!(
        header.check_pow().expect("check_pow should not error"),
        "mined block should have valid PoW"
    );
}

#[test]
fn mine_three_consecutive_blocks() {
    let settings = MonetarySettings::default();
    let pks = founder_pks();
    let (emission_box, _, _) =
        genesis::genesis_boxes(&settings, &pks, 2, PROOFS).unwrap();

    let miner_pk = test_miner_pk();
    let config = MinerConfig {
        miner_pk: miner_pk.clone(),
        reward_delay: 720,
        votes: [0, 0, 0],
        candidate_ttl: Duration::from_secs(15),
    };

    let mock_proofs =
        |_txs: &[ergo_lib::chain::transaction::Transaction]|
         -> Option<Result<(Vec<u8>, ADDigest), ergo_validation::ValidationError>> {
            Some(Ok((vec![0u8; 64], ADDigest::from([0u8; 33]))))
        };

    let mut parent = genesis_header();
    let mut current_emission_box = emission_box;
    let emission_rules = EmissionRules::new(MonetarySettings::default());

    // Track interlinks across blocks.
    // Genesis (height 1): interlinks are empty (update_interlinks returns vec![genesis.id])
    // Block 2+: interlinks from previous update_interlinks call
    let mut interlinks: Vec<BlockId> = vec![];

    for i in 0..3u32 {
        let height = parent.height + 1;

        let (candidate, _work) = ergo_mining::generate_candidate(
            &config,
            &parent,
            INITIAL_N_BITS,
            &interlinks,
            &current_emission_box,
            &mock_proofs,
        )
        .unwrap_or_else(|e| panic!("candidate at height {height} failed: {e}"));

        let header = cpu_mine(&candidate, 100)
            .unwrap_or_else(|e| panic!("mining block at height {height} failed: {e}"));

        assert_eq!(header.height, height);
        assert!(
            header.check_pow().unwrap(),
            "block at height {height} PoW invalid"
        );

        // Update interlinks for the next block
        interlinks = ergo_nipopow::NipopowAlgos::update_interlinks(
            parent.clone(),
            interlinks,
        )
        .unwrap_or_else(|e| panic!("interlinks update at height {height} failed: {e}"));

        // The emission tx is the first transaction in the candidate.
        // Its first output is the new emission box for the next block.
        let emission_tx = &candidate.transactions[0];
        current_emission_box = emission_tx.outputs.get(0).unwrap().clone();

        // Use the mined header as parent for the next block
        parent = header;
    }

    // Verify emission box value decreased over 3 blocks (heights 2, 3, 4)
    let total_reward: i64 = (2..=4)
        .map(|h| emission_rules.miners_reward_at_height(h))
        .sum();

    let original_value = {
        let (e, _, _) = genesis::genesis_boxes(&settings, &pks, 2, PROOFS).unwrap();
        e.value.as_i64()
    };

    assert_eq!(
        current_emission_box.value.as_i64(),
        original_value - total_reward,
        "emission box value should decrease by cumulative rewards"
    );
}

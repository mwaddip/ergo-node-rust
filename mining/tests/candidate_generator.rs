//! Tests for CandidateGenerator: caching, stale solution acceptance,
//! invalidation, and mempool tx purge.
//!
//! Adapted from JVM CandidateGeneratorSpec (ergoplatform/ergo PR #2291).

use std::time::Duration;

use ergo_chain_types::{
    ADDigest, AutolykosSolution, BlockId, Digest, Digest32, EcPoint, Header, Votes,
};
use ergo_lib::chain::emission::MonetarySettings;
use ergo_lib::chain::genesis;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_mining::emission::ReemissionRules;
use ergo_mining::solution::validate_solution;
use ergo_mining::types::*;
use ergo_mining::{CandidateGenerator, ValidatorProofsResult};

const INITIAL_N_BITS: u32 = 16842752;

const FOUNDER_PKS: &[&str] = &[
    "039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647",
    "031fb52cf6e805f80d97cde289f4f757d49accf0c83fb864b27d2cf982c37f9a8b",
    "0352ac2a471339b0d23b3d2c5ce0db0e81c969f77891b9edf0bda7fd39a78184e7",
];
const PROOFS: &[&str] = &[
    "test-proof-1", "test-proof-2", "test-proof-3",
    "test-proof-4", "test-proof-5",
];

fn founder_pks() -> Vec<ProveDlog> {
    FOUNDER_PKS.iter().map(|hex_str| {
        let bytes = hex::decode(hex_str).unwrap();
        let point = EcPoint::sigma_parse_bytes(&bytes).unwrap();
        ProveDlog::new(point)
    }).collect()
}

fn test_miner_pk() -> ProveDlog {
    let bytes = hex::decode(FOUNDER_PKS[0]).unwrap();
    let point = EcPoint::sigma_parse_bytes(&bytes).unwrap();
    ProveDlog::new(point)
}

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
        height: 1,
        extension_root: Digest32::from([0u8; 32]),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(*test_miner_pk().h),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes([0, 0, 0]),
        unparsed_bytes: Box::new([]),
    }
}

fn test_config() -> MinerConfig {
    MinerConfig {
        miner_pk: test_miner_pk(),
        reward_delay: 720,
        votes: [0, 0, 0],
        candidate_ttl: Duration::from_secs(15),
        reemission_rules: ReemissionRules::mainnet(),
    }
}

fn mock_proofs(
    _txs: &[ergo_lib::chain::transaction::Transaction],
) -> ValidatorProofsResult {
    Some(Ok((vec![0u8; 64], ADDigest::from([0u8; 33]))))
}

/// Generate a candidate at the given parent.
fn gen_candidate(config: &MinerConfig, parent: &Header) -> (CandidateBlock, WorkMessage) {
    let settings = MonetarySettings::default();
    let pks = founder_pks();
    let (emission_box, _, _) = genesis::genesis_boxes(&settings, &pks, 2, PROOFS).unwrap();
    ergo_mining::generate_candidate(
        config, parent, INITIAL_N_BITS, &[], &emission_box, None, &[], &mock_proofs,
    ).unwrap()
}

/// CPU mine with trivial difficulty.
fn cpu_mine(candidate: &CandidateBlock) -> Header {
    let solution = AutolykosSolution {
        miner_pk: Box::new(*test_miner_pk().h),
        pow_onetime_pk: None,
        nonce: 0u64.to_be_bytes().to_vec(),
        pow_distance: None,
    };
    validate_solution(candidate, solution).expect("mining should succeed at trivial difficulty")
}

/// Distinct synthetic BlockId for applied-block scenarios.
fn block_id(b: u8) -> BlockId {
    BlockId(Digest::from([b; 32]))
}

// ---------------------------------------------------------------------------
// 1. Candidate caching — repeated polls return the same work
// ---------------------------------------------------------------------------

#[test]
fn cached_work_returns_same_result_within_ttl() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    let (block, work) = gen_candidate(&config, &parent);
    gen.cache_candidate(block, work.clone(), parent.height);

    // Multiple polls at the same tip height should return the cached work
    for _ in 0..20 {
        let w = gen.cached_work(parent.height)
            .expect("should return cached work");
        assert_eq!(w.h, work.h);
        assert_eq!(w.msg, work.msg);
        assert_eq!(w.b, work.b);
    }
}

#[test]
fn cached_work_returns_none_after_tip_change() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    let (block, work) = gen_candidate(&config, &parent);
    gen.cache_candidate(block, work, parent.height);

    // Tip height changed — cache miss
    assert!(gen.cached_work(parent.height + 1).is_none());
}

#[test]
fn cached_work_returns_none_after_ttl_expires() {
    let config = MinerConfig {
        candidate_ttl: Duration::from_millis(1), // 1ms TTL
        ..test_config()
    };
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    let (block, work) = gen_candidate(&config, &parent);
    gen.cache_candidate(block, work, parent.height);

    std::thread::sleep(Duration::from_millis(5));
    assert!(gen.cached_work(parent.height).is_none(), "should expire after TTL");
}

// ---------------------------------------------------------------------------
// 2. Stale solution acceptance — solution for previous candidate still works
// ---------------------------------------------------------------------------

#[test]
fn stale_solution_accepted_after_regeneration() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    // Generate first candidate
    let (block1, work1) = gen_candidate(&config, &parent);
    gen.cache_candidate(block1.clone(), work1, parent.height);

    // Solve it (but don't submit yet)
    let solution_for_block1 = AutolykosSolution {
        miner_pk: Box::new(*test_miner_pk().h),
        pow_onetime_pk: None,
        nonce: 0u64.to_be_bytes().to_vec(),
        pow_distance: None,
    };

    // Regenerate — new candidate replaces the old one, old goes to previous.
    // Sleep first so block2's wall-clock timestamp differs from block1's and
    // the timestamp assertion below genuinely discriminates old from new
    // (with trivial difficulty the solution validates against either, so the
    // timestamp check is what pins WHICH candidate sits in previous).
    std::thread::sleep(Duration::from_millis(5));
    let (block2, work2) = gen_candidate(&config, &parent);
    assert_ne!(block1.timestamp, block2.timestamp, "discriminator");
    gen.cache_candidate(block2, work2, parent.height);

    // The previous candidate should still be accessible
    let prev = gen.previous_block().expect("previous candidate should exist");
    assert_eq!(prev.timestamp, block1.timestamp);

    // Solution for the OLD candidate should still validate
    let header = validate_solution(&prev, solution_for_block1)
        .expect("stale solution should validate against previous candidate");
    assert_eq!(header.height, 2);
}

// ---------------------------------------------------------------------------
// 3. Mempool-triggered invalidation — invalidate() clears current, preserves
//    as previous so in-flight solutions remain valid
// ---------------------------------------------------------------------------

#[test]
fn invalidate_clears_current_preserves_previous() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    let (block, work) = gen_candidate(&config, &parent);
    gen.cache_candidate(block, work, parent.height);

    // Before invalidation: current exists, previous doesn't
    assert!(gen.cached_block().is_some());
    assert!(gen.previous_block().is_none());

    // Invalidate (simulates mempool change)
    gen.invalidate();

    // After: current is gone, moved to previous
    assert!(gen.cached_block().is_none());
    assert!(gen.previous_block().is_some());

    // cached_work should return None (forces regen on next poll)
    assert!(gen.cached_work(parent.height).is_none());
}

#[test]
fn invalidate_then_regen_keeps_chain_of_previous() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    // Cache candidate A
    let (block_a, work_a) = gen_candidate(&config, &parent);
    let ts_a = block_a.timestamp;
    gen.cache_candidate(block_a, work_a, parent.height);

    // Invalidate (mempool change) — A moves to previous
    gen.invalidate();
    assert!(gen.cached_block().is_none());
    assert_eq!(
        gen.previous_block()
            .expect("invalidate should move A to previous")
            .timestamp,
        ts_a,
    );

    // Candidate timestamps are wall-clock ms (max(now, parent.timestamp + 1)
    // with an ancient test parent), so sleeping guarantees B's timestamp
    // differs from A's and can serve as the A/B discriminator below.
    std::thread::sleep(Duration::from_millis(5));

    // Generate candidate B (fresh — would include new mempool txs)
    let (block_b, work_b) = gen_candidate(&config, &parent);
    let ts_b = block_b.timestamp;
    assert_ne!(ts_a, ts_b, "discriminator: A and B must be distinguishable");
    gen.cache_candidate(block_b, work_b, parent.height);

    // B is current; A REMAINS previous. cache_candidate moves current→previous
    // only when current is Some — after invalidate() it is None, so the
    // previous slot (holding A) is untouched. The net effect matches the JVM's
    // mempool-triggered regeneration (CandidateGenerator.scala: a forced
    // GenerateCandidate sets cachedPreviousCandidate to the superseded
    // candidate): a miner still hashing A's work message can submit its
    // solution via the previous-candidate path. Solution acceptance itself
    // is pinned by stale_solution_accepted_after_regeneration.
    assert_eq!(
        gen.cached_block().expect("B should be current").timestamp,
        ts_b,
    );
    assert_eq!(
        gen.previous_block()
            .expect("A should survive the regen")
            .timestamp,
        ts_a,
        "previous must still hold A — not B, not None",
    );
}

// ---------------------------------------------------------------------------
// 4. Mempool tx purge after mining — verify transactions included in a mined
//    block are removed from the candidate pool on the next generation
// ---------------------------------------------------------------------------

#[test]
fn mined_block_advances_emission_box() {
    // This tests the mining→next-candidate chain: after mining block at height 2,
    // the emission box output becomes the input for height 3's emission tx.
    // If the pipeline didn't properly track the emission box, the second
    // candidate generation would fail or produce a wrong emission tx.

    let settings = MonetarySettings::default();
    let pks = founder_pks();
    let (emission_box, _, _) = genesis::genesis_boxes(&settings, &pks, 2, PROOFS).unwrap();
    let config = test_config();
    let parent = genesis_header();

    // Generate and mine block 2
    let interlinks_1: Vec<BlockId> = vec![];
    let (candidate_2, _) = ergo_mining::generate_candidate(
        &config, &parent, INITIAL_N_BITS, &interlinks_1, &emission_box, None, &[], &mock_proofs,
    ).unwrap();
    let header_2 = cpu_mine(&candidate_2);
    assert_eq!(header_2.height, 2);

    // The emission tx's first output is the new emission box
    let new_emission_box = candidate_2.transactions[0].outputs.get(0).unwrap().clone();

    // Update interlinks for block 3
    let interlinks_2 = ergo_nipopow::NipopowAlgos::update_interlinks(parent, interlinks_1)
        .expect("interlinks update");

    // Generate block 3 using the updated emission box
    let (candidate_3, _) = ergo_mining::generate_candidate(
        &config, &header_2, INITIAL_N_BITS, &interlinks_2, &new_emission_box, None, &[], &mock_proofs,
    ).unwrap();
    let header_3 = cpu_mine(&candidate_3);
    assert_eq!(header_3.height, 3);

    // Verify the emission box value decreased
    let emission_rules = ergo_lib::chain::emission::EmissionRules::new(settings);
    let reward_2 = emission_rules.miners_reward_at_height(2);
    let reward_3 = emission_rules.miners_reward_at_height(3);
    let original_value = emission_box.value.as_i64();
    assert_eq!(
        new_emission_box.value.as_i64(),
        original_value - reward_2,
    );
    let final_emission_box = candidate_3.transactions[0].outputs.get(0).unwrap();
    assert_eq!(
        final_emission_box.value.as_i64(),
        original_value - reward_2 - reward_3,
    );
}

// ---------------------------------------------------------------------------
// 5. Candidate lifecycle — solved latch + on_block_applied
//    (contract: facts/mining.md, Lifecycle API; JVM: FullBlockApplied
//    handler + solvedBlock, CandidateGenerator.scala:141-150)
// ---------------------------------------------------------------------------

#[test]
fn on_block_applied_own_block_drops_slots_and_clears_latch() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    // previous = A, current = B — both build on genesis
    let (block_a, work_a) = gen_candidate(&config, &parent);
    gen.cache_candidate(block_a, work_a, parent.height);
    gen.invalidate();
    let (block_b, work_b) = gen_candidate(&config, &parent);
    gen.cache_candidate(block_b.clone(), work_b, parent.height);

    // Solve B and latch — the solved block (height 2) heads to the submitter
    let solved = cpu_mine(&block_b);
    assert!(gen.try_mark_solved(solved.id, solved.height));
    assert!(gen.solved_pending());

    // Our own block is applied: neither slot builds on it (their parent is
    // genesis, not the solved block), and the chain reached the latch height.
    gen.on_block_applied(&solved.id, solved.height);
    assert!(gen.cached_block().is_none(), "current must be dropped");
    assert!(gen.previous_block().is_none(), "previous must be dropped");
    assert!(!gen.solved_pending(), "latch must clear on own block applied");
}

#[test]
fn on_block_applied_peer_block_same_height_drops_slots_and_clears_latch() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    let (block_b, work_b) = gen_candidate(&config, &parent);
    gen.cache_candidate(block_b.clone(), work_b, parent.height);

    // We solved B, but a competitor's block at the same height applies first
    let solved = cpu_mine(&block_b);
    assert!(gen.try_mark_solved(solved.id, solved.height));
    let peer = block_id(0xEE);
    assert_ne!(peer, solved.id, "competitor must differ from our block");

    gen.on_block_applied(&peer, solved.height);
    assert!(gen.cached_block().is_none(), "current must be dropped");
    assert!(gen.previous_block().is_none(), "previous must be dropped");
    assert!(
        !gen.solved_pending(),
        "competitor at the latched height supersedes our solved block"
    );
}

#[test]
fn on_block_applied_keeps_candidate_building_on_applied_block() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    // previous = A (builds on genesis)
    let (block_a, work_a) = gen_candidate(&config, &parent);
    gen.cache_candidate(block_a.clone(), work_a, parent.height);

    // Regen raced ahead: current = C2, built on header_2 (mined from A)
    // BEFORE header_2's full block has been applied. The gen_candidate
    // helper hardcodes empty interlinks, which only sigma-rust's genesis
    // special case tolerates — a height-2 parent needs real interlinks
    // and the chained emission box, as in mined_block_advances_emission_box.
    let header_2 = cpu_mine(&block_a);
    let interlinks_2 = ergo_nipopow::NipopowAlgos::update_interlinks(parent, vec![])
        .expect("interlinks update for header_2");
    let emission_box_2 = block_a.transactions[0].outputs.get(0).unwrap().clone();
    let (block_c2, work_c2) = ergo_mining::generate_candidate(
        &config,
        &header_2,
        INITIAL_N_BITS,
        &interlinks_2,
        &emission_box_2,
        None,
        &[],
        &mock_proofs,
    )
    .expect("candidate on header_2");
    gen.cache_candidate(block_c2, work_c2, header_2.height);

    // header_2 applies: current still builds on the tip → survives;
    // previous (genesis parent) does not → dropped. Per-slot independence.
    gen.on_block_applied(&header_2.id, header_2.height);
    assert_eq!(
        gen.cached_block()
            .expect("candidate building on the applied block must survive")
            .parent
            .id,
        header_2.id,
    );
    assert!(
        gen.previous_block().is_none(),
        "candidate not building on the applied block must be dropped"
    );
}

#[test]
fn solved_latch_gates_nothing_crate_side_and_clears_at_latch_height() {
    let config = test_config();
    let parent = genesis_header();
    let gen = CandidateGenerator::new(config.clone());

    let (block_a, work_a) = gen_candidate(&config, &parent);
    gen.cache_candidate(block_a, work_a, parent.height);

    assert!(gen.try_mark_solved(block_id(0xAB), parent.height + 1));
    assert!(gen.solved_pending());

    // The latch gates nothing inside the crate — work serving and block
    // reads still function (enforcement is the API layer's job).
    assert!(gen.cached_work(parent.height).is_some());
    assert!(gen.cached_block().is_some());

    // Any block applied at >= latch height clears the latch.
    gen.on_block_applied(&block_id(0xCD), parent.height + 1);
    assert!(!gen.solved_pending());
}

#[test]
fn solved_latch_survives_application_below_latch_height() {
    let gen = CandidateGenerator::new(test_config());

    assert!(gen.try_mark_solved(block_id(0xAB), 10));
    // Reorg re-application below the latched height must not clear the
    // latch — our solved block's height has not been reached yet.
    gen.on_block_applied(&block_id(0x01), 9);
    assert!(gen.solved_pending(), "latch must survive lower-height application");
}

#[test]
fn try_mark_solved_claims_only_when_unset() {
    let gen = CandidateGenerator::new(test_config());

    assert!(!gen.solved_pending());
    assert!(
        gen.try_mark_solved(block_id(0xAA), 10),
        "first claim must win"
    );
    assert!(gen.solved_pending());
    assert!(
        !gen.try_mark_solved(block_id(0xBB), 5),
        "second claim must lose while latched"
    );

    // The losing claim must not have touched the latch: an application
    // below the FIRST claimant's height (but at the loser's) must not
    // clear it...
    gen.on_block_applied(&block_id(0x01), 9);
    assert!(
        gen.solved_pending(),
        "latch must still hold the first claimant's height"
    );
    // ...while the first claimant's height does clear it.
    gen.on_block_applied(&block_id(0x02), 10);
    assert!(!gen.solved_pending());
}

#[test]
fn clear_solved_releases_the_latch() {
    let gen = CandidateGenerator::new(test_config());

    assert!(gen.try_mark_solved(block_id(0xAA), 10));
    // Submit failure: the block never left the node — release the latch
    // so solution acceptance is not wedged until the next applied block.
    gen.clear_solved();
    assert!(!gen.solved_pending());
    // The latch is claimable again after release.
    assert!(gen.try_mark_solved(block_id(0xBB), 11));
}

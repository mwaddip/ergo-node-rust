//! Wiring test for `build_work_message`.
//!
//! Verifies that the bytes produced by `build_work_message` match what an
//! independently-constructed `Header` would produce via sigma-rust's
//! already-JVM-verified `serialize_without_pow`.
//!
//! ## Why this is the cross-verification gate
//!
//! The byte format of `bytesWithoutPow` is verified upstream by sigma-rust
//! against the JVM reference: `ergo-chain-types::autolykos_pow_scheme::tests::
//! test_first_increase_in_big_n` parses a real mainnet header at height
//! 614,400 and asserts the resulting `Blake2b256(serialize_without_pow())`
//! equals the canonical hex `548c3e60...` produced by JVM
//! `HeaderSerializer.bytesWithoutPow`. As long as we pin a sigma-rust
//! revision where that test passes, we get JVM-equivalence on the encoder
//! for free.
//!
//! What this file verifies is the layer above: that our `build_work_message`
//! correctly maps `CandidateBlock` fields into the `Header` struct before
//! handing it to the encoder. The risk we're catching is field-mapping
//! bugs — accidentally swapping `state_root` and `transaction_root`,
//! using `parent.height` instead of `parent.height + 1`, putting the
//! extension digest in the wrong slot, etc. The compiler can't catch
//! these because all the digest types are interchangeable byte arrays.
//!
//! The second test pins the sigma-rust serializer output against the JVM
//! canonical hex in our own test suite. It catches drift on future
//! sigma-rust bumps without us having to also run sigma-rust's tests.

use std::str::FromStr;

use blake2::Digest as Blake2Digest;
use enr_chain::BigInt;
use ergo_chain_types::autolykos_pow_scheme::{decode_compact_bits, AutolykosPowScheme};
use ergo_chain_types::{
    ADDigest, AutolykosSolution, BlockId, Digest, Digest32, EcPoint, Header, Votes,
};
use ergo_lib::chain::emission::MonetarySettings;
use ergo_lib::chain::genesis;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_mining::candidate::{build_work_message, transactions_root};
use ergo_mining::emission::{build_emission_tx, ReemissionRules};
use ergo_mining::extension::extension_digest;
use ergo_mining::solution::validate_solution;
use ergo_mining::types::*;
use ergo_mining::MiningError;
use num_bigint::ToBigInt;
use sigma_ser::ScorexSerializable;

type Blake2b256 = blake2::Blake2b<blake2::digest::typenum::U32>;

fn blake2b256_bytes(input: &[u8]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    h.update(input);
    h.finalize().into()
}

const FOUNDER_PKS: &[&str] = &[
    "039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647",
    "031fb52cf6e805f80d97cde289f4f757d49accf0c83fb864b27d2cf982c37f9a8b",
    "0352ac2a471339b0d23b3d2c5ce0db0e81c969f77891b9edf0bda7fd39a78184e7",
];

const TEST_PROOFS: &[&str] = &[
    "test-proof-1",
    "test-proof-2",
    "test-proof-3",
    "test-proof-4",
    "test-proof-5",
];

const TRIVIAL_N_BITS: u32 = 16_842_752;

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

fn test_miner_pk() -> ProveDlog {
    let bytes = hex::decode(FOUNDER_PKS[0]).unwrap();
    let point = EcPoint::sigma_parse_bytes(&bytes).unwrap();
    ProveDlog::new(point)
}

/// Build a parent header with a recognizable, non-zero parent ID so a swap
/// with `transaction_root` or any other 32-byte field would show up at the
/// wrong byte offset in the serialized output.
fn parent_for_wiring_test() -> Header {
    Header {
        version: 2,
        id: BlockId(Digest::from([0xAB; 32])),
        parent_id: BlockId(Digest::from([0u8; 32])),
        ad_proofs_root: Digest32::from([0u8; 32]),
        state_root: ADDigest::from([0u8; 33]),
        transaction_root: Digest32::from([0u8; 32]),
        timestamp: 1_700_000_000_000,
        n_bits: TRIVIAL_N_BITS,
        height: 100,
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

#[test]
fn build_work_message_wiring_matches_independent_serialization() {
    // ---- 1. Set up known inputs with distinct, recognizable values ----
    let parent = parent_for_wiring_test();
    let settings = MonetarySettings::default();
    let pks = founder_pks();
    let (emission_box, _, _) = genesis::genesis_boxes(&settings, &pks, 2, TEST_PROOFS).unwrap();

    let miner_pk = test_miner_pk();
    let reemission_rules = ReemissionRules::mainnet();
    let height = parent.height + 1;

    // Real emission tx so `transactions_root` has something to chew on.
    let emission_tx =
        build_emission_tx(&emission_box, height, &miner_pk, 720, &reemission_rules).unwrap();
    let transactions = vec![emission_tx];

    // Distinct, recognizable values. A field swap would land these bytes
    // at the wrong offset in the serialized header and the assertion would
    // fail at that exact spot.
    let known_state_root = ADDigest::from([0xAA; 33]);
    let known_ad_proof_bytes = vec![0xCC; 16];
    let known_votes = [0xDE, 0xAD, 0xBE];
    let known_timestamp = 1_700_000_000_500u64;

    // Single arbitrary extension entry — we're not testing build_extension
    // here, we're testing that build_work_message wires the resulting
    // digest into the right Header field.
    let extension = ExtensionCandidate {
        fields: vec![([0x01, 0x00], vec![0xEE; 32])],
    };

    let candidate = CandidateBlock {
        parent: parent.clone(),
        version: parent.version,
        n_bits: TRIVIAL_N_BITS,
        state_root: known_state_root,
        ad_proof_bytes: known_ad_proof_bytes.clone(),
        transactions: transactions.clone(),
        timestamp: known_timestamp,
        extension: extension.clone(),
        votes: known_votes,
        header_bytes: vec![],
    };

    // ---- 2. Run build_work_message ----
    let (bytes_via_build, work) = build_work_message(&candidate, &miner_pk.h).unwrap();

    // ---- 3. Independently construct the Header that build_work_message
    //         should have produced, and serialize it the same way sigma-rust
    //         does. None of this code path goes through build_work_message
    //         — every field assignment is right here, in the test, where
    //         a swap would be visible to the reader. ----
    let expected_ad_proofs_root: [u8; 32] = blake2b256_bytes(&known_ad_proof_bytes);
    let expected_tx_root = transactions_root(&transactions, parent.version).unwrap();
    let expected_extension_root = extension_digest(&extension).unwrap();

    let independent_header = Header {
        version: parent.version,
        id: BlockId(Digest::from([0u8; 32])),
        parent_id: parent.id,
        ad_proofs_root: Digest32::from(expected_ad_proofs_root),
        state_root: known_state_root,
        transaction_root: expected_tx_root,
        timestamp: known_timestamp,
        n_bits: TRIVIAL_N_BITS,
        height,
        extension_root: Digest32::from(expected_extension_root),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(*miner_pk.h),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes(known_votes),
        unparsed_bytes: Box::new([]),
    };

    let bytes_via_serialize = independent_header
        .serialize_without_pow()
        .expect("independent serialize_without_pow failed");

    // ---- 4. Bytes must match exactly ----
    assert_eq!(
        bytes_via_build, bytes_via_serialize,
        "build_work_message produced different bytes than an independently-\
         constructed Header serialized via sigma-rust. A field-mapping bug \
         in build_work_message will land here."
    );

    // ---- 5. work.msg must equal blake2b256 of those bytes (hex encoded) ----
    let expected_msg = hex::encode(blake2b256_bytes(&bytes_via_serialize));
    assert_eq!(work.msg, expected_msg, "WorkMessage.msg mismatch");

    // ---- 6. the basic candidate omits the proof entirely ----
    // `build_work_message` produces the {msg, b, h, pk} basic candidate
    // (proof None). The header-preimage wiring is already verified
    // byte-for-byte at step 4 via the returned `bytes_via_build`; a future
    // candidateWithTxs path would set Some and hex-encode those same bytes.
    assert!(
        work.proof.is_none(),
        "basic candidate must omit the proof (got Some)"
    );

    // ---- 7. Other WorkMessage fields ----
    assert_eq!(work.h, height as u32);
}

/// Tripwire: pins the sigma-rust `serialize_without_pow` + `Blake2b256` chain
/// against the JVM-canonical hex for the height-614,400 mainnet header.
///
/// Sigma-rust has the equivalent test internally
/// (`ergo-chain-types::autolykos_pow_scheme::tests::test_first_increase_in_big_n`),
/// but having it here means a `cargo test -p ergo-mining` run catches drift
/// without depending on sigma-rust's own test suite.
///
/// Source of canonical hex: `ergoplatform/ergo`
/// `AutolykosPowSchemeSpec.scala`, property
/// `"test vectors for first increase in N value (height 614,400)"`.
#[test]
fn sigma_rust_serialize_without_pow_matches_jvm_canonical_height_614400() {
    let header = Header {
        version: 2,
        id: BlockId(Digest::from(hex_to_array_32(
            "5603a937ec1988220fc44fb5022fb82d5565b961f005ebb55d85bd5a9e6f801f",
        ))),
        parent_id: BlockId(Digest::from(hex_to_array_32(
            "ac2101807f0000ca01ff0119db227f202201007f62000177a080005d440896d0",
        ))),
        ad_proofs_root: Digest32::from(hex_to_array_32(
            "5d3f80dcff7f5e7f59007294c180808d0158d1ff6ba10000f901c7f0ef87dcff",
        )),
        state_root: ADDigest::from(hex_to_array_33(
            "5c8c00b8403d3701557181c8df800001b6d5009e2201c6ff807d71808c00019780",
        )),
        transaction_root: Digest32::from(hex_to_array_32(
            "f17fffacb6ff7f7f1180d2ff7f1e24ffffe1ff937f807f0797b9ff6ebdae007e",
        )),
        timestamp: 4_928_911_477_310_178_288,
        n_bits: 37_748_736,
        height: 614_400,
        extension_root: Digest32::from(hex_to_array_32(
            "1480887f80007f4b01cf7f013ff1ffff564a0000b9a54f00770e807f41ff88c0",
        )),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(
                EcPoint::sigma_parse_bytes(
                    &hex::decode(
                        "03bedaee069ff4829500b3c07c4d5fe6b3ea3d3bf76c5c28c1d4dcdb1bed0ade0c",
                    )
                    .unwrap(),
                )
                .unwrap(),
            ),
            pow_onetime_pk: None,
            nonce: hex::decode("0000000000003105").unwrap(),
            pow_distance: None,
        },
        votes: Votes([0, 0, 0]),
        unparsed_bytes: Box::new([]),
    };

    let bytes = header.serialize_without_pow().unwrap();
    let msg_hex = hex::encode(blake2b256_bytes(&bytes));

    assert_eq!(
        msg_hex, "548c3e602a8f36f8f2738f5f643b02425038044d98543a51cabaa9785e7e864f",
        "sigma-rust serialize_without_pow drifted from JVM-canonical msg \
         for height 614400. Either sigma-rust changed its serialization \
         (which would be a hard fork on the wire) or our blake2b256 \
         wrapper is producing different bytes than sigma-rust's."
    );
}

// ---------------------------------------------------------------------------
// WorkMessage.b — the target served to external miners
//
// v0.8.0 served `decode_compact_bits(n_bits)` here: the DIFFICULTY, not the
// target. A GPU miner ran nine minutes at 143 MH/s against the released node
// and submitted zero shares, because the bound it was handed was tens of
// orders of magnitude tighter than the one `check_pow` on the solution path
// actually enforces. Three tests over `b` (non-empty, stable across polls,
// bare JSON number) passed continuously throughout — present, consistent,
// well-typed, and the wrong quantity. These two assert the value.
// ---------------------------------------------------------------------------

/// Compact-bits encoding of the difficulty at testnet height 485,897.
/// **DERIVED**, not observed — see `served_b_matches_scala_node_vector`.
const SCALA_N_BITS: u32 = 83_945_773; // 0x0500e92d

/// Difficulty reported by a live Scala node at testnet height 485,897.
/// **Observed.**
const SCALA_DIFFICULTY: u64 = 3_912_040_448;

/// The `b` that same Scala node served for that height. **Observed**, and
/// cross-checked digit for digit against `floor(q / 3912040448)` (remainder
/// 1309294913).
const SCALA_TARGET: &str = "29598898778389163379010897437604384363675568080188445020547283242588";

/// Compact-bits encoding of a small integer difficulty. For `size == 3` the
/// mantissa *is* the value, so this is just `0x03000000 | d`.
fn n_bits_for_difficulty(d: u32) -> u32 {
    assert!(d <= 0x007f_ffff, "difficulty must fit the 23-bit mantissa");
    0x0300_0000 | d
}

/// The wiring-test fixture at a chosen difficulty encoding. Only `n_bits`
/// varies, so the header bytes — and therefore the PoW hit — are a
/// deterministic function of `n_bits` and the nonce.
fn candidate_at_n_bits(n_bits: u32) -> CandidateBlock {
    let parent = parent_for_wiring_test();
    let settings = MonetarySettings::default();
    let pks = founder_pks();
    let (emission_box, _, _) = genesis::genesis_boxes(&settings, &pks, 2, TEST_PROOFS).unwrap();
    let height = parent.height + 1;
    let emission_tx = build_emission_tx(
        &emission_box,
        height,
        &test_miner_pk(),
        720,
        &ReemissionRules::mainnet(),
    )
    .unwrap();

    CandidateBlock {
        parent,
        version: 2,
        n_bits,
        state_root: ADDigest::from([0xAA; 33]),
        ad_proof_bytes: vec![0xCC; 16],
        transactions: vec![emission_tx],
        timestamp: 1_700_000_000_500,
        extension: ExtensionCandidate {
            fields: vec![([0x01, 0x00], vec![0xEE; 32])],
        },
        votes: [0, 0, 0],
        header_bytes: vec![],
    }
}

/// The header `validate_solution` assembles for `candidate` + `nonce`, rebuilt
/// here because `pow_hit` needs a `Header` and the solution path only hands one
/// back when it accepts. Every accepted solution below cross-checks the two
/// serialize identically, so the hit read out of this really is the hit the
/// solution path compared.
fn header_for_nonce(candidate: &CandidateBlock, nonce: u64) -> Header {
    Header {
        version: candidate.version,
        id: BlockId(Digest::from([0u8; 32])),
        parent_id: candidate.parent.id,
        ad_proofs_root: Digest32::from(blake2b256_bytes(&candidate.ad_proof_bytes)),
        state_root: candidate.state_root,
        transaction_root: transactions_root(&candidate.transactions, candidate.version).unwrap(),
        timestamp: candidate.timestamp,
        n_bits: candidate.n_bits,
        height: candidate.parent.height + 1,
        extension_root: Digest32::from(extension_digest(&candidate.extension).unwrap()),
        autolykos_solution: solution_with_nonce(nonce),
        votes: Votes(candidate.votes),
        unparsed_bytes: Box::new([]),
    }
}

fn solution_with_nonce(nonce: u64) -> AutolykosSolution {
    AutolykosSolution {
        miner_pk: Box::new(*test_miner_pk().h),
        pow_onetime_pk: None,
        nonce: nonce.to_be_bytes().to_vec(),
        pow_distance: None,
    }
}

/// (a) The served `b` against a fixed pair captured from a Scala node.
///
/// Provenance, because the three numbers are not the same kind of thing:
/// `SCALA_DIFFICULTY` and `SCALA_TARGET` were **observed** — read off a live
/// Scala node at testnet height 485,897 and compared digit for digit.
/// `SCALA_N_BITS` is **derived**, computed by canonical compact-bits encoding
/// of the observed difficulty rather than read off the wire. So the round-trip
/// is asserted first: a bad derivation then fails loudly here instead of
/// letting the target assertion quietly pass against some other difficulty.
///
/// Asserted against the literal on purpose. `assert_eq!(work.b,
/// pow_target(n).to_string())` would restate the implementation and pass on
/// any self-consistent definition — including the one that shipped.
///
/// `chain/` carries the same vector against `pow_target` directly
/// (`chain/src/tests.rs::pow_target_matches_scala_node`). That is deliberate,
/// not duplication to consolidate: this one covers the whole serve path out to
/// the string a miner receives, that one covers the function.
#[test]
fn served_b_matches_scala_node_vector() {
    assert_eq!(
        decode_compact_bits(SCALA_N_BITS),
        BigInt::from(SCALA_DIFFICULTY),
        "n_bits derivation is wrong — the target assertion below would be \
         testing a difficulty the Scala node never reported"
    );

    let candidate = candidate_at_n_bits(SCALA_N_BITS);
    let (_, work) = build_work_message(&candidate, &test_miner_pk().h).unwrap();

    assert_eq!(
        work.b, SCALA_TARGET,
        "served b diverged from the value a Scala node published for the same \
         nBits. Miners hash against this number; a wrong one costs them every \
         share they find."
    );

    // The specific wrong answer, named. 10 digits against 68.
    assert_ne!(
        work.b,
        SCALA_DIFFICULTY.to_string(),
        "b is the difficulty again — that is the v0.8.0 defect exactly"
    );
}

/// (b) Serve/verify agreement: the number handed to the miner is the number the
/// solution path checks a hit against.
///
/// That is the property that was actually violated — two paths, two numbers,
/// nothing tying them together — and a test pinning only the literal above
/// would still pass if the solution side later drifted.
///
/// `check_pow` is upstream's and opaque (it returns a bool, not its target), so
/// the target is observed at its boundary instead: for each nonce, the node's
/// own accept/reject verdict must agree with `hit < b_served`. At difficulty 2
/// roughly half the nonces clear the bar, so the scan sees both verdicts and
/// brackets `b` from both sides. Under the v0.8.0 bug `b` was 2, every hit is a
/// ~77-digit number, so every `hit < b` would read false while the node
/// accepted half of them — the first accepted nonce fails this.
#[test]
fn served_b_is_the_bound_the_solution_path_enforces() {
    const NONCES: u64 = 32;

    let n_bits = n_bits_for_difficulty(2);
    assert_eq!(
        decode_compact_bits(n_bits),
        BigInt::from(2u32),
        "compact-bits encoding helper is wrong"
    );

    let candidate = candidate_at_n_bits(n_bits);
    let (_, work) = build_work_message(&candidate, &test_miner_pk().h).unwrap();
    let b = BigInt::from_str(&work.b).expect("served b must be a decimal integer");

    let pow = AutolykosPowScheme::default();
    let (mut accepted, mut rejected) = (0u32, 0u32);

    for nonce in 0..NONCES {
        let header = header_for_nonce(&candidate, nonce);
        let hit = pow
            .pow_hit(&header)
            .expect("pow_hit on a well-formed v2 header")
            .to_bigint()
            .expect("unsigned -> signed conversion never fails");

        // The real entry point — the same call the /mining/solution handler makes.
        let node_accepts = match validate_solution(&candidate, solution_with_nonce(nonce)) {
            Ok(validated) => {
                // Prove the header whose hit we just read is the header the
                // solution path validated. `id` is not serialized, so equal
                // bytes means equal header.
                assert_eq!(
                    header.scorex_serialize_bytes().unwrap(),
                    validated.scorex_serialize_bytes().unwrap(),
                    "reconstructed header diverged from the one validate_solution \
                     built, so the hit below is for a different header"
                );
                true
            }
            Err(MiningError::InvalidSolution(_)) => false,
            Err(e) => panic!("unexpected mining error at nonce {nonce}: {e}"),
        };

        assert_eq!(
            node_accepts,
            hit < b,
            "serve/verify disagreement at nonce {nonce}: the node {} this \
             solution, but against the served b={b} the hit {hit} says the \
             opposite. The number miners are given and the number the node \
             checks must be the same number.",
            if node_accepts { "ACCEPTED" } else { "REJECTED" }
        );

        if node_accepts {
            accepted += 1;
        } else {
            rejected += 1;
        }
    }

    // Both verdicts must actually occur or the loop above asserted nothing
    // about where the boundary is. The fixture is fixed, so this is a
    // deterministic property of it — it fails only if someone changes the
    // fixture or the difficulty and makes the scan one-sided.
    assert!(
        accepted > 0 && rejected > 0,
        "scan went one-sided ({accepted} accepted, {rejected} rejected) — it no \
         longer brackets the target and proves nothing"
    );
}

fn hex_to_array_32(s: &str) -> [u8; 32] {
    let bytes = hex::decode(s).unwrap();
    bytes.try_into().unwrap()
}

fn hex_to_array_33(s: &str) -> [u8; 33] {
    let bytes = hex::decode(s).unwrap();
    bytes.try_into().unwrap()
}

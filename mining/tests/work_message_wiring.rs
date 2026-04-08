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

use blake2::Digest as Blake2Digest;
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
use ergo_mining::types::*;

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
            miner_pk: Box::new((*test_miner_pk().h).clone()),
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
    let (emission_box, _, _) =
        genesis::genesis_boxes(&settings, &pks, 2, TEST_PROOFS).unwrap();

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
    let expected_tx_root = transactions_root(&transactions).unwrap();
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
            miner_pk: Box::new((*miner_pk.h).clone()),
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

    // ---- 6. proof.msg_preimage must equal the bytes (hex encoded) ----
    assert_eq!(
        work.proof.msg_preimage,
        hex::encode(&bytes_via_serialize),
        "WorkMessage.proof.msg_preimage mismatch"
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
        msg_hex,
        "548c3e602a8f36f8f2738f5f643b02425038044d98543a51cabaa9785e7e864f",
        "sigma-rust serialize_without_pow drifted from JVM-canonical msg \
         for height 614400. Either sigma-rust changed its serialization \
         (which would be a hard fork on the wire) or our blake2b256 \
         wrapper is producing different bytes than sigma-rust's."
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

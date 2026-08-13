//! Block-version gate (facts/validation.md §1b) wired through
//! `DigestValidator::apply_state`.
//!
//! The gate is BOUNDARY-ONLY, mirroring JVM `exBlockVersion`: it compares
//! `header.version` against the newly computed boundary parameters inside
//! the epoch-boundary path, and does not exist mid-epoch (the JVM's
//! `processExtension` is gated on `epochStarts`, and no header-level
//! version rule exists anywhere else — mid-epoch the reference ignores
//! `header.version`).
//!
//! The gate runs after section parsing and the boundary parameter check,
//! before any proof verification — so these tests get away with empty
//! sections and garbage proofs: a rejection at the gate surfaces as
//! `BlockVersionMismatch`, while sailing past it surfaces as the
//! downstream `ProofDigestMismatch`.

use ergo_chain_types::{ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, Votes};
use ergo_lib::chain::parameters::Parameter;
use ergo_validation::{
    serialize_ad_proofs, serialize_block_transactions, serialize_extension, BlockValidator,
    DigestValidator, Parameters, ValidationError,
};

fn make_header(height: u32, version: u8) -> Header {
    let zero32 = Digest32::zero();
    Header {
        version,
        id: BlockId(Digest32::zero()),
        parent_id: BlockId(Digest32::zero()),
        ad_proofs_root: zero32,
        state_root: ADDigest::zero(),
        transaction_root: zero32,
        timestamp: 1_000_000 + height as u64,
        n_bits: 100_000,
        height,
        extension_root: zero32,
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(EcPoint::default()),
            pow_onetime_pk: None,
            nonce: vec![0; 8],
            pow_distance: None,
        },
        votes: Votes([0, 0, 0]),
        unparsed_bytes: Box::new([]),
    }
}

/// Parameters table containing only a BlockVersion entry (or nothing).
/// The gate is the first consumer of the boundary set in the flow, so a
/// minimal table is sufficient for these tests.
fn make_params(block_version: Option<i32>) -> Parameters {
    let mut params = Parameters::default();
    params.parameters_table.clear();
    if let Some(v) = block_version {
        params.parameters_table.insert(Parameter::BlockVersion, v);
    }
    params
}

struct Fixture {
    txs: Vec<u8>,
    proofs: Vec<u8>,
}

fn make_sections() -> Fixture {
    let hid = [0u8; 32];
    Fixture {
        txs: serialize_block_transactions(&hid, 2, &[]).expect("empty tx section"),
        proofs: serialize_ad_proofs(&hid, &[]),
    }
}

#[test]
fn mid_epoch_version_divergence_not_checked() {
    // The SANTA 3-vs-4 shape over a NON-boundary height: active params say
    // blockVersion 3, header.version is 4 — and that is NOT a rejection.
    // The JVM has no mid-epoch version rule (exBlockVersion fires only at
    // epoch boundaries), so neither do we: the flow proceeds and fails
    // downstream at the AD-proofs digest check, proving no version gate
    // fired on the way.
    let mut validator = DigestValidator::new(ADDigest::zero(), 0);
    let header = make_header(1, 4);
    let fx = make_sections();
    let ext = serialize_extension(&[0u8; 32], &[]).expect("empty extension");
    let active = make_params(Some(3));

    let err = validator
        .apply_state(&header, &fx.txs, Some(&fx.proofs), &ext, &[], &active, None, None)
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::ProofDigestMismatch { .. }),
        "mid-epoch version divergence must not be version-gated, got {err:?}"
    );
}

#[test]
fn boundary_mismatch_rejected_against_recomputed_set() {
    // SANTA `version-gate` mutation shape: boundary parameters say
    // blockVersion 3, header.version is 4. `active_params` deliberately
    // says 4 — if the gate (wrongly) consulted it, the gate would pass
    // and the test would see ProofDigestMismatch.
    let mut validator = DigestValidator::new(ADDigest::zero(), 0);
    let header = make_header(1, 4);
    let fx = make_sections();
    let ext = serialize_extension(
        &[0u8; 32],
        &[([0x00, 123], 3i32.to_be_bytes().to_vec())],
    )
    .expect("extension with params");
    let active = make_params(Some(4));
    let boundary = make_params(Some(3));

    let err = validator
        .apply_state(
            &header,
            &fx.txs,
            Some(&fx.proofs),
            &ext,
            &[],
            &active,
            Some(&boundary),
            None,
        )
        .unwrap_err();
    match err {
        ValidationError::BlockVersionMismatch { expected, got } => {
            assert_eq!(expected, 3);
            assert_eq!(got, 4);
        }
        other => panic!("expected BlockVersionMismatch, got {other:?}"),
    }
    // Postcondition on Err: state unchanged.
    assert_eq!(validator.validated_height(), 0);
}

#[test]
fn matching_version_passes_gate_at_boundary() {
    let mut validator = DigestValidator::new(ADDigest::zero(), 0);
    let header = make_header(1, 4);
    let fx = make_sections();
    let ext = serialize_extension(
        &[0u8; 32],
        &[([0x00, 123], 4i32.to_be_bytes().to_vec())],
    )
    .expect("extension with params");
    let active = make_params(Some(4));
    let boundary = make_params(Some(4));

    // Gate passes; the flow proceeds and fails downstream at the AD-proofs
    // digest check (blake2b of an empty proof != zeroed ad_proofs_root).
    let err = validator
        .apply_state(
            &header,
            &fx.txs,
            Some(&fx.proofs),
            &ext,
            &[],
            &active,
            Some(&boundary),
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::ProofDigestMismatch { .. }),
        "expected ProofDigestMismatch past the gate, got {err:?}"
    );
}

#[test]
fn absent_block_version_at_boundary_rejects_without_panicking() {
    // The boundary set lacks a BlockVersion entry entirely —
    // `Parameters::block_version()` would panic on this table. An empty
    // expected set passes the v6 subset check trivially (zero entries to
    // compare), so the gate is what must catch it: reject, not panic.
    let mut validator = DigestValidator::new(ADDigest::zero(), 0);
    let header = make_header(1, 4);
    let fx = make_sections();
    let ext = serialize_extension(
        &[0u8; 32],
        &[([0x00, 123], 4i32.to_be_bytes().to_vec())],
    )
    .expect("extension with params");
    let active = make_params(Some(4));
    let boundary = make_params(None);

    let err = validator
        .apply_state(
            &header,
            &fx.txs,
            Some(&fx.proofs),
            &ext,
            &[],
            &active,
            Some(&boundary),
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::StateOperationFailed(_)),
        "expected StateOperationFailed for absent BlockVersion, got {err:?}"
    );
    assert_eq!(validator.validated_height(), 0);
}

//! Step 6a: a creation height above the preheader is transient, not fatal.
//!
//! These are the crate's first `process()`-level tests, so they build real
//! artefacts rather than poking `OrderedPool` directly: a real `ErgoStateContext`,
//! real `ErgoTree`s, and transactions that genuinely reach ergo-lib's evaluator.
//! The bug under test is invisible to a stubbed validator — it lives precisely in
//! *which* outcome a real validation failure is mapped to.
//!
//! Scripts are the two trivial sigma propositions: `sigmaProp(true)` verifies
//! against an empty proof, `sigmaProp(false)` cannot. No key material, and the
//! failure mode of the false one does not depend on any particular reduction.
//!
//! ## Why `min_fee = 0` throughout
//!
//! ergo-lib enforces *exact* ERG preservation (`ErgPreservationError` when
//! `input_sum != output_sum`), so any transaction that survives validation has
//! `input_sum - output_sum == 0` — which is what `process::extract_fee` returns
//! as the fee. Under the default `min_fee` no validating transaction can ever
//! reach `Accepted`, so these tests set it to zero to isolate the creation-height
//! behaviour. That interaction is a separate defect (the contract's step 3 wants
//! outputs to the *fee proposition*, matching the JVM's `extractFee`); it is
//! reported to the main session and deliberately not addressed here.

use std::collections::HashMap;
use std::time::Instant;

use ergo_chain_types::{
    ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, PreHeader, Votes,
};
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::input::Input;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters};
use ergo_lib::ergotree_ir::chain::tx_id::TxId;
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
use ergo_lib::ergotree_ir::mir::bool_to_sigma::BoolToSigmaProp;
use ergo_lib::ergotree_ir::mir::expr::Expr;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use ergo_mempool::types::{MempoolConfig, ProcessingOutcome, UnconfirmedTx, UtxoReader};
use ergo_mempool::Mempool;
use ergo_validation::{ErgoStateContext, Parameters};

/// Well past genesis, so nothing here trips a near-genesis special case.
const TIP_HEIGHT: u32 = 1_000;
/// Comfortably above `min_value_per_byte × box size` for these small boxes.
const BOX_VALUE: u64 = 1_000_000_000;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `sigmaProp(<value>)` — reduces to a trivial true or false with no proof and
/// no prover key.
fn sigma_bool_tree(value: bool) -> ErgoTree {
    let expr = Expr::BoolToSigmaProp(BoolToSigmaProp {
        input: Box::new(Expr::Const(value.into())),
    });
    ErgoTree::try_from(expr).expect("sigmaProp(bool) is a valid ErgoTree")
}

/// A UTXO guarded by `sigmaProp(spendable)`, distinguished from its siblings by
/// `seed` (which varies the source tx id, hence the box id).
fn make_box(spendable: bool, seed: u8, creation_height: u32) -> ErgoBox {
    ErgoBox::new(
        BoxValue::try_from(BOX_VALUE).expect("value above the minimum"),
        sigma_bool_tree(spendable),
        None,
        NonMandatoryRegisters::empty(),
        creation_height,
        TxId::from(Digest32::from([seed; 32])),
        0,
    )
    .expect("box construction")
}

/// A transaction spending `inputs` into a single always-true output at
/// `out_creation_height`. Value is preserved exactly — ergo-lib rejects any
/// other ratio outright, and an unbalanced transaction would fail for a reason
/// that has nothing to do with creation height.
fn spend_tx(inputs: &[ErgoBox], out_creation_height: u32) -> Transaction {
    let total: u64 = inputs.iter().map(|b| *b.value.as_u64()).sum();
    let output = ErgoBoxCandidate {
        value: BoxValue::try_from(total).expect("output value above the minimum"),
        ergo_tree: sigma_bool_tree(true),
        tokens: None,
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: out_creation_height,
    };
    let tx_inputs: Vec<Input> = inputs
        .iter()
        .map(|b| {
            Input::new(
                b.box_id(),
                ProverResult {
                    proof: ProofBytes::Empty,
                    extension: ContextExtension::empty(),
                },
            )
        })
        .collect();
    Transaction::new_from_vec(tx_inputs, vec![], vec![output]).expect("transaction construction")
}

fn make_header(height: u32) -> Header {
    Header {
        version: 3,
        id: BlockId(Digest32::zero()),
        parent_id: BlockId(Digest32::zero()),
        ad_proofs_root: Digest32::zero(),
        state_root: ADDigest::zero(),
        transaction_root: Digest32::zero(),
        timestamp: 1_600_000_000_000 + height as u64,
        n_bits: 100_000,
        height,
        extension_root: Digest32::zero(),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(EcPoint::default()),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes([0, 0, 0]),
        unparsed_bytes: Box::new([]),
    }
}

/// A state context whose preheader sits at `preheader_height` — i.e. describing
/// the block about to be mined. Version 3 so the monotonic-height rule is live,
/// matching mainnet.
fn state_context_at(preheader_height: u32) -> ErgoStateContext {
    let pre_header = PreHeader {
        version: 3,
        parent_id: BlockId(Digest32::zero()),
        timestamp: 1_600_000_000_000 + preheader_height as u64,
        n_bits: 100_000,
        height: preheader_height,
        miner_pk: Box::new(EcPoint::default()),
        votes: Votes([0, 0, 0]),
    };
    let headers = vec![make_header(preheader_height.saturating_sub(1))]
        .try_into()
        .expect("one header is within the 1..10 bound");
    ErgoStateContext::new(pre_header, headers, Parameters::default())
}

/// A `UtxoReader` over a fixed set of boxes.
struct StaticUtxo(HashMap<[u8; 32], ErgoBox>);

impl StaticUtxo {
    fn new(boxes: &[ErgoBox]) -> Self {
        Self(
            boxes
                .iter()
                .map(|b| {
                    let mut id = [0u8; 32];
                    id.copy_from_slice(b.box_id().as_ref());
                    (id, b.clone())
                })
                .collect(),
        )
    }
}

impl UtxoReader for StaticUtxo {
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<ErgoBox> {
        self.0.get(box_id).cloned()
    }
}

/// See the module header: the default `min_fee` is unreachable for any
/// transaction that survives ergo-lib's exact ERG-preservation check.
fn test_config() -> MempoolConfig {
    MempoolConfig {
        min_fee: 0,
        ..MempoolConfig::default()
    }
}

fn tx_id_bytes(tx: &Transaction) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr.copy_from_slice(tx.id().as_ref());
    arr
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The whole point. A transaction one block ahead of us is declined, leaves no
/// trace in the invalidation cache, and is accepted on the retry after a block
/// is applied. The bug was that the retry never got that far: the first attempt
/// cached the tx as invalid, and step 1 dropped every rebroadcast for the next
/// `invalidation_ttl` without re-validating.
#[test]
fn future_creation_height_declines_then_succeeds_next_block() {
    let mut mempool = Mempool::new(test_config());

    let input = make_box(true, 1, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    // The sender is a block ahead of us: they built against tip TIP_HEIGHT,
    // so their outputs carry creation height TIP_HEIGHT + 1. Our preheader is
    // still describing block TIP_HEIGHT.
    let tx = spend_tx(&[input], TIP_HEIGHT + 1);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(
        tx.clone(),
        tx_bytes.clone(),
        &utxo,
        &state_context_at(TIP_HEIGHT),
        Some(7),
    );

    match &outcome {
        ProcessingOutcome::Declined { reason } => {
            assert!(
                reason.contains(&(TIP_HEIGHT + 1).to_string())
                    && reason.contains(&TIP_HEIGHT.to_string()),
                "the reason should name both heights, got: {reason}"
            );
        }
        other => panic!("expected Declined, got {other:?}"),
    }

    // The load-bearing assertion: nothing was cached.
    assert!(
        !mempool.is_invalidated(&tx_id),
        "a transient creation height must not enter the invalidation cache"
    );
    assert!(
        !mempool.contains(&tx_id),
        "declined tx is neither pooled nor remembered as invalid"
    );
    assert_eq!(mempool.len(), 0);

    // One block later, the very same transaction — as a rebroadcast would
    // deliver it — is now on time.
    let outcome = mempool.process(
        tx,
        tx_bytes,
        &utxo,
        &state_context_at(TIP_HEIGHT + 1),
        Some(7),
    );

    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { tx_id: id } if id == tx_id),
        "the retry must be re-validated and accepted, got {outcome:?}"
    );
    assert_eq!(mempool.len(), 1);
}

/// A transaction at exactly the preheader height is on time and must pass
/// through 6a untouched into real validation.
#[test]
fn creation_height_at_preheader_is_accepted() {
    let mut mempool = Mempool::new(test_config());

    let input = make_box(true, 2, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let tx = spend_tx(&[input], TIP_HEIGHT);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(tx, tx_bytes, &utxo, &state_context_at(TIP_HEIGHT), Some(7));

    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { tx_id: id } if id == tx_id),
        "a transaction at the preheader height is on time, got {outcome:?}"
    );
    assert_eq!(mempool.len(), 1);
}

/// The guard must not blunt the real path: a transaction whose script cannot be
/// satisfied is still permanently invalid and is still cached as such.
#[test]
fn unsatisfiable_script_still_invalidates_and_caches() {
    let mut mempool = Mempool::new(test_config());

    // sigmaProp(false) cannot be proven, and the empty proof does not satisfy it.
    let input = make_box(false, 3, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let tx = spend_tx(&[input], TIP_HEIGHT);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(
        tx.clone(),
        tx_bytes.clone(),
        &utxo,
        &state_context_at(TIP_HEIGHT),
        Some(7),
    );

    assert!(
        matches!(outcome, ProcessingOutcome::Invalidated { .. }),
        "an unsatisfiable script is permanently invalid, got {outcome:?}"
    );
    assert!(
        mempool.is_invalidated(&tx_id),
        "a genuine failure must still be cached"
    );

    // And the cache must still short-circuit the resubmission.
    let outcome = mempool.process(tx, tx_bytes, &utxo, &state_context_at(TIP_HEIGHT), Some(7));
    assert!(
        matches!(outcome, ProcessingOutcome::Invalidated { ref reason } if reason.contains("previously invalidated")),
        "resubmission should be dropped by the cache, got {outcome:?}"
    );
}

/// A creation height with bit 31 set is "negative" under the V1 rules ergo-lib
/// still honours, so it is *not* the transient condition — it is permanently
/// malformed and must reach the invalidation cache. This pins the signed
/// comparison: an unsigned one would swallow these into the declined path and
/// re-validate the same garbage on every rebroadcast forever.
#[test]
fn negative_creation_height_is_invalidated_not_declined() {
    let mut mempool = Mempool::new(test_config());

    let input = make_box(true, 4, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let tx = spend_tx(&[input], 1 << 31);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(tx, tx_bytes, &utxo, &state_context_at(TIP_HEIGHT), Some(7));

    assert!(
        matches!(outcome, ProcessingOutcome::Invalidated { .. }),
        "a negative creation height is malformed, not early, got {outcome:?}"
    );
    assert!(
        mempool.is_invalidated(&tx_id),
        "malformed heights belong in the cache"
    );
}

/// `revalidate()` re-runs validation against the current context and caches the
/// failures, so a pooled transaction ahead of our preheader — which a reorg
/// produces, since `return_to_pool()` re-inserts without validating — would be
/// invalidated by the same transient condition. It must be left alone instead.
#[test]
fn revalidate_skips_pooled_tx_ahead_of_preheader() {
    let mut config = test_config();
    // Revalidate on the next call rather than waiting out the interval.
    config.cleanup_interval = std::time::Duration::ZERO;
    let mut mempool = Mempool::new(config);

    let input = make_box(true, 5, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    // Accepted while the preheader was at TIP_HEIGHT + 1 ...
    let tx = spend_tx(&[input], TIP_HEIGHT + 1);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
    let outcome = mempool.process(tx, tx_bytes, &utxo, &state_context_at(TIP_HEIGHT + 1), None);
    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { .. }),
        "setup: expected the tx to enter the pool, got {outcome:?}"
    );

    // ... and a reorg drops the preheader back a block underneath it.
    let removed = mempool.revalidate(&utxo, &state_context_at(TIP_HEIGHT));

    assert!(
        removed.is_empty(),
        "a transaction that is merely early must not be evicted"
    );
    assert!(
        !mempool.is_invalidated(&tx_id),
        "and it must certainly not be cached as invalid"
    );
    assert_eq!(
        mempool.len(),
        1,
        "it stays pooled until the chain catches up"
    );
}

/// The cleanup guard must not blunt cleanup either: a pooled transaction that
/// is genuinely invalid is still evicted and cached. Seeded through
/// `return_to_pool()`, which is how an unvalidated transaction really gets into
/// the pool — a reorg hands back the contents of the rolled-back blocks.
#[test]
fn revalidate_still_invalidates_genuine_failures() {
    let mut config = test_config();
    config.cleanup_interval = std::time::Duration::ZERO;
    let mut mempool = Mempool::new(config);

    // Unsatisfiable script, on time. Only the script is wrong.
    let input = make_box(false, 6, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let tx = spend_tx(&[input], TIP_HEIGHT);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
    let now = Instant::now();
    mempool.return_to_pool(vec![UnconfirmedTx {
        cost: tx_bytes.len() as u32,
        tx,
        tx_bytes,
        fee: 0,
        created: now,
        last_checked: now,
        source: None,
    }]);
    assert_eq!(mempool.len(), 1, "setup: return_to_pool skips validation");

    let removed = mempool.revalidate(&utxo, &state_context_at(TIP_HEIGHT));

    assert_eq!(removed, vec![tx_id], "a real failure is still evicted");
    assert!(
        mempool.is_invalidated(&tx_id),
        "and still cached as invalid"
    );
    assert_eq!(mempool.len(), 0);
}

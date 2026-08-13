//! Block fixtures shared by the validator tests.
//!
//! Everything here builds *real* artefacts — real `ErgoTree`s, real
//! transactions, real serialized sections — because the properties under test
//! (does the prover rewind, does `apply_state` actually evaluate) are only
//! observable when the prover really mutates and the interpreter really runs.
//! A stubbed block proves nothing.
//!
//! The scripts are the two trivial sigma propositions: `sigmaProp(true)`
//! verifies against an empty proof, `sigmaProp(false)` cannot, which gives a
//! script failure that does not depend on key material or on any particular
//! sigma-rust reduction.

use bytes::Bytes;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_chain_types::{ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, Votes};
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::input::Input;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters};
use ergo_lib::ergotree_ir::chain::tx_id::TxId;
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergotree_ir::chain::context_extension::ContextExtension;
use ergotree_ir::mir::bool_to_sigma::BoolToSigmaProp;
use ergotree_ir::mir::expr::Expr;

use crate::state_changes::{compute_state_changes, transactions_to_summaries};

/// Block heights are well past genesis so that nothing under test trips a
/// near-genesis special case (storage rent, short header windows).
pub const SEED_HEIGHT: u32 = 1_000;
pub const BLOCK_HEIGHT: u32 = SEED_HEIGHT + 1;

/// Comfortably above `minValuePerByte × box size` for these small boxes.
pub const BOX_VALUE: u64 = 1_000_000_000;

/// The header id every section in these fixtures is prefixed with.
pub const HEADER_ID: [u8; 32] = [0u8; 32];

/// `sigmaProp(<value>)` — a proposition that reduces to a trivial true or
/// false with no proof and no prover key.
pub fn sigma_bool_tree(value: bool) -> ErgoTree {
    let expr = Expr::BoolToSigmaProp(BoolToSigmaProp {
        input: Box::new(Expr::Const(value.into())),
    });
    ErgoTree::try_from(expr).expect("sigmaProp(bool) is a valid ErgoTree")
}

/// A UTXO guarded by `sigmaProp(spendable)`, distinguished from its siblings
/// by `seed` (which varies the source tx id, hence the box id, hence its key
/// in the tree).
pub fn make_box(spendable: bool, seed: u8) -> ErgoBox {
    ErgoBox::new(
        BoxValue::try_from(BOX_VALUE).expect("value above the minimum"),
        sigma_bool_tree(spendable),
        None,
        NonMandatoryRegisters::empty(),
        SEED_HEIGHT,
        TxId::from(Digest32::from([seed; 32])),
        0,
    )
    .expect("box construction")
}

pub fn box_key(ergo_box: &ErgoBox) -> [u8; 32] {
    let mut id = [0u8; 32];
    id.copy_from_slice(ergo_box.box_id().as_ref());
    id
}

pub fn serialized_box(ergo_box: &ErgoBox) -> Vec<u8> {
    ergo_box.sigma_serialize_bytes().expect("box serialization")
}

/// A transaction spending `inputs` into a single always-true output. Value is
/// preserved exactly — Ergo forbids burning ERG, and an unbalanced tx would
/// fail for a reason that has nothing to do with what these tests measure.
pub fn spend_tx(inputs: &[ErgoBox]) -> Transaction {
    let total: u64 = inputs.iter().map(|b| *b.value.as_u64()).sum();
    let output = ErgoBoxCandidate {
        value: BoxValue::try_from(total).expect("output value above the minimum"),
        ergo_tree: sigma_bool_tree(true),
        tokens: None,
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: BLOCK_HEIGHT,
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

/// The AVL operations a block of `txs` produces, in the validator's own order.
///
/// Deliberately routed through the crate's own `compute_state_changes` rather
/// than re-derived: these fixtures exist to test *when* scripts are evaluated
/// and *what* an Err leaves behind, not whether the operation ordering is
/// right — that is `state_changes.rs`'s own tests' job, and a second ordering
/// here would only produce state-root mismatches that say nothing.
pub fn block_operations(txs: &[Transaction]) -> Vec<Operation> {
    let changes = compute_state_changes(
        transactions_to_summaries(txs).expect("summaries from well-formed txs"),
    )
    .expect("state changes from non-conflicting txs");

    let mut ops = Vec::new();
    for lookup in &changes.lookups {
        ops.push(Operation::Lookup(Bytes::copy_from_slice(lookup)));
    }
    for removal in &changes.removals {
        ops.push(Operation::Remove(Bytes::copy_from_slice(removal)));
    }
    for (key, value) in &changes.insertions {
        ops.push(Operation::Insert(KeyValue {
            key: Bytes::copy_from_slice(key),
            value: Bytes::copy_from_slice(value),
        }));
    }
    ops
}

pub fn ad_digest(bytes: &Bytes) -> ADDigest {
    let mut arr = [0u8; 33];
    arr.copy_from_slice(bytes);
    ADDigest::from(arr)
}

/// A header for the block under test. `state_root` and `ad_proofs_root` are
/// caller-supplied because every test's point is which of them is right.
pub fn make_header(height: u32, state_root: ADDigest, ad_proofs_root: Digest32) -> Header {
    Header {
        version: 3,
        id: BlockId(Digest32::from(HEADER_ID)),
        parent_id: BlockId(Digest32::zero()),
        ad_proofs_root,
        state_root,
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

/// Ten preceding headers. Non-empty on purpose: `validate_transactions`
/// short-circuits to `Ok(0)` without them, which would let an "apply_state
/// rejects a bad script" test pass while evaluating nothing at all.
pub fn preceding_headers() -> Vec<Header> {
    (0..10)
        .map(|i| make_header(BLOCK_HEIGHT - 1 - i, ADDigest::zero(), Digest32::zero()))
        .collect()
}

/// Raw section bytes for a block carrying `txs`.
pub fn sections(txs: &[Transaction]) -> (Vec<u8>, Vec<u8>) {
    (
        crate::sections::serialize_block_transactions(&HEADER_ID, 3, txs)
            .expect("tx section serialization"),
        crate::sections::serialize_extension(&HEADER_ID, &[])
            .expect("extension section serialization"),
    )
}

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
use ergo_lib::ergotree_ir::ergo_tree::{ErgoTree, ErgoTreeVersion};
use ergo_lib::ergotree_ir::serialization::sigma_byte_reader::{from_bytes, SigmaByteRead};
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergotree_ir::chain::context_extension::ContextExtension;
use ergotree_ir::mir::bool_to_sigma::BoolToSigmaProp;
use ergotree_ir::mir::expr::Expr;
use sigma_ser::vlq_encode::WriteSigmaVlqExt;

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

/// A box as the UTXO set stores it: `ErgoBox::bytes()`, the value the
/// validator inserts (facts/validation.md § Insert Values).
pub fn serialized_box(ergo_box: &ErgoBox) -> Vec<u8> {
    ergo_box.bytes().expect("box serialization")
}

/// A transaction spending `inputs` into a single always-true output. Value is
/// preserved exactly — Ergo forbids burning ERG, and an unbalanced tx would
/// fail for a reason that has nothing to do with what these tests measure.
pub fn spend_tx(inputs: &[ErgoBox]) -> Transaction {
    spend_tx_to(inputs, sigma_bool_tree(true))
}

/// As [`spend_tx`], but the single output is guarded by `output_tree`.
///
/// Output scripts are never evaluated — only inputs' are — so any tree works
/// here, including the emission contract, which is what the emission-recovery
/// tests need a block to create.
pub fn spend_tx_to(inputs: &[ErgoBox], output_tree: ErgoTree) -> Transaction {
    let total: u64 = inputs.iter().map(|b| *b.value.as_u64()).sum();
    let output = ErgoBoxCandidate {
        value: BoxValue::try_from(total).expect("output value above the minimum"),
        ergo_tree: output_tree,
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
///
/// The extension carries a single dummy field so it satisfies block rule 406
/// (a non-genesis block must have at least one extension field). These
/// fixtures sit at [`BLOCK_HEIGHT`] > 1, so they exercise the same rules a
/// mainnet block does. The field key is outside the interlink range so it
/// doesn't collide with the interlink checks (401/402), and it carries no
/// meaning to any test — the block rules just need one field.
pub fn sections(txs: &[Transaction]) -> (Vec<u8>, Vec<u8>) {
    (
        crate::sections::serialize_block_transactions(&HEADER_ID, 3, txs)
            .expect("tx section serialization"),
        crate::sections::serialize_extension(&HEADER_ID, &[([0x02u8, 0x00], vec![0xff])])
            .expect("extension section serialization"),
    )
}

// ---------------------------------------------------------------------------
// Tree versions: SANTA X15, `Tuple(1, Upcast(1, Long))`.
//
// Below tree version 3 sigmastate writes an `Upcast` of a constant as the
// constant (`ValueSerializer.scala:157-169`). The fork computes a
// transaction's id at the version it read the transaction at, and writes a
// box's bytes and id at 0. So the same transaction bytes read at 0 and at 3
// are two transactions with two ids, and an output holding X15 has one byte
// image at 3 and another at 0.
//
// The transactions spend box `0…0` with an empty proof, so a transaction's id
// is the hash of its bytes as read: the `Upcast` kept at 3, written as `1` at
// 0. Every id and byte string below was computed from the wire layout, apart
// from the code under test, and matches what the fork produces.
// ---------------------------------------------------------------------------

/// One input spending box `0…0` with an empty proof and X15 as context
/// variable 0, and one 1 ERG `sigmaProp(true)` output at height 1, written at
/// tree version 3. The transaction `enr-chain`'s section tests build.
pub const EXTENSION_TX: &str =
    "01000000000000000000000000000000000000000000000000000000000000000000\
     0100860204027e0402050000018094ebdc030008d3010000";
/// [`EXTENSION_TX`] written below tree version 3: X15's `Upcast` as `1`.
pub const EXTENSION_TX_AT_0: &str =
    "01000000000000000000000000000000000000000000000000000000000000000000\
     01008602040204020000018094ebdc030008d3010000";
/// [`EXTENSION_TX`]'s id read at 3, and read at 0. `enr-chain` pins the same
/// two for the same bytes.
pub const EXTENSION_TX_ID_AT_3: &str =
    "6b11fd0ea5f1e84782c15044f03cf10df48022ba7dcb8e6c0e48233b1cf9b0a1";
pub const EXTENSION_TX_ID_AT_0: &str =
    "b006736746ec23927fecfbaeb5a27bda4df791e5440eaba24e8aadb3072b3deb";

/// [`EXTENSION_TX`] with an empty context extension, and X15 as its output's
/// R4, written at tree version 3.
pub const REGISTER_TX: &str =
    "01000000000000000000000000000000000000000000000000000000000000000000\
     000000018094ebdc030008d3010001860204027e040205";
/// [`REGISTER_TX`]'s id read at 3, and read at 0.
pub const REGISTER_TX_ID_AT_3: &str =
    "b0294d43dcd9463459545070e9a85abb7c212028adca7a3f79e3b1e10b3290ea";
pub const REGISTER_TX_ID_AT_0: &str =
    "0995636c9856a9b28b049b208d74fbcfb8f9c69664a9b4d2aa96f66f08f294b9";
/// The output of [`REGISTER_TX`] read at 3, as `ErgoBox::bytes()` writes it:
/// at 0, X15's `Upcast` as `1`, then [`REGISTER_TX_ID_AT_3`] and index 0.
/// The value the UTXO set inserts.
pub const REGISTER_OUTPUT: &str = "8094ebdc030008d3010001860204020402\
                                   b0294d43dcd9463459545070e9a85abb7c212028adca7a3f79e3b1e10b3290ea00";
/// The same output written at 3, the `Upcast` kept: `sigma_serialize_bytes()`.
pub const REGISTER_OUTPUT_AT_3: &str = "8094ebdc030008d3010001860204027e040205\
                                        b0294d43dcd9463459545070e9a85abb7c212028adca7a3f79e3b1e10b3290ea00";
/// The output's box id, the hash of [`REGISTER_OUTPUT`]: its transaction read
/// at 3. And the box id when the transaction is read at 0, which differs only
/// by the transaction id inside.
pub const REGISTER_OUTPUT_ID_AT_3: &str =
    "99ec0e43052429b24873210fa1971eb078cf90579028591279f2c6960e987ba1";
pub const REGISTER_OUTPUT_ID_AT_0: &str =
    "f22187bbb4130c471f20988713ae76a97b504cb2170115e9be41d98e7dc1b410";

/// `tx` (hex) read as one transaction at `version`, straight through the
/// fork: a `Transaction` that does not come out of the section parser.
pub fn tx_read_at(tx: &str, version: ErgoTreeVersion) -> Transaction {
    let bytes = hex::decode(tx).expect("fixture hex");
    from_bytes(&bytes)
        .with_tree_version(version, Transaction::sigma_parse)
        .expect("fixture transaction parses")
}

/// A BlockTransactions section: [`HEADER_ID`], the version framing the JVM's
/// serializer writes for `block_version` (none for version 1), the count,
/// and `txs` (hex) as they stand.
pub fn raw_section(block_version: u32, txs: &[&str]) -> Vec<u8> {
    let mut out = HEADER_ID.to_vec();
    if block_version > 1 {
        out.put_u32(10_000_000 + block_version).expect("vlq");
    }
    out.put_u32(txs.len() as u32).expect("vlq");
    for tx in txs {
        out.extend(hex::decode(tx).expect("fixture hex"));
    }
    out
}

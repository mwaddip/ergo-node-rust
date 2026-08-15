//! Fixtures shared by the `process()`-level integration tests.
//!
//! Everything here builds *real* artefacts — a real `ErgoStateContext`, real
//! `ErgoTree`s, transactions that reach ergo-lib's evaluator — because the
//! properties under test (which outcome a validation failure maps to, what
//! counts as a fee) are only observable when validation really runs. A stubbed
//! validator would let every one of these tests pass while proving nothing.
//!
//! Scripts are the two trivial sigma propositions: `sigmaProp(true)` verifies
//! against an empty proof, `sigmaProp(false)` cannot. No key material, and the
//! false one's failure does not depend on any particular sigma reduction.

#![allow(dead_code)] // each test binary uses a subset

use std::collections::HashMap;

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

use ergo_mempool::types::{MempoolConfig, UtxoReader};
use ergo_validation::{ErgoStateContext, Parameters};

/// Well past genesis, so nothing here trips a near-genesis special case.
pub const TIP_HEIGHT: u32 = 1_000;
/// Comfortably above `min_value_per_byte × box size` for these small boxes.
pub const BOX_VALUE: u64 = 1_000_000_000;

/// `sigmaProp(<value>)` — reduces to a trivial true or false with no proof and
/// no prover key.
pub fn sigma_bool_tree(value: bool) -> ErgoTree {
    let expr = Expr::BoolToSigmaProp(BoolToSigmaProp {
        input: Box::new(Expr::Const(value.into())),
    });
    ErgoTree::try_from(expr).expect("sigmaProp(bool) is a valid ErgoTree")
}

/// The tree a fee output is guarded by, at the configured reward delay — the
/// same derivation the mempool itself performs.
pub fn fee_tree() -> ErgoTree {
    ergo_lib::chain::ergo_tree_predef::fee_proposition(MempoolConfig::default().reward_delay)
        .expect("fee proposition for the default reward delay")
}

/// A UTXO guarded by `sigmaProp(spendable)`, distinguished from its siblings by
/// `seed` (which varies the source tx id, hence the box id).
pub fn make_box(spendable: bool, seed: u8, creation_height: u32) -> ErgoBox {
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
/// that has nothing to do with what these tests measure.
pub fn spend_tx(inputs: &[ErgoBox], out_creation_height: u32) -> Transaction {
    let total: u64 = inputs.iter().map(|b| *b.value.as_u64()).sum();
    spend_tx_to(
        inputs,
        &[(total, sigma_bool_tree(true))],
        out_creation_height,
    )
}

/// A transaction spending `inputs` into the given `(value, script)` outputs.
///
/// The caller is responsible for the outputs summing to the inputs: ergo-lib
/// enforces exact ERG preservation, so anything else fails validation before
/// reaching whatever the test is actually about.
pub fn spend_tx_to(
    inputs: &[ErgoBox],
    outputs: &[(u64, ErgoTree)],
    out_creation_height: u32,
) -> Transaction {
    let out_candidates: Vec<ErgoBoxCandidate> = outputs
        .iter()
        .map(|(value, ergo_tree)| ErgoBoxCandidate {
            value: BoxValue::try_from(*value).expect("output value above the minimum"),
            ergo_tree: ergo_tree.clone(),
            tokens: None,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: out_creation_height,
        })
        .collect();
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
    Transaction::new_from_vec(tx_inputs, vec![], out_candidates).expect("transaction construction")
}

pub fn make_header(height: u32) -> Header {
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
pub fn state_context_at(preheader_height: u32) -> ErgoStateContext {
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
pub struct StaticUtxo(pub HashMap<[u8; 32], ErgoBox>);

impl StaticUtxo {
    pub fn new(boxes: &[ErgoBox]) -> Self {
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

pub fn tx_id_bytes(tx: &Transaction) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr.copy_from_slice(tx.id().as_ref());
    arr
}

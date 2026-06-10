//! Transaction validation via ErgoScript evaluation.
//!
//! Validates transaction spending proofs (sigma protocols) using ergo-lib's
//! TransactionContext. This runs on top of AD proof verification — the proof
//! guarantees the state root transition, this validates that each input's
//! spending conditions are satisfied.

use std::collections::HashMap;
use std::io::Cursor;

use rayon::prelude::*;

use ergo_lib::chain::ergo_state_context::{ErgoStateContext, Headers};
use ergo_lib::chain::parameters::Parameters;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::serialization::constant_store::ConstantStore;
use ergo_lib::ergotree_ir::serialization::sigma_byte_reader::SigmaByteReader;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::wallet::tx_context::TransactionContext;
use ergo_chain_types::{Header, PreHeader};

use crate::ValidationError;

/// Deserialize an ErgoBox from raw bytes (as returned by the AVL proof verifier).
pub fn deserialize_box(bytes: &[u8]) -> Result<ErgoBox, ValidationError> {
    let cursor = Cursor::new(bytes);
    let mut reader = SigmaByteReader::new(cursor, ConstantStore::empty());
    ErgoBox::sigma_parse(&mut reader).map_err(|e| ValidationError::TransactionInvalid {
        index: 0,
        reason: format!("box deserialization: {e}"),
    })
}

/// Build an ErgoStateContext from a header and its real ≤10 preceding headers
/// (newest first), passed through unpadded.
///
/// The JVM gathers the same variable window — `headerChainBack(10, …)` stops
/// at genesis (`FullBlockProcessor:71`), so fewer than 10 headers near genesis
/// is legal chain state; padding to 10 by repeating the oldest (what this did
/// before) diverged from the reference node for scripts reading
/// `CONTEXT.headers`. ergo-lib derives `lastBlockUtxoRoot` from the newest
/// preceding header's state_root — identical to the JVM's
/// `previousStateDigest` (`ErgoStateContext.scala:92`) — with AvlTree flags
/// verified against `ErgoContext.scala:17` / `ErgoInterpreter.scala:103-106`;
/// nothing to override on our side.
///
/// Requires ≥ 1 preceding header — caller-guarded, and now also enforced by
/// the `Headers` type.
pub fn build_state_context(
    header: &Header,
    preceding_headers: &[Header],
    parameters: &Parameters,
) -> ErgoStateContext {
    let pre_header = PreHeader::from(header.clone());
    let headers = Headers::from_vec(preceding_headers.iter().take(10).cloned().collect())
        .expect("build_state_context requires at least one preceding header (caller-guarded)");
    ErgoStateContext::new(pre_header, headers, parameters.clone())
}

/// Validate a single transaction against provided input and data-input boxes.
///
/// Runs full ErgoScript evaluation via ergo-lib's TransactionContext.
/// Returns the total script evaluation cost (block cost units) on success.
pub fn validate_single_transaction(
    tx: &Transaction,
    input_boxes: Vec<ErgoBox>,
    data_boxes: Vec<ErgoBox>,
    state_context: &ErgoStateContext,
) -> Result<u64, ValidationError> {
    let tx_context = TransactionContext::new(tx.clone(), input_boxes, data_boxes)
        .map_err(|e| ValidationError::TransactionInvalid {
            index: 0,
            reason: format!("context: {e}"),
        })?;

    let cost = tx_context.validate(state_context).map_err(|e| {
        ValidationError::TransactionInvalid {
            index: 0,
            reason: format!("{e}"),
        }
    })?;

    Ok(cost)
}

/// Validate all transactions in a block using ErgoScript evaluation.
///
/// `proof_boxes`: input/data-input boxes extracted from the AD proof, keyed by box ID.
/// For intra-block spending (tx2 spends tx1's output), the box comes from
/// tx1's outputs — added to the lookup map alongside proof-returned boxes.
///
/// On success returns the block-accumulated cost: Σ of per-tx costs from
/// `validate_single_transaction`, enforced ≤ `parameters.max_block_cost()`
/// (JVM `ErgoState.execTransactions` parity — see the contract's "Block cost
/// semantics"). The degenerate guards (no transactions, no preceding
/// headers) return `Ok(0)`.
pub fn validate_transactions(
    transactions: &[Transaction],
    proof_boxes: &HashMap<[u8; 32], ErgoBox>,
    header: &Header,
    preceding_headers: &[Header],
    parameters: &Parameters,
) -> Result<u64, ValidationError> {
    if transactions.is_empty() {
        return Ok(0);
    }

    if preceding_headers.is_empty() {
        // Can't build ErgoStateContext without preceding headers.
        // Only happens at height 1 (genesis) which has no standard transactions.
        // The SDK's `Headers` type (BoundedVec<_, 1, 10>) now also enforces ≥ 1.
        tracing::warn!(height = header.height, "skipping tx validation: no preceding headers");
        return Ok(0);
    }

    let state_context = build_state_context(header, preceding_headers, parameters);

    // Box lookup: proof boxes (from UTXO set) + intra-block outputs
    let mut box_map: HashMap<[u8; 32], ErgoBox> = proof_boxes.clone();
    for tx in transactions {
        for output in tx.outputs.iter() {
            let id = box_id_bytes(&output.box_id());
            box_map.entry(id).or_insert_with(|| output.clone());
        }
    }

    let costs: Vec<u64> = transactions
        .par_iter()
        .enumerate()
        .map(|(tx_idx, tx)| {
            let input_boxes: Vec<ErgoBox> = tx
                .inputs
                .iter()
                .map(|input| {
                    let id = box_id_bytes(&input.box_id);
                    box_map.get(&id).cloned().ok_or_else(|| {
                        ValidationError::TransactionInvalid {
                            index: tx_idx,
                            reason: format!("input box {} not found", hex::encode(id)),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            let data_boxes: Vec<ErgoBox> = tx
                .data_inputs
                .as_ref()
                .map(|dis| {
                    dis.iter()
                        .map(|di| {
                            let id = box_id_bytes(&di.box_id);
                            box_map.get(&id).cloned().ok_or_else(|| {
                                ValidationError::TransactionInvalid {
                                    index: tx_idx,
                                    reason: format!("data input box {} not found", hex::encode(id)),
                                }
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();

            validate_single_transaction(tx, input_boxes, data_boxes, &state_context)
                .map_err(|e| match e {
                    ValidationError::TransactionInvalid { reason, .. } => {
                        ValidationError::TransactionInvalid { index: tx_idx, reason }
                    }
                    other => other,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // max_block_cost() is i32. Negative is unreachable through voting bounds,
    // but clamp to 0 (reject every non-empty block, the JVM's sign-preserving
    // `.toLong` verdict) rather than sign-extend into accept-everything.
    let max_cost = u64::try_from(parameters.max_block_cost()).unwrap_or(0);
    enforce_block_cost(&costs, max_cost)
}

/// Checked block-cost summation plus the maxBlockCost consensus gate.
///
/// JVM parity: `execTransactions` folds per-tx costs from 0 with `addExact`
/// and rejects once the total crosses maxBlockCost. Since per-tx costs are
/// non-negative, "final sum ≤ max" is verdict-equivalent to the JVM's
/// per-prefix check. Overflow rejects with the reported cost saturated to
/// `u64::MAX` — never wraps, never panics.
fn enforce_block_cost(costs: &[u64], max_cost: u64) -> Result<u64, ValidationError> {
    let mut total: u64 = 0;
    for &cost in costs {
        total = total
            .checked_add(cost)
            .ok_or(ValidationError::BlockCostExceeded { cost: u64::MAX, max_cost })?;
    }
    if total > max_cost {
        return Err(ValidationError::BlockCostExceeded { cost: total, max_cost });
    }
    Ok(total)
}

/// Verify spending proofs for all transactions in a block.
///
/// Pure computation — no validator state needed. Can run on any thread.
/// Uses rayon par_iter internally for intra-block parallelism.
/// On success returns the block-accumulated transaction cost (Σ per-tx
/// costs, enforced ≤ `parameters.max_block_cost()`).
pub fn evaluate_scripts(eval: &crate::DeferredEval) -> Result<u64, crate::ValidationError> {
    validate_transactions(
        &eval.transactions,
        &eval.proof_boxes,
        &eval.header,
        &eval.preceding_headers,
        &eval.parameters,
    )
}

fn box_id_bytes(box_id: &ergo_lib::ergotree_ir::chain::ergo_box::BoxId) -> [u8; 32] {
    let slice = box_id.as_ref();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(slice);
    arr
}

#[cfg(test)]
mod block_342964_tests {
    use super::*;

    use ergo_lib::chain::transaction::input::prover_result::ProverResult as ChainProverResult;
    use ergo_lib::chain::transaction::input::Input;
    use ergo_lib::chain::transaction::Transaction;
    use ergo_lib::ergotree_ir::chain::ergo_box::{
        box_value::BoxValue, ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters,
    };
    use ergo_lib::ergotree_ir::chain::tx_id::TxId;
    use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
    use ergo_chain_types::{BlockId, EcPoint, Votes};
    use ergotree_interpreter::sigma_protocol::prover::ProofBytes;
    use ergotree_ir::chain::context_extension::ContextExtension;

    // Block 342,964 — fee consolidation transaction
    //
    // tx[4] (fdb2be86...) consolidates 3 fee contract boxes into 1 miner reward output.
    // All spending proofs are empty — the script must evaluate to true on its own.
    //
    // The fee contract checks:
    //   1. HEIGHT == OUTPUTS(0).creationHeight
    //   2. OUTPUTS(0).scriptBytes == SubstConstants(template, [1], [CreateProveDlog(DecodePoint(MinerPubkey))])
    //   3. SizeOf(OUTPUTS) == 1
    //
    // sigma-rust rejects this with "Input 2 reduced to false".
    // Kushti says the block should validate without a checkpoint.

    const FEE_CONTRACT_HEX: &str = "1005040004000e36100204a00b08cd0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798ea02d192a39a8cc7a701730073011001020402d19683030193a38cc7b2a57300000193c2b2a57301007473027303830108cdeeac93b1a57304";
    const OUTPUT_TREE_HEX: &str = "100204a00b08cd02a27f37ca339c25a8ee65cbdb73fe7a7134dd89cd3e7c43e313a92c128859e4f6ea02d192a39a8cc7a70173007301";
    const MINER_PK_HEX: &str = "02a27f37ca339c25a8ee65cbdb73fe7a7134dd89cd3e7c43e313a92c128859e4f6";
    const BLOCK_HEIGHT: u32 = 342_964;

    fn make_fee_box(value: u64, creation_height: u32, src_tx_hex: &str, src_idx: u16) -> ErgoBox {
        let tree_bytes = hex::decode(FEE_CONTRACT_HEX).unwrap();
        let ergo_tree = ErgoTree::sigma_parse_bytes(&tree_bytes).unwrap();
        let tx_id_bytes: [u8; 32] = hex::decode(src_tx_hex).unwrap().try_into().unwrap();
        let tx_id = TxId::from(ergo_chain_types::Digest32::from(tx_id_bytes));
        ErgoBox::new(
            BoxValue::try_from(value).unwrap(),
            ergo_tree,
            None,
            NonMandatoryRegisters::empty(),
            creation_height,
            tx_id,
            src_idx,
        )
        .unwrap()
    }

    fn make_empty_input(box_id: ergo_lib::ergotree_ir::chain::ergo_box::BoxId) -> Input {
        Input::new(
            box_id,
            ChainProverResult {
                proof: ProofBytes::Empty,
                extension: ContextExtension::empty(),
            },
        )
    }

    fn make_pre_header() -> PreHeader {
        let miner_pk = EcPoint::from_base16_str(MINER_PK_HEX.to_string()).unwrap();
        let parent_id_bytes: [u8; 32] = hex::decode(
            "be5d64122592b6d2a07a3a619d4e68598e8df38e57ccbff732fc797bbdcf86ef",
        )
        .unwrap()
        .try_into()
        .unwrap();

        PreHeader {
            version: 1,
            parent_id: BlockId(parent_id_bytes.into()),
            timestamp: 1603134264292,
            n_bits: 118099735,
            height: BLOCK_HEIGHT,
            miner_pk: Box::new(miner_pk),
            votes: Votes([4, 3, 0]),
        }
    }

    fn make_dummy_header() -> Header {
        use ergo_chain_types::{ADDigest, AutolykosSolution, Digest32};

        let miner_pk = EcPoint::from_base16_str(
            "03163a845c33cccd5e7fe7cf8467d449cacc3c8362e29a50bbed7c4d5b4b5b1311".to_string(),
        )
        .unwrap();
        Header {
            version: 1,
            id: BlockId(Digest32::from([0u8; 32])),
            parent_id: BlockId(Digest32::from([0u8; 32])),
            ad_proofs_root: Digest32::from([0u8; 32]),
            state_root: ADDigest::from([0u8; 33]),
            transaction_root: Digest32::from([0u8; 32]),
            timestamp: 1603134202817,
            n_bits: 118099735,
            height: BLOCK_HEIGHT - 1,
            extension_root: Digest32::from([0u8; 32]),
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(miner_pk),
                pow_onetime_pk: Some(Box::new(EcPoint::default())),
                nonce: vec![0u8; 8],
                pow_distance: None,
            },
            votes: Votes([4, 0, 0]),
            unparsed_bytes: Box::new([]),
        }
    }

    /// Reproduce the block 342,964 fee consolidation tx failure.
    ///
    /// If this test fails with "ReducedToFalse", we've confirmed the sigma-rust bug.
    /// When fixed, this test should pass.
    #[test]
    fn fee_consolidation_tx_342964() {
        // 3 fee contract inputs (from explorer data)
        let input0 = make_fee_box(
            2_000_000,
            342_957,
            "d5b93c63183f4d8a8eb94e5b9a696a600eb7a76fc9afd945bc34eff138e9639f",
            1,
        );
        let input1 = make_fee_box(
            1_000_000,
            342_935,
            "9302a2983d9cc3f2b9e271097aa3128581c6cad8b59f7b6bc3e08fa6cb63ad3f",
            2,
        );
        let input2 = make_fee_box(
            1_000_000,
            342_960,
            "188e5937c9797f0a21be06a67ce7992a3bca71deb4ae9ca24ebed273b0d850bf",
            1,
        );

        // Single output — miner reward with miner's pk substituted
        let output_tree_bytes = hex::decode(OUTPUT_TREE_HEX).unwrap();
        let output_tree = ErgoTree::sigma_parse_bytes(&output_tree_bytes).unwrap();
        let output_candidate = ErgoBoxCandidate {
            value: BoxValue::try_from(4_000_000u64).unwrap(),
            ergo_tree: output_tree,
            tokens: None,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: BLOCK_HEIGHT,
        };

        // Build transaction
        let inputs = vec![
            make_empty_input(input0.box_id()),
            make_empty_input(input1.box_id()),
            make_empty_input(input2.box_id()),
        ];
        let tx = Transaction::new_from_vec(inputs, vec![], vec![output_candidate]).unwrap();

        // Build state context
        let pre_header = make_pre_header();
        let dummy_header = make_dummy_header();
        let headers = Headers::from_vec(vec![dummy_header.clone(); 10]).unwrap();
        let state_context =
            ErgoStateContext::new(pre_header, headers, Parameters::default());

        // Validate — this is where we expect the "ReducedToFalse" failure
        let input_boxes = vec![input0, input1, input2];
        let result = validate_single_transaction(&tx, input_boxes, vec![], &state_context);

        match &result {
            Ok(cost) => println!("PASS — tx validated, cost={cost}"),
            Err(e) => {
                println!("FAIL — {e}");
                // Dump diagnostics
                println!("  miner_pk: {MINER_PK_HEX}");
                println!("  height: {BLOCK_HEIGHT}");
                println!("  output ergoTree: {OUTPUT_TREE_HEX}");
            }
        }

        // Fee consolidation tx passes — the bug is in a different transaction.
        result.unwrap();
    }

    /// Test each input individually to see which ones fail.
    #[test]
    fn fee_contract_individual_inputs_342964() {
        let boxes = [
            (
                2_000_000u64,
                342_957u32,
                "d5b93c63183f4d8a8eb94e5b9a696a600eb7a76fc9afd945bc34eff138e9639f",
                1u16,
            ),
            (
                1_000_000,
                342_935,
                "9302a2983d9cc3f2b9e271097aa3128581c6cad8b59f7b6bc3e08fa6cb63ad3f",
                2,
            ),
            (
                1_000_000,
                342_960,
                "188e5937c9797f0a21be06a67ce7992a3bca71deb4ae9ca24ebed273b0d850bf",
                1,
            ),
        ];

        let output_tree_bytes = hex::decode(OUTPUT_TREE_HEX).unwrap();
        let output_tree = ErgoTree::sigma_parse_bytes(&output_tree_bytes).unwrap();

        let pre_header = make_pre_header();
        let dummy_header = make_dummy_header();
        let headers = Headers::from_vec(vec![dummy_header.clone(); 10]).unwrap();
        let state_context =
            ErgoStateContext::new(pre_header, headers, Parameters::default());

        for (i, (value, creation_height, src_tx, src_idx)) in boxes.iter().enumerate() {
            let input_box = make_fee_box(*value, *creation_height, src_tx, *src_idx);

            let output_candidate = ErgoBoxCandidate {
                value: BoxValue::try_from(*value).unwrap(),
                ergo_tree: output_tree.clone(),
                tokens: None,
                additional_registers: NonMandatoryRegisters::empty(),
                creation_height: BLOCK_HEIGHT,
            };

            let inputs = vec![make_empty_input(input_box.box_id())];
            let tx =
                Transaction::new_from_vec(inputs, vec![], vec![output_candidate]).unwrap();

            let result =
                validate_single_transaction(&tx, vec![input_box], vec![], &state_context);

            match &result {
                Ok(cost) => println!("  input[{i}]: PASS (cost={cost})"),
                Err(e) => println!("  input[{i}]: FAIL — {e}"),
            }
        }
    }

    // ---------------------------------------------------------------
    // tx[1] input[2] — P2S script with CONTEXT.selfBoxIndex
    //
    // Script (decompiled):
    //   val prop1 = proveDlog(SELF.R6[GroupElement].get)
    //   sigmaProp(
    //     (HEIGHT < SELF.R7[Int].get) && OUTPUTS.exists(
    //       {(box2: Box) =>
    //         ((box2.value >= SELF.R5[Long].get) &&
    //          (box2.R4[Int].get == CONTEXT.selfBoxIndex)) &&
    //         (box2.propositionBytes == prop1.propBytes) }
    //     )
    //   ) || prop1
    //
    // The JVM accepts this with NO spending proof — meaning the boolean
    // condition evaluates to true. But OUTPUTS[0].R4 = SInt(-1) and
    // selfBoxIndex should be 2, so the condition should be false.
    //
    // If sigma-rust evaluates the condition as false (and reduces to
    // proveDlog), but the JVM evaluates it as true, that's the divergence.
    // ---------------------------------------------------------------

    const P2S_TREE_HEX: &str = "1000d801d601cde4c6a70607eb02d1ed8fa3e4c6a70704aea5d9010263eded92c17202e4c6a7050593e4c672020404db6508fe93c27202d072017201";

    fn make_box_with_regs(
        value: u64,
        creation_height: u32,
        ergo_tree_hex: &str,
        src_tx_hex: &str,
        src_idx: u16,
        registers: NonMandatoryRegisters,
        tokens: Option<ergotree_ir::chain::ergo_box::BoxTokens>,
    ) -> ErgoBox {
        let tree_bytes = hex::decode(ergo_tree_hex).unwrap();
        let ergo_tree = ErgoTree::sigma_parse_bytes(&tree_bytes).unwrap();
        let tx_id_bytes: [u8; 32] = hex::decode(src_tx_hex).unwrap().try_into().unwrap();
        let tx_id = TxId::from(ergo_chain_types::Digest32::from(tx_id_bytes));
        ErgoBox::new(
            BoxValue::try_from(value).unwrap(),
            ergo_tree,
            tokens,
            registers,
            creation_height,
            tx_id,
            src_idx,
        )
        .unwrap()
    }

    /// Evaluate the P2S script from tx[1] input[2] and check whether the
    /// boolean condition is true (JVM behavior) or false (sigma-rust behavior).
    #[test]
    fn p2s_script_selfboxindex_342964() {
        use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisterId;
        use ergo_lib::ergotree_ir::mir::constant::Constant;
        use ergo_lib::wallet::signing::make_context;
        use ergo_lib::wallet::tx_context::TransactionContext;
        use ergotree_interpreter::eval::reduce_to_crypto;
        use ergotree_ir::sigma_protocol::sigma_boolean::SigmaBoolean;

        // --- Input boxes for tx[1] ---
        // input[0]: P2PK, 10 ERG
        let input0 = make_box_with_regs(
            10_000_000_000,
            342_704,
            "0008cd02d84a11191f434daa5bed70e0e4db4e1563910622ee269f3dc219e0e854e108a5",
            "08c97e58d0ffc6356b31230b2c69ceb2e1a6883dcdde22cd1d41d5102e9e2503",
            0,
            NonMandatoryRegisters::empty(),
            None,
        );
        // input[1]: P2PK, 100 ERG
        let input1 = make_box_with_regs(
            100_000_000_000,
            342_717,
            "0008cd02d84a11191f434daa5bed70e0e4db4e1563910622ee269f3dc219e0e854e108a5",
            "034c3ac56efe249edcf88dbbf974531596848a1c48aa39e17948689d5e78c877",
            0,
            NonMandatoryRegisters::empty(),
            None,
        );
        // input[2]: P2S script, 0.001 ERG, with token and registers R4-R7
        let r4_bytes = hex::decode("73b6c8cfa0ef80096cb7127fa0f943acb22053e87f77e824b4d2749ffe0336d2").unwrap();
        let r6_pk = EcPoint::from_base16_str(
            "0355e3409b35892e2b916a6362a93f742d06ce1726e2eaa688738b34b652d1142a".to_string(),
        )
        .unwrap();

        let p2s_regs = NonMandatoryRegisters::new(vec![
            (NonMandatoryRegisterId::R4, Constant::from(r4_bytes)),
            (NonMandatoryRegisterId::R5, Constant::from(100_000_000_000i64)),
            (NonMandatoryRegisterId::R6, Constant::from(r6_pk)),
            (NonMandatoryRegisterId::R7, Constant::from(350_000i32)),
        ])
        .unwrap();

        let token_id_bytes: [u8; 32] =
            hex::decode("f9230aa721f97a319d91c6b701742403fcb4a8e069c9172d9e3370f3fcd01f47")
                .unwrap()
                .try_into()
                .unwrap();
        let token_id = ergo_lib::ergotree_ir::chain::token::TokenId::from(
            ergo_chain_types::Digest32::from(token_id_bytes),
        );
        let token_amount =
            ergo_lib::ergotree_ir::chain::token::TokenAmount::try_from(4_294_967_296u64).unwrap();
        let token = ergo_lib::ergotree_ir::chain::token::Token {
            token_id,
            amount: token_amount,
        };
        let tokens =
            Some(ergotree_ir::chain::ergo_box::BoxTokens::from_vec(vec![token]).unwrap());

        let input2 = make_box_with_regs(
            1_000_000,
            342_935,
            P2S_TREE_HEX,
            "e35caa4c6e257053381b1cd7b453bac84c7064a6b955c07ff02a57ea2c61a703",
            0,
            p2s_regs,
            tokens.clone(),
        );

        // --- Output candidates for tx[1] ---
        // output[0]: 100 ERG, P2PK(0355e3...), R4=SInt(-1)
        let out0_regs = NonMandatoryRegisters::new(vec![(
            NonMandatoryRegisterId::R4,
            Constant::from(-1i32),
        )])
        .unwrap();
        let out0 = ErgoBoxCandidate {
            value: BoxValue::try_from(100_000_000_000u64).unwrap(),
            ergo_tree: ErgoTree::sigma_parse_bytes(
                &hex::decode(
                    "0008cd0355e3409b35892e2b916a6362a93f742d06ce1726e2eaa688738b34b652d1142a",
                )
                .unwrap(),
            )
            .unwrap(),
            tokens: None,
            additional_registers: out0_regs,
            creation_height: 342_935,
        };
        // output[1]: 10 ERG, P2PK, with token
        let out1 = ErgoBoxCandidate {
            value: BoxValue::try_from(10_000_000_000u64).unwrap(),
            ergo_tree: ErgoTree::sigma_parse_bytes(
                &hex::decode(
                    "0008cd02d84a11191f434daa5bed70e0e4db4e1563910622ee269f3dc219e0e854e108a5",
                )
                .unwrap(),
            )
            .unwrap(),
            tokens,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: 342_935,
        };
        // output[2]: 0.001 ERG, fee contract
        let out2 = ErgoBoxCandidate {
            value: BoxValue::try_from(1_000_000u64).unwrap(),
            ergo_tree: ErgoTree::sigma_parse_bytes(&hex::decode(FEE_CONTRACT_HEX).unwrap())
                .unwrap(),
            tokens: None,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: 342_935,
        };

        // Build transaction (all inputs with empty proofs — we only test script eval)
        let inputs = vec![
            make_empty_input(input0.box_id()),
            make_empty_input(input1.box_id()),
            make_empty_input(input2.box_id()),
        ];
        let tx = Transaction::new_from_vec(inputs, vec![], vec![out0, out1, out2]).unwrap();

        // Build state context
        let pre_header = make_pre_header();
        let dummy_header = make_dummy_header();
        let headers = Headers::from_vec(vec![dummy_header.clone(); 10]).unwrap();
        let state_context =
            ErgoStateContext::new(pre_header, headers, Parameters::default());

        // Build TransactionContext + evaluation context for input[2]
        let tx_context =
            TransactionContext::new(tx.clone(), vec![input0, input1, input2], vec![]).unwrap();
        let ctx = make_context(&state_context, &tx_context, 2).unwrap();

        // Evaluate the P2S script
        let p2s_tree = ErgoTree::sigma_parse_bytes(&hex::decode(P2S_TREE_HEX).unwrap()).unwrap();
        let result = reduce_to_crypto(&p2s_tree, &ctx);

        match &result {
            Ok(rr) => {
                match &rr.sigma_prop {
                    SigmaBoolean::TrivialProp(true) => {
                        println!("PASS — script reduces to TrivialProp(true) — no proof needed");
                        println!("  This matches JVM behavior.");
                    }
                    SigmaBoolean::TrivialProp(false) => {
                        println!("BUG — script reduces to TrivialProp(false)");
                        println!("  JVM evaluates this as true. sigma-rust divergence!");
                    }
                    other => {
                        println!("SIGMA — script reduces to: {:?}", other);
                        println!("  If this is ProveDlog, it means the boolean condition is false");
                        println!("  and the fallback path (needing a proof) is taken.");
                        println!("  JVM accepts this without a proof, so this is a divergence.");
                    }
                }
            }
            Err(e) => {
                println!("ERROR — script evaluation failed: {e}");
                println!("  This is an evaluation error, not a 'reduced to false'.");
            }
        }

        // The JVM accepts this with no proof — selfBoxIndex returned -1 in pre-JIT mode
        // (v4.x bug, https://github.com/ScorexFoundation/sigmastate-interpreter/issues/603).
        // sigma-rust must match: return -1 for activated_script_version < V2.
        let rr = result.expect("script evaluation should not error");
        assert!(
            matches!(rr.sigma_prop, SigmaBoolean::TrivialProp(true)),
            "Expected TrivialProp(true) for pre-JIT selfBoxIndex=-1, got {:?}",
            rr.sigma_prop
        );
    }

    /// Diagnostic: check what selfBoxIndex returns and what OUTPUTS[0].R4 is.
    #[test]
    fn diagnose_selfboxindex_vs_r4() {
        use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisterId;
        use ergo_lib::ergotree_ir::mir::constant::Constant;
        use ergo_lib::wallet::signing::make_context;
        use ergo_lib::wallet::tx_context::TransactionContext;
        // (no extra eval imports needed for this diagnostic test)

        // Build same context as p2s_script_selfboxindex_342964
        let input0 = make_box_with_regs(
            10_000_000_000, 342_704,
            "0008cd02d84a11191f434daa5bed70e0e4db4e1563910622ee269f3dc219e0e854e108a5",
            "08c97e58d0ffc6356b31230b2c69ceb2e1a6883dcdde22cd1d41d5102e9e2503",
            0, NonMandatoryRegisters::empty(), None,
        );
        let input1 = make_box_with_regs(
            100_000_000_000, 342_717,
            "0008cd02d84a11191f434daa5bed70e0e4db4e1563910622ee269f3dc219e0e854e108a5",
            "034c3ac56efe249edcf88dbbf974531596848a1c48aa39e17948689d5e78c877",
            0, NonMandatoryRegisters::empty(), None,
        );

        let r4_bytes = hex::decode("73b6c8cfa0ef80096cb7127fa0f943acb22053e87f77e824b4d2749ffe0336d2").unwrap();
        let r6_pk = EcPoint::from_base16_str(
            "0355e3409b35892e2b916a6362a93f742d06ce1726e2eaa688738b34b652d1142a".to_string(),
        ).unwrap();
        let p2s_regs = NonMandatoryRegisters::new(vec![
            (NonMandatoryRegisterId::R4, Constant::from(r4_bytes)),
            (NonMandatoryRegisterId::R5, Constant::from(100_000_000_000i64)),
            (NonMandatoryRegisterId::R6, Constant::from(r6_pk)),
            (NonMandatoryRegisterId::R7, Constant::from(350_000i32)),
        ]).unwrap();

        let token_id_bytes: [u8; 32] = hex::decode("f9230aa721f97a319d91c6b701742403fcb4a8e069c9172d9e3370f3fcd01f47").unwrap().try_into().unwrap();
        let token_id = ergo_lib::ergotree_ir::chain::token::TokenId::from(ergo_chain_types::Digest32::from(token_id_bytes));
        let token_amount = ergo_lib::ergotree_ir::chain::token::TokenAmount::try_from(4_294_967_296u64).unwrap();
        let token = ergo_lib::ergotree_ir::chain::token::Token { token_id, amount: token_amount };
        let tokens = Some(ergotree_ir::chain::ergo_box::BoxTokens::from_vec(vec![token]).unwrap());

        let input2 = make_box_with_regs(
            1_000_000, 342_935, P2S_TREE_HEX,
            "e35caa4c6e257053381b1cd7b453bac84c7064a6b955c07ff02a57ea2c61a703",
            0, p2s_regs, tokens.clone(),
        );

        // Output[0] with R4=SInt(-1)
        let out0_regs = NonMandatoryRegisters::new(vec![
            (NonMandatoryRegisterId::R4, Constant::from(-1i32)),
        ]).unwrap();
        let out0 = ErgoBoxCandidate {
            value: BoxValue::try_from(100_000_000_000u64).unwrap(),
            ergo_tree: ErgoTree::sigma_parse_bytes(&hex::decode(
                "0008cd0355e3409b35892e2b916a6362a93f742d06ce1726e2eaa688738b34b652d1142a"
            ).unwrap()).unwrap(),
            tokens: None,
            additional_registers: out0_regs,
            creation_height: 342_935,
        };
        let out1 = ErgoBoxCandidate {
            value: BoxValue::try_from(10_000_000_000u64).unwrap(),
            ergo_tree: ErgoTree::sigma_parse_bytes(&hex::decode(
                "0008cd02d84a11191f434daa5bed70e0e4db4e1563910622ee269f3dc219e0e854e108a5"
            ).unwrap()).unwrap(),
            tokens,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: 342_935,
        };
        let out2 = ErgoBoxCandidate {
            value: BoxValue::try_from(1_000_000u64).unwrap(),
            ergo_tree: ErgoTree::sigma_parse_bytes(&hex::decode(FEE_CONTRACT_HEX).unwrap()).unwrap(),
            tokens: None,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: 342_935,
        };

        let inputs = vec![
            make_empty_input(input0.box_id()),
            make_empty_input(input1.box_id()),
            make_empty_input(input2.box_id()),
        ];
        let tx = Transaction::new_from_vec(inputs, vec![], vec![out0, out1, out2]).unwrap();

        let pre_header = make_pre_header();
        let dummy_header = make_dummy_header();
        let headers = Headers::from_vec(vec![dummy_header.clone(); 10]).unwrap();
        let state_context = ErgoStateContext::new(pre_header, headers, Parameters::default());

        let tx_context = TransactionContext::new(tx.clone(), vec![input0, input1, input2], vec![]).unwrap();
        let ctx = make_context(&state_context, &tx_context, 2).unwrap();

        // Check selfBoxIndex by finding self_box position in inputs
        let self_box_idx = ctx.inputs.iter().position(|it| *it == ctx.self_box);
        println!("selfBoxIndex for input[2]: {:?}", self_box_idx);

        // Check output[0].R4
        let out0_box = &ctx.outputs[0];
        let r4_val = out0_box.additional_registers.get(NonMandatoryRegisterId::R4);
        println!("output[0].R4: {:?}", r4_val);

        // Check: do propositionBytes match?
        let out0_script_bytes = out0_box.ergo_tree.sigma_serialize_bytes().unwrap();
        let pk_hex = "0355e3409b35892e2b916a6362a93f742d06ce1726e2eaa688738b34b652d1142a";
        let expected_p2pk = format!("0008cd{}", pk_hex);
        let expected_bytes = hex::decode(&expected_p2pk).unwrap();
        println!("output[0] ergoTree bytes: {}", hex::encode(&out0_script_bytes));
        println!("expected proveDlog bytes: {}", expected_p2pk);
        println!("propositionBytes match: {}", out0_script_bytes == expected_bytes);

        // Now try evaluating simpler scripts to isolate the failing condition
        // Test: HEIGHT < SELF.R7
        println!("\nHEIGHT = {}", ctx.height);
        println!("SELF.R7 should be 350000");
        println!("HEIGHT < R7 = {} < 350000 = {}", ctx.height, ctx.height < 350_000);

        // The real question: what does CONTEXT.selfBoxIndex evaluate to?
        // And does output[0].R4[Int] == selfBoxIndex?
        println!("\nKey comparison: output[0].R4 = {:?} vs selfBoxIndex = {:?}",
            r4_val, self_box_idx);

        // Verify R4 by constructing output[0] with the REAL tx_id and checking box_id
        let real_tx_id_bytes: [u8; 32] = hex::decode(
            "9302a2983d9cc3f2b9e271097aa3128581c6cad8b59f7b6bc3e08fa6cb63ad3f"
        ).unwrap().try_into().unwrap();
        let real_tx_id = TxId::from(ergo_chain_types::Digest32::from(real_tx_id_bytes));

        // Construct output[0] as ErgoBox with R4=SInt(-1)
        let out0_with_r4 = ErgoBox::new(
            BoxValue::try_from(100_000_000_000u64).unwrap(),
            ErgoTree::sigma_parse_bytes(&hex::decode(
                "0008cd0355e3409b35892e2b916a6362a93f742d06ce1726e2eaa688738b34b652d1142a"
            ).unwrap()).unwrap(),
            None,
            NonMandatoryRegisters::new(vec![
                (NonMandatoryRegisterId::R4, Constant::from(-1i32)),
            ]).unwrap(),
            342_935,
            real_tx_id,
            0,
        ).unwrap();

        let computed_box_id = hex::encode(out0_with_r4.box_id().as_ref());
        let expected_box_id = "a384930961967f4112d71445feebe000706a064a5c467b483f3d5b982b52a502";
        println!("\nBox ID verification:");
        println!("  computed (R4=-1): {}", computed_box_id);
        println!("  expected:         {}", expected_box_id);
        println!("  match: {}", computed_box_id == expected_box_id);

        if computed_box_id != expected_box_id {
            // Try R4=SInt(0)
            let out0_r4_zero = ErgoBox::new(
                BoxValue::try_from(100_000_000_000u64).unwrap(),
                ErgoTree::sigma_parse_bytes(&hex::decode(
                    "0008cd0355e3409b35892e2b916a6362a93f742d06ce1726e2eaa688738b34b652d1142a"
                ).unwrap()).unwrap(),
                None,
                NonMandatoryRegisters::new(vec![
                    (NonMandatoryRegisterId::R4, Constant::from(0i32)),
                ]).unwrap(),
                342_935,
                real_tx_id,
                0,
            ).unwrap();
            println!("  computed (R4=0):  {}", hex::encode(out0_r4_zero.box_id().as_ref()));

            // Try R4=SInt(2) — would match selfBoxIndex
            let out0_r4_two = ErgoBox::new(
                BoxValue::try_from(100_000_000_000u64).unwrap(),
                ErgoTree::sigma_parse_bytes(&hex::decode(
                    "0008cd0355e3409b35892e2b916a6362a93f742d06ce1726e2eaa688738b34b652d1142a"
                ).unwrap()).unwrap(),
                None,
                NonMandatoryRegisters::new(vec![
                    (NonMandatoryRegisterId::R4, Constant::from(2i32)),
                ]).unwrap(),
                342_935,
                real_tx_id,
                0,
            ).unwrap();
            println!("  computed (R4=2):  {}", hex::encode(out0_r4_two.box_id().as_ref()));
        }
    }
}

#[cfg(test)]
mod state_context_window_tests {
    use super::*;

    use ergo_chain_types::{
        ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Votes,
    };

    fn header_at(height: u32) -> Header {
        Header {
            version: 1,
            id: BlockId(Digest32::from([height as u8; 32])),
            parent_id: BlockId(Digest32::from([0u8; 32])),
            ad_proofs_root: Digest32::from([0u8; 32]),
            state_root: ADDigest::from([height as u8; 33]),
            transaction_root: Digest32::from([0u8; 32]),
            timestamp: 0,
            n_bits: 118_099_735,
            height,
            extension_root: Digest32::from([0u8; 32]),
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

    /// A real < 10 window passes through unpadded. The pre-fix code padded the
    /// window to 10 by repeating the oldest header, diverging from the JVM's
    /// variable window near genesis for scripts reading `CONTEXT.headers`.
    #[test]
    fn short_window_passes_through_unpadded() {
        let header = header_at(4);
        // Real window at height 4: parents at heights 3, 2, 1 — newest first.
        let preceding = [header_at(3), header_at(2), header_at(1)];
        let ctx = build_state_context(&header, &preceding, &Parameters::default());

        assert_eq!(ctx.headers.len(), 3, "window must not be padded to 10");
        assert_eq!(ctx.headers.first().height, 3, "parent header comes first");
        assert_eq!(ctx.headers.as_vec()[1].height, 2);
        assert_eq!(ctx.headers.last().height, 1);
    }

    /// Steady state: a full 10-header window passes through as-is.
    #[test]
    fn full_window_passes_through_identically() {
        let header = header_at(20);
        // Heights 19 down to 10, newest first.
        let preceding: Vec<Header> = (10..20).rev().map(header_at).collect();
        let ctx = build_state_context(&header, &preceding, &Parameters::default());

        assert_eq!(ctx.headers.len(), 10);
        assert_eq!(ctx.headers.first().height, 19);
        assert_eq!(ctx.headers.last().height, 10);
    }
}

#[cfg(test)]
mod block_cost_tests {
    use super::*;

    use ergo_chain_types::{ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Votes};
    use ergo_lib::chain::parameters::Parameter;
    use ergo_lib::chain::transaction::input::prover_result::ProverResult as ChainProverResult;
    use ergo_lib::chain::transaction::input::Input;
    use ergo_lib::ergotree_ir::chain::ergo_box::{
        box_value::BoxValue, ErgoBoxCandidate, NonMandatoryRegisters,
    };
    use ergo_lib::ergotree_ir::chain::tx_id::TxId;
    use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
    use ergotree_interpreter::sigma_protocol::prover::ProofBytes;
    use ergotree_ir::chain::context_extension::ContextExtension;

    // Fee-contract spend fixtures from block 342,964 (same vectors as
    // block_342964_tests): one fee box in, one substituted miner-reward box
    // out, empty proof — the script reduces to true on its own, giving a
    // cheap deterministic per-tx cost.
    const FEE_CONTRACT_HEX: &str = "1005040004000e36100204a00b08cd0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798ea02d192a39a8cc7a701730073011001020402d19683030193a38cc7b2a57300000193c2b2a57301007473027303830108cdeeac93b1a57304";
    const OUTPUT_TREE_HEX: &str = "100204a00b08cd02a27f37ca339c25a8ee65cbdb73fe7a7134dd89cd3e7c43e313a92c128859e4f6ea02d192a39a8cc7a70173007301";
    const MINER_PK_HEX: &str = "02a27f37ca339c25a8ee65cbdb73fe7a7134dd89cd3e7c43e313a92c128859e4f6";
    const BLOCK_HEIGHT: u32 = 342_964;

    fn fee_box(seed: u8, value: u64) -> ErgoBox {
        let tree = ErgoTree::sigma_parse_bytes(&hex::decode(FEE_CONTRACT_HEX).unwrap()).unwrap();
        let tx_id = TxId::from(Digest32::from([seed; 32]));
        ErgoBox::new(
            BoxValue::try_from(value).unwrap(),
            tree,
            None,
            NonMandatoryRegisters::empty(),
            342_900,
            tx_id,
            0,
        )
        .unwrap()
    }

    fn fee_spend_tx(src: &ErgoBox) -> Transaction {
        let output_tree =
            ErgoTree::sigma_parse_bytes(&hex::decode(OUTPUT_TREE_HEX).unwrap()).unwrap();
        let output = ErgoBoxCandidate {
            value: src.value,
            ergo_tree: output_tree,
            tokens: None,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: BLOCK_HEIGHT,
        };
        let input = Input::new(
            src.box_id(),
            ChainProverResult {
                proof: ProofBytes::Empty,
                extension: ContextExtension::empty(),
            },
        );
        Transaction::new_from_vec(vec![input], vec![], vec![output]).unwrap()
    }

    /// Header for block 342,964 — `PreHeader::from(header)` must carry the
    /// miner_pk/height the fee contract's SubstConstants check reads.
    fn block_header() -> Header {
        let miner_pk = EcPoint::from_base16_str(MINER_PK_HEX.to_string()).unwrap();
        let parent_id_bytes: [u8; 32] = hex::decode(
            "be5d64122592b6d2a07a3a619d4e68598e8df38e57ccbff732fc797bbdcf86ef",
        )
        .unwrap()
        .try_into()
        .unwrap();
        Header {
            version: 1,
            id: BlockId(Digest32::from([0u8; 32])),
            parent_id: BlockId(parent_id_bytes.into()),
            ad_proofs_root: Digest32::from([0u8; 32]),
            state_root: ADDigest::from([0u8; 33]),
            transaction_root: Digest32::from([0u8; 32]),
            timestamp: 1603134264292,
            n_bits: 118099735,
            height: BLOCK_HEIGHT,
            extension_root: Digest32::from([0u8; 32]),
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(miner_pk),
                pow_onetime_pk: None,
                nonce: vec![0u8; 8],
                pow_distance: None,
            },
            votes: Votes([4, 3, 0]),
            unparsed_bytes: Box::new([]),
        }
    }

    fn parent_header() -> Header {
        Header {
            version: 1,
            id: BlockId(Digest32::from([1u8; 32])),
            parent_id: BlockId(Digest32::from([0u8; 32])),
            ad_proofs_root: Digest32::from([0u8; 32]),
            state_root: ADDigest::from([0u8; 33]),
            transaction_root: Digest32::from([0u8; 32]),
            timestamp: 1603134202817,
            n_bits: 118099735,
            height: BLOCK_HEIGHT - 1,
            extension_root: Digest32::from([0u8; 32]),
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: vec![0u8; 8],
                pow_distance: None,
            },
            votes: Votes([4, 0, 0]),
            unparsed_bytes: Box::new([]),
        }
    }

    fn params_with_max_cost(max: i32) -> Parameters {
        let mut params = Parameters::default();
        params.parameters_table.insert(Parameter::MaxBlockCost, max);
        params
    }

    fn proof_box_map(boxes: &[ErgoBox]) -> HashMap<[u8; 32], ErgoBox> {
        boxes
            .iter()
            .map(|b| (box_id_bytes(&b.box_id()), b.clone()))
            .collect()
    }

    /// Accept path: the returned block cost is the sum of the per-tx costs
    /// `validate_single_transaction` reports on identical fixtures.
    #[test]
    fn accept_path_returns_per_tx_cost_sum() {
        let box_a = fee_box(0xA1, 2_000_000);
        let box_b = fee_box(0xB2, 1_000_000);
        let tx_a = fee_spend_tx(&box_a);
        let tx_b = fee_spend_tx(&box_b);
        let header = block_header();
        let preceding = vec![parent_header()];
        let params = Parameters::default();

        let ctx = build_state_context(&header, &preceding, &params);
        let cost_a =
            validate_single_transaction(&tx_a, vec![box_a.clone()], vec![], &ctx).unwrap();
        let cost_b =
            validate_single_transaction(&tx_b, vec![box_b.clone()], vec![], &ctx).unwrap();
        assert!(cost_a > 0 && cost_b > 0, "fixture txs must have nonzero cost");

        let total = validate_transactions(
            &[tx_a, tx_b],
            &proof_box_map(&[box_a, box_b]),
            &header,
            &preceding,
            &params,
        )
        .unwrap();
        assert_eq!(total, cost_a + cost_b);
    }

    /// The consensus gap this closes (fork direction): every tx individually
    /// passes under a shrunk MaxBlockCost (each gets the 10× per-tx JIT
    /// budget), but the block-level sum crosses it — JVM rejects, so must we.
    #[test]
    fn block_sum_over_max_block_cost_rejects() {
        let box_a = fee_box(0xA1, 2_000_000);
        let box_b = fee_box(0xB2, 1_000_000);
        let tx_a = fee_spend_tx(&box_a);
        let tx_b = fee_spend_tx(&box_b);
        let header = block_header();
        let preceding = vec![parent_header()];

        // Shrink MaxBlockCost to one tx's cost (+1 block unit: the block-scale
        // cost floors the JIT-scale spend /10, so a limit of exactly 10×C JIT
        // units trips on the rounding residue): each tx fits, the pair doesn't.
        // Per-tx cost itself is independent of MaxBlockCost — it only sets limits.
        let baseline_ctx = build_state_context(&header, &preceding, &Parameters::default());
        let cost_a = validate_single_transaction(&tx_a, vec![box_a.clone()], vec![], &baseline_ctx)
            .unwrap();
        let cost_b = validate_single_transaction(&tx_b, vec![box_b.clone()], vec![], &baseline_ctx)
            .unwrap();
        let max = cost_a.max(cost_b) + 1;
        let params = params_with_max_cost(i32::try_from(max).unwrap());

        // Each tx individually passes under the shrunk parameters.
        let shrunk_ctx = build_state_context(&header, &preceding, &params);
        validate_single_transaction(&tx_a, vec![box_a.clone()], vec![], &shrunk_ctx).unwrap();
        validate_single_transaction(&tx_b, vec![box_b.clone()], vec![], &shrunk_ctx).unwrap();

        let err = validate_transactions(
            &[tx_a, tx_b],
            &proof_box_map(&[box_a, box_b]),
            &header,
            &preceding,
            &params,
        )
        .unwrap_err();
        match err {
            ValidationError::BlockCostExceeded { cost, max_cost } => {
                assert_eq!(cost, cost_a + cost_b);
                assert_eq!(max_cost, max);
            }
            other => panic!("expected BlockCostExceeded, got: {other}"),
        }
    }

    /// Empty-tx block and the no-preceding-headers guard both report cost 0.
    #[test]
    fn degenerate_blocks_cost_zero() {
        let header = block_header();
        let params = Parameters::default();

        let empty =
            validate_transactions(&[], &HashMap::new(), &header, &[parent_header()], &params)
                .unwrap();
        assert_eq!(empty, 0);

        // Non-empty txs but no preceding headers (height-1 shape).
        let bx = fee_box(0xC3, 1_000_000);
        let tx = fee_spend_tx(&bx);
        let no_headers =
            validate_transactions(&[tx], &proof_box_map(&[bx]), &header, &[], &params).unwrap();
        assert_eq!(no_headers, 0);
    }

    /// Direct overflow coverage for the checked summation — constructing
    /// real overflow-cost transactions is impractical.
    #[test]
    fn enforce_block_cost_overflow_rejects_saturated() {
        match enforce_block_cost(&[u64::MAX, 1], 1_000_000).unwrap_err() {
            ValidationError::BlockCostExceeded { cost, max_cost } => {
                assert_eq!(cost, u64::MAX, "overflow reports saturated cost");
                assert_eq!(max_cost, 1_000_000);
            }
            other => panic!("expected BlockCostExceeded, got: {other}"),
        }
    }

    /// Boundary semantics: sum == max accepts (JVM `maxCost >= startCost`),
    /// one over rejects, empty sums to zero.
    #[test]
    fn enforce_block_cost_boundary() {
        assert_eq!(enforce_block_cost(&[], 0).unwrap(), 0);
        assert_eq!(enforce_block_cost(&[5, 6], 11).unwrap(), 11);
        match enforce_block_cost(&[5, 7], 11).unwrap_err() {
            ValidationError::BlockCostExceeded { cost, max_cost } => {
                assert_eq!(cost, 12);
                assert_eq!(max_cost, 11);
            }
            other => panic!("expected BlockCostExceeded, got: {other}"),
        }
    }
}

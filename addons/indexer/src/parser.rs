use anyhow::{Context, Result};
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};
use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisterId;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::mir::constant::TryExtractInto;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use crate::node_client::{BlockTransactionsJson, HeaderJson};
use crate::types::*;

type Blake2b256 = Blake2b<U32>;

fn blake2b256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Parse header + transactions into an IndexedBlock.
pub fn parse_block(
    header: &HeaderJson,
    txs_json: &BlockTransactionsJson,
    network: NetworkPrefix,
) -> Result<IndexedBlock> {
    let header_id = hex_to_bytes32(&header.id).context("invalid header ID hex")?;
    let miner_pk = hex::decode(&header.pow_solutions.pk).context("invalid miner pk hex")?;
    let encoder = AddressEncoder::new(network);

    let mut transactions = Vec::with_capacity(txs_json.transactions.len());
    let mut block_size: u32 = 0;

    for (tx_index, tx_val) in txs_json.transactions.iter().enumerate() {
        let tx: Transaction = serde_json::from_value(tx_val.clone())
            .with_context(|| format!("failed to parse tx at index {tx_index}"))?;

        let tx_bytes = tx
            .sigma_serialize_bytes()
            .with_context(|| format!("failed to serialize tx at index {tx_index}"))?;
        let tx_size = tx_bytes.len() as u32;
        block_size += tx_size;

        let tx_id_val = tx.id();
        let tx_id_ref: &[u8] = tx_id_val.as_ref();
        let tx_id: [u8; 32] = tx_id_ref
            .try_into()
            .with_context(|| format!("tx_id not 32 bytes at index {tx_index}"))?;

        // Parse inputs
        let inputs: Vec<InputRef> = tx
            .inputs
            .iter()
            .map(|input| {
                let b: &[u8] = input.box_id.as_ref();
                let box_id: [u8; 32] = b.try_into().expect("BoxId should be 32 bytes");
                InputRef { box_id }
            })
            .collect();

        // First input box ID — used for token minting detection
        // TxIoVec has min=1, so first() always exists
        let first_input_id: [u8; 32] = {
            let b: &[u8] = tx.inputs.first().box_id.as_ref();
            b.try_into().expect("BoxId should be 32 bytes")
        };

        // Parse outputs
        let mut outputs = Vec::with_capacity(tx.outputs.len());
        for (out_idx, ergo_box) in tx.outputs.iter().enumerate() {
            let bid_val = ergo_box.box_id();
            let bid: &[u8] = bid_val.as_ref();
            let box_id: [u8; 32] = bid.try_into().expect("BoxId should be 32 bytes");
            let ergo_tree_bytes = ergo_box.ergo_tree.sigma_serialize_bytes().unwrap_or_default();
            let ergo_tree_hash = blake2b256(&ergo_tree_bytes);

            let address = match Address::recreate_from_ergo_tree(&ergo_box.ergo_tree) {
                Ok(addr) => encoder.address_to_str(&addr),
                Err(_) => hex::encode(&ergo_tree_bytes),
            };

            // Boxes may list the same token_id multiple times; the DB PK is
            // (box_id, token_id) so we sum amounts per token to match explorer
            // semantics and avoid constraint violations.
            let tokens: Vec<IndexedToken> = ergo_box
                .tokens
                .as_ref()
                .map(|toks| {
                    let mut deduped: Vec<IndexedToken> = Vec::with_capacity(toks.len());
                    for t in toks.iter() {
                        let tid: &[u8] = t.token_id.as_ref();
                        let token_id: [u8; 32] =
                            tid.try_into().expect("TokenId should be 32 bytes");
                        let amount = *t.amount.as_u64();
                        match deduped.iter_mut().find(|e| e.token_id == token_id) {
                            Some(existing) => {
                                existing.amount = existing.amount.saturating_add(amount);
                            }
                            None => deduped.push(IndexedToken { token_id, amount }),
                        }
                    }
                    deduped
                })
                .unwrap_or_default();

            // Token minting: token_id == first input's box_id
            let minted_token =
                tokens
                    .iter()
                    .find(|t| t.token_id == first_input_id)
                    .map(|_| {
                        let name = extract_register_string(ergo_box, NonMandatoryRegisterId::R4);
                        let description =
                            extract_register_string(ergo_box, NonMandatoryRegisterId::R5);
                        let decimals =
                            extract_register_int(ergo_box, NonMandatoryRegisterId::R6);
                        MintedToken {
                            token_id: first_input_id,
                            name,
                            description,
                            decimals,
                        }
                    });

            let registers = extract_registers(ergo_box);

            outputs.push(IndexedBox {
                box_id,
                output_index: out_idx as u32,
                ergo_tree: ergo_tree_bytes,
                ergo_tree_hash,
                address,
                value: *ergo_box.value.as_u64(),
                tokens,
                registers,
                minted_token,
            });
        }

        transactions.push(IndexedTx {
            tx_id,
            tx_index: tx_index as u32,
            size: tx_size,
            inputs,
            outputs,
        });
    }

    Ok(IndexedBlock {
        height: header.height,
        header_id,
        timestamp: header.timestamp,
        difficulty: header.n_bits,
        miner_pk,
        block_size,
        transactions,
    })
}

fn hex_to_bytes32(hex_str: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str)?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 32 bytes"))?;
    Ok(arr)
}

/// Try to extract a UTF-8 string from a register (expects Coll[Byte]).
fn extract_register_string(ergo_box: &ErgoBox, reg_id: NonMandatoryRegisterId) -> Option<String> {
    let constant = ergo_box.additional_registers.get_constant(reg_id).ok()??;
    let bytes: Vec<u8> = constant.try_extract_into().ok()?;
    String::from_utf8(bytes).ok()
}

/// Try to extract an i32 from a register.
fn extract_register_int(ergo_box: &ErgoBox, reg_id: NonMandatoryRegisterId) -> Option<i32> {
    let constant = ergo_box.additional_registers.get_constant(reg_id).ok()??;
    constant.try_extract_into::<i32>().ok()
}

/// Extract non-empty registers R4-R9 as serialized bytes.
fn extract_registers(ergo_box: &ErgoBox) -> Vec<IndexedRegister> {
    let mut regs = Vec::new();
    for reg_id in NonMandatoryRegisterId::REG_IDS {
        if let Ok(Some(constant)) = ergo_box.additional_registers.get_constant(reg_id) {
            if let Ok(bytes) = constant.sigma_serialize_bytes() {
                regs.push(IndexedRegister {
                    register_id: reg_id as u8,
                    serialized: bytes,
                });
            }
        }
    }
    regs
}

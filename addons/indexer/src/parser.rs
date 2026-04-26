use anyhow::{Context, Result};
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
use ergo_lib::ergotree_ir::mir::constant::Constant;
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
///
/// Parses directly from JSON rather than round-tripping through sigma-rust's
/// Transaction deserializer, which rejects txs whose ErgoTree serialization
/// doesn't byte-for-byte match the original (a sigma-rust limitation, not a
/// protocol violation). The node already validated the block.
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
        let tx_id_hex = tx_val
            .get("id")
            .and_then(|v| v.as_str())
            .with_context(|| format!("missing tx id at index {tx_index}"))?;
        let tx_id = hex_to_bytes32(tx_id_hex)
            .with_context(|| format!("invalid tx id hex at index {tx_index}"))?;

        let inputs_arr = tx_val
            .get("inputs")
            .and_then(|v| v.as_array())
            .with_context(|| format!("missing inputs at tx index {tx_index}"))?;

        let inputs: Vec<InputRef> = inputs_arr
            .iter()
            .enumerate()
            .map(|(i, inp)| {
                let bid = inp
                    .get("boxId")
                    .and_then(|v| v.as_str())
                    .with_context(|| format!("missing input boxId at tx {tx_index} input {i}"))?;
                Ok(InputRef {
                    box_id: hex_to_bytes32(bid)?,
                })
            })
            .collect::<Result<_>>()?;

        let first_input_id = inputs
            .first()
            .context("transaction has no inputs")?
            .box_id;

        let outputs_arr = tx_val
            .get("outputs")
            .and_then(|v| v.as_array())
            .with_context(|| format!("missing outputs at tx index {tx_index}"))?;

        let mut outputs = Vec::with_capacity(outputs_arr.len());
        for (out_idx, out_val) in outputs_arr.iter().enumerate() {
            let box_id_hex = out_val
                .get("boxId")
                .and_then(|v| v.as_str())
                .with_context(|| format!("missing boxId at tx {tx_index} output {out_idx}"))?;
            let box_id = hex_to_bytes32(box_id_hex)?;

            let ergo_tree_hex = out_val
                .get("ergoTree")
                .and_then(|v| v.as_str())
                .with_context(|| format!("missing ergoTree at tx {tx_index} output {out_idx}"))?;
            let ergo_tree_bytes = hex::decode(ergo_tree_hex)
                .with_context(|| format!("invalid ergoTree hex at tx {tx_index} output {out_idx}"))?;
            let ergo_tree_hash = blake2b256(&ergo_tree_bytes);

            let address = match ErgoTree::sigma_parse_bytes(&ergo_tree_bytes) {
                Ok(tree) => match Address::recreate_from_ergo_tree(&tree) {
                    Ok(addr) => encoder.address_to_str(&addr),
                    Err(_) => ergo_tree_hex.to_string(),
                },
                Err(_) => ergo_tree_hex.to_string(),
            };

            let value = out_val
                .get("value")
                .and_then(|v| v.as_u64())
                .with_context(|| format!("missing value at tx {tx_index} output {out_idx}"))?;

            let tokens = parse_tokens(out_val)?;

            let minted_token = if tokens.iter().any(|t| t.token_id == first_input_id) {
                Some(MintedToken {
                    token_id: first_input_id,
                    name: extract_register_string(out_val, "R4"),
                    description: extract_register_string(out_val, "R5"),
                    decimals: extract_register_int(out_val, "R6"),
                })
            } else {
                None
            };

            let registers = extract_registers(out_val);

            outputs.push(IndexedBox {
                box_id,
                output_index: out_idx as u32,
                ergo_tree: ergo_tree_bytes,
                ergo_tree_hash,
                address,
                value,
                tokens,
                registers,
                minted_token,
            });
        }

        let tx_size = estimate_tx_size(&inputs, &outputs);
        block_size += tx_size;

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

/// Parse and deduplicate tokens from an output JSON object.
/// Boxes may list the same token_id multiple times; we sum amounts per token
/// to match explorer semantics and the DB's (box_id, token_id) PK.
fn parse_tokens(out_val: &serde_json::Value) -> Result<Vec<IndexedToken>> {
    let assets = match out_val.get("assets").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    let mut deduped: Vec<IndexedToken> = Vec::with_capacity(assets.len());
    for asset in assets {
        let tid_hex = asset
            .get("tokenId")
            .and_then(|v| v.as_str())
            .context("missing tokenId in asset")?;
        let token_id = hex_to_bytes32(tid_hex)?;
        let amount = asset
            .get("amount")
            .and_then(|v| v.as_u64())
            .context("missing amount in asset")?;
        match deduped.iter_mut().find(|e| e.token_id == token_id) {
            Some(existing) => existing.amount = existing.amount.saturating_add(amount),
            None => deduped.push(IndexedToken { token_id, amount }),
        }
    }
    Ok(deduped)
}

/// Estimate binary tx size from known parts. Not exact (spending proofs are
/// variable) but close enough for indexer display purposes.
fn estimate_tx_size(inputs: &[InputRef], outputs: &[IndexedBox]) -> u32 {
    let input_bytes = inputs.len() * 70; // 32 box_id + ~34 proof + ~4 context ext
    let output_bytes: usize = outputs
        .iter()
        .map(|o| {
            o.ergo_tree.len()
                + o.tokens.len() * 40
                + o.registers.iter().map(|r| r.serialized.len()).sum::<usize>()
                + 20 // value + creation_height + VLQ overhead
        })
        .sum();
    (input_bytes + output_bytes) as u32
}

/// Decode a register's hex string and parse it as a sigma Constant.
fn parse_register_constant(out_val: &serde_json::Value, reg_key: &str) -> Option<Constant> {
    let hex_str = out_val
        .get("additionalRegisters")?
        .get(reg_key)?
        .as_str()?;
    let bytes = hex::decode(hex_str).ok()?;
    Constant::sigma_parse_bytes(&bytes).ok()
}

/// Try to extract a UTF-8 string from a register (expects Coll[Byte]).
fn extract_register_string(out_val: &serde_json::Value, reg_key: &str) -> Option<String> {
    let data: Vec<u8> = parse_register_constant(out_val, reg_key)?.try_extract_into().ok()?;
    String::from_utf8(data).ok()
}

/// Try to extract an i32 from a register.
fn extract_register_int(out_val: &serde_json::Value, reg_key: &str) -> Option<i32> {
    parse_register_constant(out_val, reg_key)?.try_extract_into::<i32>().ok()
}

/// Extract non-empty registers R4-R9 as their original serialized bytes.
fn extract_registers(out_val: &serde_json::Value) -> Vec<IndexedRegister> {
    let map = match out_val
        .get("additionalRegisters")
        .and_then(|v| v.as_object())
    {
        Some(m) => m,
        None => return Vec::new(),
    };
    let mut regs: Vec<IndexedRegister> = map
        .iter()
        .filter_map(|(key, val)| {
            let reg_id = key.strip_prefix('R')?.parse::<u8>().ok()?;
            let hex_str = val.as_str()?;
            let bytes = hex::decode(hex_str).ok()?;
            Some(IndexedRegister {
                register_id: reg_id,
                serialized: bytes,
            })
        })
        .collect();
    regs.sort_by_key(|r| r.register_id);
    regs
}

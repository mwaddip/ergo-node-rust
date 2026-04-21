//! Convert JVM REST API JSON into wire-format modifier bytes for the ingest endpoint.
//!
//! Each modifier is `(type_id, modifier_id, data)` where `data` is the raw
//! Scorex-serialized body — the same bytes the node would receive over P2P.

use anyhow::{Context, Result, bail};
use blake2::digest::{Update, VariableOutput};
use blake2::Blake2bVar;
use ergo_chain_types::Header;
use sigma_ser::vlq_encode::WriteSigmaVlqExt;

use crate::types::{JvmAdProofs, JvmBlockTransactions, JvmExtension, JvmFullBlock};

// Modifier type IDs — must match chain/src/section.rs
pub const HEADER_TYPE_ID: u8 = 101;
pub const BLOCK_TRANSACTIONS_TYPE_ID: u8 = 102;
pub const AD_PROOFS_TYPE_ID: u8 = 104;
pub const EXTENSION_TYPE_ID: u8 = 108;

/// Version sentinel for BlockTransactions encoding (matches JVM).
const BLOCK_VERSION_SENTINEL: u32 = 10_000_000;

/// A modifier ready for ingestion: type_id, modifier_id, wire bytes.
pub struct Modifier {
    pub type_id: u8,
    pub id: [u8; 32],
    pub data: Vec<u8>,
}

/// Blake2b256 hash.
fn blake2b256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2bVar::new(32).unwrap();
    hasher.update(data);
    let mut out = [0u8; 32];
    hasher.finalize_variable(&mut out).unwrap();
    out
}

/// `Blake2b256(prefix || a || b)` — mirrors Scorex `Algos.hash.prefixedHash`.
fn prefixed_hash(prefix: u8, a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(65);
    buf.push(prefix);
    buf.extend_from_slice(a);
    buf.extend_from_slice(b);
    blake2b256(&buf)
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// Parse a JVM header JSON value into an ergo_chain_types::Header.
///
/// sigma-rust's Header already supports the JVM JSON format (extensionHash,
/// powSolutions, etc.) — verified by enr-chain's test suite.
pub fn parse_header_json(value: &serde_json::Value) -> Result<Header> {
    serde_json::from_value(value.clone()).context("failed to parse header JSON")
}

/// Serialize a header to wire format. Returns a Modifier with type 101.
pub fn header_to_modifier(header: &Header) -> Result<Modifier> {
    use sigma_ser::ScorexSerializable;
    let data = header
        .scorex_serialize_bytes()
        .context("header scorex serialize")?;
    let id: [u8; 32] = header.id.0 .0;
    Ok(Modifier {
        type_id: HEADER_TYPE_ID,
        id,
        data,
    })
}

// ---------------------------------------------------------------------------
// Full block sections
// ---------------------------------------------------------------------------

/// Convert a JVM full block into modifiers for all sections (excluding header).
///
/// The header should be pushed separately (it was already sent during the
/// chainSlice phase). This returns BlockTransactions, ADProofs (if present),
/// and Extension modifiers.
pub fn block_sections_to_modifiers(
    block: &JvmFullBlock,
    header: &Header,
) -> Result<Vec<Modifier>> {
    let header_id: [u8; 32] = header.id.0 .0;
    let mut mods = Vec::with_capacity(3);

    // BlockTransactions
    mods.push(
        block_transactions_to_modifier(&block.block_transactions, &header_id, header)
            .context("block transactions")?,
    );

    // ADProofs (optional — UTXO-mode peers may not serve them)
    if let Some(ref ad_proofs) = block.ad_proofs {
        mods.push(
            ad_proofs_to_modifier(ad_proofs, &header_id, header).context("ad proofs")?,
        );
    }

    // Extension
    mods.push(
        extension_to_modifier(&block.extension, &header_id, header).context("extension")?,
    );

    Ok(mods)
}

// ---------------------------------------------------------------------------
// BlockTransactions (type 102)
// ---------------------------------------------------------------------------

/// Serialize block transactions from JVM JSON to wire format.
///
/// Wire: `[header_id: 32B] [version_sentinel: VLQ] [tx_count: VLQ] [txs...]`
///
/// Transactions are deserialized via ergo-lib's Transaction JSON support,
/// then sigma-serialized to bytes.
fn block_transactions_to_modifier(
    bt: &JvmBlockTransactions,
    header_id: &[u8; 32],
    header: &Header,
) -> Result<Modifier> {
    use ergo_lib::chain::transaction::Transaction;
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

    let mut out = Vec::new();
    out.extend_from_slice(header_id);

    // Version sentinel
    let mut buf = Vec::new();
    buf.put_u32(BLOCK_VERSION_SENTINEL + bt.block_version)?;
    out.extend_from_slice(&buf);

    // Transaction count
    buf.clear();
    buf.put_u32(bt.transactions.len() as u32)?;
    out.extend_from_slice(&buf);

    // Serialize each transaction
    for (i, tx_json) in bt.transactions.iter().enumerate() {
        let tx: Transaction = serde_json::from_value(tx_json.clone())
            .with_context(|| format!("parse transaction {i} JSON"))?;
        let tx_bytes = tx
            .sigma_serialize_bytes()
            .with_context(|| format!("serialize transaction {i}"))?;
        out.extend_from_slice(&tx_bytes);
    }

    let modifier_id = prefixed_hash(
        BLOCK_TRANSACTIONS_TYPE_ID,
        header_id,
        &header.transaction_root.0,
    );

    Ok(Modifier {
        type_id: BLOCK_TRANSACTIONS_TYPE_ID,
        id: modifier_id,
        data: out,
    })
}

// ---------------------------------------------------------------------------
// ADProofs (type 104)
// ---------------------------------------------------------------------------

/// Serialize AD proofs from JVM JSON to wire format.
///
/// Wire: `[header_id: 32B] [proof_size: VLQ] [proof_bytes]`
///
/// JVM JSON has `proofBytes` as a hex string — that's the raw proof data.
fn ad_proofs_to_modifier(
    ap: &JvmAdProofs,
    header_id: &[u8; 32],
    header: &Header,
) -> Result<Modifier> {
    let proof_bytes =
        hex::decode(&ap.proof_bytes).context("hex decode proofBytes")?;

    let mut out = Vec::with_capacity(32 + 5 + proof_bytes.len());
    out.extend_from_slice(header_id);

    let mut buf = Vec::new();
    buf.put_u32(proof_bytes.len() as u32)?;
    out.extend_from_slice(&buf);
    out.extend_from_slice(&proof_bytes);

    let modifier_id = prefixed_hash(
        AD_PROOFS_TYPE_ID,
        header_id,
        &header.ad_proofs_root.0,
    );

    Ok(Modifier {
        type_id: AD_PROOFS_TYPE_ID,
        id: modifier_id,
        data: out,
    })
}

// ---------------------------------------------------------------------------
// Extension (type 108)
// ---------------------------------------------------------------------------

/// Serialize extension from JVM JSON to wire format.
///
/// Wire: `[header_id: 32B] [field_count: VLQ] [{key: 2B, val_len: 1B, val}...]`
///
/// JVM JSON has `fields` as `[[key_hex, value_hex], ...]`.
fn extension_to_modifier(
    ext: &JvmExtension,
    header_id: &[u8; 32],
    header: &Header,
) -> Result<Modifier> {
    let mut out = Vec::new();
    out.extend_from_slice(header_id);

    let mut buf = Vec::new();
    buf.put_u32(ext.fields.len() as u32)?;
    out.extend_from_slice(&buf);

    for (i, (key_hex, val_hex)) in ext.fields.iter().enumerate() {
        let key = hex::decode(key_hex)
            .with_context(|| format!("field {i}: hex decode key"))?;
        if key.len() != 2 {
            bail!("field {i}: key length {} != 2", key.len());
        }
        let value = hex::decode(val_hex)
            .with_context(|| format!("field {i}: hex decode value"))?;
        if value.len() > 255 {
            bail!("field {i}: value length {} > 255", value.len());
        }
        out.extend_from_slice(&key);
        out.push(value.len() as u8);
        out.extend_from_slice(&value);
    }

    let modifier_id = prefixed_hash(
        EXTENSION_TYPE_ID,
        header_id,
        &header.extension_root.0,
    );

    Ok(Modifier {
        type_id: EXTENSION_TYPE_ID,
        id: modifier_id,
        data: out,
    })
}

// ---------------------------------------------------------------------------
// Ingest binary encoding
// ---------------------------------------------------------------------------

/// Encode a batch of modifiers into the binary format expected by
/// `POST /ingest/modifiers`.
///
/// Format: `(type_id: u8, modifier_id: [u8; 32], data_len: u32 BE, data)` repeated.
pub fn encode_ingest_body(modifiers: &[Modifier]) -> Vec<u8> {
    let total: usize = modifiers.iter().map(|m| 1 + 32 + 4 + m.data.len()).sum();
    let mut body = Vec::with_capacity(total);
    for m in modifiers {
        body.push(m.type_id);
        body.extend_from_slice(&m.id);
        body.extend_from_slice(&(m.data.len() as u32).to_be_bytes());
        body.extend_from_slice(&m.data);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake2b256_sanity() {
        let hash = blake2b256(b"test");
        assert_eq!(hash.len(), 32);
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn encode_ingest_body_roundtrip() {
        let mods = vec![
            Modifier {
                type_id: 101,
                id: [0xAA; 32],
                data: vec![1, 2, 3],
            },
            Modifier {
                type_id: 104,
                id: [0xBB; 32],
                data: vec![4, 5],
            },
        ];
        let body = encode_ingest_body(&mods);

        // First modifier: 1 + 32 + 4 + 3 = 40 bytes
        assert_eq!(body[0], 101);
        assert_eq!(&body[1..33], &[0xAA; 32]);
        assert_eq!(&body[33..37], &[0, 0, 0, 3]); // 3 BE
        assert_eq!(&body[37..40], &[1, 2, 3]);

        // Second: 1 + 32 + 4 + 2 = 39 bytes
        assert_eq!(body[40], 104);
        assert_eq!(&body[41..73], &[0xBB; 32]);
        assert_eq!(&body[73..77], &[0, 0, 0, 2]);
        assert_eq!(&body[77..79], &[4, 5]);
    }

    #[test]
    fn prefixed_hash_is_deterministic() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let h1 = prefixed_hash(102, &a, &b);
        let h2 = prefixed_hash(102, &a, &b);
        assert_eq!(h1, h2);
        // Different prefix → different hash
        let h3 = prefixed_hash(104, &a, &b);
        assert_ne!(h1, h3);
    }
}

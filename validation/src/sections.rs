//! Parse raw block section bytes into typed structures.

use std::io::Cursor;

use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::serialization::constant_store::ConstantStore;
use ergo_lib::ergotree_ir::serialization::sigma_byte_reader::SigmaByteReader;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use sigma_ser::vlq_encode::ReadSigmaVlqExt;

use crate::ValidationError;

/// Parsed ADProofs section (type 104).
pub struct ParsedAdProofs {
    pub header_id: [u8; 32],
    pub proof_bytes: Vec<u8>,
}

/// Parsed BlockTransactions section (type 102).
pub struct ParsedBlockTransactions {
    pub header_id: [u8; 32],
    pub block_version: u32,
    pub transactions: Vec<Transaction>,
}

/// A single key-value field from the Extension section.
pub struct ExtensionField {
    pub key: [u8; 2],
    pub value: Vec<u8>,
}

/// Parsed Extension section (type 108).
pub struct ParsedExtension {
    pub header_id: [u8; 32],
    pub fields: Vec<ExtensionField>,
}

fn section_parse_err(section_type: u8, reason: impl Into<String>) -> ValidationError {
    ValidationError::SectionParse {
        section_type,
        reason: reason.into(),
    }
}

/// Parse an ADProofs section (type 104) from raw modifier bytes.
///
/// Wire format: `[header_id: 32B] [proof_size: VLQ u32] [proof_bytes: proof_size B]`
pub fn parse_ad_proofs(data: &[u8]) -> Result<ParsedAdProofs, ValidationError> {
    const TYPE_ID: u8 = 104;
    if data.len() < 33 {
        return Err(section_parse_err(TYPE_ID, "too short for header_id + proof_size"));
    }
    let header_id: [u8; 32] = data[..32].try_into().unwrap();
    let mut cursor = Cursor::new(&data[32..]);
    let proof_size = cursor
        .get_u32()
        .map_err(|e| section_parse_err(TYPE_ID, format!("proof_size VLQ: {e}")))?
        as usize;
    let pos = 32 + cursor.position() as usize;
    let remaining = data.len() - pos;
    if remaining < proof_size {
        return Err(section_parse_err(
            TYPE_ID,
            format!("proof_size={proof_size} but only {remaining} bytes remain"),
        ));
    }
    let proof_bytes = data[pos..pos + proof_size].to_vec();
    Ok(ParsedAdProofs {
        header_id,
        proof_bytes,
    })
}

/// Version sentinel threshold for BlockTransactions encoding.
const BLOCK_VERSION_SENTINEL: u32 = 10_000_000;

/// Parse a BlockTransactions section (type 102) from raw modifier bytes.
///
/// Wire format: `[header_id: 32B] [ver_or_count: VLQ] [tx_count: VLQ if ver>1] [txs...]`
///
/// Version hack: if first VLQ > 10M, subtract 10M for block_version, read separate tx_count.
/// Otherwise first VLQ IS the tx_count and block_version = 1.
pub fn parse_block_transactions(data: &[u8]) -> Result<ParsedBlockTransactions, ValidationError> {
    const TYPE_ID: u8 = 102;
    if data.len() < 33 {
        return Err(section_parse_err(TYPE_ID, "too short for header_id"));
    }
    let header_id: [u8; 32] = data[..32].try_into().unwrap();
    let mut cursor = Cursor::new(&data[32..]);

    let ver_or_count = cursor
        .get_u32()
        .map_err(|e| section_parse_err(TYPE_ID, format!("ver_or_count VLQ: {e}")))?;

    let (block_version, tx_count) = if ver_or_count > BLOCK_VERSION_SENTINEL {
        let version = ver_or_count - BLOCK_VERSION_SENTINEL;
        let count = cursor
            .get_u32()
            .map_err(|e| section_parse_err(TYPE_ID, format!("tx_count VLQ: {e}")))?;
        (version, count as usize)
    } else {
        (1, ver_or_count as usize)
    };

    // Parse individual transactions from the remaining byte stream.
    // Transaction::sigma_parse reads from a SigmaByteReader and advances the position.
    let pos = 32 + cursor.position() as usize;
    let tx_cursor = Cursor::new(&data[pos..]);
    let mut reader = SigmaByteReader::new(tx_cursor, ConstantStore::empty());
    let mut transactions = Vec::with_capacity(tx_count);

    for i in 0..tx_count {
        let tx = Transaction::sigma_parse(&mut reader)
            .map_err(|e| section_parse_err(TYPE_ID, format!("transaction {i}: {e}")))?;
        transactions.push(tx);
    }

    Ok(ParsedBlockTransactions {
        header_id,
        block_version,
        transactions,
    })
}

/// Parse an Extension section (type 108) from raw modifier bytes.
///
/// Wire format: `[header_id: 32B] [field_count: VLQ] [fields: {key: 2B, val_len: 1B, val}...]`
pub fn parse_extension(data: &[u8]) -> Result<ParsedExtension, ValidationError> {
    const TYPE_ID: u8 = 108;
    if data.len() < 33 {
        return Err(section_parse_err(TYPE_ID, "too short for header_id"));
    }
    let header_id: [u8; 32] = data[..32].try_into().unwrap();
    let mut cursor = Cursor::new(&data[32..]);

    let field_count = cursor
        .get_u32()
        .map_err(|e| section_parse_err(TYPE_ID, format!("field_count VLQ: {e}")))?
        as usize;

    let mut pos = 32 + cursor.position() as usize;
    let mut fields = Vec::with_capacity(field_count);

    for i in 0..field_count {
        if pos + 3 > data.len() {
            return Err(section_parse_err(TYPE_ID, format!("field {i}: truncated")));
        }
        let key: [u8; 2] = data[pos..pos + 2].try_into().unwrap();
        let value_len = data[pos + 2] as usize;
        pos += 3;
        if pos + value_len > data.len() {
            return Err(section_parse_err(
                TYPE_ID,
                format!(
                    "field {i}: value_len={value_len} but only {} bytes remain",
                    data.len() - pos
                ),
            ));
        }
        let value = data[pos..pos + value_len].to_vec();
        pos += value_len;
        fields.push(ExtensionField { key, value });
    }

    Ok(ParsedExtension {
        header_id,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigma_ser::vlq_encode::WriteSigmaVlqExt;

    // --- ADProofs tests ---

    #[test]
    fn parse_ad_proofs_basic() {
        let mut data = Vec::new();
        let header_id = [0xAA; 32];
        data.extend_from_slice(&header_id);
        // VLQ encode 3 (single byte since < 128)
        data.push(3);
        data.extend_from_slice(&[0x01, 0x02, 0x03]);

        let parsed = parse_ad_proofs(&data).unwrap();
        assert_eq!(parsed.header_id, header_id);
        assert_eq!(parsed.proof_bytes, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn parse_ad_proofs_too_short() {
        let data = [0u8; 31];
        assert!(parse_ad_proofs(&data).is_err());
    }

    #[test]
    fn parse_ad_proofs_truncated_proof() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 32]); // header_id
        data.push(10); // claim 10 bytes of proof
        data.extend_from_slice(&[0u8; 5]); // only 5 bytes
        assert!(parse_ad_proofs(&data).is_err());
    }

    // --- BlockTransactions tests ---

    #[test]
    fn parse_block_transactions_empty_block() {
        let mut data = Vec::new();
        let header_id = [0xBB; 32];
        data.extend_from_slice(&header_id);
        // VLQ encode 10_000_002 (version 2 sentinel)
        let mut sentinel_buf = Vec::new();
        WriteSigmaVlqExt::put_u32(&mut sentinel_buf, 10_000_002).unwrap();
        data.extend_from_slice(&sentinel_buf);
        // VLQ encode 0 (tx_count)
        data.push(0);

        let parsed = parse_block_transactions(&data).unwrap();
        assert_eq!(parsed.header_id, header_id);
        assert_eq!(parsed.block_version, 2);
        assert!(parsed.transactions.is_empty());
    }

    #[test]
    fn parse_block_transactions_too_short() {
        let data = [0u8; 20];
        assert!(parse_block_transactions(&data).is_err());
    }

    // --- Extension tests ---

    #[test]
    fn parse_extension_basic() {
        let mut data = Vec::new();
        let header_id = [0xCC; 32];
        data.extend_from_slice(&header_id);
        // VLQ encode field_count = 2
        data.push(2);
        // Field 1: key=[0x00, 0x01], value_len=4, value=[0xDE, 0xAD, 0xBE, 0xEF]
        data.extend_from_slice(&[0x00, 0x01]);
        data.push(4);
        data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        // Field 2: key=[0x01, 0x00], value_len=1, value=[0xFF]
        data.extend_from_slice(&[0x01, 0x00]);
        data.push(1);
        data.push(0xFF);

        let parsed = parse_extension(&data).unwrap();
        assert_eq!(parsed.header_id, header_id);
        assert_eq!(parsed.fields.len(), 2);
        assert_eq!(parsed.fields[0].key, [0x00, 0x01]);
        assert_eq!(parsed.fields[0].value, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(parsed.fields[1].key, [0x01, 0x00]);
        assert_eq!(parsed.fields[1].value, vec![0xFF]);
    }

    #[test]
    fn parse_extension_empty() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 32]); // header_id
        data.push(0); // field_count = 0
        let parsed = parse_extension(&data).unwrap();
        assert!(parsed.fields.is_empty());
    }

    #[test]
    fn parse_extension_truncated_value() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 32]);
        data.push(1); // field_count = 1
        data.extend_from_slice(&[0x00, 0x01]); // key
        data.push(10); // claim 10 bytes
        data.extend_from_slice(&[0u8; 3]); // only 3 bytes
        assert!(parse_extension(&data).is_err());
    }
}

//! Parse raw block section bytes into typed structures.

use std::io::Cursor;

use enr_chain::{block_transactions_read_version, block_transactions_write_version};
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::serialization::constant_store::ConstantStore;
use ergo_lib::ergotree_ir::serialization::sigma_byte_reader::{SigmaByteRead, SigmaByteReader};
use ergo_lib::ergotree_ir::serialization::sigma_byte_writer::{SigmaByteWrite, SigmaByteWriter};
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
        return Err(section_parse_err(
            TYPE_ID,
            "too short for header_id + proof_size",
        ));
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
/// Otherwise first VLQ IS the tx_count and block_version = 1. A block_version
/// that does not fit `u8` is a parse error.
///
/// Each transaction is read at `enr_chain::block_transactions_read_version`
/// of that block_version: version 3 from block version 4, version 0 below.
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
        // `Header.version` is a byte. `enr_chain::section_id_from_body`
        // rejects a larger version, and the two parsers must agree
        // (facts/chain.md § "Two parsers, one wire format").
        let version = u8::try_from(version).map_err(|_| {
            section_parse_err(TYPE_ID, format!("block version {version} does not fit u8"))
        })?;
        let count = cursor
            .get_u32()
            .map_err(|e| section_parse_err(TYPE_ID, format!("tx_count VLQ: {e}")))?;
        (version, count as usize)
    } else {
        (1, ver_or_count as usize)
    };
    // The version the section encodes, not the header's, selects the tree
    // version its transactions are read at, and the fork computes each
    // transaction's id at the version it read it at (facts/chain.md § "The
    // tree version a block's transactions are read at").
    let tree_version = block_transactions_read_version(block_version);

    // Parse individual transactions from the remaining byte stream.
    // Transaction::sigma_parse reads from a SigmaByteReader and advances the position.
    // It starts each transaction on a fresh parse state (level 0, empty
    // stores); `with_tree_version` swaps only the version around it.
    // The wire count bounds the loop trip count only — never a preallocation.
    let pos = 32 + cursor.position() as usize;
    let tx_cursor = Cursor::new(&data[pos..]);
    let mut reader = SigmaByteReader::new(tx_cursor, ConstantStore::empty());
    let mut transactions = Vec::new();

    for i in 0..tx_count {
        let tx = reader
            .with_tree_version(tree_version, Transaction::sigma_parse)
            .map_err(|e| section_parse_err(TYPE_ID, format!("transaction {i}: {e}")))?;
        transactions.push(tx);
    }

    Ok(ParsedBlockTransactions {
        header_id,
        block_version: u32::from(block_version),
        transactions,
    })
}

/// Parse an Extension section (type 108) from raw modifier bytes.
///
/// Wire format: `[header_id: 32B] [field_count: VLQ u16] [fields: {key: 2B, val_len: 1B, val}...]`
///
/// The count is a VLQ ushort (JVM `ExtensionSerializer` writes/reads it via
/// `putUShort`/`getUShort`, range-checked 0..=65535); any larger value is a
/// parse error, matching the JVM and `chain::section_id_from_body`.
pub fn parse_extension(data: &[u8]) -> Result<ParsedExtension, ValidationError> {
    const TYPE_ID: u8 = 108;
    if data.len() < 33 {
        return Err(section_parse_err(TYPE_ID, "too short for header_id"));
    }
    let header_id: [u8; 32] = data[..32].try_into().unwrap();
    let mut cursor = Cursor::new(&data[32..]);

    let field_count = cursor
        .get_u16()
        .map_err(|e| section_parse_err(TYPE_ID, format!("field_count VLQ: {e}")))?
        as usize;

    let mut pos = 32 + cursor.position() as usize;
    // The wire count bounds the loop trip count only — never a preallocation.
    let mut fields = Vec::new();

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

    Ok(ParsedExtension { header_id, fields })
}

// ---------------------------------------------------------------------------
// Serializers (inverses of the parsers above) — used by the mining crate to
// convert structured block sections back to wire format for storage and
// pipeline injection.
// ---------------------------------------------------------------------------

use sigma_ser::vlq_encode::WriteSigmaVlqExt;

/// Serialize an ADProofs section to wire format.
///
/// Wire format: `[header_id: 32B] [proof_size: VLQ u32] [proof_bytes]`
pub fn serialize_ad_proofs(header_id: &[u8; 32], proof_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 5 + proof_bytes.len());
    out.extend_from_slice(header_id);
    let mut size_buf = Vec::new();
    WriteSigmaVlqExt::put_u32(&mut size_buf, proof_bytes.len() as u32).unwrap();
    out.extend_from_slice(&size_buf);
    out.extend_from_slice(proof_bytes);
    out
}

/// Serialize a BlockTransactions section to wire format.
///
/// Wire format: `[header_id: 32B] [version_sentinel: VLQ] [tx_count: VLQ] [txs...]`
/// where `version_sentinel = BLOCK_VERSION_SENTINEL + block_version` for v2+.
///
/// Each transaction is written at `enr_chain::block_transactions_write_version`
/// of `block_version`: version 3 from block version 3, version 0 below (JVM
/// `BlockTransactionsSerializer.serialize`). A v3 block is written at 3 and
/// read back at 0; the asymmetry is the JVM's own. `sigma_serialize_bytes()`
/// is not this rule: it writes every block at the fork's highest version.
/// A `block_version` that does not fit `u8` is an error.
pub fn serialize_block_transactions(
    header_id: &[u8; 32],
    block_version: u32,
    transactions: &[Transaction],
) -> Result<Vec<u8>, ValidationError> {
    const TYPE_ID: u8 = 102;
    // `Header.version` is a byte, and neither parser reads a larger one back.
    let version = u8::try_from(block_version).map_err(|_| {
        section_parse_err(
            TYPE_ID,
            format!("block version {block_version} does not fit u8"),
        )
    })?;
    let tree_version = block_transactions_write_version(version);

    let mut out = Vec::new();
    out.extend_from_slice(header_id);

    // Version sentinel for v2+
    let mut sentinel_buf = Vec::new();
    WriteSigmaVlqExt::put_u32(&mut sentinel_buf, BLOCK_VERSION_SENTINEL + block_version).unwrap();
    out.extend_from_slice(&sentinel_buf);

    // Transaction count
    let mut count_buf = Vec::new();
    WriteSigmaVlqExt::put_u32(&mut count_buf, transactions.len() as u32).unwrap();
    out.extend_from_slice(&count_buf);

    // Transactions, on one writer as the JVM's serializer writes them
    let mut w = SigmaByteWriter::new(&mut out, None);
    for (i, tx) in transactions.iter().enumerate() {
        w.with_tree_version(tree_version, |w| tx.sigma_serialize(w))
            .map_err(|e| section_parse_err(TYPE_ID, format!("transaction {i} serialize: {e}")))?;
    }

    Ok(out)
}

/// Serialize an Extension section to wire format.
///
/// Wire format: `[header_id: 32B] [field_count: VLQ] [fields: {key: 2B, val_len: 1B, val}]`
pub fn serialize_extension(
    header_id: &[u8; 32],
    fields: &[([u8; 2], Vec<u8>)],
) -> Result<Vec<u8>, ValidationError> {
    let mut out = Vec::new();
    out.extend_from_slice(header_id);

    let mut count_buf = Vec::new();
    WriteSigmaVlqExt::put_u32(&mut count_buf, fields.len() as u32).unwrap();
    out.extend_from_slice(&count_buf);

    for (i, (key, value)) in fields.iter().enumerate() {
        if value.len() > 255 {
            return Err(section_parse_err(
                108,
                format!("field {i}: value length {} exceeds 255", value.len()),
            ));
        }
        out.extend_from_slice(key);
        out.push(value.len() as u8);
        out.extend_from_slice(value);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        raw_section, tx_read_at, EXTENSION_TX, EXTENSION_TX_AT_0, EXTENSION_TX_ID_AT_0,
        EXTENSION_TX_ID_AT_3, HEADER_ID, REGISTER_OUTPUT_ID_AT_0, REGISTER_OUTPUT_ID_AT_3,
        REGISTER_TX, REGISTER_TX_ID_AT_0, REGISTER_TX_ID_AT_3,
    };
    use ergo_lib::ergotree_ir::ergo_tree::ErgoTreeVersion;
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

    /// JVM `ExtensionSerializer` writes the field count with `putUShort` and
    /// reads it with `getUShort`, range-checked 0..=65535. A count of 65_536
    /// is not a legal extension body on the wire; our parser must Err rather
    /// than silently accept it as it would with `get_u32`.
    #[test]
    fn parse_extension_field_count_above_u16_max_is_err() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 32]); // header_id

        // VLQ-encode 65_536 (out of range for u16). put_u32 produces the
        // same VLQ bytes as any encoder for this value; the type only
        // decides the range check on read.
        let mut count_buf = Vec::new();
        WriteSigmaVlqExt::put_u32(&mut count_buf, 65_536).unwrap();
        data.extend_from_slice(&count_buf);
        match parse_extension(&data) {
            Err(ValidationError::SectionParse {
                section_type,
                reason,
            }) => {
                assert_eq!(section_type, 108);
                assert!(
                    reason.contains("field_count"),
                    "unexpected reason: {reason}"
                );
            }
            Err(other) => panic!("expected SectionParse, got {other:?}"),
            Ok(_) => panic!("count 65536 must not parse"),
        }
    }

    /// A hostile BlockTransactions body declares `u32::MAX` transactions and
    /// carries none. The old `Vec::with_capacity(tx_count)` would attempt a
    /// ~34 GB allocation on 64-bit before the loop; the fixed parser must
    /// Err from the first `Transaction::sigma_parse` on the empty tail.
    ///
    /// We cannot easily prove "did not allocate 4 billion Transactions"
    /// without a counting allocator in the test harness — none is wired up
    /// here — so this test asserts the observable outcome (Err), and the
    /// no-preallocation guarantee is code-inspected on the change to
    /// `Vec::new()` in `parse_block_transactions`.
    #[test]
    fn parse_block_transactions_hostile_count_is_err_without_allocating() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 32]); // header_id

        // version sentinel for BlockVersion == 2
        let mut sentinel_buf = Vec::new();
        WriteSigmaVlqExt::put_u32(&mut sentinel_buf, BLOCK_VERSION_SENTINEL + 2).unwrap();
        data.extend_from_slice(&sentinel_buf);
        // hostile tx_count == u32::MAX
        let mut count_buf = Vec::new();
        WriteSigmaVlqExt::put_u32(&mut count_buf, u32::MAX).unwrap();
        data.extend_from_slice(&count_buf);
        // no transaction bytes follow
        match parse_block_transactions(&data) {
            Err(ValidationError::SectionParse { section_type, .. }) => {
                assert_eq!(section_type, 102);
            }
            Err(other) => panic!("expected SectionParse, got {other:?}"),
            Ok(_) => panic!("u32::MAX transactions with none supplied must Err"),
        }
    }

    // --- Tree versions (facts/chain.md § "The tree version a block's
    // transactions are read at"). Fixtures in `test_support`. ---

    /// The ids of a section holding [`EXTENSION_TX`] then [`REGISTER_TX`]:
    /// both transaction ids, and the box id of the register transaction's
    /// output.
    fn ids(parsed: &ParsedBlockTransactions) -> [String; 3] {
        let [extension, register] = &parsed.transactions[..] else {
            panic!("two transactions, got {}", parsed.transactions.len());
        };
        [
            hex::encode(extension.id().as_ref()),
            hex::encode(register.id().as_ref()),
            hex::encode(register.outputs.first().box_id().as_ref()),
        ]
    }

    const IDS_AT_3: [&str; 3] = [
        EXTENSION_TX_ID_AT_3,
        REGISTER_TX_ID_AT_3,
        REGISTER_OUTPUT_ID_AT_3,
    ];
    const IDS_AT_0: [&str; 3] = [
        EXTENSION_TX_ID_AT_0,
        REGISTER_TX_ID_AT_0,
        REGISTER_OUTPUT_ID_AT_0,
    ];

    /// From block version 4 a section's transactions are read at tree
    /// version 3, so X15's `Upcast` stays in the ids, in every transaction of
    /// the section.
    #[test]
    fn a_v4_section_reads_its_transactions_at_3() {
        let parsed =
            parse_block_transactions(&raw_section(4, &[EXTENSION_TX, REGISTER_TX])).unwrap();
        assert_eq!(parsed.block_version, 4);
        assert_eq!(ids(&parsed), IDS_AT_3);
    }

    /// Below block version 4 the same bytes are read at 0, where X15's
    /// `Upcast` is written as `1`: the ids are others. Version 1 carries no
    /// version framing and reads at 0 as well.
    #[test]
    fn below_v4_a_section_reads_the_same_transactions_at_0() {
        for block_version in [1, 2, 3] {
            let body = raw_section(block_version, &[EXTENSION_TX, REGISTER_TX]);
            let parsed = parse_block_transactions(&body).unwrap();
            assert_eq!(parsed.block_version, block_version);
            assert_eq!(ids(&parsed), IDS_AT_0, "block version {block_version}");
        }
    }

    /// `Header.version` is a byte: 255 parses, 256 does not, as in
    /// `enr_chain::section_id_from_body`.
    #[test]
    fn a_block_version_beyond_u8_is_a_parse_error() {
        let parsed = parse_block_transactions(&raw_section(255, &[EXTENSION_TX])).unwrap();
        assert_eq!(parsed.block_version, 255);
        match parse_block_transactions(&raw_section(256, &[EXTENSION_TX])) {
            Err(ValidationError::SectionParse {
                section_type,
                reason,
            }) => {
                assert_eq!(section_type, 102);
                assert!(
                    reason.contains("block version 256 does not fit u8"),
                    "unexpected reason: {reason}"
                );
            }
            Err(other) => panic!("expected SectionParse, got {other:?}"),
            Ok(_) => panic!("block version 256 must not parse"),
        }
    }

    /// `Transaction::sigma_parse` starts each transaction on a fresh parse
    /// state (facts/validation.md § Transaction rules taken from ergo-lib,
    /// "Per transaction"), and reading at a tree version leaves it so: a
    /// `ValUse(1)` without a `ValDef` of its own is rejected after a
    /// transaction that defined 1, at both read versions.
    #[test]
    fn each_transaction_starts_on_a_fresh_parse_state_at_either_version() {
        // Box `0…0` spent with an empty proof into one 1 ERG output guarded by
        // `tree`.
        let tx = |tree: &str| {
            format!(
                "01{}0000000001{}{tree}010000",
                "00".repeat(32),
                "8094ebdc03"
            )
        };
        // `{ val v1 = sigmaProp(true); v1 }`, and `v1` with no definition
        let defines = tx("00d801d60108d37201");
        let uses_undefined = tx("007201");
        for block_version in [3, 4] {
            let both_define = raw_section(block_version, &[&defines, &defines]);
            assert!(
                parse_block_transactions(&both_define).is_ok(),
                "block version {block_version}"
            );
            match parse_block_transactions(&raw_section(
                block_version,
                &[&defines, &uses_undefined],
            )) {
                Err(ValidationError::SectionParse {
                    section_type: 102,
                    reason,
                }) => assert!(
                    reason.starts_with("transaction 1: "),
                    "block version {block_version}: {reason}"
                ),
                Err(other) => panic!("expected SectionParse, got {other:?}"),
                Ok(_) => panic!("block version {block_version}: an undefined ValUse parsed"),
            }
        }
    }

    /// The writer follows the JVM's serializer: version 3 from block version
    /// 3, version 0 below. X15's `Upcast` is the difference between the two.
    #[test]
    fn serialize_writes_a_v3_block_at_3_and_a_v2_block_at_0() {
        let tx = tx_read_at(EXTENSION_TX, ErgoTreeVersion::V3);
        for (block_version, written) in
            [(2, EXTENSION_TX_AT_0), (3, EXTENSION_TX), (4, EXTENSION_TX)]
        {
            let body =
                serialize_block_transactions(&HEADER_ID, block_version, std::slice::from_ref(&tx))
                    .unwrap();
            assert_eq!(
                hex::encode(body),
                hex::encode(raw_section(block_version, &[written])),
                "block version {block_version}"
            );
        }
    }

    /// A v4 section written and read back keeps the ids: both sides are at 3.
    /// A v3 one is written at 3 and read at 0, so X15's ids change: the JVM's
    /// own asymmetry (facts/validation.md § Section Parsing, "Writing").
    #[test]
    fn written_then_read_a_v4_section_keeps_the_ids_and_a_v3_one_does_not() {
        let txs = [
            tx_read_at(EXTENSION_TX, ErgoTreeVersion::V3),
            tx_read_at(REGISTER_TX, ErgoTreeVersion::V3),
        ];
        for (block_version, expected) in [(4, IDS_AT_3), (3, IDS_AT_0)] {
            let body = serialize_block_transactions(&HEADER_ID, block_version, &txs).unwrap();
            let parsed = parse_block_transactions(&body).unwrap();
            assert_eq!(ids(&parsed), expected, "block version {block_version}");
        }
    }

    #[test]
    fn serialize_rejects_a_block_version_beyond_u8() {
        match serialize_block_transactions(&HEADER_ID, 256, &[]) {
            Err(ValidationError::SectionParse {
                section_type,
                reason,
            }) => {
                assert_eq!(section_type, 102);
                assert!(
                    reason.contains("block version 256 does not fit u8"),
                    "unexpected reason: {reason}"
                );
            }
            Err(other) => panic!("expected SectionParse, got {other:?}"),
            Ok(_) => panic!("block version 256 must not be written"),
        }
    }
}

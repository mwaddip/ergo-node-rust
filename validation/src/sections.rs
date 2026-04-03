//! Parse raw block section bytes into typed structures.

/// Parsed ADProofs section (type 104).
pub struct ParsedAdProofs {
    pub header_id: [u8; 32],
    pub proof_bytes: Vec<u8>,
}

/// Parsed BlockTransactions section (type 102).
pub struct ParsedBlockTransactions {
    pub header_id: [u8; 32],
    pub block_version: u32,
    pub transactions: Vec<ergo_lib::chain::transaction::Transaction>,
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

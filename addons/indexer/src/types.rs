use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// Node REST API response types (deserialization)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub full_height: u32,
    pub headers_height: u32,
    pub best_full_header_id: String,
    pub network: String,
}

// ---------------------------------------------------------------------------
// Database row types (API response types)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BlockRow {
    pub height: u64,
    pub header_id: String,
    pub timestamp: u64,
    pub difficulty: u64,
    pub miner_pk: String,
    pub block_size: u32,
    pub tx_count: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TxRow {
    pub tx_id: String,
    pub header_id: String,
    pub height: u64,
    pub tx_index: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoxRow {
    pub box_id: String,
    pub tx_id: String,
    pub header_id: String,
    pub height: u64,
    pub output_index: u32,
    pub ergo_tree: String,
    pub ergo_tree_hash: String,
    pub address: String,
    pub value: u64,
    pub spent_tx_id: Option<String>,
    pub spent_height: Option<u64>,
    pub tokens: Vec<BoxTokenRow>,
    pub registers: Vec<RegisterRow>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoxTokenRow {
    pub token_id: String,
    pub amount: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegisterRow {
    pub register_id: u8,
    pub serialized: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenRow {
    pub token_id: String,
    pub minting_tx_id: String,
    pub minting_height: u64,
    pub name: Option<String>,
    pub description: Option<String>,
    pub decimals: Option<i32>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HolderRow {
    pub address: String,
    pub amount: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub nano_ergs: u64,
    pub tokens: Vec<TokenBalance>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenBalance {
    pub token_id: String,
    pub amount: u64,
    pub name: Option<String>,
    pub decimals: Option<i32>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStats {
    pub indexed_height: u64,
    pub total_blocks: u64,
    pub total_transactions: u64,
    pub total_boxes: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DailyStats {
    pub date: String,
    pub tx_count: u64,
    pub block_count: u64,
    pub volume: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexerInfo {
    pub indexed_height: u64,
    pub node_height: u32,
    pub backend: String,
    pub uptime_secs: u64,
}

/// Paginated response wrapper.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Page<T: Serialize> {
    pub items: Vec<T>,
    pub total: u64,
}

// ---------------------------------------------------------------------------
// Intermediate types for the sync pipeline (not API-facing)
// ---------------------------------------------------------------------------

/// A fully parsed block ready for DB insertion.
pub struct IndexedBlock {
    pub height: u64,
    pub header_id: [u8; 32],
    pub timestamp: u64,
    pub difficulty: u64,
    pub miner_pk: Vec<u8>,
    pub block_size: u32,
    pub transactions: Vec<IndexedTx>,
}

pub struct IndexedTx {
    pub tx_id: [u8; 32],
    pub tx_index: u32,
    pub size: u32,
    pub inputs: Vec<InputRef>,
    pub outputs: Vec<IndexedBox>,
}

pub struct InputRef {
    pub box_id: [u8; 32],
}

pub struct IndexedBox {
    pub box_id: [u8; 32],
    pub output_index: u32,
    pub ergo_tree: Vec<u8>,
    pub ergo_tree_hash: [u8; 32],
    pub address: String,
    pub value: u64,
    pub tokens: Vec<IndexedToken>,
    pub registers: Vec<IndexedRegister>,
    /// If this output mints a new token, holds the EIP-4 metadata.
    pub minted_token: Option<MintedToken>,
}

pub struct IndexedToken {
    pub token_id: [u8; 32],
    pub amount: u64,
}

pub struct IndexedRegister {
    pub register_id: u8,
    pub serialized: Vec<u8>,
}

pub struct MintedToken {
    pub token_id: [u8; 32],
    pub name: Option<String>,
    pub description: Option<String>,
    pub decimals: Option<i32>,
}

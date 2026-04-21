//! JSON types for the local node and JVM peer REST APIs.

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Local node responses (our node)
// ---------------------------------------------------------------------------

/// GET /info response from our local node.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LocalNodeInfo {
    pub headers_height: u32,
    pub full_height: u32,
    #[serde(default)]
    pub downloaded_height: u32,
    pub state_type: String,
}

/// GET /peers/api-urls response entry.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PeerApiUrl {
    pub url: String,
}

/// POST /ingest/modifiers response.
#[derive(Deserialize, Debug)]
pub struct IngestResponse {
    pub accepted: u32,
}

// ---------------------------------------------------------------------------
// JVM peer responses
// ---------------------------------------------------------------------------

/// GET /info from a JVM peer — only the fields we need.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PeerNodeInfo {
    /// JVM uses "headersHeight" in its /info response.
    pub headers_height: u32,
    pub full_height: u32,
}

/// A full block from `GET /blocks/{headerId}` or `POST /blocks/headerIds`.
///
/// chainSlice returns headers only — full blocks come from headerIds.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JvmFullBlock {
    pub header: serde_json::Value,
    pub block_transactions: JvmBlockTransactions,
    #[serde(default)]
    pub ad_proofs: Option<JvmAdProofs>,
    pub extension: JvmExtension,
}

/// BlockTransactions section from JVM JSON.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JvmBlockTransactions {
    pub transactions: Vec<serde_json::Value>,
    #[serde(default = "default_block_version")]
    pub block_version: u32,
}

fn default_block_version() -> u32 {
    1
}

/// ADProofs section from JVM JSON.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JvmAdProofs {
    pub proof_bytes: String,
}

/// Extension section from JVM JSON.
///
/// Fields are `[[key_hex, value_hex], ...]` — pairs of hex strings.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JvmExtension {
    pub fields: Vec<(String, String)>,
}

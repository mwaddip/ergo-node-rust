use serde::Serialize;

/// Standard error response matching the JVM node's format.
#[derive(Serialize)]
pub struct ApiError {
    pub error: u16,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// GET /info response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub name: String,
    pub app_version: String,
    pub network: String,
    pub full_height: u32,
    pub headers_height: u32,
    pub downloaded_height: u32,
    pub best_full_header_id: String,
    pub best_header_id: String,
    pub state_root: String,
    pub state_type: String,
    pub peers_count: usize,
    pub unconfirmed_count: usize,
    pub is_mining: bool,
    pub current_time: u64,
}

/// GET /emission/at/{height} response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmissionInfo {
    pub miner_reward: u64,
    pub total_coins_issued: u64,
    pub total_remain_coins: u64,
}

/// Fee recommendation response.
#[derive(Serialize)]
pub struct FeeResponse {
    pub fee: u64,
}

/// GET /peers/api-urls response entry.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerApiUrl {
    pub peer_id: u64,
    pub url: String,
}


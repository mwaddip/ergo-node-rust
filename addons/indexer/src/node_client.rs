use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::types::NodeInfo;

pub struct NodeClient {
    client: Client,
    base_url: String,
}

/// Header JSON from GET /blocks/{id}/header.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderJson {
    pub id: String,
    pub height: u64,
    pub timestamp: u64,
    #[serde(alias = "nBits")]
    pub n_bits: u64,
    pub pow_solutions: PowSolutions,
}

#[derive(Deserialize)]
pub struct PowSolutions {
    pub pk: String,
}

/// Block transactions from GET /blocks/{id}/transactions.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockTransactionsJson {
    pub header_id: String,
    pub transactions: Vec<serde_json::Value>,
}

impl NodeClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("failed to build HTTP client")?;
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self { client, base_url })
    }

    /// GET /info
    pub async fn info(&self) -> Result<NodeInfo> {
        let resp = self
            .client
            .get(format!("{}/info", self.base_url))
            .send()
            .await
            .context("GET /info failed")?
            .error_for_status()
            .context("GET /info returned error")?;
        resp.json().await.context("GET /info parse failed")
    }

    /// GET /info/wait?after={height} — long-poll.
    /// Returns None on 204 (timeout), Some(info) on new block.
    pub async fn info_wait(&self, after: u64) -> Result<Option<NodeInfo>> {
        let resp = self
            .client
            .get(format!("{}/info/wait?after={}", self.base_url, after))
            .timeout(std::time::Duration::from_secs(35))
            .send()
            .await
            .context("GET /info/wait failed")?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let info = resp
            .error_for_status()
            .context("GET /info/wait error")?
            .json()
            .await
            .context("GET /info/wait parse failed")?;
        Ok(Some(info))
    }

    /// GET /blocks/at/{height}
    pub async fn block_ids_at(&self, height: u64) -> Result<Vec<String>> {
        let resp = self
            .client
            .get(format!("{}/blocks/at/{}", self.base_url, height))
            .send()
            .await
            .context("GET /blocks/at failed")?
            .error_for_status()
            .context("GET /blocks/at error")?;
        resp.json().await.context("GET /blocks/at parse failed")
    }

    /// GET /blocks/{header_id}/header
    pub async fn header(&self, header_id: &str) -> Result<HeaderJson> {
        let resp = self
            .client
            .get(format!("{}/blocks/{}/header", self.base_url, header_id))
            .send()
            .await
            .context("GET header failed")?
            .error_for_status()
            .context("GET header error")?;
        resp.json().await.context("GET header parse failed")
    }

    /// GET /blocks/{header_id}/transactions
    pub async fn transactions(&self, header_id: &str) -> Result<BlockTransactionsJson> {
        let resp = self
            .client
            .get(format!(
                "{}/blocks/{}/transactions",
                self.base_url, header_id
            ))
            .send()
            .await
            .context("GET transactions failed")?
            .error_for_status()
            .context("GET transactions error")?;
        resp.json()
            .await
            .context("GET transactions parse failed")
    }
}

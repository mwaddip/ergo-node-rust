//! HTTP client for fetching from JVM peer REST API.
//!
//! Two phases:
//! 1. Headers via GET /blocks/chainSlice (batches of up to 2000)
//! 2. Full blocks via POST /blocks/headerIds (batches by header ID)

use std::time::Duration;

use anyhow::{Context, Result, bail};
use ergo_chain_types::Header;
use reqwest::Client;

use crate::types::{JvmFullBlock, PeerNodeInfo};

/// Per-request timeout — chainSlice can take 10-15s for 2000 headers on
/// slower JVM peers, so we need headroom.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Max header IDs per POST /blocks/headerIds request.
const BLOCK_BATCH_SIZE: usize = 32;

/// Peer fetcher — wraps a reqwest client targeting a single JVM peer.
pub struct PeerFetcher {
    client: Client,
    base_url: String,
}

impl PeerFetcher {
    pub fn new(base_url: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .context("build reqwest client")?;
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self { client, base_url })
    }

    /// GET /info — peer's current chain state.
    pub async fn peer_info(&self) -> Result<PeerNodeInfo> {
        let url = format!("{}/info", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("GET /info")?;
        if !resp.status().is_success() {
            bail!("GET /info returned {}", resp.status());
        }
        resp.json().await.context("parse /info JSON")
    }

    /// GET /blocks/chainSlice — fetch a batch of headers by height range.
    ///
    /// Returns parsed Headers. The JVM returns header JSON objects;
    /// sigma-rust deserializes them directly.
    pub async fn chain_slice(
        &self,
        from_height: u32,
        to_height: u32,
    ) -> Result<Vec<Header>> {
        let url = format!(
            "{}/blocks/chainSlice?fromHeight={}&toHeight={}",
            self.base_url, from_height, to_height
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET chainSlice {from_height}..{to_height}"))?;
        if !resp.status().is_success() {
            bail!(
                "chainSlice {from_height}..{to_height} returned {}",
                resp.status()
            );
        }
        let headers: Vec<Header> = resp
            .json()
            .await
            .with_context(|| format!("parse chainSlice {from_height}..{to_height}"))?;
        Ok(headers)
    }

    /// POST /blocks/headerIds — fetch full blocks by header ID batch.
    ///
    /// JVM returns `Seq[ErgoFullBlock]` as JSON array.
    pub async fn blocks_by_ids(
        &self,
        header_ids: &[String],
    ) -> Result<Vec<JvmFullBlock>> {
        let url = format!("{}/blocks/headerIds", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(header_ids)
            .send()
            .await
            .with_context(|| {
                format!("POST /blocks/headerIds ({} ids)", header_ids.len())
            })?;
        if !resp.status().is_success() {
            bail!(
                "POST /blocks/headerIds returned {} ({} ids)",
                resp.status(),
                header_ids.len()
            );
        }
        resp.json()
            .await
            .with_context(|| format!("parse headerIds response ({} ids)", header_ids.len()))
    }

}

/// Batch size for POST /blocks/headerIds.
pub fn block_batch_size() -> usize {
    BLOCK_BATCH_SIZE
}

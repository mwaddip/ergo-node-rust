//! Push modifiers to the local node via POST /ingest/modifiers.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;

use crate::types::IngestResponse;
use crate::wire::{Modifier, encode_ingest_body};

/// Ingest client targeting the local node.
pub struct IngestClient {
    client: Client,
    url: String,
}

impl IngestClient {
    pub fn new(node_url: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("build ingest client")?;
        let base = node_url.trim_end_matches('/');
        Ok(Self {
            client,
            url: format!("{base}/ingest/modifiers"),
        })
    }

    /// Push a batch of modifiers. Returns the number accepted by the node.
    pub async fn push(&self, modifiers: &[Modifier]) -> Result<u32> {
        if modifiers.is_empty() {
            return Ok(0);
        }
        let body = encode_ingest_body(modifiers);
        let resp = self
            .client
            .post(&self.url)
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .context("POST /ingest/modifiers")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("ingest returned {status}: {text}");
        }
        let r: IngestResponse = resp.json().await.context("parse ingest response")?;
        Ok(r.accepted)
    }
}

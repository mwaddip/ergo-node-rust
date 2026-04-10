use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::types::*;

#[async_trait]
pub trait IndexerDb: Send + Sync {
    // Write path
    async fn get_indexed_height(&self) -> Result<Option<u64>>;
    async fn get_block_id_at(&self, height: u64) -> Result<Option<Vec<u8>>>;
    async fn insert_block(&self, block: &IndexedBlock) -> Result<()>;
    async fn rollback_to(&self, height: u64) -> Result<()>;

    // Read path
    async fn get_block_by_height(&self, height: u64) -> Result<Option<BlockRow>>;
    async fn get_block_by_id(&self, id: &[u8]) -> Result<Option<BlockRow>>;
    async fn get_blocks(&self, offset: u64, limit: u64) -> Result<Page<BlockRow>>;
    async fn get_transactions_for_block(&self, header_id: &[u8]) -> Result<Vec<TxRow>>;
    async fn get_transaction(&self, tx_id: &[u8]) -> Result<Option<TxRow>>;
    async fn get_box(&self, box_id: &[u8]) -> Result<Option<BoxRow>>;
    async fn get_unspent_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<BoxRow>>;
    async fn get_boxes_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<BoxRow>>;
    async fn get_balance(&self, addr: &str) -> Result<Balance>;
    async fn get_txs_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<TxRow>>;
    async fn get_unspent_by_ergo_tree(&self, hash: &[u8], offset: u64, limit: u64) -> Result<Page<BoxRow>>;
    async fn get_token(&self, token_id: &[u8]) -> Result<Option<TokenRow>>;
    async fn get_token_holders(&self, token_id: &[u8], offset: u64, limit: u64) -> Result<Page<HolderRow>>;
    async fn get_token_boxes(&self, token_id: &[u8], offset: u64, limit: u64) -> Result<Page<BoxRow>>;
    async fn get_tokens(&self, offset: u64, limit: u64) -> Result<Page<TokenRow>>;
    async fn get_stats(&self) -> Result<NetworkStats>;
    async fn get_daily_stats(&self, days: u32) -> Result<Vec<DailyStats>>;
}

/// Open database based on URL scheme. postgres:// → PgDb, else → SqliteDb.
pub async fn open_db(url: &str) -> Result<Arc<dyn IndexerDb>> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        #[cfg(feature = "postgres")]
        {
            let db = postgres::PgDb::connect(url).await?;
            return Ok(Arc::new(db));
        }
        #[cfg(not(feature = "postgres"))]
        anyhow::bail!("PostgreSQL support requires --features postgres");
    }

    #[cfg(feature = "sqlite")]
    {
        let db = sqlite::SqliteDb::open(url)?;
        return Ok(Arc::new(db));
    }
    #[cfg(not(feature = "sqlite"))]
    anyhow::bail!("No database backend enabled — build with --features sqlite or --features postgres");
}

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

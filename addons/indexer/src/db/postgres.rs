use std::str::FromStr;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::PgPool;

use crate::db::{DbMemoryStats, IndexerDb};
use crate::types::*;

type BlockTuple = (i64, Vec<u8>, i64, i64, Vec<u8>, i32, i32);
type TxTuple = (Vec<u8>, Vec<u8>, i64, i32, i32);
type BoxTuple = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i32,
    Vec<u8>,
    Vec<u8>,
    String,
    i64,
    Option<Vec<u8>>,
    Option<i64>,
);
type TokenTuple = (
    Vec<u8>,
    Vec<u8>,
    i64,
    Option<String>,
    Option<String>,
    Option<i32>,
);

fn tuple_to_block((h, hid, ts, d, mpk, bs, tc): BlockTuple) -> BlockRow {
    BlockRow {
        height: h as u64,
        header_id: hex::encode(hid),
        timestamp: ts as u64,
        difficulty: d as u64,
        miner_pk: hex::encode(mpk),
        block_size: bs as u32,
        tx_count: tc as u32,
    }
}

fn tuple_to_tx((tid, hid, h, ti, s): TxTuple) -> TxRow {
    TxRow {
        tx_id: hex::encode(tid),
        header_id: hex::encode(hid),
        height: h as u64,
        tx_index: ti as u32,
        size: s as u32,
    }
}

/// Build a BoxRow from a sqlx tuple. Caller is responsible for filling tokens/registers.
fn tuple_to_box_bare((bid, tid, hid, h, oi, et, eth, addr, v, stid, sh): BoxTuple) -> BoxRow {
    BoxRow {
        box_id: hex::encode(bid),
        tx_id: hex::encode(tid),
        header_id: hex::encode(hid),
        height: h as u64,
        output_index: oi as u32,
        ergo_tree: hex::encode(et),
        ergo_tree_hash: hex::encode(eth),
        address: addr,
        value: v as u64,
        spent_tx_id: stid.map(hex::encode),
        spent_height: sh.map(|h| h as u64),
        tokens: vec![],
        registers: vec![],
    }
}

fn tuple_to_token((tid, mtid, mh, n, d, dec): TokenTuple) -> TokenRow {
    TokenRow {
        token_id: hex::encode(tid),
        minting_tx_id: hex::encode(mtid),
        minting_height: mh as u64,
        name: n,
        description: d,
        decimals: dec,
    }
}

const CREATE_TABLES_PG: &str = "
CREATE TABLE IF NOT EXISTS indexer_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS blocks (
    height     BIGINT PRIMARY KEY,
    header_id  BYTEA NOT NULL UNIQUE,
    timestamp  BIGINT NOT NULL,
    difficulty BIGINT NOT NULL,
    miner_pk   BYTEA NOT NULL,
    block_size INT NOT NULL,
    tx_count   INT NOT NULL
);

CREATE TABLE IF NOT EXISTS transactions (
    tx_id     BYTEA PRIMARY KEY,
    header_id BYTEA NOT NULL REFERENCES blocks(header_id),
    height    BIGINT NOT NULL,
    tx_index  INT NOT NULL,
    size      INT NOT NULL
);

CREATE TABLE IF NOT EXISTS boxes (
    box_id         BYTEA PRIMARY KEY,
    tx_id          BYTEA NOT NULL REFERENCES transactions(tx_id),
    header_id      BYTEA NOT NULL,
    height         BIGINT NOT NULL,
    output_index   INT NOT NULL,
    ergo_tree      BYTEA NOT NULL,
    ergo_tree_hash BYTEA NOT NULL,
    address        TEXT NOT NULL,
    value          BIGINT NOT NULL,
    spent_tx_id    BYTEA,
    spent_height   BIGINT
);

CREATE TABLE IF NOT EXISTS box_tokens (
    box_id   BYTEA NOT NULL REFERENCES boxes(box_id),
    token_id BYTEA NOT NULL,
    amount   BIGINT NOT NULL,
    PRIMARY KEY (box_id, token_id)
);

CREATE TABLE IF NOT EXISTS box_registers (
    box_id      BYTEA NOT NULL REFERENCES boxes(box_id),
    register_id SMALLINT NOT NULL,
    serialized  BYTEA NOT NULL,
    PRIMARY KEY (box_id, register_id)
);

CREATE TABLE IF NOT EXISTS tokens (
    token_id       BYTEA PRIMARY KEY,
    minting_tx_id  BYTEA NOT NULL,
    minting_height BIGINT NOT NULL,
    name           TEXT,
    description    TEXT,
    decimals       INT
);

CREATE INDEX IF NOT EXISTS idx_boxes_address_unspent ON boxes(address) WHERE spent_tx_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_boxes_ergo_tree_hash_unspent ON boxes(ergo_tree_hash) WHERE spent_tx_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_boxes_address ON boxes(address);
CREATE INDEX IF NOT EXISTS idx_box_tokens_token_id ON box_tokens(token_id);
CREATE INDEX IF NOT EXISTS idx_transactions_height ON transactions(height);
CREATE INDEX IF NOT EXISTS idx_boxes_height ON boxes(height);
";

pub struct PgDb {
    pool: PgPool,
}

impl PgDb {
    pub async fn connect(url: &str) -> Result<Self> {
        let opts = PgConnectOptions::from_str(url)
            .with_context(|| format!("invalid PostgreSQL URL: {url}"))?;
        let opts = apply_libpq_env(opts, |k| std::env::var(k).ok())?;
        let pool = PgPool::connect_with(opts)
            .await
            .context("failed to connect to PostgreSQL")?;
        let db = Self { pool };
        db.run_migrations().await?;
        Ok(db)
    }

    async fn run_migrations(&self) -> Result<()> {
        // Run each statement individually (PG doesn't support multi-statement execute_batch like SQLite)
        for stmt in CREATE_TABLES_PG.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() {
                sqlx::query(stmt)
                    .execute(&self.pool)
                    .await
                    .with_context(|| {
                        format!("migration failed: {}", &stmt[..stmt.len().min(80)])
                    })?;
            }
        }
        // Initialize schema version
        sqlx::query("INSERT INTO indexer_state (key, value) VALUES ('schema_version', '1') ON CONFLICT DO NOTHING")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl IndexerDb for PgDb {
    async fn get_indexed_height(&self) -> Result<Option<u64>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM indexer_state WHERE key = 'height'")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(s,)| s.parse().ok()))
    }

    async fn get_block_id_at(&self, height: u64) -> Result<Option<Vec<u8>>> {
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT header_id FROM blocks WHERE height = $1")
                .bind(height as i64)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(id,)| id))
    }

    async fn insert_block(&self, block: &IndexedBlock) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO blocks (height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(block.height as i64)
        .bind(block.header_id.as_slice())
        .bind(block.timestamp as i64)
        .bind(block.difficulty as i64)
        .bind(block.miner_pk.as_slice())
        .bind(block.block_size as i32)
        .bind(block.transactions.len() as i32)
        .execute(&mut *tx)
        .await?;

        for itx in &block.transactions {
            sqlx::query(
                "INSERT INTO transactions (tx_id, header_id, height, tx_index, size)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(itx.tx_id.as_slice())
            .bind(block.header_id.as_slice())
            .bind(block.height as i64)
            .bind(itx.tx_index as i32)
            .bind(itx.size as i32)
            .execute(&mut *tx)
            .await?;

            for input in &itx.inputs {
                sqlx::query(
                    "UPDATE boxes SET spent_tx_id = $1, spent_height = $2 WHERE box_id = $3",
                )
                .bind(itx.tx_id.as_slice())
                .bind(block.height as i64)
                .bind(input.box_id.as_slice())
                .execute(&mut *tx)
                .await?;
            }

            for out in &itx.outputs {
                sqlx::query(
                    "INSERT INTO boxes (box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                )
                .bind(out.box_id.as_slice())
                .bind(itx.tx_id.as_slice())
                .bind(block.header_id.as_slice())
                .bind(block.height as i64)
                .bind(out.output_index as i32)
                .bind(out.ergo_tree.as_slice())
                .bind(out.ergo_tree_hash.as_slice())
                .bind(&out.address)
                .bind(out.value as i64)
                .execute(&mut *tx)
                .await?;

                for tok in &out.tokens {
                    sqlx::query(
                        "INSERT INTO box_tokens (box_id, token_id, amount) VALUES ($1, $2, $3)",
                    )
                    .bind(out.box_id.as_slice())
                    .bind(tok.token_id.as_slice())
                    .bind(tok.amount as i64)
                    .execute(&mut *tx)
                    .await?;
                }

                for reg in &out.registers {
                    sqlx::query(
                        "INSERT INTO box_registers (box_id, register_id, serialized) VALUES ($1, $2, $3)",
                    )
                    .bind(out.box_id.as_slice())
                    .bind(reg.register_id as i16)
                    .bind(reg.serialized.as_slice())
                    .execute(&mut *tx)
                    .await?;
                }

                if let Some(minted) = &out.minted_token {
                    sqlx::query(
                        "INSERT INTO tokens (token_id, minting_tx_id, minting_height, name, description, decimals)
                         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
                    )
                    .bind(minted.token_id.as_slice())
                    .bind(itx.tx_id.as_slice())
                    .bind(block.height as i64)
                    .bind(&minted.name)
                    .bind(&minted.description)
                    .bind(minted.decimals)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }

        sqlx::query(
            "INSERT INTO indexer_state (key, value) VALUES ('height', $1)
             ON CONFLICT (key) DO UPDATE SET value = $1",
        )
        .bind((block.height as i64).to_string())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn rollback_to(&self, height: u64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let h = height as i64;

        sqlx::query(
            "UPDATE boxes SET spent_tx_id = NULL, spent_height = NULL WHERE spent_height > $1",
        )
        .bind(h)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM box_registers WHERE box_id IN (SELECT box_id FROM boxes WHERE height > $1)")
            .bind(h).execute(&mut *tx).await?;
        sqlx::query(
            "DELETE FROM box_tokens WHERE box_id IN (SELECT box_id FROM boxes WHERE height > $1)",
        )
        .bind(h)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM boxes WHERE height > $1")
            .bind(h)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM tokens WHERE minting_height > $1")
            .bind(h)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM transactions WHERE height > $1")
            .bind(h)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM blocks WHERE height > $1")
            .bind(h)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO indexer_state (key, value) VALUES ('height', $1) ON CONFLICT (key) DO UPDATE SET value = $1")
            .bind(h.to_string()).execute(&mut *tx).await?;

        tx.commit().await?;
        Ok(())
    }

    // Read-path: implemented as pass-through queries using sqlx::query_as tuples
    // Pattern: query → map to row type → return

    async fn get_block_by_height(&self, height: u64) -> Result<Option<BlockRow>> {
        let row: Option<BlockTuple> = sqlx::query_as(
            "SELECT height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count FROM blocks WHERE height = $1",
        ).bind(height as i64).fetch_optional(&self.pool).await?;
        Ok(row.map(tuple_to_block))
    }

    async fn get_block_by_id(&self, id: &[u8]) -> Result<Option<BlockRow>> {
        let row: Option<BlockTuple> = sqlx::query_as(
            "SELECT height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count FROM blocks WHERE header_id = $1",
        ).bind(id).fetch_optional(&self.pool).await?;
        Ok(row.map(tuple_to_block))
    }

    async fn get_blocks(&self, offset: u64, limit: u64) -> Result<Page<BlockRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM blocks")
            .fetch_one(&self.pool)
            .await?;
        let rows: Vec<BlockTuple> = sqlx::query_as(
            "SELECT height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count FROM blocks ORDER BY height DESC LIMIT $1 OFFSET $2",
        ).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let items = rows.into_iter().map(tuple_to_block).collect();
        Ok(Page {
            items,
            total: total as u64,
        })
    }

    async fn get_transactions_for_block(&self, header_id: &[u8]) -> Result<Vec<TxRow>> {
        let rows: Vec<TxTuple> = sqlx::query_as(
            "SELECT tx_id, header_id, height, tx_index, size FROM transactions WHERE header_id = $1 ORDER BY tx_index",
        ).bind(header_id).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(tuple_to_tx).collect())
    }

    async fn get_transaction(&self, tx_id: &[u8]) -> Result<Option<TxRow>> {
        let row: Option<TxTuple> = sqlx::query_as(
            "SELECT tx_id, header_id, height, tx_index, size FROM transactions WHERE tx_id = $1",
        )
        .bind(tx_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(tuple_to_tx))
    }

    async fn get_box(&self, box_id: &[u8]) -> Result<Option<BoxRow>> {
        let row: Option<BoxTuple> = sqlx::query_as(
            "SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value, spent_tx_id, spent_height FROM boxes WHERE box_id = $1",
        ).bind(box_id).fetch_optional(&self.pool).await?;
        match row {
            None => Ok(None),
            Some(tup) => {
                let bid_bytes = tup.0.clone();
                let mut row = tuple_to_box_bare(tup);
                row.tokens = self.fetch_box_tokens(&bid_bytes).await?;
                row.registers = self.fetch_box_registers(&bid_bytes).await?;
                Ok(Some(row))
            }
        }
    }

    async fn get_unspent_by_address(
        &self,
        addr: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Page<BoxRow>> {
        let (total,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM boxes WHERE address = $1 AND spent_tx_id IS NULL")
                .bind(addr)
                .fetch_one(&self.pool)
                .await?;
        let rows = self.fetch_boxes_query("SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value, spent_tx_id, spent_height FROM boxes WHERE address = $1 AND spent_tx_id IS NULL ORDER BY height DESC LIMIT $2 OFFSET $3", addr, limit, offset).await?;
        Ok(Page {
            items: rows,
            total: total as u64,
        })
    }

    async fn get_boxes_by_address(
        &self,
        addr: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Page<BoxRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boxes WHERE address = $1")
            .bind(addr)
            .fetch_one(&self.pool)
            .await?;
        let rows = self.fetch_boxes_query("SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value, spent_tx_id, spent_height FROM boxes WHERE address = $1 ORDER BY height DESC LIMIT $2 OFFSET $3", addr, limit, offset).await?;
        Ok(Page {
            items: rows,
            total: total as u64,
        })
    }

    async fn get_balance(&self, addr: &str) -> Result<Balance> {
        let (nano_ergs,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(value), 0) FROM boxes WHERE address = $1 AND spent_tx_id IS NULL",
        )
        .bind(addr)
        .fetch_one(&self.pool)
        .await?;
        let token_rows: Vec<(Vec<u8>, i64, Option<String>, Option<i32>)> = sqlx::query_as(
            "SELECT bt.token_id, SUM(bt.amount) as total, t.name, t.decimals FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id LEFT JOIN tokens t ON t.token_id = bt.token_id WHERE b.address = $1 AND b.spent_tx_id IS NULL GROUP BY bt.token_id, t.name, t.decimals",
        ).bind(addr).fetch_all(&self.pool).await?;
        let tokens = token_rows
            .into_iter()
            .map(|(tid, amt, name, dec)| TokenBalance {
                token_id: hex::encode(tid),
                amount: amt as u64,
                name,
                decimals: dec,
            })
            .collect();
        Ok(Balance {
            nano_ergs: nano_ergs as u64,
            tokens,
        })
    }

    async fn get_txs_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<TxRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(DISTINCT t.tx_id) FROM transactions t JOIN boxes b ON b.tx_id = t.tx_id WHERE b.address = $1").bind(addr).fetch_one(&self.pool).await?;
        let rows: Vec<TxTuple> = sqlx::query_as(
            "SELECT DISTINCT t.tx_id, t.header_id, t.height, t.tx_index, t.size FROM transactions t JOIN boxes b ON b.tx_id = t.tx_id WHERE b.address = $1 ORDER BY t.height DESC LIMIT $2 OFFSET $3",
        ).bind(addr).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let items = rows.into_iter().map(tuple_to_tx).collect();
        Ok(Page {
            items,
            total: total as u64,
        })
    }

    async fn get_unspent_by_ergo_tree(
        &self,
        hash: &[u8],
        offset: u64,
        limit: u64,
    ) -> Result<Page<BoxRow>> {
        let (total,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM boxes WHERE ergo_tree_hash = $1 AND spent_tx_id IS NULL",
        )
        .bind(hash)
        .fetch_one(&self.pool)
        .await?;
        let rows: Vec<BoxTuple> = sqlx::query_as(
            "SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value, spent_tx_id, spent_height FROM boxes WHERE ergo_tree_hash = $1 AND spent_tx_id IS NULL ORDER BY height DESC LIMIT $2 OFFSET $3",
        ).bind(hash).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let mut items: Vec<BoxRow> = rows.into_iter().map(tuple_to_box_bare).collect();
        for bx in &mut items {
            let bid = hex::decode(&bx.box_id)?;
            bx.tokens = self.fetch_box_tokens(&bid).await?;
            bx.registers = self.fetch_box_registers(&bid).await?;
        }
        Ok(Page {
            items,
            total: total as u64,
        })
    }

    async fn get_token(&self, token_id: &[u8]) -> Result<Option<TokenRow>> {
        let row: Option<TokenTuple> = sqlx::query_as(
            "SELECT token_id, minting_tx_id, minting_height, name, description, decimals FROM tokens WHERE token_id = $1",
        ).bind(token_id).fetch_optional(&self.pool).await?;
        Ok(row.map(tuple_to_token))
    }

    async fn get_token_holders(
        &self,
        token_id: &[u8],
        offset: u64,
        limit: u64,
    ) -> Result<Page<HolderRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(DISTINCT b.address) FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id WHERE bt.token_id = $1 AND b.spent_tx_id IS NULL").bind(token_id).fetch_one(&self.pool).await?;
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT b.address, SUM(bt.amount) as total FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id WHERE bt.token_id = $1 AND b.spent_tx_id IS NULL GROUP BY b.address ORDER BY total DESC LIMIT $2 OFFSET $3",
        ).bind(token_id).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let items = rows
            .into_iter()
            .map(|(addr, amt)| HolderRow {
                address: addr,
                amount: amt as u64,
            })
            .collect();
        Ok(Page {
            items,
            total: total as u64,
        })
    }

    async fn get_token_boxes(
        &self,
        token_id: &[u8],
        offset: u64,
        limit: u64,
    ) -> Result<Page<BoxRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id WHERE bt.token_id = $1 AND b.spent_tx_id IS NULL").bind(token_id).fetch_one(&self.pool).await?;
        let rows: Vec<BoxTuple> = sqlx::query_as(
            "SELECT b.box_id, b.tx_id, b.header_id, b.height, b.output_index, b.ergo_tree, b.ergo_tree_hash, b.address, b.value, b.spent_tx_id, b.spent_height FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id WHERE bt.token_id = $1 AND b.spent_tx_id IS NULL ORDER BY b.height DESC LIMIT $2 OFFSET $3",
        ).bind(token_id).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let mut items: Vec<BoxRow> = rows.into_iter().map(tuple_to_box_bare).collect();
        for bx in &mut items {
            let bid = hex::decode(&bx.box_id)?;
            bx.tokens = self.fetch_box_tokens(&bid).await?;
            bx.registers = self.fetch_box_registers(&bid).await?;
        }
        Ok(Page {
            items,
            total: total as u64,
        })
    }

    async fn get_tokens(&self, offset: u64, limit: u64) -> Result<Page<TokenRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tokens")
            .fetch_one(&self.pool)
            .await?;
        let rows: Vec<TokenTuple> = sqlx::query_as(
            "SELECT token_id, minting_tx_id, minting_height, name, description, decimals FROM tokens ORDER BY minting_height DESC LIMIT $1 OFFSET $2",
        ).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let items = rows.into_iter().map(tuple_to_token).collect();
        Ok(Page {
            items,
            total: total as u64,
        })
    }

    async fn get_stats(&self) -> Result<NetworkStats> {
        let (ih,): (i64,) = sqlx::query_as("SELECT COALESCE(MAX(height), 0) FROM blocks")
            .fetch_one(&self.pool)
            .await?;
        let (tb,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM blocks")
            .fetch_one(&self.pool)
            .await?;
        let (tt,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&self.pool)
            .await?;
        let (tbx,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boxes")
            .fetch_one(&self.pool)
            .await?;
        let (ttk,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tokens")
            .fetch_one(&self.pool)
            .await?;
        Ok(NetworkStats {
            indexed_height: ih as u64,
            total_blocks: tb as u64,
            total_transactions: tt as u64,
            total_boxes: tbx as u64,
            total_tokens: ttk as u64,
        })
    }

    async fn get_daily_stats(&self, days: u32) -> Result<Vec<DailyStats>> {
        let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
            "SELECT to_char(to_timestamp(b.timestamp / 1000), 'YYYY-MM-DD') as day, COUNT(DISTINCT t.tx_id), COUNT(DISTINCT b.height), COALESCE(SUM(bx.value), 0) FROM blocks b LEFT JOIN transactions t ON t.header_id = b.header_id LEFT JOIN boxes bx ON bx.tx_id = t.tx_id WHERE b.timestamp >= (EXTRACT(EPOCH FROM NOW()) - $1 * 86400) * 1000 GROUP BY day ORDER BY day DESC",
        ).bind(days as i64).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(date, tc, bc, vol)| DailyStats {
                date,
                tx_count: tc as u64,
                block_count: bc as u64,
                volume: vol as u64,
            })
            .collect())
    }

    async fn memory_stats(&self) -> DbMemoryStats {
        // Postgres holds its caches server-side, outside the indexer
        // process. Surface only what's true at the pool level here;
        // operators reading the server's `pg_stat_*` views get the rest.
        DbMemoryStats {
            backend: "postgres",
            on_disk_bytes: None,
            cache_bytes_per_conn: None,
            connection_count: self.pool.size(),
        }
    }
}

// Helper methods for PgDb
impl PgDb {
    async fn fetch_box_tokens(&self, box_id: &[u8]) -> Result<Vec<BoxTokenRow>> {
        let rows: Vec<(Vec<u8>, i64)> =
            sqlx::query_as("SELECT token_id, amount FROM box_tokens WHERE box_id = $1")
                .bind(box_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(tid, amt)| BoxTokenRow {
                token_id: hex::encode(tid),
                amount: amt as u64,
            })
            .collect())
    }

    async fn fetch_box_registers(&self, box_id: &[u8]) -> Result<Vec<RegisterRow>> {
        let rows: Vec<(i16, Vec<u8>)> =
            sqlx::query_as("SELECT register_id, serialized FROM box_registers WHERE box_id = $1")
                .bind(box_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(rid, ser)| RegisterRow {
                register_id: rid as u8,
                serialized: hex::encode(ser),
            })
            .collect())
    }

    async fn fetch_boxes_query(
        &self,
        sql: &str,
        addr: &str,
        limit: u64,
        offset: u64,
    ) -> Result<Vec<BoxRow>> {
        let rows: Vec<BoxTuple> = sqlx::query_as(sql)
            .bind(addr)
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?;
        let mut items: Vec<BoxRow> = rows.into_iter().map(tuple_to_box_bare).collect();
        for bx in &mut items {
            let bid = hex::decode(&bx.box_id)?;
            bx.tokens = self.fetch_box_tokens(&bid).await?;
            bx.registers = self.fetch_box_registers(&bid).await?;
        }
        Ok(items)
    }
}

/// Layer libpq env vars on top of URL-parsed `PgConnectOptions`.
///
/// Precedence per `facts/indexer.md` v1.2.0 § Configuration: env vars > file.
/// sqlx's `PgConnectOptions::from_str(url)` parses URL components into
/// the options but does NOT consult env vars (only the no-URL `new()` path
/// does). This helper re-applies the libpq env vars on top so the contract's
/// ladder holds.
///
/// `~/.pgpass` is honored by sqlx natively inside `PgConnectOptions::from_str`
/// (`apply_pgpass()`). If `PGPASSWORD` is set, it overrides whatever
/// `.pgpass` filled in.
///
/// `getenv` is parameterized so tests can supply a synthetic environment
/// without mutating process-global state (Rust tests run in parallel).
pub(crate) fn apply_libpq_env<F>(mut opts: PgConnectOptions, getenv: F) -> Result<PgConnectOptions>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(host) = getenv("PGHOST") {
        opts = opts.host(&host);
    }
    if let Some(port) = getenv("PGPORT") {
        let port: u16 = port
            .parse()
            .with_context(|| format!("PGPORT is not a valid port number: {port}"))?;
        opts = opts.port(port);
    }
    if let Some(user) = getenv("PGUSER") {
        opts = opts.username(&user);
    }
    if let Some(password) = getenv("PGPASSWORD") {
        opts = opts.password(&password);
    }
    if let Some(dbname) = getenv("PGDATABASE") {
        opts = opts.database(&dbname);
    }
    if let Some(mode) = getenv("PGSSLMODE") {
        let mode = parse_ssl_mode(&mode)
            .with_context(|| format!("PGSSLMODE is not a recognized value: {mode}"))?;
        opts = opts.ssl_mode(mode);
    }
    Ok(opts)
}

fn parse_ssl_mode(s: &str) -> Result<PgSslMode> {
    // Match libpq's documented values; case-insensitive.
    match s.to_ascii_lowercase().as_str() {
        "disable" => Ok(PgSslMode::Disable),
        "allow" => Ok(PgSslMode::Allow),
        "prefer" => Ok(PgSslMode::Prefer),
        "require" => Ok(PgSslMode::Require),
        "verify-ca" => Ok(PgSslMode::VerifyCa),
        "verify-full" => Ok(PgSslMode::VerifyFull),
        other => anyhow::bail!("unknown PGSSLMODE value: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_opts() -> PgConnectOptions {
        // URL with no password — file URLs do not carry passwords per contract.
        PgConnectOptions::from_str("postgres://user@host:5432/db").expect("static URL parses")
    }

    fn mkenv(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    /// `env_overrides_file_pg_creds` — file URL has no password; PGPASSWORD
    /// env supplies it. `PgConnectOptions` exposes no `get_password()`, so we
    /// match the Debug repr (sqlx's derive includes the field).
    #[test]
    fn env_overrides_file_pg_creds() {
        let env = mkenv(&[("PGPASSWORD", "secret")]);
        let opts = apply_libpq_env(base_opts(), env).expect("env layering succeeds");
        let dbg = format!("{opts:?}");
        assert!(
            dbg.contains("secret"),
            "PGPASSWORD env did not land in resolved options; dbg={dbg}"
        );
    }

    #[test]
    fn env_overrides_host_port_user_db() {
        let env = mkenv(&[
            ("PGHOST", "envhost"),
            ("PGPORT", "6543"),
            ("PGUSER", "envuser"),
            ("PGDATABASE", "envdb"),
        ]);
        let opts = apply_libpq_env(base_opts(), env).expect("env layering succeeds");
        assert_eq!(opts.get_host(), "envhost", "PGHOST not applied");
        assert_eq!(opts.get_port(), 6543, "PGPORT not applied");
        assert_eq!(opts.get_username(), "envuser", "PGUSER not applied");
        assert_eq!(opts.get_database(), Some("envdb"), "PGDATABASE not applied");
    }

    #[test]
    fn env_pgport_invalid_errors() {
        let env = mkenv(&[("PGPORT", "not-a-port")]);
        let err = apply_libpq_env(base_opts(), env).expect_err("bad PGPORT must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("PGPORT"), "diagnostic: {msg}");
    }

    #[test]
    fn env_pgsslmode_recognized_values() {
        for v in [
            "disable",
            "allow",
            "prefer",
            "require",
            "verify-ca",
            "verify-full",
        ] {
            let env = mkenv(&[("PGSSLMODE", v)]);
            apply_libpq_env(base_opts(), env)
                .unwrap_or_else(|e| panic!("PGSSLMODE={v} failed: {e:#}"));
        }
    }

    #[test]
    fn env_pgsslmode_bad_value_errors() {
        let env = mkenv(&[("PGSSLMODE", "nonsense")]);
        let err = apply_libpq_env(base_opts(), env).expect_err("bad PGSSLMODE must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("PGSSLMODE"), "diagnostic: {msg}");
    }

    #[test]
    fn empty_env_is_identity() {
        let env = mkenv(&[]);
        let opts = apply_libpq_env(base_opts(), env).expect("identity case");
        assert_eq!(opts.get_host(), "host");
        assert_eq!(opts.get_port(), 5432);
        assert_eq!(opts.get_username(), "user");
        assert_eq!(opts.get_database(), Some("db"));
    }
}

// ---------------------------------------------------------------------------
// Migration backend
// ---------------------------------------------------------------------------
//
// `PostgresBackend` implements `crate::migrate::Backend` and is used ONLY by
// the `ergo-indexer-migratedb` binary. Separate from `PgDb` above — the
// running indexer's pool init path stays untouched.
//
// Per facts/indexer-migration.md § Per-block migration unit: each `apply_block`
// runs `SET CONSTRAINTS ALL DEFERRED` then INSERTs/UPDATEs the block's tables
// inside one transaction. The CREATE TABLE schema declares FKs as the default
// (NOT DEFERRABLE), so this is belt-and-suspenders against future schema
// changes that flip them to DEFERRABLE — today the ordering inside the apply
// block (blocks → transactions → boxes → registers/tokens) satisfies FK
// ordering directly.

use crate::migrate::{
    Backend as MigrateBackend, BlockData as MBlockData, BlockRow as MBlockRow,
    BoxRegisterRow as MBoxRegisterRow, BoxRow as MBoxRow, BoxSpentUpdate,
    BoxTokenRow as MBoxTokenRow, Cursor, TokenRow as MTokenRow, TransactionRow as MTransactionRow,
};

// Used by ergo-indexer-migratedb only; not constructed by the daemon bin.
#[allow(dead_code)]
pub struct PostgresBackend {
    pool: PgPool,
}

#[allow(dead_code)] // ergo-indexer-migratedb calls this; daemon bin does not
impl PostgresBackend {
    /// Open a PostgreSQL pool for migration.
    ///
    /// Intentionally separate from `PgDb::connect` — the migrator does NOT
    /// auto-create the schema on open; the migrator calls `init_schema` only
    /// when starting a fresh migration. The running indexer's startup path
    /// (which DOES create on open) is untouched.
    pub async fn new_for_migration(url: &str) -> Result<Self> {
        let opts = PgConnectOptions::from_str(url)
            .with_context(|| format!("invalid PostgreSQL URL: {url}"))?;
        let opts = apply_libpq_env(opts, |k| std::env::var(k).ok())?;
        let pool = PgPool::connect_with(opts)
            .await
            .context("failed to connect to PostgreSQL")?;
        Ok(Self { pool })
    }
}

/// Convert `Vec<u8>` from a BYTEA column into `[u8; 32]`. Postgres BYTEA has
/// no fixed-width subtype declared in the schema, so length is asserted at
/// read time — identical strategy to the SQLite side.
#[allow(dead_code)] // called only via PostgresBackend impl; daemon bin doesn't construct that
fn pg_vec_to_id32(v: Vec<u8>, col: &str) -> Result<[u8; 32]> {
    let len = v.len();
    v.try_into()
        .map_err(|_| anyhow::anyhow!("column {col}: expected 32 bytes, got {len}"))
}

#[async_trait]
impl MigrateBackend for PostgresBackend {
    async fn schema_version(&mut self) -> Result<Option<u32>> {
        // information_schema is the portable existence check — handles the
        // "freshly created database" case where indexer_state hasn't been
        // CREATEd yet.
        let exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'indexer_state')",
        )
        .fetch_one(&self.pool)
        .await?;
        if !exists.0 {
            return Ok(None);
        }
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM indexer_state WHERE key = 'schema_version'")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(s,)| s.parse().ok()))
    }

    async fn init_schema(&mut self, version: u32) -> Result<()> {
        for stmt in CREATE_TABLES_PG.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            sqlx::query(stmt)
                .execute(&self.pool)
                .await
                .with_context(|| {
                    format!("migration init failed: {}", &stmt[..stmt.len().min(80)])
                })?;
        }
        sqlx::query(
            "INSERT INTO indexer_state (key, value) VALUES ('schema_version', $1) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(version.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn read_cursor(&mut self) -> Result<Option<Cursor>> {
        // Guard against the table not existing on a fresh DB.
        let exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'indexer_state')",
        )
        .fetch_one(&self.pool)
        .await?;
        if !exists.0 {
            return Ok(None);
        }
        let cursor_v: Option<(String,)> =
            sqlx::query_as("SELECT value FROM indexer_state WHERE key = 'migration_cursor'")
                .fetch_optional(&self.pool)
                .await?;
        let source_v: Option<(String,)> =
            sqlx::query_as("SELECT value FROM indexer_state WHERE key = 'migration_source'")
                .fetch_optional(&self.pool)
                .await?;
        let fp_v: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM indexer_state WHERE key = 'migration_source_fingerprint'",
        )
        .fetch_optional(&self.pool)
        .await?;

        match (cursor_v, source_v, fp_v) {
            (None, None, None) => Ok(None),
            (Some((c,)), Some((s,)), Some((f,))) => {
                let last_height: u32 = c
                    .parse()
                    .with_context(|| format!("migration_cursor not a u32: {c}"))?;
                let fp_bytes = hex::decode(&f)
                    .with_context(|| "migration_source_fingerprint is not hex")?;
                let fp: [u8; 32] = fp_bytes.try_into().map_err(|v: Vec<u8>| {
                    anyhow::anyhow!(
                        "migration_source_fingerprint must be 32 bytes, got {}",
                        v.len()
                    )
                })?;
                Ok(Some(Cursor {
                    last_height,
                    source_url: s,
                    source_fingerprint: fp,
                }))
            }
            _ => Err(anyhow::anyhow!(
                "indexer_state has a partial migration cursor — the 3 keys must all be present or all absent"
            )),
        }
    }

    async fn write_cursor(&mut self, cursor: &Cursor) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        write_cursor_pg(&mut tx, cursor).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn max_height(&mut self) -> Result<Option<u32>> {
        let row: (Option<i64>,) = sqlx::query_as("SELECT MAX(height) FROM blocks")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0.map(|h| h as u32))
    }

    async fn read_block_data(&mut self, height: u32) -> Result<MBlockData> {
        let h = height as i64;

        // blocks
        let block_tup: BlockTuple = sqlx::query_as(
            "SELECT height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count \
             FROM blocks WHERE height = $1",
        )
        .bind(h)
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("blocks row missing at height {height}"))?;
        let block = MBlockRow {
            height: block_tup.0 as u32,
            header_id: pg_vec_to_id32(block_tup.1, "blocks.header_id")?,
            timestamp: block_tup.2,
            difficulty: block_tup.3,
            miner_pk: block_tup.4,
            block_size: block_tup.5 as u32,
            tx_count: block_tup.6 as u32,
        };

        // transactions
        let tx_rows: Vec<TxTuple> = sqlx::query_as(
            "SELECT tx_id, header_id, height, tx_index, size \
             FROM transactions WHERE height = $1 ORDER BY tx_index",
        )
        .bind(h)
        .fetch_all(&self.pool)
        .await?;
        let transactions: Vec<MTransactionRow> = tx_rows
            .into_iter()
            .map(|(tid, hid, h, ti, sz)| {
                Ok::<_, anyhow::Error>(MTransactionRow {
                    tx_id: pg_vec_to_id32(tid, "transactions.tx_id")?,
                    header_id: pg_vec_to_id32(hid, "transactions.header_id")?,
                    height: h as u32,
                    tx_index: ti as u32,
                    size: sz as u32,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // boxes WHERE height = H
        let box_rows: Vec<BoxTuple> = sqlx::query_as(
            "SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, \
                    ergo_tree_hash, address, value, spent_tx_id, spent_height \
             FROM boxes WHERE height = $1 ORDER BY box_id",
        )
        .bind(h)
        .fetch_all(&self.pool)
        .await?;
        let created_boxes: Vec<MBoxRow> = box_rows
            .into_iter()
            .map(|(bid, tid, hid, h, oi, et, eth, addr, val, stid, sh)| {
                Ok::<_, anyhow::Error>(MBoxRow {
                    box_id: pg_vec_to_id32(bid, "boxes.box_id")?,
                    tx_id: pg_vec_to_id32(tid, "boxes.tx_id")?,
                    header_id: pg_vec_to_id32(hid, "boxes.header_id")?,
                    height: h as u32,
                    output_index: oi as u32,
                    ergo_tree: et,
                    ergo_tree_hash: pg_vec_to_id32(eth, "boxes.ergo_tree_hash")?,
                    address: addr,
                    value: val,
                    spent_tx_id: stid
                        .map(|v| pg_vec_to_id32(v, "boxes.spent_tx_id"))
                        .transpose()?,
                    spent_height: sh.map(|s| s as u32),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // box_registers / box_tokens for the just-created boxes
        let mut box_registers: Vec<MBoxRegisterRow> = Vec::new();
        let mut box_tokens: Vec<MBoxTokenRow> = Vec::new();
        for bx in &created_boxes {
            let reg_rows: Vec<(Vec<u8>, i16, Vec<u8>)> = sqlx::query_as(
                "SELECT box_id, register_id, serialized FROM box_registers \
                 WHERE box_id = $1 ORDER BY register_id",
            )
            .bind(bx.box_id.as_slice())
            .fetch_all(&self.pool)
            .await?;
            for (bid, rid, ser) in reg_rows {
                box_registers.push(MBoxRegisterRow {
                    box_id: pg_vec_to_id32(bid, "box_registers.box_id")?,
                    register_id: rid as u8,
                    serialized: ser,
                });
            }

            let tok_rows: Vec<(Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
                "SELECT box_id, token_id, amount FROM box_tokens \
                 WHERE box_id = $1 ORDER BY token_id",
            )
            .bind(bx.box_id.as_slice())
            .fetch_all(&self.pool)
            .await?;
            for (bid, tid, amt) in tok_rows {
                box_tokens.push(MBoxTokenRow {
                    box_id: pg_vec_to_id32(bid, "box_tokens.box_id")?,
                    token_id: pg_vec_to_id32(tid, "box_tokens.token_id")?,
                    amount: amt,
                });
            }
        }

        // tokens WHERE minting_height = H
        let token_rows: Vec<TokenTuple> = sqlx::query_as(
            "SELECT token_id, minting_tx_id, minting_height, name, description, decimals \
             FROM tokens WHERE minting_height = $1 ORDER BY token_id",
        )
        .bind(h)
        .fetch_all(&self.pool)
        .await?;
        let minted_tokens: Vec<MTokenRow> = token_rows
            .into_iter()
            .map(|(tid, mtid, mh, n, d, dec)| {
                Ok::<_, anyhow::Error>(MTokenRow {
                    token_id: pg_vec_to_id32(tid, "tokens.token_id")?,
                    minting_tx_id: pg_vec_to_id32(mtid, "tokens.minting_tx_id")?,
                    minting_height: mh as u32,
                    name: n,
                    description: d,
                    decimals: dec,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // boxes WHERE spent_height = H → spent updates
        let spent_rows: Vec<(Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
            "SELECT box_id, spent_tx_id, spent_height FROM boxes \
             WHERE spent_height = $1 ORDER BY box_id",
        )
        .bind(h)
        .fetch_all(&self.pool)
        .await?;
        let spent_box_updates: Vec<BoxSpentUpdate> = spent_rows
            .into_iter()
            .map(|(bid, stid, sh)| {
                Ok::<_, anyhow::Error>(BoxSpentUpdate {
                    box_id: pg_vec_to_id32(bid, "boxes.box_id")?,
                    spent_tx_id: pg_vec_to_id32(stid, "boxes.spent_tx_id")?,
                    spent_height: sh as u32,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(MBlockData {
            block,
            transactions,
            created_boxes,
            box_registers,
            box_tokens,
            minted_tokens,
            spent_box_updates,
        })
    }

    async fn apply_block(&mut self, data: &MBlockData, cursor: &Cursor) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // Per facts/indexer-migration.md § Per-block migration unit: defer FK
        // checks to commit time. No-op if FKs aren't declared deferrable —
        // CREATE TABLE here uses the default (NOT DEFERRABLE) so the only
        // protection at present is the ordering below, but the spec requires
        // the statement.
        sqlx::query("SET CONSTRAINTS ALL DEFERRED")
            .execute(&mut *tx)
            .await?;

        // 1. blocks
        sqlx::query(
            "INSERT INTO blocks (height, header_id, timestamp, difficulty, miner_pk, \
                                 block_size, tx_count) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(data.block.height as i64)
        .bind(data.block.header_id.as_slice())
        .bind(data.block.timestamp)
        .bind(data.block.difficulty)
        .bind(data.block.miner_pk.as_slice())
        .bind(data.block.block_size as i32)
        .bind(data.block.tx_count as i32)
        .execute(&mut *tx)
        .await?;

        // 2. transactions
        for t in &data.transactions {
            sqlx::query(
                "INSERT INTO transactions (tx_id, header_id, height, tx_index, size) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(t.tx_id.as_slice())
            .bind(t.header_id.as_slice())
            .bind(t.height as i64)
            .bind(t.tx_index as i32)
            .bind(t.size as i32)
            .execute(&mut *tx)
            .await?;
        }

        // 3. boxes (created)
        for b in &data.created_boxes {
            sqlx::query(
                "INSERT INTO boxes (box_id, tx_id, header_id, height, output_index, \
                                    ergo_tree, ergo_tree_hash, address, value, \
                                    spent_tx_id, spent_height) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(b.box_id.as_slice())
            .bind(b.tx_id.as_slice())
            .bind(b.header_id.as_slice())
            .bind(b.height as i64)
            .bind(b.output_index as i32)
            .bind(b.ergo_tree.as_slice())
            .bind(b.ergo_tree_hash.as_slice())
            .bind(&b.address)
            .bind(b.value)
            .bind(b.spent_tx_id.as_ref().map(|s| s.as_slice()))
            .bind(b.spent_height.map(|h| h as i64))
            .execute(&mut *tx)
            .await?;
        }

        // 4. box_registers
        for r in &data.box_registers {
            sqlx::query(
                "INSERT INTO box_registers (box_id, register_id, serialized) \
                 VALUES ($1, $2, $3)",
            )
            .bind(r.box_id.as_slice())
            .bind(r.register_id as i16)
            .bind(r.serialized.as_slice())
            .execute(&mut *tx)
            .await?;
        }

        // 5. box_tokens
        for tk in &data.box_tokens {
            sqlx::query("INSERT INTO box_tokens (box_id, token_id, amount) VALUES ($1, $2, $3)")
                .bind(tk.box_id.as_slice())
                .bind(tk.token_id.as_slice())
                .bind(tk.amount)
                .execute(&mut *tx)
                .await?;
        }

        // 6. tokens (minted at H)
        for t in &data.minted_tokens {
            sqlx::query(
                "INSERT INTO tokens (token_id, minting_tx_id, minting_height, name, description, decimals) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(t.token_id.as_slice())
            .bind(t.minting_tx_id.as_slice())
            .bind(t.minting_height as i64)
            .bind(&t.name)
            .bind(&t.description)
            .bind(t.decimals)
            .execute(&mut *tx)
            .await?;
        }

        // 7. boxes spent updates
        for s in &data.spent_box_updates {
            sqlx::query("UPDATE boxes SET spent_tx_id = $1, spent_height = $2 WHERE box_id = $3")
                .bind(s.spent_tx_id.as_slice())
                .bind(s.spent_height as i64)
                .bind(s.box_id.as_slice())
                .execute(&mut *tx)
                .await?;
        }

        // 8. cursor (atomic with the block data)
        write_cursor_pg(&mut tx, cursor).await?;

        tx.commit().await?;
        Ok(())
    }
}

/// Inline 3-key cursor write inside a caller-controlled PG transaction.
/// Shared by `write_cursor` and `apply_block`.
#[allow(dead_code)] // called only via PostgresBackend impl; daemon bin doesn't construct that
async fn write_cursor_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cursor: &Cursor,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO indexer_state (key, value) VALUES ('migration_cursor', $1) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(cursor.last_height.to_string())
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO indexer_state (key, value) VALUES ('migration_source', $1) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&cursor.source_url)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO indexer_state (key, value) VALUES ('migration_source_fingerprint', $1) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(hex::encode(cursor.source_fingerprint))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod migration_backend_tests {
    use super::*;
    use crate::migrate::hash::hash_block;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Test-helper: a temporary, per-test PG database. Drops on `Drop`.
    ///
    /// The libpq env (~/.pgpass) supplies password for the `mwaddip` role on
    /// the local 5432 instance, per the laptop's standing setup. Each test
    /// CREATEs a randomly-named DB, runs against it, then DROPs.
    ///
    /// `Drop` cleanup is `tokio::spawn`'d into the current runtime — if the
    /// test panics, the runtime still drives the cleanup task before the
    /// process exits. Worst case (process kill before cleanup runs) leaves
    /// a `test_migrate_<rand>_<pid>` DB behind; manual `DROP DATABASE` is
    /// the recovery.
    struct PgTestDb {
        name: String,
        url: String,
    }

    impl PgTestDb {
        /// CREATE DATABASE <unique>. The admin connection to `postgres` is
        /// opened, used, and closed inside this call.
        async fn create() -> Self {
            let admin_url = Self::admin_url();
            let name = Self::unique_name();

            let admin_pool = PgPool::connect(&admin_url)
                .await
                .expect("admin connect to `postgres` DB");
            sqlx::query(&format!("CREATE DATABASE {name}"))
                .execute(&admin_pool)
                .await
                .expect("CREATE DATABASE");
            admin_pool.close().await;

            let url = format!("postgres://mwaddip@127.0.0.1:5432/{name}");
            Self { name, url }
        }

        fn admin_url() -> String {
            "postgres://mwaddip@127.0.0.1:5432/postgres".to_string()
        }

        /// `test_migrate_<unix_nanos>_<pid>_<counter>` — unique per CREATE call
        /// within one process AND across parallel runs (nanos + pid). Avoids
        /// adding a uuid dep just for tests.
        fn unique_name() -> String {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let pid = std::process::id();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            format!("test_migrate_{nanos}_{pid}_{n}")
        }
    }

    impl Drop for PgTestDb {
        fn drop(&mut self) {
            // Cleanup MUST be synchronous-blocking so that tests don't leak
            // databases when the parent tokio runtime exits before a spawned
            // cleanup task can run. Run in a fresh thread + fresh runtime —
            // independent of whatever runtime called Drop. Joining the thread
            // makes Drop block until DROP DATABASE returns.
            let name = self.name.clone();
            let admin_url = Self::admin_url();
            let handle = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("PgTestDb cleanup runtime");
                rt.block_on(async {
                    let admin = match PgPool::connect(&admin_url).await {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("PgTestDb cleanup admin-connect failed for {name}: {e}");
                            return;
                        }
                    };
                    // pg_terminate_backend kicks any leftover sessions off
                    // the test DB — sqlx pool Drop ought to have closed them
                    // already, but DROP DATABASE refuses with a non-empty
                    // backend list, so belt-and-suspenders.
                    let _ = sqlx::query(&format!(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                         WHERE datname = '{name}' AND pid <> pg_backend_pid()"
                    ))
                    .execute(&admin)
                    .await;
                    if let Err(e) = sqlx::query(&format!("DROP DATABASE IF EXISTS {name}"))
                        .execute(&admin)
                        .await
                    {
                        eprintln!("PgTestDb cleanup DROP DATABASE {name} failed: {e}");
                    }
                    admin.close().await;
                });
            });
            if let Err(e) = handle.join() {
                eprintln!("PgTestDb cleanup thread panicked: {e:?}");
            }
        }
    }

    /// Same fixture shape as the SQLite-side `build_synth_block_data` (kept
    /// independent to avoid cross-module test dependencies — both use the
    /// same seed → same hash invariant since the encoding is backend-agnostic).
    fn build_synth_block_data(height: u32, seed: u8) -> MBlockData {
        let header_id = [seed; 32];
        let block = MBlockRow {
            height,
            header_id,
            timestamp: 1_700_000_000_000i64 + seed as i64,
            difficulty: 1_000_000i64 + seed as i64,
            miner_pk: (0u8..33).map(|i| seed.wrapping_add(i)).collect(),
            block_size: 1000u32 + seed as u32,
            tx_count: 2,
        };
        let mut transactions = Vec::with_capacity(2);
        for i in 0u32..2 {
            transactions.push(MTransactionRow {
                tx_id: [seed.wrapping_add(i as u8); 32],
                header_id,
                height,
                tx_index: i,
                size: 200u32 + i + seed as u32,
            });
        }
        let mut created_boxes = Vec::with_capacity(2);
        for i in 0u32..2 {
            created_boxes.push(MBoxRow {
                box_id: [seed.wrapping_mul(2).wrapping_add(i as u8); 32],
                tx_id: transactions[i as usize].tx_id,
                header_id,
                height,
                output_index: i,
                ergo_tree: vec![0x10u8, 0x01, 0x04, seed],
                ergo_tree_hash: [seed.wrapping_add(99); 32],
                address: format!("9addr_{seed}_{i}"),
                value: 1_000_000i64 + i as i64 + seed as i64,
                spent_tx_id: None,
                spent_height: None,
            });
        }
        let box_registers = created_boxes
            .iter()
            .map(|b| MBoxRegisterRow {
                box_id: b.box_id,
                register_id: 4,
                serialized: vec![0x05u8, seed, 0x42],
            })
            .collect();
        let box_tokens = created_boxes
            .iter()
            .map(|b| MBoxTokenRow {
                box_id: b.box_id,
                token_id: [seed.wrapping_add(50); 32],
                amount: 100i64 + seed as i64,
            })
            .collect();
        let minted_tokens = vec![MTokenRow {
            token_id: [seed.wrapping_add(77); 32],
            minting_tx_id: transactions[0].tx_id,
            minting_height: height,
            name: Some(format!("token_{seed}")),
            description: None,
            decimals: Some(seed as i32),
        }];
        let spent_box_updates = vec![BoxSpentUpdate {
            box_id: [seed.wrapping_add(200); 32],
            spent_tx_id: transactions[1].tx_id,
            spent_height: height,
        }];
        MBlockData {
            block,
            transactions,
            created_boxes,
            box_registers,
            box_tokens,
            minted_tokens,
            spent_box_updates,
        }
    }

    /// See sqlite::migration_backend_tests::seed_prior_height_box — same
    /// rationale: the spent_box_updates UPDATE needs a row to hit.
    async fn seed_prior_height_box(
        backend: &mut PostgresBackend,
        box_id: [u8; 32],
        prior_height: u32,
        cursor: &Cursor,
    ) {
        let tx_id = [0xEEu8; 32];
        let header_id = [0xDDu8; 32];
        let data = MBlockData {
            block: MBlockRow {
                height: prior_height,
                header_id,
                timestamp: 1_699_000_000_000,
                difficulty: 999_999,
                miner_pk: vec![0u8; 33],
                block_size: 100,
                tx_count: 1,
            },
            transactions: vec![MTransactionRow {
                tx_id,
                header_id,
                height: prior_height,
                tx_index: 0,
                size: 100,
            }],
            created_boxes: vec![MBoxRow {
                box_id,
                tx_id,
                header_id,
                height: prior_height,
                output_index: 0,
                ergo_tree: vec![0u8; 4],
                ergo_tree_hash: [0u8; 32],
                address: "9addr_prior".to_string(),
                value: 1,
                spent_tx_id: None,
                spent_height: None,
            }],
            box_registers: vec![],
            box_tokens: vec![],
            minted_tokens: vec![],
            spent_box_updates: vec![],
        };
        backend.apply_block(&data, cursor).await.unwrap();
    }

    #[tokio::test]
    async fn postgres_backend_schema_version_roundtrip() {
        let db = PgTestDb::create().await;
        let mut b = PostgresBackend::new_for_migration(&db.url).await.unwrap();
        assert!(b.schema_version().await.unwrap().is_none());
        b.init_schema(1).await.unwrap();
        assert_eq!(b.schema_version().await.unwrap(), Some(1));
        b.pool.close().await;
    }

    #[tokio::test]
    async fn postgres_backend_cursor_roundtrip() {
        let db = PgTestDb::create().await;
        let mut b = PostgresBackend::new_for_migration(&db.url).await.unwrap();
        b.init_schema(1).await.unwrap();
        assert!(b.read_cursor().await.unwrap().is_none());
        let c = Cursor {
            last_height: 42,
            source_url: "sqlite:///source".into(),
            source_fingerprint: [0xAB; 32],
        };
        b.write_cursor(&c).await.unwrap();
        assert_eq!(b.read_cursor().await.unwrap(), Some(c));
        b.pool.close().await;
    }

    #[tokio::test]
    async fn postgres_backend_apply_block_then_read_back() {
        let db = PgTestDb::create().await;
        let mut b = PostgresBackend::new_for_migration(&db.url).await.unwrap();
        b.init_schema(1).await.unwrap();

        let data = build_synth_block_data(100, 0x33);
        let prior_cursor = Cursor {
            last_height: 50,
            source_url: "sqlite:///fake".into(),
            source_fingerprint: [0u8; 32],
        };
        seed_prior_height_box(&mut b, data.spent_box_updates[0].box_id, 50, &prior_cursor).await;

        let cursor = Cursor {
            last_height: 100,
            source_url: "sqlite:///fake".into(),
            source_fingerprint: [0u8; 32],
        };
        b.apply_block(&data, &cursor).await.unwrap();
        let read_back = b.read_block_data(100).await.unwrap();

        assert_eq!(read_back.block, data.block);
        assert_eq!(read_back.transactions, data.transactions);
        assert_eq!(read_back.created_boxes, data.created_boxes);
        assert_eq!(read_back.box_registers, data.box_registers);
        assert_eq!(read_back.box_tokens, data.box_tokens);
        assert_eq!(read_back.minted_tokens, data.minted_tokens);
        assert_eq!(read_back.spent_box_updates, data.spent_box_updates);

        assert_eq!(hash_block(&read_back), hash_block(&data));
        assert_eq!(b.read_cursor().await.unwrap(), Some(cursor));
        assert_eq!(b.max_height().await.unwrap(), Some(100));

        b.pool.close().await;
    }
}

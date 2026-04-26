use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::PgPool;

use crate::db::IndexerDb;
use crate::types::*;

type BlockTuple = (i64, Vec<u8>, i64, i64, Vec<u8>, i32, i32);
type TxTuple = (Vec<u8>, Vec<u8>, i64, i32, i32);
type BoxTuple = (Vec<u8>, Vec<u8>, Vec<u8>, i64, i32, Vec<u8>, Vec<u8>, String, i64, Option<Vec<u8>>, Option<i64>);
type TokenTuple = (Vec<u8>, Vec<u8>, i64, Option<String>, Option<String>, Option<i32>);

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
        let pool = PgPool::connect(url)
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
                    .with_context(|| format!("migration failed: {}", &stmt[..stmt.len().min(80)]))?;
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
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM indexer_state WHERE key = 'height'",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(s,)| s.parse().ok()))
    }

    async fn get_block_id_at(&self, height: u64) -> Result<Option<Vec<u8>>> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT header_id FROM blocks WHERE height = $1",
        )
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

        sqlx::query("UPDATE boxes SET spent_tx_id = NULL, spent_height = NULL WHERE spent_height > $1")
            .bind(h).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM box_registers WHERE box_id IN (SELECT box_id FROM boxes WHERE height > $1)")
            .bind(h).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM box_tokens WHERE box_id IN (SELECT box_id FROM boxes WHERE height > $1)")
            .bind(h).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM boxes WHERE height > $1").bind(h).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM tokens WHERE minting_height > $1").bind(h).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM transactions WHERE height > $1").bind(h).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM blocks WHERE height > $1").bind(h).execute(&mut *tx).await?;
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
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM blocks").fetch_one(&self.pool).await?;
        let rows: Vec<BlockTuple> = sqlx::query_as(
            "SELECT height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count FROM blocks ORDER BY height DESC LIMIT $1 OFFSET $2",
        ).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let items = rows.into_iter().map(tuple_to_block).collect();
        Ok(Page { items, total: total as u64 })
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
        ).bind(tx_id).fetch_optional(&self.pool).await?;
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

    async fn get_unspent_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<BoxRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boxes WHERE address = $1 AND spent_tx_id IS NULL").bind(addr).fetch_one(&self.pool).await?;
        let rows = self.fetch_boxes_query("SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value, spent_tx_id, spent_height FROM boxes WHERE address = $1 AND spent_tx_id IS NULL ORDER BY height DESC LIMIT $2 OFFSET $3", addr, limit, offset).await?;
        Ok(Page { items: rows, total: total as u64 })
    }

    async fn get_boxes_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<BoxRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boxes WHERE address = $1").bind(addr).fetch_one(&self.pool).await?;
        let rows = self.fetch_boxes_query("SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value, spent_tx_id, spent_height FROM boxes WHERE address = $1 ORDER BY height DESC LIMIT $2 OFFSET $3", addr, limit, offset).await?;
        Ok(Page { items: rows, total: total as u64 })
    }

    async fn get_balance(&self, addr: &str) -> Result<Balance> {
        let (nano_ergs,): (i64,) = sqlx::query_as("SELECT COALESCE(SUM(value), 0) FROM boxes WHERE address = $1 AND spent_tx_id IS NULL").bind(addr).fetch_one(&self.pool).await?;
        let token_rows: Vec<(Vec<u8>, i64, Option<String>, Option<i32>)> = sqlx::query_as(
            "SELECT bt.token_id, SUM(bt.amount) as total, t.name, t.decimals FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id LEFT JOIN tokens t ON t.token_id = bt.token_id WHERE b.address = $1 AND b.spent_tx_id IS NULL GROUP BY bt.token_id, t.name, t.decimals",
        ).bind(addr).fetch_all(&self.pool).await?;
        let tokens = token_rows.into_iter().map(|(tid, amt, name, dec)| TokenBalance {
            token_id: hex::encode(tid), amount: amt as u64, name, decimals: dec,
        }).collect();
        Ok(Balance { nano_ergs: nano_ergs as u64, tokens })
    }

    async fn get_txs_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<TxRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(DISTINCT t.tx_id) FROM transactions t JOIN boxes b ON b.tx_id = t.tx_id WHERE b.address = $1").bind(addr).fetch_one(&self.pool).await?;
        let rows: Vec<TxTuple> = sqlx::query_as(
            "SELECT DISTINCT t.tx_id, t.header_id, t.height, t.tx_index, t.size FROM transactions t JOIN boxes b ON b.tx_id = t.tx_id WHERE b.address = $1 ORDER BY t.height DESC LIMIT $2 OFFSET $3",
        ).bind(addr).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let items = rows.into_iter().map(tuple_to_tx).collect();
        Ok(Page { items, total: total as u64 })
    }

    async fn get_unspent_by_ergo_tree(&self, hash: &[u8], offset: u64, limit: u64) -> Result<Page<BoxRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boxes WHERE ergo_tree_hash = $1 AND spent_tx_id IS NULL").bind(hash).fetch_one(&self.pool).await?;
        let rows: Vec<BoxTuple> = sqlx::query_as(
            "SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value, spent_tx_id, spent_height FROM boxes WHERE ergo_tree_hash = $1 AND spent_tx_id IS NULL ORDER BY height DESC LIMIT $2 OFFSET $3",
        ).bind(hash).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let mut items: Vec<BoxRow> = rows.into_iter().map(tuple_to_box_bare).collect();
        for bx in &mut items {
            let bid = hex::decode(&bx.box_id)?;
            bx.tokens = self.fetch_box_tokens(&bid).await?;
            bx.registers = self.fetch_box_registers(&bid).await?;
        }
        Ok(Page { items, total: total as u64 })
    }

    async fn get_token(&self, token_id: &[u8]) -> Result<Option<TokenRow>> {
        let row: Option<TokenTuple> = sqlx::query_as(
            "SELECT token_id, minting_tx_id, minting_height, name, description, decimals FROM tokens WHERE token_id = $1",
        ).bind(token_id).fetch_optional(&self.pool).await?;
        Ok(row.map(tuple_to_token))
    }

    async fn get_token_holders(&self, token_id: &[u8], offset: u64, limit: u64) -> Result<Page<HolderRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(DISTINCT b.address) FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id WHERE bt.token_id = $1 AND b.spent_tx_id IS NULL").bind(token_id).fetch_one(&self.pool).await?;
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT b.address, SUM(bt.amount) as total FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id WHERE bt.token_id = $1 AND b.spent_tx_id IS NULL GROUP BY b.address ORDER BY total DESC LIMIT $2 OFFSET $3",
        ).bind(token_id).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let items = rows.into_iter().map(|(addr, amt)| HolderRow { address: addr, amount: amt as u64 }).collect();
        Ok(Page { items, total: total as u64 })
    }

    async fn get_token_boxes(&self, token_id: &[u8], offset: u64, limit: u64) -> Result<Page<BoxRow>> {
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
        Ok(Page { items, total: total as u64 })
    }

    async fn get_tokens(&self, offset: u64, limit: u64) -> Result<Page<TokenRow>> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tokens").fetch_one(&self.pool).await?;
        let rows: Vec<TokenTuple> = sqlx::query_as(
            "SELECT token_id, minting_tx_id, minting_height, name, description, decimals FROM tokens ORDER BY minting_height DESC LIMIT $1 OFFSET $2",
        ).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let items = rows.into_iter().map(tuple_to_token).collect();
        Ok(Page { items, total: total as u64 })
    }

    async fn get_stats(&self) -> Result<NetworkStats> {
        let (ih,): (i64,) = sqlx::query_as("SELECT COALESCE(MAX(height), 0) FROM blocks").fetch_one(&self.pool).await?;
        let (tb,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM blocks").fetch_one(&self.pool).await?;
        let (tt,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions").fetch_one(&self.pool).await?;
        let (tbx,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boxes").fetch_one(&self.pool).await?;
        let (ttk,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tokens").fetch_one(&self.pool).await?;
        Ok(NetworkStats { indexed_height: ih as u64, total_blocks: tb as u64, total_transactions: tt as u64, total_boxes: tbx as u64, total_tokens: ttk as u64 })
    }

    async fn get_daily_stats(&self, days: u32) -> Result<Vec<DailyStats>> {
        let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
            "SELECT to_char(to_timestamp(b.timestamp / 1000), 'YYYY-MM-DD') as day, COUNT(DISTINCT t.tx_id), COUNT(DISTINCT b.height), COALESCE(SUM(bx.value), 0) FROM blocks b LEFT JOIN transactions t ON t.header_id = b.header_id LEFT JOIN boxes bx ON bx.tx_id = t.tx_id WHERE b.timestamp >= (EXTRACT(EPOCH FROM NOW()) - $1 * 86400) * 1000 GROUP BY day ORDER BY day DESC",
        ).bind(days as i64).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(date, tc, bc, vol)| DailyStats {
            date, tx_count: tc as u64, block_count: bc as u64, volume: vol as u64,
        }).collect())
    }
}

// Helper methods for PgDb
impl PgDb {
    async fn fetch_box_tokens(&self, box_id: &[u8]) -> Result<Vec<BoxTokenRow>> {
        let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as(
            "SELECT token_id, amount FROM box_tokens WHERE box_id = $1",
        ).bind(box_id).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(tid, amt)| BoxTokenRow {
            token_id: hex::encode(tid), amount: amt as u64,
        }).collect())
    }

    async fn fetch_box_registers(&self, box_id: &[u8]) -> Result<Vec<RegisterRow>> {
        let rows: Vec<(i16, Vec<u8>)> = sqlx::query_as(
            "SELECT register_id, serialized FROM box_registers WHERE box_id = $1",
        ).bind(box_id).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(rid, ser)| RegisterRow {
            register_id: rid as u8, serialized: hex::encode(ser),
        }).collect())
    }

    async fn fetch_boxes_query(&self, sql: &str, addr: &str, limit: u64, offset: u64) -> Result<Vec<BoxRow>> {
        let rows: Vec<BoxTuple> = sqlx::query_as(sql)
            .bind(addr).bind(limit as i64).bind(offset as i64)
            .fetch_all(&self.pool).await?;
        let mut items: Vec<BoxRow> = rows.into_iter().map(tuple_to_box_bare).collect();
        for bx in &mut items {
            let bid = hex::decode(&bx.box_id)?;
            bx.tokens = self.fetch_box_tokens(&bid).await?;
            bx.registers = self.fetch_box_registers(&bid).await?;
        }
        Ok(items)
    }
}

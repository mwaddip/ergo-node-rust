use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::IndexerDb;
use crate::types::*;

const SCHEMA_VERSION: u32 = 1;

const CREATE_TABLES: &str = "
CREATE TABLE IF NOT EXISTS indexer_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS blocks (
    height     INTEGER PRIMARY KEY,
    header_id  BLOB NOT NULL UNIQUE,
    timestamp  INTEGER NOT NULL,
    difficulty INTEGER NOT NULL,
    miner_pk   BLOB NOT NULL,
    block_size INTEGER NOT NULL,
    tx_count   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS transactions (
    tx_id     BLOB PRIMARY KEY,
    header_id BLOB NOT NULL REFERENCES blocks(header_id),
    height    INTEGER NOT NULL,
    tx_index  INTEGER NOT NULL,
    size      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS boxes (
    box_id         BLOB PRIMARY KEY,
    tx_id          BLOB NOT NULL REFERENCES transactions(tx_id),
    header_id      BLOB NOT NULL,
    height         INTEGER NOT NULL,
    output_index   INTEGER NOT NULL,
    ergo_tree      BLOB NOT NULL,
    ergo_tree_hash BLOB NOT NULL,
    address        TEXT NOT NULL,
    value          INTEGER NOT NULL,
    spent_tx_id    BLOB,
    spent_height   INTEGER
);

CREATE TABLE IF NOT EXISTS box_tokens (
    box_id   BLOB NOT NULL REFERENCES boxes(box_id),
    token_id BLOB NOT NULL,
    amount   INTEGER NOT NULL,
    PRIMARY KEY (box_id, token_id)
);

CREATE TABLE IF NOT EXISTS box_registers (
    box_id      BLOB NOT NULL REFERENCES boxes(box_id),
    register_id INTEGER NOT NULL,
    serialized  BLOB NOT NULL,
    PRIMARY KEY (box_id, register_id)
);

CREATE TABLE IF NOT EXISTS tokens (
    token_id       BLOB PRIMARY KEY,
    minting_tx_id  BLOB NOT NULL,
    minting_height INTEGER NOT NULL,
    name           TEXT,
    description    TEXT,
    decimals       INTEGER
);

CREATE INDEX IF NOT EXISTS idx_boxes_address_unspent ON boxes(address) WHERE spent_tx_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_boxes_ergo_tree_hash_unspent ON boxes(ergo_tree_hash) WHERE spent_tx_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_boxes_address ON boxes(address);
CREATE INDEX IF NOT EXISTS idx_box_tokens_token_id ON box_tokens(token_id);
CREATE INDEX IF NOT EXISTS idx_transactions_height ON transactions(height);
CREATE INDEX IF NOT EXISTS idx_boxes_height ON boxes(height);
";

pub struct SqliteDb {
    writer: Arc<Mutex<Connection>>,
    reader: Arc<Mutex<Connection>>,
}

impl SqliteDb {
    pub fn open(path: &str) -> Result<Self> {
        let writer = Connection::open(path)
            .with_context(|| format!("failed to open SQLite at {path}"))?;
        writer.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )?;
        writer.execute_batch(CREATE_TABLES)?;

        writer.execute(
            "INSERT OR IGNORE INTO indexer_state (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;

        let reader = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open read-only SQLite at {path}"))?;
        reader.execute_batch("PRAGMA query_only=ON;")?;

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            reader: Arc::new(Mutex::new(reader)),
        })
    }

    /// Fetch tokens and registers for a list of box rows (enriches the BoxRow).
    fn enrich_boxes(conn: &Connection, boxes: &mut [BoxRow]) -> Result<()> {
        for bx in boxes.iter_mut() {
            let box_id_bytes = hex::decode(&bx.box_id)?;
            // Tokens
            let mut stmt = conn.prepare_cached(
                "SELECT token_id, amount FROM box_tokens WHERE box_id = ?1",
            )?;
            bx.tokens = stmt
                .query_map(params![box_id_bytes.as_slice()], |row| {
                    let tid: Vec<u8> = row.get(0)?;
                    let amt: i64 = row.get(1)?;
                    Ok(BoxTokenRow {
                        token_id: hex::encode(tid),
                        amount: amt as u64,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            // Registers
            let mut stmt = conn.prepare_cached(
                "SELECT register_id, serialized FROM box_registers WHERE box_id = ?1",
            )?;
            bx.registers = stmt
                .query_map(params![box_id_bytes.as_slice()], |row| {
                    let rid: i32 = row.get(0)?;
                    let ser: Vec<u8> = row.get(1)?;
                    Ok(RegisterRow {
                        register_id: rid as u8,
                        serialized: hex::encode(ser),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
        }
        Ok(())
    }

    fn row_to_block(row: &rusqlite::Row<'_>) -> rusqlite::Result<BlockRow> {
        let hid: Vec<u8> = row.get("header_id")?;
        let mpk: Vec<u8> = row.get("miner_pk")?;
        Ok(BlockRow {
            height: row.get::<_, i64>("height")? as u64,
            header_id: hex::encode(hid),
            timestamp: row.get::<_, i64>("timestamp")? as u64,
            difficulty: row.get::<_, i64>("difficulty")? as u64,
            miner_pk: hex::encode(mpk),
            block_size: row.get::<_, i32>("block_size")? as u32,
            tx_count: row.get::<_, i32>("tx_count")? as u32,
        })
    }

    fn row_to_tx(row: &rusqlite::Row<'_>) -> rusqlite::Result<TxRow> {
        let tid: Vec<u8> = row.get("tx_id")?;
        let hid: Vec<u8> = row.get("header_id")?;
        Ok(TxRow {
            tx_id: hex::encode(tid),
            header_id: hex::encode(hid),
            height: row.get::<_, i64>("height")? as u64,
            tx_index: row.get::<_, i32>("tx_index")? as u32,
            size: row.get::<_, i32>("size")? as u32,
        })
    }

    fn row_to_box(row: &rusqlite::Row<'_>) -> rusqlite::Result<BoxRow> {
        let bid: Vec<u8> = row.get("box_id")?;
        let tid: Vec<u8> = row.get("tx_id")?;
        let hid: Vec<u8> = row.get("header_id")?;
        let et: Vec<u8> = row.get("ergo_tree")?;
        let eth: Vec<u8> = row.get("ergo_tree_hash")?;
        let stid: Option<Vec<u8>> = row.get("spent_tx_id")?;
        Ok(BoxRow {
            box_id: hex::encode(bid),
            tx_id: hex::encode(tid),
            header_id: hex::encode(hid),
            height: row.get::<_, i64>("height")? as u64,
            output_index: row.get::<_, i32>("output_index")? as u32,
            ergo_tree: hex::encode(et),
            ergo_tree_hash: hex::encode(eth),
            address: row.get("address")?,
            value: row.get::<_, i64>("value")? as u64,
            spent_tx_id: stid.map(hex::encode),
            spent_height: row.get::<_, Option<i64>>("spent_height")?.map(|h| h as u64),
            tokens: vec![],
            registers: vec![],
        })
    }
}

use rusqlite::OptionalExtension;

#[async_trait]
impl IndexerDb for SqliteDb {
    // -----------------------------------------------------------------------
    // Write path
    // -----------------------------------------------------------------------

    async fn get_indexed_height(&self) -> Result<Option<u64>> {
        let conn = self.writer.lock().await;
        let height: Option<String> = conn
            .query_row(
                "SELECT value FROM indexer_state WHERE key = 'height'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(height.and_then(|s| s.parse().ok()))
    }

    async fn get_block_id_at(&self, height: u64) -> Result<Option<Vec<u8>>> {
        let conn = self.writer.lock().await;
        let id: Option<Vec<u8>> = conn
            .query_row(
                "SELECT header_id FROM blocks WHERE height = ?1",
                params![height as i64],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    async fn insert_block(&self, block: &IndexedBlock) -> Result<()> {
        let conn = self.writer.lock().await;
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "INSERT INTO blocks (height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                block.height as i64,
                block.header_id.as_slice(),
                block.timestamp as i64,
                block.difficulty as i64,
                block.miner_pk.as_slice(),
                block.block_size,
                block.transactions.len() as u32,
            ],
        )?;

        for itx in &block.transactions {
            tx.execute(
                "INSERT INTO transactions (tx_id, header_id, height, tx_index, size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    itx.tx_id.as_slice(),
                    block.header_id.as_slice(),
                    block.height as i64,
                    itx.tx_index,
                    itx.size,
                ],
            )?;

            for input in &itx.inputs {
                tx.execute(
                    "UPDATE boxes SET spent_tx_id = ?1, spent_height = ?2 WHERE box_id = ?3",
                    params![
                        itx.tx_id.as_slice(),
                        block.height as i64,
                        input.box_id.as_slice()
                    ],
                )?;
            }

            for out in &itx.outputs {
                tx.execute(
                    "INSERT INTO boxes (box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        out.box_id.as_slice(),
                        itx.tx_id.as_slice(),
                        block.header_id.as_slice(),
                        block.height as i64,
                        out.output_index,
                        out.ergo_tree.as_slice(),
                        out.ergo_tree_hash.as_slice(),
                        out.address,
                        out.value as i64,
                    ],
                )?;

                for tok in &out.tokens {
                    tx.execute(
                        "INSERT INTO box_tokens (box_id, token_id, amount) VALUES (?1, ?2, ?3)",
                        params![
                            out.box_id.as_slice(),
                            tok.token_id.as_slice(),
                            tok.amount as i64
                        ],
                    )?;
                }

                for reg in &out.registers {
                    tx.execute(
                        "INSERT INTO box_registers (box_id, register_id, serialized) VALUES (?1, ?2, ?3)",
                        params![
                            out.box_id.as_slice(),
                            reg.register_id as i32,
                            reg.serialized.as_slice()
                        ],
                    )?;
                }

                if let Some(minted) = &out.minted_token {
                    tx.execute(
                        "INSERT OR IGNORE INTO tokens (token_id, minting_tx_id, minting_height, name, description, decimals)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            minted.token_id.as_slice(),
                            itx.tx_id.as_slice(),
                            block.height as i64,
                            minted.name,
                            minted.description,
                            minted.decimals,
                        ],
                    )?;
                }
            }
        }

        tx.execute(
            "INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('height', ?1)",
            params![(block.height as i64).to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    async fn rollback_to(&self, height: u64) -> Result<()> {
        let conn = self.writer.lock().await;
        let tx = conn.unchecked_transaction()?;
        let h = height as i64;

        tx.execute(
            "UPDATE boxes SET spent_tx_id = NULL, spent_height = NULL WHERE spent_height > ?1",
            params![h],
        )?;
        tx.execute(
            "DELETE FROM box_registers WHERE box_id IN (SELECT box_id FROM boxes WHERE height > ?1)",
            params![h],
        )?;
        tx.execute(
            "DELETE FROM box_tokens WHERE box_id IN (SELECT box_id FROM boxes WHERE height > ?1)",
            params![h],
        )?;
        tx.execute("DELETE FROM boxes WHERE height > ?1", params![h])?;
        tx.execute("DELETE FROM tokens WHERE minting_height > ?1", params![h])?;
        tx.execute(
            "DELETE FROM transactions WHERE height > ?1",
            params![h],
        )?;
        tx.execute("DELETE FROM blocks WHERE height > ?1", params![h])?;

        tx.execute(
            "INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('height', ?1)",
            params![h.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Read path
    // -----------------------------------------------------------------------

    async fn get_block_by_height(&self, height: u64) -> Result<Option<BlockRow>> {
        let r = self.reader.lock().await;
        let row = r
            .query_row(
                "SELECT * FROM blocks WHERE height = ?1",
                params![height as i64],
                Self::row_to_block,
            )
            .optional()?;
        Ok(row)
    }

    async fn get_block_by_id(&self, id: &[u8]) -> Result<Option<BlockRow>> {
        let r = self.reader.lock().await;
        let row = r
            .query_row(
                "SELECT * FROM blocks WHERE header_id = ?1",
                params![id],
                Self::row_to_block,
            )
            .optional()?;
        Ok(row)
    }

    async fn get_blocks(&self, offset: u64, limit: u64) -> Result<Page<BlockRow>> {
        let r = self.reader.lock().await;
        let total: u64 = r.query_row(
            "SELECT COUNT(*) FROM blocks",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT * FROM blocks ORDER BY height DESC LIMIT ?1 OFFSET ?2",
        )?;
        let items = stmt
            .query_map(params![limit as i64, offset as i64], Self::row_to_block)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(Page { items, total })
    }

    async fn get_transactions_for_block(&self, header_id: &[u8]) -> Result<Vec<TxRow>> {
        let r = self.reader.lock().await;
        let mut stmt = r.prepare_cached(
            "SELECT * FROM transactions WHERE header_id = ?1 ORDER BY tx_index",
        )?;
        let items = stmt
            .query_map(params![header_id], Self::row_to_tx)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }

    async fn get_transaction(&self, tx_id: &[u8]) -> Result<Option<TxRow>> {
        let r = self.reader.lock().await;
        let row = r
            .query_row(
                "SELECT * FROM transactions WHERE tx_id = ?1",
                params![tx_id],
                Self::row_to_tx,
            )
            .optional()?;
        Ok(row)
    }

    async fn get_box(&self, box_id: &[u8]) -> Result<Option<BoxRow>> {
        let r = self.reader.lock().await;
        let mut bx = match r
            .query_row(
                "SELECT * FROM boxes WHERE box_id = ?1",
                params![box_id],
                Self::row_to_box,
            )
            .optional()?
        {
            Some(b) => b,
            None => return Ok(None),
        };
        Self::enrich_boxes(&r,std::slice::from_mut(&mut bx))?;
        Ok(Some(bx))
    }

    async fn get_unspent_by_address(
        &self,
        addr: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Page<BoxRow>> {
        let r = self.reader.lock().await;
        let total: u64 = r.query_row(
            "SELECT COUNT(*) FROM boxes WHERE address = ?1 AND spent_tx_id IS NULL",
            params![addr],
            |r| r.get::<_, i64>(0),
        )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT * FROM boxes WHERE address = ?1 AND spent_tx_id IS NULL ORDER BY height DESC LIMIT ?2 OFFSET ?3",
        )?;
        let mut items = stmt
            .query_map(
                params![addr, limit as i64, offset as i64],
                Self::row_to_box,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::enrich_boxes(&r,&mut items)?;
        Ok(Page { items, total })
    }

    async fn get_boxes_by_address(
        &self,
        addr: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Page<BoxRow>> {
        let r = self.reader.lock().await;
        let total: u64 = r.query_row(
            "SELECT COUNT(*) FROM boxes WHERE address = ?1",
            params![addr],
            |r| r.get::<_, i64>(0),
        )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT * FROM boxes WHERE address = ?1 ORDER BY height DESC LIMIT ?2 OFFSET ?3",
        )?;
        let mut items = stmt
            .query_map(
                params![addr, limit as i64, offset as i64],
                Self::row_to_box,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::enrich_boxes(&r,&mut items)?;
        Ok(Page { items, total })
    }

    async fn get_balance(&self, addr: &str) -> Result<Balance> {
        let r = self.reader.lock().await;
        let nano_ergs: u64 = r
            .query_row(
                "SELECT COALESCE(SUM(value), 0) FROM boxes WHERE address = ?1 AND spent_tx_id IS NULL",
                params![addr],
                |r| r.get::<_, i64>(0),
            )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT bt.token_id, SUM(bt.amount) as total, t.name, t.decimals
             FROM box_tokens bt
             JOIN boxes b ON b.box_id = bt.box_id
             LEFT JOIN tokens t ON t.token_id = bt.token_id
             WHERE b.address = ?1 AND b.spent_tx_id IS NULL
             GROUP BY bt.token_id",
        )?;
        let tokens = stmt
            .query_map(params![addr], |row| {
                let tid: Vec<u8> = row.get(0)?;
                let amt: i64 = row.get(1)?;
                let name: Option<String> = row.get(2)?;
                let decimals: Option<i32> = row.get(3)?;
                Ok(TokenBalance {
                    token_id: hex::encode(tid),
                    amount: amt as u64,
                    name,
                    decimals,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(Balance { nano_ergs, tokens })
    }

    async fn get_txs_by_address(
        &self,
        addr: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Page<TxRow>> {
        let r = self.reader.lock().await;
        let total: u64 = r.query_row(
            "SELECT COUNT(DISTINCT t.tx_id) FROM transactions t
             JOIN boxes b ON b.tx_id = t.tx_id
             WHERE b.address = ?1",
            params![addr],
            |r| r.get::<_, i64>(0),
        )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT DISTINCT t.* FROM transactions t
             JOIN boxes b ON b.tx_id = t.tx_id
             WHERE b.address = ?1
             ORDER BY t.height DESC
             LIMIT ?2 OFFSET ?3",
        )?;
        let items = stmt
            .query_map(
                params![addr, limit as i64, offset as i64],
                Self::row_to_tx,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Page { items, total })
    }

    async fn get_unspent_by_ergo_tree(
        &self,
        hash: &[u8],
        offset: u64,
        limit: u64,
    ) -> Result<Page<BoxRow>> {
        let r = self.reader.lock().await;
        let total: u64 = r.query_row(
            "SELECT COUNT(*) FROM boxes WHERE ergo_tree_hash = ?1 AND spent_tx_id IS NULL",
            params![hash],
            |r| r.get::<_, i64>(0),
        )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT * FROM boxes WHERE ergo_tree_hash = ?1 AND spent_tx_id IS NULL ORDER BY height DESC LIMIT ?2 OFFSET ?3",
        )?;
        let mut items = stmt
            .query_map(
                params![hash, limit as i64, offset as i64],
                Self::row_to_box,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::enrich_boxes(&r,&mut items)?;
        Ok(Page { items, total })
    }

    async fn get_token(&self, token_id: &[u8]) -> Result<Option<TokenRow>> {
        let r = self.reader.lock().await;
        let row = r
            .query_row(
                "SELECT * FROM tokens WHERE token_id = ?1",
                params![token_id],
                |row| {
                    let tid: Vec<u8> = row.get("token_id")?;
                    let mtid: Vec<u8> = row.get("minting_tx_id")?;
                    Ok(TokenRow {
                        token_id: hex::encode(tid),
                        minting_tx_id: hex::encode(mtid),
                        minting_height: row.get::<_, i64>("minting_height")? as u64,
                        name: row.get("name")?,
                        description: row.get("description")?,
                        decimals: row.get("decimals")?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    async fn get_token_holders(
        &self,
        token_id: &[u8],
        offset: u64,
        limit: u64,
    ) -> Result<Page<HolderRow>> {
        let r = self.reader.lock().await;
        let total: u64 = r.query_row(
            "SELECT COUNT(DISTINCT b.address)
             FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id
             WHERE bt.token_id = ?1 AND b.spent_tx_id IS NULL",
            params![token_id],
            |r| r.get::<_, i64>(0),
        )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT b.address, SUM(bt.amount) as total
             FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id
             WHERE bt.token_id = ?1 AND b.spent_tx_id IS NULL
             GROUP BY b.address ORDER BY total DESC LIMIT ?2 OFFSET ?3",
        )?;
        let items = stmt
            .query_map(
                params![token_id, limit as i64, offset as i64],
                |row| {
                    Ok(HolderRow {
                        address: row.get(0)?,
                        amount: row.get::<_, i64>(1)? as u64,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Page { items, total })
    }

    async fn get_token_boxes(
        &self,
        token_id: &[u8],
        offset: u64,
        limit: u64,
    ) -> Result<Page<BoxRow>> {
        let r = self.reader.lock().await;
        let total: u64 = r.query_row(
            "SELECT COUNT(*)
             FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id
             WHERE bt.token_id = ?1 AND b.spent_tx_id IS NULL",
            params![token_id],
            |r| r.get::<_, i64>(0),
        )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT b.* FROM box_tokens bt JOIN boxes b ON b.box_id = bt.box_id
             WHERE bt.token_id = ?1 AND b.spent_tx_id IS NULL
             ORDER BY b.height DESC LIMIT ?2 OFFSET ?3",
        )?;
        let mut items = stmt
            .query_map(
                params![token_id, limit as i64, offset as i64],
                Self::row_to_box,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::enrich_boxes(&r,&mut items)?;
        Ok(Page { items, total })
    }

    async fn get_tokens(&self, offset: u64, limit: u64) -> Result<Page<TokenRow>> {
        let r = self.reader.lock().await;
        let total: u64 = r.query_row(
            "SELECT COUNT(*) FROM tokens",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;

        let mut stmt = r.prepare_cached(
            "SELECT * FROM tokens ORDER BY minting_height DESC LIMIT ?1 OFFSET ?2",
        )?;
        let items = stmt
            .query_map(params![limit as i64, offset as i64], |row| {
                let tid: Vec<u8> = row.get("token_id")?;
                let mtid: Vec<u8> = row.get("minting_tx_id")?;
                Ok(TokenRow {
                    token_id: hex::encode(tid),
                    minting_tx_id: hex::encode(mtid),
                    minting_height: row.get::<_, i64>("minting_height")? as u64,
                    name: row.get("name")?,
                    description: row.get("description")?,
                    decimals: row.get("decimals")?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Page { items, total })
    }

    async fn get_stats(&self) -> Result<NetworkStats> {
        let r = self.reader.lock().await;
        let indexed_height: u64 = r
            .query_row(
                "SELECT COALESCE(MAX(height), 0) FROM blocks",
                [],
                |r| r.get::<_, i64>(0),
            )? as u64;
        let total_blocks: u64 = r.query_row(
            "SELECT COUNT(*) FROM blocks",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        let total_transactions: u64 = r.query_row(
            "SELECT COUNT(*) FROM transactions",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        let total_boxes: u64 = r.query_row(
            "SELECT COUNT(*) FROM boxes",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        let total_tokens: u64 = r.query_row(
            "SELECT COUNT(*) FROM tokens",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;

        Ok(NetworkStats {
            indexed_height,
            total_blocks,
            total_transactions,
            total_boxes,
            total_tokens,
        })
    }

    async fn get_daily_stats(&self, days: u32) -> Result<Vec<DailyStats>> {
        let r = self.reader.lock().await;
        let mut stmt = r.prepare_cached(
            "SELECT date(timestamp / 1000, 'unixepoch') as day,
                    COUNT(DISTINCT t.tx_id) as tx_count,
                    COUNT(DISTINCT b2.height) as block_count,
                    COALESCE(SUM(bx.value), 0) as volume
             FROM blocks b2
             LEFT JOIN transactions t ON t.header_id = b2.header_id
             LEFT JOIN boxes bx ON bx.tx_id = t.tx_id
             WHERE b2.timestamp >= (strftime('%s', 'now') - ?1 * 86400) * 1000
             GROUP BY day ORDER BY day DESC",
        )?;
        let items = stmt
            .query_map(params![days], |row| {
                Ok(DailyStats {
                    date: row.get(0)?,
                    tx_count: row.get::<_, i64>(1)? as u64,
                    block_count: row.get::<_, i64>(2)? as u64,
                    volume: row.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }
}

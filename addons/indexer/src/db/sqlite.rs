use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::{DbMemoryStats, IndexerDb};
use crate::types::*;

/// Pragma set applied at connection open. Tuned for an indexer workload
/// (read-heavy with periodic write bursts during block ingest) against a
/// multi-GB SQLite file.
///
/// - `journal_mode=WAL` — concurrent reader + writer; checkpoint on
///   commit boundary. Necessary for the writer/reader split below.
/// - `synchronous=NORMAL` — fsync only at WAL checkpoint, not on every
///   commit. With WAL this is durable across application crashes (only
///   power loss can lose the most recent commits). Block-ingest is
///   idempotent re-runnable, so this is the right tradeoff.
/// - `foreign_keys=ON` — schema declares FKs; without this they're
///   silently not enforced.
/// - `cache_size=-65536` — 64 MiB per connection (negative = KiB). Bumped
///   from SQLite's 2 MiB default because the hot-path queries
///   (`get_box_by_id`, `enrich_boxes`) do random-page lookups across a
///   multi-GB DB; the default cache thrashes under the 100-way fan-out
///   the validation harness hits the `/boxes/{id}/bytes` endpoint with.
///   With 2 connections, total in-process cache ceiling is ~128 MiB.
/// - `mmap_size=268435456` — 256 MiB memory-mapped read window. Reduces
///   read() syscalls on the hot path. Note: this counts as virtual
///   address space (`VmSize`), NOT resident memory. Kernel page cache
///   is shareable across processes — the indexer doesn't carry this
///   in its own RSS, but tools that read VmSize will see a larger
///   number. See [[feedback_jemalloc_retained_is_virtual]] for the same
///   gotcha on the allocator side.
/// - `temp_store=MEMORY` — temp tables and sort scratch in RAM instead
///   of /tmp. Helps the `get_balance` / `get_token_holders` joins.
/// - `busy_timeout=5000` — WAL serializes writers via OS file lock; the
///   timeout keeps transient `SQLITE_BUSY` from breaking a write when
///   the checkpoint thread holds the lock briefly. 5 s is conservative;
///   our writer is single-threaded behind a Mutex so contention is
///   only from the SQLite checkpointer itself.
const PRAGMAS_WRITER: &str = "\
    PRAGMA journal_mode=WAL; \
    PRAGMA synchronous=NORMAL; \
    PRAGMA foreign_keys=ON; \
    PRAGMA cache_size=-65536; \
    PRAGMA mmap_size=268435456; \
    PRAGMA temp_store=MEMORY; \
    PRAGMA busy_timeout=5000;";

const PRAGMAS_READER: &str = "\
    PRAGMA query_only=ON; \
    PRAGMA cache_size=-65536; \
    PRAGMA mmap_size=268435456; \
    PRAGMA temp_store=MEMORY; \
    PRAGMA busy_timeout=5000;";

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
        writer.execute_batch(PRAGMAS_WRITER)?;
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
        reader.execute_batch(PRAGMAS_READER)?;

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            reader: Arc::new(Mutex::new(reader)),
        })
    }

    /// Read SQLite's view of its own memory footprint via PRAGMAs. Both
    /// `page_size` and `cache_size` are connection-scoped — values come
    /// from the reader connection (the writer's are identical post-PRAGMAS).
    /// On error we return a stats struct with `None` fields rather than
    /// surface the failure, because this is diagnostic.
    fn read_sqlite_stats(&self) -> rusqlite::Result<(u64, u64, i64)> {
        let r = self.reader.try_lock().map_err(|_| {
            rusqlite::Error::ExecuteReturnedResults
        })?;
        let page_size: u64 = r.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))? as u64;
        let page_count: u64 = r.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))? as u64;
        let cache_size: i64 = r.query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))?;
        Ok((page_size, page_count, cache_size))
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

    async fn memory_stats(&self) -> DbMemoryStats {
        // Best-effort: failures return zero/None. Don't surface
        // SQLITE_BUSY noise on the diagnostic endpoint.
        let (on_disk_bytes, cache_bytes_per_conn) = match self.read_sqlite_stats() {
            Ok((page_size, page_count, cache_size)) => {
                let on_disk = page_size.saturating_mul(page_count);
                // cache_size: negative = absolute KiB, positive = pages.
                let cache_bytes = if cache_size < 0 {
                    (cache_size.unsigned_abs()).saturating_mul(1024)
                } else {
                    (cache_size as u64).saturating_mul(page_size)
                };
                (Some(on_disk), Some(cache_bytes))
            }
            Err(_) => (None, None),
        };
        DbMemoryStats {
            backend: "sqlite",
            on_disk_bytes,
            cache_bytes_per_conn,
            connection_count: 2, // writer + reader, see `open`
        }
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

// ---------------------------------------------------------------------------
// Migration backend
// ---------------------------------------------------------------------------
//
// `SqliteBackend` implements `crate::migrate::Backend` and is used ONLY by the
// `ergo-indexer-migratedb` binary. It is intentionally a different struct from
// `SqliteDb` above — the running indexer's connection-init path stays untouched.
//
// Differences from `SqliteDb::open`:
//   * single connection (writer-only — the migrator never reads concurrently)
//   * `foreign_keys=OFF` for the entire migration (per facts/indexer-migration.md
//     § Per-block migration unit). FK ordering inside each block is correct, but
//     belt-and-suspenders against any future ordering surprise.
//   * wraps the rusqlite connection in `std::sync::Mutex` (not tokio's), because
//     every method dispatches to `tokio::task::spawn_blocking` which is sync.

use std::sync::Mutex as StdMutex;

use crate::migrate::{
    Backend, BlockData, BlockRow as MBlockRow, BoxRegisterRow as MBoxRegisterRow,
    BoxRow as MBoxRow, BoxSpentUpdate, BoxTokenRow as MBoxTokenRow, Cursor,
    TokenRow as MTokenRow, TransactionRow as MTransactionRow,
};

const PRAGMAS_MIGRATION: &str = "\
    PRAGMA journal_mode=WAL; \
    PRAGMA synchronous=NORMAL; \
    PRAGMA foreign_keys=OFF; \
    PRAGMA cache_size=-65536; \
    PRAGMA mmap_size=268435456; \
    PRAGMA temp_store=MEMORY; \
    PRAGMA busy_timeout=5000;";

pub struct SqliteBackend {
    conn: Arc<StdMutex<Connection>>,
}

impl SqliteBackend {
    /// Open a SQLite database for migration. Creates the file if absent.
    ///
    /// This constructor is intentionally separate from `SqliteDb::open` — the
    /// migrator wants `foreign_keys=OFF` and a single connection. The running
    /// indexer's two-connection (writer+reader) path is untouched.
    pub async fn new_for_migration(path: &std::path::Path) -> Result<Self> {
        let path = path.to_owned();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open(&path)
                .with_context(|| format!("failed to open SQLite at {}", path.display()))?;
            conn.execute_batch(PRAGMAS_MIGRATION)?;
            Ok(conn)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))??;
        Ok(Self {
            conn: Arc::new(StdMutex::new(conn)),
        })
    }
}

/// Convert `Vec<u8>` from a BLOB column into `[u8; 32]`. SQLite has no
/// fixed-width binary type, so length is asserted at read time.
fn vec_to_id32(v: Vec<u8>, col: &str) -> Result<[u8; 32]> {
    let len = v.len();
    v.try_into()
        .map_err(|_| anyhow::anyhow!("column {col}: expected 32 bytes, got {len}"))
}

#[async_trait]
impl Backend for SqliteBackend {
    async fn schema_version(&mut self) -> Result<Option<u32>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<u32>> {
            let conn = conn.lock().expect("sqlite migration mutex poisoned");
            // Table may not exist on a freshly-created DB; treat that as "no
            // schema_version yet". sqlite_master is the canonical check.
            let table_exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='indexer_state'",
                    [],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !table_exists {
                return Ok(None);
            }
            let v: Option<String> = conn
                .query_row(
                    "SELECT value FROM indexer_state WHERE key = 'schema_version'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(v.and_then(|s| s.parse().ok()))
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))?
    }

    async fn init_schema(&mut self, version: u32) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().expect("sqlite migration mutex poisoned");
            conn.execute_batch(CREATE_TABLES)?;
            conn.execute(
                "INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('schema_version', ?1)",
                params![version.to_string()],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))?
    }

    async fn read_cursor(&mut self) -> Result<Option<Cursor>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<Cursor>> {
            let conn = conn.lock().expect("sqlite migration mutex poisoned");
            // indexer_state may not exist yet (fresh DB pre-init_schema).
            let table_exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='indexer_state'",
                    [],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !table_exists {
                return Ok(None);
            }
            let cursor_v: Option<String> = conn
                .query_row(
                    "SELECT value FROM indexer_state WHERE key = 'migration_cursor'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let source_v: Option<String> = conn
                .query_row(
                    "SELECT value FROM indexer_state WHERE key = 'migration_source'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let fp_v: Option<String> = conn
                .query_row(
                    "SELECT value FROM indexer_state WHERE key = 'migration_source_fingerprint'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;

            match (cursor_v, source_v, fp_v) {
                (None, None, None) => Ok(None),
                (Some(c), Some(s), Some(f)) => {
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
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))?
    }

    async fn write_cursor(&mut self, cursor: &Cursor) -> Result<()> {
        let conn = self.conn.clone();
        let cursor = cursor.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().expect("sqlite migration mutex poisoned");
            let tx = conn.unchecked_transaction()?;
            write_cursor_inline(&tx, &cursor)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))?
    }

    async fn max_height(&mut self) -> Result<Option<u32>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<u32>> {
            let conn = conn.lock().expect("sqlite migration mutex poisoned");
            // SELECT MAX returns one row of NULL when the table is empty.
            let v: Option<i64> = conn
                .query_row("SELECT MAX(height) FROM blocks", [], |row| row.get(0))
                .optional()?
                .flatten();
            Ok(v.map(|h| h as u32))
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))?
    }

    async fn read_block_data(&mut self, height: u32) -> Result<BlockData> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<BlockData> {
            let conn = conn.lock().expect("sqlite migration mutex poisoned");
            let h = height as i64;

            // blocks row at H — exactly one
            let block: MBlockRow = conn.query_row(
                "SELECT height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count \
                 FROM blocks WHERE height = ?1",
                params![h],
                |row| {
                    let hid: Vec<u8> = row.get(1)?;
                    let mpk: Vec<u8> = row.get(4)?;
                    Ok((
                        row.get::<_, i64>(0)? as u32,
                        hid,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        mpk,
                        row.get::<_, i64>(5)? as u32,
                        row.get::<_, i64>(6)? as u32,
                    ))
                },
            )
            .map_err(|e| anyhow::anyhow!("blocks row missing at height {height}: {e}"))
            .and_then(|(height, hid, ts, diff, mpk, bs, tc)| {
                Ok(MBlockRow {
                    height,
                    header_id: vec_to_id32(hid, "blocks.header_id")?,
                    timestamp: ts,
                    difficulty: diff,
                    miner_pk: mpk,
                    block_size: bs,
                    tx_count: tc,
                })
            })?;

            // transactions WHERE height = H
            let mut stmt = conn.prepare_cached(
                "SELECT tx_id, header_id, height, tx_index, size \
                 FROM transactions WHERE height = ?1 ORDER BY tx_index",
            )?;
            let transactions: Vec<MTransactionRow> = stmt
                .query_map(params![h], |row| {
                    let tid: Vec<u8> = row.get(0)?;
                    let hid: Vec<u8> = row.get(1)?;
                    Ok((tid, hid, row.get::<_, i64>(2)? as u32, row.get::<_, i64>(3)? as u32, row.get::<_, i64>(4)? as u32))
                })?
                .map(|r| {
                    let (tid, hid, height, tx_index, size) = r?;
                    Ok(MTransactionRow {
                        tx_id: vec_to_id32(tid, "transactions.tx_id")?,
                        header_id: vec_to_id32(hid, "transactions.header_id")?,
                        height,
                        tx_index,
                        size,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            drop(stmt);

            // boxes WHERE height = H (created-at-H)
            let mut stmt = conn.prepare_cached(
                "SELECT box_id, tx_id, header_id, height, output_index, ergo_tree, \
                        ergo_tree_hash, address, value, spent_tx_id, spent_height \
                 FROM boxes WHERE height = ?1 ORDER BY box_id",
            )?;
            let created_boxes: Vec<MBoxRow> = stmt
                .query_map(params![h], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                        row.get::<_, Vec<u8>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, Option<Vec<u8>>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                    ))
                })?
                .map(|r| {
                    let (bid, tid, hid, height, oi, et, eth, addr, val, stid, sh) = r?;
                    Ok(MBoxRow {
                        box_id: vec_to_id32(bid, "boxes.box_id")?,
                        tx_id: vec_to_id32(tid, "boxes.tx_id")?,
                        header_id: vec_to_id32(hid, "boxes.header_id")?,
                        height: height as u32,
                        output_index: oi as u32,
                        ergo_tree: et,
                        ergo_tree_hash: vec_to_id32(eth, "boxes.ergo_tree_hash")?,
                        address: addr,
                        value: val,
                        spent_tx_id: stid
                            .map(|v| vec_to_id32(v, "boxes.spent_tx_id"))
                            .transpose()?,
                        spent_height: sh.map(|s| s as u32),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            drop(stmt);

            // box_registers / box_tokens for the just-created boxes
            let mut box_registers: Vec<MBoxRegisterRow> = Vec::new();
            let mut box_tokens: Vec<MBoxTokenRow> = Vec::new();
            for bx in &created_boxes {
                let mut stmt = conn.prepare_cached(
                    "SELECT box_id, register_id, serialized FROM box_registers WHERE box_id = ?1 ORDER BY register_id",
                )?;
                let regs: Vec<MBoxRegisterRow> = stmt
                    .query_map(params![bx.box_id.as_slice()], |row| {
                        let bid: Vec<u8> = row.get(0)?;
                        let rid: i64 = row.get(1)?;
                        let ser: Vec<u8> = row.get(2)?;
                        Ok((bid, rid, ser))
                    })?
                    .map(|r| {
                        let (bid, rid, ser) = r?;
                        Ok(MBoxRegisterRow {
                            box_id: vec_to_id32(bid, "box_registers.box_id")?,
                            register_id: rid as u8,
                            serialized: ser,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                box_registers.extend(regs);

                let mut stmt = conn.prepare_cached(
                    "SELECT box_id, token_id, amount FROM box_tokens WHERE box_id = ?1 ORDER BY token_id",
                )?;
                let toks: Vec<MBoxTokenRow> = stmt
                    .query_map(params![bx.box_id.as_slice()], |row| {
                        let bid: Vec<u8> = row.get(0)?;
                        let tid: Vec<u8> = row.get(1)?;
                        let amt: i64 = row.get(2)?;
                        Ok((bid, tid, amt))
                    })?
                    .map(|r| {
                        let (bid, tid, amt) = r?;
                        Ok(MBoxTokenRow {
                            box_id: vec_to_id32(bid, "box_tokens.box_id")?,
                            token_id: vec_to_id32(tid, "box_tokens.token_id")?,
                            amount: amt,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                box_tokens.extend(toks);
            }

            // tokens WHERE minting_height = H
            let mut stmt = conn.prepare_cached(
                "SELECT token_id, minting_tx_id, minting_height, name, description, decimals \
                 FROM tokens WHERE minting_height = ?1 ORDER BY token_id",
            )?;
            let minted_tokens: Vec<MTokenRow> = stmt
                .query_map(params![h], |row| {
                    let tid: Vec<u8> = row.get(0)?;
                    let mtid: Vec<u8> = row.get(1)?;
                    Ok((
                        tid,
                        mtid,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                })?
                .map(|r| {
                    let (tid, mtid, mh, name, desc, dec) = r?;
                    Ok(MTokenRow {
                        token_id: vec_to_id32(tid, "tokens.token_id")?,
                        minting_tx_id: vec_to_id32(mtid, "tokens.minting_tx_id")?,
                        minting_height: mh as u32,
                        name,
                        description: desc,
                        decimals: dec.map(|d| d as i32),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            drop(stmt);

            // boxes WHERE spent_height = H → spent updates
            let mut stmt = conn.prepare_cached(
                "SELECT box_id, spent_tx_id, spent_height FROM boxes \
                 WHERE spent_height = ?1 ORDER BY box_id",
            )?;
            let spent_box_updates: Vec<BoxSpentUpdate> = stmt
                .query_map(params![h], |row| {
                    let bid: Vec<u8> = row.get(0)?;
                    let stid: Vec<u8> = row.get(1)?;
                    let sh: i64 = row.get(2)?;
                    Ok((bid, stid, sh))
                })?
                .map(|r| {
                    let (bid, stid, sh) = r?;
                    Ok(BoxSpentUpdate {
                        box_id: vec_to_id32(bid, "boxes.box_id")?,
                        spent_tx_id: vec_to_id32(stid, "boxes.spent_tx_id")?,
                        spent_height: sh as u32,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            drop(stmt);

            Ok(BlockData {
                block,
                transactions,
                created_boxes,
                box_registers,
                box_tokens,
                minted_tokens,
                spent_box_updates,
            })
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))?
    }

    async fn apply_block(&mut self, data: &BlockData, cursor: &Cursor) -> Result<()> {
        let conn = self.conn.clone();
        let data = data.clone();
        let cursor = cursor.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().expect("sqlite migration mutex poisoned");
            let tx = conn.unchecked_transaction()?;

            // 1. blocks
            tx.execute(
                "INSERT INTO blocks (height, header_id, timestamp, difficulty, miner_pk, \
                                     block_size, tx_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    data.block.height as i64,
                    data.block.header_id.as_slice(),
                    data.block.timestamp,
                    data.block.difficulty,
                    data.block.miner_pk.as_slice(),
                    data.block.block_size as i64,
                    data.block.tx_count as i64,
                ],
            )?;

            // 2. transactions
            for t in &data.transactions {
                tx.execute(
                    "INSERT INTO transactions (tx_id, header_id, height, tx_index, size) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        t.tx_id.as_slice(),
                        t.header_id.as_slice(),
                        t.height as i64,
                        t.tx_index as i64,
                        t.size as i64,
                    ],
                )?;
            }

            // 3. boxes (created)
            for b in &data.created_boxes {
                tx.execute(
                    "INSERT INTO boxes (box_id, tx_id, header_id, height, output_index, \
                                        ergo_tree, ergo_tree_hash, address, value, \
                                        spent_tx_id, spent_height) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        b.box_id.as_slice(),
                        b.tx_id.as_slice(),
                        b.header_id.as_slice(),
                        b.height as i64,
                        b.output_index as i64,
                        b.ergo_tree.as_slice(),
                        b.ergo_tree_hash.as_slice(),
                        b.address,
                        b.value,
                        b.spent_tx_id.as_ref().map(|s| s.as_slice()),
                        b.spent_height.map(|h| h as i64),
                    ],
                )?;
            }

            // 4. box_registers
            for r in &data.box_registers {
                tx.execute(
                    "INSERT INTO box_registers (box_id, register_id, serialized) \
                     VALUES (?1, ?2, ?3)",
                    params![
                        r.box_id.as_slice(),
                        r.register_id as i64,
                        r.serialized.as_slice(),
                    ],
                )?;
            }

            // 5. box_tokens
            for t in &data.box_tokens {
                tx.execute(
                    "INSERT INTO box_tokens (box_id, token_id, amount) \
                     VALUES (?1, ?2, ?3)",
                    params![t.box_id.as_slice(), t.token_id.as_slice(), t.amount],
                )?;
            }

            // 6. tokens (minted at H)
            for t in &data.minted_tokens {
                tx.execute(
                    "INSERT INTO tokens (token_id, minting_tx_id, minting_height, name, description, decimals) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        t.token_id.as_slice(),
                        t.minting_tx_id.as_slice(),
                        t.minting_height as i64,
                        t.name,
                        t.description,
                        t.decimals.map(|d| d as i64),
                    ],
                )?;
            }

            // 7. boxes spent updates
            for s in &data.spent_box_updates {
                tx.execute(
                    "UPDATE boxes SET spent_tx_id = ?1, spent_height = ?2 WHERE box_id = ?3",
                    params![
                        s.spent_tx_id.as_slice(),
                        s.spent_height as i64,
                        s.box_id.as_slice(),
                    ],
                )?;
            }

            // 8. cursor
            write_cursor_inline(&tx, &cursor)?;

            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))?
    }
}

/// Inline the 3-key cursor write inside a caller-controlled transaction.
/// Shared by `write_cursor` (its own short-lived tx) and `apply_block` (the
/// per-block tx). Per facts/indexer-migration.md § Resume Semantics: the 3
/// keys are `migration_cursor`, `migration_source`, `migration_source_fingerprint`.
fn write_cursor_inline(tx: &rusqlite::Transaction<'_>, cursor: &Cursor) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('migration_cursor', ?1)",
        params![cursor.last_height.to_string()],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('migration_source', ?1)",
        params![cursor.source_url],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('migration_source_fingerprint', ?1)",
        params![hex::encode(cursor.source_fingerprint)],
    )?;
    Ok(())
}

#[cfg(test)]
mod migration_backend_tests {
    use super::*;
    use crate::migrate::hash::hash_block;

    /// Deterministic synthetic block data for a given height + seed. Used by
    /// the apply→read round-trip test. Mirrors the fixture from migrate/hash.rs
    /// tests so encoding/sort semantics stay consistent across modules.
    fn build_synth_block_data(height: u32, seed: u8) -> BlockData {
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
                // Created-this-block rows are unspent at creation. The spent
                // state for a box created here would arrive at a future
                // height as a `BoxSpentUpdate`; the spent-at-prior-height
                // updates this block performs live in spent_box_updates.
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

        // Spent-update: a box from a PRIOR height (we use a synthetic
        // box_id that doesn't collide with `created_boxes`) was spent at
        // this height.
        let spent_box_updates = vec![BoxSpentUpdate {
            box_id: [seed.wrapping_add(200); 32],
            spent_tx_id: transactions[1].tx_id,
            spent_height: height,
        }];

        BlockData {
            block,
            transactions,
            created_boxes,
            box_registers,
            box_tokens,
            minted_tokens,
            spent_box_updates,
        }
    }

    #[tokio::test]
    async fn sqlite_backend_schema_version_roundtrip() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let mut b = SqliteBackend::new_for_migration(db.path()).await.unwrap();
        assert!(b.schema_version().await.unwrap().is_none());
        b.init_schema(1).await.unwrap();
        assert_eq!(b.schema_version().await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn sqlite_backend_cursor_roundtrip() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let mut b = SqliteBackend::new_for_migration(db.path()).await.unwrap();
        b.init_schema(1).await.unwrap();
        assert!(b.read_cursor().await.unwrap().is_none());
        let c = Cursor {
            last_height: 42,
            source_url: "sqlite:///source".into(),
            source_fingerprint: [0xAB; 32],
        };
        b.write_cursor(&c).await.unwrap();
        assert_eq!(b.read_cursor().await.unwrap(), Some(c));
    }

    /// Pre-seed a target box row at a prior height so the `spent_box_updates`
    /// path of `apply_block` has a row to UPDATE. In production migration the
    /// prior-height block carrying the soon-to-be-spent box is copied first;
    /// in a single-block unit test we have to plant the box manually.
    async fn seed_prior_height_box(
        backend: &mut SqliteBackend,
        box_id: [u8; 32],
        prior_height: u32,
        cursor_after_seed: &Cursor,
    ) {
        let tx_id = [0xEEu8; 32];
        let header_id = [0xDDu8; 32];

        let prior_block = MBlockRow {
            height: prior_height,
            header_id,
            timestamp: 1_699_000_000_000,
            difficulty: 999_999,
            miner_pk: vec![0u8; 33],
            block_size: 100,
            tx_count: 1,
        };
        let prior_tx = MTransactionRow {
            tx_id,
            header_id,
            height: prior_height,
            tx_index: 0,
            size: 100,
        };
        let prior_box = MBoxRow {
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
        };
        let prior_data = BlockData {
            block: prior_block,
            transactions: vec![prior_tx],
            created_boxes: vec![prior_box],
            box_registers: vec![],
            box_tokens: vec![],
            minted_tokens: vec![],
            spent_box_updates: vec![],
        };
        backend
            .apply_block(&prior_data, cursor_after_seed)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn sqlite_backend_apply_block_then_read_back() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let mut b = SqliteBackend::new_for_migration(db.path()).await.unwrap();
        b.init_schema(1).await.unwrap();

        let data = build_synth_block_data(100, 0x33);

        // Pre-seed: the spent_box_updates row references a box created at a
        // prior height (the migrator would have copied that height first in
        // real usage). Plant it now.
        let prior_cursor = Cursor {
            last_height: 50,
            source_url: "sqlite:///fake".into(),
            source_fingerprint: [0u8; 32],
        };
        let spent_target = data.spent_box_updates[0].box_id;
        seed_prior_height_box(&mut b, spent_target, 50, &prior_cursor).await;

        let cursor = Cursor {
            last_height: 100,
            source_url: "sqlite:///fake".into(),
            source_fingerprint: [0u8; 32],
        };

        b.apply_block(&data, &cursor).await.unwrap();
        let read_back = b.read_block_data(100).await.unwrap();

        // Structural equality per vector (the hash test below would catch any
        // mismatch, but field-level asserts give a sharper diagnostic).
        assert_eq!(read_back.block, data.block);
        assert_eq!(read_back.transactions, data.transactions);
        assert_eq!(read_back.created_boxes, data.created_boxes);
        assert_eq!(read_back.box_registers, data.box_registers);
        assert_eq!(read_back.box_tokens, data.box_tokens);
        assert_eq!(read_back.minted_tokens, data.minted_tokens);
        assert_eq!(read_back.spent_box_updates, data.spent_box_updates);

        // The cross-backend acceptance test: hash of round-tripped data MUST
        // equal hash of input. If this breaks, T5's hash will report false
        // mismatches for every block of every migration.
        assert_eq!(hash_block(&read_back), hash_block(&data));

        // Cursor was committed atomically with the block data.
        assert_eq!(b.read_cursor().await.unwrap(), Some(cursor));

        // max_height reflects the inserted block.
        assert_eq!(b.max_height().await.unwrap(), Some(100));
    }
}

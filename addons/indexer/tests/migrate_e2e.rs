//! End-to-end integration tests for `ergo-indexer-migratedb`.
//!
//! Three scenarios per the contract (`facts/indexer-migration.md`):
//!   1. SQLite → Postgres full migration of a synthetic chain
//!   2. Postgres → SQLite full migration of the same shape
//!   3. SQLite → Postgres with a pre-seeded partial target, run with --resume
//!
//! Per-test isolation:
//!   - SQLite: each test gets its own `tempfile::tempdir()`; no cross-test
//!     collision.
//!   - PostgreSQL: each test CREATEs a uniquely-named DB on
//!     `127.0.0.1:5432` as role `mwaddip` (libpq picks up the password from
//!     `~/.pgpass`). DROP runs in a synchronous `Drop` impl on a fresh
//!     runtime — survives test-runner panics so we don't leak DBs.
//!
//! Liveness gate: the binary refuses to proceed if it sees a running indexer.
//! Tests set `INDEXER_BIND=127.0.0.1:1` so the API ping fails fast (no
//! listener at port 1).
//!
//! Why bypass `--update-config`: we don't want tests touching
//! `/etc/ergo-node/indexer.toml`. The flag is exercised in the unit tests
//! for `config_update`.
//!
//! Resume strategy: Option C (pre-seeded cursor). We populate the PG target
//! directly via `PostgresBackend::apply_block` to mimic a partial migration,
//! then run the binary with `--resume`. This is deterministic — no SIGTERM
//! timing dependencies.

#![cfg(all(feature = "sqlite", feature = "postgres"))]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ergo_indexer::db::postgres::PostgresBackend;
use ergo_indexer::db::sqlite::SqliteBackend;
use ergo_indexer::migrate::{
    Backend, BlockData, BlockRow, BoxRegisterRow, BoxRow, BoxSpentUpdate, BoxTokenRow, Cursor,
    TokenRow, TransactionRow,
};

// ---------------------------------------------------------------------------
// PG test-database lifecycle. Mirrors the `PgTestDb` pattern from
// `src/db/postgres.rs` — copied (not imported) because the original is
// `#[cfg(test)]` private to the postgres module.
// ---------------------------------------------------------------------------

struct PgTestDb {
    name: String,
    url: String,
}

impl PgTestDb {
    /// `test_migrate_e2e_<unix_nanos>_<pid>_<counter>` — unique across
    /// parallel runs and within a single process.
    fn unique_name() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("test_migrate_e2e_{nanos}_{pid}_{n}")
    }

    /// Spawn `psql` to CREATE DATABASE. The admin connection is opened,
    /// used, and torn down by the psql subprocess — no sqlx dev-dep needed.
    fn create() -> Self {
        let name = Self::unique_name();
        let out = StdCommand::new("psql")
            .args([
                "-h",
                "127.0.0.1",
                "-U",
                "mwaddip",
                "-d",
                "postgres",
                "-c",
                &format!("CREATE DATABASE {name}"),
            ])
            .output()
            .expect("spawn psql for CREATE DATABASE");
        assert!(
            out.status.success(),
            "CREATE DATABASE {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let url = format!("postgres://mwaddip@127.0.0.1:5432/{name}");
        Self { name, url }
    }
}

impl Drop for PgTestDb {
    fn drop(&mut self) {
        // Synchronous DROP — uses psql, not sqlx, so no runtime gymnastics.
        // pg_terminate_backend kicks any leftover sessions off the test DB
        // (sqlx connection-pool Drop ought to have closed them already, but
        // DROP DATABASE refuses with a non-empty backend list).
        let _ = StdCommand::new("psql")
            .args([
                "-h",
                "127.0.0.1",
                "-U",
                "mwaddip",
                "-d",
                "postgres",
                "-c",
                &format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = '{}' AND pid <> pg_backend_pid()",
                    self.name
                ),
            ])
            .output();
        let out = StdCommand::new("psql")
            .args([
                "-h",
                "127.0.0.1",
                "-U",
                "mwaddip",
                "-d",
                "postgres",
                "-c",
                &format!("DROP DATABASE IF EXISTS {}", self.name),
            ])
            .output();
        match out {
            Ok(o) if !o.status.success() => {
                eprintln!(
                    "PgTestDb cleanup DROP DATABASE {} failed: {}",
                    self.name,
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Err(e) => eprintln!("PgTestDb cleanup psql spawn failed for {}: {e}", self.name),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic synthetic chain fixture.
//
// Real-world data shape (and the only one the migrator's walk supports):
//   - The migrator walks heights `1..=max_height`. Anything below 1 is never
//     copied.
//   - h=1 is the "genesis" of our synthetic chain. It creates a stash of
//     long-lived "spendable" boxes alongside its normal per-height payload.
//   - h>1 may reference one of those h=1 boxes in `spent_box_updates`. The
//     migrator will have copied that h=1 row already when it reaches h, so
//     the UPDATE finds a row to mutate.
//   - At `SAME_HEIGHT_CREATE_SPEND`, an extra box is both CREATED and SPENT
//     within the same block. This mirrors the mainnet pattern where a tx
//     creates a box that's spent later in the same block.
//
// Each height carries:
//   - A block row
//   - 2 transactions (3 at h=1 for the seed-stash creator tx; 3 at
//     SAME_HEIGHT_CREATE_SPEND for the create-then-spend tx)
//   - 2 created boxes (plus extras at h=1 and at SAME_HEIGHT_CREATE_SPEND)
//   - Box registers + tokens for each created box
//   - 1 minted token
//   - 1+ spent_box_updates referencing a box created at h=1
// ---------------------------------------------------------------------------

const NUM_BLOCKS: u32 = 10;
const SAME_HEIGHT_CREATE_SPEND: u32 = 7;

/// Synthetic block data for a given height. The migrator's `apply_block`
/// will normalize same-height create-and-spend patterns (INSERT the box as
/// unspent, then UPDATE it to spent), so the test must NOT assert against
/// these inputs verbatim — instead it asserts that the migrator's source-vs-
/// target hash agreement is reached, and that source.read_block_data(h) and
/// target.read_block_data(h) hash identically after migration.
fn synth_block(height: u32) -> BlockData {
    let seed = height as u8;
    let header_id = block_header_id(height);

    let mut tx_count = 2u32;
    if height == 1 || height == SAME_HEIGHT_CREATE_SPEND {
        tx_count = 3;
    }

    let transactions: Vec<TransactionRow> = (0u32..tx_count)
        .map(|i| TransactionRow {
            tx_id: tx_id_for(height, i),
            header_id,
            height,
            tx_index: i,
            size: 200u32 + i + height,
        })
        .collect();

    let block = BlockRow {
        height,
        header_id,
        timestamp: 1_700_000_000_000i64 + height as i64,
        difficulty: 1_000_000i64 + height as i64,
        miner_pk: (0u8..33).map(|i| seed.wrapping_add(i)).collect(),
        block_size: 1000u32 + height,
        tx_count,
    };

    // Base: 2 created boxes (one per first-two txs).
    let mut created_boxes: Vec<BoxRow> = (0u32..2)
        .map(|i| BoxRow {
            box_id: created_box_id(height, i),
            tx_id: transactions[i as usize].tx_id,
            header_id,
            height,
            output_index: i,
            ergo_tree: vec![0x10u8, 0x01, 0x04, seed],
            ergo_tree_hash: [seed.wrapping_add(99); 32],
            address: format!("9addr_{height}_{i}"),
            value: 1_000_000i64 + i as i64 + height as i64,
            spent_tx_id: None,
            spent_height: None,
        })
        .collect();

    // At h=1, additionally create NUM_BLOCKS - 1 "spendable" boxes that
    // h=2..=NUM_BLOCKS will each spend (one per height). Distinct from the
    // base created_boxes via the `spendable_box_id` namespace.
    if height == 1 {
        for spender_h in 2u32..=NUM_BLOCKS {
            created_boxes.push(BoxRow {
                box_id: spendable_box_id(spender_h),
                tx_id: transactions[2].tx_id,
                header_id,
                height,
                output_index: spender_h - 1,
                ergo_tree: vec![0x20u8, spender_h as u8],
                ergo_tree_hash: [0x88u8; 32],
                address: format!("9spendable_for_{spender_h}"),
                value: 100_000i64 + spender_h as i64,
                spent_tx_id: None,
                spent_height: None,
            });
        }
    }

    // At SAME_HEIGHT_CREATE_SPEND, create one extra box that will also be
    // spent within this block. This INSERT writes spent_tx_id=None; the
    // matching `BoxSpentUpdate` (below) will set it via UPDATE.
    if height == SAME_HEIGHT_CREATE_SPEND {
        created_boxes.push(BoxRow {
            box_id: same_height_box_id(height),
            tx_id: transactions[2].tx_id,
            header_id,
            height,
            output_index: 99,
            ergo_tree: vec![0x30u8, seed],
            ergo_tree_hash: [0x77u8; 32],
            address: format!("9same_height_create_spend_{height}"),
            value: 42_000,
            spent_tx_id: None,
            spent_height: None,
        });
    }

    let box_registers: Vec<BoxRegisterRow> = created_boxes
        .iter()
        .map(|b| BoxRegisterRow {
            box_id: b.box_id,
            register_id: 4,
            serialized: vec![0x05u8, seed, 0x42],
        })
        .collect();

    let box_tokens: Vec<BoxTokenRow> = created_boxes
        .iter()
        .map(|b| BoxTokenRow {
            box_id: b.box_id,
            token_id: [seed.wrapping_add(50); 32],
            amount: 100i64 + height as i64,
        })
        .collect();

    let minted_tokens = vec![TokenRow {
        token_id: [seed.wrapping_add(77); 32],
        minting_tx_id: transactions[0].tx_id,
        minting_height: height,
        name: Some(format!("token_{height}")),
        description: None,
        decimals: Some(seed as i32),
    }];

    // At h=1, no spent updates (nothing exists to spend yet).
    // At h>1, spend the spendable_box_id(h) box created at h=1.
    let mut spent_box_updates = vec![];
    if height > 1 {
        spent_box_updates.push(BoxSpentUpdate {
            box_id: spendable_box_id(height),
            spent_tx_id: transactions[1].tx_id,
            spent_height: height,
        });
    }
    if height == SAME_HEIGHT_CREATE_SPEND {
        // The extra-created box from above gets spent in the same block.
        spent_box_updates.push(BoxSpentUpdate {
            box_id: same_height_box_id(height),
            spent_tx_id: transactions[0].tx_id,
            spent_height: height,
        });
    }

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

fn block_header_id(height: u32) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[..4].copy_from_slice(&height.to_be_bytes());
    id[31] = 0xAA;
    id
}

fn tx_id_for(height: u32, i: u32) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[..4].copy_from_slice(&height.to_be_bytes());
    id[4..8].copy_from_slice(&i.to_be_bytes());
    id[31] = 0xBB;
    id
}

fn created_box_id(height: u32, i: u32) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[..4].copy_from_slice(&height.to_be_bytes());
    id[4..8].copy_from_slice(&i.to_be_bytes());
    id[31] = 0xCC;
    id
}

/// Distinct namespace for the "created at h=1, spent at h>1" boxes. The
/// `0xDD` tail byte separates these from the per-height `created_box_id`
/// (`0xCC`) so the two namespaces don't collide.
fn spendable_box_id(spent_at_height: u32) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[..4].copy_from_slice(&spent_at_height.to_be_bytes());
    id[31] = 0xDD;
    id
}

/// Box created and spent within the same block at SAME_HEIGHT_CREATE_SPEND.
fn same_height_box_id(height: u32) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[..4].copy_from_slice(&height.to_be_bytes());
    id[31] = 0xEE;
    id
}

/// Populate a backend with a synthetic `1..=NUM_BLOCKS` chain. The cursor at
/// each height is written by apply_block per the contract. After this
/// returns, `backend.max_height()` is `NUM_BLOCKS`.
async fn populate_chain<B: Backend + ?Sized>(backend: &mut B, source_url: &str) {
    backend
        .init_schema(1)
        .await
        .expect("populate_chain: init_schema");

    for h in 1..=NUM_BLOCKS {
        let data = synth_block(h);
        let cursor = Cursor {
            last_height: h,
            source_url: source_url.to_string(),
            source_fingerprint: [0u8; 32], // not load-bearing during populate
        };
        backend
            .apply_block(&data, &cursor)
            .await
            .unwrap_or_else(|e| panic!("populate_chain apply_block h={h}: {e}"));
    }
}

/// Populate only heights `1..=stop_at`. Used by the resume scenario to plant
/// a partial target. The cursor at `stop_at` carries the real source
/// fingerprint so the runner's check-4 (fingerprint match) passes.
async fn populate_chain_partial<B: Backend + ?Sized>(
    backend: &mut B,
    source_url: &str,
    stop_at: u32,
    source_fingerprint: [u8; 32],
) {
    assert!(stop_at <= NUM_BLOCKS, "stop_at out of range");
    backend
        .init_schema(1)
        .await
        .expect("populate_chain_partial: init_schema");

    for h in 1..=stop_at {
        let data = synth_block(h);
        let cursor = Cursor {
            last_height: h,
            source_url: source_url.to_string(),
            source_fingerprint,
        };
        backend
            .apply_block(&data, &cursor)
            .await
            .unwrap_or_else(|e| panic!("populate_chain_partial apply_block h={h}: {e}"));
    }
}

/// Run the migrator binary with the given args and standard test env.
/// Returns the subprocess output; the caller asserts on status / stderr.
fn run_migrator(args: &[&str]) -> std::process::Output {
    StdCommand::new(env!("CARGO_BIN_EXE_ergo-indexer-migratedb"))
        // INDEXER_BIND=127.0.0.1:1 — port 1 is the IETF "tcpmux" port; no
        // listener in any sane test env. The API ping fails fast (connection
        // refused). This bypasses the liveness gate without needing
        // /etc/ergo-node/indexer.toml.
        .env("INDEXER_BIND", "127.0.0.1:1")
        .args(args)
        .output()
        .expect("spawn ergo-indexer-migratedb")
}

/// Open a `PostgresBackend` against the given URL.
async fn open_pg(url: &str) -> PostgresBackend {
    PostgresBackend::new_for_migration(url)
        .await
        .expect("PostgresBackend::new_for_migration")
}

/// Open a `SqliteBackend` against the given filesystem path.
async fn open_sqlite(path: &Path) -> SqliteBackend {
    SqliteBackend::new_for_migration(path)
        .await
        .expect("SqliteBackend::new_for_migration")
}

/// Assert that the target backend's full chain matches the source backend's
/// at every height in `1..=NUM_BLOCKS`. This is the migrator's own contract:
/// `blake2b256(source.read_block_data(H)) == blake2b256(target.read_block_data(H))`
/// after a successful migration.
///
/// Comparing against the source's `read_block_data` rather than the input
/// fixture is load-bearing: `apply_block` normalizes same-height
/// create-and-spend patterns (the box INSERTs as unspent then UPDATEs to
/// spent), so the stored state always differs from the input fixture for
/// such boxes. The hash check the migrator runs is source-vs-target, not
/// fixture-vs-target.
async fn assert_target_matches_source<S, T>(source: &mut S, target: &mut T)
where
    S: Backend + ?Sized,
    T: Backend + ?Sized,
{
    let src_max = source
        .max_height()
        .await
        .expect("source max_height")
        .unwrap();
    let tgt_max = target
        .max_height()
        .await
        .expect("target max_height")
        .unwrap();
    assert_eq!(
        src_max, NUM_BLOCKS,
        "source max_height should be NUM_BLOCKS"
    );
    assert_eq!(
        tgt_max, NUM_BLOCKS,
        "target max_height should be NUM_BLOCKS"
    );

    let cursor = target.read_cursor().await.expect("read_cursor").unwrap();
    assert_eq!(
        cursor.last_height, NUM_BLOCKS,
        "cursor.last_height should be NUM_BLOCKS"
    );

    for h in 1..=NUM_BLOCKS {
        let src_data = source
            .read_block_data(h)
            .await
            .unwrap_or_else(|e| panic!("source read_block_data h={h}: {e}"));
        let tgt_data = target
            .read_block_data(h)
            .await
            .unwrap_or_else(|e| panic!("target read_block_data h={h}: {e}"));

        let src_hash = ergo_indexer::migrate::hash::hash_block(&src_data);
        let tgt_hash = ergo_indexer::migrate::hash::hash_block(&tgt_data);
        assert_eq!(
            src_hash,
            tgt_hash,
            "block hash mismatch at h={h}: source={} target={}",
            hex::encode(src_hash),
            hex::encode(tgt_hash)
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migrate_sqlite_to_postgres_full() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_path: PathBuf = dir.path().join("source.db");
    let src_url = format!("sqlite://{}", src_path.display());

    // 1. Populate SQLite source.
    {
        let mut src = open_sqlite(&src_path).await;
        populate_chain(&mut src, &src_url).await;
    } // drop the SqliteBackend so the binary can take an EXCLUSIVE lock

    // 2. CREATE the PG target.
    let pg = PgTestDb::create();

    // 3. Run the migrator.
    let out = run_migrator(&["--in", &src_url, "--out", &pg.url, "-y"]);
    assert!(
        out.status.success(),
        "migrator failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // 4. Verify target matches source.
    let mut src = open_sqlite(&src_path).await;
    let mut tgt = open_pg(&pg.url).await;
    assert_target_matches_source(&mut src, &mut tgt).await;
}

#[tokio::test]
async fn migrate_postgres_to_sqlite_full() {
    // 1. CREATE the PG source.
    let pg = PgTestDb::create();
    {
        let mut src = open_pg(&pg.url).await;
        populate_chain(&mut src, &pg.url).await;
    }

    // 2. Pick an absent SQLite target path.
    let dir = tempfile::tempdir().expect("tempdir");
    let tgt_path: PathBuf = dir.path().join("target.db");
    let tgt_url = format!("sqlite://{}", tgt_path.display());

    // 3. Run the migrator.
    let out = run_migrator(&["--in", &pg.url, "--out", &tgt_url, "-y"]);
    assert!(
        out.status.success(),
        "migrator failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // 4. Verify target matches source.
    let mut src = open_pg(&pg.url).await;
    let mut tgt = open_sqlite(&tgt_path).await;
    assert_target_matches_source(&mut src, &mut tgt).await;
}

#[tokio::test]
async fn migrate_sqlite_to_postgres_resume() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_path: PathBuf = dir.path().join("source.db");
    let src_url = format!("sqlite://{}", src_path.display());

    // 1. Populate SQLite source (10 blocks).
    let h1_fingerprint = {
        let mut src = open_sqlite(&src_path).await;
        populate_chain(&mut src, &src_url).await;
        // The runner's fingerprint = blake2b256 of source's block at h=1.
        let h1 = src.read_block_data(1).await.expect("read h=1");
        ergo_indexer::migrate::hash::hash_block(&h1)
    };

    // 2. CREATE the PG target and pre-seed it with heights 1..=5 + a cursor
    //    at h=5 whose fingerprint matches the source's h=1. This is
    //    Option C: deterministic resume, no SIGTERM timing.
    let pg = PgTestDb::create();
    let stop_at = 5u32;
    {
        let mut tgt = open_pg(&pg.url).await;
        populate_chain_partial(&mut tgt, &src_url, stop_at, h1_fingerprint).await;

        // Sanity-check the partial state.
        let cursor = tgt.read_cursor().await.unwrap().unwrap();
        assert_eq!(cursor.last_height, stop_at);
        assert_eq!(cursor.source_url, src_url);
        assert_eq!(cursor.source_fingerprint, h1_fingerprint);
    }

    // 3. Run the migrator with --resume.
    let out = run_migrator(&["--in", &src_url, "--out", &pg.url, "--resume", "-y"]);
    assert!(
        out.status.success(),
        "resume migrator failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // 4. Final state must match the source's full chain.
    let mut src = open_sqlite(&src_path).await;
    let mut tgt = open_pg(&pg.url).await;
    assert_target_matches_source(&mut src, &mut tgt).await;
}

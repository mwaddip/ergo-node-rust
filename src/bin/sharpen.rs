//! sharpen — cut the node's chain tip off above a given height.
//!
//! Rolls back `state.redb` to the AVL digest of the target header and
//! deletes all header/section/fork data above the target height from
//! `modifiers.redb`. When the node restarts it resumes from the target
//! height as if it had been stopped there.
//!
//! Optionally also truncates the indexer's SQLite DB above the target
//! height, keeping node and indexer aligned. Uses the sqlite3 CLI.
//!
//! Usage:
//!   sharpen <height> [--data-dir PATH] [--indexer] [--indexer-db PATH]
//!
//! Defaults:
//!   --data-dir    /var/lib/ergo-node/data
//!   --indexer-db  /var/lib/ergo-indexer/index.db  (only used if --indexer
//!                 or --indexer-db is passed)
//!
//! The node AND indexer MUST be stopped. redb holds an exclusive lock;
//! sqlite will happily write over a running indexer and corrupt it.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use sigma_ser::ScorexSerializable;

// Must match store/src/redb.rs — sharpen opens the DB directly.
const PRIMARY: TableDefinition<(u8, [u8; 32]), &[u8]> =
    TableDefinition::new("primary");
const HEIGHT_INDEX: TableDefinition<(u8, u32), [u8; 32]> =
    TableDefinition::new("height_index");
const HEADER_FORKS: TableDefinition<(u32, u32), [u8; 32]> =
    TableDefinition::new("header_forks");
const HEADER_SCORES: TableDefinition<[u8; 32], &[u8]> =
    TableDefinition::new("header_scores");
const BEST_CHAIN: TableDefinition<u32, [u8; 32]> =
    TableDefinition::new("best_chain");

const HEADER_TYPE_ID: u8 = 101;
const BLOCK_TRANSACTIONS_TYPE_ID: u8 = 102;
const AD_PROOFS_TYPE_ID: u8 = 104;
const EXTENSION_TYPE_ID: u8 = 108;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let target: u32 = args
        .next()
        .context("missing height argument")?
        .parse()
        .context("height must be a number")?;

    let mut data_dir = PathBuf::from("/var/lib/ergo-node/data");
    let mut indexer_db: Option<PathBuf> = None;
    let mut indexer_default = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--data-dir" => {
                data_dir = PathBuf::from(
                    args.next().context("--data-dir needs a value")?,
                );
            }
            "--indexer" => {
                indexer_default = true;
            }
            "--indexer-db" => {
                indexer_db = Some(PathBuf::from(
                    args.next().context("--indexer-db needs a value")?,
                ));
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    if indexer_default && indexer_db.is_none() {
        indexer_db = Some(PathBuf::from("/var/lib/ergo-indexer/index.db"));
    }

    let modifiers_path = data_dir.join("modifiers.redb");
    let state_path = data_dir.join("state.redb");

    // Open both databases upfront so any lock failure bails before we do
    // partial work. redb holds an exclusive lock, so if the node is
    // running either open() will fail with a clear error.
    let modifiers = Database::open(&modifiers_path)
        .with_context(|| format!("opening {}", modifiers_path.display()))?;

    let current_tip = load_best_tip_height(&modifiers)?
        .context("modifiers.redb has no best chain — nothing to sharpen")?;

    if target >= current_tip {
        bail!(
            "target height {} >= current tip {} — nothing to cut",
            target,
            current_tip
        );
    }

    // Extract state_root from the header we're keeping as the new tip.
    let target_header = read_header(&modifiers, target)?
        .with_context(|| format!("no header at height {target} in store"))?;
    let state_root: [u8; 33] = target_header.state_root.into();

    println!("current tip:   {current_tip}");
    println!("new tip:       {target}");
    println!("state_root:    {}", hex::encode(state_root));
    println!(
        "will delete:   heights {}..={} ({} blocks)",
        target + 1,
        current_tip,
        current_tip - target
    );

    // Close the read-only handle before opening for writes.
    drop(modifiers);

    // 1. Roll back state.redb to the target's AVL digest. This uses the
    //    undo log — if the target digest isn't in the retained version
    //    chain, this fails cleanly.
    print!("rolling back state.redb ... ");
    let mut storage = RedbAVLStorage::open(
        &state_path,
        AVLTreeParams {
            key_length: 32,
            value_length: None,
        },
        200,
        CacheSize::Bytes(64 * 1024 * 1024),
    )
    .context("opening state.redb")?;
    let avl_digest = Bytes::copy_from_slice(&state_root);
    storage
        .rollback(&avl_digest)
        .context("state rollback failed — target digest likely not in retention")?;
    drop(storage);
    println!("done");

    // 2. Truncate modifiers.redb: delete headers, sections, fork entries,
    //    and best-chain entries above target.
    print!("truncating modifiers.redb ... ");
    let modifiers = Database::open(&modifiers_path)?;
    let (headers_deleted, sections_deleted) =
        truncate_modifiers(&modifiers, target, current_tip)?;
    println!(
        "done ({headers_deleted} headers + forks, {sections_deleted} block sections)"
    );

    // 3. Optional: truncate indexer SQLite DB above target. Uses the same
    //    cascade as the indexer's internal rollback_to().
    if let Some(db_path) = indexer_db {
        print!("truncating {} ... ", db_path.display());
        truncate_indexer_db(&db_path, target)?;
        println!("done");
    }

    println!();
    println!("sharpen complete. node will resume from height {target} on next start.");
    Ok(())
}

/// Truncate the indexer's SQLite DB above `target`, matching the cascade
/// in `addons/indexer/src/db/sqlite.rs::rollback_to`. Shells out to sqlite3
/// so the main crate doesn't carry a rusqlite dep.
fn truncate_indexer_db(db_path: &std::path::Path, target: u32) -> Result<()> {
    let sql = format!(
        "BEGIN;\n\
         UPDATE boxes SET spent_tx_id = NULL, spent_height = NULL WHERE spent_height > {h};\n\
         DELETE FROM box_registers WHERE box_id IN (SELECT box_id FROM boxes WHERE height > {h});\n\
         DELETE FROM box_tokens WHERE box_id IN (SELECT box_id FROM boxes WHERE height > {h});\n\
         DELETE FROM boxes WHERE height > {h};\n\
         DELETE FROM tokens WHERE minting_height > {h};\n\
         DELETE FROM transactions WHERE height > {h};\n\
         DELETE FROM blocks WHERE height > {h};\n\
         INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('height', '{h}');\n\
         COMMIT;\n",
        h = target
    );
    let status = std::process::Command::new("sqlite3")
        .arg(db_path)
        .arg(&sql)
        .status()
        .context("spawning sqlite3 — is it installed and on PATH?")?;
    if !status.success() {
        bail!("sqlite3 exited with status: {status}");
    }
    Ok(())
}

/// Scan BEST_CHAIN for its last entry (the chain tip).
fn load_best_tip_height(db: &Database) -> Result<Option<u32>> {
    let read_txn = db.begin_read()?;
    let table = match read_txn.open_table(BEST_CHAIN) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let result = match table.last()? {
        Some((key_guard, _)) => Ok(Some(key_guard.value())),
        None => Ok(None),
    };
    result
}

/// Read and parse the header at a given height (from the best chain).
fn read_header(
    db: &Database,
    height: u32,
) -> Result<Option<ergo_chain_types::Header>> {
    let read_txn = db.begin_read()?;
    let best = read_txn.open_table(BEST_CHAIN)?;
    let id = match best.get(height)? {
        Some(g) => g.value(),
        None => return Ok(None),
    };
    let primary = read_txn.open_table(PRIMARY)?;
    let raw = primary
        .get((HEADER_TYPE_ID, id))?
        .context("header ID in BEST_CHAIN but not in PRIMARY")?
        .value()
        .to_vec();
    let header = ergo_chain_types::Header::scorex_parse_bytes(&raw)
        .context("header parse failed")?;
    Ok(Some(header))
}

/// Delete all data at heights > target. Returns (header-like entries
/// removed, block-section entries removed).
fn truncate_modifiers(
    db: &Database,
    target: u32,
    current_tip: u32,
) -> Result<(usize, usize)> {
    // First pass: collect everything we need to delete. Can't iterate
    // and delete in the same pass because redb tables are borrowed
    // mutably for deletion.
    let mut headers_to_delete: Vec<(u32, [u8; 32])> = Vec::new();
    let mut fork_headers_to_delete: Vec<(u32, u32, [u8; 32])> = Vec::new();
    let mut section_keys_to_delete: Vec<(u8, [u8; 32])> = Vec::new();

    {
        let read_txn = db.begin_read()?;
        let best = read_txn.open_table(BEST_CHAIN)?;
        let forks = match read_txn.open_table(HEADER_FORKS) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let primary = read_txn.open_table(PRIMARY)?;

        for h in (target + 1)..=current_tip {
            // Best-chain header at h.
            if let Some(g) = best.get(h)? {
                let id = g.value();
                headers_to_delete.push((h, id));

                // Derive the section IDs the header announces so we can
                // delete PRIMARY[(type_id, derived_id)] entries too.
                if let Some(raw) = primary.get((HEADER_TYPE_ID, id))? {
                    if let Ok(header) =
                        ergo_chain_types::Header::scorex_parse_bytes(raw.value())
                    {
                        for (type_id, derived_id) in
                            required_section_ids(&header)
                        {
                            section_keys_to_delete.push((type_id, derived_id));
                        }
                    }
                }
            }

            // Fork headers at h (any fork_num).
            if let Some(forks_tbl) = forks.as_ref() {
                for entry in forks_tbl.range((h, 0)..=(h, u32::MAX))? {
                    let (key_g, val_g) = entry?;
                    let (fh, fork_num) = key_g.value();
                    let id = val_g.value();
                    fork_headers_to_delete.push((fh, fork_num, id));
                }
            }
        }
    }

    let headers_deleted = headers_to_delete.len() + fork_headers_to_delete.len();
    let sections_deleted = section_keys_to_delete.len();

    // Second pass: perform all deletions in a single write transaction.
    let write_txn = db.begin_write()?;
    {
        let mut primary = write_txn.open_table(PRIMARY)?;
        let mut best = write_txn.open_table(BEST_CHAIN)?;
        let mut height_index = write_txn.open_table(HEIGHT_INDEX)?;
        let mut scores = write_txn.open_table(HEADER_SCORES)?;
        let mut forks = match write_txn.open_table(HEADER_FORKS) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(e.into()),
        };

        for (h, id) in &headers_to_delete {
            primary.remove((HEADER_TYPE_ID, *id))?;
            scores.remove(id)?;
            best.remove(*h)?;
            height_index.remove((HEADER_TYPE_ID, *h))?;
        }
        for (h, fork_num, id) in &fork_headers_to_delete {
            primary.remove((HEADER_TYPE_ID, *id))?;
            scores.remove(id)?;
            if let Some(t) = forks.as_mut() {
                t.remove((*h, *fork_num))?;
            }
        }
        for (type_id, derived_id) in &section_keys_to_delete {
            primary.remove((*type_id, *derived_id))?;
        }
        // Also wipe HEIGHT_INDEX entries for sections at heights > target.
        for type_id in [
            BLOCK_TRANSACTIONS_TYPE_ID,
            AD_PROOFS_TYPE_ID,
            EXTENSION_TYPE_ID,
        ] {
            for h in (target + 1)..=current_tip {
                height_index.remove((type_id, h))?;
            }
        }
    }
    write_txn.commit()?;

    Ok((headers_deleted, sections_deleted))
}

/// Compute the (type_id, id) keys for block sections of a given header.
/// Mirrors chain/src/section.rs::section_ids.
fn required_section_ids(
    header: &ergo_chain_types::Header,
) -> [(u8, [u8; 32]); 3] {
    [
        (
            BLOCK_TRANSACTIONS_TYPE_ID,
            prefixed_hash(
                BLOCK_TRANSACTIONS_TYPE_ID,
                &header.id.0 .0,
                &header.transaction_root.0,
            ),
        ),
        (
            AD_PROOFS_TYPE_ID,
            prefixed_hash(
                AD_PROOFS_TYPE_ID,
                &header.id.0 .0,
                &header.ad_proofs_root.0,
            ),
        ),
        (
            EXTENSION_TYPE_ID,
            prefixed_hash(
                EXTENSION_TYPE_ID,
                &header.id.0 .0,
                &header.extension_root.0,
            ),
        ),
    ]
}

/// `Blake2b256(prefix || data1 || data2)` — Scorex `Algos.hash.prefixedHash`.
fn prefixed_hash(
    prefix: u8,
    data1: &[u8; 32],
    data2: &[u8; 32],
) -> [u8; 32] {
    let mut buf = Vec::with_capacity(1 + 32 + 32);
    buf.push(prefix);
    buf.extend_from_slice(data1);
    buf.extend_from_slice(data2);
    ergo_chain_types::blake2b256_hash(&buf).0
}

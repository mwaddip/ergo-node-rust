//! Look up a modifier by (type_id, id_hex) directly in modifiers.redb.
//!
//! Bypasses the API layer's height-based lookup, so it surfaces fork
//! headers (and any other modifier present in PRIMARY but not on the
//! canonical chain).
//!
//! Usage:
//!   inspect-modifier <modifiers.redb path> <type_id> <id_hex>
//!
//! Common type_ids:
//!   2   Transaction
//!   101 Header
//!   102 BlockTransactions
//!   104 ADProofs
//!   108 Extension
//!
//! The node MUST be stopped — redb holds an exclusive lock.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

const PRIMARY: TableDefinition<(u8, [u8; 32]), &[u8]> =
    TableDefinition::new("primary");
const BEST_CHAIN: TableDefinition<u32, [u8; 32]> =
    TableDefinition::new("best_chain");
const HEADER_FORKS: TableDefinition<(u32, u32), [u8; 32]> =
    TableDefinition::new("header_forks");

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        bail!("usage: inspect-modifier <modifiers.redb path> <type_id> <id_hex>");
    }
    let path = PathBuf::from(&args[0]);
    let type_id: u8 = args[1].parse().context("type_id must be 0..=255")?;
    let id: [u8; 32] = hex::decode(&args[2])
        .context("id must be hex")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("id must be 32 bytes (64 hex chars)"))?;

    let db = Database::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let rtx = db.begin_read()?;

    // 1. Direct PRIMARY lookup.
    let primary = rtx.open_table(PRIMARY)?;
    let in_primary = primary.get((type_id, id))?;
    println!("modifiers.redb: {}", path.display());
    println!("  type_id: {type_id}");
    println!("  id:      {}", hex::encode(id));
    println!();
    match &in_primary {
        Some(g) => println!("PRIMARY[(type_id, id)] -> PRESENT, {} bytes", g.value().len()),
        None => println!("PRIMARY[(type_id, id)] -> ABSENT"),
    }

    // 2. If it's a header, also check canonical chain and forks.
    if type_id == 101 {
        let mut canonical_height: Option<u32> = None;
        let best = rtx.open_table(BEST_CHAIN)?;
        // BEST_CHAIN is height -> id; scan to find height for our id.
        // This is O(N) but the chain is at most ~2M entries — fine for forensics.
        for entry in best.iter()? {
            let (h_g, id_g) = entry?;
            if id_g.value() == id {
                canonical_height = Some(h_g.value());
                break;
            }
        }
        match canonical_height {
            Some(h) => println!("BEST_CHAIN: header is canonical at height {h}"),
            None => println!("BEST_CHAIN: header is NOT on the canonical chain"),
        }

        // HEADER_FORKS: (height, fork_num) -> id; scan for our id.
        let mut fork_hits: Vec<(u32, u32)> = Vec::new();
        let forks = match rtx.open_table(HEADER_FORKS) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(t) = forks {
            for entry in t.iter()? {
                let (k_g, v_g) = entry?;
                if v_g.value() == id {
                    fork_hits.push(k_g.value());
                }
            }
        }
        if fork_hits.is_empty() {
            println!("HEADER_FORKS: header is not recorded as a fork header");
        } else {
            for (h, fork_num) in &fork_hits {
                println!("HEADER_FORKS: header is fork header at height {h} (fork_num {fork_num})");
            }
        }
    }

    Ok(())
}

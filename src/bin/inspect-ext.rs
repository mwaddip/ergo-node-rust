//! One-off debugging tool: print the ID 124 extension field at given heights.

use std::path::PathBuf;

use anyhow::{Context, Result};
use enr_chain::{
    parse_extension_bytes, required_section_ids, StateType, EXTENSION_TYPE_ID, HEADER_TYPE_ID,
};
use redb::{Database, ReadableDatabase, TableDefinition};
use sigma_ser::ScorexSerializable;

const PRIMARY: TableDefinition<(u8, [u8; 32]), &[u8]> =
    TableDefinition::new("primary");
const BEST_CHAIN: TableDefinition<u32, [u8; 32]> =
    TableDefinition::new("best_chain");

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        anyhow::bail!("usage: inspect-ext <height> [<height>...]");
    }
    let data_dir = PathBuf::from("/var/lib/ergo-node/data");
    let db = Database::open(data_dir.join("modifiers.redb"))?;

    for arg in &args {
        let height: u32 = arg.parse()?;
        let rtx = db.begin_read()?;
        let best = rtx.open_table(BEST_CHAIN)?;
        let id = best.get(height)?.context("no block at height")?.value();
        let primary = rtx.open_table(PRIMARY)?;
        let raw_header = primary.get((HEADER_TYPE_ID, id))?
            .context("no header in primary")?.value().to_vec();
        let header = ergo_chain_types::Header::scorex_parse_bytes(&raw_header)?;
        let sections = required_section_ids(&header, StateType::Utxo);
        let (_, ext_id) = sections.iter().find(|(t, _)| *t == EXTENSION_TYPE_ID)
            .context("no extension section id")?;
        let ext_raw = primary.get((EXTENSION_TYPE_ID, *ext_id))?
            .context("extension not in primary")?.value().to_vec();
        let (_header_id, fields) = parse_extension_bytes(&ext_raw)?;

        println!("=== height {height} (header.version={}) ===", header.version);
        println!("  header_id: {}", hex::encode(id));
        println!("  ID 124 bytes:");
        let mut found = false;
        for (k, v) in &fields {
            if k[0] == 0x00 && (k[1] as i8) == 124 {
                println!("    key=[{:02x},{:02x}] len={} value={}",
                    k[0], k[1], v.len(), hex::encode(v));
                found = true;
            }
        }
        if !found {
            println!("    (absent)");
        }
    }
    Ok(())
}

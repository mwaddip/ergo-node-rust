//! Walk the AVL+ tree in state.redb from META_TOP_NODE_HASH toward a target
//! key, reporting each node label visited. If a referenced child digest is
//! missing from NODES_TABLE, report it — that's the layer-1 corruption
//! signature.
//!
//! Usage: inspect-state <state.redb path> <target key hex>

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

const META_TOP_NODE_HASH: &str = "top_node_hash";
const META_TOP_NODE_HEIGHT: &str = "top_node_height";
const META_CURRENT_VERSION: &str = "current_version";
const META_BLOCK_HEIGHT: &str = "block_height";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        bail!("usage: inspect-state <state.redb path> <target key hex | scan | check-digest <hex>>");
    }
    let path = PathBuf::from(&args[0]);
    let cmd = &args[1];

    if cmd == "scan" {
        return scan_tree(&path);
    }
    if cmd == "check-digest" {
        if args.len() < 3 { bail!("check-digest needs a 32-byte hex digest arg"); }
        return check_digest(&path, &args[2]);
    }

    let target_hex = cmd;
    let target: [u8; 32] = hex::decode(target_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("target key must be 32 bytes"))?;

    let db = Database::open(&path)?;
    let rtx = db.begin_read()?;

    let meta = rtx.open_table(META)?;
    let nodes = rtx.open_table(NODES)?;

    // Read root + metadata.
    let root_hash = meta
        .get(META_TOP_NODE_HASH)?
        .context("META_TOP_NODE_HASH missing")?
        .value()
        .to_vec();
    let top_height = meta
        .get(META_TOP_NODE_HEIGHT)?
        .map(|v| u32::from_be_bytes(v.value().try_into().unwrap_or([0; 4])));
    let current_version = meta
        .get(META_CURRENT_VERSION)?
        .map(|v| hex::encode(v.value()));
    let block_height = meta
        .get(META_BLOCK_HEIGHT)?
        .map(|v| u32::from_be_bytes(v.value().try_into().unwrap_or([0; 4])));

    println!("state.redb: {}", path.display());
    println!("  META_TOP_NODE_HASH:  {}", hex::encode(&root_hash));
    println!("  META_TOP_NODE_HEIGHT: {:?}", top_height);
    println!("  META_CURRENT_VERSION: {:?}", current_version);
    println!("  META_BLOCK_HEIGHT:    {:?}", block_height);
    println!("  target key:           {target_hex}");
    println!();

    // Count nodes (rough sanity check).
    let node_count = nodes.iter()?.count();
    println!("  NODES_TABLE entry count: {node_count}");
    println!();

    // Walk from root toward target key, printing each node.
    println!("Walk from root toward target:");
    let mut current_label: Vec<u8> = root_hash.clone();
    let mut depth = 0u32;
    loop {
        let packed_guard = match nodes.get(current_label.as_slice())? {
            Some(g) => g,
            None => {
                println!("  depth={depth} label={} -- MISSING FROM NODES_TABLE", hex::encode(&current_label));
                println!("  >>> this is the missing-node corruption signature <<<");
                return Ok(());
            }
        };
        let packed = packed_guard.value();
        if packed.is_empty() {
            println!("  depth={depth} label={} -- EMPTY PACKED BYTES", hex::encode(&current_label));
            return Ok(());
        }
        let node_type = packed[0];

        if node_type == 0x01 {
            // Leaf: type | key (32) | vlen u32 BE | value
            let key_end = 1 + 32;
            if packed.len() < key_end {
                println!("  depth={depth} label={} -- TRUNCATED LEAF", hex::encode(&current_label));
                return Ok(());
            }
            let leaf_key = &packed[1..key_end];
            println!("  depth={depth} type=Leaf label={}.. key={}",
                hex::encode(&current_label[..8]), hex::encode(leaf_key));
            if leaf_key == target.as_slice() {
                println!("  >>> target found at this leaf <<<");
            } else {
                println!("  >>> walk ended at non-matching leaf (target not in tree at this position) <<<");
            }
            return Ok(());
        }

        // Internal: 0x00 | balance(i8) | key(32) | left(32) | right(32)
        let key_off = 2;
        let left_off = key_off + 32;
        let right_off = left_off + 32;
        let end = right_off + 32;
        if packed.len() < end {
            println!("  depth={depth} label={} -- TRUNCATED INTERNAL", hex::encode(&current_label));
            return Ok(());
        }
        let balance = packed[1] as i8;
        let node_key = &packed[key_off..left_off];
        let left = &packed[left_off..right_off];
        let right = &packed[right_off..end];

        let direction = if target.as_slice() < node_key { "left" } else { "right" };
        println!("  depth={depth} type=Internal label={}.. balance={} key={}.. -> {} ({}..)",
            hex::encode(&current_label[..8]),
            balance,
            hex::encode(&node_key[..8]),
            direction,
            hex::encode(if direction == "left" { &left[..8] } else { &right[..8] }),
        );

        current_label = if direction == "left" { left.to_vec() } else { right.to_vec() };
        depth += 1;
    }
}

/// Check whether a specific digest is in NODES_TABLE.
fn check_digest(path: &std::path::Path, digest_hex: &str) -> Result<()> {
    let digest: [u8; 32] = hex::decode(digest_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("digest must be 32 bytes"))?;
    let db = Database::open(path)?;
    let rtx = db.begin_read()?;
    let nodes = rtx.open_table(NODES)?;
    match nodes.get(digest.as_slice())? {
        Some(g) => println!("PRESENT: {} ({} bytes)", digest_hex, g.value().len()),
        None => println!("MISSING: {}", digest_hex),
    }
    Ok(())
}

/// Walk the entire tree from META_TOP_NODE_HASH, reporting any reference
/// to a digest not in NODES_TABLE. Stops after first 100 missing for sanity.
fn scan_tree(path: &std::path::Path) -> Result<()> {
    let db = Database::open(path)?;
    let rtx = db.begin_read()?;
    let meta = rtx.open_table(META)?;
    let nodes = rtx.open_table(NODES)?;

    let root_hash = meta
        .get(META_TOP_NODE_HASH)?
        .context("META_TOP_NODE_HASH missing")?
        .value()
        .to_vec();

    println!("Full tree scan from root: {}", hex::encode(&root_hash));
    let total_in_table = nodes.iter()?.count();
    println!("NODES_TABLE entries: {total_in_table}");

    let mut visited: std::collections::HashSet<[u8; 32]> =
        std::collections::HashSet::with_capacity(total_in_table);
    let mut stack: Vec<(Vec<u8>, Vec<u8>, &'static str)> = Vec::new();
    let mut root_label = [0u8; 32];
    root_label.copy_from_slice(&root_hash);
    stack.push((root_hash.clone(), vec![], "root"));

    let mut visited_count = 0u64;
    let mut leaves = 0u64;
    let mut missing_count = 0u64;
    let max_missing_to_print = 100u64;

    while let Some((label, parent, side)) = stack.pop() {
        let arr: [u8; 32] = match label.as_slice().try_into() {
            Ok(a) => a,
            Err(_) => continue,
        };
        if !visited.insert(arr) {
            continue;
        }

        let packed_guard = match nodes.get(label.as_slice())? {
            Some(g) => g,
            None => {
                missing_count += 1;
                if missing_count <= max_missing_to_print {
                    println!("MISSING ref={} from parent={} side={}",
                        hex::encode(&label),
                        if parent.is_empty() { "META_TOP_NODE_HASH".to_string() } else { hex::encode(&parent) },
                        side);
                }
                continue;
            }
        };
        let packed = packed_guard.value();
        if packed.is_empty() { continue; }

        visited_count += 1;
        let node_type = packed[0];
        if node_type == 0x01 {
            leaves += 1;
            continue;
        }

        // Internal: 0x00 | balance | key(32) | left(32) | right(32)
        let key_off = 2;
        let left_off = key_off + 32;
        let right_off = left_off + 32;
        let end = right_off + 32;
        if packed.len() < end { continue; }
        let left = packed[left_off..right_off].to_vec();
        let right = packed[right_off..end].to_vec();
        let parent_label = label.clone();
        stack.push((right, parent_label.clone(), "right"));
        stack.push((left, parent_label, "left"));

        if visited_count.is_multiple_of(500_000) {
            println!("  ... visited {visited_count} nodes (leaves={leaves}, missing={missing_count})");
        }
    }

    println!();
    println!("Scan done.");
    println!("  Reachable nodes:    {visited_count}");
    println!("  Leaves:             {leaves}");
    println!("  Internals:          {}", visited_count - leaves);
    println!("  Missing references: {missing_count}");
    println!("  Orphan nodes (storage but unreachable): {}", total_in_table as i64 - visited_count as i64);

    Ok(())
}

use std::path::PathBuf;

use enr_chain::{parse_header, HEADER_TYPE_ID};
use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage};
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::AVLTree;
use ergo_avltree_rust::persistent_batch_avl_prover::PersistentBatchAVLProver;
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

const NODES_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
const UNDO_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("undo");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: state_inspect <path-to-state.redb>");
        std::process::exit(1);
    }
    let path = PathBuf::from(&args[1]);
    println!("Inspecting: {}", path.display());

    println!("\n=== raw redb metadata (fresh read txn) ===");
    {
        let db = Database::builder().open(&path).expect("open failed");
        let read = db.begin_read().expect("begin_read");

        match read.open_table(META_TABLE) {
            Ok(meta) => {
                for key in &[
                    "top_node_hash",
                    "top_node_height",
                    "current_version",
                    "lsn",
                    "versions",
                ] {
                    match meta.get(*key).unwrap() {
                        Some(v) => {
                            let bytes = v.value();
                            println!(
                                "  {} = {} ({} bytes)",
                                key,
                                hex(bytes),
                                bytes.len()
                            );
                        }
                        None => println!("  {} = <missing>", key),
                    }
                }
            }
            Err(e) => println!("  META_TABLE open failed: {e}"),
        }

        match read.open_table(NODES_TABLE) {
            Ok(nodes) => match nodes.len() {
                Ok(n) => println!("  NODES_TABLE entries: {}", n),
                Err(e) => println!("  NODES_TABLE len failed: {e}"),
            },
            Err(e) => println!("  NODES_TABLE open failed: {e}"),
        }

        match read.open_table(UNDO_TABLE) {
            Ok(undo) => {
                match undo.len() {
                    Ok(n) => println!("  UNDO_TABLE entries: {}", n),
                    Err(e) => println!("  UNDO_TABLE len failed: {e}"),
                }
                println!("  UNDO_TABLE first 10 LSNs:");
                match undo.iter() {
                    Ok(iter) => {
                        for (i, item) in iter.enumerate() {
                            if i >= 10 {
                                break;
                            }
                            if let Ok((k, v)) = item {
                                println!(
                                    "    lsn={}, undo_bytes={}",
                                    k.value(),
                                    v.value().len()
                                );
                            }
                        }
                    }
                    Err(e) => println!("    iter failed: {e}"),
                }
            }
            Err(e) => println!("  UNDO_TABLE open failed: {e}"),
        }
    }

    println!("\n=== via RedbAVLStorage::open ===");
    let params = AVLTreeParams {
        key_length: 32,
        value_length: None,
    };
    let storage = RedbAVLStorage::open(
        &path,
        params,
        200,
        CacheSize::Bytes(256 * 1024 * 1024),
    )
    .expect("open storage");

    match storage.version() {
        Some(v) => println!(
            "  storage.version() = {} ({} bytes)",
            hex(&v),
            v.len()
        ),
        None => println!("  storage.version() = None"),
    }

    match storage.root_state() {
        Some((hash, height)) => {
            println!("  storage.root_state() = (hash={}, height={})", hex(&hash), height)
        }
        None => println!("  storage.root_state() = None"),
    }

    let rollback_versions: Vec<_> = storage
        .rollback_versions()
        .collect();
    println!("  rollback_versions (chain w/o current): {}", rollback_versions.len());
    for (i, d) in rollback_versions.iter().take(5).enumerate() {
        println!("    [{}] {}", i, hex(d));
    }

    println!("\n=== PersistentBatchAVLProver digest ===");
    let resolver = storage.resolver();
    let tree = AVLTree::new(resolver, 32, None);
    let prover = BatchAVLProver::new(tree, true);
    let persistent_prover = PersistentBatchAVLProver::new(prover, Box::new(storage), vec![])
        .expect("construct persistent prover");

    let d = persistent_prover.digest();
    println!("  prover.digest() = {} ({} bytes)", hex(d.as_ref()), d.len());
    let prover_digest = d.to_vec();

    if args.len() >= 3 {
        let modifiers_path = PathBuf::from(&args[2]);
        println!("\n=== cross-check vs modifiers.redb ({}) ===", modifiers_path.display());
        let store = RedbModifierStore::new(&modifiers_path).expect("open modifiers");

        match store.best_header_tip() {
            Ok(Some((tip_h, _))) => println!("  best_header_tip height = {}", tip_h),
            Ok(None) => println!("  best_header_tip = None"),
            Err(e) => println!("  best_header_tip error: {e}"),
        }

        let target_height: u32 = if args.len() >= 4 {
            args[3].parse().unwrap_or(1_307_879)
        } else {
            1_307_879
        };

        println!("\n  Checking header at target height {}:", target_height);
        match store.best_header_at(target_height) {
            Ok(Some(id)) => {
                println!("    header_id = {}", hex(&id));
                match store.get(HEADER_TYPE_ID, &id) {
                    Ok(Some(bytes)) => match parse_header(&bytes) {
                        Ok(header) => {
                            let root_bytes: [u8; 33] = header.state_root.into();
                            let matches = root_bytes.as_slice() == prover_digest.as_slice();
                            println!("    state_root = {}", hex(&root_bytes));
                            println!("    matches prover.digest(): {}", matches);
                        }
                        Err(e) => println!("    parse_header failed: {e}"),
                    },
                    Ok(None) => println!("    header bytes not found"),
                    Err(e) => println!("    store.get error: {e}"),
                }
            }
            Ok(None) => println!("    no header at height {}", target_height),
            Err(e) => println!("    best_header_at error: {e}"),
        }

        // Scan backwards from target_height + 50 down to target_height - 100 to find
        // any header whose state_root matches prover.digest().
        let scan_top = target_height.saturating_add(50);
        let scan_bottom = target_height.saturating_sub(100);
        println!("\n  Scanning headers [{}..{}] for state_root matching prover.digest():", scan_bottom, scan_top);
        let mut found_any = false;
        for h in (scan_bottom..=scan_top).rev() {
            if let Ok(Some(id)) = store.best_header_at(h) {
                if let Ok(Some(bytes)) = store.get(HEADER_TYPE_ID, &id) {
                    if let Ok(header) = parse_header(&bytes) {
                        let root_bytes: [u8; 33] = header.state_root.into();
                        if root_bytes.as_slice() == prover_digest.as_slice() {
                            println!("    MATCH at height {}: state_root = {}", h, hex(&root_bytes));
                            found_any = true;
                        }
                    }
                }
            }
        }
        if !found_any {
            println!("    no match in scan range");
        }
    }
}

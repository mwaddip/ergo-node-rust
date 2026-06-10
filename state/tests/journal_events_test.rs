//! Verifies that emissions inside `enr-state` honour the
//! `facts/journal-events.md` v1.0 contract. The Doctor adapter parses
//! the default human-readable subscriber output, so the marker prefix
//! and named-field shape are the load-bearing contract — not just the
//! event name.
//!
//! Only `state_storage_open_complete` lives in this crate. The other
//! state-section events (`state_storage_open_started`,
//! `utxo_bootstrap_initiated`, `utxo_snapshot_stored`) are emitted by
//! the orchestration layer in `src/main.rs` per `facts/state.md`
//! (bootstrap is explicitly NOT owned here).
//!
//! Uses the `tracing-test` crate's `#[traced_test]` macro rather than
//! a thread-local subscriber. A hand-rolled `with_default` capture
//! works in isolation but loses emits from outside the test closure
//! when sibling tests in the same binary run in parallel — the
//! cross-thread emit doesn't see the subscriber. `#[traced_test]`
//! installs a per-test global subscriber and matches emits regardless
//! of which thread they originated on. The `no-env-filter` feature
//! is mandatory: without it, the macro hard-codes
//! `EnvFilter = "<test_crate>=trace"`, which silently filters out
//! emits whose target is `enr_state`.

use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage};
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::AVLTree;
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use tempfile::tempdir;
use tracing_test::traced_test;

const KEY_LEN: usize = 32;

fn params() -> AVLTreeParams {
    AVLTreeParams {
        key_length: KEY_LEN,
        value_length: None,
    }
}

fn make_key(seed: u8) -> bytes::Bytes {
    let mut key = vec![0u8; KEY_LEN];
    key[0] = 0x01;
    key[1] = seed;
    bytes::Bytes::from(key)
}

#[test]
#[traced_test]
fn open_on_empty_path_emits_state_storage_open_complete() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let _storage = RedbAVLStorage::open(&path, params(), 0, CacheSize::default()).unwrap();

    // Marker prefix is the journal-events parse anchor.
    assert!(
        logs_contain("UTXO state storage opened"),
        "missing journal-events marker `UTXO state storage opened`"
    );
    // Empty storage → digest field rendered as the literal "none".
    // The contract permits `digest` (string) as an optional field.
    assert!(
        logs_contain("digest=\"none\"") || logs_contain("digest=none"),
        "digest field should render `none` on empty storage"
    );
    // Level is not asserted via logs_contain — `info!` is structurally
    // INFO at the macro site, and tracing-test's subscriber doesn't
    // include the level word in `logs_contain`'s scan in a stable way.
}

#[test]
#[traced_test]
fn reopen_with_committed_state_emits_digest_hex() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");

    // First open + commit + drop populates the redb file. This open
    // ALSO emits a `state_storage_open_complete` event with
    // `digest=none` (empty storage at that point), so the second open's
    // emission below has to be distinguished from it by content, not
    // by presence/absence. `logs_assert` scans the line set.
    {
        let mut storage =
            RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
        let mut prover = BatchAVLProver::new(tree, true);
        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(1),
                value: bytes::Bytes::from(vec![0xAA; 32]),
            }))
            .unwrap();
        storage.update(&mut prover, vec![]).unwrap();
        storage.flush().unwrap();
    }

    let _storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();

    assert!(
        logs_contain("UTXO state storage opened"),
        "missing journal-events marker `UTXO state storage opened`"
    );

    // Among all the open-complete emissions, at least one must render
    // `digest=` with a hex prefix (the reopen after the populated
    // commit). The first emission has `digest=none`; the second has
    // the AVL+ ADDigest as 66 hex chars.
    logs_assert(|lines: &[&str]| {
        let opens: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("UTXO state storage opened"))
            .collect();
        if opens.len() < 2 {
            return Err(format!(
                "expected ≥2 open-complete emissions (setup + reopen), got {}",
                opens.len()
            ));
        }
        let has_hex_digest = opens.iter().any(|l| {
            // digest is rendered unquoted by the default fmt subscriber:
            // `digest=a3ed750c...`. Scan the run of chars after `digest=`.
            l.split("digest=")
                .nth(1)
                .map(|after| {
                    let hex_run: String = after
                        .chars()
                        .take_while(|c| c.is_ascii_hexdigit())
                        .collect();
                    hex_run.len() >= 64
                })
                .unwrap_or(false)
        });
        if !has_hex_digest {
            return Err(format!(
                "no open-complete emission had a ≥64-char hex digest after reopen: {opens:?}"
            ));
        }
        Ok(())
    });
}

//! Journal-event conformance for the sync crate — `facts/journal-events.md`.
//!
//! The Doctor adapter and other downstream consumers parse on the marker
//! prefixes and field names this crate emits; drift here is silent breakage
//! over there.
//!
//! ## Why this file is an index and not a test suite
//!
//! Every one of this crate's contract events is emitted from a private method
//! (`HeaderSync::advance_script_frontier`, `::await_eval_capacity`,
//! `::handle_eval_failure`, the sweep loop) or a private module
//! (`sweep_backoff`, `eval_backlog`), and setting one up means writing the
//! struct's private watermark fields. None of that is reachable from an
//! integration test, which sees only `ergo_sync`'s public API.
//!
//! This file used to paper over that with **inline mirrors**: each test called
//! `info!(…)` with its own copy of the marker and fields, then asserted the
//! captured output contained that same copy. That tests `tracing`'s formatter
//! against itself. It cannot fail when an emit site drifts, because it never
//! invokes one — which is precisely how `deferred_eval_gate_engaged` and
//! `eval_frontier_hole` both shipped not matching the contract under a green
//! suite on 2026-08-12, and how the `validation_rollback_failed` mirror came to
//! assert an `error=` value the real emit never produced.
//!
//! The conformance tests therefore live beside the harness that can drive the
//! real emit, and this file records where. Each entry below names the test that
//! renders that event from its actual emit site:
//!
//! | Contract event | Driven by |
//! |---|---|
//! | `validation_sweep_started` | `state::sweep_resume_tests::journal_sweep_and_block_applied_conform` |
//! | `validation_sweep_complete` | ditto |
//! | `block_applied` | ditto |
//! | `chain_tip_reached` | `state::sweep_resume_tests::journal_chain_tip_reached_conforms` |
//! | `validation_stuck` | `sweep_backoff::tests::validation_stuck_fires_on_fifth_*_stall` |
//! | `validation_rollback_failed` | `state::sweep_resume_tests::journal_validation_rollback_failed_conforms` |
//! | `deferred_eval_backlog` | `eval_backlog::tests::*` (marker, every field, both probe branches) |
//! | `deferred_eval_gate_engaged` | `state::sweep_resume_tests::journal_deferred_eval_gate_engaged_conforms`, `::journal_gate_engaged_names_which_bound_tripped` |
//! | `eval_frontier_hole` | `state::sweep_resume_tests::journal_eval_frontier_hole_conforms`, `::journal_eval_frontier_hole_reports_the_validator_tip_not_the_stale_cache` |
//!
//! What remains here is what an integration test *can* exercise for real: the
//! public classifier that produces `validation_stuck`'s `error_kind` and
//! `missing_key` field values.

use bytes::Bytes;
use ergo_sync::apply_state_error::classify_apply_state_error;

/// A realistic 32-byte AVL key with the three byte categories the
/// `bytes::Bytes` Debug impl renders differently: printable ASCII
/// (literal), special escapes (`\r`, `\0`), and `\xHH` escapes for
/// the rest. Exercises the parser against the actual format the
/// AVL prover produces.
const AVL_KEY: [u8; 32] = [
    0x96, 0x5f, 0x0d, 0x67, 0xa4, 0x12, 0xff, 0x00, 0x61, 0x62, 0x63, 0xde, 0xad, 0xbe, 0xef, 0x42,
    0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0, 0xd0, 0xe0, 0xf0, 0xff,
];

#[test]
fn classify_extracts_hex_from_avl_missing_key_error() {
    let key = Bytes::copy_from_slice(&AVL_KEY);
    let err = format!("Key {key:?} does not exists");
    let (kind, hex) = classify_apply_state_error(&err);
    assert_eq!(kind, "missing_key");
    let expected: String = AVL_KEY.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex.as_deref(),
        Some(expected.as_str()),
        "expected hex {expected} from error {err:?}, got {hex:?}"
    );
}

#[test]
fn classify_other_for_non_missing_key_error() {
    let (kind, hex) = classify_apply_state_error("validator state root mismatch");
    assert_eq!(kind, "other");
    assert!(hex.is_none());
}

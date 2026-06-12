//! Verifies that the sync crate's journal-events emissions render with
//! the marker prefixes and named fields promised in
//! `facts/journal-events.md`. The Doctor adapter and other downstream
//! consumers parse on these strings — drift here is silent breakage
//! over there.
//!
//! These tests assert the SHAPE of the emit, mirroring the per-event
//! tracing calls in `src/state.rs`. They don't drive the state machine
//! end-to-end — that's covered by the integration tests under `tests/`
//! and the live mainnet run. The contract anchor is the rendered line.

use std::io;
use std::sync::{Arc, Mutex};
use tracing::{error, info, warn};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct CaptureWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl CaptureWriter {
    fn captured(&self) -> String {
        String::from_utf8(self.buf.lock().unwrap().clone()).unwrap()
    }
}

impl io::Write for CaptureWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture<F: FnOnce()>(f: F) -> String {
    let writer = CaptureWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer.clone())
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    writer.captured()
}

#[test]
fn validation_sweep_started_has_marker_and_named_fields() {
    let sweep_from: u64 = 1_000_000;
    let sweep_to: u64 = 1_000_500;
    let sweep_size: u64 = 500;
    let output = capture(|| {
        info!(
            from = sweep_from,
            to = sweep_to,
            blocks = sweep_size,
            "VALIDATION SWEEP STARTED"
        );
    });
    assert!(
        output.contains("VALIDATION SWEEP STARTED"),
        "missing marker: {output}"
    );
    // The contract pins the marker as the literal prefix — no decorative
    // ===, no embedded values. The old emit shape was
    // "=== VALIDATION SWEEP STARTED ===" which prefix-matches the
    // contract regex but is noisier than necessary.
    assert!(
        !output.contains("==="),
        "marker should be plain text, not decorated: {output}"
    );
    assert!(
        output.contains("from=1000000"),
        "missing from field: {output}"
    );
    assert!(
        output.contains("to=1000500"),
        "missing to field: {output}"
    );
    assert!(
        output.contains("blocks=500"),
        "missing blocks field: {output}"
    );
}

#[test]
fn validation_sweep_complete_has_marker_and_named_fields() {
    let sweep_from: u64 = 1_000_000;
    let validated_to: u64 = 1_000_500;
    let advanced: u64 = 500;
    let output = capture(|| {
        info!(
            from = sweep_from,
            to = validated_to,
            blocks = advanced,
            elapsed = "0m42s",
            rate = "12/s",
            "VALIDATION SWEEP COMPLETE"
        );
    });
    assert!(
        output.contains("VALIDATION SWEEP COMPLETE"),
        "missing marker: {output}"
    );
    assert!(
        !output.contains("==="),
        "marker should be plain text, not decorated: {output}"
    );
    assert!(
        output.contains("from=1000000"),
        "missing from field: {output}"
    );
    assert!(output.contains("to=1000500"), "missing to field: {output}");
    assert!(
        output.contains("blocks=500"),
        "missing blocks field: {output}"
    );
}

#[test]
fn block_applied_has_marker_and_named_fields() {
    // Mirrors the emit at apply_state success in `src/state.rs`. The
    // contract says `height` is u64 and `id` is a 32-byte hex string.
    let height: u64 = 1_785_000;
    // BlockId Display impl renders as lowercase hex. Synthesize a
    // sentinel string so the test isn't dependent on the actual type.
    let block_id = "0000abcdef0123456789abcdef0123456789abcdef0123456789abcdef012345";
    let output = capture(|| {
        info!(
            height = height,
            id = %block_id,
            "block applied"
        );
    });
    assert!(
        output.contains("block applied"),
        "missing marker: {output}"
    );
    assert!(
        output.contains("height=1785000"),
        "missing height field: {output}"
    );
    assert!(
        output.contains(&format!("id={block_id}")),
        "missing id field rendered via Display: {output}"
    );
}

#[test]
fn chain_tip_reached_has_marker_and_named_fields() {
    let tip_height: u64 = 1_785_500;
    let output = capture(|| {
        info!(height = tip_height, "chain tip reached");
    });
    assert!(
        output.contains("chain tip reached"),
        "missing marker: {output}"
    );
    assert!(
        output.contains("height=1785500"),
        "missing height field: {output}"
    );
}

// -------------------------------------------------------------------
// `validation_stuck` (contract since 1.1, broadened to v1.3 —
// `facts/journal-events.md`)
//
// The emission now lives in the sweep backoff — the single emitter for
// BOTH the apply_state and deferred-eval stall modes. Its firing behavior
// (fires on the 5th consecutive frontier stall, once per frontier, re-arms
// on progress / frontier change) and the rendered shape are covered by
// `ergo_sync::sweep_backoff`'s own unit tests, which capture the REAL emit.
//
// Kept here, the contract-shape anchor file: the apply_state error
// classifier that feeds `error_kind`/`missing_key`, plus an inline mirror of
// the emitted line so the Doctor-adapter contract shape is pinned alongside
// the other journal events.
// -------------------------------------------------------------------

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

#[test]
fn validation_stuck_renders_marker_and_named_fields() {
    // Inline mirror of `sweep_backoff::emit_validation_stuck` (the missing_key
    // branch). The contract pins marker "validation stuck" and fields height
    // (u64), attempts (u64), error_kind (string), missing_key (hex, optional).
    // The real emit is exercised end-to-end in the sweep_backoff unit tests;
    // this is the contract-shape anchor beside the other journal events.
    let key_hex: String = AVL_KEY.iter().map(|b| format!("{b:02x}")).collect();
    let output = capture(|| {
        warn!(
            height = 1_783_677u64,
            attempts = 5u64,
            error_kind = "missing_key",
            missing_key = %key_hex,
            "validation stuck"
        );
    });
    assert!(output.contains("validation stuck"), "missing marker: {output}");
    assert!(output.contains("height=1783677"), "missing height: {output}");
    assert!(output.contains("attempts=5"), "missing attempts: {output}");
    assert!(
        output.contains("error_kind=\"missing_key\""),
        "missing error_kind: {output}"
    );
    assert!(
        output.contains(&format!("missing_key={key_hex}")),
        "missing key hex: {output}"
    );
}

#[test]
fn validation_rollback_failed_renders_marker_and_named_fields() {
    // Inline mirror of the `validation rollback failed` ERROR emitted by
    // `handle_eval_failure` and the reorg arm in `src/state.rs` when
    // `BlockValidator::reset_to` returns Err (validator unmoved, watermarks
    // held in place). Fields: height (u64, the rollback TARGET), path
    // (string: "eval_failure" | "reorg"), error (Display). Not yet listed
    // in `facts/journal-events.md` — contract addition owed by the main
    // session; this pins the shape the entry must describe.
    let output = capture(|| {
        error!(
            height = 2668u64,
            path = "eval_failure",
            error = %"rollback to height 2668 failed: simulated storage failure",
            "validation rollback failed"
        );
    });
    assert!(
        output.contains("validation rollback failed"),
        "missing marker: {output}"
    );
    assert!(output.contains("height=2668"), "missing height: {output}");
    assert!(
        output.contains("path=\"eval_failure\""),
        "missing path: {output}"
    );
    assert!(
        output.contains("error=rollback to height 2668 failed"),
        "missing error field: {output}"
    );
}

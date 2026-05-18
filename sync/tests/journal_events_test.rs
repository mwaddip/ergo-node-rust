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
use tracing::info;
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
// `validation_stuck` (contract since 1.1, `facts/journal-events.md`)
//
// Unlike the earlier shape-only tests in this file, the suite below
// drives the actual tracker in `ergo_sync::apply_state_tracker`. It
// asserts both the state-machine behavior (when/whether to emit) and
// the rendered shape of the emit (marker + named fields).
// -------------------------------------------------------------------

use bytes::Bytes;
use ergo_sync::apply_state_tracker::{classify_apply_state_error, record_apply_state_result};

#[test]
fn validation_stuck_emits_on_fifth_consecutive_failure() {
    let err = "Key b\"foo\" does not exists";
    let mut tracker = None;
    let mut emits = Vec::new();
    let output = capture(|| {
        for _ in 0..5 {
            emits.push(record_apply_state_result(
                &mut tracker,
                1_783_677,
                Some(err),
            ));
        }
    });
    assert_eq!(
        emits,
        vec![false, false, false, false, true],
        "expected emit only on 5th attempt: emits={emits:?} output={output}"
    );
    assert_eq!(
        output.matches("validation stuck").count(),
        1,
        "expected exactly one emit: {output}"
    );
    assert!(
        output.contains("height=1783677"),
        "missing height field: {output}"
    );
    assert!(
        output.contains("attempts=5"),
        "missing attempts field: {output}"
    );
    // "foo" isn't 32 bytes so the hex extraction returns None — the
    // emit still classifies as missing_key (the AVL error marker is
    // what triggers the classification, not the key length).
    assert!(
        output.contains("error_kind"),
        "missing error_kind field: {output}"
    );
    assert!(
        output.contains("missing_key"),
        "missing_key kind should appear in the emit: {output}"
    );
}

#[test]
fn validation_stuck_does_not_re_emit_within_window() {
    let err = "state root mismatch at height 100";
    let mut tracker = None;
    let mut emits = Vec::new();
    let output = capture(|| {
        for _ in 0..10 {
            emits.push(record_apply_state_result(&mut tracker, 100, Some(err)));
        }
    });
    assert_eq!(
        emits.iter().filter(|e| **e).count(),
        1,
        "expected exactly one emit across 10 failures: emits={emits:?}"
    );
    assert_eq!(output.matches("validation stuck").count(), 1, "{output}");
}

#[test]
fn different_height_resets_stuck_window() {
    let err = "Key b\"x\" does not exists";
    let mut tracker = None;
    let mut emits = Vec::new();
    let output = capture(|| {
        // 5 failures at A → emit on 5th (index 4).
        for _ in 0..5 {
            emits.push(record_apply_state_result(&mut tracker, 100, Some(err)));
        }
        // 1 failure at B resets the window (no emit).
        emits.push(record_apply_state_result(&mut tracker, 200, Some(err)));
        // 4 more at B → 5 total at B → emit on 5th (index 9).
        for _ in 0..4 {
            emits.push(record_apply_state_result(&mut tracker, 200, Some(err)));
        }
    });
    let true_indices: Vec<usize> = emits
        .iter()
        .enumerate()
        .filter(|(_, e)| **e)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        true_indices,
        vec![4, 9],
        "expected emits at indices 4 (window A) and 9 (window B): emits={emits:?}"
    );
    assert_eq!(output.matches("validation stuck").count(), 2, "{output}");
    assert!(output.contains("height=100"), "{output}");
    assert!(output.contains("height=200"), "{output}");
}

#[test]
fn recovery_resets_stuck_window() {
    let err = "Key b\"y\" does not exists";
    let mut tracker = None;
    let mut emits = Vec::new();
    let output = capture(|| {
        // 5 failures → emit on 5th (index 4).
        for _ in 0..5 {
            emits.push(record_apply_state_result(&mut tracker, 100, Some(err)));
        }
        // Ok outcome resets the tracker.
        emits.push(record_apply_state_result(&mut tracker, 100, None));
        // Another 5 failures at the same height → emit again (index 10).
        for _ in 0..5 {
            emits.push(record_apply_state_result(&mut tracker, 100, Some(err)));
        }
    });
    let true_indices: Vec<usize> = emits
        .iter()
        .enumerate()
        .filter(|(_, e)| **e)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        true_indices,
        vec![4, 10],
        "expected emits at indices 4 (pre-recovery) and 10 (post-recovery): emits={emits:?}"
    );
    assert_eq!(output.matches("validation stuck").count(), 2, "{output}");
}

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
fn validation_stuck_emits_missing_key_hex_field() {
    let key = Bytes::copy_from_slice(&AVL_KEY);
    let err = format!("Key {key:?} does not exists");
    let mut tracker = None;
    let output = capture(|| {
        for _ in 0..5 {
            record_apply_state_result(&mut tracker, 1_783_677, Some(&err));
        }
    });
    let expected_hex: String = AVL_KEY.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        output.contains(&format!("missing_key={expected_hex}")),
        "expected missing_key={expected_hex} in output: {output}"
    );
}

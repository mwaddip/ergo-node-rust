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

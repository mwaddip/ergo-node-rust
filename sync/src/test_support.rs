//! Test-only tracing capture.
//!
//! Journal-event conformance tests (`../facts/journal-events.md`) assert on the
//! *rendered* line, which means every one of them needs a subscriber writing
//! into a buffer. Four near-identical copies of that scaffolding had grown
//! across `state.rs`, `sweep_backoff.rs`, `eval_backlog.rs` (since deleted) and
//! the integration test before this module existed.
//!
//! Duplication is not the only reason to have one copy. The max level lives
//! here, and it is the thing that silently decides whether an event is
//! capturable at all: `tracing_subscriber::fmt()` defaults to INFO, so an event
//! emitted below that renders to nothing under a copy that took the default,
//! and looks exactly like an event that was never emitted. That is not
//! hypothetical — it is how `deferred_eval_gate_engaged` (DEBUG, removed in
//! v0.8.0 with the queue it described) shipped unverified. Callers name the
//! level explicitly.

use std::io;
use std::sync::{Arc, Mutex};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
pub(crate) struct CaptureWriter {
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

/// The subscriber every capture below installs: no time, no ANSI, no target,
/// so an assertion sees `marker field=value` and nothing else.
fn subscriber(writer: CaptureWriter, max: LevelFilter) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_writer(writer)
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .with_max_level(max)
        .finish()
}

/// Capture everything at or above `max` that `f` emits on this thread.
pub(crate) fn capture_at<F: FnOnce()>(max: LevelFilter, f: F) -> String {
    let writer = CaptureWriter::default();
    tracing::subscriber::with_default(subscriber(writer.clone(), max), f);
    writer.captured()
}

/// [`capture_at`] at INFO, the level of most contract events.
pub(crate) fn capture<F: FnOnce()>(f: F) -> String {
    capture_at(LevelFilter::INFO, f)
}

/// Async [`capture_at`].
///
/// Takes the future directly rather than a closure so the caller can pass
/// `sync.some_method()` — a future borrowing `&mut sync` — without wrapping it
/// in `move || async move {…}` to satisfy capture rules. The subscriber guard
/// outlives the `await`, so emits from the future are captured. Valid only on a
/// current-thread runtime (`#[tokio::test]`'s default): the dispatcher is a
/// thread-local, and a future migrating to another worker would leave it
/// behind.
///
/// The future's own output is discarded — the rendered line is what a
/// journal-event test is asserting on. A test that needs both wraps the call
/// itself in an `async { … }` block and keeps the value there.
pub(crate) async fn capture_async<Fut>(max: LevelFilter, f: Fut) -> String
where
    Fut: std::future::Future,
{
    let writer = CaptureWriter::default();
    let guard = tracing::subscriber::set_default(subscriber(writer.clone(), max));
    let _ = f.await;
    drop(guard);
    writer.captured()
}

/// [`capture_async`] at WARN.
pub(crate) async fn capture_warn<Fut>(f: Fut) -> String
where
    Fut: std::future::Future,
{
    capture_async(LevelFilter::WARN, f).await
}

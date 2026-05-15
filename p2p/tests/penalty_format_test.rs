//! Verifies that `peer_penalised` PENALTY emissions render with the
//! marker prefix and named fields promised in `facts/journal-events.md`.
//!
//! Tracing's default formatter is the canonical rendering — that's what
//! fail2ban regex consumes and what the journal-events contract pins
//! down. If this test stops passing, fail2ban almost certainly stops
//! banning.

use std::io;
use std::sync::{Arc, Mutex};
use tracing::warn;
use tracing_subscriber::fmt::MakeWriter;

/// Captures the formatted output of a single tracing event into an
/// in-memory buffer. Cloned per-event by the fmt subscriber via
/// [`MakeWriter::make_writer`].
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

/// Install the capture subscriber for the duration of `f` and return
/// the formatted output.
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
fn penalty_emit_has_marker_and_named_fields() {
    // Mirrors `transport::frame::read_frame`'s bad-magic emit, which
    // is the canonical permanent-ban shape that fail2ban parses.
    let addr: std::net::IpAddr = "192.0.2.1".parse().unwrap();
    let output = capture(|| {
        warn!(
            peer = %addr,
            kind = "bad_magic",
            "PENALTY"
        );
    });
    // Marker is the literal "PENALTY" — no trailing colon, no embedded
    // reason. fail2ban depends on this being the leading word.
    assert!(
        output.contains("PENALTY"),
        "missing PENALTY marker: {output}"
    );
    // `peer = %addr` uses Display, so the IP renders unquoted —
    // exactly what fail2ban's `<HOST>` macro expects.
    assert!(
        output.contains("peer=192.0.2.1"),
        "peer field should render unquoted via Display: {output}"
    );
    // `kind = "literal"` is a string literal, rendered with quotes.
    assert!(
        output.contains("kind=\"bad_magic\""),
        "kind field should render quoted: {output}"
    );
    // Marker precedes the field block. Operators (and fail2ban) match
    // on `PENALTY peer=...`, so the order matters.
    let marker_idx = output.find("PENALTY").unwrap();
    let peer_idx = output.find("peer=").unwrap();
    assert!(
        marker_idx < peer_idx,
        "PENALTY marker must precede peer field: {output}"
    );
}

#[test]
fn penalty_emit_with_detail_includes_reason() {
    // Mirrors `node::run_peer`'s message-parse-failed emit (misbehavior
    // kind — logged but not banned).
    let addr: std::net::IpAddr = "192.0.2.42".parse().unwrap();
    let err = std::io::Error::new(std::io::ErrorKind::InvalidData, "bad VLQ");
    let output = capture(|| {
        warn!(
            peer = %addr,
            kind = "message_parse_failed",
            detail = %err,
            "PENALTY"
        );
    });
    assert!(output.contains("PENALTY"), "missing marker: {output}");
    assert!(
        output.contains("kind=\"message_parse_failed\""),
        "missing quoted kind: {output}"
    );
    assert!(output.contains("detail=") && output.contains("bad VLQ"), "missing detail: {output}");
}

/// The deploy/fail2ban/ergo-proxy.conf regex is the load-bearing
/// integration with the operator's ban path. Drift between this regex
/// and the actual emission shape means fail2ban silently stops banning.
#[test]
fn fail2ban_regex_matches_permanent_kinds() {
    // Mirror of the production failregex — fail2ban's `<HOST>` macro
    // is approximated by `(\S+)` for the purpose of this test.
    let regex_pattern = r#"PENALTY peer=(\S+) kind="(?:bad_magic|oversized_frame|bad_checksum|handshake_failed|address_sanity|malformed_peers)""#;

    let addr: std::net::IpAddr = "192.0.2.99".parse().unwrap();
    for kind in [
        "bad_magic",
        "oversized_frame",
        "bad_checksum",
        "handshake_failed",
        "address_sanity",
        "malformed_peers",
    ] {
        let line = capture(|| {
            warn!(peer = %addr, kind = kind, "PENALTY");
        });
        // Hand-rolled substring scan — pulls in no extra regex crate.
        let needle = format!("PENALTY peer=192.0.2.99 kind=\"{kind}\"");
        assert!(
            line.contains(&needle),
            "production emit shape does not match fail2ban regex \"{regex_pattern}\" \
             for kind={kind}: actual={line}"
        );
    }

    // Misbehavior kinds must NOT match the fail2ban regex.
    for kind in ["message_parse_failed", "connection_limit_exceeded"] {
        let line = capture(|| {
            warn!(peer = %addr, kind = kind, "PENALTY");
        });
        for permanent in [
            "bad_magic",
            "oversized_frame",
            "bad_checksum",
            "handshake_failed",
            "address_sanity",
            "malformed_peers",
        ] {
            let needle = format!("kind=\"{permanent}\"");
            assert!(
                !line.contains(&needle),
                "misbehavior kind {kind} unexpectedly contains permanent-ban kind {permanent}: {line}"
            );
        }
    }
}

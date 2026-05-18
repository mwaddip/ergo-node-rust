//! Detection of stuck `apply_state` retry loops.
//!
//! When the sync state machine repeatedly fails to apply the same
//! block with the same error, the symptom is invisible at INFO level:
//! the operator sees only normal `VALIDATION SWEEP STARTED` lines
//! and a single ERROR `apply_state failed` per sweep, easily lost in
//! steady-state traffic. Operator #10 hit this for hours with a
//! corrupt state DB before noticing.
//!
//! After [`STUCK_THRESHOLD`] consecutive identical failures at the
//! same height, the contract `validation_stuck` WARN event is
//! emitted (per `facts/journal-events.md` § "Validation and sync",
//! since 1.1). The Doctor adapter treats this as a primary "node
//! stuck" signal.

/// Number of consecutive same-height same-kind `apply_state` failures
/// before `validation_stuck` is emitted. Pinned by the contract.
pub const STUCK_THRESHOLD: u64 = 5;

/// Tracks consecutive same-height same-kind `apply_state` failures
/// so the contract `validation_stuck` event can be emitted once per
/// stuck window. A "stuck window" ends when a recovery happens
/// (Ok outcome) OR a different `(height, error_kind)` appears.
#[derive(Debug, Clone)]
pub struct ApplyStateFailureTracker {
    pub height: u32,
    pub error_kind: &'static str,
    pub missing_key: Option<String>,
    pub attempts: u64,
    pub stuck_emitted: bool,
}

/// Classify an `apply_state` error Display string.
///
/// The AVL prover wraps its errors via `anyhow!`; the surface form of
/// the missing-key case is `Key b"<escaped bytes>" does not exists`
/// (typo preserved upstream — accept both spellings defensively).
///
/// Returns `(error_kind, missing_key_hex)`. `error_kind` is one of
/// `"missing_key"` or `"other"`. `missing_key_hex` is `Some(_)` only
/// when `error_kind == "missing_key"` AND the byte-string literal
/// decoded cleanly to exactly 32 bytes.
pub fn classify_apply_state_error(display: &str) -> (&'static str, Option<String>) {
    if display.contains("does not exist") {
        ("missing_key", extract_missing_key_hex(display))
    } else {
        ("other", None)
    }
}

/// Best-effort extraction of the 32-byte key from a
/// `Key b"<escaped>" does not exists` error string. Returns None if
/// the format doesn't match or the decoded byte string isn't 32 bytes.
fn extract_missing_key_hex(s: &str) -> Option<String> {
    const MARKER: &str = "Key b\"";
    let after_marker = s.find(MARKER)? + MARKER.len();
    let rest = &s[after_marker..];
    let end = rest.find("\" does not exist")?;
    let bytes = parse_byte_string_literal(&rest[..end])?;
    if bytes.len() != 32 {
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Parse a Rust byte-string literal body (without surrounding `b""`)
/// back into raw bytes. Handles `\n`, `\r`, `\t`, `\0`, `\\`, `\"`,
/// and `\xHH`. Returns None on malformed input.
fn parse_byte_string_literal(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            return None;
        }
        match bytes[i + 1] {
            b'n' => { out.push(b'\n'); i += 2; }
            b'r' => { out.push(b'\r'); i += 2; }
            b't' => { out.push(b'\t'); i += 2; }
            b'0' => { out.push(0); i += 2; }
            b'\\' => { out.push(b'\\'); i += 2; }
            b'"' => { out.push(b'"'); i += 2; }
            b'x' => {
                if i + 3 >= bytes.len() {
                    return None;
                }
                let h1 = char::from(bytes[i + 2]).to_digit(16)? as u8;
                let h2 = char::from(bytes[i + 3]).to_digit(16)? as u8;
                out.push((h1 << 4) | h2);
                i += 4;
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Record an `apply_state` outcome on the tracker and emit
/// `validation_stuck` when the threshold is crossed. Returns true if
/// `validation_stuck` was emitted as a result of this call.
///
/// Semantics (per `facts/journal-events.md`):
///
/// - `error_display == None` (an Ok outcome) → reset the tracker.
/// - `error_display == Some(_)` at a new `(height, error_kind)` →
///   start a fresh tracker with `attempts = 1`,
///   `stuck_emitted = false`.
/// - `error_display == Some(_)` at the same `(height, error_kind)` →
///   increment `attempts`. Emit `validation_stuck` once when
///   `attempts >= STUCK_THRESHOLD && !stuck_emitted`.
pub fn record_apply_state_result(
    tracker: &mut Option<ApplyStateFailureTracker>,
    height: u32,
    error_display: Option<&str>,
) -> bool {
    let Some(err) = error_display else {
        *tracker = None;
        return false;
    };
    let (error_kind, missing_key) = classify_apply_state_error(err);
    let same_window = tracker
        .as_ref()
        .is_some_and(|t| t.height == height && t.error_kind == error_kind);
    if !same_window {
        *tracker = Some(ApplyStateFailureTracker {
            height,
            error_kind,
            missing_key,
            attempts: 1,
            stuck_emitted: false,
        });
        return false;
    }
    let t = tracker.as_mut().expect("same_window implies Some");
    t.attempts += 1;
    if t.attempts >= STUCK_THRESHOLD && !t.stuck_emitted {
        emit_validation_stuck(t);
        t.stuck_emitted = true;
        return true;
    }
    false
}

fn emit_validation_stuck(t: &ApplyStateFailureTracker) {
    match &t.missing_key {
        Some(hex) => tracing::warn!(
            height = t.height as u64,
            attempts = t.attempts,
            error_kind = t.error_kind,
            missing_key = %hex,
            "validation stuck"
        ),
        None => tracing::warn!(
            height = t.height as u64,
            attempts = t.attempts,
            error_kind = t.error_kind,
            "validation stuck"
        ),
    }
}

//! Classification of `apply_state` error strings for the
//! `validation_stuck` contract event.
//!
//! When the validation sweep's frontier wedges on an `apply_state`
//! failure, [`crate::sweep_backoff`] emits the contract
//! `validation_stuck` WARN event (per `facts/journal-events.md`
//! § "Validation and sync"). That event's `error_kind` / `missing_key`
//! fields name the failure mode. The sweep — which caught the error —
//! turns the error's `Display` string into those fields via
//! [`classify_apply_state_error`] and hands the result to the backoff.
//!
//! Stall *detection* and emission live in the backoff; this module owns
//! only the `apply_state`-specific string parsing, so the backoff stays
//! mode-agnostic.

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact apply_state error the sweep wedge produced. It does NOT
    /// contain "does not exist", so it classifies as `error_kind = "other"`.
    const HEIGHT_MISMATCH: &str = "unexpected block height: expected 2666, got 2668";

    #[test]
    fn height_mismatch_classifies_as_other_with_no_key() {
        let (kind, key) = classify_apply_state_error(HEIGHT_MISMATCH);
        assert_eq!(kind, "other");
        assert!(key.is_none());
    }
}

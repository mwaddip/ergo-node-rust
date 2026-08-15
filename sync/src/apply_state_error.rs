//! Classification of `apply_state` errors for the `validation_stuck`
//! contract event.
//!
//! When the validation sweep's frontier wedges on an `apply_state`
//! failure, [`crate::sweep_backoff`] emits the contract
//! `validation_stuck` WARN event (per `facts/journal-events.md`
//! § "Validation and sync"). That event's `error_kind` / `missing_key`
//! fields name the failure mode. The sweep — which caught the error —
//! turns the [`ValidationError`] into those fields via
//! [`classify_apply_state_error`] and hands the result to the backoff.
//!
//! Stall *detection* and emission live in the backoff; this module owns
//! only the `apply_state`-specific classification, so the backoff stays
//! mode-agnostic.
//!
//! ## Classify on the variant, never on the Display string
//!
//! Until v0.8.0 this took a `&str` and grepped the whole rendered error
//! for `"does not exist"`, while its caller held the typed
//! [`ValidationError`] and stringified it one line earlier. **Nothing
//! referenced the variant.** So when deferred evaluation was removed, the
//! script-failure kind stopped being produced and nothing failed to
//! compile — the same silent-drift shape as an `if let` that swallows a
//! new variant. A string-keyed classifier cannot be broken by a change to
//! the errors it classifies, which sounds like robustness and is the
//! opposite: it means the compiler has nothing to tell you.
//!
//! The one string parse that remains is *inside* a matched variant, where
//! the AVL key genuinely arrives as prose from the tree layer. That parse
//! is scoped to the variant's own payload, not to the rendered whole, so
//! it can no longer misfire on an unrelated variant whose `reason` text
//! happens to contain the phrase.

use ergo_validation::ValidationError;

/// Classify an `apply_state` error into the `validation_stuck`
/// `error_kind` / `missing_key` fields.
///
/// Returns `(error_kind, missing_key_hex)`. The `error_kind` domain is
/// pinned by `facts/journal-events.md` at contract 2.0:
///
/// | Kind | When |
/// |---|---|
/// | `missing_key` | the AVL tree lacks a key the block spends |
/// | `transaction_invalid` | [`ValidationError::TransactionInvalid`] |
/// | `other` | everything else |
///
/// `transaction_invalid` is **not** a rename of the retired `script_eval`.
/// That value named the deferred eval-failure *path*; this one names the
/// error *variant*, which covers a failed script and equally a failed ERG
/// or token preservation check. The distinction the Doctor needs is "is
/// the state store damaged, or does this node keep refusing a block the
/// network accepted?" — not which internal stage noticed.
///
/// `missing_key_hex` is `Some(_)` only when `error_kind == "missing_key"`
/// AND the byte-string literal decoded cleanly to exactly 32 bytes.
pub fn classify_apply_state_error(err: &ValidationError) -> (&'static str, Option<String>) {
    match err {
        ValidationError::TransactionInvalid { .. } => ("transaction_invalid", None),

        // The two channels through which `ergo_avltree_rust` surfaces a
        // missing key. Both wrap the same `anyhow!("Key {:?} does not
        // exists", …)` raised in the shared `operation.rs`, reached from
        // the persistent prover in UTXO mode and from the batch verifier
        // in digest mode. Matching only the UTXO one would drop
        // `missing_key` on every digest-mode node — silently, which is
        // the failure this rewrite exists to prevent.
        ValidationError::StateOperationFailed(reason)
        | ValidationError::ProofVerificationFailed(reason)
            if reason.contains("does not exist") =>
        {
            ("missing_key", extract_missing_key_hex(reason))
        }

        // `other` is the contract's explicit catch-all, so a variant added
        // to `ValidationError` landing here is the specified behaviour and
        // not an unhandled case. Widening the domain is a contract change
        // (consumers parse this field) and belongs to the main session.
        _ => ("other", None),
    }
}

/// Best-effort extraction of the 32-byte key from an AVL layer
/// `Key b"<escaped>" does not exists` message (typo preserved upstream —
/// the marker match stops before it, so both spellings are accepted).
///
/// Takes the matched variant's payload, not the rendered error. Returns
/// None if the format doesn't match or the decoded byte string isn't 32
/// bytes.
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
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b'r' => {
                out.push(b'\r');
                i += 2;
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'0' => {
                out.push(0);
                i += 2;
            }
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            b'"' => {
                out.push(b'"');
                i += 2;
            }
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

    #[test]
    fn height_mismatch_classifies_as_other_with_no_key() {
        // The exact apply_state error the sweep wedge produced. Its own
        // variant carries no prose at all, so there is nothing to grep
        // even if someone were inclined to.
        let err = ValidationError::HeightMismatch {
            expected: 2666,
            got: 2668,
        };
        let (kind, key) = classify_apply_state_error(&err);
        assert_eq!(kind, "other");
        assert!(key.is_none());
    }

    #[test]
    fn transaction_invalid_classifies_on_the_variant() {
        // Not keyed on the reason text: a preservation failure and a
        // script failure are the same fact to the consumer, and neither
        // renders a phrase this classifier looks for.
        let err = ValidationError::TransactionInvalid {
            index: 3,
            reason: "ERG preservation failed".to_string(),
        };
        let (kind, key) = classify_apply_state_error(&err);
        assert_eq!(kind, "transaction_invalid");
        assert!(key.is_none());
    }

    #[test]
    fn state_operation_failure_without_the_marker_is_other() {
        let err = ValidationError::StateOperationFailed("persist failed: disk full".to_string());
        let (kind, key) = classify_apply_state_error(&err);
        assert_eq!(kind, "other");
        assert!(key.is_none());
    }

    #[test]
    fn transaction_invalid_wins_over_a_reason_that_mentions_the_marker() {
        // The old string-grep classified on the rendered whole, so a
        // transaction rejection whose reason happened to say "does not
        // exist" would have come back `missing_key`. Variant-first
        // ordering makes that impossible.
        let err = ValidationError::TransactionInvalid {
            index: 0,
            reason: "input box does not exist in the proof".to_string(),
        };
        let (kind, _) = classify_apply_state_error(&err);
        assert_eq!(kind, "transaction_invalid");
    }
}

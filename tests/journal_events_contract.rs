//! Mechanical checks on `facts/journal-events.md` itself.
//!
//! These assert the two Format-conventions rules that are properties of the
//! **document** and need no emit sites: no marker may be a prefix of another,
//! and every marker is ASCII. Both were broken on 2026-08-12, the same day the
//! conventions were written down, and the breakage reached a release because
//! nothing checked either one.
//!
//! Deliberately NOT here: pairing marker literals against the code. That would
//! mean re-exporting every marker from every crate as a `const` purely so this
//! file could import it, and it buys less than the per-crate real-emit tests
//! already provide (`sync/src/state.rs` `sweep_resume_tests`,
//! `sweep_backoff`, `eval_backlog`). Document invariants live here; emit-site
//! conformance lives next to the emit.
//!
//! This test lives at workspace root because the contract is the main
//! session's and spans every crate. Putting it in `api/` — the crate that
//! advertises `JOURNAL_EVENTS_VERSION` — would couple `cargo test -p ergo-api`
//! to the repo layout above it and invert the crate boundary.

const CONTRACT: &str = include_str!("../facts/journal-events.md");

/// Every backtick-and-double-quote wrapped literal on a line, in order.
///
/// A `- **Marker:**` line may name more than one — `shutdown_signal_received`
/// documents `"SIGINT received"` or `"SIGTERM received"` — so this returns all
/// of them rather than the first.
fn quoted_literals(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("`\"") {
        let after = &rest[start + 2..];
        match after.find("\"`") {
            Some(end) => {
                out.push(&after[..end]);
                rest = &after[end + 2..];
            }
            None => break,
        }
    }
    out
}

fn marker_lines() -> Vec<&'static str> {
    CONTRACT
        .lines()
        .filter(|l| l.trim_start().starts_with("- **Marker:**"))
        .collect()
}

fn markers() -> Vec<&'static str> {
    marker_lines().iter().flat_map(|l| quoted_literals(l)).collect()
}

/// Guards every other test in this file against passing vacuously.
///
/// If the document's formatting changes so the extraction stops matching, the
/// prefix and ASCII checks below would iterate an empty list and pass — which
/// is precisely the failure mode this whole file exists to correct. The mirror
/// tests it replaces looked like coverage for months while asserting against
/// their own copy of the string.
#[test]
fn extraction_finds_the_markers() {
    let lines = marker_lines();
    assert!(
        lines.len() > 20,
        "found only {} `- **Marker:**` lines in facts/journal-events.md — the \
         document's formatting probably changed and every other check in this \
         file is now vacuous",
        lines.len()
    );

    for line in &lines {
        assert!(
            !quoted_literals(line).is_empty(),
            "a marker line yielded no literal, so extraction is silently \
             partial: {line}"
        );
    }
}

/// Parsers identify events by matching the marker **prefix**. If one marker
/// begins with the whole of another, a parser keyed on the shorter matches
/// both events and then fails looking for fields only the shorter carries.
///
/// Shipped once: `"deferred eval backlog at bound — draining to half"` began
/// with the entirety of `"deferred eval backlog"`. Renamed to
/// `"eval dispatch gate engaged"`.
#[test]
fn no_marker_is_a_prefix_of_another() {
    let markers = markers();
    let mut collisions = Vec::new();

    for (i, a) in markers.iter().enumerate() {
        for (j, b) in markers.iter().enumerate() {
            if i != j && a != b && b.starts_with(*a) {
                collisions.push(format!("{a:?} is a prefix of {b:?}"));
            }
        }
    }

    assert!(
        collisions.is_empty(),
        "marker prefix collisions — a parser keyed on the shorter matches \
         both:\n  {}",
        collisions.join("\n  ")
    );
}

/// Two markers rendering identically make the events indistinguishable to a
/// parser, which is the prefix collision's degenerate case.
#[test]
fn markers_are_unique() {
    let markers = markers();
    let mut sorted = markers.clone();
    sorted.sort_unstable();
    let mut dupes: Vec<_> = sorted.windows(2).filter(|w| w[0] == w[1]).map(|w| w[0]).collect();
    dupes.dedup();

    assert!(
        dupes.is_empty(),
        "duplicate markers, so these events cannot be told apart: {dupes:?}"
    );
}

/// The matched portion of a marker must survive being retyped from a report,
/// a terminal, or a grep. The free-text suffix after it may contain anything
/// and several markers do carry an em dash there — that is why this checks the
/// documented marker literal and not the emitted line.
///
/// Shipped once: the gate event carried U+2014 inside its marker while the
/// contract specified an ASCII hyphen, so the documented literal matched
/// nothing at all.
#[test]
fn markers_are_ascii() {
    let offenders: Vec<_> = markers()
        .into_iter()
        .filter(|m| !m.is_ascii())
        .map(|m| {
            let bad: Vec<String> = m
                .chars()
                .filter(|c| !c.is_ascii())
                .map(|c| format!("{c:?} (U+{:04X})", c as u32))
                .collect();
            format!("{m:?} contains {}", bad.join(", "))
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "non-ASCII inside a marker's matched portion:\n  {}",
        offenders.join("\n  ")
    );
}

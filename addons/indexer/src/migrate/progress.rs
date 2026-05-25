use std::io::Write;

/// Progress reporter for the migration runner.
///
/// Emits a `.` to `out` for every 1000 blocks migrated. At each
/// integer-percent boundary it terminates the current line with
/// ` (N%)\n` and begins a new dot-line. On `finalize()` it emits a
/// summary line with block count and elapsed time.
///
/// Generic over `W: Write` so tests can capture output into a buffer.
///
/// ## Percentage rounding rule
/// `pct = blocks_done * 100 / total_blocks`, truncating (Rust integer
/// division). `last_percent_emitted` starts at the truncated percentage
/// of `starting_from` so that resume runs begin at the right boundary.
///
/// ## `total_blocks = 0` handling
/// When `total_blocks` is 0, `tick()` is a no-op (division guard) and
/// `finalize()` still emits the summary line. No panic.
pub struct Progress<W: Write> {
    out: W,
    total_blocks: u32,
    blocks_done: u32,
    last_percent_emitted: u8,
    is_tty: bool,
    /// Counts how many blocks have been processed since the last dot.
    dot_counter: u32,
}

impl<W: Write> Progress<W> {
    /// Create a new progress reporter.
    ///
    /// - `out`: sink for progress output.
    /// - `total_blocks`: total number of source blocks to migrate.
    /// - `starting_from`: number of blocks already migrated (resume case).
    ///   Pass `0` for a fresh migration.
    /// - `is_tty`: whether `out` is a TTY. Controls flush behavior — per
    ///   contract § Progress output, TTY flushes after every dot (real-time
    ///   movement); non-TTY relies on line buffering (one line per 1%
    ///   boundary).
    pub fn new(mut out: W, total_blocks: u32, starting_from: u32, is_tty: bool) -> Self {
        // Percentage already done at resume start.
        let resume_pct: u8 = if total_blocks > 0 {
            ((starting_from as u64 * 100) / total_blocks as u64).min(100) as u8
        } else {
            0
        };

        // On resume (starting_from > 0), emit a leading marker so the
        // operator can see where progress picks up.
        if starting_from > 0 {
            let _ = write!(out, "({resume_pct}%) ");
            let _ = out.flush();
        }

        Self {
            out,
            total_blocks,
            blocks_done: starting_from,
            last_percent_emitted: resume_pct,
            is_tty,
            dot_counter: 0,
        }
    }

    /// Record one successfully-migrated block. Emits a `.` every 1000
    /// calls; emits ` (N%)\n` at each integer-percent boundary.
    pub fn tick(&mut self) {
        if self.total_blocks == 0 {
            return; // degenerate — nothing to track
        }

        self.blocks_done += 1;
        self.dot_counter += 1;

        if self.dot_counter >= 1000 {
            self.dot_counter = 0;
            let _ = write!(self.out, ".");
            // Only flush under TTY — non-TTY (journald, piped to a file)
            // relies on line buffering and ships output at newlines.
            if self.is_tty {
                let _ = self.out.flush();
            }
        }

        let current_pct: u8 =
            ((self.blocks_done as u64 * 100) / self.total_blocks as u64).min(100) as u8;

        if current_pct > self.last_percent_emitted {
            self.last_percent_emitted = current_pct;
            let _ = writeln!(self.out, " ({current_pct}%)");
            // Always flush on percent boundaries — even under non-TTY, the
            // operator wants to see progress markers as they happen.
            let _ = self.out.flush();
        }
    }

    /// Emit the final summary line and consume the reporter.
    ///
    /// The caller passes the wall-clock duration of the entire migration.
    /// Output is a human-readable line: `\nMigration complete: <N> blocks in <elapsed>`.
    pub fn finalize(mut self, elapsed: std::time::Duration) {
        // Emit a newline to close any trailing dot-line cleanly.
        let _ = writeln!(self.out);
        let blocks = self.blocks_done;
        let secs = elapsed.as_secs();
        let (mins, secs_rem) = (secs / 60, secs % 60);
        let human = if mins > 0 {
            format!("{mins}m{secs_rem:02}s")
        } else {
            format!("{secs_rem}s")
        };
        let bps = if elapsed.as_secs_f64() > 0.0 {
            format!(" ({:.0} blk/s avg)", blocks as f64 / elapsed.as_secs_f64())
        } else {
            String::new()
        };
        let _ = writeln!(
            self.out,
            "Migration complete: {blocks} blocks in {human}{bps}"
        );
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_emits_dot_every_1000_blocks() {
        let mut buf = Vec::new();
        let mut p = Progress::new(&mut buf, 100_000, 0, true);
        for _ in 0..1000 {
            p.tick();
        }
        assert!(String::from_utf8_lossy(&buf).contains("."));
    }

    #[test]
    fn percentage_boundary_emits_newline_with_percent() {
        let mut buf = Vec::new();
        let mut p = Progress::new(&mut buf, 100_000, 0, true);
        for _ in 0..1000 {
            p.tick();
        }
        let s = String::from_utf8_lossy(&buf).to_string();
        assert!(s.contains("(1%)") || s.contains("(0%)")); // 1000/100k = 1%
    }

    #[test]
    fn resume_starts_at_correct_percentage() {
        let mut buf = Vec::new();
        let _p = Progress::new(&mut buf, 100_000, 35_000, true);
        // Rule: pct = 35_000 * 100 / 100_000 = 35 (integer division, truncated).
        // The leading resume marker should be "(35%)".
        let s = String::from_utf8_lossy(&buf).to_string();
        assert!(
            s.contains("(35%)"),
            "expected '(35%)' in resume marker, got: {s:?}"
        );
    }

    #[test]
    fn finalize_emits_summary_line() {
        let mut buf = Vec::new();
        let mut p = Progress::new(&mut buf, 1000, 0, true);
        for _ in 0..1000 {
            p.tick();
        }
        p.finalize(std::time::Duration::from_secs(60));
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("Migration complete"),
            "missing 'Migration complete': {s}"
        );
        assert!(s.contains("1000 blocks"), "missing '1000 blocks': {s}");
    }

    #[test]
    fn no_panic_on_zero_total_blocks() {
        let mut buf = Vec::new();
        let mut p = Progress::new(&mut buf, 0, 0, false);
        p.tick(); // should not panic
        p.finalize(std::time::Duration::from_secs(1));
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("Migration complete"));
    }

    #[test]
    fn percentage_increases_monotonically() {
        let mut buf = Vec::new();
        let mut p = Progress::new(&mut buf, 10_000, 0, true);
        for _ in 0..10_000 {
            p.tick();
        }
        let s = String::from_utf8_lossy(&buf).to_string();
        // Should contain (1%) through (100%)
        assert!(s.contains("(1%)"), "missing (1%): {s}");
        assert!(s.contains("(100%)"), "missing (100%): {s}");
    }
}

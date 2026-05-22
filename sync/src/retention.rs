//! Body retention helpers for `blocks_to_keep` storage pruning.
//!
//! Pure math: no I/O, no state. The flush_pair calls these to compute the
//! prune horizon and to derive effective dial guardrails when
//! `blocks_to_keep >= 0`. See `../facts/sync.md` § "Block Body Retention".
//!
//! `blocks_to_keep == -1` (the default) means "no pruning, full archival" —
//! callers gate on the negative value and skip the dial-cap and prune paths
//! entirely.

/// Compute the prune horizon: the lowest height to RETAIN.
///
/// All bodies at heights `< horizon` are eligible for deletion. Bodies at
/// `horizon` and above stay.
///
/// `flushed` is the just-flushed `validator.validated_height()`, NOT the
/// chain tip. Using flushed-height as the anchor means bodies in
/// `(flushed, tip]` are always retained — crash recovery can re-apply
/// from on-disk bodies without ever needing peer re-fetches inside the
/// flush window.
///
/// Voting-epoch alignment: if the raw horizon (`flushed - keep + 1`) lands
/// inside a voting epoch, pull it back to the start of that epoch so the
/// current epoch's extensions stay intact for
/// `recompute_active_parameters_from_storage`. Mirrors JVM's
/// `FullBlockPruningProcessor.updateBestFullBlock`.
pub(crate) fn compute_prune_horizon(flushed: u32, keep: u32, voting_length: u32) -> u32 {
    let raw = flushed.saturating_sub(keep).saturating_add(1);
    if raw > voting_length {
        let epoch_start = (raw / voting_length) * voting_length;
        raw.min(epoch_start)
    } else {
        raw
    }
}

/// Apply the `blocks_to_keep` cap to the flush dial's min/max guardrails.
///
/// Returns `(effective_min, effective_max)`. When `blocks_to_keep < 0` (no
/// pruning), the configured values pass through unchanged. Otherwise each
/// is capped at `blocks_to_keep` so the `validated_height → tip` gap can
/// never exceed what the archive covers.
///
/// For `blocks_to_keep = 0`: `effective_max = 0`, meaning every block
/// triggers a flush (Durability::Immediate per block, JVM-equivalent
/// throughput cost — borne by operators who chose that setting).
pub(crate) fn effective_flush_bounds(
    blocks_to_keep: i32,
    flush_min_blocks: u32,
    flush_max_blocks: u32,
) -> (u32, u32) {
    if blocks_to_keep < 0 {
        (flush_min_blocks, flush_max_blocks)
    } else {
        let cap = blocks_to_keep as u32;
        (flush_min_blocks.min(cap), flush_max_blocks.min(cap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- compute_prune_horizon ---

    #[test]
    fn horizon_no_alignment_when_flushed_below_voting_length() {
        // raw = 600 - 100 + 1 = 501; 501 <= 1024 voting_length → no alignment
        assert_eq!(compute_prune_horizon(600, 100, 1024), 501);
    }

    #[test]
    fn horizon_pulled_back_to_epoch_start_when_raw_mid_epoch() {
        // flushed = 2500, keep = 100 → raw = 2401
        // raw > 1024, epoch_start = (2401 / 1024) * 1024 = 2048
        // raw.min(epoch_start) = 2048 → retains the current epoch [2048, 2500]
        assert_eq!(compute_prune_horizon(2500, 100, 1024), 2048);
    }

    #[test]
    fn horizon_unchanged_when_raw_at_exact_epoch_boundary() {
        // flushed = 2147, keep = 100 → raw = 2048 (= 2 * 1024)
        // raw > 1024 path: epoch_start = (2048 / 1024) * 1024 = 2048
        // raw.min(epoch_start) = 2048 → unchanged
        assert_eq!(compute_prune_horizon(2147, 100, 1024), 2048);
    }

    #[test]
    fn horizon_saturates_to_one_when_keep_exceeds_flushed() {
        // saturating_sub: 100 - 500 = 0, +1 = 1
        assert_eq!(compute_prune_horizon(100, 500, 1024), 1);
    }

    #[test]
    fn horizon_with_zero_keep_targets_one_past_flushed() {
        // raw = flushed - 0 + 1 = flushed + 1
        // For small flushed (< voting_length): no alignment
        assert_eq!(compute_prune_horizon(500, 0, 1024), 501);
        // For large flushed: alignment applies
        // raw = 3001, epoch_start = (3001 / 1024) * 1024 = 2048
        // → 2048 (pulled back to keep current epoch [2048, 3000])
        assert_eq!(compute_prune_horizon(3000, 0, 1024), 2048);
    }

    // --- effective_flush_bounds ---

    #[test]
    fn flush_bounds_pass_through_when_blocks_to_keep_negative() {
        let (min, max) = effective_flush_bounds(-1, 5, 100);
        assert_eq!((min, max), (5, 100));
    }

    #[test]
    fn flush_bounds_collapse_to_zero_when_blocks_to_keep_zero() {
        // Cap = 0 → both bounds clamp to 0; every block forces a flush.
        let (min, max) = effective_flush_bounds(0, 5, 100);
        assert_eq!((min, max), (0, 0));
    }

    #[test]
    fn flush_bounds_cap_at_small_blocks_to_keep() {
        // blocks_to_keep = 5 → max clamps from 100 to 5; min stays at 5.
        let (min, max) = effective_flush_bounds(5, 5, 100);
        assert_eq!((min, max), (5, 5));
    }

    #[test]
    fn flush_bounds_pass_through_when_blocks_to_keep_exceeds_config() {
        // blocks_to_keep = 1_000_000 → both bounds stay at their config values.
        let (min, max) = effective_flush_bounds(1_000_000, 5, 100);
        assert_eq!((min, max), (5, 100));
    }
}

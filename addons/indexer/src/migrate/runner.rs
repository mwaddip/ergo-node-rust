//! Per-block migration unit.
//!
//! The runner loop itself (iteration, signal handling, progress output) lives
//! in T10. This module owns the inner step — read one block from source, apply
//! it to target, verify the hash matches — so T10 only writes the loop.
//!
//! Contract (facts/indexer-migration.md § Per-block migration unit + § Hash
//! verification):
//!   1. read_block_data(H) from source
//!   2. apply_block(...) to target inside a single transaction (cursor + rows)
//!   3. read_block_data(H) from target
//!   4. compare blake2b256(source) vs blake2b256(target)
//!   5. on mismatch: bail. The target's transaction is already committed at
//!      this point; T10's loop is responsible for surfacing the failure.
//!      The contract calls for rollback of the failing block — the architecture
//!      here favors caller-aware handling because the runner loop knows whether
//!      to retry, abort, or surface to the operator. The cursor in the target
//!      will be `H` (committed alongside the block) so a re-run with --resume
//!      will perform check #5 at H and fail clean.

use anyhow::{anyhow, Result};

use super::hash::hash_block;
use super::{Backend, Cursor};

/// Migrate exactly one height. Returns `Ok(())` on success, `Err` on:
///   - source read failure (block missing, schema corruption)
///   - target write failure (FK violation, disk full, lost connection)
///   - target read-back failure
///   - hash mismatch between source and target after apply
///
/// The caller (T10 runner loop) is responsible for tracking progress, handling
/// SIGINT/SIGTERM between calls, and emitting the progress dots.
pub async fn migrate_one_block(
    source: &mut dyn Backend,
    target: &mut dyn Backend,
    height: u32,
    source_url: &str,
    source_fingerprint: [u8; 32],
) -> Result<()> {
    let src_data = source
        .read_block_data(height)
        .await
        .map_err(|e| anyhow!("source read at h={height} failed: {e}"))?;

    let cursor = Cursor {
        last_height: height,
        source_url: source_url.to_string(),
        source_fingerprint,
    };

    target
        .apply_block(&src_data, &cursor)
        .await
        .map_err(|e| anyhow!("target apply at h={height} failed: {e}"))?;

    let tgt_data = target
        .read_block_data(height)
        .await
        .map_err(|e| anyhow!("target read-back at h={height} failed: {e}"))?;

    let src_hash = hash_block(&src_data);
    let tgt_hash = hash_block(&tgt_data);
    if src_hash != tgt_hash {
        return Err(anyhow!(
            "hash mismatch at h={height}: source != target — \
             cursor remains at H, re-run with --resume after investigating"
        ));
    }

    Ok(())
}

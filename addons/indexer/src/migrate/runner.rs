//! Per-block migration unit + top-level `run()` loop.
//!
//! `migrate_one_block` (T7) handles a single height: read source, apply target,
//! verify hashes match. `run()` (T10) walks heights from start to max, ticking
//! progress and checking a cancel signal between blocks so SIGINT/SIGTERM
//! interrupts the loop cleanly between fully-committed blocks.
//!
//! Contract (facts/indexer-migration.md § Per-block migration unit + § Hash
//! verification):
//!   1. read_block_data(H) from source
//!   2. apply_block(...) to target inside a single transaction (cursor + rows)
//!   3. read_block_data(H) from target
//!   4. compare blake2b256(source) vs blake2b256(target)
//!   5. on mismatch: bail. The target's transaction is already committed at
//!      this point; T10's loop is responsible for surfacing the failure.
//!      The contract's literal "rollback on mismatch" was relaxed during T10:
//!      commit-then-error preserves diagnostic state and the operator's
//!      `--resume` retry triggers the spot-check failure at the same height,
//!      surfacing the divergence immediately. See `facts/indexer-migration.md`
//!      § Hash verification for the documented behavior.

use anyhow::{anyhow, Result};
use std::io::Write;
use tokio::sync::watch;

use super::hash::hash_block;
use super::progress::Progress;
use super::{resume, Backend, Cursor, MigrationPlan};

/// Migrate exactly one height. Returns `Ok(())` on success, `Err` on:
///   - source read failure (block missing, schema corruption)
///   - target write failure (FK violation, disk full, lost connection)
///   - target read-back failure
///   - hash mismatch between source and target after apply
///
/// The caller (the `run()` loop) is responsible for tracking progress, handling
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

/// Top-level migration loop. Walks source heights from the appropriate start
/// height (1 for fresh, `cursor.last_height + 1` for resume) up to
/// `source.max_height()`. Between every block, checks the `cancel` watch
/// receiver — when set to `true`, exits cleanly with an error indicating
/// interruption. The last fully-committed height becomes the target's cursor;
/// a subsequent `--resume` picks up at that height + 1.
///
/// `progress.tick()` is called once per committed block. The caller owns the
/// `Progress` and is responsible for calling `finalize()` when this returns
/// `Ok`. On error, the caller decides whether to print finalize or not — this
/// function does not touch `finalize()` so callers can choose.
///
/// Returns the highest height committed (== `max_height` on success).
pub async fn run<W: Write>(
    source: &mut dyn Backend,
    target: &mut dyn Backend,
    plan: &MigrationPlan,
    progress: &mut Progress<W>,
    cancel: watch::Receiver<bool>,
) -> Result<u32> {
    // Determine start height.
    //
    // Resume: re-validate all 6 preconditions (T6) and resume at cursor+1.
    // Fresh: init the target schema if the indexer_state table doesn't yet
    // have a schema_version row, then start at height 1.
    let start_height = if plan.resume {
        let cursor = resume::check_resume_preconditions(source, target, &plan.source.url).await?;
        cursor.last_height + 1
    } else {
        if target.schema_version().await?.is_none() {
            target.init_schema(resume::EXPECTED_SCHEMA_VERSION).await?;
        }
        1
    };

    let max_height = source
        .max_height()
        .await?
        .ok_or_else(|| anyhow!("source has no blocks (max_height returned None)"))?;

    // Source fingerprint stored on every per-block cursor write so resume can
    // detect source-data drift between runs (contract check #4).
    let source_fingerprint = hash_source_fingerprint(source).await?;

    if start_height > max_height {
        // Nothing to do — already at max. Common when --resume is set against
        // an already-completed migration.
        return Ok(max_height);
    }

    for h in start_height..=max_height {
        if *cancel.borrow() {
            return Err(anyhow!(
                "migration interrupted by signal at height {} (last committed: {})",
                h,
                h.saturating_sub(1)
            ));
        }

        migrate_one_block(source, target, h, &plan.source.url, source_fingerprint).await?;

        progress.tick();
    }

    Ok(max_height)
}

async fn hash_source_fingerprint(source: &mut dyn Backend) -> Result<[u8; 32]> {
    let data = source
        .read_block_data(1)
        .await
        .map_err(|e| anyhow!("source fingerprint read at h=1 failed: {e}"))?;
    Ok(hash_block(&data))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::{
        BlockData, BlockRow, BoxRow, DbSpec, DbType, MigrationPlan, TransactionRow,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    // Named alias to keep the FakeBackend struct field readable. The full type
    // is Arc<Mutex<Option<Box<dyn FnMut() + Send>>>> which trips clippy's
    // type_complexity lint; extracting it here satisfies the lint without
    // obscuring what the type actually is.
    type OnApplyHook = Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>;

    /// Writable in-memory backend for runner tests. Models:
    ///   - schema_version (Option<u32>)
    ///   - cursor (Option<Cursor>)
    ///   - block data by height (HashMap<u32, BlockData>) — both as a
    ///     "what's stored on the source side" and as "what apply_block wrote
    ///     to the target side"
    ///
    /// `apply_block` writes the supplied `BlockData` into the height map (so a
    /// subsequent `read_block_data` returns identical content — necessary for
    /// the runner's hash-verification step to pass under test conditions). It
    /// also clones the supplied cursor into self.
    ///
    /// An optional `on_apply` hook fires AFTER each successful apply — used by
    /// the cancel test to flip the watch flag mid-loop.
    struct FakeBackend {
        schema: Option<u32>,
        cursor: Option<Cursor>,
        block_data_by_height: HashMap<u32, BlockData>,
        max_height_override: Option<u32>,
        on_apply: OnApplyHook,
    }

    impl FakeBackend {
        fn empty() -> Self {
            Self {
                schema: None,
                cursor: None,
                block_data_by_height: HashMap::new(),
                max_height_override: None,
                on_apply: Arc::new(Mutex::new(None)),
            }
        }

        fn with_source_blocks(mut self, blocks: Vec<(u32, BlockData)>) -> Self {
            let max = blocks.iter().map(|(h, _)| *h).max();
            self.max_height_override = max;
            for (h, d) in blocks {
                self.block_data_by_height.insert(h, d);
            }
            // Source-side: source treats itself as schema-initialized.
            self.schema = Some(resume::EXPECTED_SCHEMA_VERSION);
            self
        }

        /// Install a hook that fires once after each successful `apply_block`.
        /// Used by the cancel test to trigger SIGINT-like behavior after the
        /// first block is committed.
        fn with_on_apply(self, hook: Box<dyn FnMut() + Send>) -> Self {
            *self.on_apply.lock().unwrap() = Some(hook);
            self
        }
    }

    #[async_trait]
    impl Backend for FakeBackend {
        async fn schema_version(&mut self) -> Result<Option<u32>> {
            Ok(self.schema)
        }
        async fn init_schema(&mut self, v: u32) -> Result<()> {
            self.schema = Some(v);
            Ok(())
        }
        async fn read_cursor(&mut self) -> Result<Option<Cursor>> {
            Ok(self.cursor.clone())
        }
        async fn write_cursor(&mut self, c: &Cursor) -> Result<()> {
            self.cursor = Some(c.clone());
            Ok(())
        }
        async fn max_height(&mut self) -> Result<Option<u32>> {
            Ok(self.max_height_override)
        }
        async fn read_block_data(&mut self, height: u32) -> Result<BlockData> {
            self.block_data_by_height
                .get(&height)
                .cloned()
                .ok_or_else(|| anyhow!("FakeBackend: no block data for height {height}"))
        }
        async fn apply_block(&mut self, data: &BlockData, cursor: &Cursor) -> Result<()> {
            self.block_data_by_height
                .insert(data.block.height, data.clone());
            self.cursor = Some(cursor.clone());
            // Fire the apply hook AFTER the commit is recorded — mirrors the
            // real backends, which commit then return.
            if let Some(hook) = self.on_apply.lock().unwrap().as_mut() {
                hook();
            }
            Ok(())
        }
    }

    fn synth_block(height: u32, seed: u8) -> BlockData {
        let header_id = [seed; 32];
        let block = BlockRow {
            height,
            header_id,
            timestamp: 1_700_000_000_000i64 + seed as i64,
            difficulty: 1_000_000i64 + seed as i64,
            miner_pk: (0u8..33).map(|i| seed.wrapping_add(i)).collect(),
            block_size: 1000u32 + seed as u32,
            tx_count: 1,
        };
        let tx = TransactionRow {
            tx_id: [seed.wrapping_add(1); 32],
            header_id,
            height,
            tx_index: 0,
            size: 200u32 + seed as u32,
        };
        let created_box = BoxRow {
            box_id: [seed.wrapping_add(2); 32],
            tx_id: tx.tx_id,
            header_id,
            height,
            output_index: 0,
            ergo_tree: vec![0x10u8, 0x01, 0x04, seed],
            ergo_tree_hash: [seed.wrapping_add(99); 32],
            address: format!("9addr_{seed}"),
            value: 1_000_000i64 + seed as i64,
            spent_tx_id: None,
            spent_height: None,
        };
        BlockData {
            block,
            transactions: vec![tx],
            created_boxes: vec![created_box],
            box_registers: vec![],
            box_tokens: vec![],
            minted_tokens: vec![],
            spent_box_updates: vec![],
        }
    }

    fn fresh_plan() -> MigrationPlan {
        MigrationPlan {
            source: DbSpec {
                url: "sqlite:///source.db".into(),
                kind: DbType::Sqlite,
            },
            target: DbSpec {
                url: "postgres://x@h/d".into(),
                kind: DbType::Postgres,
            },
            update_config: false,
            resume: false,
            yes: true,
        }
    }

    #[tokio::test]
    async fn run_migrates_all_blocks_when_no_cancel() {
        let blocks: Vec<(u32, BlockData)> = (1u32..=5u32)
            .map(|h| (h, synth_block(h, h as u8)))
            .collect();

        let mut source = FakeBackend::empty().with_source_blocks(blocks.clone());
        let mut target = FakeBackend::empty();

        let mut buf = Vec::new();
        let mut progress = Progress::new(&mut buf, 5, 0, false);

        let (_tx, rx) = watch::channel(false);

        let result = run(&mut source, &mut target, &fresh_plan(), &mut progress, rx).await;
        let final_h = result.expect("run should succeed");
        assert_eq!(final_h, 5);
        assert_eq!(target.cursor.as_ref().map(|c| c.last_height), Some(5));
        // All 5 blocks landed on target.
        for h in 1u32..=5 {
            assert!(
                target.block_data_by_height.contains_key(&h),
                "missing block at h={h}"
            );
        }
    }

    #[tokio::test]
    async fn run_stops_between_blocks_on_cancel_no_partial_commit() {
        let blocks: Vec<(u32, BlockData)> = (1u32..=5u32)
            .map(|h| (h, synth_block(h, h as u8)))
            .collect();

        let (tx, rx) = watch::channel(false);
        // The cancel-firing hook needs to run AFTER the first block commits.
        // We attach it to the target so apply_block triggers it. Use an Arc
        // so the hook can be cloned into both the apply state and reference.
        let cancel_after_first = Arc::new(Mutex::new(0u32));
        let cancel_after_first_for_hook = cancel_after_first.clone();
        let tx_for_hook = tx.clone();
        let hook: Box<dyn FnMut() + Send> = Box::new(move || {
            let mut n = cancel_after_first_for_hook.lock().unwrap();
            *n += 1;
            if *n == 1 {
                // After the first apply, fire the cancel signal — the next
                // iteration of the loop should observe `*cancel.borrow() ==
                // true` and bail before calling apply_block again.
                let _ = tx_for_hook.send(true);
            }
        });

        let mut source = FakeBackend::empty().with_source_blocks(blocks);
        let mut target = FakeBackend::empty().with_on_apply(hook);

        let mut buf = Vec::new();
        let mut progress = Progress::new(&mut buf, 5, 0, false);

        let err = run(&mut source, &mut target, &fresh_plan(), &mut progress, rx)
            .await
            .expect_err("expected interruption error");
        let msg = err.to_string();
        assert!(
            msg.contains("interrupted by signal"),
            "expected 'interrupted by signal' in error, got: {msg}"
        );

        // Cursor reflects only the first block (h=1). No partial commit at h=2.
        assert_eq!(target.cursor.as_ref().map(|c| c.last_height), Some(1));
        assert!(target.block_data_by_height.contains_key(&1));
        assert!(
            !target.block_data_by_height.contains_key(&2),
            "h=2 should not have landed — cancel fired before it ran"
        );
    }

    #[tokio::test]
    async fn run_resume_starts_at_cursor_plus_one() {
        let blocks: Vec<(u32, BlockData)> = (1u32..=5u32)
            .map(|h| (h, synth_block(h, h as u8)))
            .collect();

        // Source has all 5 blocks.
        let mut source = FakeBackend::empty().with_source_blocks(blocks.clone());

        // Target is already partially migrated: h=1 and h=2 are present,
        // cursor.last_height = 2. The 5 preconditions need to hold.
        let mut target = FakeBackend::empty();
        target.schema = Some(resume::EXPECTED_SCHEMA_VERSION);
        let fingerprint = hash_block(&synth_block(1, 1));
        target.cursor = Some(Cursor {
            last_height: 2,
            source_url: "sqlite:///source.db".into(),
            source_fingerprint: fingerprint,
        });
        target.block_data_by_height.insert(2, synth_block(2, 2));

        let mut buf = Vec::new();
        let mut progress = Progress::new(&mut buf, 5, 2, false);

        let plan = MigrationPlan {
            resume: true,
            ..fresh_plan()
        };

        let (_tx, rx) = watch::channel(false);
        let final_h = run(&mut source, &mut target, &plan, &mut progress, rx)
            .await
            .expect("resume run should succeed");
        assert_eq!(final_h, 5);
        // Resume started at 3, so blocks 3,4,5 should now exist on target.
        for h in 3u32..=5 {
            assert!(
                target.block_data_by_height.contains_key(&h),
                "missing block at h={h} after resume"
            );
        }
        assert_eq!(target.cursor.as_ref().map(|c| c.last_height), Some(5));
    }
}

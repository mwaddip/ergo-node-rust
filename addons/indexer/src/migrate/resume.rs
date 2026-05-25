use super::*;
use anyhow::{anyhow, bail, Result};

pub const EXPECTED_SCHEMA_VERSION: u32 = 1;

/// Run the 5-step resume precondition check (returns the cursor on success).
/// On any failure, return Err with a diagnostic naming the specific check.
pub async fn check_resume_preconditions(
    source: &mut dyn Backend,
    target: &mut dyn Backend,
    source_url: &str,
) -> Result<Cursor> {
    // Check 1: target schema_version match
    let tgt_ver = target
        .schema_version()
        .await?
        .ok_or_else(|| anyhow!("resume: target has no schema_version row"))?;
    if tgt_ver != EXPECTED_SCHEMA_VERSION {
        bail!("resume: target schema_version is {tgt_ver}, expected {EXPECTED_SCHEMA_VERSION}");
    }

    // Checks 2/3: cursor + source URL
    let cursor = target.read_cursor().await?.ok_or_else(|| {
        anyhow!("resume: target has no migration_cursor row — was it created by this tool?")
    })?;

    let normalized_src = normalize_url(source_url);
    let normalized_stored = normalize_url(&cursor.source_url);
    if normalized_src != normalized_stored {
        bail!(
            "resume: stored migration_source ({}) does not match current --in ({})",
            cursor.source_url,
            source_url
        );
    }

    // Check 4: source fingerprint
    let now_fingerprint = compute_source_fingerprint(source).await?;
    if now_fingerprint != cursor.source_fingerprint {
        bail!("resume: source data fingerprint changed between runs — refusing to continue");
    }

    // Check 5: spot-check the cursor-height block hash
    let src_block_data = source.read_block_data(cursor.last_height).await?;
    let tgt_block_data = target.read_block_data(cursor.last_height).await?;
    let src_hash = crate::migrate::hash::hash_block(&src_block_data);
    let tgt_hash = crate::migrate::hash::hash_block(&tgt_block_data);
    if src_hash != tgt_hash {
        bail!(
            "resume: spot-check failed at height {} — source and target diverge",
            cursor.last_height
        );
    }

    Ok(cursor)
}

fn normalize_url(url: &str) -> String {
    // Trim trailing slash, canonicalize SQLite paths, etc.
    // Minimum: handle "sqlite://" prefix consistently AND trim trailing `/`.
    url.trim_end_matches('/').to_string()
}

async fn compute_source_fingerprint(source: &mut dyn Backend) -> Result<[u8; 32]> {
    // Fingerprint = hash of block at height 1.
    let data = source.read_block_data(1).await?;
    Ok(crate::migrate::hash::hash_block(&data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ---------------------------------------------------------------------------
    // FakeBackend — richer than the T4 version: configurable schema, cursor,
    // and a per-height BlockData map so spot-check tests can inject divergence.
    // ---------------------------------------------------------------------------

    struct FakeBackend {
        schema: Option<u32>,
        cursor: Option<Cursor>,
        block_data_by_height: HashMap<u32, BlockData>,
    }

    impl FakeBackend {
        fn new(schema: Option<u32>, cursor: Option<Cursor>) -> Self {
            Self {
                schema,
                cursor,
                block_data_by_height: HashMap::new(),
            }
        }

        fn with_block(mut self, height: u32, data: BlockData) -> Self {
            self.block_data_by_height.insert(height, data);
            self
        }
    }

    #[async_trait::async_trait]
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
            unimplemented!()
        }
        async fn read_block_data(&mut self, height: u32) -> Result<BlockData> {
            self.block_data_by_height
                .get(&height)
                .cloned()
                .ok_or_else(|| anyhow!("FakeBackend: no block data for height {height}"))
        }
        async fn apply_block(&mut self, _: &BlockData, _: &Cursor) -> Result<()> {
            unimplemented!()
        }
    }

    /// Deterministic synthetic block keyed by seed. Mirrors the pattern from
    /// hash.rs tests — different seeds produce different data and thus different
    /// hashes.
    fn synth_block_data(seed: u8, height: u32) -> BlockData {
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

    /// Build a cursor whose source_fingerprint is consistent with the given
    /// source backend's height-1 block (so check 4 passes by default).
    fn cursor_for(source_data_at_h1: &BlockData, source_url: &str, last_height: u32) -> Cursor {
        let fingerprint = crate::migrate::hash::hash_block(source_data_at_h1);
        Cursor {
            last_height,
            source_url: source_url.to_string(),
            source_fingerprint: fingerprint,
        }
    }

    const SRC_URL: &str = "sqlite:///var/lib/ergo-indexer/source.db";

    #[tokio::test]
    async fn resume_fails_if_target_schema_version_mismatch() {
        let h1_data = synth_block_data(1, 1);
        let cursor = cursor_for(&h1_data, SRC_URL, 500);
        let cursor_h500 = synth_block_data(5, 500);

        let mut source = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), None)
            .with_block(1, h1_data.clone())
            .with_block(500, cursor_h500.clone());

        // Target has wrong schema version (2 instead of 1)
        let mut target = FakeBackend::new(Some(2), Some(cursor)).with_block(500, cursor_h500);

        let err = check_resume_preconditions(&mut source, &mut target, SRC_URL)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("schema_version is 2"), "got: {msg}");
        assert!(msg.contains("expected 1"), "got: {msg}");
    }

    #[tokio::test]
    async fn resume_fails_if_cursor_missing() {
        let h1_data = synth_block_data(1, 1);

        let mut source =
            FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), None).with_block(1, h1_data);

        // Target has correct schema but no cursor row
        let mut target = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), None);

        let err = check_resume_preconditions(&mut source, &mut target, SRC_URL)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no migration_cursor row"), "got: {msg}");
        assert!(msg.contains("was it created by this tool"), "got: {msg}");
    }

    #[tokio::test]
    async fn resume_fails_if_migration_source_mismatch() {
        let h1_data = synth_block_data(1, 1);
        // Cursor was created with a DIFFERENT source URL
        let stored_url = "sqlite:///var/lib/ergo-indexer/other.db";
        let cursor = cursor_for(&h1_data, stored_url, 500);
        let cursor_h500 = synth_block_data(5, 500);

        let mut source = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), None)
            .with_block(1, h1_data)
            .with_block(500, cursor_h500.clone());
        let mut target = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), Some(cursor))
            .with_block(500, cursor_h500);

        // We pass SRC_URL but cursor says stored_url — mismatch
        let err = check_resume_preconditions(&mut source, &mut target, SRC_URL)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stored migration_source"), "got: {msg}");
        assert!(msg.contains("does not match current --in"), "got: {msg}");
    }

    #[tokio::test]
    async fn resume_fails_if_source_fingerprint_mismatch() {
        let h1_data_original = synth_block_data(1, 1);
        // Cursor was created with the fingerprint of the original data
        let cursor = cursor_for(&h1_data_original, SRC_URL, 500);
        let cursor_h500 = synth_block_data(5, 500);

        // Source NOW has different data at height 1 (seed 99 ≠ seed 1)
        let h1_data_changed = synth_block_data(99, 1);

        let mut source = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), None)
            .with_block(1, h1_data_changed) // different from original
            .with_block(500, cursor_h500.clone());
        let mut target = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), Some(cursor))
            .with_block(500, cursor_h500);

        let err = check_resume_preconditions(&mut source, &mut target, SRC_URL)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("source data fingerprint changed between runs"),
            "got: {msg}"
        );
        assert!(msg.contains("refusing to continue"), "got: {msg}");
    }

    #[tokio::test]
    async fn resume_fails_if_spot_check_block_hash_mismatch() {
        let h1_data = synth_block_data(1, 1);
        let cursor = cursor_for(&h1_data, SRC_URL, 500);

        // Source and target have DIFFERENT data at the cursor height
        let src_h500 = synth_block_data(5, 500);
        let tgt_h500 = synth_block_data(7, 500); // different seed → different hash

        let mut source = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), None)
            .with_block(1, h1_data.clone())
            .with_block(500, src_h500);
        let mut target =
            FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), Some(cursor)).with_block(500, tgt_h500);

        let err = check_resume_preconditions(&mut source, &mut target, SRC_URL)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("spot-check failed at height 500"),
            "got: {msg}"
        );
        assert!(msg.contains("source and target diverge"), "got: {msg}");
    }

    #[tokio::test]
    async fn resume_succeeds_when_all_six_checks_pass() {
        let h1_data = synth_block_data(1, 1);
        let cursor = cursor_for(&h1_data, SRC_URL, 500);
        let expected_cursor = cursor.clone();
        // Same block data on both sides at cursor height
        let h500_data = synth_block_data(5, 500);

        let mut source = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), None)
            .with_block(1, h1_data)
            .with_block(500, h500_data.clone());
        let mut target = FakeBackend::new(Some(EXPECTED_SCHEMA_VERSION), Some(cursor))
            .with_block(500, h500_data);

        let result = check_resume_preconditions(&mut source, &mut target, SRC_URL)
            .await
            .unwrap();
        assert_eq!(result, expected_cursor);
    }
}

use std::sync::Arc;

use anyhow::{Context, Result};
use ergo_lib::ergotree_ir::chain::address::NetworkPrefix;
use tokio::sync::watch;

use crate::db::IndexerDb;
use crate::node_client::NodeClient;
use crate::parser;

pub async fn run(
    db: Arc<dyn IndexerDb>,
    node_url: &str,
    start_height: Option<u64>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let client = NodeClient::new(node_url)?;

    // Determine starting height
    let mut last_indexed = match start_height {
        Some(h) => {
            tracing::info!(height = h, "starting from specified height");
            h.saturating_sub(1)
        }
        None => match db.get_indexed_height().await? {
            Some(h) => {
                tracing::info!(height = h, "resuming from last indexed height");
                h
            }
            None => {
                tracing::info!("fresh database — starting from genesis (height 1)");
                0
            }
        },
    };

    // Detect network from node info. The node may be restoring or otherwise
    // unreachable at startup — retry rather than letting systemd respawn us.
    // Shutdown during startup retry exits immediately; nothing is in flight.
    let max_backoff = std::time::Duration::from_secs(60);
    let mut init_backoff = std::time::Duration::from_secs(5);
    let info = loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                tracing::info!("shutdown during startup; sync exiting");
                return Ok(());
            }
            result = client.info() => match result {
                Ok(info) => break info,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        backoff_secs = init_backoff.as_secs(),
                        "node unreachable at startup; retrying"
                    );
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            tracing::info!("shutdown during startup backoff; sync exiting");
                            return Ok(());
                        }
                        _ = tokio::time::sleep(init_backoff) => {}
                    }
                    init_backoff = (init_backoff * 2).min(max_backoff);
                }
            }
        }
    };
    let network = match info.network.as_str() {
        "mainnet" => NetworkPrefix::Mainnet,
        _ => NetworkPrefix::Testnet,
    };
    tracing::info!(
        network = %info.network,
        node_height = info.full_height,
        indexed_height = last_indexed,
        "sync starting"
    );

    let mut backoff = std::time::Duration::from_secs(1);

    loop {
        // Cheap shutdown check at the top of each outer iteration. Catches
        // the signal between blocks even if the inner `info_wait` select
        // arm hasn't fired yet.
        if *shutdown_rx.borrow() {
            tracing::info!(last_indexed, "shutdown signal received; sync exiting");
            return Ok(());
        }

        // Verify our tip is still canonical before waiting for new blocks.
        // Catches reorgs at or below `last_indexed` that the per-target check
        // misses (it only fires on the next height, where `get_block_id_at`
        // returns None and the comparison is skipped). Also necessary to
        // unblock `info_wait` in deep-reorg cases where the node's height has
        // dropped below ours — without rolling back first we'd block forever
        // waiting for the node to surpass a stale watermark.
        // Errors are logged-and-skipped so transient node connectivity blips
        // don't terminate the indexer; the next iteration retries.
        if last_indexed > 0 {
            match check_canonical_or_rollback(&db, &client, last_indexed).await {
                Ok(Some(fork)) => {
                    tracing::warn!(
                        prev_tip = last_indexed,
                        fork_point = fork,
                        "tip reorg detected — rolled back"
                    );
                    last_indexed = fork;
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "tip canonical check failed, proceeding");
                }
            }
        }

        // Wait for new blocks via long-poll. The long-poll is the
        // longest single await in the loop (~tens of seconds), so it's
        // also the most important to make cancellable.
        let node_height = tokio::select! {
            _ = shutdown_rx.changed() => {
                tracing::info!(last_indexed, "shutdown during long-poll; sync exiting");
                return Ok(());
            }
            result = client.info_wait(last_indexed) => match result {
                Ok(Some(info)) => info.full_height as u64,
                Ok(None) => continue, // 204 timeout
                Err(e) => {
                    tracing::warn!(error = %e, backoff_secs = backoff.as_secs(), "node unreachable");
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            tracing::info!(last_indexed, "shutdown during backoff; sync exiting");
                            return Ok(());
                        }
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }
        };
        backoff = std::time::Duration::from_secs(1);

        // Index all new blocks
        while last_indexed < node_height {
            // Check shutdown between each block. We deliberately let any
            // in-flight `insert_block` finish — its SQLite transaction
            // is atomic under WAL and rolling back mid-commit would just
            // re-do the work on next start. Exit before fetching the
            // next block instead.
            if *shutdown_rx.borrow() {
                tracing::info!(
                    last_indexed,
                    "shutdown signal received; sync exiting between blocks"
                );
                return Ok(());
            }
            let target = last_indexed + 1;

            // Per-target reorg detection — safety net for the `start_height=`
            // re-run case where target is already in the DB. Tip-level reorgs
            // are caught earlier by the outer check above.
            if let Some(fork) = check_canonical_or_rollback(&db, &client, target).await? {
                tracing::warn!(
                    height = target,
                    fork_point = fork,
                    "reorg detected at target — rolled back"
                );
                last_indexed = fork;
                continue;
            }

            // Fetch block data
            let block_ids = client
                .block_ids_at(target)
                .await
                .with_context(|| format!("failed to fetch block IDs at height {target}"))?;
            let header_id = block_ids
                .first()
                .with_context(|| format!("no block at height {target}"))?;

            let header = client
                .header(header_id)
                .await
                .with_context(|| format!("failed to fetch header {header_id}"))?;
            let txs = match client.transactions(header_id).await {
                Ok(t) => t,
                Err(e) => {
                    // Block sections not available (e.g. node didn't store pre-validation blocks)
                    tracing::debug!(height = target, error = %e, "skipping block — transactions unavailable");
                    last_indexed = target;
                    continue;
                }
            };

            // Parse and store
            let indexed = parser::parse_block(&header, &txs, network)
                .with_context(|| format!("failed to parse block at height {target}"))?;
            db.insert_block(&indexed)
                .await
                .with_context(|| format!("failed to insert block at height {target}"))?;

            last_indexed = target;

            if last_indexed % 1000 == 0 {
                tracing::info!(height = last_indexed, "indexed");
            }
        }
    }
}

/// Compare the stored block ID at `height` against the canonical chain.
/// If they match, returns `Ok(None)`. If they differ, walks back from `height`
/// to find the fork point, rolls back the DB to that fork, and returns
/// `Ok(Some(fork))`. If there's no stored ID at `height` (nothing to compare)
/// or the node returns no canonical ID, returns `Ok(None)`.
async fn check_canonical_or_rollback(
    db: &Arc<dyn IndexerDb>,
    client: &NodeClient,
    height: u64,
) -> Result<Option<u64>> {
    let stored_id = match db.get_block_id_at(height).await? {
        Some(id) => id,
        None => return Ok(None),
    };
    let canonical_ids = client.block_ids_at(height).await?;
    let canonical = match canonical_ids.first() {
        Some(c) => c,
        None => return Ok(None),
    };
    if hex::decode(canonical)? == stored_id {
        return Ok(None);
    }
    let mut fork = height;
    while fork > 0 {
        fork -= 1;
        let db_id = match db.get_block_id_at(fork).await? {
            Some(id) => id,
            None => break,
        };
        let ids = client.block_ids_at(fork).await?;
        if let Some(node_id) = ids.first() {
            if hex::decode(node_id)? == db_id {
                break;
            }
        }
    }
    tracing::info!(fork_point = fork, "rolling back to fork point");
    db.rollback_to(fork).await?;
    Ok(Some(fork))
}

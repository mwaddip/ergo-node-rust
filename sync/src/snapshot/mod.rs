pub mod download;
pub mod manifest;
pub mod parser;
pub mod protocol;

use std::collections::{HashMap, HashSet};

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::time::{Duration, Instant};

use crate::traits::{SyncChain, SyncTransport};
use download::ChunkDownloadStore;
use manifest::extract_subtree_ids;
use parser::parse_dfs_stream;
use protocol::*;

/// Parsed snapshot data ready for loading into state storage.
pub struct SnapshotData {
    /// (node_label, packed_node_bytes) pairs for all nodes in the snapshot.
    pub nodes: Vec<([u8; 32], Vec<u8>)>,
    /// Root hash of the AVL+ tree (first 32 bytes of stateRoot).
    pub root_hash: [u8; 32],
    /// Height of the AVL+ tree (last byte of stateRoot).
    pub tree_height: u8,
    /// Block height at which this snapshot was taken.
    pub snapshot_height: u32,
}

/// Configuration for snapshot sync.
pub struct SnapshotConfig {
    /// Minimum peers that must announce the same manifest before downloading.
    pub min_snapshot_peers: u32,
    /// Delivery timeout multiplier for chunks (applied to base delivery_timeout).
    pub chunk_timeout_multiplier: u32,
    /// Directory for temporary download storage.
    pub data_dir: std::path::PathBuf,
}

/// Split a 33-byte state_root into (root_hash[32], tree_height[1]).
pub fn split_state_root(state_root: &[u8; 33]) -> ([u8; 32], u8) {
    let mut root_hash = [0u8; 32];
    root_hash.copy_from_slice(&state_root[..32]);
    (root_hash, state_root[32])
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("no peers available for snapshot sync")]
    NoPeers,
    #[error("no valid snapshot found with sufficient peer quorum")]
    NoQuorum,
    #[error("manifest download failed from all peers")]
    ManifestDownloadFailed,
    #[error("chunk download stalled — no progress")]
    ChunkStalled,
    #[error("parse error: {0}")]
    Parse(#[from] parser::ParseError),
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("download store error: {0}")]
    Store(#[from] download::DownloadStoreError),
    #[error("download invalidated by chain reorg")]
    Invalidated,
    #[error("event stream closed")]
    StreamClosed,
}

// ── Discovery ───────────────────────────────────────────────────────────────

/// A validated snapshot: manifest ID confirmed against our header chain.
struct ValidatedSnapshot {
    height: u32,
    manifest_id: [u8; 32],
    peers: Vec<PeerId>,
}

/// Broadcast GetSnapshotsInfo, collect responses, validate against headers.
/// Returns the best snapshot (highest height) once quorum is reached.
async fn discover_snapshot<T: SyncTransport, C: SyncChain>(
    transport: &mut T,
    chain: &C,
    min_peers: u32,
    timeout: Duration,
) -> Result<ValidatedSnapshot, SnapshotError> {
    let peers = transport.outbound_peers().await;
    if peers.is_empty() {
        return Err(SnapshotError::NoPeers);
    }

    let (code, body) = SnapshotMessage::GetSnapshotsInfo.encode();
    for &peer in &peers {
        if let Err(e) = transport
            .send_to(peer, ProtocolMessage::Unknown { code, body: body.clone() })
            .await
        {
            tracing::warn!(%peer, "failed to send GetSnapshotsInfo: {e}");
        }
    }

    // manifest_id → (height, announcing_peers)
    let mut manifests: HashMap<[u8; 32], (u32, Vec<PeerId>)> = HashMap::new();
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let remaining = deadline - Instant::now();
        let event = tokio::time::timeout(remaining, transport.next_event()).await;

        let event = match event {
            Ok(Some(e)) => e,
            Ok(None) => return Err(SnapshotError::StreamClosed),
            Err(_) => break, // timeout
        };

        if let ProtocolEvent::Message {
            peer_id,
            message: ProtocolMessage::Unknown { code: SNAPSHOTS_INFO, body },
        } = event
        {
            if let Ok(SnapshotMessage::SnapshotsInfo(entries)) =
                SnapshotMessage::parse(SNAPSHOTS_INFO, &body)
            {
                for entry in entries {
                    if let Some(state_root) = chain.header_state_root(entry.height).await {
                        let (root_hash, _) = split_state_root(&state_root);
                        if root_hash == entry.manifest_id {
                            let record = manifests
                                .entry(entry.manifest_id)
                                .or_insert((entry.height, Vec::new()));
                            if !record.1.contains(&peer_id) {
                                record.1.push(peer_id);
                            }
                        }
                    }
                }

                // Check quorum
                if let Some((manifest_id, (height, peers))) = manifests
                    .iter()
                    .filter(|(_, (_, p))| p.len() as u32 >= min_peers)
                    .max_by_key(|(_, (h, _))| *h)
                {
                    return Ok(ValidatedSnapshot {
                        height: *height,
                        manifest_id: *manifest_id,
                        peers: peers.clone(),
                    });
                }
            }
        }
    }

    Err(SnapshotError::NoQuorum)
}

// ── Manifest download ───────────────────────────────────────────────────────

/// Download the manifest from one of the quorum peers. Retries with different
/// peers on failure.
async fn download_manifest<T: SyncTransport>(
    transport: &mut T,
    snapshot: &ValidatedSnapshot,
    timeout: Duration,
) -> Result<Vec<u8>, SnapshotError> {
    let (code, body) = SnapshotMessage::GetManifest(snapshot.manifest_id).encode();

    for &peer in &snapshot.peers {
        if let Err(e) = transport
            .send_to(peer, ProtocolMessage::Unknown { code, body: body.clone() })
            .await
        {
            tracing::warn!(%peer, "failed to send GetManifest: {e}");
        }

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline - Instant::now();
            let event = tokio::time::timeout(remaining, transport.next_event()).await;

            match event {
                Ok(Some(ProtocolEvent::Message {
                    message: ProtocolMessage::Unknown { code: MANIFEST, body },
                    ..
                })) => {
                    if let Ok(SnapshotMessage::Manifest(data)) =
                        SnapshotMessage::parse(MANIFEST, &body)
                    {
                        if data.len() >= 2 {
                            return Ok(data);
                        }
                    }
                }
                Ok(Some(_)) => continue, // other message, keep waiting
                Ok(None) => return Err(SnapshotError::StreamClosed),
                Err(_) => break, // timeout, try next peer
            }
        }
    }

    Err(SnapshotError::ManifestDownloadFailed)
}

// ── Chunk download ──────────────────────────────────────────────────────────

const CHUNKS_IN_PARALLEL: usize = 16;
const CHUNKS_PER_PEER: usize = 4;

/// Download all chunks into the temporary store. Resumes from existing progress.
async fn download_chunks<T: SyncTransport>(
    transport: &mut T,
    store: &ChunkDownloadStore,
    all_subtree_ids: &[[u8; 32]],
    peers: &[PeerId],
    chunk_timeout: Duration,
) -> Result<(), SnapshotError> {
    let stored: HashSet<[u8; 32]> = store.stored_chunk_ids()?.into_iter().collect();
    let mut pending: Vec<[u8; 32]> = all_subtree_ids
        .iter()
        .filter(|id| !stored.contains(*id))
        .copied()
        .collect();
    let mut in_flight: HashMap<[u8; 32], Instant> = HashMap::new();
    let mut peer_idx = 0;

    loop {
        if pending.is_empty() && in_flight.is_empty() {
            return Ok(());
        }

        // Fill in-flight up to CHUNKS_IN_PARALLEL
        while in_flight.len() < CHUNKS_IN_PARALLEL && !pending.is_empty() {
            if peers.is_empty() {
                return Err(SnapshotError::NoPeers);
            }
            let peer = peers[peer_idx % peers.len()];
            peer_idx += 1;

            let batch_size = CHUNKS_PER_PEER.min(pending.len());
            for _ in 0..batch_size {
                let id = match pending.pop() {
                    Some(id) => id,
                    None => break,
                };
                let (code, body) = SnapshotMessage::GetUtxoSnapshotChunk(id).encode();
                if let Err(e) = transport
                    .send_to(peer, ProtocolMessage::Unknown { code, body })
                    .await
                {
                    tracing::warn!(%peer, "failed to send GetUtxoSnapshotChunk: {e}");
                    pending.push(id); // put it back
                    break;
                }
                in_flight.insert(id, Instant::now());
            }
        }

        // Wait for a response (1s poll)
        let event = tokio::time::timeout(Duration::from_secs(1), transport.next_event()).await;

        match event {
            Ok(Some(ProtocolEvent::Message {
                message:
                    ProtocolMessage::Unknown {
                        code: UTXO_SNAPSHOT_CHUNK,
                        body,
                    },
                ..
            })) => {
                if let Ok(SnapshotMessage::UtxoSnapshotChunk(data)) =
                    SnapshotMessage::parse(UTXO_SNAPSHOT_CHUNK, &body)
                {
                    // The chunk's root node is the first node — its label is the chunk ID
                    if let Ok((node, _)) = parser::parse_node(&data, 32) {
                        let chunk_id = *node.label();
                        if in_flight.remove(&chunk_id).is_some() {
                            store.store_chunk(&chunk_id, &data)?;
                            let count = store.chunk_count()?;
                            let total = store.total_chunks();
                            if count % 100 == 0 || count == total {
                                tracing::info!("snapshot chunks: {count}/{total}");
                            }
                        }
                    }
                }
            }
            Ok(Some(_)) => {} // other message type, ignore
            Ok(None) => return Err(SnapshotError::StreamClosed),
            Err(_) => {}      // poll timeout, check for stale requests
        }

        // Re-queue timed-out requests
        let now = Instant::now();
        let timed_out: Vec<[u8; 32]> = in_flight
            .iter()
            .filter(|(_, sent_at)| now.duration_since(**sent_at) > chunk_timeout)
            .map(|(id, _)| *id)
            .collect();
        for id in timed_out {
            in_flight.remove(&id);
            pending.push(id);
        }
    }
}

// ── Assembly ────────────────────────────────────────────────────────────────

/// Parse manifest + all chunks into (label, packed_bytes) pairs.
fn assemble_snapshot(
    manifest_bytes: &[u8],
    store: &ChunkDownloadStore,
    snapshot_height: u32,
) -> Result<SnapshotData, SnapshotError> {
    let (root_height, _) = manifest::parse_manifest_header(manifest_bytes)?;

    // Parse manifest nodes (tree structure down to boundary depth)
    let manifest_nodes = parse_dfs_stream(&manifest_bytes[2..], 32)?;

    let mut all_nodes: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    for node in &manifest_nodes {
        all_nodes.push((*node.label(), node.packed_bytes().to_vec()));
    }

    // Parse all chunk nodes
    for (_, chunk_data) in store.iter_chunks()? {
        let chunk_nodes = parse_dfs_stream(&chunk_data, 32)?;
        for node in &chunk_nodes {
            all_nodes.push((*node.label(), node.packed_bytes().to_vec()));
        }
    }

    let root_hash = manifest_nodes
        .first()
        .map(|n| *n.label())
        .ok_or(parser::ParseError::UnexpectedEof)?;

    Ok(SnapshotData {
        nodes: all_nodes,
        root_hash,
        tree_height: root_height,
        snapshot_height,
    })
}

// ── Top-level orchestrator ──────────────────────────────────────────────────

/// Run the complete snapshot sync: discover → manifest → chunks → assemble.
///
/// Uses a temporary redb file for crash-safe chunk storage. On restart,
/// detects an interrupted download and resumes from where it left off.
pub async fn run_snapshot_sync<T: SyncTransport, C: SyncChain>(
    transport: &mut T,
    chain: &C,
    config: &SnapshotConfig,
) -> Result<SnapshotData, SnapshotError> {
    let download_path = config.data_dir.join("snapshot_download.redb");
    let chunk_timeout =
        Duration::from_secs(config.chunk_timeout_multiplier as u64 * 10);

    // ── Crash recovery: check for interrupted download ──────────────────
    if download_path.exists() {
        tracing::info!("found interrupted snapshot download, attempting resume");
        let store = ChunkDownloadStore::open(&download_path)?;
        let manifest_id = store.manifest_id();
        let height = store.snapshot_height();

        // Verify the download is still valid (no reorg at this height)
        if let Some(state_root) = chain.header_state_root(height).await {
            let (root_hash, _) = split_state_root(&state_root);
            if root_hash == manifest_id {
                let manifest_bytes = store.manifest_bytes()?;
                let subtree_ids = extract_subtree_ids(&manifest_bytes, 32)?;

                if store.is_complete()? {
                    tracing::info!("all chunks already downloaded, assembling");
                    let data = assemble_snapshot(&manifest_bytes, &store, height)?;
                    ChunkDownloadStore::cleanup(&download_path).ok();
                    return Ok(data);
                }

                // Resume chunk download
                tracing::info!(
                    "resuming: {}/{} chunks",
                    store.chunk_count()?,
                    store.total_chunks()
                );
                let peers = transport.outbound_peers().await;
                download_chunks(transport, &store, &subtree_ids, &peers, chunk_timeout)
                    .await?;
                let data = assemble_snapshot(&manifest_bytes, &store, height)?;
                ChunkDownloadStore::cleanup(&download_path).ok();
                return Ok(data);
            }
        }

        tracing::warn!("stale snapshot download (reorg?), starting fresh");
        ChunkDownloadStore::cleanup(&download_path).ok();
    }

    // ── Fresh discovery ─────────────────────────────────────────────────
    tracing::info!("starting UTXO snapshot discovery");
    let snapshot = discover_snapshot(
        transport,
        chain,
        config.min_snapshot_peers,
        Duration::from_secs(60),
    )
    .await?;
    tracing::info!(
        "snapshot found: height={}, peers={}",
        snapshot.height,
        snapshot.peers.len(),
    );

    // ── Download manifest ───────────────────────────────────────────────
    let manifest_bytes =
        download_manifest(transport, &snapshot, Duration::from_secs(30)).await?;
    tracing::info!("manifest downloaded: {} bytes", manifest_bytes.len());

    let subtree_ids = extract_subtree_ids(&manifest_bytes, 32)?;
    tracing::info!("{} subtree chunks to download", subtree_ids.len());

    // ── Create download store and fetch chunks ──────────────────────────
    let store = ChunkDownloadStore::create(
        &download_path,
        snapshot.manifest_id,
        snapshot.height,
        &manifest_bytes,
        subtree_ids.len() as u32,
    )?;

    download_chunks(
        transport,
        &store,
        &subtree_ids,
        &snapshot.peers,
        chunk_timeout,
    )
    .await?;

    // ── Assemble ────────────────────────────────────────────────────────
    tracing::info!("all chunks downloaded, assembling snapshot");
    let data = assemble_snapshot(&manifest_bytes, &store, snapshot.height)?;
    ChunkDownloadStore::cleanup(&download_path).ok();
    Ok(data)
}

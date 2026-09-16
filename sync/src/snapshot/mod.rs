pub mod download;
pub mod manifest;
pub mod parser;
pub mod protocol;
pub mod tree;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::time::{Duration, Instant};

use crate::traits::{SyncChain, SyncTransport};
use download::ChunkDownloadStore;
use manifest::{verify_manifest, VerifiedManifest};
use parser::parse_node;
use protocol::*;
use tree::{verify_chunk, KEY_LENGTH};

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
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("download store error: {0}")]
    Store(#[from] download::DownloadStoreError),
    #[error("download invalidated by chain reorg")]
    Invalidated,
    #[error("manifest: {0}")]
    Manifest(#[from] manifest::ManifestError),
    #[error("stored chunk does not verify against its subtree id: {0}")]
    ChunkInvalid(tree::TreeError),
    #[error("a subtree id of the manifest has no stored chunk")]
    ChunkMissing,
    #[error("event stream closed")]
    StreamClosed,
}

// ── Discovery ───────────────────────────────────────────────────────────────

/// A validated snapshot: manifest ID confirmed against our header chain.
/// `peers` is the quorum set. Manifest download removes a peer whose
/// manifest fails to verify, so chunk download never asks it either.
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
            .send_to(
                peer,
                ProtocolMessage::Unknown {
                    code,
                    body: body.clone(),
                },
            )
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
            message:
                ProtocolMessage::Unknown {
                    code: SNAPSHOTS_INFO,
                    body,
                },
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

/// Download the manifest from the quorum peers, one at a time, and return it
/// only once it verifies against the header's `state_root` at the snapshot
/// height.
///
/// A code-79 message counts only from the peer it was requested from (JVM
/// `processManifest`: `ri.peer == remote`); any other sender is ignored. A
/// peer whose manifest fails to verify is removed from `snapshot.peers` for
/// the rest of this bootstrap — it will not be asked for chunks either — and
/// the next peer is asked. A peer that does not answer within `timeout` keeps
/// its place in the quorum and is skipped.
async fn download_manifest<T: SyncTransport, C: SyncChain>(
    transport: &mut T,
    chain: &C,
    snapshot: &mut ValidatedSnapshot,
    timeout: Duration,
) -> Result<VerifiedManifest, SnapshotError> {
    // The expectation is the header's state root as the chain holds it now,
    // not the id discovery recorded from it.
    let Some(state_root) = chain.header_state_root(snapshot.height).await else {
        tracing::warn!(
            height = snapshot.height,
            "no header at the snapshot height any more, discovery is stale"
        );
        return Err(SnapshotError::Invalidated);
    };
    let (code, body) = SnapshotMessage::GetManifest(snapshot.manifest_id).encode();

    let mut idx = 0;
    while idx < snapshot.peers.len() {
        let peer = snapshot.peers[idx];
        if let Err(e) = transport
            .send_to(
                peer,
                ProtocolMessage::Unknown {
                    code,
                    body: body.clone(),
                },
            )
            .await
        {
            tracing::warn!(%peer, "failed to send GetManifest: {e}");
            idx += 1;
            continue;
        }

        let deadline = Instant::now() + timeout;
        let answer: Option<Vec<u8>> = loop {
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => break None,
            };
            match tokio::time::timeout(remaining, transport.next_event()).await {
                Ok(Some(ProtocolEvent::Message {
                    peer_id,
                    message:
                        ProtocolMessage::Unknown {
                            code: MANIFEST,
                            body,
                        },
                })) => {
                    if peer_id != peer {
                        tracing::debug!(
                            from = %peer_id, asked = %peer,
                            "ignoring a manifest from a peer we did not ask"
                        );
                        continue;
                    }
                    break Some(body);
                }
                Ok(Some(_)) => continue, // other message, keep waiting
                Ok(None) => return Err(SnapshotError::StreamClosed),
                Err(_) => break None, // timeout
            }
        };

        let Some(answer) = answer else {
            tracing::warn!(%peer, "no manifest within {timeout:?}, trying the next peer");
            idx += 1;
            continue;
        };

        let rejected = match SnapshotMessage::parse(MANIFEST, &answer) {
            Ok(SnapshotMessage::Manifest(data)) => match verify_manifest(data, &state_root) {
                Ok(manifest) => return Ok(manifest),
                Err(e) => e.to_string(),
            },
            Ok(other) => format!("code {MANIFEST} decoded as {other:?}"),
            Err(e) => e.to_string(),
        };
        // Nothing from a rejected manifest is kept, and neither is the peer:
        // it leaves the quorum set for this bootstrap. `idx` stays put — the
        // next peer has moved into this slot.
        tracing::warn!(
            peer_id = peer.0,
            kind = "manifest_rejected",
            detail = %rejected,
            height = snapshot.height,
            "PENALTY"
        );
        snapshot.peers.remove(idx);
    }

    Err(SnapshotError::ManifestDownloadFailed)
}

// ── Chunk download ──────────────────────────────────────────────────────────

const CHUNKS_IN_PARALLEL: usize = 16;
const CHUNKS_PER_PEER: usize = 4;

/// Chunk scheduling state: what is still to ask for, what is in flight, and
/// which peer last served an id badly.
struct ChunkQueue {
    pending: Vec<[u8; 32]>,
    in_flight: HashMap<[u8; 32], Instant>,
    /// Ids whose last delivery was rejected, and the peer that sent it.
    rejected_by: HashMap<[u8; 32], PeerId>,
}

impl ChunkQueue {
    /// Everything in `all_subtree_ids` that `store` does not hold yet.
    fn new(
        all_subtree_ids: &[[u8; 32]],
        store: &ChunkDownloadStore,
    ) -> Result<Self, SnapshotError> {
        let stored: HashSet<[u8; 32]> = store.stored_chunk_ids()?.into_iter().collect();
        Ok(Self {
            pending: all_subtree_ids
                .iter()
                .filter(|id| !stored.contains(*id))
                .copied()
                .collect(),
            in_flight: HashMap::new(),
            rejected_by: HashMap::new(),
        })
    }

    fn is_done(&self) -> bool {
        self.pending.is_empty() && self.in_flight.is_empty()
    }

    /// Up to `CHUNKS_PER_PEER` pending ids to ask `peer` for, skipping ids
    /// whose last delivery `peer` itself sent and we rejected — unless
    /// `peer` is the only peer there is and the preference cannot be met.
    fn take_batch(&mut self, peer: PeerId, sole_peer: bool) -> Vec<[u8; 32]> {
        let mut batch = Vec::with_capacity(CHUNKS_PER_PEER);
        let mut i = self.pending.len();
        while i > 0 && batch.len() < CHUNKS_PER_PEER {
            i -= 1;
            if sole_peer || self.rejected_by.get(&self.pending[i]) != Some(&peer) {
                batch.push(self.pending.swap_remove(i));
            }
        }
        batch
    }

    fn sent(&mut self, id: [u8; 32]) {
        self.in_flight.insert(id, Instant::now());
    }

    fn unsent(&mut self, ids: impl IntoIterator<Item = [u8; 32]>) {
        self.pending.extend(ids);
    }

    /// A chunk arrived from `peer`. It is stored only if its recomputed root
    /// label is in flight and the whole chunk verifies against that id —
    /// every link down to the leaves, every byte (`tree::verify_chunk`). A
    /// chunk that fails is dropped with a `PENALTY` naming the peer, its id
    /// goes back to `pending`, and the next request for it prefers another
    /// peer. Returns whether a chunk was stored.
    fn deliver(
        &mut self,
        peer: PeerId,
        data: &[u8],
        store: &ChunkDownloadStore,
    ) -> Result<bool, SnapshotError> {
        // The chunk's root node is the first node — its recomputed label is
        // the chunk id. Without a parseable root there is no id to attribute
        // the bytes to.
        let id = match parse_node(data, KEY_LENGTH) {
            Ok((root, _)) => *root.label(),
            Err(e) => {
                tracing::debug!(%peer, "ignoring a chunk whose root does not parse: {e}");
                return Ok(false);
            }
        };
        if !self.in_flight.contains_key(&id) {
            return Ok(false); // not asked for, or already satisfied
        }
        match verify_chunk(data, &id) {
            Ok(_) => {
                store.store_chunk(&id, data)?;
                self.in_flight.remove(&id);
                Ok(true)
            }
            Err(e) => {
                tracing::warn!(
                    peer_id = peer.0,
                    kind = "chunk_rejected",
                    detail = %e,
                    "PENALTY"
                );
                self.in_flight.remove(&id);
                self.pending.push(id);
                self.rejected_by.insert(id, peer);
                Ok(false)
            }
        }
    }

    /// Move requests older than `chunk_timeout` back to `pending`.
    fn requeue_timed_out(&mut self, chunk_timeout: Duration) {
        let now = Instant::now();
        let timed_out: Vec<[u8; 32]> = self
            .in_flight
            .iter()
            .filter(|(_, sent_at)| now.duration_since(**sent_at) > chunk_timeout)
            .map(|(id, _)| *id)
            .collect();
        for id in timed_out {
            self.in_flight.remove(&id);
            self.pending.push(id);
        }
    }
}

/// Download all chunks into the temporary store. Resumes from existing progress.
///
/// Every stored chunk verified on receipt against the id it was requested
/// under (`ChunkQueue::deliver`). Completion means every id in
/// `all_subtree_ids` has a stored chunk — nothing is counted.
async fn download_chunks<T: SyncTransport>(
    transport: &mut T,
    store: &ChunkDownloadStore,
    all_subtree_ids: &[[u8; 32]],
    peers: &[PeerId],
    chunk_timeout: Duration,
) -> Result<(), SnapshotError> {
    let total = all_subtree_ids.len();
    let mut queue = ChunkQueue::new(all_subtree_ids, store)?;
    let mut peer_idx = 0;

    loop {
        if queue.is_done() {
            return Ok(());
        }
        if !queue.pending.is_empty() && peers.is_empty() {
            return Err(SnapshotError::NoPeers);
        }

        // Fill in-flight up to CHUNKS_IN_PARALLEL, one pass over the peer
        // rotation at most: a peer with nothing it should be asked for, or
        // whose send fails, is skipped, and a rotation that sends nothing
        // falls through to the wait below instead of spinning.
        let mut idle_peers = 0;
        while queue.in_flight.len() < CHUNKS_IN_PARALLEL
            && !queue.pending.is_empty()
            && idle_peers < peers.len()
        {
            let peer = peers[peer_idx % peers.len()];
            peer_idx += 1;

            let mut batch = queue.take_batch(peer, peers.len() == 1).into_iter();
            let mut sent_any = false;
            while let Some(id) = batch.next() {
                let (code, body) = SnapshotMessage::GetUtxoSnapshotChunk(id).encode();
                if let Err(e) = transport
                    .send_to(peer, ProtocolMessage::Unknown { code, body })
                    .await
                {
                    tracing::warn!(%peer, "failed to send GetUtxoSnapshotChunk: {e}");
                    queue.unsent(std::iter::once(id).chain(batch));
                    break;
                }
                queue.sent(id);
                sent_any = true;
            }
            idle_peers = if sent_any { 0 } else { idle_peers + 1 };
        }

        // Wait for a response (1s poll)
        let event = tokio::time::timeout(Duration::from_secs(1), transport.next_event()).await;

        match event {
            Ok(Some(ProtocolEvent::Message {
                peer_id,
                message:
                    ProtocolMessage::Unknown {
                        code: UTXO_SNAPSHOT_CHUNK,
                        body,
                    },
            })) => {
                if let Ok(SnapshotMessage::UtxoSnapshotChunk(data)) =
                    SnapshotMessage::parse(UTXO_SNAPSHOT_CHUNK, &body)
                {
                    if queue.deliver(peer_id, &data, store)? {
                        let count = store.chunk_count()? as usize;
                        if count.is_multiple_of(100) || count == total {
                            tracing::info!("snapshot chunks: {count}/{total}");
                        }
                    }
                }
            }
            Ok(Some(_)) => {} // other message type, ignore
            Ok(None) => return Err(SnapshotError::StreamClosed),
            Err(_) => {} // poll timeout, check for stale requests
        }

        queue.requeue_timed_out(chunk_timeout);
    }
}

// ── Assembly ────────────────────────────────────────────────────────────────

/// Re-verify the manifest and every stored chunk, then collect their nodes
/// into `(label, packed_bytes)` pairs for `load_snapshot`.
///
/// The manifest is walked once more through `VerifiedManifest::nodes`. Each
/// stored chunk is verified against the subtree id it is stored under — root
/// label, every link down to the leaves, every byte. A missing or invalid
/// chunk is the same as a tampered stored manifest: the download is not
/// worth keeping.
fn assemble_snapshot(
    manifest: &VerifiedManifest,
    store: &ChunkDownloadStore,
    snapshot_height: u32,
) -> Result<SnapshotData, SnapshotError> {
    let root_hash = manifest.root_hash();

    // Re-walk the manifest: every link checked, every byte consumed.
    let manifest_nodes = manifest.nodes()?;
    let mut all_nodes: Vec<([u8; 32], Vec<u8>)> =
        Vec::with_capacity(manifest_nodes.len() + manifest.subtree_ids().len() * 8);
    for node in manifest_nodes {
        all_nodes.push(node.into_parts());
    }

    // Verify and collect every stored chunk.
    for subtree_id in manifest.subtree_ids() {
        let chunk_data = store
            .get_chunk(subtree_id)?
            .ok_or(SnapshotError::ChunkMissing)?;
        let tree = verify_chunk(&chunk_data, subtree_id).map_err(SnapshotError::ChunkInvalid)?;
        for node in tree.nodes {
            all_nodes.push(node.into_parts());
        }
    }

    Ok(SnapshotData {
        nodes: all_nodes,
        root_hash,
        tree_height: manifest.root_height(),
        snapshot_height,
    })
}

// ── Top-level orchestrator ──────────────────────────────────────────────────

/// Re-open an interrupted download and re-verify its manifest against the
/// header chain as it is now.
///
/// `None` means the file is not worth resuming: it cannot be read, or its
/// manifest no longer verifies against the header at its height — a reorg
/// while we were down, or a file we cannot vouch for. The recorded
/// `manifest_id` is not consulted; the bytes are hashed. The caller deletes
/// the file and starts discovery afresh.
async fn reopen_download<C: SyncChain>(
    chain: &C,
    path: &Path,
) -> Option<(ChunkDownloadStore, VerifiedManifest)> {
    let store = match ChunkDownloadStore::open(path) {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!("cannot open the interrupted snapshot download: {e}");
            return None;
        }
    };
    let height = store.snapshot_height();
    let Some(state_root) = chain.header_state_root(height).await else {
        tracing::warn!(height, "no header at the interrupted download's height");
        return None;
    };
    let bytes = match store.manifest_bytes() {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(
                height,
                "cannot read the interrupted download's manifest: {e}"
            );
            return None;
        }
    };
    match verify_manifest(bytes, &state_root) {
        Ok(manifest) => Some((store, manifest)),
        Err(e) => {
            tracing::warn!(
                height,
                "stored manifest does not verify against the header chain: {e}"
            );
            None
        }
    }
}

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
    let chunk_timeout = Duration::from_secs(config.chunk_timeout_multiplier as u64 * 10);

    // ── Crash recovery: check for interrupted download ──────────────────
    if download_path.exists() {
        tracing::info!("found interrupted snapshot download, attempting resume");
        match reopen_download(chain, &download_path).await {
            Some((store, manifest)) => {
                let height = store.snapshot_height();
                if store.is_complete(manifest.subtree_ids())? {
                    tracing::info!("all chunks already downloaded, assembling");
                } else {
                    tracing::info!(
                        "resuming: {}/{} chunks",
                        store.chunk_count()?,
                        store.total_chunks()
                    );
                    let peers = transport.outbound_peers().await;
                    download_chunks(
                        transport,
                        &store,
                        manifest.subtree_ids(),
                        &peers,
                        chunk_timeout,
                    )
                    .await?;
                }
                match assemble_snapshot(&manifest, &store, height) {
                    Ok(data) => {
                        ChunkDownloadStore::cleanup(&download_path).ok();
                        return Ok(data);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "stored chunks do not verify at assembly ({e}), \
                             discarding and starting fresh"
                        );
                        ChunkDownloadStore::cleanup(&download_path).ok();
                    }
                }
            }
            None => {
                tracing::warn!("stale snapshot download, starting fresh");
                ChunkDownloadStore::cleanup(&download_path).ok();
            }
        }
    }

    // ── Fresh discovery ─────────────────────────────────────────────────
    tracing::info!("starting UTXO snapshot discovery");
    let mut snapshot = discover_snapshot(
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

    // ── Download and verify the manifest ────────────────────────────────
    let manifest =
        download_manifest(transport, chain, &mut snapshot, Duration::from_secs(30)).await?;
    tracing::info!(
        "manifest verified: {} bytes, {} subtree chunks to download",
        manifest.bytes().len(),
        manifest.subtree_ids().len()
    );

    // ── Create download store and fetch chunks ──────────────────────────
    let store = ChunkDownloadStore::create(
        &download_path,
        manifest.root_hash(),
        snapshot.height,
        manifest.bytes(),
        manifest.subtree_ids().len() as u32,
    )?;

    download_chunks(
        transport,
        &store,
        manifest.subtree_ids(),
        &snapshot.peers,
        chunk_timeout,
    )
    .await?;

    // ── Assemble ────────────────────────────────────────────────────────
    tracing::info!("all chunks downloaded, assembling snapshot");
    let data = assemble_snapshot(&manifest, &store, snapshot.height)?;
    ChunkDownloadStore::cleanup(&download_path).ok();
    Ok(data)
}

use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use redb::{
    Database, DatabaseError, Durability, ReadOnlyDatabase, ReadableDatabase, ReadableTable,
    ReadableTableMetadata,
};
use tracing::{debug, error, info, warn};

use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::{AVLTree, Node, NodeHeader, NodeId, Resolver};
use ergo_avltree_rust::operation::{ADDigest, ADKey, ADValue, Digest32};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;

use crate::tables::*;
use crate::undo::UndoRecord;

/// Parameters describing the AVL+ tree's key/value structure.
pub struct AVLTreeParams {
    /// Key length in bytes.  32 for Ergo (box ID).
    pub key_length: usize,
    /// Fixed value length, or None for variable-length values.
    pub value_length: Option<usize>,
}

/// Lightweight read-only handle for snapshot operations.
/// Shares the underlying redb `Database` with `RedbAVLStorage` via `Arc`.
#[derive(Clone)]
pub struct SnapshotReader {
    db: Arc<Database>,
    key_length: usize,
}

/// A serialized snapshot of the AVL+ tree, split into manifest and chunks.
pub struct SnapshotDump {
    /// Root node hash (32 bytes).
    pub root_hash: [u8; 32],
    /// AVL+ tree height (from metadata).
    pub tree_height: u8,
    /// Serialized manifest: 2-byte header + DFS node bytes down to `manifest_depth`.
    pub manifest: Vec<u8>,
    /// Serialized subtree chunks: (subtree_root_label, DFS node bytes).
    pub chunks: Vec<([u8; 32], Vec<u8>)>,
}

/// Redb page cache size configuration.
pub enum CacheSize {
    /// Fixed size in bytes.
    Bytes(usize),
    /// Fraction of total system memory (0.0–1.0).
    Percent(f64),
}

impl Default for CacheSize {
    fn default() -> Self {
        CacheSize::Bytes(256 * 1024 * 1024)
    }
}

/// Outcome of a successful [`RedbAVLStorage::compact_to`].
#[derive(Debug, Clone)]
pub struct CompactionStats {
    /// Live tree nodes copied into the destination.
    pub nodes_written: u64,
    /// Size of the source file, which the call never modifies.
    pub source_bytes: u64,
    /// Size of the destination file after commit.
    pub dest_bytes: u64,
    /// Block height carried over from the source verbatim.
    pub block_height: u32,
    /// The destination's root digest, recomputed from its own stored nodes
    /// and proven equal to the source's `current_version`.  Cross-check it
    /// against the `stateRoot` of the header at `block_height` before
    /// swapping the file into place.
    pub digest: ADDigest,
}

/// Periodic progress report from [`RedbAVLStorage::compact_to`]'s copy pass.
#[derive(Debug, Clone, Copy)]
pub struct CompactionProgress {
    /// Nodes copied so far.
    pub nodes_written: u64,
}

impl CacheSize {
    /// Resolve to a concrete byte count.
    ///
    /// For `Percent`, reads `/proc/meminfo` to detect total RAM.
    /// Falls back to 256 MB if system memory cannot be determined.
    pub fn resolve(&self) -> usize {
        match self {
            CacheSize::Bytes(b) => *b,
            CacheSize::Percent(p) => {
                let total = read_memtotal().unwrap_or(256 * 1024 * 1024);
                (total as f64 * p.clamp(0.0, 1.0)) as usize
            }
        }
    }
}

/// Format bytes as a flat lowercase hex string.
fn bytes_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}

/// Format a 32-byte digest as a flat lowercase hex string (64 chars).
/// Used for grep-friendly diagnostic logging.
fn digest_hex(label: &Digest32) -> String {
    bytes_hex(label)
}

/// Log a resolver miss at WARN with the digest hex and a short reason tag.
/// The digest tells us which node the prover expected but storage didn't have —
/// grep the log for `resolver miss` after a "Should never reach this point"
/// bail to recover the missing label and walk the tree state.
fn log_resolver_miss(digest: &Digest32, reason: &'static str) {
    warn!(
        digest = %digest_hex(digest),
        reason,
        "resolver miss: returning LabelOnly placeholder"
    );
}

/// Read MemTotal from `/proc/meminfo`.  Returns total RAM in bytes.
fn read_memtotal() -> Option<usize> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb_str = rest.trim().strip_suffix("kB")?.trim();
            let kb: usize = kb_str.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Persistent, versioned, crash-safe AVL+ authenticated dictionary over redb.
pub struct RedbAVLStorage {
    db: Arc<Database>,
    tree_params: AVLTreeParams,
    keep_versions: u32,
    current_version: Option<ADDigest>,
    /// (LSN, digest) pairs, newest first.  Head is the current version.
    version_chain: VecDeque<(u64, ADDigest)>,
}

impl RedbAVLStorage {
    /// Open or create state storage at `path`.
    pub fn open(
        path: &Path,
        tree_params: AVLTreeParams,
        keep_versions: u32,
        cache_size: CacheSize,
    ) -> Result<Self> {
        let db = Database::builder()
            .set_cache_size(cache_size.resolve())
            .create(path)
            .context("failed to create/open redb")?;

        // Ensure tables exist.
        {
            let mut write_txn = db.begin_write()?;
            write_txn.set_quick_repair(true);
            write_txn.open_table(NODES_TABLE)?;
            write_txn.open_table(UNDO_TABLE)?;
            write_txn.open_table(META_TABLE)?;
            write_txn.commit()?;
        }

        let db = Arc::new(db);
        let (current_version, version_chain) = Self::restore_state(&db)?;

        // One-shot migration for storage written before META_BLOCK_HEIGHT
        // existed.  Contract: block_height().is_some() == version().is_some().
        // A legacy file has version but no block_height key; fix that here
        // so callers never see the invariant violated.  Legacy data has no
        // real block_height to recover — caller must fall back to
        // header-scan when it encounters Some(0) on an older file.
        if current_version.is_some() {
            let has_block_height = {
                let read_txn = db.begin_read()?;
                let meta = read_txn.open_table(META_TABLE)?;
                meta.get(META_BLOCK_HEIGHT)?.is_some()
            };
            if !has_block_height {
                warn!("state.redb predates block_height metadata — migrating to 0");
                let mut write_txn = db.begin_write()?;
                write_txn.set_quick_repair(true);
                write_txn.set_durability(Durability::Immediate)?;
                {
                    let mut meta = write_txn.open_table(META_TABLE)?;
                    meta.insert(META_BLOCK_HEIGHT, 0u32.to_be_bytes().as_slice())?;
                }
                write_txn.commit()?;
            }
        }

        // Journal-events contract: `state_storage_open_complete`.
        // Marker is the parse anchor for the Doctor adapter; `digest` is the
        // contract's optional field (empty storage → "none").
        let digest_str = current_version
            .as_ref()
            .map(|v| {
                let mut hex = String::with_capacity(v.len() * 2);
                for b in v.as_ref() {
                    let _ = write!(&mut hex, "{:02x}", b);
                }
                hex
            })
            .unwrap_or_else(|| String::from("none"));
        info!(
            digest = %digest_str,
            "UTXO state storage opened"
        );
        debug!(
            chain_len = version_chain.len(),
            "state storage open: in-memory state restored"
        );

        Ok(Self {
            db,
            tree_params,
            keep_versions,
            current_version,
            version_chain,
        })
    }

    /// Rebuild in-memory state from an existing database.
    #[allow(clippy::type_complexity)]
    fn restore_state(db: &Database) -> Result<(Option<ADDigest>, VecDeque<(u64, ADDigest)>)> {
        let read_txn = db.begin_read()?;
        let meta = read_txn.open_table(META_TABLE)?;

        let current_version = match meta.get(META_CURRENT_VERSION)? {
            Some(v) => {
                let bytes: &[u8] = v.value();
                Some(Bytes::copy_from_slice(bytes))
            }
            None => return Ok((None, VecDeque::new())),
        };

        let version_chain = match meta.get(META_VERSIONS)? {
            Some(chain_data) => {
                let bytes: &[u8] = chain_data.value();
                Self::deserialize_version_chain(bytes)?
            }
            None => VecDeque::new(),
        };

        Ok((current_version, version_chain))
    }

    /// Update keep_versions at runtime (e.g. switching from initial sync to normal).
    pub fn set_keep_versions(&mut self, keep_versions: u32) {
        self.keep_versions = keep_versions;
    }

    /// Resize the read cache at runtime without reopening the database.
    /// Evicts excess cached pages immediately if the new limit is smaller.
    pub fn resize_cache(&self, cache_bytes: usize) {
        self.db.set_read_cache_limit(cache_bytes);
        self.db.clear_read_cache();
    }

    /// Create a read-only snapshot reader that shares the database handle.
    /// Call this BEFORE handing the storage to PersistentBatchAVLProver.
    pub fn snapshot_reader(&self) -> SnapshotReader {
        SnapshotReader {
            db: Arc::clone(&self.db),
            key_length: self.tree_params.key_length,
        }
    }

    /// Create a Resolver closure that reads nodes from storage on demand.
    /// Misses log WARN with the digest hex for post-failure diagnostics.
    pub fn resolver(&self) -> Resolver {
        let db = Arc::clone(&self.db);
        let key_length = self.tree_params.key_length;
        let value_length = self.tree_params.value_length;

        Arc::new(move |digest: &Digest32| {
            let read_txn = match db.begin_read() {
                Ok(txn) => txn,
                Err(e) => {
                    error!(error = %e, "resolver: begin_read failed");
                    log_resolver_miss(digest, "begin_read_error");
                    return Node::LabelOnly(NodeHeader::new(Some(*digest), None));
                }
            };
            let table = match read_txn.open_table(NODES_TABLE) {
                Ok(t) => t,
                Err(e) => {
                    error!(error = %e, "resolver: open_table failed");
                    log_resolver_miss(digest, "open_table_error");
                    return Node::LabelOnly(NodeHeader::new(Some(*digest), None));
                }
            };
            match table.get(digest.as_slice()) {
                Ok(Some(data)) => {
                    let bytes: &[u8] = data.value();
                    let dummy: Resolver = Arc::new(|_| panic!("resolver called during unpack"));
                    let tree = AVLTree::with_resolver(dummy, key_length, value_length);
                    let node_id = tree.unpack(&Bytes::copy_from_slice(bytes));
                    let node = node_id.borrow().clone();
                    node
                }
                Ok(None) => {
                    log_resolver_miss(digest, "not_in_storage");
                    Node::LabelOnly(NodeHeader::new(Some(*digest), None))
                }
                Err(e) => {
                    error!(error = %e, "resolver: table.get failed");
                    log_resolver_miss(digest, "table_get_error");
                    Node::LabelOnly(NodeHeader::new(Some(*digest), None))
                }
            }
        })
    }

    /// Read a single node's packed bytes by label.
    pub fn get_node(&self, label: &Digest32) -> Result<Option<Bytes>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(NODES_TABLE)?;
        match table.get(label.as_slice())? {
            Some(data) => {
                let bytes: &[u8] = data.value();
                Ok(Some(Bytes::copy_from_slice(bytes)))
            }
            None => Ok(None),
        }
    }

    /// Read top node hash and height from metadata.
    pub fn root_state(&self) -> Option<(Digest32, usize)> {
        let read_txn = self.db.begin_read().ok()?;
        let meta = read_txn.open_table(META_TABLE).ok()?;

        let hash_guard = meta.get(META_TOP_NODE_HASH).ok()??;
        let hash_bytes: &[u8] = hash_guard.value();
        let mut hash: Digest32 = [0u8; 32];
        hash.copy_from_slice(hash_bytes);
        drop(hash_guard);

        let height_guard = meta.get(META_TOP_NODE_HEIGHT).ok()??;
        let height_bytes: &[u8] = height_guard.value();
        let height = u32::from_be_bytes(
            height_bytes.try_into().ok()?,
        ) as usize;

        Some((hash, height))
    }

    // ── helpers ────────────────────────────────────────────────────────

    /// Walk the prover's tree from `node`, collecting changed/new nodes.
    fn collect_changed_nodes(
        tree: &AVLTree,
        node: &NodeId,
        is_root: bool,
        results: &mut Vec<(Digest32, Bytes)>,
    ) {
        let n = node.borrow();

        // LabelOnly = never loaded = never changed.
        if matches!(&*n, Node::LabelOnly(_)) {
            return;
        }

        let is_new = n.is_new();
        let visited = n.visited();

        if !is_root && !is_new && !visited {
            return;
        }

        let is_internal = n.is_internal();
        // Grab children before dropping the borrow — avoids triggering resolve.
        let children = if is_internal {
            if let Node::Internal(internal) = &*n {
                Some((internal.left.clone(), internal.right.clone()))
            } else {
                unreachable!()
            }
        } else {
            None
        };

        drop(n);

        let label = tree.label(node);
        let packed = tree.pack(node.clone());
        results.push((label, packed));

        if let Some((left, right)) = children {
            Self::collect_changed_nodes(tree, &left, false, results);
            Self::collect_changed_nodes(tree, &right, false, results);
        }
    }

    /// Create a lightweight AVLTree for pack/unpack only (no real resolver).
    fn make_tree(&self) -> AVLTree {
        let dummy: Resolver = Arc::new(|_| panic!("dummy resolver"));
        AVLTree::with_resolver(dummy, self.tree_params.key_length, self.tree_params.value_length)
    }

    fn serialize_version_chain(chain: &VecDeque<(u64, ADDigest)>) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(chain.len() as u32).to_be_bytes());
        for (lsn, digest) in chain {
            buf.extend_from_slice(&lsn.to_be_bytes());
            buf.extend_from_slice(&(digest.len() as u32).to_be_bytes());
            buf.extend_from_slice(digest);
        }
        buf
    }

    fn deserialize_version_chain(data: &[u8]) -> Result<VecDeque<(u64, ADDigest)>> {
        let mut pos = 0;
        if data.len() < 4 {
            bail!("version chain data too short");
        }
        let count = u32::from_be_bytes(data[pos..pos + 4].try_into()?) as usize;
        pos += 4;
        let mut chain = VecDeque::with_capacity(count);
        for _ in 0..count {
            if pos + 12 > data.len() {
                bail!("version chain truncated");
            }
            let lsn = u64::from_be_bytes(data[pos..pos + 8].try_into()?);
            pos += 8;
            let digest_len = u32::from_be_bytes(data[pos..pos + 4].try_into()?) as usize;
            pos += 4;
            if pos + digest_len > data.len() {
                bail!("version chain truncated reading digest");
            }
            let digest = Bytes::copy_from_slice(&data[pos..pos + digest_len]);
            pos += digest_len;
            chain.push_back((lsn, digest));
        }
        Ok(chain)
    }

    /// Current LSN from the version chain (0 if empty).
    fn current_lsn(&self) -> u64 {
        self.version_chain.front().map(|(lsn, _)| *lsn).unwrap_or(0)
    }

    /// Bulk-load a UTXO snapshot without undo records.
    ///
    /// Writes all packed nodes and sets root state in a single transaction.
    /// No rollback history is created — this is a one-shot bootstrap.
    pub fn load_snapshot(
        &mut self,
        nodes: impl Iterator<Item = (Digest32, Bytes)>,
        root_hash: Digest32,
        height: usize,
        version: ADDigest,
        block_height: u32,
    ) -> Result<()> {
        let mut write_txn = self.db.begin_write()?;
        write_txn.set_quick_repair(true);
        {
            let mut nodes_table = write_txn.open_table(NODES_TABLE)?;
            let mut meta_table = write_txn.open_table(META_TABLE)?;

            for (label, packed) in nodes {
                nodes_table.insert(label.as_slice(), packed.as_ref())?;
            }

            meta_table.insert(META_TOP_NODE_HASH, root_hash.as_slice())?;
            meta_table
                .insert(META_TOP_NODE_HEIGHT, (height as u32).to_be_bytes().as_slice())?;
            meta_table.insert(META_CURRENT_VERSION, version.as_ref())?;
            meta_table.insert(META_LSN, 1u64.to_be_bytes().as_slice())?;
            meta_table.insert(META_BLOCK_HEIGHT, block_height.to_be_bytes().as_slice())?;

            let chain = VecDeque::from([(1u64, version.clone())]);
            let chain_bytes = Self::serialize_version_chain(&chain);
            meta_table.insert(META_VERSIONS, chain_bytes.as_slice())?;
        }
        write_txn.commit()?;

        self.current_version = Some(version.clone());
        self.version_chain = VecDeque::from([(1, version)]);

        Ok(())
    }

    /// Read the caller-supplied block height last committed with the
    /// current state version.  Returns `None` iff `version()` is `None`
    /// (empty storage).
    pub fn block_height(&self) -> Option<u32> {
        let read_txn = self.db.begin_read().ok()?;
        let meta = read_txn.open_table(META_TABLE).ok()?;
        let guard = meta.get(META_BLOCK_HEIGHT).ok()??;
        let bytes: &[u8] = guard.value();
        bytes.try_into().ok().map(u32::from_be_bytes)
    }

    /// Atomically persist tree changes and the caller-supplied block
    /// height.  Identical to the trait `update()` but with block_height
    /// written in the same redb transaction as the state nodes,
    /// metadata, and undo record.
    ///
    /// Call this from block-applying code where the caller knows which
    /// block produced this state root.  On resume after crash, retrieve
    /// it via `block_height()` — the storage knows exactly which block
    /// it is at, no header scan required.
    pub fn update_with_height(
        &mut self,
        prover: &mut BatchAVLProver,
        additional_data: Vec<(ADKey, ADValue)>,
        block_height: u32,
    ) -> Result<()> {
        self.update_internal(prover, additional_data, Some(block_height))
    }

    /// Force a durable commit — fsync all pending writes to disk.
    ///
    /// `update()` uses `Durability::None` so normal commits skip fsync and
    /// batch through the OS page cache.  Without an fsync, the redb commit
    /// pointer is not guaranteed to be on disk when the process exits — a
    /// SIGTERM that skips destructors can leave the database appearing
    /// empty on reopen.  Call this periodically during long-running writes
    /// (e.g. every N blocks in the sync loop) and on graceful shutdown to
    /// bound worst-case data loss to the interval between flushes.
    ///
    /// Implemented as an empty write transaction committed with
    /// `Durability::Immediate`.  A redb commit with Immediate durability
    /// fsyncs all outstanding data and the metadata pointer, including
    /// prior `Durability::None` commits still held in the page cache.
    pub fn flush(&self) -> Result<()> {
        let mut write_txn = self.db.begin_write()?;
        write_txn.set_quick_repair(true);
        write_txn.set_durability(Durability::Immediate)?;
        // Write a sentinel key so the fsync has real pages to flush.
        // An empty commit with shadow-paging may not dirty any pages,
        // leaving prior Durability::None commits unreachable by a
        // new Database handle opened against the same file.
        {
            let mut meta = write_txn.open_table(META_TABLE)?;
            let val = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            meta.insert(META_FLUSH_SENTINEL, val.to_be_bytes().as_slice())?;
        }
        write_txn.commit().context("flush commit failed")?;
        Ok(())
    }
}

// ── SnapshotReader ───────────────────────────────────────────────────

impl SnapshotReader {
    /// Dump the AVL+ tree as a snapshot manifest + chunks.
    ///
    /// Opens a single read transaction for consistency. Walks the tree in
    /// pre-order DFS, serializing nodes into manifest bytes (root to
    /// `manifest_depth`) and chunk bytes (each subtree below the boundary).
    ///
    /// Returns `None` if the tree is empty (no root state).
    pub fn dump_snapshot(&self, manifest_depth: u8) -> Result<Option<SnapshotDump>> {
        let read_txn = self.db.begin_read()?;
        let nodes_table = read_txn.open_table(NODES_TABLE)?;
        let meta_table = read_txn.open_table(META_TABLE)?;

        // Read root hash from metadata.
        let root_hash: [u8; 32] = match meta_table.get(META_TOP_NODE_HASH)? {
            Some(v) => {
                let bytes: &[u8] = v.value();
                let mut h = [0u8; 32];
                h.copy_from_slice(bytes);
                h
            }
            None => return Ok(None),
        };

        // Read tree height from metadata.
        let tree_height = match meta_table.get(META_TOP_NODE_HEIGHT)? {
            Some(v) => {
                let bytes: &[u8] = v.value();
                let h = u32::from_be_bytes(bytes.try_into().context("bad height bytes")?);
                h as u8
            }
            None => return Ok(None),
        };

        // Manifest header: [tree_height, manifest_depth].
        let mut manifest = Vec::new();
        manifest.push(tree_height);
        manifest.push(manifest_depth);

        // Collect subtree root labels at the manifest boundary.
        let mut subtree_roots: Vec<[u8; 32]> = Vec::new();

        // Pre-order DFS for manifest — level starts at 1 (JVM convention).
        self.walk_manifest(
            &nodes_table,
            &root_hash,
            1,
            manifest_depth,
            &mut manifest,
            &mut subtree_roots,
        )?;

        // Serialize chunks: full DFS from each subtree root.
        let mut chunks = Vec::with_capacity(subtree_roots.len());
        for subtree_label in &subtree_roots {
            let mut chunk_buf = Vec::new();
            self.walk_chunk(&nodes_table, subtree_label, &mut chunk_buf)?;
            chunks.push((*subtree_label, chunk_buf));
        }

        Ok(Some(SnapshotDump {
            root_hash,
            tree_height,
            manifest,
            chunks,
        }))
    }

    /// Recursive manifest DFS. Appends packed bytes to `manifest`.
    /// At boundary depth, records child labels as subtree roots.
    fn walk_manifest(
        &self,
        table: &redb::ReadOnlyTable<&[u8], &[u8]>,
        label: &[u8; 32],
        level: u8,
        manifest_depth: u8,
        manifest: &mut Vec<u8>,
        subtree_roots: &mut Vec<[u8; 32]>,
    ) -> Result<()> {
        let packed = table
            .get(label.as_slice())?
            .with_context(|| format!("manifest: node {:02x?} not found", label))?;
        let packed_bytes = packed.value();
        manifest.extend_from_slice(packed_bytes);

        let node_type = packed_bytes[0];

        // Leaf (0x01): no children, stop.
        if node_type == 0x01 {
            return Ok(());
        }

        // Internal (0x00): extract child labels.
        debug_assert_eq!(node_type, 0x00, "unexpected node type byte");
        let (left_label, right_label) = self.extract_child_labels(packed_bytes)?;

        if level == manifest_depth {
            // Boundary: record children as subtree roots, don't recurse.
            subtree_roots.push(left_label);
            subtree_roots.push(right_label);
        } else {
            // level < manifest_depth: recurse.
            self.walk_manifest(table, &left_label, level + 1, manifest_depth, manifest, subtree_roots)?;
            self.walk_manifest(table, &right_label, level + 1, manifest_depth, manifest, subtree_roots)?;
        }

        Ok(())
    }

    /// Recursive chunk DFS. Walks the full subtree to all leaves.
    fn walk_chunk(
        &self,
        table: &redb::ReadOnlyTable<&[u8], &[u8]>,
        label: &[u8; 32],
        buf: &mut Vec<u8>,
    ) -> Result<()> {
        let packed = table
            .get(label.as_slice())?
            .with_context(|| format!("chunk: node {:02x?} not found", label))?;
        let packed_bytes = packed.value();
        buf.extend_from_slice(packed_bytes);

        let node_type = packed_bytes[0];

        // Leaf: stop.
        if node_type == 0x01 {
            return Ok(());
        }

        // Internal: recurse into children.
        let (left_label, right_label) = self.extract_child_labels(packed_bytes)?;
        drop(packed);
        self.walk_chunk(table, &left_label, buf)?;
        self.walk_chunk(table, &right_label, buf)
    }

    /// Look up a key in the AVL+ tree, returning the value bytes if found.
    ///
    /// Navigates from root to leaf by comparing keys at internal nodes.
    /// Read-only — no tree modification.
    pub fn lookup_key(&self, key: &[u8; 32]) -> Option<Vec<u8>> {
        let read_txn = self.db.begin_read().ok()?;
        let nodes_table = read_txn.open_table(NODES_TABLE).ok()?;
        let meta_table = read_txn.open_table(META_TABLE).ok()?;

        // Get root label
        let root_guard = meta_table.get(META_TOP_NODE_HASH).ok()??;
        let root_bytes: &[u8] = root_guard.value();
        let mut current_label = [0u8; 32];
        current_label.copy_from_slice(root_bytes);
        drop(root_guard);

        loop {
            let packed_guard = nodes_table.get(current_label.as_slice()).ok()??;
            let packed = packed_guard.value();
            if packed.is_empty() {
                warn!("corrupt node: empty packed data");
                return None;
            }
            let node_type = packed[0];

            if node_type == 0x01 {
                // Leaf: key starts at byte 1
                let leaf_end = 1 + self.key_length;
                if packed.len() < leaf_end {
                    warn!("corrupt leaf node: truncated key");
                    return None;
                }
                let leaf_key = &packed[1..leaf_end];
                if leaf_key == key.as_slice() {
                    // value_length is u32 BE (variable-length mode)
                    let vlen_offset = leaf_end;
                    if packed.len() < vlen_offset + 4 {
                        warn!("corrupt leaf node: truncated value length");
                        return None;
                    }
                    let vlen = u32::from_be_bytes(
                        packed[vlen_offset..vlen_offset + 4].try_into().ok()?,
                    ) as usize;
                    if packed.len() < vlen_offset + 4 + vlen {
                        warn!("corrupt leaf node: truncated value");
                        return None;
                    }
                    let value = packed[vlen_offset + 4..vlen_offset + 4 + vlen].to_vec();
                    return Some(value);
                }
                return None;
            }

            // Internal: key at bytes [2..2+key_length], children after
            let child_offset = 2 + self.key_length;
            if packed.len() < child_offset + 64 {
                warn!("corrupt internal node: truncated child labels");
                return None;
            }
            let node_key = &packed[2..child_offset];
            if key.as_slice() < node_key {
                // Go left
                current_label.copy_from_slice(&packed[child_offset..child_offset + 32]);
            } else {
                // Go right (key >= node_key)
                current_label.copy_from_slice(&packed[child_offset + 32..child_offset + 64]);
            }
            drop(packed_guard);
        }
    }

    /// Extract left and right child labels from an internal node's packed bytes.
    /// Format: 0x00 | balance: i8 | key: key_length | left_label: 32B | right_label: 32B
    fn extract_child_labels(&self, packed: &[u8]) -> Result<([u8; 32], [u8; 32])> {
        let offset = 2 + self.key_length; // skip type byte + balance byte + key
        let required = offset + 64;
        if packed.len() < required {
            bail!(
                "corrupt internal node: need {} bytes, got {}",
                required,
                packed.len()
            );
        }
        let mut left = [0u8; 32];
        let mut right = [0u8; 32];
        left.copy_from_slice(&packed[offset..offset + 32]);
        right.copy_from_slice(&packed[offset + 32..offset + 64]);
        Ok((left, right))
    }
}

impl RedbAVLStorage {
    /// Shared implementation behind the trait `update()` and the
    /// inherent `update_with_height()`.
    ///
    /// `block_height == None` ⇒ preserve whatever block height is
    /// already in the metadata table (write it back explicitly so
    /// block_height() never returns None for a non-empty storage).
    /// `Some(h)` ⇒ replace it with `h`.  Either way, block_height is
    /// committed in the same redb transaction as the state nodes and
    /// metadata, so crash recovery can't leave them out of sync.
    fn update_internal(
        &mut self,
        prover: &mut BatchAVLProver,
        additional_data: Vec<(ADKey, ADValue)>,
        block_height: Option<u32>,
    ) -> Result<()> {
        // 1. Compute new digest.
        let new_digest = prover.digest().context("prover has no root")?;
        let root = prover.top_node();

        // 2. Walk the prover's tree for new/modified nodes.
        let mut changed_nodes = Vec::new();
        Self::collect_changed_nodes(&prover.base.tree, &root, true, &mut changed_nodes);

        // 3. Removed nodes — labels of nodes that left the tree.
        let removed_ids = prover.removed_nodes();
        let removed_labels: Vec<Digest32> =
            removed_ids.iter().map(|n| n.borrow_mut().label()).collect();

        // 4. Snapshot metadata we'll need inside the transaction.
        let new_root_label = prover.base.tree.label(&root);
        let new_height = prover.base.tree.height as u32;
        let new_lsn = self.current_lsn() + 1;

        // Pre-compute new version chain (applied after commit).
        let mut new_chain = self.version_chain.clone();
        new_chain.push_front((new_lsn, new_digest.clone()));

        // Determine pruning.
        let max_chain_len = if self.keep_versions > 0 {
            self.keep_versions as usize + 1
        } else {
            1
        };
        let mut prune_lsns = Vec::new();
        while new_chain.len() > max_chain_len {
            if let Some((old_lsn, _)) = new_chain.pop_back() {
                prune_lsns.push(old_lsn);
            }
        }

        // 5. Single write transaction — atomic or nothing.
        //    Skip fsync per commit — the OS page cache batches writes.
        //    On crash the sync layer re-applies missing blocks.
        let mut write_txn = self.db.begin_write()?;
        write_txn.set_quick_repair(true);
        write_txn.set_durability(Durability::None)?;
        {
            let mut nodes_table = write_txn.open_table(NODES_TABLE)?;
            let mut meta_table = write_txn.open_table(META_TABLE)?;

            // Read pre-update block_height — used for the undo record and,
            // when the caller didn't pass a new one, as the value to write
            // back (preserves the invariant that a non-empty storage always
            // has a block_height).
            let prev_block_height = match meta_table.get(META_BLOCK_HEIGHT)? {
                Some(v) => u32::from_be_bytes(
                    v.value()
                        .try_into()
                        .context("corrupt META_BLOCK_HEIGHT: expected 4 bytes")?,
                ),
                None => 0,
            };
            let new_block_height = block_height.unwrap_or(prev_block_height);

            // Build + write undo record.
            if self.keep_versions > 0 {
                let mut undo_table = write_txn.open_table(UNDO_TABLE)?;

                // Read old packed bytes for removed nodes (for the undo record).
                let mut removed_with_bytes = Vec::with_capacity(removed_labels.len());
                for label in &removed_labels {
                    if let Some(data) = nodes_table.get(label.as_slice())? {
                        removed_with_bytes
                            .push((*label, Bytes::copy_from_slice(data.value())));
                    }
                }

                let inserted_labels: Vec<Digest32> =
                    changed_nodes.iter().map(|(label, _)| *label).collect();

                let prev_top_node_hash = match meta_table.get(META_TOP_NODE_HASH)? {
                    Some(v) => {
                        let mut h: Digest32 = [0u8; 32];
                        h.copy_from_slice(v.value());
                        h
                    }
                    None => [0u8; 32],
                };
                let prev_top_node_height = match meta_table.get(META_TOP_NODE_HEIGHT)? {
                    Some(v) => u32::from_be_bytes(
                        v.value()
                            .try_into()
                            .context("corrupt META_TOP_NODE_HEIGHT: expected 4 bytes")?,
                    ),
                    None => 0,
                };
                let prev_version = self.current_version.clone().unwrap_or_default();

                let undo = UndoRecord {
                    removed_nodes: removed_with_bytes,
                    inserted_labels,
                    prev_top_node_hash,
                    prev_top_node_height,
                    prev_version,
                    prev_block_height,
                };
                let undo_bytes = undo.serialize();
                undo_table.insert(new_lsn, undo_bytes.as_slice())?;

                // Prune old undo records.
                for lsn in &prune_lsns {
                    undo_table.remove(*lsn)?;
                }
            }

            // 6. Write new/modified nodes.  Track labels we just wrote so
            //    the delete loop can refuse to remove them — see the
            //    overlap guard at step 7 for the reasoning.
            let mut written_labels: HashSet<Digest32> =
                HashSet::with_capacity(changed_nodes.len());
            for (label, packed) in &changed_nodes {
                nodes_table.insert(label.as_slice(), packed.as_ref())?;
                written_labels.insert(*label);
            }

            // 7. Delete removed nodes.  If a label appears in both
            //    `removed_labels` and `changed_nodes` (a stale entry in the
            //    prover's `changed_nodes_buffer*` whose digest matches a
            //    freshly-written node), removing it here would silently
            //    destroy the node we just wrote.  Subsequent traversals
            //    would then panic in the prover with "Should never reach
            //    this point" because a parent references a digest that's
            //    missing from NODES_TABLE.  Skipping the delete leaves at
            //    worst an orphan in storage (harmless: never re-referenced
            //    if truly orphan) and protects against the v0.4.x at-tip
            //    state corruption.
            let mut skipped_overlapping = 0u32;
            for label in &removed_labels {
                if written_labels.contains(label) {
                    skipped_overlapping += 1;
                    warn!(
                        label = %digest_hex(label),
                        block_height = new_block_height,
                        "skipping deletion: digest also in changed_nodes (would destroy freshly-written node)"
                    );
                    continue;
                }
                nodes_table.remove(label.as_slice())?;
            }

            if skipped_overlapping > 0 {
                info!(
                    removed_labels = removed_labels.len(),
                    skipped_overlapping,
                    block_height = new_block_height,
                    "update_internal completed with overlap"
                );
            }

            // 8. Store additional data.
            for (key, value) in &additional_data {
                nodes_table.insert(key.as_ref(), value.as_ref())?;
            }

            // 9. Update metadata.
            meta_table.insert(META_TOP_NODE_HASH, new_root_label.as_slice())?;
            meta_table.insert(META_TOP_NODE_HEIGHT, new_height.to_be_bytes().as_slice())?;
            meta_table.insert(META_CURRENT_VERSION, new_digest.as_ref())?;
            meta_table.insert(META_LSN, new_lsn.to_be_bytes().as_slice())?;
            meta_table.insert(META_BLOCK_HEIGHT, new_block_height.to_be_bytes().as_slice())?;

            let chain_bytes = Self::serialize_version_chain(&new_chain);
            meta_table.insert(META_VERSIONS, chain_bytes.as_slice())?;
        }

        // 11. Commit.
        write_txn.commit()?;

        // Update in-memory state only after successful commit.
        self.current_version = Some(new_digest);
        self.version_chain = new_chain;

        // Reset the prover's dirty-node bookkeeping. In UTXO mode we never
        // call generate_proof on the main prover, so without this the
        // is_new/visited flags accumulate forever. collect_changed_nodes
        // treats is_new|visited as "changed since last flush", so stale
        // flags cause inserted_labels in every undo record to include the
        // ENTIRE live tree — a single rollback then deletes everything.
        prover.base.tree.reset();
        prover.base.changed_nodes_buffer.clear();
        prover.base.changed_nodes_buffer_to_check.clear();

        Ok(())
    }
}

// ── VersionedAVLStorage ───────────────────────────────────────────────

impl VersionedAVLStorage for RedbAVLStorage {
    fn update(
        &mut self,
        prover: &mut BatchAVLProver,
        additional_data: Vec<(ADKey, ADValue)>,
    ) -> Result<()> {
        self.update_internal(prover, additional_data, None)
    }

    fn rollback(&mut self, version: &ADDigest) -> Result<(NodeId, usize)> {
        // Short-circuit: if target equals current version, just return the
        // current root.  PersistentBatchAVLProver::new() does this after
        // load_snapshot() sets a single version.
        if self.current_version.as_ref() == Some(version) {
            let (root_hash, height) = self
                .root_state()
                .context("no root state for current version")?;
            let node_bytes = self
                .get_node(&root_hash)?
                .context("root node not found in storage")?;
            let tree = self.make_tree();
            let root_node = tree.unpack(&node_bytes);
            return Ok((root_node, height));
        }

        // Find target in the version chain.
        let target_pos = self
            .version_chain
            .iter()
            .position(|(_, d)| d == version)
            .context("version not found in rollback targets")?;

        let mut write_txn = self.db.begin_write()?;
        write_txn.set_quick_repair(true);
        let mut last_undo: Option<UndoRecord> = None;
        let mut skipped_deletions: usize = 0;

        {
            let mut nodes_table = write_txn.open_table(NODES_TABLE)?;
            let mut undo_table = write_txn.open_table(UNDO_TABLE)?;
            let mut meta_table = write_txn.open_table(META_TABLE)?;

            // Process undo records from newest towards the target.
            for i in 0..target_pos {
                let (lsn, _) = self.version_chain[i];

                let undo_data = undo_table
                    .get(lsn)?
                    .with_context(|| format!("missing undo record for LSN {}", lsn))?;
                let undo = UndoRecord::deserialize(undo_data.value())?;
                drop(undo_data);

                // Reverse: delete nodes that were inserted.
                //
                // SKIPPED — see contains() fix in ergo_avltree_rust commit
                // 879545c for the symmetric forward-path fix. Deleting an
                // inserted_label here is unsafe when that label is still
                // referenced from either (a) the rolled-back-to state's
                // tree, or (b) older versions still in the chain. Net
                // effect of skipping: orphan nodes accumulate in
                // NODES_TABLE; the tree on disk stays consistent. Periodic
                // offline mark-and-sweep can reclaim space if needed.
                skipped_deletions += undo.inserted_labels.len();

                // Reverse: re-insert nodes that were removed.
                for (label, packed) in &undo.removed_nodes {
                    nodes_table.insert(label.as_slice(), packed.as_ref())?;
                }

                // Delete the undo record itself (rollback is not reversible).
                undo_table.remove(lsn)?;

                last_undo = Some(undo);
            }

            // Restore metadata from the last processed undo record.
            let undo = last_undo.as_ref().unwrap();
            meta_table
                .insert(META_TOP_NODE_HASH, undo.prev_top_node_hash.as_slice())?;
            meta_table.insert(
                META_TOP_NODE_HEIGHT,
                undo.prev_top_node_height.to_be_bytes().as_slice(),
            )?;
            meta_table.insert(META_CURRENT_VERSION, undo.prev_version.as_ref())?;
            meta_table.insert(
                META_BLOCK_HEIGHT,
                undo.prev_block_height.to_be_bytes().as_slice(),
            )?;

            let (target_lsn, _) = self.version_chain[target_pos];
            meta_table.insert(META_LSN, target_lsn.to_be_bytes().as_slice())?;

            // Trim the version chain.
            let mut new_chain = self.version_chain.clone();
            for _ in 0..target_pos {
                new_chain.pop_front();
            }
            let chain_bytes = Self::serialize_version_chain(&new_chain);
            meta_table.insert(META_VERSIONS, chain_bytes.as_slice())?;
        }

        write_txn.commit()?;

        // Update in-memory state after commit.
        for _ in 0..target_pos {
            self.version_chain.pop_front();
        }
        self.current_version = Some(version.clone());

        // Unpack root node from storage.
        let undo = last_undo.unwrap();
        let root_hash = undo.prev_top_node_hash;
        let height = undo.prev_top_node_height as usize;

        let node_bytes = self
            .get_node(&root_hash)?
            .with_context(|| "root node not found in storage after rollback")?;

        let tree = self.make_tree();
        let root_node = tree.unpack(&node_bytes);

        let mut version_hex = String::with_capacity(version.len() * 2);
        for b in version.as_ref() {
            let _ = write!(&mut version_hex, "{:02x}", b);
        }
        info!(
            target_version = %version_hex,
            target_position = target_pos,
            skipped_deletions,
            "rollback: skipped node deletions to preserve cross-version references"
        );
        debug!(height, "rollback complete");
        Ok((root_node, height))
    }

    fn version(&self) -> Option<ADDigest> {
        self.current_version.clone()
    }

    fn rollback_versions<'a>(&'a self) -> Box<dyn Iterator<Item = ADDigest> + 'a> {
        Box::new(
            self.version_chain
                .iter()
                .skip(1) // skip current version
                .map(|(_, digest)| digest.clone()),
        )
    }

    fn flush(&self) -> Result<()> {
        RedbAVLStorage::flush(self)
    }
}

// ── Offline compaction ────────────────────────────────────────────────

/// First byte of an internal node's packed form.
const PACKED_INTERNAL_PREFIX: u8 = 0x00;
/// First byte of a leaf node's packed form.
const PACKED_LEAF_PREFIX: u8 = 0x01;
/// An internal node's two child labels always occupy the final 64 bytes of
/// its packed form, whatever the key length.  The copy pass relies on that
/// to walk the tree without knowing the tree's parameters.
const INTERNAL_CHILD_LABELS_LEN: usize = 64;
/// The AVL digest encodes tree height in a single byte, so no well-formed
/// tree is deeper than this.  Used as a traversal depth cap: a corrupt file
/// whose child labels form a cycle would otherwise walk forever.
const MAX_TREE_DEPTH: usize = 255;
/// Upper bound for the key-length search on a single-leaf tree.
const MAX_KEY_LENGTH: usize = 64;
/// redb page cache for the handles compaction opens, split 9:1 by redb
/// between its read and write caches.
///
/// Pinning this is what makes peak memory a constant.  The traversal is
/// O(tree depth) on its own, but redb's 1 GiB-per-handle default lets the
/// caches fill to whatever the database offers, and peak RSS then tracks
/// file size rather than tree depth.  Measured on a synthetic 800k-node
/// tree (1.08 GB source, 269 MB destination): 32 MB cache → 39 MB peak in
/// 16.5 s, 256 MB → 275 MB in 10.0 s, 1 GiB → 414 MB in 9.6 s.  256 MB is
/// the knee — a further 4x of cache buys 4% of throughput.
const COMPACTION_CACHE_BYTES: usize = 256 * 1024 * 1024;
/// Nodes between progress callbacks during the copy pass.
const COMPACTION_PROGRESS_INTERVAL: u64 = 100_000;
/// Nodes between `info!` ticks during the verification pass.  The contract's
/// progress callback covers the copy pass only, and verification of a
/// mainnet-sized tree is long enough that silence looks like a hang.
const COMPACTION_VERIFY_LOG_INTERVAL: u64 = 1_000_000;

/// Shape of a packed node, with an internal node's child labels extracted.
enum PackedShape {
    Internal { left: Digest32, right: Digest32 },
    Leaf,
}

/// Validate a packed node against `key_length` and pull out its child labels.
///
/// `AVLTree::unpack` panics on short or malformed input; every call site here
/// runs this check first so a damaged database produces an error instead of
/// aborting the process.
///
/// Assumes variable-length values (`value_length: None`) — the only mode this
/// crate is used in.  A fixed-length database fails the length arithmetic and
/// aborts the compaction; it cannot be silently mis-verified.
fn inspect_packed(packed: &[u8], key_length: usize) -> Result<PackedShape> {
    match packed.first() {
        Some(&PACKED_INTERNAL_PREFIX) => {
            // [0x00 | balance: i8 | key | left_label: 32B | right_label: 32B]
            let expected = 2 + key_length + INTERNAL_CHILD_LABELS_LEN;
            if packed.len() != expected {
                bail!(
                    "internal node is {} bytes, expected {} for a {}-byte key",
                    packed.len(),
                    expected,
                    key_length
                );
            }
            let off = 2 + key_length;
            let mut left: Digest32 = [0u8; 32];
            let mut right: Digest32 = [0u8; 32];
            left.copy_from_slice(&packed[off..off + 32]);
            right.copy_from_slice(&packed[off + 32..off + 64]);
            Ok(PackedShape::Internal { left, right })
        }
        Some(&PACKED_LEAF_PREFIX) => {
            // [0x01 | key | value_len: u32 BE | value | next_key]
            let vlen_off = 1 + key_length;
            if packed.len() < vlen_off + 4 + key_length {
                bail!(
                    "leaf node is {} bytes, too short for a {}-byte key",
                    packed.len(),
                    key_length
                );
            }
            let vlen = u32::from_be_bytes(packed[vlen_off..vlen_off + 4].try_into()?) as usize;
            let expected = vlen_off + 4 + vlen + key_length;
            if packed.len() != expected {
                bail!(
                    "leaf node is {} bytes, expected {} for a {}-byte value",
                    packed.len(),
                    expected,
                    vlen
                );
            }
            Ok(PackedShape::Leaf)
        }
        Some(other) => bail!("unknown packed node prefix 0x{:02x}", other),
        None => bail!("empty packed node"),
    }
}

/// Pack/unpack-only tree, used to recompute labels during verification.
///
/// The resolver is never called — `unpack` builds `LabelOnly` children
/// directly from the packed bytes and `label()` reads their labels straight
/// out of the node header — but it returns a labelled placeholder rather
/// than panicking, per this crate's no-panic invariant.
fn compaction_tree(key_length: usize) -> AVLTree {
    let resolver: Resolver =
        Arc::new(|digest: &Digest32| Node::LabelOnly(NodeHeader::new(Some(*digest), None)));
    AVLTree::with_resolver(resolver, key_length, None)
}

/// Recompute a packed node's label using the crate's own hashing.
///
/// Internal nodes hash `balance` plus their two child labels; leaves hash
/// key, value and next-key.  `unpack` leaves the header label unset, so
/// `label()` derives it from the bytes rather than returning a cached value —
/// which is what makes the comparison against the stored key meaningful.
fn recompute_label(tree: &AVLTree, packed: &[u8]) -> Digest32 {
    tree.label(&tree.unpack(&Bytes::copy_from_slice(packed)))
}

/// Recover the tree's key length from the root node's packed bytes.
///
/// `compact_to` takes only two paths and a callback, so it has no
/// `AVLTreeParams` to work from.  Every candidate is confirmed by
/// recomputing the root label and comparing it against the label the source
/// metadata stores the root under, so a wrong guess is rejected rather than
/// accepted — the operation fails closed.
fn derive_key_length(root_packed: &[u8], expected_root: &Digest32) -> Result<usize> {
    let candidates: Vec<usize> = match root_packed.first() {
        Some(&PACKED_INTERNAL_PREFIX) => {
            // 1 + 1 + key + 32 + 32 — the key length falls straight out.
            let fixed = 2 + INTERNAL_CHILD_LABELS_LEN;
            if root_packed.len() <= fixed {
                bail!(
                    "corrupt root: internal node is {} bytes, needs more than {}",
                    root_packed.len(),
                    fixed
                );
            }
            vec![root_packed.len() - fixed]
        }
        // Single-leaf tree: key length and value length are two unknowns in
        // one length equation, so try every key length the arithmetic
        // permits and let the label decide.
        Some(&PACKED_LEAF_PREFIX) => (1..=MAX_KEY_LENGTH)
            .filter(|k| inspect_packed(root_packed, *k).is_ok())
            .collect(),
        Some(other) => bail!("corrupt root: unknown packed node prefix 0x{:02x}", other),
        None => bail!("corrupt root: empty packed bytes"),
    };

    for key_length in candidates {
        if inspect_packed(root_packed, key_length).is_err() {
            continue;
        }
        if recompute_label(&compaction_tree(key_length), root_packed) == *expected_root {
            return Ok(key_length);
        }
    }

    bail!(
        "verification failed: the root node's bytes do not hash to {} under any key length",
        digest_hex(expected_root)
    )
}

/// Open `source` with a shared lock and no write capability.
///
/// The error mapping is deliberately operator-facing: a locked file means
/// the node is still running, and an unclean shutdown means redb has no
/// saved allocator state to load and cannot rebuild it through a read-only
/// handle.  Neither should surface as a raw redb error.
fn open_source_read_only(source: &Path) -> Result<ReadOnlyDatabase> {
    match redb::Builder::new()
        .set_cache_size(COMPACTION_CACHE_BYTES)
        .open_read_only(source)
    {
        Ok(db) => Ok(db),
        Err(DatabaseError::DatabaseAlreadyOpen) => bail!(
            "{} is locked by another process — stop the node first, then run compaction",
            source.display()
        ),
        Err(DatabaseError::RepairAborted) => bail!(
            "{} was not shut down cleanly and needs repair, which compaction cannot perform \
             without writing to it — start the node once and stop it gracefully, then retry",
            source.display()
        ),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("failed to open {} read-only", source.display()))),
    }
}

/// Walk `dest` from `expected_root`, recomputing every node's label from its
/// own stored bytes and checking it against the label the parent — or, at the
/// root, the metadata — references.
///
/// A label is Blake2b256 over the node's contents *including its children's
/// labels*, so a match at the root, established inductively down every
/// branch, proves that each reachable node was copied byte-exactly and that
/// none went missing.  That is what makes the rewrite provably lossless
/// rather than merely plausible; a shallow root-only check would prove
/// nothing about the other twenty million rows.
///
/// Returns the recomputed root label.
fn verify_compacted(dest: &Path, expected_root: &Digest32, expected_nodes: u64) -> Result<Digest32> {
    let db = redb::Builder::new()
        .set_cache_size(COMPACTION_CACHE_BYTES)
        .open_read_only(dest)
        .map_err(anyhow::Error::new)
        .context("failed to reopen the compacted database for verification")?;
    let read_txn = db.begin_read()?;
    let nodes = read_txn.open_table(NODES_TABLE)?;

    let key_length = {
        let guard = nodes
            .get(expected_root.as_slice())?
            .context("verification failed: the root node is missing from the compacted database")?;
        derive_key_length(guard.value(), expected_root)?
    };
    let tree = compaction_tree(key_length);

    let mut root_label: Option<Digest32> = None;
    let mut visited: u64 = 0;
    let mut next_log = COMPACTION_VERIFY_LOG_INTERVAL;
    let mut stack: Vec<(Digest32, usize)> = vec![(*expected_root, 0)];

    while let Some((label, depth)) = stack.pop() {
        if depth > MAX_TREE_DEPTH {
            bail!(
                "verification failed: traversal passed {} levels at node {}",
                MAX_TREE_DEPTH,
                digest_hex(&label)
            );
        }

        let guard = nodes.get(label.as_slice())?.with_context(|| {
            format!(
                "verification failed: node {} is missing from the compacted database",
                digest_hex(&label)
            )
        })?;
        let packed: &[u8] = guard.value();

        let shape = inspect_packed(packed, key_length).with_context(|| {
            format!(
                "verification failed: node {} is malformed",
                digest_hex(&label)
            )
        })?;

        let computed = recompute_label(&tree, packed);
        if computed != label {
            // The check cannot tell a bad copy from an already-damaged
            // source, and does not need to: either way the result must not
            // be presented as a compacted database.
            bail!(
                "verification failed: the node stored under {} hashes to {} — the source is \
                 corrupt, or the copy was not byte-exact",
                digest_hex(&label),
                digest_hex(&computed)
            );
        }
        drop(guard);

        if depth == 0 {
            root_label = Some(computed);
        }
        visited += 1;
        if visited >= next_log {
            info!(verified = visited, "compaction: verification in progress");
            next_log += COMPACTION_VERIFY_LOG_INTERVAL;
        }

        if let PackedShape::Internal { left, right } = shape {
            stack.push((right, depth + 1));
            stack.push((left, depth + 1));
        }
    }

    if visited != expected_nodes {
        bail!(
            "verification failed: the compacted database holds {} reachable nodes, but {} were \
             written",
            visited,
            expected_nodes
        );
    }

    // Postcondition: dest contains *exactly* the reachable nodes.  Anything
    // extra would mean the rewrite carried bloat across.
    let total_rows = nodes.len()?;
    if total_rows != visited {
        bail!(
            "verification failed: the compacted database holds {} rows but only {} are reachable",
            total_rows,
            visited
        );
    }

    root_label.context("verification failed: traversal visited no nodes")
}

impl RedbAVLStorage {
    /// Rewrite the reachable tree from `source` into a fresh database at
    /// `dest`, discarding rows that no longer belong to the live tree.
    ///
    /// Two mechanisms leave unreachable rows behind: the rollback path skips
    /// deleting an undo record's inserted labels (they may still be
    /// referenced), and node v0.7.5–v0.7.9 shipped a call-order bug that
    /// leaked every superseded node.  Neither corrupts the tree — traversals
    /// only ever follow labels from a root — so this reclaims space, it does
    /// not repair anything.
    ///
    /// A depth-first walk of the current root visits every live node exactly
    /// once (each has exactly one parent), so no visited set is needed and
    /// traversal memory is O(tree depth).  The destination is built fresh
    /// from only those nodes, which makes it minimal by construction —
    /// `redb::Database::compact()` is neither used nor required.
    ///
    /// The rollback window is not carried over: `dest` holds a single
    /// version and an empty `undo` table.  Retaining the window would mean
    /// walking every retained root, which reintroduces the visited set this
    /// design exists to avoid.
    ///
    /// `source` is opened read-only and is never written to.  On any error
    /// `dest` is removed.  Returning `Ok` requires the verification pass to
    /// pass, so a missing or altered node cannot be laundered into a
    /// "compacted" database.
    ///
    /// The tree's key length is recovered from the root node's packed bytes
    /// and confirmed cryptographically; values are assumed variable-length,
    /// which is the only mode this crate is used in.  A database that
    /// violates that assumption fails verification rather than compacting
    /// incorrectly.
    ///
    /// Rows in `nodes` that are not tree nodes — the `additional_data`
    /// entries `update()` accepts — are not reachable from the root and are
    /// therefore dropped.  No caller in this workspace passes any.
    pub fn compact_to(
        source: &Path,
        dest: &Path,
        progress: Option<&dyn Fn(CompactionProgress)>,
    ) -> Result<CompactionStats> {
        if dest.exists() {
            bail!(
                "compaction destination {} already exists — refusing to overwrite it",
                dest.display()
            );
        }

        match Self::compact_to_inner(source, dest, progress) {
            Ok(stats) => Ok(stats),
            Err(e) => {
                // Every handle on dest is owned by compact_to_inner and has
                // been dropped by the time we get here, so the file can go.
                if dest.exists() {
                    if let Err(rm) = std::fs::remove_file(dest) {
                        error!(
                            path = %dest.display(),
                            error = %rm,
                            "failed to remove the partial compaction output — delete it manually"
                        );
                    }
                }
                Err(e)
            }
        }
    }

    fn compact_to_inner(
        source: &Path,
        dest: &Path,
        progress: Option<&dyn Fn(CompactionProgress)>,
    ) -> Result<CompactionStats> {
        let source_bytes = std::fs::metadata(source)
            .with_context(|| format!("cannot stat compaction source {}", source.display()))?
            .len();

        let source_db = open_source_read_only(source)?;

        // 1. Read the source's root state.  An empty database is an error,
        //    not a no-op — there is nothing to compact and silently
        //    producing an empty dest would be worse than saying so.
        let source_read = source_db.begin_read()?;
        let (top_node_hash, top_node_height, current_version, block_height, lsn) = {
            let meta = source_read
                .open_table(META_TABLE)
                .context("source is not an enr-state database (no `metadata` table)")?;

            let hash_guard = meta.get(META_TOP_NODE_HASH)?.context(
                "source database has no top_node_hash — it is empty or uninitialised, \
                 nothing to compact",
            )?;
            let hash_bytes: &[u8] = hash_guard.value();
            if hash_bytes.len() != 32 {
                bail!(
                    "corrupt source: top_node_hash is {} bytes, expected 32",
                    hash_bytes.len()
                );
            }
            let mut top_node_hash: Digest32 = [0u8; 32];
            top_node_hash.copy_from_slice(hash_bytes);
            drop(hash_guard);

            let height_guard = meta
                .get(META_TOP_NODE_HEIGHT)?
                .context("corrupt source: top_node_hash is set but top_node_height is missing")?;
            let top_node_height = u32::from_be_bytes(
                height_guard
                    .value()
                    .try_into()
                    .context("corrupt source: top_node_height is not 4 bytes")?,
            );
            drop(height_guard);

            let version_guard = meta.get(META_CURRENT_VERSION)?.context(
                "source database has no current_version — it is empty or uninitialised, \
                 nothing to compact",
            )?;
            let current_version: ADDigest = Bytes::copy_from_slice(version_guard.value());
            drop(version_guard);

            let block_height = match meta.get(META_BLOCK_HEIGHT)? {
                Some(v) => u32::from_be_bytes(
                    v.value()
                        .try_into()
                        .context("corrupt source: block_height is not 4 bytes")?,
                ),
                None => 0,
            };
            let lsn = match meta.get(META_LSN)? {
                Some(v) => u64::from_be_bytes(
                    v.value()
                        .try_into()
                        .context("corrupt source: lsn is not 8 bytes")?,
                ),
                None => 1,
            };

            (
                top_node_hash,
                top_node_height,
                current_version,
                block_height,
                lsn,
            )
        };

        info!(
            source = %source.display(),
            dest = %dest.display(),
            block_height,
            root = %digest_hex(&top_node_hash),
            "compaction: copying the reachable tree"
        );

        // 2. Copy pass.  One read transaction over the source for a
        //    consistent view, one write transaction over the destination so
        //    a failure leaves nothing behind.  redb returns pages freed
        //    within a transaction straight to its allocator, so the single
        //    large transaction does not accumulate superseded B-tree pages.
        let dest_db = Database::builder()
            .set_cache_size(COMPACTION_CACHE_BYTES)
            .create(dest)
            .with_context(|| {
                format!("failed to create compaction destination {}", dest.display())
            })?;

        let mut nodes_written: u64 = 0;
        {
            let src_nodes = source_read
                .open_table(NODES_TABLE)
                .context("source is not an enr-state database (no `nodes` table)")?;

            let mut write_txn = dest_db.begin_write()?;
            write_txn.set_quick_repair(true);
            write_txn.set_durability(Durability::Immediate)?;
            {
                let mut dst_nodes = write_txn.open_table(NODES_TABLE)?;
                let mut dst_meta = write_txn.open_table(META_TABLE)?;
                // Create `undo` so the destination has the same table shape
                // as a normally-created database.  It stays empty.
                let _ = write_txn.open_table(UNDO_TABLE)?;

                let mut next_progress = COMPACTION_PROGRESS_INTERVAL;
                let mut stack: Vec<(Digest32, usize)> = vec![(top_node_hash, 0)];

                while let Some((label, depth)) = stack.pop() {
                    if depth > MAX_TREE_DEPTH {
                        bail!(
                            "source is corrupt: traversal passed {} levels at node {} — the \
                             child labels form a cycle or are damaged",
                            MAX_TREE_DEPTH,
                            digest_hex(&label)
                        );
                    }

                    // A reachable label with no row behind it is genuine
                    // corruption, not bloat.  Skipping it would launder a
                    // damaged tree into a database that looks compacted.
                    let guard = src_nodes.get(label.as_slice())?.with_context(|| {
                        format!(
                            "source is corrupt: node {} is reachable from the root but missing \
                             from the `nodes` table",
                            digest_hex(&label)
                        )
                    })?;
                    let packed: &[u8] = guard.value();

                    // The copy pass does not need the tree parameters: an
                    // internal node's child labels are always its final 64
                    // bytes, and leaves terminate.
                    let children = match packed.first() {
                        Some(&PACKED_INTERNAL_PREFIX) => {
                            if packed.len() < 2 + INTERNAL_CHILD_LABELS_LEN {
                                bail!(
                                    "source is corrupt: internal node {} is only {} bytes",
                                    digest_hex(&label),
                                    packed.len()
                                );
                            }
                            let split = packed.len() - INTERNAL_CHILD_LABELS_LEN;
                            let mut left: Digest32 = [0u8; 32];
                            let mut right: Digest32 = [0u8; 32];
                            left.copy_from_slice(&packed[split..split + 32]);
                            right.copy_from_slice(&packed[split + 32..]);
                            Some((left, right))
                        }
                        Some(&PACKED_LEAF_PREFIX) => None,
                        Some(other) => bail!(
                            "source is corrupt: node {} has unknown packed prefix 0x{:02x}",
                            digest_hex(&label),
                            other
                        ),
                        None => bail!(
                            "source is corrupt: node {} has empty packed bytes",
                            digest_hex(&label)
                        ),
                    };

                    dst_nodes.insert(label.as_slice(), packed)?;
                    drop(guard);
                    nodes_written += 1;

                    if nodes_written >= next_progress {
                        if let Some(cb) = progress {
                            cb(CompactionProgress { nodes_written });
                        }
                        next_progress += COMPACTION_PROGRESS_INTERVAL;
                    }

                    if let Some((left, right)) = children {
                        stack.push((right, depth + 1));
                        stack.push((left, depth + 1));
                    }
                }

                // 3. Metadata carried over verbatim, except the version
                //    chain, which collapses to the single surviving version.
                dst_meta.insert(META_TOP_NODE_HASH, top_node_hash.as_slice())?;
                dst_meta.insert(
                    META_TOP_NODE_HEIGHT,
                    top_node_height.to_be_bytes().as_slice(),
                )?;
                dst_meta.insert(META_CURRENT_VERSION, current_version.as_ref())?;
                dst_meta.insert(META_LSN, lsn.to_be_bytes().as_slice())?;
                dst_meta.insert(META_BLOCK_HEIGHT, block_height.to_be_bytes().as_slice())?;

                let chain = VecDeque::from([(lsn, current_version.clone())]);
                let chain_bytes = Self::serialize_version_chain(&chain);
                dst_meta.insert(META_VERSIONS, chain_bytes.as_slice())?;
            }
            write_txn
                .commit()
                .context("failed to commit the compacted database")?;
        }

        if let Some(cb) = progress {
            cb(CompactionProgress { nodes_written });
        }

        // Release both exclusive handles: the verification pass opens dest
        // read-only, which needs a shared lock.
        drop(dest_db);
        drop(source_read);
        drop(source_db);

        let dest_bytes = std::fs::metadata(dest)
            .with_context(|| format!("cannot stat compaction destination {}", dest.display()))?
            .len();

        info!(
            nodes_written,
            source_bytes, dest_bytes, "compaction: copy complete, verifying"
        );

        // 4. Verification is mandatory.  Without it "compacted" would mean
        //    "we think we copied everything".
        let root_label = verify_compacted(dest, &top_node_hash, nodes_written)?;

        // The AVL digest is the root label followed by the tree height as a
        // single byte — see AuthenticatedTreeOps::digest.
        if top_node_height > u8::MAX as u32 {
            bail!(
                "corrupt source: top_node_height {} does not fit the single byte the AVL digest \
                 encodes it in",
                top_node_height
            );
        }
        let mut digest = BytesMut::with_capacity(root_label.len() + 1);
        digest.extend_from_slice(&root_label);
        digest.extend_from_slice(&[top_node_height as u8]);
        let digest: ADDigest = digest.freeze();

        if digest != current_version {
            bail!(
                "verification failed: the compacted root digest {} does not match the source's \
                 current_version {}",
                bytes_hex(&digest),
                bytes_hex(&current_version)
            );
        }

        info!(
            nodes_written,
            source_bytes,
            dest_bytes,
            block_height,
            digest = %bytes_hex(&digest),
            "compaction: verified"
        );

        Ok(CompactionStats {
            nodes_written,
            source_bytes,
            dest_bytes,
            block_height,
            digest,
        })
    }
}

use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use redb::{Database, Durability, ReadableDatabase, ReadableTable};
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

/// Format a 32-byte digest as a flat lowercase hex string (64 chars).
/// Used for grep-friendly diagnostic logging.
fn digest_hex(label: &Digest32) -> String {
    let mut s = String::with_capacity(64);
    for b in label {
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
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

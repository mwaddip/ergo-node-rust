pub mod parser;
pub mod manifest;
pub mod protocol;
pub mod download;

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

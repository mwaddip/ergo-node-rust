use anyhow::{bail, Result};
use bytes::Bytes;
use ergo_avltree_rust::operation::Digest32;

/// Per-version undo record. Stores everything needed to reverse a single update().
pub struct UndoRecord {
    /// Nodes removed from the tree during this update — label + packed bytes.
    /// On rollback these get re-inserted into the nodes table.
    pub removed_nodes: Vec<(Digest32, Bytes)>,
    /// Labels of nodes inserted during this update.
    /// On rollback these get deleted from the nodes table.
    pub inserted_labels: Vec<Digest32>,
    /// Root node hash before this update.
    pub prev_top_node_hash: Digest32,
    /// Tree height before this update.
    pub prev_top_node_height: u32,
    /// ADDigest (version) before this update.  Empty for the first update.
    pub prev_version: Bytes,
    /// Caller-supplied block height before this update.  0 if the storage
    /// was empty or the caller never set one.
    pub prev_block_height: u32,
}

impl UndoRecord {
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Removed nodes
        buf.extend_from_slice(&(self.removed_nodes.len() as u32).to_be_bytes());
        for (label, packed) in &self.removed_nodes {
            buf.extend_from_slice(label);
            buf.extend_from_slice(&(packed.len() as u32).to_be_bytes());
            buf.extend_from_slice(packed);
        }

        // Inserted labels
        buf.extend_from_slice(&(self.inserted_labels.len() as u32).to_be_bytes());
        for label in &self.inserted_labels {
            buf.extend_from_slice(label);
        }

        // Previous metadata
        buf.extend_from_slice(&self.prev_top_node_hash);
        buf.extend_from_slice(&self.prev_top_node_height.to_be_bytes());
        buf.extend_from_slice(&(self.prev_version.len() as u32).to_be_bytes());
        buf.extend_from_slice(&self.prev_version);
        // Appended in the block_height revision.  Pre-revision records
        // stop here; deserialize treats absent trailing bytes as 0.
        buf.extend_from_slice(&self.prev_block_height.to_be_bytes());

        buf
    }

    pub fn deserialize(data: &[u8]) -> Result<Self> {
        let mut pos = 0;

        let read_u32 = |pos: &mut usize| -> Result<u32> {
            if *pos + 4 > data.len() {
                bail!("undo record truncated at offset {}", *pos);
            }
            let val = u32::from_be_bytes(data[*pos..*pos + 4].try_into()?);
            *pos += 4;
            Ok(val)
        };

        let read_digest = |pos: &mut usize| -> Result<Digest32> {
            if *pos + 32 > data.len() {
                bail!("undo record truncated at offset {}", *pos);
            }
            let mut d: Digest32 = [0u8; 32];
            d.copy_from_slice(&data[*pos..*pos + 32]);
            *pos += 32;
            Ok(d)
        };

        // Removed nodes
        let removed_count = read_u32(&mut pos)? as usize;
        let mut removed_nodes = Vec::with_capacity(removed_count);
        for _ in 0..removed_count {
            let label = read_digest(&mut pos)?;
            let packed_len = read_u32(&mut pos)? as usize;
            if pos + packed_len > data.len() {
                bail!("undo record truncated reading packed bytes at offset {}", pos);
            }
            let packed = Bytes::copy_from_slice(&data[pos..pos + packed_len]);
            pos += packed_len;
            removed_nodes.push((label, packed));
        }

        // Inserted labels
        let inserted_count = read_u32(&mut pos)? as usize;
        let mut inserted_labels = Vec::with_capacity(inserted_count);
        for _ in 0..inserted_count {
            inserted_labels.push(read_digest(&mut pos)?);
        }

        // Previous metadata
        let prev_top_node_hash = read_digest(&mut pos)?;
        let prev_top_node_height = read_u32(&mut pos)?;
        let version_len = read_u32(&mut pos)? as usize;
        if pos + version_len > data.len() {
            bail!("undo record truncated reading prev_version at offset {}", pos);
        }
        let prev_version = Bytes::copy_from_slice(&data[pos..pos + version_len]);
        pos += version_len;

        // Trailing field — present only on records written after the
        // block_height revision.  Absent ⇒ treat as 0 (pre-revision
        // caller didn't track block height).
        let prev_block_height = if pos + 4 <= data.len() {
            read_u32(&mut pos)?
        } else {
            0
        };

        Ok(UndoRecord {
            removed_nodes,
            inserted_labels,
            prev_top_node_hash,
            prev_top_node_height,
            prev_version,
            prev_block_height,
        })
    }
}

//! Snapshot serving — handles incoming P2P requests for UTXO snapshots.
//!
//! Request codes 76 (GetSnapshotsInfo), 78 (GetManifest), 80 (GetUtxoSnapshotChunk)
//! are intercepted from the P2P event stream and answered from the local
//! `SnapshotStore`. Unknown IDs get no response (matches JVM behavior).

use ergo_sync::snapshot::protocol::{
    SnapshotEntry, SnapshotMessage, GET_MANIFEST, GET_SNAPSHOTS_INFO, GET_UTXO_SNAPSHOT_CHUNK,
};

use crate::snapshot_store::SnapshotStore;

/// Handle a snapshot request. Returns `Some((response_code, response_body))` if
/// a response should be sent, or `None` for unknown IDs (no response = JVM behavior).
pub fn handle_snapshot_request(
    code: u8,
    body: &[u8],
    store: &SnapshotStore,
) -> Option<(u8, Vec<u8>)> {
    match code {
        GET_SNAPSHOTS_INFO => {
            let info = store.snapshots_info().ok()?;
            let entries: Vec<SnapshotEntry> = info
                .into_iter()
                .map(|(height, manifest_id)| SnapshotEntry {
                    height,
                    manifest_id,
                })
                .collect();
            if entries.is_empty() {
                return None;
            }
            Some(SnapshotMessage::SnapshotsInfo(entries).encode())
        }

        GET_MANIFEST => {
            if body.len() < 32 {
                return None;
            }
            let mut id = [0u8; 32];
            id.copy_from_slice(&body[..32]);
            let data = store.get_manifest(&id).ok()??;
            Some(SnapshotMessage::Manifest(data).encode())
        }

        GET_UTXO_SNAPSHOT_CHUNK => {
            if body.len() < 32 {
                return None;
            }
            let mut id = [0u8; 32];
            id.copy_from_slice(&body[..32]);
            let data = store.get_chunk(&id).ok()??;
            Some(SnapshotMessage::UtxoSnapshotChunk(data).encode())
        }

        _ => None,
    }
}

/// Returns true if `code` is a snapshot request that should be intercepted
/// for serving (not forwarded to the sync machine).
pub fn is_snapshot_request(code: u8) -> bool {
    matches!(code, GET_SNAPSHOTS_INFO | GET_MANIFEST | GET_UTXO_SNAPSHOT_CHUNK)
}

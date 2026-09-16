//! End-to-end snapshot sync against a scripted transport: discovery, manifest
//! binding, chunk download, assembly, and crash-resume.
//!
//! The transport answers requests from a script keyed by `(peer, code)`,
//! serves chunks by subtree id, and — when it has nothing queued — pends like
//! a quiet network rather than closing the stream, so the code under test
//! reaches its own timeouts. Every test runs on a paused clock: a timeout
//! elapses the moment nothing else can make progress.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::pending;
use std::path::PathBuf;
use std::sync::Mutex;

use enr_chain::{BlockId, ChainError, Header, SyncInfo};
use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use ergo_sync::snapshot::download::ChunkDownloadStore;
use ergo_sync::snapshot::parser::{
    compute_internal_label, compute_leaf_label, PACKED_INTERNAL_PREFIX, PACKED_LEAF_PREFIX,
};
use ergo_sync::snapshot::protocol::{
    SnapshotEntry, SnapshotMessage, GET_MANIFEST, GET_SNAPSHOTS_INFO, GET_UTXO_SNAPSHOT_CHUNK,
};
use ergo_sync::snapshot::{run_snapshot_sync, SnapshotConfig, SnapshotData, SnapshotError};
use ergo_sync::{SyncChain, SyncTransport};
use ergo_validation::Parameters;

const SNAPSHOT_HEIGHT: u32 = 522_239;
const ROOT_HEIGHT: u8 = 5;
const PEER_A: PeerId = PeerId(1);
const PEER_B: PeerId = PeerId(2);
/// Not in the quorum: never asked for anything.
const PEER_X: PeerId = PeerId(99);

// ── Fixture: a depth-2 manifest over four single-leaf subtrees ──────────────

fn pack_internal(balance: i8, key: &[u8; 32], left: &[u8; 32], right: &[u8; 32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(98);
    buf.push(PACKED_INTERNAL_PREFIX);
    buf.push(balance as u8);
    buf.extend_from_slice(key);
    buf.extend_from_slice(left);
    buf.extend_from_slice(right);
    buf
}

fn pack_leaf(key: &[u8; 32], value: &[u8], next_key: &[u8; 32]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(PACKED_LEAF_PREFIX);
    buf.extend_from_slice(key);
    buf.extend_from_slice(&(value.len() as u32).to_be_bytes());
    buf.extend_from_slice(value);
    buf.extend_from_slice(next_key);
    buf
}

struct Fixture {
    /// Header included.
    manifest: Vec<u8>,
    /// What the header at `SNAPSHOT_HEIGHT` carries: root hash + tree height.
    state_root: [u8; 33],
    root_hash: [u8; 32],
    /// Subtree id → chunk bytes. Each chunk is a single leaf.
    chunks: HashMap<[u8; 32], Vec<u8>>,
}

fn fixture() -> Fixture {
    let leaves: Vec<([u8; 32], Vec<u8>)> = (0..4u8)
        .map(|i| {
            let key = [0x10 * (i + 1); 32];
            let value = vec![i; 8];
            let next = [0x10 * (i + 2); 32];
            (
                compute_leaf_label(&key, &value, &next),
                pack_leaf(&key, &value, &next),
            )
        })
        .collect();
    let boundary_l = compute_internal_label(0, &leaves[0].0, &leaves[1].0);
    let boundary_r = compute_internal_label(0, &leaves[2].0, &leaves[3].0);
    let root_hash = compute_internal_label(0, &boundary_l, &boundary_r);

    let mut manifest = vec![ROOT_HEIGHT, 2];
    manifest.extend(pack_internal(0, &[0x20; 32], &boundary_l, &boundary_r));
    manifest.extend(pack_internal(0, &[0x10; 32], &leaves[0].0, &leaves[1].0));
    manifest.extend(pack_internal(0, &[0x30; 32], &leaves[2].0, &leaves[3].0));

    let mut state_root = [0u8; 33];
    state_root[..32].copy_from_slice(&root_hash);
    state_root[32] = ROOT_HEIGHT;

    Fixture {
        manifest,
        state_root,
        root_hash,
        chunks: leaves.into_iter().collect(),
    }
}

impl Fixture {
    /// Same tree, different root label: the root node's balance byte flipped.
    fn manifest_with_wrong_root(&self) -> Vec<u8> {
        let mut m = self.manifest.clone();
        m[3] ^= 0x01;
        m
    }

    /// Right root label, wrong tree height.
    fn manifest_with_wrong_height(&self) -> Vec<u8> {
        let mut m = self.manifest.clone();
        m[0] = ROOT_HEIGHT + 1;
        m
    }
}

// ── Scripted transport ──────────────────────────────────────────────────────

fn message(peer: PeerId, msg: &SnapshotMessage) -> ProtocolEvent {
    let (code, body) = msg.encode();
    ProtocolEvent::Message {
        peer_id: peer,
        message: ProtocolMessage::Unknown { code, body },
    }
}

fn announcement(peer: PeerId, manifest_id: [u8; 32]) -> ProtocolEvent {
    message(
        peer,
        &SnapshotMessage::SnapshotsInfo(vec![SnapshotEntry {
            height: SNAPSHOT_HEIGHT,
            manifest_id,
        }]),
    )
}

fn manifest_reply(peer: PeerId, bytes: &[u8]) -> ProtocolEvent {
    message(peer, &SnapshotMessage::Manifest(bytes.to_vec()))
}

#[derive(Default)]
struct Script {
    /// Events served in order by `next_event`.
    queue: VecDeque<ProtocolEvent>,
    /// The n-th request of `(peer, code)` enqueues the n-th batch.
    replies: HashMap<(PeerId, u8), VecDeque<Vec<ProtocolEvent>>>,
    /// Chunks by subtree id. A request for a known id is answered by the peer asked.
    chunks: HashMap<[u8; 32], Vec<u8>>,
    /// Per-peer chunk overrides: (peer, subtree_id) → bytes. When set, this
    /// peer serves these bytes instead of the default chunk.
    chunk_overrides: HashMap<(PeerId, [u8; 32]), Vec<u8>>,
    /// Every request sent: (peer, code, body).
    sent: Vec<(PeerId, u8, Vec<u8>)>,
}

struct ScriptedTransport {
    outbound: Vec<PeerId>,
    script: Mutex<Script>,
}

impl ScriptedTransport {
    fn new(outbound: Vec<PeerId>) -> Self {
        Self {
            outbound,
            script: Mutex::new(Script::default()),
        }
    }

    fn on(&self, peer: PeerId, code: u8, events: Vec<ProtocolEvent>) {
        self.script
            .lock()
            .unwrap()
            .replies
            .entry((peer, code))
            .or_default()
            .push_back(events);
    }

    fn serve_chunks(&self, chunks: &HashMap<[u8; 32], Vec<u8>>) {
        self.script
            .lock()
            .unwrap()
            .chunks
            .extend(chunks.iter().map(|(id, bytes)| (*id, bytes.clone())));
    }

    /// Make `peer` serve `bytes` instead of the default chunk for `id`.
    fn tamper_chunk(&self, peer: PeerId, id: [u8; 32], bytes: Vec<u8>) {
        self.script
            .lock()
            .unwrap()
            .chunk_overrides
            .insert((peer, id), bytes);
    }

    /// Peers asked with `code`, in request order.
    fn asked(&self, code: u8) -> Vec<PeerId> {
        self.script
            .lock()
            .unwrap()
            .sent
            .iter()
            .filter(|(_, c, _)| *c == code)
            .map(|(peer, _, _)| *peer)
            .collect()
    }

    /// Subtree ids requested, in request order.
    fn chunk_ids_asked(&self) -> Vec<[u8; 32]> {
        self.script
            .lock()
            .unwrap()
            .sent
            .iter()
            .filter(|(_, c, _)| *c == GET_UTXO_SNAPSHOT_CHUNK)
            .map(|(_, _, body)| body[..32].try_into().unwrap())
            .collect()
    }
}

impl SyncTransport for ScriptedTransport {
    async fn send_to(
        &self,
        peer: PeerId,
        message: ProtocolMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        let ProtocolMessage::Unknown { code, body } = message else {
            return Ok(());
        };
        let mut script = self.script.lock().unwrap();
        if code == GET_UTXO_SNAPSHOT_CHUNK {
            let id: [u8; 32] = body[..32].try_into().unwrap();
            let chunk = script
                .chunk_overrides
                .get(&(peer, id))
                .or_else(|| script.chunks.get(&id))
                .cloned();
            if let Some(chunk) = chunk {
                let reply = self::message(peer, &SnapshotMessage::UtxoSnapshotChunk(chunk));
                script.queue.push_back(reply);
            }
        } else if let Some(batch) = script
            .replies
            .get_mut(&(peer, code))
            .and_then(VecDeque::pop_front)
        {
            script.queue.extend(batch);
        }
        script.sent.push((peer, code, body));
        Ok(())
    }

    async fn outbound_peers(&self) -> Vec<PeerId> {
        self.outbound.clone()
    }

    async fn next_event(&mut self) -> Option<ProtocolEvent> {
        let next = self.script.lock().unwrap().queue.pop_front();
        match next {
            Some(event) => Some(event),
            None => pending().await,
        }
    }
}

// ── Header chain that knows one state root ──────────────────────────────────

struct OneHeaderChain {
    state_root: [u8; 33],
}

impl SyncChain for OneHeaderChain {
    async fn chain_height(&self) -> u32 {
        SNAPSHOT_HEIGHT + 100
    }
    async fn build_sync_info(&self) -> Vec<u8> {
        Vec::new()
    }
    async fn header_at(&self, _height: u32) -> Option<Header> {
        None
    }
    async fn header_state_root(&self, height: u32) -> Option<[u8; 33]> {
        (height == SNAPSHOT_HEIGHT).then_some(self.state_root)
    }
    fn parse_sync_info(&self, _body: &[u8]) -> Result<SyncInfo, ChainError> {
        unimplemented!("not used by snapshot sync")
    }
    async fn continuation_ids(&self, _ids: &[BlockId], _limit: usize) -> Vec<[u8; 32]> {
        Vec::new()
    }
    async fn active_parameters(&self) -> Parameters {
        unimplemented!("not used by snapshot sync")
    }
    async fn is_epoch_boundary(&self, _height: u32) -> bool {
        false
    }
    async fn voting_length(&self) -> u32 {
        1024
    }
    async fn compute_expected_parameters(
        &self,
        _height: u32,
        _proposed_update: &[u8],
    ) -> Result<Parameters, ChainError> {
        unimplemented!("not used by snapshot sync")
    }
    async fn apply_epoch_boundary_parameters(&self, _params: Parameters, _update: Vec<u8>) {}
    async fn active_proposed_update_bytes(&self) -> Vec<u8> {
        Vec::new()
    }
    async fn verify_nipopow_envelope(&self, _body: &[u8]) -> Result<Vec<Header>, ChainError> {
        unimplemented!("not used by snapshot sync")
    }
    async fn is_better_nipopow(&self, _this: &[u8], _than: &[u8]) -> Result<bool, ChainError> {
        unimplemented!("not used by snapshot sync")
    }
    async fn install_nipopow_suffix(
        &self,
        _head: Header,
        _tail: Vec<Header>,
    ) -> Result<(), ChainError> {
        unimplemented!("not used by snapshot sync")
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn config(dir: &tempfile::TempDir) -> SnapshotConfig {
    SnapshotConfig {
        min_snapshot_peers: 2,
        chunk_timeout_multiplier: 1,
        data_dir: dir.path().to_path_buf(),
    }
}

fn download_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("snapshot_download.redb")
}

/// Both quorum peers announce the fixture's snapshot.
fn announce_both(transport: &ScriptedTransport, fx: &Fixture) {
    transport.on(
        PEER_A,
        GET_SNAPSHOTS_INFO,
        vec![announcement(PEER_A, fx.root_hash)],
    );
    transport.on(
        PEER_B,
        GET_SNAPSHOTS_INFO,
        vec![announcement(PEER_B, fx.root_hash)],
    );
}

fn assert_assembled(data: &SnapshotData, fx: &Fixture) {
    assert_eq!(data.root_hash, fx.root_hash);
    assert_eq!(data.tree_height, ROOT_HEIGHT);
    assert_eq!(data.snapshot_height, SNAPSHOT_HEIGHT);
    // 3 manifest nodes + 4 single-leaf chunks.
    assert_eq!(data.nodes.len(), 7);
    let labels: HashSet<[u8; 32]> = data.nodes.iter().map(|(label, _)| *label).collect();
    assert!(labels.contains(&fx.root_hash));
    for id in fx.chunks.keys() {
        assert!(labels.contains(id), "chunk node missing from the assembly");
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn happy_path_completes_with_a_verified_manifest() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![manifest_reply(PEER_A, &fx.manifest)],
    );
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert_eq!(transport.asked(GET_MANIFEST), vec![PEER_A]);
    assert!(
        !download_path(&dir).exists(),
        "download store is removed after assembly"
    );
}

#[tokio::test(start_paused = true)]
async fn manifest_from_a_peer_that_was_not_asked_is_ignored() {
    // PEER_X answers first with a manifest that would fail verification. Were
    // it taken as PEER_A's answer, PEER_A would be dropped and PEER_B asked.
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![
            manifest_reply(PEER_X, &fx.manifest_with_wrong_root()),
            manifest_reply(PEER_A, &fx.manifest),
        ],
    );
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert_eq!(
        transport.asked(GET_MANIFEST),
        vec![PEER_A],
        "PEER_A's own answer got through"
    );
    // PEER_A kept its place in the quorum: it heads the chunk rotation.
    let chunk_peers = transport.asked(GET_UTXO_SNAPSHOT_CHUNK);
    assert!(!chunk_peers.is_empty());
    assert!(chunk_peers.iter().all(|p| *p == PEER_A), "{chunk_peers:?}");
}

#[tokio::test(start_paused = true)]
async fn a_correct_manifest_from_an_unasked_peer_is_not_the_answer() {
    // PEER_X sends the right bytes, but PEER_A was asked. PEER_A stays silent,
    // the request times out, and PEER_B is asked next.
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![manifest_reply(PEER_X, &fx.manifest)],
    );
    transport.on(
        PEER_B,
        GET_MANIFEST,
        vec![manifest_reply(PEER_B, &fx.manifest)],
    );
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert_eq!(transport.asked(GET_MANIFEST), vec![PEER_A, PEER_B]);
    // A timeout is not a verification failure: PEER_A stays in the quorum.
    let chunk_peers = transport.asked(GET_UTXO_SNAPSHOT_CHUNK);
    assert!(!chunk_peers.is_empty());
    assert!(chunk_peers.iter().all(|p| *p == PEER_A), "{chunk_peers:?}");
}

#[tokio::test(start_paused = true)]
async fn manifest_with_the_wrong_root_label_drops_the_peer_and_tries_the_next() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![manifest_reply(PEER_A, &fx.manifest_with_wrong_root())],
    );
    transport.on(
        PEER_B,
        GET_MANIFEST,
        vec![manifest_reply(PEER_B, &fx.manifest)],
    );
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert_eq!(transport.asked(GET_MANIFEST), vec![PEER_A, PEER_B]);
    let chunk_peers = transport.asked(GET_UTXO_SNAPSHOT_CHUNK);
    assert!(!chunk_peers.is_empty());
    assert!(
        chunk_peers.iter().all(|p| *p == PEER_B),
        "PEER_A left the quorum set: {chunk_peers:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn manifest_with_the_right_root_but_wrong_height_is_rejected() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![manifest_reply(PEER_A, &fx.manifest_with_wrong_height())],
    );
    transport.on(
        PEER_B,
        GET_MANIFEST,
        vec![manifest_reply(PEER_B, &fx.manifest)],
    );
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert_eq!(transport.asked(GET_MANIFEST), vec![PEER_A, PEER_B]);
    let chunk_peers = transport.asked(GET_UTXO_SNAPSHOT_CHUNK);
    assert!(!chunk_peers.is_empty());
    assert!(
        chunk_peers.iter().all(|p| *p == PEER_B),
        "PEER_A left the quorum set: {chunk_peers:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn every_quorum_peer_rejected_fails_the_download_and_keeps_nothing() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![manifest_reply(PEER_A, &fx.manifest_with_wrong_root())],
    );
    transport.on(
        PEER_B,
        GET_MANIFEST,
        vec![manifest_reply(PEER_B, &fx.manifest_with_wrong_height())],
    );
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let result = run_snapshot_sync(&mut transport, &chain, &config(&dir)).await;

    let Err(err) = result else {
        panic!("a bootstrap with no verified manifest must not complete");
    };
    assert!(
        matches!(err, SnapshotError::ManifestDownloadFailed),
        "{err}"
    );
    assert!(
        transport.asked(GET_UTXO_SNAPSHOT_CHUNK).is_empty(),
        "no subtree id came out of a rejected manifest"
    );
    assert!(
        !download_path(&dir).exists(),
        "no download store was created for a rejected manifest"
    );
}

#[tokio::test(start_paused = true)]
async fn resume_with_a_tampered_stored_manifest_discards_the_download() {
    // A complete download whose stored manifest has one byte flipped. Its
    // recorded manifest_id is still the right one — exactly what a resume
    // must not trust. The download is discarded and discovery starts over;
    // a resumed download would have asked nobody for SnapshotsInfo or a
    // manifest.
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    {
        let store = ChunkDownloadStore::create(
            &download_path(&dir),
            fx.root_hash,
            SNAPSHOT_HEIGHT,
            &fx.manifest_with_wrong_root(),
            4,
        )
        .unwrap();
        for (id, chunk) in &fx.chunks {
            store.store_chunk(id, chunk).unwrap();
        }
        let ids: Vec<[u8; 32]> = fx.chunks.keys().copied().collect();
        assert!(store.is_complete(&ids).unwrap());
    }
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![manifest_reply(PEER_A, &fx.manifest)],
    );
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert_eq!(transport.asked(GET_SNAPSHOTS_INFO), vec![PEER_A, PEER_B]);
    assert_eq!(transport.asked(GET_MANIFEST), vec![PEER_A]);
    assert_eq!(
        transport.chunk_ids_asked().len(),
        4,
        "every chunk was fetched again"
    );
    assert!(!download_path(&dir).exists());
}

#[tokio::test(start_paused = true)]
async fn resume_with_an_intact_stored_manifest_fetches_only_the_missing_chunks() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut ids: Vec<[u8; 32]> = fx.chunks.keys().copied().collect();
    ids.sort();
    let (stored, missing) = ids.split_at(2);
    {
        let store = ChunkDownloadStore::create(
            &download_path(&dir),
            fx.root_hash,
            SNAPSHOT_HEIGHT,
            &fx.manifest,
            4,
        )
        .unwrap();
        for id in stored {
            store.store_chunk(id, &fx.chunks[id]).unwrap();
        }
    }
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert!(transport.asked(GET_SNAPSHOTS_INFO).is_empty());
    assert!(transport.asked(GET_MANIFEST).is_empty());
    let asked: HashSet<[u8; 32]> = transport.chunk_ids_asked().into_iter().collect();
    let expected: HashSet<[u8; 32]> = missing.iter().copied().collect();
    assert_eq!(asked, expected, "only the missing chunks were requested");
    assert!(!download_path(&dir).exists());
}

// ── Chunk link verification ─────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn tampered_chunk_interior_is_dropped_and_the_next_peers_intact_chunk_is_accepted() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![manifest_reply(PEER_A, &fx.manifest)],
    );
    transport.serve_chunks(&fx.chunks);

    // Pick one chunk and tamper its interior — flip a byte in the value field
    // so the leaf label changes but the root's label is still what we asked for.
    // Actually, flipping any byte inside the chunk changes some label, so the
    // root no longer matches unless the flip is outside the hash preimage. The
    // simplest tamper: flip a byte inside the first (and only) leaf's value.
    let target_id = *fx.chunks.keys().next().unwrap();
    let mut bad = fx.chunks[&target_id].clone();
    // The leaf is: 0x01 | key(32) | value_len(4) | value(8) | next_key(32)
    // value starts at offset 37. Flip one byte there.
    bad[37] ^= 0xFF;
    transport.tamper_chunk(PEER_A, target_id, bad);

    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);

    // target_id was asked twice: once to PEER_A (tampered → rejected) and
    // once to a different peer (PEER_B). The id appears in the request list
    // at least twice.
    let chunk_asks: Vec<(PeerId, [u8; 32])> = transport
        .script
        .lock()
        .unwrap()
        .sent
        .iter()
        .filter(|(_, c, _)| *c == GET_UTXO_SNAPSHOT_CHUNK)
        .map(|(p, _, body)| (*p, body[..32].try_into().unwrap()))
        .collect();

    let target_asks: Vec<PeerId> = chunk_asks
        .iter()
        .filter(|(_, id)| *id == target_id)
        .map(|(p, _)| *p)
        .collect();
    assert!(
        target_asks.len() >= 2,
        "the tampered chunk should be re-requested: {target_asks:?}"
    );
    // The second ask should prefer a different peer.
    assert_ne!(
        target_asks[0], target_asks[1],
        "the re-request should prefer a different peer"
    );
    assert!(!download_path(&dir).exists());
}

#[tokio::test(start_paused = true)]
async fn stored_chunk_with_interior_tampered_rejects_at_assembly_and_restarts_fresh() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let ids: Vec<[u8; 32]> = fx.chunks.keys().copied().collect();
    {
        let store = ChunkDownloadStore::create(
            &download_path(&dir),
            fx.root_hash,
            SNAPSHOT_HEIGHT,
            &fx.manifest,
            ids.len() as u32,
        )
        .unwrap();
        for (i, id) in ids.iter().enumerate() {
            if i == 0 {
                // Tamper the first chunk's interior.
                let mut bad = fx.chunks[id].clone();
                bad[37] ^= 0xFF;
                store.store_chunk(id, &bad).unwrap();
            } else {
                store.store_chunk(id, &fx.chunks[id]).unwrap();
            }
        }
        assert!(store.is_complete(&ids).unwrap());
    }
    // Resume should detect the tampered chunk at assembly, discard the
    // download, and start fresh.
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    announce_both(&transport, &fx);
    transport.on(
        PEER_A,
        GET_MANIFEST,
        vec![manifest_reply(PEER_A, &fx.manifest)],
    );
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert_eq!(transport.asked(GET_SNAPSHOTS_INFO), vec![PEER_A, PEER_B]);
    assert_eq!(transport.asked(GET_MANIFEST), vec![PEER_A]);
    assert!(!download_path(&dir).exists());
}

// ── Completeness follows the manifest ───────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn total_chunks_counter_too_low_does_not_stop_the_download_early() {
    // total_chunks = 2 but the manifest has 4 subtree ids. Completeness
    // follows the manifest, so all 4 are downloaded.
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let ids: Vec<[u8; 32]> = fx.chunks.keys().copied().collect();
    {
        let store = ChunkDownloadStore::create(
            &download_path(&dir),
            fx.root_hash,
            SNAPSHOT_HEIGHT,
            &fx.manifest,
            2, // lies: claims only 2 chunks needed
        )
        .unwrap();
        // Store 2 of 4 chunks — the counter says we're done, the manifest says not.
        for id in &ids[..2] {
            store.store_chunk(id, &fx.chunks[id]).unwrap();
        }
        // Counter-based is_complete would be true, manifest-based is not.
        assert!(store.chunk_count().unwrap() >= store.total_chunks());
        assert!(!store.is_complete(&ids).unwrap());
    }
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    // The remaining 2 chunks were fetched.
    let asked: HashSet<[u8; 32]> = transport.chunk_ids_asked().into_iter().collect();
    let remaining: HashSet<[u8; 32]> = ids[2..].iter().copied().collect();
    assert_eq!(asked, remaining, "exactly the missing chunks were fetched");
    assert!(!download_path(&dir).exists());
}

#[tokio::test(start_paused = true)]
async fn total_chunks_counter_too_high_does_not_delay_assembly() {
    // total_chunks = 999 but the manifest has 4 subtree ids. All 4 are
    // already stored, so the download resumes with nothing to fetch.
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let ids: Vec<[u8; 32]> = fx.chunks.keys().copied().collect();
    {
        let store = ChunkDownloadStore::create(
            &download_path(&dir),
            fx.root_hash,
            SNAPSHOT_HEIGHT,
            &fx.manifest,
            999, // lies: claims 999 chunks
        )
        .unwrap();
        for id in &ids {
            store.store_chunk(id, &fx.chunks[id]).unwrap();
        }
        // Counter says far from done; manifest says done.
        assert!(store.chunk_count().unwrap() < store.total_chunks());
        assert!(store.is_complete(&ids).unwrap());
    }
    let mut transport = ScriptedTransport::new(vec![PEER_A, PEER_B]);
    transport.serve_chunks(&fx.chunks);
    let chain = OneHeaderChain {
        state_root: fx.state_root,
    };

    let data = run_snapshot_sync(&mut transport, &chain, &config(&dir))
        .await
        .unwrap();

    assert_assembled(&data, &fx);
    assert!(
        transport.chunk_ids_asked().is_empty(),
        "no chunks were fetched — all present"
    );
    assert!(!download_path(&dir).exists());
}

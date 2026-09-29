//! `section_id_from_body` and the shared root computations, pinned against
//! a real testnet block: height 2666, header version 4, three transactions
//! mixing empty and non-empty spending proofs, sixteen extension fields,
//! and the AD proof. `tests/fixtures/block-2666.json` is the
//! `entries[0].block` object of the SANTA vector
//! `block/v6/captured/bigint-downcast-2666.json`.
//!
//! The section bodies are rebuilt in wire format from the fixture's JSON by
//! serializers local to this file, written from the JVM
//! `BlockTransactionsSerializer` / `ADProofsSerializer` / `ExtensionSerializer`
//! layouts, so the assertions bind the parser to the JVM layout and not to
//! another serializer in the workspace.
//!
//! Section 6 adds a transaction built for the purpose, whose id depends on
//! the tree version it is read at.

use enr_chain::{
    ad_proofs_digest, extension_root, section_id, section_id_from_body, section_ids,
    transactions_root, witness_id, ChainError, Header, SectionIdentity, Transaction,
    AD_PROOFS_TYPE_ID, BLOCK_TRANSACTIONS_TYPE_ID, EXTENSION_TYPE_ID, HEADER_TYPE_ID,
    TRANSACTION_TYPE_ID,
};
use ergo_chain_types::blake2b256_hash;
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::Input;
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{BoxId, ErgoBoxCandidate, NonMandatoryRegisters};
use ergo_lib::ergotree_ir::chain::evaluated_value::{EvaluatedExpr, EvaluatedValue};
use ergo_lib::ergotree_ir::ergo_tree::{ErgoTree, ErgoTreeVersion};
use ergo_lib::ergotree_ir::mir::constant::Constant;
use ergo_lib::ergotree_ir::mir::expr::Expr;
use ergo_lib::ergotree_ir::mir::upcast::Upcast;
use ergo_lib::ergotree_ir::serialization::sigma_byte_reader::{from_bytes, SigmaByteRead};
use ergo_lib::ergotree_ir::serialization::sigma_byte_writer::{SigmaByteWrite, SigmaByteWriter};
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::{SigmaBoolean, SigmaProp};
use ergo_lib::ergotree_ir::types::stype::SType;
use sigma_ser::vlq_encode::WriteSigmaVlqExt;
use sigma_ser::ScorexSerializable;

const FIXTURE: &str = include_str!("fixtures/block-2666.json");

/// `Blake2b256(no bytes)`: sigma-rust `MerkleTree::root_hash_special`, JVM
/// `Algos.emptyMerkleTreeRoot`.
const EMPTY_TREE_ROOT: &str = "0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8";

/// JVM `BlockTransactionsSerializer.MaxTransactionsInBlock`.
const BLOCK_VERSION_SENTINEL: u32 = 10_000_000;

struct Block {
    header: Header,
    txs: Vec<Transaction>,
    fields: Vec<([u8; 2], Vec<u8>)>,
    proof: Vec<u8>,
    /// JVM `BlockTransactions.size`: the serialized section length.
    tx_section_size: usize,
    extension_digest: [u8; 32],
    proof_digest: [u8; 32],
}

fn hex32(s: &str) -> [u8; 32] {
    hex::decode(s).unwrap().try_into().unwrap()
}

fn load() -> Block {
    let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let header: Header = serde_json::from_value(v["header"].clone()).unwrap();
    let txs: Vec<Transaction> = v["blockTransactions"]["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tx| serde_json::from_value(tx.clone()).unwrap())
        .collect();
    let fields = v["extension"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kv| {
            let key: [u8; 2] = hex::decode(kv[0].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
            (key, hex::decode(kv[1].as_str().unwrap()).unwrap())
        })
        .collect();
    Block {
        header,
        txs,
        fields,
        proof: hex::decode(v["adProofs"]["proofBytes"].as_str().unwrap()).unwrap(),
        tx_section_size: v["blockTransactions"]["size"].as_u64().unwrap() as usize,
        extension_digest: hex32(v["extension"]["digest"].as_str().unwrap()),
        proof_digest: hex32(v["adProofs"]["digest"].as_str().unwrap()),
    }
}

impl Block {
    fn header_id(&self) -> [u8; 32] {
        self.header.id.0 .0
    }

    fn tx_body(&self) -> Vec<u8> {
        tx_body(&self.header_id(), self.header.version, &self.txs)
    }

    fn ad_body(&self) -> Vec<u8> {
        ad_body(&self.header_id(), &self.proof)
    }

    fn ext_body(&self) -> Vec<u8> {
        ext_body(&self.header_id(), &self.fields)
    }

    /// `(type_id, body)` for all three sections.
    fn bodies(&self) -> [(u8, Vec<u8>); 3] {
        [
            (BLOCK_TRANSACTIONS_TYPE_ID, self.tx_body()),
            (AD_PROOFS_TYPE_ID, self.ad_body()),
            (EXTENSION_TYPE_ID, self.ext_body()),
        ]
    }
}

fn vlq_u32(out: &mut Vec<u8>, v: u32) {
    WriteSigmaVlqExt::put_u32(out, v).unwrap();
}

fn vlq_u16(out: &mut Vec<u8>, v: u16) {
    WriteSigmaVlqExt::put_u16(out, v).unwrap();
}

/// JVM `BlockTransactionsSerializer.serialize`: the version sentinel is
/// written only for block versions above 1, and from block version 3 each
/// transaction is written under `withVersions(blockVersion, blockVersion)`,
/// which writes as tree version 3 does. The rule is stated here rather than
/// taken from `block_transactions_write_version`, so the bodies stay
/// independent of the crate under test.
fn tx_body(header_id: &[u8; 32], block_version: u8, txs: &[Transaction]) -> Vec<u8> {
    let mut out = header_id.to_vec();
    if block_version > 1 {
        vlq_u32(&mut out, BLOCK_VERSION_SENTINEL + u32::from(block_version));
    }
    vlq_u32(&mut out, txs.len() as u32);
    let version = if block_version >= 3 {
        ErgoTreeVersion::V3
    } else {
        ErgoTreeVersion::V0
    };
    for tx in txs {
        out.extend(written_at(tx, version));
    }
    out
}

/// `tx` as a writer at tree version `version` writes it.
fn written_at(tx: &Transaction, version: ErgoTreeVersion) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = SigmaByteWriter::new(&mut out, None);
    w.with_tree_version(version, |w| tx.sigma_serialize(w))
        .unwrap();
    out
}

/// `bytes` read as a transaction at tree version `version`.
fn read_at(bytes: &[u8], version: ErgoTreeVersion) -> Transaction {
    from_bytes(bytes)
        .with_tree_version(version, Transaction::sigma_parse)
        .unwrap()
}

/// JVM `ADProofsSerializer.serialize`.
fn ad_body(header_id: &[u8; 32], proof: &[u8]) -> Vec<u8> {
    let mut out = header_id.to_vec();
    vlq_u32(&mut out, proof.len() as u32);
    out.extend_from_slice(proof);
    out
}

/// JVM `ExtensionSerializer.serialize`: `putUShort` count, raw-byte value length.
fn ext_body(header_id: &[u8; 32], fields: &[([u8; 2], Vec<u8>)]) -> Vec<u8> {
    let mut out = header_id.to_vec();
    vlq_u16(&mut out, fields.len() as u16);
    for (key, value) in fields {
        out.extend_from_slice(key);
        out.push(value.len() as u8);
        out.extend_from_slice(value);
    }
    out
}

fn identity(type_id: u8, body: &[u8]) -> SectionIdentity {
    section_id_from_body(type_id, body)
        .unwrap_or_else(|e| panic!("type {type_id} body must bind: {e}"))
}

fn rejected(type_id: u8, body: &[u8]) -> String {
    match section_id_from_body(type_id, body) {
        Err(ChainError::Section { type_id: t, reason }) => {
            assert_eq!(t, type_id, "error carries the type id it was given");
            reason
        }
        Err(other) => panic!("type {type_id}: expected ChainError::Section, got {other}"),
        Ok(id) => panic!(
            "type {type_id}: expected rejection, bound to {}",
            hex::encode(id.id)
        ),
    }
}

// ---------------------------------------------------------------------------
// Fixture integrity: the JSON round-trip must reconstruct the on-chain bytes,
// otherwise every "matches the real block" assertion below is vacuous.
// ---------------------------------------------------------------------------

#[test]
fn fixture_round_trips_to_the_on_chain_bytes() {
    let b = load();
    assert_eq!(b.header.height, 2666);
    assert_eq!(b.header.version, 4);
    assert_eq!(b.txs.len(), 3);
    assert_eq!(b.fields.len(), 16);

    let serialized = b.header.scorex_serialize_bytes().unwrap();
    assert_eq!(
        blake2b256_hash(&serialized).0,
        b.header_id(),
        "header id must be the hash of the re-serialized header"
    );

    let fixture: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let json_txs = fixture["blockTransactions"]["transactions"]
        .as_array()
        .unwrap();
    for (tx, json_tx) in b.txs.iter().zip(json_txs) {
        assert_eq!(
            hex::encode(tx.id().as_ref()),
            json_tx["id"].as_str().unwrap(),
            "parsed tx id diverged from the on-chain id"
        );
    }

    assert_eq!(
        b.tx_body().len(),
        b.tx_section_size,
        "rebuilt BlockTransactions body must have the JVM's serialized size"
    );
}

// ---------------------------------------------------------------------------
// 1. The binding, end to end, on the real block.
// ---------------------------------------------------------------------------

#[test]
fn real_block_bodies_bind_to_their_header() {
    let b = load();
    let expected = section_ids(&b.header);
    for ((type_id, body), (expected_type, expected_id)) in b.bodies().iter().zip(expected) {
        assert_eq!(*type_id, expected_type);
        let got = identity(*type_id, body);
        assert_eq!(got.header_id, b.header_id(), "type {type_id}: header id");
        assert_eq!(
            hex::encode(got.id),
            hex::encode(expected_id),
            "type {type_id}: recomputed id must equal the header-derived id"
        );
    }
}

#[test]
fn digests_match_the_fixture() {
    let b = load();
    assert_eq!(
        hex::encode(transactions_root(&b.txs, b.header.version)),
        hex::encode(b.header.transaction_root.0),
        "transactions root (block version 4: tx ids then witness ids)"
    );
    assert_eq!(
        hex::encode(extension_root(&b.fields)),
        hex::encode(b.extension_digest),
        "extension root"
    );
    assert_eq!(
        hex::encode(ad_proofs_digest(&b.proof)),
        hex::encode(b.proof_digest),
        "AD proofs digest"
    );
    assert_eq!(
        hex::encode(b.proof_digest),
        hex::encode(b.header.ad_proofs_root.0),
        "fixture premise: the header commits to the AD proof digest"
    );
}

#[test]
fn section_id_is_the_header_side_computation() {
    let b = load();
    let hid = b.header_id();
    let [(_, txs_id), (_, proofs_id), (_, ext_id)] = section_ids(&b.header);
    assert_eq!(
        section_id(
            BLOCK_TRANSACTIONS_TYPE_ID,
            &hid,
            &b.header.transaction_root.0
        ),
        txs_id
    );
    assert_eq!(
        section_id(AD_PROOFS_TYPE_ID, &hid, &b.header.ad_proofs_root.0),
        proofs_id
    );
    assert_eq!(
        section_id(EXTENSION_TYPE_ID, &hid, &b.header.extension_root.0),
        ext_id
    );
    assert_ne!(txs_id, proofs_id);
    assert_ne!(proofs_id, ext_id);
}

#[test]
fn witness_id_of_empty_proofs_is_the_hash_of_nothing() {
    let b = load();
    let tx = &b.txs[0];
    assert!(
        tx.inputs
            .iter()
            .all(|i| i.spending_proof.proof.as_ref().is_empty()),
        "fixture premise: tx 0 carries only empty proofs"
    );
    let expected = &blake2b256_hash(&[]).0[1..];
    assert_eq!(witness_id(tx), expected);

    let signed = &b.txs[1];
    assert!(
        signed
            .inputs
            .iter()
            .any(|i| !i.spending_proof.proof.as_ref().is_empty()),
        "fixture premise: tx 1 carries a non-empty proof"
    );
    assert_ne!(witness_id(signed), expected);
}

// ---------------------------------------------------------------------------
// Block-version sentinel semantics.
// ---------------------------------------------------------------------------

#[test]
fn version_one_bodies_commit_to_transaction_ids_only() {
    let b = load();
    let hid = b.header_id();
    let v1_id = section_id(
        BLOCK_TRANSACTIONS_TYPE_ID,
        &hid,
        &transactions_root(&b.txs, 1),
    );
    let v4_id = identity(BLOCK_TRANSACTIONS_TYPE_ID, &b.tx_body()).id;
    assert_ne!(v1_id, v4_id, "witness leaves must change the root");

    // No sentinel: the first VLQ is the count, block version 1.
    let bare = tx_body(&hid, 1, &b.txs);
    assert_eq!(identity(BLOCK_TRANSACTIONS_TYPE_ID, &bare).id, v1_id);

    // An explicit version-1 sentinel is accepted too (JVM: `verOrCount > 10M`).
    let mut explicit = hid.to_vec();
    vlq_u32(&mut explicit, BLOCK_VERSION_SENTINEL + 1);
    vlq_u32(&mut explicit, b.txs.len() as u32);
    for tx in &b.txs {
        explicit.extend(tx.sigma_serialize_bytes().unwrap());
    }
    assert_eq!(identity(BLOCK_TRANSACTIONS_TYPE_ID, &explicit).id, v1_id);

    // Every version above 1 uses the witness tree (JVM: `== InitialVersion`).
    assert_eq!(
        identity(BLOCK_TRANSACTIONS_TYPE_ID, &tx_body(&hid, 2, &b.txs)).id,
        v4_id
    );
}

// ---------------------------------------------------------------------------
// 2. Mutations change the id.
// ---------------------------------------------------------------------------

#[test]
fn flipping_a_payload_byte_changes_the_id() {
    let b = load();
    for (type_id, body) in b.bodies() {
        let original = identity(type_id, &body);
        // Well inside the payload: past the 32-byte header id and any VLQ
        // framing, and for transactions inside a spending proof so the
        // transaction still parses.
        let offset = match type_id {
            BLOCK_TRANSACTIONS_TYPE_ID => proof_offset(&body, &b.txs[1]) + 5,
            _ => body.len() / 2,
        };
        let mut mutated = body.clone();
        mutated[offset] ^= 0x01;
        let got = identity(type_id, &mutated);
        assert_eq!(got.header_id, original.header_id, "type {type_id}");
        assert_ne!(got.id, original.id, "type {type_id}: byte {offset}");
    }
}

/// Offset of tx's first non-empty spending proof inside `body`.
fn proof_offset(body: &[u8], tx: &Transaction) -> usize {
    let proof: &[u8] = tx
        .inputs
        .iter()
        .map(|i| i.spending_proof.proof.as_ref())
        .find(|p| !p.is_empty())
        .expect("a non-empty proof");
    body.windows(proof.len())
        .position(|w| w == proof)
        .expect("proof bytes appear verbatim in the body")
}

#[test]
fn reordering_transactions_changes_the_id() {
    let b = load();
    let hid = b.header_id();
    let original = identity(BLOCK_TRANSACTIONS_TYPE_ID, &b.tx_body());
    let swapped = vec![b.txs[1].clone(), b.txs[0].clone(), b.txs[2].clone()];
    let got = identity(
        BLOCK_TRANSACTIONS_TYPE_ID,
        &tx_body(&hid, b.header.version, &swapped),
    );
    assert_eq!(got.header_id, hid);
    assert_ne!(got.id, original.id);
}

#[test]
fn altering_a_spending_proof_changes_the_id() {
    let b = load();
    let body = b.tx_body();
    let original = identity(BLOCK_TRANSACTIONS_TYPE_ID, &body);
    let mut mutated = body.clone();
    let offset = proof_offset(&body, &b.txs[1]);
    mutated[offset] ^= 0x80;
    let got = identity(BLOCK_TRANSACTIONS_TYPE_ID, &mutated);
    assert_eq!(got.header_id, original.header_id);
    assert_ne!(
        got.id, original.id,
        "block version 4 commits to witness ids, so a proof byte moves the root"
    );
    // Same mutation under block version 1 leaves the id alone: only tx ids
    // are in the tree, and the proof is not part of the tx id.
    let v1 = tx_body(&b.header_id(), 1, &b.txs);
    let mut v1_mutated = v1.clone();
    let v1_offset = proof_offset(&v1, &b.txs[1]);
    v1_mutated[v1_offset] ^= 0x80;
    assert_eq!(
        identity(BLOCK_TRANSACTIONS_TYPE_ID, &v1_mutated).id,
        identity(BLOCK_TRANSACTIONS_TYPE_ID, &v1).id
    );
}

#[test]
fn header_id_prefix_is_reported_as_written() {
    let b = load();
    let other = [0xA5u8; 32];
    for (type_id, body) in b.bodies() {
        let original = identity(type_id, &body);
        let mut mutated = body.clone();
        mutated[..32].copy_from_slice(&other);
        let got = identity(type_id, &mutated);
        assert_eq!(
            got.header_id, other,
            "type {type_id}: header id follows the bytes"
        );
        assert_ne!(
            got.id, original.id,
            "type {type_id}: id covers the header id"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Malformed input errs.
// ---------------------------------------------------------------------------

#[test]
fn trailing_byte_is_rejected() {
    let b = load();
    for (type_id, mut body) in b.bodies() {
        body.push(0x00);
        let reason = rejected(type_id, &body);
        assert!(reason.contains("trailing"), "type {type_id}: {reason}");
    }
}

#[test]
fn truncated_body_is_rejected() {
    let b = load();
    for (type_id, body) in b.bodies() {
        rejected(type_id, &body[..body.len() - 1]);
        rejected(type_id, &body[..33]);
        rejected(type_id, &body[..32]);
        rejected(type_id, &body[..31]);
        rejected(type_id, &[]);
    }
}

#[test]
fn non_section_type_ids_are_rejected() {
    let b = load();
    for (_, body) in b.bodies() {
        for type_id in [HEADER_TYPE_ID, TRANSACTION_TYPE_ID, 0, 255, 103, 107] {
            let reason = rejected(type_id, &body);
            assert!(
                reason.contains("not a block section"),
                "{type_id}: {reason}"
            );
        }
    }
}

#[test]
fn block_version_beyond_u8_is_rejected() {
    let b = load();
    let mut body = b.header_id().to_vec();
    vlq_u32(&mut body, BLOCK_VERSION_SENTINEL + 300);
    vlq_u32(&mut body, b.txs.len() as u32);
    for tx in &b.txs {
        body.extend(tx.sigma_serialize_bytes().unwrap());
    }
    let reason = rejected(BLOCK_TRANSACTIONS_TYPE_ID, &body);
    assert!(reason.contains("300"), "{reason}");
}

#[test]
fn empty_transaction_list_is_rejected() {
    // JVM `BlockTransactions` asserts `txs.nonEmpty` in its constructor, so
    // an empty body never parses there either.
    let b = load();
    let bare = tx_body(&b.header_id(), 1, &[]);
    assert!(rejected(BLOCK_TRANSACTIONS_TYPE_ID, &bare).contains("no transactions"));
    let versioned = tx_body(&b.header_id(), 4, &[]);
    assert!(rejected(BLOCK_TRANSACTIONS_TYPE_ID, &versioned).contains("no transactions"));
}

#[test]
fn transaction_count_beyond_the_bytes_is_rejected() {
    let b = load();
    let mut body = b.header_id().to_vec();
    vlq_u32(&mut body, BLOCK_VERSION_SENTINEL + 4);
    vlq_u32(&mut body, u32::MAX);
    for tx in &b.txs {
        body.extend(tx.sigma_serialize_bytes().unwrap());
    }
    let reason = rejected(BLOCK_TRANSACTIONS_TYPE_ID, &body);
    assert!(reason.starts_with("transaction 3:"), "{reason}");
}

#[test]
fn extension_count_beyond_u16_is_rejected() {
    // JVM `getUShort` requires the VLQ value to fit 16 bits.
    let b = load();
    let mut body = b.header_id().to_vec();
    vlq_u32(&mut body, 65_536);
    for (key, value) in &b.fields {
        body.extend_from_slice(key);
        body.push(value.len() as u8);
        body.extend_from_slice(value);
    }
    let reason = rejected(EXTENSION_TYPE_ID, &body);
    assert!(reason.starts_with("field count"), "{reason}");
}

#[test]
fn proof_size_beyond_the_bytes_is_rejected() {
    let b = load();
    let mut body = b.header_id().to_vec();
    vlq_u32(&mut body, b.proof.len() as u32 + 1);
    body.extend_from_slice(&b.proof);
    let reason = rejected(AD_PROOFS_TYPE_ID, &body);
    assert!(reason.starts_with("proof bytes"), "{reason}");
}

// ---------------------------------------------------------------------------
// 4. Empty trees.
// ---------------------------------------------------------------------------

#[test]
fn empty_trees_hash_to_the_hash_of_nothing() {
    assert_eq!(hex::encode(transactions_root(&[], 1)), EMPTY_TREE_ROOT);
    assert_eq!(hex::encode(transactions_root(&[], 2)), EMPTY_TREE_ROOT);
    assert_eq!(hex::encode(extension_root(&[])), EMPTY_TREE_ROOT);
}

#[test]
fn empty_extension_body_binds_to_the_empty_root() {
    // The genesis extension: a header id and a zero count.
    let hid = [0x11u8; 32];
    let mut body = hid.to_vec();
    body.push(0x00);
    let got = identity(EXTENSION_TYPE_ID, &body);
    assert_eq!(got.header_id, hid);
    assert_eq!(
        got.id,
        section_id(EXTENSION_TYPE_ID, &hid, &hex32(EMPTY_TREE_ROOT))
    );
}

// ---------------------------------------------------------------------------
// 5. No panics: every prefix of every real body, plus noise.
// ---------------------------------------------------------------------------

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

#[test]
fn never_panics_on_truncations_or_noise() {
    let b = load();
    let bodies = b.bodies();
    let mut ok = 0usize;
    let mut err = 0usize;
    let mut tally = |r: Result<SectionIdentity, ChainError>| match r {
        Ok(_) => ok += 1,
        Err(_) => err += 1,
    };

    // Every prefix length of every real body.
    for (type_id, body) in &bodies {
        for n in 0..=body.len() {
            tally(section_id_from_body(*type_id, &body[..n]));
        }
    }

    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let types = [
        BLOCK_TRANSACTIONS_TYPE_ID,
        AD_PROOFS_TYPE_ID,
        EXTENSION_TYPE_ID,
    ];

    // Pure noise, random length, section or random type.
    for _ in 0..300 {
        let len = rng.below(600);
        let noise = rng.bytes(len);
        let type_id = if rng.below(4) == 0 {
            rng.next() as u8
        } else {
            types[rng.below(3)]
        };
        tally(section_id_from_body(type_id, &noise));
    }

    // A real body's framing with a random tail: the transaction parser sees
    // garbage after a valid header id, sentinel and count.
    for _ in 0..300 {
        let (type_id, body) = &bodies[rng.below(3)];
        let keep = 32 + rng.below(8);
        let mut fuzzed = body[..keep.min(body.len())].to_vec();
        let tail = rng.below(400);
        fuzzed.extend(rng.bytes(tail));
        tally(section_id_from_body(*type_id, &fuzzed));
    }

    // A real body with one byte replaced: nearly valid input, deep parse paths.
    for _ in 0..300 {
        let (type_id, body) = &bodies[rng.below(3)];
        let mut mutated = body.clone();
        let at = rng.below(mutated.len());
        mutated[at] = rng.next() as u8;
        tally(section_id_from_body(*type_id, &mutated));
    }

    assert!(ok > 0, "the sweep must include bodies that bind");
    assert!(err > 0, "the sweep must include bodies that fail");
}

// ---------------------------------------------------------------------------
// 6. The tree version a section's transactions are read at.
//
// SANTA X15, `Tuple(1, Upcast(1, Long))`, as a context-extension value. Below
// tree version 3 sigmastate writes an `Upcast` of a constant back as the
// constant (`ValueSerializer.scala:157-169`), and the fork computes a
// transaction's id at the version it read the transaction at, as ergo computes
// `serializedId` at parse (`ErgoTransaction.scala:68`). The same bytes read at
// 0 and at 3 are two transactions with two ids.
//
// Block versions 3 and 4 commit to the same leaves (ids, then witness ids), so
// a v3 and a v4 section carrying the same transaction bytes differ only in the
// version those bytes are read at.
// ---------------------------------------------------------------------------

/// [`upcast_tx`]'s id at 3: the hash of its bytes with the `Upcast` kept.
const UPCAST_TX_ID_AT_3: &str = "6b11fd0ea5f1e84782c15044f03cf10df48022ba7dcb8e6c0e48233b1cf9b0a1";
/// The id of the same bytes read at 0: the hash with the `Upcast` written as `1`.
const UPCAST_TX_ID_AT_0: &str = "b006736746ec23927fecfbaeb5a27bda4df791e5440eaba24e8aadb3072b3deb";
/// The section id a v4 header commits to for [`upcast_tx`] under
/// [`UPCAST_HEADER_ID`]: its root over the transaction as built, id at 3.
const UPCAST_SECTION_ID_AT_3: &str =
    "3008411c73c67ba3a40000ea3a4848f5a7ed11543a0998269d65d51198b904c7";
/// The section id the same transaction bytes give when read at 0.
const UPCAST_SECTION_ID_AT_0: &str =
    "19b7302a6820653cadb5ec78e3417c7386c92002dc9a443fba5ba631e3175428";

/// The header id the sections below claim.
const UPCAST_HEADER_ID: [u8; 32] = [0x44; 32];

/// `Upcast(1, Long)` as sigmastate writes it from tree version 3: the opcode,
/// the constant `1: Int`, the target type `Long`.
const UPCAST: [u8; 4] = [0x7e, 0x04, 0x02, 0x05];
/// `1: Int`, which sigmastate writes in the `Upcast`'s place below version 3.
const CONSTANT_1: [u8; 2] = [0x04, 0x02];

/// SANTA X15, `Tuple(1, Upcast(1, Long))`, built with the fork's API.
fn x15() -> EvaluatedValue {
    let one = || Expr::Const(1i32.into());
    EvaluatedValue::Expr(EvaluatedExpr::Tuple(vec![
        one(),
        Expr::Upcast(Upcast::new(one(), SType::SLong).unwrap()),
    ]))
}

/// One input spending box `0…0` with an empty proof and X15 as context
/// variable 0, and one 1 ERG `sigmaProp(true)` output. Built, so the fork
/// computes its id at 3, the version a v4 block's transactions are read at.
fn upcast_tx() -> Transaction {
    let mut extension = ContextExtension::empty();
    extension.values.insert(0, x15());
    let input = Input::new(
        BoxId::zero(),
        ProverResult {
            proof: ProofBytes::Empty,
            extension,
        },
    );
    let always = SigmaProp::new(SigmaBoolean::TrivialProp(true));
    let output = ErgoBoxCandidate {
        value: BoxValue::new(1_000_000_000).unwrap(),
        ergo_tree: ErgoTree::try_from(Expr::Const(Constant::from(always))).unwrap(),
        tokens: None,
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: 1,
    };
    Transaction::new_from_vec(vec![input], vec![], vec![output]).unwrap()
}

/// `bytes` with each [`UPCAST`] written as [`CONSTANT_1`], as a writer below
/// tree version 3 writes it.
fn without_upcast(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&first, tail)) = rest.split_first() {
        if let Some(after) = rest.strip_prefix(&UPCAST[..]) {
            out.extend_from_slice(&CONSTANT_1);
            rest = after;
        } else {
            out.push(first);
            rest = tail;
        }
    }
    out
}

#[test]
fn the_upcast_transaction_has_one_id_at_3_and_another_at_0() {
    let tx = upcast_tx();
    let at_3 = written_at(&tx, ErgoTreeVersion::V3);
    let at_0 = without_upcast(&at_3);
    assert_eq!(at_0.len(), at_3.len() - 2, "one Upcast, kept at 3");
    // The proof is empty, so the bytes to sign are the transaction's own
    // bytes, and its id is their hash.
    assert_eq!(hex::encode(tx.id().as_ref()), UPCAST_TX_ID_AT_3);
    assert_eq!(hex::encode(blake2b256_hash(&at_3).0), UPCAST_TX_ID_AT_3);
    let read_3 = read_at(&at_3, ErgoTreeVersion::V3);
    assert_eq!(hex::encode(read_3.id().as_ref()), UPCAST_TX_ID_AT_3);
    let read_0 = read_at(&at_3, ErgoTreeVersion::V0);
    assert_eq!(hex::encode(read_0.id().as_ref()), UPCAST_TX_ID_AT_0);
    assert_eq!(hex::encode(blake2b256_hash(&at_0).0), UPCAST_TX_ID_AT_0);
}

#[test]
fn a_v4_section_binds_under_the_root_its_header_computes_at_3() {
    let tx = upcast_tx();
    let header_side = section_id(
        BLOCK_TRANSACTIONS_TYPE_ID,
        &UPCAST_HEADER_ID,
        &transactions_root(std::slice::from_ref(&tx), 4),
    );
    assert_eq!(hex::encode(header_side), UPCAST_SECTION_ID_AT_3);

    let got = identity(
        BLOCK_TRANSACTIONS_TYPE_ID,
        &tx_body(&UPCAST_HEADER_ID, 4, &[tx]),
    );
    assert_eq!(got.header_id, UPCAST_HEADER_ID);
    assert_eq!(hex::encode(got.id), UPCAST_SECTION_ID_AT_3);
}

#[test]
fn a_v4_section_read_at_0_does_not_bind() {
    let tx = upcast_tx();
    let body = tx_body(&UPCAST_HEADER_ID, 4, std::slice::from_ref(&tx));
    let tx_bytes = written_at(&tx, ErgoTreeVersion::V3);
    assert!(
        body.ends_with(&tx_bytes),
        "the v4 body carries the bytes read below"
    );

    let read_0 = read_at(&tx_bytes, ErgoTreeVersion::V0);
    let at_0 = section_id(
        BLOCK_TRANSACTIONS_TYPE_ID,
        &UPCAST_HEADER_ID,
        &transactions_root(&[read_0], 4),
    );
    assert_eq!(hex::encode(at_0), UPCAST_SECTION_ID_AT_0);
    assert_ne!(at_0, identity(BLOCK_TRANSACTIONS_TYPE_ID, &body).id);
}

#[test]
fn a_v3_section_with_the_same_transaction_is_read_at_0() {
    // The JVM writes a v3 block's transactions at 3 and reads them at 0, so a
    // v3 body carries the same bytes as a v4 one, and they get the id the v4
    // body's would get if it were read at 0.
    let tx = upcast_tx();
    let v3 = tx_body(&UPCAST_HEADER_ID, 3, std::slice::from_ref(&tx));
    let v4 = tx_body(&UPCAST_HEADER_ID, 4, std::slice::from_ref(&tx));
    let tx_bytes = written_at(&tx, ErgoTreeVersion::V3);
    assert!(v3.ends_with(&tx_bytes) && v4.ends_with(&tx_bytes));
    assert_eq!(v3.len(), v4.len());

    let got = identity(BLOCK_TRANSACTIONS_TYPE_ID, &v3);
    assert_eq!(got.header_id, UPCAST_HEADER_ID);
    assert_eq!(hex::encode(got.id), UPCAST_SECTION_ID_AT_0);
    assert_ne!(got.id, identity(BLOCK_TRANSACTIONS_TYPE_ID, &v4).id);
}

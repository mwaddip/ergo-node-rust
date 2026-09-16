//! Identity of the three non-header block sections.
//!
//! [`section_ids`] derives the ids from a header's roots; [`section_id_from_body`]
//! recomputes the same ids from a delivered body, so the receive path can
//! check the label a peer attached to the bytes against the bytes themselves
//! (`facts/receive-path.md`). The root computations — [`transactions_root`],
//! [`extension_root`], [`ad_proofs_digest`] — are the single implementations
//! in the workspace: mining builds candidate headers with them, and this
//! crate rebuilds delivered sections with them.

use std::io::Cursor;

use ergo_chain_types::blake2b256_hash;
use ergo_chain_types::Header;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::serialization::sigma_byte_reader::{from_bytes, SigmaByteRead};
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_merkle_tree::{MerkleNode, MerkleTree};
use sigma_ser::vlq_encode::{ReadSigmaVlqExt, VlqEncodingError};

use crate::error::ChainError;
use crate::state_type::StateType;

/// Modifier type IDs (Ergo `NetworkObjectTypeId`).
///
/// `HEADER_TYPE_ID` and the three section IDs identify the components of a
/// full block. `TRANSACTION_TYPE_ID` identifies an *unconfirmed* transaction
/// modifier (mempool / inv-relay), not a block section — it's not returned
/// by [`section_ids`] or [`required_section_ids`].
pub const HEADER_TYPE_ID: u8 = 101;
pub const BLOCK_TRANSACTIONS_TYPE_ID: u8 = 102;
pub const AD_PROOFS_TYPE_ID: u8 = 104;
pub const EXTENSION_TYPE_ID: u8 = 108;

/// Modifier type ID for unconfirmed transactions (mempool / inv-relay).
///
/// Distinct from `BLOCK_TRANSACTIONS_TYPE_ID` (102), which is the block
/// section carrying confirmed transactions. This constant is the modifier
/// type used when relaying or requesting individual unconfirmed transactions.
pub const TRANSACTION_TYPE_ID: u8 = 2;

/// A BlockTransactions body whose first VLQ exceeds this carries
/// `BLOCK_VERSION_SENTINEL + block_version` there and the transaction count
/// in a second VLQ; otherwise the first VLQ is the count and the block
/// version is 1 (JVM `BlockTransactionsSerializer.MaxTransactionsInBlock`).
const BLOCK_VERSION_SENTINEL: u32 = 10_000_000;

/// Compute the modifier IDs for the three non-header block sections.
///
/// Returns `[(type_id, modifier_id); 3]` for BlockTransactions, ADProofs,
/// and Extension. Each modifier ID is `Blake2b256(type_id || header.id || section_root)`.
///
/// Matches JVM `Header.sectionIds`. For mode-filtered sections, use
/// [`required_section_ids`] instead.
pub fn section_ids(header: &Header) -> [(u8, [u8; 32]); 3] {
    [
        (
            BLOCK_TRANSACTIONS_TYPE_ID,
            section_id(
                BLOCK_TRANSACTIONS_TYPE_ID,
                &header.id.0 .0,
                &header.transaction_root.0,
            ),
        ),
        (
            AD_PROOFS_TYPE_ID,
            section_id(AD_PROOFS_TYPE_ID, &header.id.0 .0, &header.ad_proofs_root.0),
        ),
        (
            EXTENSION_TYPE_ID,
            section_id(EXTENSION_TYPE_ID, &header.id.0 .0, &header.extension_root.0),
        ),
    ]
}

/// Block sections required for a given header and node state type.
///
/// Mirrors JVM's `ToDownloadProcessor.requiredModifiersForHeader`:
/// - UTXO mode → `sectionIdsWithNoProof` (BlockTransactions + Extension)
/// - Digest mode → `sectionIds` (all three including ADProofs)
/// - Light mode → empty Vec; light clients download no block sections.
///   Returning empty here lets sync's section-queue construction handle
///   `Light` without a special case at the call site.
pub fn required_section_ids(header: &Header, state_type: StateType) -> Vec<(u8, [u8; 32])> {
    match state_type {
        StateType::Light => Vec::new(),
        StateType::Digest => section_ids(header).to_vec(),
        StateType::Utxo => section_ids(header)
            .iter()
            .filter(|(type_id, _)| *type_id != AD_PROOFS_TYPE_ID)
            .copied()
            .collect(),
    }
}

/// `Blake2b256(type_id || header_id || digest)` — the modifier id of a
/// non-header block section (JVM `NonHeaderBlockSection.computeIdBytes`,
/// Scorex `Algos.hash.prefixedHash`).
///
/// [`section_ids`] is this function applied to a header's three roots;
/// [`section_id_from_body`] applies it to a digest recomputed from a
/// delivered body.
pub fn section_id(type_id: u8, header_id: &[u8; 32], digest: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 65];
    buf[0] = type_id;
    buf[1..33].copy_from_slice(header_id);
    buf[33..].copy_from_slice(digest);
    blake2b256_hash(&buf).0
}

/// The Merkle root a header commits to in `transaction_root`
/// (JVM `BlockTransactions.transactionsRoot`).
///
/// For block version 1 the leaves are the transaction ids; for every other
/// version they are the transaction ids followed by the witness ids — two
/// concatenated lists, never interleaved (the JVM branches on
/// `blockVersion == Header.InitialVersion`). An empty `txs` yields the
/// empty-tree root `Blake2b256(no bytes)` (JVM `Algos.emptyMerkleTreeRoot`).
pub fn transactions_root(txs: &[Transaction], block_version: u8) -> [u8; 32] {
    let with_witnesses = block_version != 1;
    let mut leaves = Vec::with_capacity(if with_witnesses {
        2 * txs.len()
    } else {
        txs.len()
    });
    leaves.extend(
        txs.iter()
            .map(|tx| MerkleNode::from_bytes(tx.id().as_ref())),
    );
    if with_witnesses {
        leaves.extend(txs.iter().map(|tx| MerkleNode::from_bytes(witness_id(tx))));
    }
    MerkleTree::new(leaves).root_hash_special().into()
}

/// Witness id of a transaction: `Blake2b256(concat(inputs[*].spending_proof.proof))`
/// with the first byte dropped — 31 bytes (JVM `ErgoTransaction.witnessSerializedId`).
///
/// The truncation is what tells a witness leaf apart from a 32-byte id
/// leaf in the transactions tree. Empty proofs (storage-rent and emission
/// spends) contribute no bytes to the concatenation.
pub fn witness_id(tx: &Transaction) -> [u8; 31] {
    let mut proofs = Vec::new();
    for input in tx.inputs.iter() {
        proofs.extend_from_slice(input.spending_proof.proof.as_ref());
    }
    let hash = blake2b256_hash(&proofs).0;
    let mut id = [0u8; 31];
    id.copy_from_slice(&hash[1..]);
    id
}

/// Merkle root of extension fields over leaves `len(key) || key || value`
/// (JVM `Extension.kvToLeaf`), in field order.
///
/// Empty `fields` → the empty-tree root `Blake2b256(no bytes)`
/// (JVM `Algos.merkleTreeRoot`), which is what the genesis header carries.
pub fn extension_root(fields: &[([u8; 2], Vec<u8>)]) -> [u8; 32] {
    let leaves: Vec<MerkleNode> = fields
        .iter()
        .map(|(key, value)| {
            let mut leaf = Vec::with_capacity(1 + key.len() + value.len());
            leaf.push(key.len() as u8);
            leaf.extend_from_slice(key);
            leaf.extend_from_slice(value);
            MerkleNode::from_bytes(leaf)
        })
        .collect();
    MerkleTree::new(leaves).root_hash_special().into()
}

/// `Blake2b256(proof_bytes)` (JVM `ADProofs.proofDigest`).
pub fn ad_proofs_digest(proof_bytes: &[u8]) -> [u8; 32] {
    blake2b256_hash(proof_bytes).0
}

/// What a delivered section body says about itself, recomputed from its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionIdentity {
    /// The header this body claims to belong to: its first 32 bytes.
    pub header_id: [u8; 32],
    /// [`section_id`]`(type_id, header_id, digest)` with `digest`
    /// recomputed from the body.
    pub id: [u8; 32],
}

/// Recompute a block section's modifier id from its wire bytes.
///
/// `Ok` iff `body` parses completely as the section's wire format — every
/// byte consumed — and then `id` is the id the JVM computes for the same
/// bytes (`NonHeaderBlockSection.id`, the value `bsCorrespondsToHeader`
/// checks against `Header.sectionIds`). Integers are VLQ unless stated:
///
/// - **102 BlockTransactions**: `header_id[32] || ver_or_count: u32 || [count: u32] || txs`.
///   `ver_or_count > 10_000_000` means `block_version = ver_or_count - 10_000_000`
///   and a separate `count` follows; otherwise the block version is 1 and
///   `ver_or_count` is the count. The version must fit `u8`; a count of 0
///   is rejected as the JVM does (`BlockTransactions` asserts `txs.nonEmpty`).
///   Each transaction is parsed with `Transaction::sigma_parse`;
///   `digest = transactions_root(txs, block_version)`.
/// - **104 ADProofs**: `header_id[32] || size: u32 || proof_bytes[size]`;
///   `digest = ad_proofs_digest(proof_bytes)`.
/// - **108 Extension**: `header_id[32] || count: u16 || (key[2] || len: u8 || value[len]) × count`
///   where `len` is a raw byte; `digest = extension_root(fields)`.
///
/// Any other `type_id`, any parse failure, or trailing bytes →
/// [`ChainError::Section`]. Pure: it does not know whether `header_id`
/// names a header the node holds — that is the caller's request gate.
///
/// This function faces the network. Counts bound loop trips, never
/// allocations: a count larger than the bytes behind it fails on the first
/// missing byte. No input panics.
pub fn section_id_from_body(type_id: u8, body: &[u8]) -> Result<SectionIdentity, ChainError> {
    let mut body = SectionBody {
        type_id,
        bytes: body,
        pos: 0,
    };
    let header_id: [u8; 32] = body.take_array("header id")?;
    let digest = match type_id {
        BLOCK_TRANSACTIONS_TYPE_ID => {
            let ver_or_count = body.vlq_u32("block version or transaction count")?;
            let (block_version, count) = if ver_or_count > BLOCK_VERSION_SENTINEL {
                let version = ver_or_count - BLOCK_VERSION_SENTINEL;
                let version = u8::try_from(version)
                    .map_err(|_| body.err(format!("block version {version} does not fit u8")))?;
                (version, body.vlq_u32("transaction count")?)
            } else {
                (1, ver_or_count)
            };
            if count == 0 {
                return Err(body.err("no transactions: a block carries at least one"));
            }
            let txs = body.transactions(count)?;
            transactions_root(&txs, block_version)
        }
        AD_PROOFS_TYPE_ID => {
            let size = body.vlq_u32("proof size")?;
            let size = usize::try_from(size)
                .map_err(|_| body.err(format!("proof size {size} does not fit usize")))?;
            ad_proofs_digest(body.take(size, "proof bytes")?)
        }
        EXTENSION_TYPE_ID => {
            let count = body.vlq_u16("field count")?;
            let mut fields = Vec::new();
            for i in 0..count {
                let key: [u8; 2] = body.take_array(&format!("field {i} key"))?;
                let len = body.u8(&format!("field {i} value length"))?;
                let value = body.take(usize::from(len), &format!("field {i} value"))?;
                fields.push((key, value.to_vec()));
            }
            extension_root(&fields)
        }
        _ => {
            return Err(body.err("not a block section type (expected 102, 104 or 108)"));
        }
    };
    body.finish()?;
    Ok(SectionIdentity {
        header_id,
        id: section_id(type_id, &header_id, &digest),
    })
}

/// Bounds-checked cursor over a section body. Every read is a `Result`:
/// nothing is sliced without a length check, and nothing is allocated from
/// a count before the bytes it counts have been read.
struct SectionBody<'a> {
    type_id: u8,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> SectionBody<'a> {
    fn err(&self, reason: impl Into<String>) -> ChainError {
        ChainError::Section {
            type_id: self.type_id,
            reason: reason.into(),
        }
    }

    /// The unread tail; empty once the body is consumed.
    fn remaining(&self) -> &'a [u8] {
        self.bytes.get(self.pos..).unwrap_or(&[])
    }

    /// Take the next `n` bytes, or fail naming the field that ran short.
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], ChainError> {
        let rest = self.remaining();
        let (head, _) = rest.split_at_checked(n).ok_or_else(|| {
            self.err(format!(
                "{what}: need {n} bytes at offset {}, {} remain",
                self.pos,
                rest.len()
            ))
        })?;
        self.pos += n;
        Ok(head)
    }

    fn take_array<const N: usize>(&mut self, what: &str) -> Result<[u8; N], ChainError> {
        let bytes = self.take(N, what)?;
        bytes
            .try_into()
            .map_err(|_| self.err(format!("{what}: expected {N} bytes")))
    }

    fn u8(&mut self, what: &str) -> Result<u8, ChainError> {
        self.take_array::<1>(what).map(|[b]| b)
    }

    /// Decode one VLQ value with sigma-ser's reader — the decoder the
    /// transaction parser and the JVM port share — then advance past the
    /// bytes it consumed.
    fn vlq<T>(
        &mut self,
        what: &str,
        read: impl FnOnce(&mut Cursor<&'a [u8]>) -> Result<T, VlqEncodingError>,
    ) -> Result<T, ChainError> {
        let mut cursor = Cursor::new(self.remaining());
        let value = read(&mut cursor).map_err(|e| self.err(format!("{what}: {e}")))?;
        self.pos += usize::try_from(cursor.position())
            .map_err(|_| self.err(format!("{what}: position does not fit usize")))?;
        Ok(value)
    }

    fn vlq_u32(&mut self, what: &str) -> Result<u32, ChainError> {
        self.vlq(what, |c| c.get_u32())
    }

    /// Range-checked like JVM `getUShort`: a VLQ value above 65535 is an error.
    fn vlq_u16(&mut self, what: &str) -> Result<u16, ChainError> {
        self.vlq(what, |c| c.get_u16())
    }

    /// Parse `count` transactions with `Transaction::sigma_parse`, pushing
    /// each as it is read, then advance past the bytes the parser consumed.
    fn transactions(&mut self, count: u32) -> Result<Vec<Transaction>, ChainError> {
        let mut reader = from_bytes(self.remaining());
        let mut txs = Vec::new();
        for i in 0..count {
            let tx = Transaction::sigma_parse(&mut reader)
                .map_err(|e| self.err(format!("transaction {i}: {e}")))?;
            txs.push(tx);
        }
        let used = reader
            .position()
            .map_err(|e| self.err(format!("transaction stream position: {e}")))?;
        self.pos += usize::try_from(used)
            .map_err(|_| self.err("transaction stream position does not fit usize"))?;
        Ok(txs)
    }

    /// The body must end where the section ends.
    fn finish(self) -> Result<(), ChainError> {
        let trailing = self.remaining().len();
        if trailing != 0 {
            return Err(self.err(format!(
                "{trailing} trailing bytes after the section at offset {}",
                self.pos
            )));
        }
        Ok(())
    }
}

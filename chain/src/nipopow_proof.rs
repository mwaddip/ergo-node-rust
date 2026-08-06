//! NiPoPoW proof construction and verification (Phase 6).
//!
//! Wraps `ergo-nipopow` for build/verify on the local header chain. Received
//! proofs are bound to their request parameters and configured genesis before
//! their exact prefix/suffix split is returned to the light-bootstrap layer.
//! Chain-state mutation remains the responsibility of that consumer.
//!
//! JVM reference:
//! - `ergo-core/src/main/scala/org/ergoplatform/modifiers/history/popow/NipopowProof.scala`
//! - `ergo-core/src/main/scala/org/ergoplatform/modifiers/history/popow/NipopowAlgos.scala`

use ergo_chain_types::{BlockId, ExtensionCandidate, Header};
use ergo_nipopow::{NipopowAlgos, NipopowProof, PoPowHeader, PopowHeaderReader};
use sigma_ser::ScorexSerializable;

use crate::chain::HeaderChain;
use crate::error::ChainError;

/// Cap on the m and k security parameters.
///
/// Both must be ≥ 1 and ≤ this value. Prevents pathological calls and
/// caps the proof size for sanity.
pub const MAX_M_K: u32 = 256;

/// Request and trust-anchor values a received proof must match exactly.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NipopowVerificationContext {
    pub expected_m: u32,
    pub expected_k: u32,
    pub expected_genesis_id: BlockId,
}

impl NipopowVerificationContext {
    /// Build a context from the chain's configured trust anchor.
    pub fn from_chain(
        chain: &HeaderChain,
        expected_m: u32,
        expected_k: u32,
    ) -> Result<Self, ChainError> {
        let expected_genesis_id = chain
            .configured_genesis_id()
            .ok_or_else(|| ChainError::Nipopow("configured genesis id is absent".into()))?;
        Ok(Self {
            expected_m,
            expected_k,
            expected_genesis_id,
        })
    }

    fn validate(&self) -> Result<(), ChainError> {
        if self.expected_m == 0
            || self.expected_k == 0
            || self.expected_m > MAX_M_K
            || self.expected_k > MAX_M_K
        {
            return Err(ChainError::Nipopow(format!(
                "invalid verification context: m={}, k={}, max={MAX_M_K}",
                self.expected_m, self.expected_k
            )));
        }
        Ok(())
    }
}

/// Lossless result of context-bound NiPoPoW proof verification.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NipopowVerificationResult {
    pub m: u32,
    pub k: u32,
    /// JVM terminal mode, or `None` for a Rust-core-only payload.
    pub continuous: Option<bool>,
    pub prefix: Vec<Header>,
    pub suffix_head: Header,
    pub suffix_tail: Vec<Header>,
}

impl NipopowVerificationResult {
    pub fn total_headers(&self) -> usize {
        self.prefix.len() + 1 + self.suffix_tail.len()
    }

    pub fn suffix_tip_height(&self) -> u32 {
        self.suffix_tail.last().unwrap_or(&self.suffix_head).height
    }

    pub fn headers(&self) -> impl Iterator<Item = &Header> {
        self.prefix
            .iter()
            .chain(std::iter::once(&self.suffix_head))
            .chain(self.suffix_tail.iter())
    }
}

/// Structurally and cryptographically checked metadata for diagnostics only.
///
/// This type intentionally carries no headers and cannot authorize bootstrap
/// installation because it is not bound to a request or configured genesis.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NipopowInspection {
    pub m: u32,
    pub k: u32,
    pub continuous: Option<bool>,
    pub suffix_tip_height: u32,
    pub total_headers: usize,
}

/// `PopowHeaderReader` adapter over the local `HeaderChain`.
///
/// Backs [`build_nipopow_proof`] via
/// [`ergo_nipopow::NipopowAlgos::prove_with_reader`]. The reader walks the
/// interlink hierarchy on demand and only fetches the popow headers the
/// proof actually visits — `O(m + k + m · log₂ N)` per call instead of `O(N)`.
///
/// **Genesis special case**: the genesis block has empty interlinks and an
/// empty proof. The extension loader MUST NOT be called for `height == 1`
/// because real testnet/mainnet genesis extensions are empty. The
/// `popow_header_at_height(1)` and `popow_header_by_id(genesis_id)` paths
/// synthesize the popow header in-process; every other height path goes
/// through the loader as normal.
struct ChainPopowReader<'a> {
    chain: &'a HeaderChain,
}

impl<'a> ChainPopowReader<'a> {
    /// Synthesize the canonical genesis `PoPowHeader` without consulting the
    /// extension loader.
    fn build_genesis_popow_header(header: Header) -> Option<PoPowHeader> {
        let extension_candidate = ExtensionCandidate::default();
        let interlinks_proof = NipopowAlgos::proof_for_interlink_vector(&extension_candidate)?;
        Some(PoPowHeader {
            header,
            interlinks: Vec::new(),
            interlinks_proof,
        })
    }
}

impl<'a> PopowHeaderReader for ChainPopowReader<'a> {
    fn headers_height(&self) -> u32 {
        self.chain.height()
    }

    fn popow_header_by_id(&self, id: &BlockId) -> Option<PoPowHeader> {
        let h = self.chain.height_of(id)?;
        self.popow_header_at_height(h)
    }

    fn popow_header_at_height(&self, height: u32) -> Option<PoPowHeader> {
        let header = self.chain.header_at(height)?;

        // Genesis: synthesize in-process. NEVER call the loader for h=1 —
        // Real genesis extensions and canonical interlinks/proof are empty.
        if height == 1 {
            return Self::build_genesis_popow_header(header);
        }

        // h >= 2: load real extension bytes, unpack canonical interlinks.
        let loader = self.chain.extension_loader()?;
        let ext_bytes = loader(height)?;
        let (parsed_header_id, fields) =
            crate::voting::parse_extension_bytes(&ext_bytes).ok()?;
        // The loader is not trusted to return matching data: an upstream
        // backward-walk recovery (e.g., enr-store papering over BEST_CHAIN
        // holes) can return extension bytes for a different block at the
        // queried height. Returning silently with wrong interlinks would
        // produce a `PoPowHeader` whose `header` is from height `h` but
        // whose `interlinks` belong to some other block — `prove_with_reader`
        // would then walk into that wrong lineage and emit a prefix whose
        // adjacent entries don't link via interlink, failing
        // `NipopowProof::has_valid_connections()` at verify time. Reject
        // mismatched bytes here so the walk surfaces as `MissingPopowHeader`,
        // which `build_nipopow_proof` maps to `ChainError::Nipopow`. Clean
        // fail beats silent corruption.
        if parsed_header_id != header.id {
            return None;
        }
        let extension_candidate = ExtensionCandidate::new(fields).ok()?;
        let interlinks = NipopowAlgos::unpack_interlinks(&extension_candidate).ok()?;
        let interlinks_proof = NipopowAlgos::proof_for_interlink_vector(&extension_candidate)?;
        Some(PoPowHeader {
            header,
            interlinks,
            interlinks_proof,
        })
    }

    fn last_headers(&self, k: usize) -> Vec<Header> {
        let height = self.chain.height();
        let k_u32 = k as u32;
        if k == 0 || k_u32 > height {
            // sigma-rust handles `last.len() < k` with `ChainTooShort`; we
            // surface the same condition by returning an empty vec.
            return Vec::new();
        }
        let start = height - k_u32 + 1;
        self.chain.headers_from(start, k)
    }

    fn best_headers_after(&self, header: &Header, n: usize) -> Vec<Header> {
        if n == 0 {
            return Vec::new();
        }
        let start = match header.height.checked_add(1) {
            Some(s) => s,
            None => return Vec::new(),
        };
        self.chain.headers_from(start, n)
    }
}

/// Build a NiPoPoW proof for the local chain.
///
/// **Preconditions**:
/// - `1 <= m, k <= MAX_M_K`
/// - The chain must contain at least `m + k` headers
/// - If `header_id` is `Some`, it must be in the chain (the suffix tip)
/// - An extension loader must be set on the chain (for fetching interlinks)
///
/// Returns the inner serialized NiPoPoW proof bytes — NO P2P envelope.
/// The main crate handles message wrapping when sending.
///
/// **Algorithm**: thin adapter over
/// [`ergo_nipopow::NipopowAlgos::prove_with_reader`] using
/// [`ChainPopowReader`]. The reader walks the interlink hierarchy on demand
/// and only fetches the popow headers the proof actually visits —
/// `O(m + k + m · log₂ N)` instead of `O(N)`. See `facts/chain.md` Phase 6.
pub fn build_nipopow_proof(
    chain: &HeaderChain,
    m: u32,
    k: u32,
    header_id: Option<BlockId>,
) -> Result<Vec<u8>, ChainError> {
    if m == 0 || k == 0 {
        return Err(ChainError::Nipopow("m and k must be >= 1".into()));
    }
    if m > MAX_M_K || k > MAX_M_K {
        return Err(ChainError::Nipopow(format!(
            "m and k must be <= {MAX_M_K}"
        )));
    }

    // The reader's h>=2 path delegates to the loader, so it must be wired.
    // (The h==1 path synthesizes genesis in-process and doesn't touch it.)
    if chain.extension_loader().is_none() {
        return Err(ChainError::Nipopow("extension loader not set".into()));
    }

    let reader = ChainPopowReader { chain };
    let proof = NipopowAlgos::default()
        .prove_with_reader(&reader, header_id.as_ref(), k, m)
        .map_err(|e| ChainError::Nipopow(format!("prove_with_reader failed: {e:?}")))?;

    proof
        .scorex_serialize_bytes()
        .map_err(|e| ChainError::Nipopow(format!("serialize failed: {e:?}")))
}

/// Fetch a single popow header by its block id and return the
/// Scorex-serialized bytes.
///
/// **Semantics**:
/// - `Ok(Some(bytes))` — header exists in the chain; `bytes` is the
///   Scorex-serialized `PoPowHeader` (header + interlinks +
///   interlinksProof). Maps to HTTP 200 at the API layer.
/// - `Ok(None)` — header not in the chain. Maps to HTTP 404.
/// - `Err(ChainError::Nipopow)` — header is in the chain but the
///   `PoPowHeader` could not be constructed (missing extension bytes,
///   parse failure, etc). Maps to HTTP 500.
///
/// Thin adapter over [`ChainPopowReader::popow_header_by_id`], with the
/// extra step of separating "not found" from "internal failure" since
/// the trait collapses both into `None`.
pub fn popow_header_by_id(
    chain: &HeaderChain,
    id: &BlockId,
) -> Result<Option<Vec<u8>>, ChainError> {
    // Header not in chain → 404, not a server error.
    if chain.height_of(id).is_none() {
        return Ok(None);
    }

    // Header IS in chain; if we can't materialize the PoPowHeader from
    // here it's a real failure (missing extension, parse error, etc).
    let reader = ChainPopowReader { chain };
    let popow_header = reader.popow_header_by_id(id).ok_or_else(|| {
        ChainError::Nipopow(format!(
            "failed to construct PoPowHeader for {id:?}"
        ))
    })?;

    let bytes = popow_header
        .scorex_serialize_bytes()
        .map_err(|e| ChainError::Nipopow(format!("serialize failed: {e:?}")))?;

    Ok(Some(bytes))
}

/// Compare two NiPoPoW proofs from raw bytes (post-envelope, inner proof
/// payload only). Returns `true` if `a` represents a better chain than `b`
/// per KMZ17 §4.3.
///
/// Both byte slices must already have passed [`verify_nipopow_proof_bytes`]
/// against the same [`NipopowVerificationContext`]. This function validates
/// both proofs again, and sigma-rust rejects unequal `m`/`k` parameters during
/// comparison. Parse or validation failure on either side returns an error.
pub fn compare_nipopow_proof_bytes(a: &[u8], b: &[u8]) -> Result<bool, ChainError> {
    let (proof_a, _) = parse_received_nipopow_proof(a)
        .map_err(|e| ChainError::Nipopow(format!("parse proof A failed: {e}")))?;
    let (proof_b, _) = parse_received_nipopow_proof(b)
        .map_err(|e| ChainError::Nipopow(format!("parse proof B failed: {e}")))?;
    proof_a
        .is_better_than(&proof_b)
        .map_err(|e| ChainError::Nipopow(format!("comparison failed: {e:?}")))
}

fn parse_received_nipopow_proof(bytes: &[u8]) -> Result<(NipopowProof, Option<bool>), ChainError> {
    if bytes.is_empty() {
        return Err(ChainError::Nipopow("empty proof bytes".into()));
    }

    let mut cursor = std::io::Cursor::new(bytes);
    let proof = NipopowProof::scorex_parse(&mut cursor)
        .map_err(|e| ChainError::Nipopow(format!("parse failed: {e:?}")))?;
    let consumed = usize::try_from(cursor.position())
        .map_err(|_| ChainError::Nipopow("proof cursor position does not fit usize".into()))?;
    let remaining = bytes
        .get(consumed..)
        .ok_or_else(|| ChainError::Nipopow("proof cursor exceeded input".into()))?;
    let continuous = match remaining {
        [] => None,
        [0] => Some(false),
        [1] => Some(true),
        [mode] => {
            return Err(ChainError::Nipopow(format!(
                "invalid JVM continuous mode byte {mode}"
            )))
        }
        _ => {
            return Err(ChainError::Nipopow(format!(
                "unexpected {} trailing bytes after proof core",
                remaining.len()
            )))
        }
    };

    Ok((proof, continuous))
}

fn validate_received_nipopow_proof(proof: &NipopowProof) -> Result<(), ChainError> {
    proof
        .validate()
        .map_err(|e| ChainError::Nipopow(format!("proof validation failed: {e}")))?;
    Ok(())
}

fn verify_nipopow_headers_pow(proof: &NipopowProof) -> Result<(), ChainError> {
    for header in proof
        .prefix
        .iter()
        .map(|p| &p.header)
        .chain(std::iter::once(&proof.suffix_head.header))
        .chain(proof.suffix_tail.iter())
    {
        crate::verify_pow(header)?;
    }
    Ok(())
}

/// Verify a NiPoPoW proof from raw bytes.
///
/// **Precondition**: `bytes` is the inner NiPoPoW proof payload (the main
/// crate has stripped any P2P message envelope).
///
/// The proof must pass canonical sigma-rust validation, match `context`
/// exactly, bind its first proof-chain header to the configured genesis, and
/// pass [`crate::verify_pow`] for every extracted header.
///
/// Does NOT touch chain state. Does NOT apply the proof to local chain.
/// The returned result preserves the parsed prefix and suffix boundary so an
/// installer never has to reconstruct it from a flattened header vector.
pub fn verify_nipopow_proof_bytes(
    bytes: &[u8],
    context: &NipopowVerificationContext,
) -> Result<NipopowVerificationResult, ChainError> {
    verify_inner(bytes, context, true)
}

/// Inspect a proof for diagnostics without binding it to a request or genesis.
///
/// The returned type intentionally contains no headers and MUST NOT authorize
/// bootstrap selection or installation.
pub fn inspect_nipopow_proof_bytes(bytes: &[u8]) -> Result<NipopowInspection, ChainError> {
    let (proof, continuous) = parse_received_nipopow_proof(bytes)?;
    validate_received_nipopow_proof(&proof)?;
    verify_nipopow_headers_pow(&proof)?;
    let suffix_tip_height = proof
        .suffix_tail
        .last()
        .unwrap_or(&proof.suffix_head.header)
        .height;
    let total_headers = proof.prefix.len() + 1 + proof.suffix_tail.len();
    Ok(NipopowInspection {
        m: proof.m,
        k: proof.k,
        continuous,
        suffix_tip_height,
        total_headers,
    })
}

/// Test-only: verify a NiPoPoW proof without running the per-header PoW
/// check. Used by unit tests on synthetic chains where headers don't have
/// real Autolykos solutions.
#[cfg(test)]
pub(crate) fn verify_nipopow_proof_bytes_no_pow(
    bytes: &[u8],
    context: &NipopowVerificationContext,
) -> Result<NipopowVerificationResult, ChainError> {
    verify_inner(bytes, context, false)
}

fn verify_inner(
    bytes: &[u8],
    context: &NipopowVerificationContext,
    check_pow: bool,
) -> Result<NipopowVerificationResult, ChainError> {
    context.validate()?;
    let (proof, continuous) = parse_received_nipopow_proof(bytes)?;
    validate_received_nipopow_proof(&proof)?;

    if proof.m != context.expected_m {
        return Err(ChainError::Nipopow(format!(
            "expected m {}, got {}",
            context.expected_m, proof.m
        )));
    }
    if proof.k != context.expected_k {
        return Err(ChainError::Nipopow(format!(
            "expected k {}, got {}",
            context.expected_k, proof.k
        )));
    }

    let proof_genesis_id = proof
        .prefix
        .first()
        .map(|p| p.header.id)
        .unwrap_or(proof.suffix_head.header.id);
    if proof_genesis_id != context.expected_genesis_id {
        return Err(ChainError::Nipopow(format!(
            "proof genesis {proof_genesis_id} does not match configured genesis {}",
            context.expected_genesis_id
        )));
    }

    if check_pow {
        verify_nipopow_headers_pow(&proof)?;
    }

    let NipopowProof {
        m,
        k,
        prefix,
        suffix_head,
        suffix_tail,
        ..
    } = proof;
    Ok(NipopowVerificationResult {
        m,
        k,
        continuous,
        prefix: prefix.into_iter().map(|p| p.header).collect(),
        suffix_head: suffix_head.header,
        suffix_tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voting::pack_extension_bytes;
    use crate::{ChainConfig, HeaderChain};
    use ergo_chain_types::{
        ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, Votes,
    };
    use ergo_merkle_tree::{MerkleNode, MerkleTree};
    use sigma_ser::ScorexSerializable;
    use std::sync::{Arc, Mutex};

    fn extension_root(fields: &[([u8; 2], Vec<u8>)]) -> Digest32 {
        MerkleTree::new(
            fields
                .iter()
                .map(|(key, value)| {
                    std::iter::once(2u8)
                        .chain(key.iter().copied())
                        .chain(value.iter().copied())
                        .collect::<Vec<_>>()
                })
                .map(MerkleNode::from_bytes)
                .collect::<Vec<_>>(),
        )
        .root_hash_special()
    }

    fn make_synthetic_header_with_extension_root(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
        extension_root: Digest32,
    ) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp,
            n_bits,
            height,
            extension_root,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: height.to_be_bytes().repeat(2),
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    fn make_synthetic_header(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
    ) -> Header {
        make_synthetic_header_with_extension_root(
            height,
            parent_id,
            timestamp,
            n_bits,
            Digest32::zero(),
        )
    }

    /// Build a synthetic chain of `count` headers + a per-height extension
    /// store containing interlink fields. Returns the chain (with loader
    /// already wired) and the synthetic store.
    fn build_chain_with_interlinks(count: u32) -> HeaderChain {
        build_chain_with_interlinks_opts(count, true)
    }

    /// Like [`build_chain_with_interlinks`] but caller controls whether the
    /// genesis (h=1) extension bytes are inserted into the loader's backing
    /// store. Setting `include_genesis_in_loader = false` produces a chain
    /// whose loader returns `None` for h=1 — used to verify that
    /// `build_nipopow_proof` synthesizes the genesis `PoPowHeader` in-process
    /// rather than querying the loader.
    fn build_chain_with_interlinks_opts(count: u32, include_genesis_in_loader: bool) -> HeaderChain {
        let config = ChainConfig::testnet();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;

        // Build sequentially because each header id commits to its extension
        // root and the next block's interlinks commit to prior header ids.
        let mut headers: Vec<Header> = Vec::with_capacity(count as usize);
        let mut interlinks: Vec<BlockId> = Vec::new();
        let mut store: std::collections::HashMap<u32, Vec<u8>> =
            std::collections::HashMap::new();
        let mut prev_id = BlockId(Digest32::zero());
        for h in 1..=count {
            if let Some(previous_header) = headers.last() {
                interlinks =
                    NipopowAlgos::update_interlinks(previous_header.clone(), interlinks)
                        .expect("update_interlinks");
            }

            let mut fields = if h == 1 {
                Vec::new()
            } else {
                NipopowAlgos::pack_interlinks(interlinks.clone())
            };
            if h > 1 {
                // Exercise full-extension proofs rather than the degenerate
                // interlinks-only root used by the legacy verifier.
                fields.insert(0, ([0x00, 0x00], h.to_be_bytes().to_vec()));
            }
            // Compute expected difficulty based on currently-built chain
            // for nBits inheritance — but to avoid bringing in chain state
            // here, we just use the parent's n_bits within the first epoch.
            let header = make_synthetic_header_with_extension_root(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
                extension_root(&fields),
            );
            if h != 1 || include_genesis_in_loader {
                store.insert(h, pack_extension_bytes(&header.id, &fields));
            }
            prev_id = header.id;
            headers.push(header);
        }

        // Append headers to chain (no_pow path).
        for h in headers {
            chain.try_append_no_pow(h).expect("append");
        }

        // Wire loader.
        let store_arc = Arc::new(Mutex::new(store));
        chain.set_extension_loader(move |height| {
            store_arc.lock().unwrap().get(&height).cloned()
        });

        chain
    }

    fn verification_context(
        chain: &HeaderChain,
        expected_m: u32,
        expected_k: u32,
    ) -> NipopowVerificationContext {
        NipopowVerificationContext {
            expected_m,
            expected_k,
            expected_genesis_id: chain.header_at(1).expect("genesis header").id,
        }
    }

    #[test]
    fn build_proof_too_short_chain_errors() {
        let chain = build_chain_with_interlinks(3);
        let r = build_nipopow_proof(&chain, 2, 2, None);
        assert!(r.is_err(), "chain of 3 < m+k=4 must error");
    }

    #[test]
    fn build_proof_invalid_m_k_errors() {
        let chain = build_chain_with_interlinks(20);
        assert!(build_nipopow_proof(&chain, 0, 2, None).is_err());
        assert!(build_nipopow_proof(&chain, 2, 0, None).is_err());
        assert!(build_nipopow_proof(&chain, 257, 2, None).is_err());
        assert!(build_nipopow_proof(&chain, 2, 257, None).is_err());
    }

    #[test]
    fn build_proof_no_loader_errors() {
        let mut chain = HeaderChain::new(ChainConfig::testnet());
        // Build small chain without loader
        let n_bits = chain.config().initial_n_bits;
        let mut prev = BlockId(Digest32::zero());
        for h in 1..=10 {
            let hdr = make_synthetic_header(
                h,
                prev,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
            );
            prev = hdr.id;
            chain.try_append_no_pow(hdr).unwrap();
        }
        let r = build_nipopow_proof(&chain, 2, 2, None);
        assert!(r.is_err());
    }

    #[test]
    fn build_proof_returns_non_empty_bytes() {
        let chain = build_chain_with_interlinks(20);
        let bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn build_then_verify_roundtrip_no_pow() {
        let chain = build_chain_with_interlinks(20);
        let bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        let context = verification_context(&chain, 2, 2);
        let result = verify_nipopow_proof_bytes_no_pow(&bytes, &context).expect("verify");
        assert!(result.total_headers() > 0);
        assert_eq!(result.suffix_tip_height(), 20);
        // The lossless segments carry the same chain reflected in
        // total_headers + suffix_tip_height; the install path consumes them
        // directly without re-parsing the bytes.
        let headers: Vec<&Header> = result.headers().collect();
        assert_eq!(headers.len(), result.total_headers());
        assert_eq!(headers.last().unwrap().height, result.suffix_tip_height());
        // Heights are strictly increasing across the extracted chain.
        for pair in headers.windows(2) {
            assert!(pair[0].height < pair[1].height);
        }
    }

    #[test]
    fn verification_result_preserves_parsed_suffix_boundary() {
        let chain = build_chain_with_interlinks(20);
        let bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        let parsed = NipopowProof::scorex_parse_bytes(&bytes).expect("parse control");
        assert!(
            !parsed.prefix.is_empty(),
            "fixture must exercise the prefix"
        );

        let expected_prefix: Vec<BlockId> = parsed.prefix.iter().map(|p| p.header.id).collect();
        let expected_suffix_head = parsed.suffix_head.header.id;
        let expected_suffix_tail: Vec<BlockId> = parsed.suffix_tail.iter().map(|h| h.id).collect();
        let context = verification_context(&chain, 2, 2);

        let result = verify_nipopow_proof_bytes_no_pow(&bytes, &context).expect("verify");

        assert_eq!(result.m, 2);
        assert_eq!(result.k, 2);
        assert_eq!(result.continuous, None);
        assert_eq!(
            result.prefix.iter().map(|h| h.id).collect::<Vec<_>>(),
            expected_prefix
        );
        assert_eq!(result.suffix_head.id, expected_suffix_head);
        assert_eq!(
            result.suffix_tail.iter().map(|h| h.id).collect::<Vec<_>>(),
            expected_suffix_tail
        );
        assert_eq!(
            result.total_headers(),
            result.prefix.len() + 1 + result.suffix_tail.len()
        );
        assert_eq!(result.suffix_tip_height(), 20);
    }

    #[test]
    fn verify_rejects_unrequested_m() {
        let chain = build_chain_with_interlinks(20);
        let bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        let context = verification_context(&chain, 3, 2);

        let err = verify_nipopow_proof_bytes_no_pow(&bytes, &context)
            .expect_err("proof m must match the request");
        assert!(matches!(err, ChainError::Nipopow(ref msg) if msg.contains("expected m")));
    }

    #[test]
    fn verify_rejects_unrequested_k() {
        let chain = build_chain_with_interlinks(20);
        let bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        let context = verification_context(&chain, 2, 3);

        let err = verify_nipopow_proof_bytes_no_pow(&bytes, &context)
            .expect_err("proof k must match the request");
        assert!(matches!(err, ChainError::Nipopow(ref msg) if msg.contains("expected k")));
    }

    #[test]
    fn verify_rejects_suffix_length_mismatch() {
        let chain = build_chain_with_interlinks(20);
        let bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        let mut proof = NipopowProof::scorex_parse_bytes(&bytes).expect("parse control");
        proof.k = 3;
        let malformed = proof
            .scorex_serialize_bytes()
            .expect("serialize malformed proof");
        let context = verification_context(&chain, 2, 3);

        assert!(verify_nipopow_proof_bytes_no_pow(&malformed, &context).is_err());
    }

    #[test]
    fn verify_rejects_wrong_genesis() {
        let chain = build_chain_with_interlinks(20);
        let bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        let mut context = verification_context(&chain, 2, 2);
        context.expected_genesis_id = BlockId(Digest32::from([0xA6; 32]));

        let err = verify_nipopow_proof_bytes_no_pow(&bytes, &context)
            .expect_err("proof must bind configured genesis");
        assert!(matches!(err, ChainError::Nipopow(ref msg) if msg.contains("genesis")));
    }

    #[test]
    fn verification_context_rejects_absent_configured_genesis() {
        let chain = HeaderChain::new(ChainConfig::testnet());

        assert!(NipopowVerificationContext::from_chain(&chain, 6, 10).is_err());
    }

    #[test]
    fn verify_parses_optional_jvm_terminal_mode() {
        let chain = build_chain_with_interlinks(20);
        let core = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        let context = verification_context(&chain, 2, 2);

        for (terminal, expected) in [(None, None), (Some(0), Some(false)), (Some(1), Some(true))] {
            let mut bytes = core.clone();
            if let Some(mode) = terminal {
                bytes.push(mode);
            }
            let result = verify_nipopow_proof_bytes_no_pow(&bytes, &context).expect("verify");
            assert_eq!(result.continuous, expected);
        }
    }

    #[test]
    fn verify_rejects_invalid_jvm_terminal_mode() {
        let chain = build_chain_with_interlinks(20);
        let mut bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        bytes.push(2);
        let context = verification_context(&chain, 2, 2);

        assert!(verify_nipopow_proof_bytes_no_pow(&bytes, &context).is_err());
    }

    #[test]
    fn verify_rejects_extra_bytes_after_jvm_terminal_mode() {
        let chain = build_chain_with_interlinks(20);
        let mut bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        bytes.extend_from_slice(&[0, 0]);
        let context = verification_context(&chain, 2, 2);

        assert!(verify_nipopow_proof_bytes_no_pow(&bytes, &context).is_err());
    }

    #[test]
    fn verify_rejects_invalid_interlinks_proof() {
        let source_chain = build_chain_with_interlinks(20);
        let bytes = build_nipopow_proof(&source_chain, 6, 10, None).expect("build");
        let context = verification_context(&source_chain, 6, 10);
        let control = verify_nipopow_proof_bytes_no_pow(&bytes, &context)
            .expect("control proof must pass verification");
        assert_eq!(control.suffix_tip_height(), 20);

        for mutate_prefix in [false, true] {
            let mut proof = NipopowProof::scorex_parse_bytes(&bytes).expect("parse built proof");
            assert_eq!(proof.m, 6);
            assert_eq!(proof.k, 10);
            assert!(
                proof.has_valid_connections(),
                "control connections must be valid"
            );

            let location = if mutate_prefix {
                "prefix"
            } else {
                "suffix head"
            };
            {
                let target = if mutate_prefix {
                    proof
                        .prefix
                        .last_mut()
                        .expect("fixture must contain a prefix")
                } else {
                    &mut proof.suffix_head
                };
                assert!(
                    target.check_interlinks_proof(),
                    "control {location} interlinks proof must be valid"
                );

                // Change only the disclosed interlinks while retaining the
                // original Merkle proof. Existing connection evidence remains
                // present, but the interlinks-proof predicate must now fail.
                let added_link = BlockId(Digest32::from([0xA5; 32]));
                assert!(!target.interlinks.contains(&added_link));
                target.interlinks.push(added_link);
                assert!(
                    !target.check_interlinks_proof(),
                    "isolated {location} mutation must fail the proof predicate"
                );
            }

            assert!(
                proof.has_valid_connections(),
                "isolated {location} mutation must not break connections"
            );
            let observed_bytes = proof
                .scorex_serialize_bytes()
                .expect("serialize observation");
            let err = verify_nipopow_proof_bytes_no_pow(&observed_bytes, &context)
                .expect_err("verifier must reject an invalid interlinks proof");
            assert!(
                matches!(err, ChainError::Nipopow(ref msg) if msg.contains("interlink proofs")),
                "unexpected {location} rejection: {err:?}"
            );
        }
    }

    #[test]
    fn verify_empty_bytes_errors() {
        let context = NipopowVerificationContext {
            expected_m: 6,
            expected_k: 10,
            expected_genesis_id: BlockId(Digest32::zero()),
        };
        let r = verify_nipopow_proof_bytes(&[], &context);
        assert!(r.is_err());
    }

    #[test]
    fn verify_garbage_bytes_errors() {
        let context = NipopowVerificationContext {
            expected_m: 6,
            expected_k: 10,
            expected_genesis_id: BlockId(Digest32::zero()),
        };
        let r = verify_nipopow_proof_bytes(&[0xFFu8; 32], &context);
        assert!(r.is_err());
    }

    #[test]
    fn build_proof_skips_loader_for_genesis() {
        // Loader has bytes for h>=2 only — pre-fix this fails with
        // "extension at height 1 missing from loader" because the impl
        // unconditionally queries the loader for every height. Post-fix the
        // genesis PoPowHeader is synthesized in-process and the loader is
        // never asked for h=1.
        let chain = build_chain_with_interlinks_opts(20, false);
        let bytes = build_nipopow_proof(&chain, 2, 2, None)
            .expect("build must succeed without genesis loader entry");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn build_then_verify_synth_genesis_roundtrip() {
        // Same chain shape: loader returns None for h=1. The proof must
        // build, serialize, and round-trip back through the verifier.
        let chain = build_chain_with_interlinks_opts(20, false);
        let bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");
        let context = verification_context(&chain, 2, 2);
        let result = verify_nipopow_proof_bytes_no_pow(&bytes, &context).expect("verify");
        assert!(result.total_headers() >= 4); // m + k = 4
        assert_eq!(result.suffix_tip_height(), 20);
    }

    #[test]
    fn verify_mutated_proof_fails() {
        let chain = build_chain_with_interlinks(20);
        let mut bytes = build_nipopow_proof(&chain, 2, 2, None).expect("build");

        // Mutate a byte well past the m, k header (around offset 50 to land
        // in the proof body, not in the m/k prefix).
        if bytes.len() > 50 {
            bytes[50] ^= 0xFFu8;
        }
        let context = verification_context(&chain, 2, 2);
        let r = verify_nipopow_proof_bytes_no_pow(&bytes, &context);
        assert!(r.is_err(), "mutated proof must fail");
    }

    #[test]
    fn reader_rejects_extension_with_mismatched_header_id() {
        // Focused regression test for the silent-corruption bug:
        //
        // `ChainPopowReader::popow_header_at_height` used to discard the
        // header_id embedded in the extension bytes, accepting any payload
        // the loader returned. If the integrator's loader returned bytes for
        // a different block at the queried height — exactly what
        // enr-store's backward-walk recovery does at BEST_CHAIN holes — the
        // reader would silently produce a `PoPowHeader { header: chain[h],
        // interlinks: <from-other-block> }`. The walker in
        // `prove_with_reader` would then follow the wrong block's
        // interlinks into a wrong lineage, producing a prefix whose
        // adjacent entries don't link via interlink and failing
        // `NipopowProof::has_valid_connections()` at verify time.
        //
        // Post-fix: the reader compares the extension's embedded header_id
        // against the queried `header.id` and returns `None` on mismatch.
        // This propagates up as `MissingPopowHeader` →
        // `ChainError::Nipopow` at the chain crate boundary.
        use std::sync::{Arc, Mutex};

        let config = ChainConfig::testnet();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;

        // Build a small chain.
        let mut prev = BlockId(Digest32::zero());
        for h in 1..=5u32 {
            let hdr = make_synthetic_header(
                h,
                prev,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
            );
            prev = hdr.id;
            chain.try_append_no_pow(hdr).expect("append");
        }

        // Wire a loader that returns extension bytes whose embedded
        // header_id is NOT the one for the queried height. The bytes are
        // well-formed (parse_extension_bytes accepts them) — they just
        // describe a different block.
        let bogus_id = BlockId(Digest32::zero());
        // chain.header_at(3).id is computed via Blake2b over the header
        // serialization and is overwhelmingly unlikely to be all zeros.
        assert_ne!(
            chain.header_at(3).unwrap().id,
            bogus_id,
            "test relies on chain[3].id != bogus_id"
        );
        let bogus_fields = NipopowAlgos::pack_interlinks(vec![bogus_id]);
        let bogus_bytes = pack_extension_bytes(&bogus_id, &bogus_fields);

        let mut store: std::collections::HashMap<u32, Vec<u8>> =
            std::collections::HashMap::new();
        store.insert(3u32, bogus_bytes);
        let store_arc = Arc::new(Mutex::new(store));
        chain.set_extension_loader(move |height| {
            store_arc.lock().unwrap().get(&height).cloned()
        });

        let reader = ChainPopowReader { chain: &chain };
        let result = reader.popow_header_at_height(3);
        assert!(
            result.is_none(),
            "popow_header_at_height(3) must return None when extension bytes carry a mismatched header_id"
        );
    }

    #[test]
    fn build_proof_no_silent_corruption_with_loader_gap() {
        // End-to-end regression test for the silent-corruption postcondition:
        //
        // `build_nipopow_proof` MUST NEVER return `Ok(bytes)` where
        // context-bound verification of `bytes` fails. Either the build
        // returns an `Err` (clean fail) or it returns bytes that pass
        // verify (correct construction). There is no third state.
        //
        // The repro: a synthetic chain whose extension loader returns
        // bytes for a *different* block at some heights — the same shape
        // as enr-store's backward-walk recovery for `modifiers.redb` holes.
        // Pre-fix this could produce a structurally invalid proof that
        // round-trips through the serializer but fails
        // `has_valid_connections` at verify time.
        //
        // Post-fix the chain reader rejects mismatched extension bytes,
        // surfacing as a clean error from `prove_with_reader`. The build
        // either errors or completes against a chain whose corrupted
        // heights are simply skipped by the walk.
        use std::sync::{Arc, Mutex};

        let config = ChainConfig::testnet();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;

        let count: u32 = 64;

        // Build chain headers with strictly-increasing timestamps.
        let mut headers: Vec<Header> = Vec::with_capacity(count as usize);
        let mut prev_id = BlockId(Digest32::zero());
        for h in 1..=count {
            let header = make_synthetic_header(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
            );
            prev_id = header.id;
            headers.push(header);
        }

        // Compute correct interlinks per height.
        let mut interlinks: Vec<Vec<BlockId>> = Vec::with_capacity(headers.len());
        for (idx, h) in headers.iter().enumerate() {
            if idx == 0 {
                interlinks.push(vec![h.id]);
            } else {
                let prev_header = &headers[idx - 1];
                let prev_interlinks = interlinks[idx - 1].clone();
                interlinks.push(
                    NipopowAlgos::update_interlinks(prev_header.clone(), prev_interlinks)
                        .expect("update_interlinks"),
                );
            }
        }

        // Build a corrupted store: for heights 16..=48 return the
        // extension bytes for `height - 4` instead of `height`. The bytes
        // are well-formed and parse cleanly through `parse_extension_bytes`
        // — they just don't match the header at the queried height. This
        // models enr-store returning a stale entry for a missing modifier.
        let mut store: std::collections::HashMap<u32, Vec<u8>> =
            std::collections::HashMap::new();
        for (idx, h) in headers.iter().enumerate() {
            let bytes = if h.height >= 16 && h.height <= 48 && idx >= 4 {
                let src_idx = idx - 4;
                let fields = NipopowAlgos::pack_interlinks(interlinks[src_idx].clone());
                pack_extension_bytes(&headers[src_idx].id, &fields)
            } else {
                let fields = NipopowAlgos::pack_interlinks(interlinks[idx].clone());
                pack_extension_bytes(&h.id, &fields)
            };
            store.insert(h.height, bytes);
        }

        for h in headers.into_iter() {
            chain.try_append_no_pow(h).expect("append");
        }

        let store_arc = Arc::new(Mutex::new(store));
        chain.set_extension_loader(move |height| {
            store_arc.lock().unwrap().get(&height).cloned()
        });

        // Build with both anchor variants: `None` (uses last_headers) and
        // an explicit deep anchor (uses popow_header_by_id directly). The
        // bug affects either path through the reader.
        for header_id_opt in [None, Some(chain.tip().id)] {
            let result = build_nipopow_proof(&chain, 2, 2, header_id_opt);
            match result {
                Err(_) => {
                    // Clean fail — acceptable.
                }
                Ok(bytes) => {
                    let context = verification_context(&chain, 2, 2);
                    verify_nipopow_proof_bytes_no_pow(&bytes, &context).unwrap_or_else(|e| panic!(
                        "build_nipopow_proof returned Ok with bytes that fail verify (silent corruption): {e:?}"
                    ));
                }
            }
        }
    }

    #[test]
    fn popow_header_by_id_returns_some_for_known_header() {
        let chain = build_chain_with_interlinks(20);
        // Pick a non-genesis header so the loader path is exercised.
        let known = chain.header_at(10).expect("header at h=10 exists");
        let known_id = known.id;

        let bytes = popow_header_by_id(&chain, &known_id)
            .expect("call succeeds")
            .expect("known header must return Some(bytes)");
        assert!(!bytes.is_empty());

        // Round-trip through the inverse Scorex deserializer.
        let parsed = PoPowHeader::scorex_parse_bytes(&bytes).expect("parses cleanly");
        assert_eq!(parsed.header.id, known_id);
    }

    #[test]
    fn popow_header_by_id_returns_some_for_genesis() {
        // h=1 hits the in-process synthesis path — no loader involvement.
        let chain = build_chain_with_interlinks(20);
        let genesis_id = chain.header_at(1).expect("genesis exists").id;

        let bytes = popow_header_by_id(&chain, &genesis_id)
            .expect("call succeeds")
            .expect("genesis must return Some(bytes)");
        let parsed = PoPowHeader::scorex_parse_bytes(&bytes).expect("parses cleanly");
        assert_eq!(parsed.header.id, genesis_id);
        assert!(parsed.interlinks.is_empty());
        assert!(parsed.interlinks_proof.get_indices().is_empty());
        assert!(parsed.interlinks_proof.get_proofs().is_empty());
        assert!(parsed.check_interlinks_proof());
    }

    #[test]
    fn popow_header_by_id_returns_none_for_unknown_id() {
        let chain = build_chain_with_interlinks(20);
        let unknown = BlockId(Digest32::zero());
        // Blake2b of a non-degenerate header is overwhelmingly unlikely
        // to collide with all-zeros, but assert for explicitness.
        assert_ne!(chain.tip().id, unknown);

        let result = popow_header_by_id(&chain, &unknown).expect("call succeeds");
        assert!(result.is_none(), "unknown id must return Ok(None)");
    }
}

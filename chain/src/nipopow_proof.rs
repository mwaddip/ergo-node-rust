//! NiPoPoW proof construction and verification (Phase 6).
//!
//! Wraps `ergo-nipopow` for build/verify on the local header chain.
//! Light-client sync mode (applying a proof to chain state) is out of
//! scope — proofs are verified for correctness but not used to skip
//! block download.
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

/// Result of a successful NiPoPoW proof verification.
///
/// Carries the metadata fields used by serve-side log paths AND the full
/// extracted header chain so the light-client install path can pass it to
/// [`HeaderChain::install_from_nipopow_proof`] without re-parsing the bytes.
///
/// Renamed from `NipopowProofMeta` (which only carried metadata) to reflect
/// the new return shape. Existing serve-side consumers reference fields by
/// name only, so the rename is type-name-only there.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NipopowVerificationResult {
    /// Height of the suffix tip (highest header in the proof).
    pub suffix_tip_height: u32,
    /// Total number of headers in the proof (prefix + suffix).
    pub total_headers: usize,
    /// Whether the proof is in continuous mode (carries difficulty headers).
    ///
    /// Always `false` for first release; we don't currently propagate the
    /// continuous-mode flag from `NipopowProof`. Difficulty-recalculation
    /// header presence is not separately validated either.
    pub continuous: bool,
    /// Headers extracted from the verified proof, in strictly-increasing
    /// height order: `prefix.iter().map(|p| p.header)
    ///     .chain(once(suffix_head.header))
    ///     .chain(suffix_tail)`.
    ///
    /// The light-client install path passes `headers.last()` as `suffix_head`
    /// and the `k - 1` headers preceding it as `suffix_tail`. Callers that
    /// only want metadata (the existing serve-side log path) can ignore the
    /// field at zero parsing cost — it's already materialized.
    pub headers: Vec<Header>,
}

/// `PopowHeaderReader` adapter over the local `HeaderChain`.
///
/// Backs [`build_nipopow_proof`] via
/// [`ergo_nipopow::NipopowAlgos::prove_with_reader`]. The reader walks the
/// interlink hierarchy on demand and only fetches the popow headers the
/// proof actually visits — `O(m + k + m · log₂ N)` per call instead of `O(N)`.
///
/// **Genesis special case**: per `facts/chain.md` Phase 6 invariants, the
/// genesis block's `interlinks = [genesis_id]` is canonical and the extension
/// loader MUST NOT be called for `height == 1` (real testnet/mainnet genesis
/// extensions are empty and would yield wrong interlinks). The
/// `popow_header_at_height(1)` and `popow_header_by_id(genesis_id)` paths
/// synthesize the popow header in-process; every other height path goes
/// through the loader as normal.
struct ChainPopowReader<'a> {
    chain: &'a HeaderChain,
}

impl<'a> ChainPopowReader<'a> {
    /// Synthesize a `PoPowHeader` for `header` with interlinks given by
    /// `interlinks`. Builds a synthetic `ExtensionCandidate` carrying the
    /// canonical packed-interlinks fields and feeds it through
    /// `NipopowAlgos::proof_for_interlink_vector` so the resulting merkle
    /// proof is byte-identical with the JVM equivalent.
    fn build_popow_header(header: Header, interlinks: Vec<BlockId>) -> Option<PoPowHeader> {
        let extension_candidate =
            ExtensionCandidate::new(NipopowAlgos::pack_interlinks(interlinks.clone())).ok()?;
        let interlinks_proof = NipopowAlgos::proof_for_interlink_vector(&extension_candidate)?;
        Some(PoPowHeader {
            header,
            interlinks,
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
        // real genesis extensions are empty and would produce empty
        // interlinks, which is wrong by convention.
        if height == 1 {
            let genesis_id = header.id;
            return Self::build_popow_header(header, vec![genesis_id]);
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

/// Compare two NiPoPoW proofs from raw bytes (post-envelope, inner proof
/// payload only). Returns `true` if `a` represents a better chain than `b`
/// per KMZ17 §4.3.
///
/// Both byte slices must be valid NiPoPoW proof payloads (as produced by
/// `NipopowProof::scorex_serialize_bytes`). Parse failure on either side
/// returns an error.
pub fn compare_nipopow_proof_bytes(a: &[u8], b: &[u8]) -> Result<bool, ChainError> {
    let proof_a = NipopowProof::scorex_parse_bytes(a)
        .map_err(|e| ChainError::Nipopow(format!("parse proof A failed: {e:?}")))?;
    let proof_b = NipopowProof::scorex_parse_bytes(b)
        .map_err(|e| ChainError::Nipopow(format!("parse proof B failed: {e:?}")))?;
    proof_a
        .is_better_than(&proof_b)
        .map_err(|e| ChainError::Nipopow(format!("comparison failed: {e:?}")))
}

/// Verify a NiPoPoW proof from raw bytes.
///
/// **Precondition**: `bytes` is the inner NiPoPoW proof payload (the main
/// crate has stripped any P2P message envelope).
///
/// **Validation checks** (mirrors `NipopowProof.isValid`):
/// 1. The proof parses cleanly via the Scorex serializer.
/// 2. Heights strictly increasing across the headers chain.
/// 3. Each header's PoW passes [`crate::verify_pow`].
/// 4. Parent connections in the chain are consistent (via
///    `NipopowProof::has_valid_connections`).
///
/// Does NOT touch chain state. Does NOT apply the proof to local chain.
/// The returned [`NipopowVerificationResult::headers`] field carries the
/// extracted header chain in height order so the caller can install it via
/// [`crate::HeaderChain::install_from_nipopow_proof`] without re-parsing.
pub fn verify_nipopow_proof_bytes(bytes: &[u8]) -> Result<NipopowVerificationResult, ChainError> {
    verify_inner(bytes, true)
}

/// Test-only: verify a NiPoPoW proof without running the per-header PoW
/// check. Used by unit tests on synthetic chains where headers don't have
/// real Autolykos solutions.
#[cfg(test)]
pub(crate) fn verify_nipopow_proof_bytes_no_pow(
    bytes: &[u8],
) -> Result<NipopowVerificationResult, ChainError> {
    verify_inner(bytes, false)
}

fn verify_inner(bytes: &[u8], check_pow: bool) -> Result<NipopowVerificationResult, ChainError> {
    if bytes.is_empty() {
        return Err(ChainError::Nipopow("empty proof bytes".into()));
    }
    let proof = NipopowProof::scorex_parse_bytes(bytes).map_err(|e| {
        ChainError::Nipopow(format!("parse failed: {e:?}"))
    })?;

    if !proof.has_valid_connections() {
        return Err(ChainError::Nipopow("invalid connections".into()));
    }

    // Walk all headers (prefix + suffix_head + suffix_tail) in order, check
    // strictly-increasing heights + (optionally) PoW, and retain ownership
    // so the caller can install them without re-parsing the bytes.
    let all_headers: Vec<Header> = proof
        .prefix
        .iter()
        .map(|p| &p.header)
        .chain(std::iter::once(&proof.suffix_head.header))
        .chain(proof.suffix_tail.iter())
        .cloned()
        .collect();

    if all_headers.is_empty() {
        return Err(ChainError::Nipopow("empty proof headers chain".into()));
    }

    let mut last_height: Option<u32> = None;
    for h in &all_headers {
        if let Some(prev) = last_height {
            if h.height <= prev {
                return Err(ChainError::Nipopow(format!(
                    "non-increasing heights: {} after {}",
                    h.height, prev
                )));
            }
        }
        last_height = Some(h.height);
        if check_pow {
            crate::verify_pow(h)?;
        }
    }

    Ok(NipopowVerificationResult {
        suffix_tip_height: last_height.unwrap_or(0),
        total_headers: all_headers.len(),
        continuous: false,
        headers: all_headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voting::pack_extension_bytes;
    use crate::{ChainConfig, HeaderChain};
    use ergo_chain_types::{ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, Votes};
    use sigma_ser::ScorexSerializable;
    use std::sync::{Arc, Mutex};

    fn make_synthetic_header(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
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
            extension_root: zero32,
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

        // Build headers list first so we can compute interlinks for each
        // and store the extension bytes per height.
        let mut headers: Vec<Header> = Vec::with_capacity(count as usize);
        let mut prev_id = BlockId(Digest32::zero());
        let g = make_synthetic_header(1, prev_id, 1_000_000, n_bits);
        prev_id = g.id;
        headers.push(g);
        for h in 2..=count {
            // Compute expected difficulty based on currently-built chain
            // for nBits inheritance — but to avoid bringing in chain state
            // here, we just use the parent's n_bits within the first epoch.
            let header = make_synthetic_header(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
            );
            prev_id = header.id;
            headers.push(header);
        }

        // Build per-height interlinks and extension bytes.
        let mut interlinks: Vec<Vec<BlockId>> = Vec::with_capacity(headers.len());
        for (idx, h) in headers.iter().enumerate() {
            if idx == 0 {
                // Genesis: interlinks = [genesis_id]
                interlinks.push(vec![h.id]);
            } else {
                let prev_header = &headers[idx - 1];
                let prev_interlinks = interlinks[idx - 1].clone();
                let new_interlinks =
                    NipopowAlgos::update_interlinks(prev_header.clone(), prev_interlinks)
                        .expect("update_interlinks");
                interlinks.push(new_interlinks);
            }
        }

        // Pack each into extension bytes keyed by height.
        let mut store: std::collections::HashMap<u32, Vec<u8>> =
            std::collections::HashMap::new();
        for (idx, h) in headers.iter().enumerate() {
            if h.height == 1 && !include_genesis_in_loader {
                continue;
            }
            let interlinks_for_h = &interlinks[idx];
            let fields = NipopowAlgos::pack_interlinks(interlinks_for_h.clone());
            let bytes = pack_extension_bytes(&h.id, &fields);
            store.insert(h.height, bytes);
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
        let result = verify_nipopow_proof_bytes_no_pow(&bytes).expect("verify");
        assert!(result.total_headers > 0);
        assert_eq!(result.suffix_tip_height, 20);
        // The headers field carries the same chain that's reflected in
        // total_headers + suffix_tip_height — the install path consumes it
        // directly without re-parsing the bytes.
        assert_eq!(result.headers.len(), result.total_headers);
        assert_eq!(
            result.headers.last().unwrap().height,
            result.suffix_tip_height
        );
        // Heights are strictly increasing across the extracted chain.
        for pair in result.headers.windows(2) {
            assert!(pair[0].height < pair[1].height);
        }
    }

    #[test]
    fn verify_empty_bytes_errors() {
        let r = verify_nipopow_proof_bytes(&[]);
        assert!(r.is_err());
    }

    #[test]
    fn verify_garbage_bytes_errors() {
        let r = verify_nipopow_proof_bytes(&[0xFFu8; 32]);
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
        let result = verify_nipopow_proof_bytes_no_pow(&bytes).expect("verify");
        assert!(result.total_headers >= 4); // m + k = 4
        assert_eq!(result.suffix_tip_height, 20);
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
        let r = verify_nipopow_proof_bytes_no_pow(&bytes);
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
        // `verify_nipopow_proof_bytes(bytes)` fails. Either the build
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
                    verify_nipopow_proof_bytes_no_pow(&bytes).unwrap_or_else(|e| panic!(
                        "build_nipopow_proof returned Ok with bytes that fail verify (silent corruption): {e:?}"
                    ));
                }
            }
        }
    }
}

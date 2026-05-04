use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use ergo_chain_types::autolykos_pow_scheme::decode_compact_bits;
use ergo_chain_types::{BlockId, Digest32, Header};
use ergo_lib::chain::parameters::{Parameter, Parameters};
use num_bigint::BigUint;

use crate::cache::LazyHeaderStore;
use crate::config::ChainConfig;
use crate::error::ChainError;
use crate::voting::{default_parameters, default_proposed_update_bytes, SOFT_FORK_VOTE};

/// Map a signed parameter ID (1-8) to its [`Parameter`] enum variant.
///
/// Returns `None` for soft-fork IDs (120-124) and unknown IDs.
fn ordinary_param(signed_id: i8) -> Option<Parameter> {
    match signed_id.unsigned_abs() as i8 {
        1 => Some(Parameter::StorageFeeFactor),
        2 => Some(Parameter::MinValuePerByte),
        3 => Some(Parameter::MaxBlockSize),
        4 => Some(Parameter::MaxBlockCost),
        5 => Some(Parameter::TokenAccessCost),
        6 => Some(Parameter::InputCost),
        7 => Some(Parameter::DataInputCost),
        8 => Some(Parameter::OutputCost),
        _ => None,
    }
}

/// Callback type for loading the raw extension bytes for a given height.
///
/// Wired by the integrator (main crate) to bridge `enr-store`. Returns
/// `None` if no extension is available at that height.
pub type ExtensionLoader =
    Arc<dyn Fn(u32) -> Option<Vec<u8>> + Send + Sync + 'static>;

/// Result of a successful `try_append` call.
#[derive(Debug)]
pub enum AppendResult {
    /// Header extends the best chain. Chain height increased.
    Extended,
    /// Header is valid but forks from the best chain at the given height.
    /// The header is NOT added to the chain — caller stores it separately.
    Forked { fork_height: u32 },
}

/// A validated chain of headers.
///
/// Every header in the chain has been checked for parent linkage, timestamp
/// bounds, PoW validity, and correct difficulty. Supports 1-deep and deep
/// reorganization when a competing fork proves longer.
pub struct HeaderChain {
    config: ChainConfig,
    /// Lowest header height currently in the chain. `None` on empty.
    ///
    /// For full chains starting at genesis this is `Some(1)`; for
    /// chains installed from a NiPoPoW proof this is the suffix-head
    /// height. Replaces the old `by_height[0].height` computation —
    /// the `Vec<Header>` itself was retired in Phase 3, with headers
    /// now served by [`LazyHeaderStore`] backed by the registered
    /// [`HeaderLoader`].
    base_height: Option<u32>,
    /// Map from header ID to height for O(1) containment / height
    /// lookup. Kept as an in-memory index because hitting storage on
    /// every `contains()` / `height_of()` check is not worth the save
    /// (~64 MB at mainnet scale — an order of magnitude below the
    /// retired header Vec, which was ~1.4 GB).
    by_id: HashMap<BlockId, u32>,
    /// Cumulative difficulty score at each height, base-relative:
    /// `scores[0]` is the score at [`Self::base_height`].
    ///
    /// Not yet migrated to the lazy loader — `enr-store` currently
    /// persists empty-placeholder scores for the best chain (see
    /// `facts/store.md`). Chain keeps this Vec (~105 MB at mainnet)
    /// as the source of truth until the store contract carries real
    /// scores. That's a ~10× smaller footprint than the retired
    /// header Vec, so leaving it is a pragmatic Phase 3 trade.
    scores: Vec<BigUint>,
    /// Currently active blockchain parameters (Phase 6: Soft-Fork Voting).
    /// Updated only at epoch-boundary block validation via
    /// [`Self::apply_epoch_boundary_parameters`].
    ///
    /// Soft-fork lifecycle state (IDs 121, 122) lives directly inside
    /// `parameters_table` via `Parameter::SoftForkVotesCollected` and
    /// `Parameter::SoftForkStartingHeight`. When voting is inactive, those
    /// keys are absent. This mirrors JVM `parametersTable` exactly.
    active_parameters: Parameters,
    /// Variable-length encoding of `ErgoValidationSettingsUpdate` (extension
    /// key `[0x00, 124]`, JVM `Parameters.proposedUpdate`) in effect at the
    /// current chain tip.
    ///
    /// Seeded at [`Self::new`] from
    /// [`crate::voting::default_proposed_update_bytes`] (the JVM
    /// `LaunchParameters.proposedUpdate` encoding for the configured
    /// network). Advanced at every validated epoch-boundary block via
    /// [`Self::apply_epoch_boundary_parameters`], which records that
    /// block's exact ID 124 bytes — so after the first boundary the
    /// field tracks on-chain state byte-for-byte.
    ///
    /// JVM stores this on `Parameters.proposedUpdate`, separate from
    /// `parametersTable`. Sigma-rust does not yet expose this on its
    /// `Parameters` type, so we track the raw bytes here on
    /// `HeaderChain`. The main-session validator reads them via
    /// [`Self::active_proposed_update_bytes`] and does the byte-for-byte
    /// comparison against incoming block extensions (JVM
    /// `Parameters.matchParameters60`), gated on `BlockVersion >=
    /// Interpreter60Version`.
    active_proposed_update_bytes: Vec<u8>,
    /// Optional callback for loading extension bytes by height. Required
    /// before calling [`Self::recompute_active_parameters_from_storage`]
    /// or [`crate::nipopow_proof::build_nipopow_proof`].
    extension_loader: Option<ExtensionLoader>,
    /// Set to `true` by [`Self::install_from_nipopow_proof`] and never
    /// reset. When `true`, [`Self::validate_child`] and (under `cfg(test)`)
    /// `validate_child_no_pow` skip the `expected_difficulty` recalculation
    /// and the `header.n_bits` comparison.
    ///
    /// Standard SPV behavior — light clients can't recompute
    /// `expected_difficulty` because the recalc reads
    /// `use_last_epochs * epoch_length` headers of pre-install history that
    /// they don't have. PoW verification, parent linkage, and timestamp
    /// bounds remain in force. See `facts/chain.md` Phase 6 invariants.
    light_client_mode: bool,
    /// Lazy header/score store. After Phase 3, this is the sole
    /// source of truth for header reads — the old `by_height` Vec is
    /// gone. Callers register a [`HeaderLoader`] at startup so the
    /// cache can fall through to persistent storage on eviction.
    /// Score lookups still use the in-memory [`Self::scores`] Vec as
    /// a safety net; the score slots on this store are coherent via
    /// write-through but not yet authoritative.
    lazy: LazyHeaderStore,
}

/// All-zeros parent ID expected for the genesis header.
fn genesis_parent_id() -> BlockId {
    BlockId(Digest32::zero())
}

/// Compute the difficulty contribution of a header as BigUint.
fn header_difficulty(header: &Header) -> BigUint {
    decode_compact_bits(header.n_bits)
        .to_biguint()
        .unwrap_or_default()
}

impl HeaderChain {
    /// Create a new empty chain with the given network configuration.
    pub fn new(config: ChainConfig) -> Self {
        let active_parameters = default_parameters(config.network);
        let active_proposed_update_bytes = default_proposed_update_bytes(config.network);
        Self {
            config,
            base_height: None,
            by_id: HashMap::new(),
            scores: Vec::new(),
            active_parameters,
            active_proposed_update_bytes,
            extension_loader: None,
            light_client_mode: false,
            lazy: LazyHeaderStore::with_default_capacity(),
        }
    }

    /// Validate and append a header to the chain.
    ///
    /// Returns `Extended` if the header extends the best chain, or
    /// `Forked` if its parent is in the chain but is not the tip.
    /// A `Forked` result does NOT add the header — the caller stores it.
    pub fn try_append(&mut self, header: Header) -> Result<AppendResult, ChainError> {
        if self.is_empty() {
            self.validate_genesis(&header)?;
            self.push_header(header);
            return Ok(AppendResult::Extended);
        }

        let tip_id = self.tip().id;
        if header.parent_id == tip_id {
            self.validate_child(&header)?;
            self.push_header(header);
            Ok(AppendResult::Extended)
        } else if let Some(&parent_height) = self.by_id.get(&header.parent_id) {
            // Parent exists but is not the tip — this is a fork.
            if header.height != parent_height + 1 {
                return Err(ChainError::NonSequentialHeight {
                    expected: parent_height + 1,
                    got: header.height,
                });
            }
            Ok(AppendResult::Forked { fork_height: parent_height })
        } else {
            Err(ChainError::ParentNotFound {
                parent_id: header.parent_id,
            })
        }
    }

    /// Height of the best validated chain tip, or 0 if empty.
    ///
    /// Post-Phase-3 this is derived from [`Self::base_height`] and
    /// the length of the score vector, which remain in lockstep on
    /// every push/pop/reorg path.
    pub fn height(&self) -> u32 {
        match self.base_height {
            Some(base) if !self.scores.is_empty() => base + (self.scores.len() as u32) - 1,
            _ => 0,
        }
    }

    /// The tip header of the best validated chain.
    ///
    /// Reads through [`LazyHeaderStore`]: cache first, loader on
    /// miss. Post-Phase-3 there is no in-memory fallback — if the
    /// loader is unwired (test fixture or misconfigured node) and
    /// the cache has evicted the tip, this panics, which is the
    /// honest signal that the chain is broken.
    ///
    /// # Panics
    ///
    /// Panics if the chain is empty, or if the tip cannot be
    /// resolved through the lazy store (shouldn't happen — the
    /// cache is write-through on push, so the tip is always
    /// freshly resident).
    pub fn tip(&self) -> Header {
        if self.is_empty() {
            panic!("tip() called on empty chain");
        }
        // Delegate to `height()` instead of recomputing `base +
        // scores.len() - 1` here — that arithmetic only holds under
        // the `base_height.is_some() iff !scores.is_empty()`
        // invariant; routing through `height()` makes the empty-path
        // guard explicit at every caller.
        self.lazy
            .get_header(self.height())
            .expect("tip header unavailable — cache evicted with no loader wired")
    }

    /// Header at the given height, if it exists in the chain.
    ///
    /// Routes through [`LazyHeaderStore`]: cache first, loader on
    /// miss. Returns `None` for heights below [`Self::base_height`],
    /// above [`Self::height`], or when the lazy store can't resolve
    /// the height (cache evicted + no loader wired).
    pub fn header_at(&self, height: u32) -> Option<Header> {
        let base = self.base_height?;
        if height < base || height > self.height() {
            return None;
        }
        self.lazy.get_header(height)
    }

    /// Whether this header ID is part of the validated chain.
    pub fn contains(&self, header_id: &BlockId) -> bool {
        self.by_id.contains_key(header_id)
    }

    /// Height of a header in the chain by its ID, or `None` if not present.
    pub fn height_of(&self, header_id: &BlockId) -> Option<u32> {
        self.by_id.get(header_id).copied()
    }

    /// Up to `count` sequential headers starting at `height`.
    ///
    /// Per-height reads delegate to [`Self::header_at`] so
    /// cache/loader resolution is uniform across the public API.
    pub fn headers_from(&self, height: u32, count: usize) -> Vec<Header> {
        if count == 0 || self.is_empty() {
            return Vec::new();
        }
        let tip_height = self.height();
        if height > tip_height {
            return Vec::new();
        }
        let base = match self.base_height {
            Some(b) => b,
            None => return Vec::new(),
        };
        if height < base {
            return Vec::new();
        }
        let span = (tip_height - height) as usize + 1;
        let take = count.min(span);
        (0..take)
            .filter_map(|i| self.header_at(height + i as u32))
            .collect()
    }

    /// The chain configuration.
    pub fn config(&self) -> &ChainConfig {
        &self.config
    }

    /// Number of headers in the chain.
    pub fn len(&self) -> usize {
        self.scores.len()
    }

    /// Whether the chain is empty.
    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }

    /// Cumulative difficulty score at the chain tip.
    ///
    /// Phase 2 routing: delegates to [`Self::score_at`] for uniform
    /// cache/loader/Vec resolution.
    pub fn cumulative_score(&self) -> BigUint {
        if self.is_empty() {
            return BigUint::ZERO;
        }
        self.score_at(self.height()).unwrap_or_default()
    }

    /// Cumulative difficulty score at a given height.
    ///
    /// Cache first; Vec safety net on miss. The score Vec remains
    /// authoritative until `enr-store` carries real best-chain
    /// scores — see the `scores` field comment.
    pub fn score_at(&self, height: u32) -> Option<BigUint> {
        if let Some(cached) = self.lazy.get_score(height) {
            debug_assert_eq!(
                self.score_ref_from_vec(height),
                Some(&cached),
                "score cache/Vec disagreement at height {}",
                height,
            );
            return Some(cached);
        }
        self.score_ref_from_vec(height).cloned()
    }

    /// Vec-only score lookup by height. The score Vec is still the
    /// in-memory source of truth post-Phase-3.
    fn score_ref_from_vec(&self, height: u32) -> Option<&BigUint> {
        let base = self.base_height?;
        let idx = height.checked_sub(base)? as usize;
        self.scores.get(idx)
    }

    /// `true` iff `height` is the start of a new voting epoch.
    ///
    /// Mirrors JVM `(height % votingEpochLength == 0) && height > 0`.
    /// Pure computation; safe to call on an empty chain.
    pub fn is_epoch_boundary(&self, height: u32) -> bool {
        height > 0 && height.is_multiple_of(self.config.voting.voting_length)
    }

    /// Register a callback for loading raw extension bytes by height.
    ///
    /// Required before [`Self::recompute_active_parameters_from_storage`]
    /// or [`crate::nipopow_proof::build_nipopow_proof`] can do useful work.
    /// Tests that don't need voting/nipopow can skip this entirely.
    ///
    /// Wired by the integrator (main crate) to bridge `enr-store`. The
    /// loader returns `None` if no extension is available at that height.
    pub fn set_extension_loader<F>(&mut self, loader: F)
    where
        F: Fn(u32) -> Option<Vec<u8>> + Send + Sync + 'static,
    {
        self.extension_loader = Some(Arc::new(loader));
    }

    /// Whether an extension loader has been registered.
    pub fn has_extension_loader(&self) -> bool {
        self.extension_loader.is_some()
    }

    /// Internal accessor for nipopow_proof and other modules within the crate.
    pub(crate) fn extension_loader(&self) -> Option<&ExtensionLoader> {
        self.extension_loader.as_ref()
    }

    /// Register a callback for loading a header by height from
    /// persistent storage.
    ///
    /// Wired by the integrator (main crate) to bridge `enr-store`.
    /// Phase 1: the loader is installed on the lazy store but not yet
    /// consulted by any public read — `header_at`, `tip`, and
    /// `headers_from` still read from the in-memory `Vec<Header>`.
    /// Phases 2–3 migrate those reads onto the cache + loader.
    pub fn set_header_loader<F>(&mut self, loader: F)
    where
        F: Fn(u32) -> Option<Header> + Send + Sync + 'static,
    {
        self.lazy.set_header_loader(Arc::new(loader));
    }

    /// Register a callback for loading a cumulative-difficulty score
    /// by height from persistent storage.
    ///
    /// Split from [`Self::set_header_loader`] so consumers that only
    /// need the header (NiPoPoW build, difficulty walk) don't pay
    /// `BigUint` deserialization on every lookup.
    pub fn set_score_loader<F>(&mut self, loader: F)
    where
        F: Fn(u32) -> Option<BigUint> + Send + Sync + 'static,
    {
        self.lazy.set_score_loader(Arc::new(loader));
    }

    /// Whether a header loader has been registered.
    pub fn has_header_loader(&self) -> bool {
        self.lazy.has_header_loader()
    }

    /// Whether a score loader has been registered.
    pub fn has_score_loader(&self) -> bool {
        self.lazy.has_score_loader()
    }

    /// Resize the lazy header/score caches in place. Default is
    /// [`crate::cache::DEFAULT_CACHE_CAPACITY`] (16 384 entries each);
    /// raise for deeper difficulty walks or reorg coverage if
    /// measurements warrant it.
    pub fn set_cache_capacity(&self, capacity: NonZeroUsize) {
        self.lazy.resize(capacity);
    }

    /// Crate-internal accessor for the lazy header/score store.
    /// Tests use it to inspect cache behavior directly. Also useful
    /// for future observer APIs (stats, capacity tuning).
    #[allow(dead_code)] // used only by tests in the current tree
    pub(crate) fn lazy(&self) -> &LazyHeaderStore {
        &self.lazy
    }

    /// Load the parameters in effect at `target_height` from storage.
    ///
    /// Picks the most recent epoch-boundary block at or before
    /// `target_height`, reads its extension via the registered loader,
    /// parses the parameters, and installs them as
    /// [`Self::active_parameters`].
    ///
    /// `target_height` is the height the validator is about to resume
    /// validating from — typically far behind the chain tip on a fresh
    /// resync, identical to the chain tip on a normal restart. Using the
    /// validator's resume height (rather than the chain tip) is what makes
    /// a fresh genesis resync against a chain whose tip carries v6-era
    /// parameters validate the v1-era epoch boundaries correctly: at
    /// `target_height = 0`, no boundary exists at or before, so
    /// `active_parameters` stays at the chain-internal defaults and the
    /// first epoch-boundary block (mainnet 1024) is computed against the
    /// v1 table the chain genuinely expects.
    ///
    /// Behavior by `target_height`:
    /// - `target_height < voting_length` → no-op success;
    ///   `active_parameters` is left at the construction defaults. The
    ///   loader is NOT consulted (no boundary block exists at or before).
    /// - `target_height ∈ [k*voting_length, (k+1)*voting_length - 1]` for
    ///   `k ≥ 1` → loads the extension at height `k*voting_length`.
    ///
    /// Errors if (and only if a load is required):
    /// - The loader is unset
    /// - The loader returns `None` for the boundary height
    /// - The extension bytes fail to parse
    /// - The boundary header is missing from the chain (caller misuse:
    ///   `target_height` points past the chain's known headers)
    /// - The extension's `header_id` field disagrees with the chain
    ///
    /// Cost: bounded — at most one extension read. Acceptable at startup.
    pub fn recompute_active_parameters_from_storage(
        &mut self,
        target_height: u32,
    ) -> Result<(), ChainError> {
        let voting_length = self.config.voting.voting_length;
        if target_height < voting_length {
            // No epoch boundary exists at or before `target_height`.
            // Leave `active_parameters` at construction defaults.
            return Ok(());
        }
        let boundary_height = (target_height / voting_length) * voting_length;
        if boundary_height == 0 {
            // Defensive: voting_length must be > 0 by config invariant, so
            // this is unreachable, but we keep the guard cheap.
            return Ok(());
        }

        let loader = self.extension_loader.as_ref().ok_or_else(|| {
            ChainError::Voting(
                "extension loader not set; cannot recompute parameters".into(),
            )
        })?;

        let extension_bytes = loader(boundary_height).ok_or_else(|| {
            ChainError::Voting(format!(
                "extension loader returned None for boundary height {boundary_height}"
            ))
        })?;

        let (header_id, fields) = crate::voting::parse_extension_bytes(&extension_bytes)?;
        let expected = self.header_at(boundary_height).ok_or_else(|| {
            ChainError::Voting(format!(
                "no header at boundary height {boundary_height}"
            ))
        })?;
        if header_id != expected.id {
            return Err(ChainError::Voting(format!(
                "extension header_id mismatch at height {boundary_height}: \
                 extension says {}, chain says {}",
                header_id, expected.id
            )));
        }
        let parsed = crate::voting::parse_parameters_from_kv(&fields)?;

        // Build a Parameters table from the parsed kv. Start from network
        // defaults and override with any parsed entries. Note that
        // `default_parameters(Testnet)` already includes
        // `SubblocksPerBlock = 30`; the explicit branch below will overwrite
        // it with whatever the on-chain extension carried.
        let mut new_params = default_parameters(self.config.network);
        for (signed_id, value) in parsed {
            if let Some(p) = ordinary_param(signed_id) {
                new_params.parameters_table.insert(p, value);
            } else if signed_id == crate::voting::ID_SUBBLOCKS_PER_BLOCK {
                new_params
                    .parameters_table
                    .insert(Parameter::SubblocksPerBlock, value);
            } else if signed_id == crate::voting::ID_BLOCK_VERSION {
                new_params
                    .parameters_table
                    .insert(Parameter::BlockVersion, value);
            } else if signed_id == crate::voting::ID_SOFT_FORK_VOTES_COLLECTED {
                new_params
                    .parameters_table
                    .insert(Parameter::SoftForkVotesCollected, value);
            } else if signed_id == crate::voting::ID_SOFT_FORK_STARTING_HEIGHT {
                new_params
                    .parameters_table
                    .insert(Parameter::SoftForkStartingHeight, value);
            }
        }

        // Extract proposedUpdate (ID 124) raw bytes from the boundary
        // extension. JVM stores this on `Parameters.proposedUpdate`, not
        // `parametersTable`; we track it separately on `HeaderChain`.
        // Fallback to the launch default if the extension is missing the
        // field — JVM's equivalent path reads
        // `ErgoValidationSettingsUpdate.empty` as an absent-field default,
        // but on mainnet the on-chain value has been non-empty since
        // before any observed boundary, so an absent ID 124 here would
        // indicate either a corrupt extension or a test fixture. Either
        // way, holding the launch default keeps the field well-formed
        // and the subsequent boundary's `apply_epoch_boundary_parameters`
        // call overwrites it.
        let extracted = crate::voting::extract_disabling_rules_from_kv(&fields);
        let proposed_update = if extracted.is_empty() {
            default_proposed_update_bytes(self.config.network)
        } else {
            extracted
        };

        self.active_parameters = new_params;
        self.active_proposed_update_bytes = proposed_update;
        Ok(())
    }

    /// Raw `ErgoValidationSettingsUpdate` bytes (extension key
    /// `[0x00, 124]`, JVM `Parameters.proposedUpdate`) in effect at the
    /// current chain tip.
    ///
    /// On a fresh chain returns the launch default from
    /// [`crate::voting::default_proposed_update_bytes`] for the
    /// configured network. After every accepted epoch-boundary block
    /// returns that block's exact ID 124 bytes (or the launch default
    /// as a fallback when a boundary extension has no ID 124 field —
    /// see [`Self::recompute_active_parameters_from_storage`]).
    ///
    /// Used by the main-session validator for the byte-for-byte
    /// `proposedUpdate` comparison in JVM `Parameters.matchParameters60`.
    /// Sigma-rust does not yet expose `proposedUpdate` on its
    /// `Parameters` type, so we track the raw bytes here.
    pub fn active_proposed_update_bytes(&self) -> &[u8] {
        &self.active_proposed_update_bytes
    }

    /// The blockchain parameters in effect at the current chain tip.
    ///
    /// Returns the parameters set by the most recent epoch-boundary block.
    /// On a fresh chain (or a chain shorter than one voting epoch), returns
    /// the chain-internal startup defaults from
    /// [`crate::voting::default_parameters`].
    ///
    /// Used by the validator to bound transaction costs and by mining when
    /// assembling new candidate blocks.
    pub fn active_parameters(&self) -> &Parameters {
        &self.active_parameters
    }

    /// Set the active parameters and proposed-update bytes after a
    /// successful epoch-boundary block.
    ///
    /// **Preconditions**:
    /// - `params` was returned by [`Self::compute_expected_parameters`]
    ///   for the just-validated epoch-boundary block AND was confirmed
    ///   to match the params parsed from that block's extension.
    /// - `proposed_update_bytes` is the block's exact ID 124 extension
    ///   value (or `Vec::new()` if the block's extension had no ID 124
    ///   field — JVM's `ErgoValidationSettingsUpdate.empty` convention).
    ///   On `BlockVersion >= Interpreter60Version` the main-session
    ///   validator will already have verified this byte-for-byte against
    ///   [`Self::active_proposed_update_bytes`] before calling.
    ///
    /// Called by the block-application pipeline AFTER the full block
    /// has been validated and persisted. Validators must NOT call this
    /// themselves — they are stateless w.r.t. chain state mutation.
    ///
    /// `params` carries the full table including soft-fork state
    /// (`Parameter::SoftForkVotesCollected`, `Parameter::SoftForkStartingHeight`) when voting
    /// is active. Both are absent when voting is inactive.
    ///
    /// Both fields are updated atomically: a caller that has verified
    /// the boundary block must not be able to advance one without the
    /// other. Callers that only need to advance `active_parameters`
    /// (e.g. test fixtures with no ID 124 data) can pass the current
    /// [`Self::active_proposed_update_bytes`] back in unchanged.
    pub fn apply_epoch_boundary_parameters(
        &mut self,
        params: Parameters,
        proposed_update_bytes: Vec<u8>,
    ) {
        self.active_parameters = params;
        self.active_proposed_update_bytes = proposed_update_bytes;
    }

    /// Tally the just-ended voting epoch's votes for an epoch boundary.
    ///
    /// The just-ended epoch nominally spans
    /// `[epoch_boundary_height - voting_length, epoch_boundary_height - 1]`,
    /// but since the chain's first valid block is height 1 (no block at
    /// height 0), the very first epoch is one block shorter. Walks
    /// `[max(1, h - voting_length), h - 1]` and tallies votes.
    fn tally_just_ended_epoch(
        &self,
        epoch_boundary_height: u32,
    ) -> Result<HashMap<i8, u32>, ChainError> {
        let voting_length = self.config.voting.voting_length;
        let nominal_start = epoch_boundary_height.saturating_sub(voting_length);
        let start = nominal_start.max(1);
        let end = epoch_boundary_height
            .checked_sub(1)
            .ok_or_else(|| ChainError::Voting("epoch boundary cannot be 0".into()))?;

        if start > end {
            return Ok(HashMap::new());
        }

        let mut votes: Vec<[u8; 3]> = Vec::with_capacity((end - start + 1) as usize);
        for h in start..=end {
            let header = self.header_at(h).ok_or_else(|| {
                ChainError::Voting(format!(
                    "header at height {h} missing from chain (epoch boundary {epoch_boundary_height})"
                ))
            })?;
            votes.push(header.votes.0);
        }
        Ok(crate::voting::tally_votes(&votes))
    }

    /// Compute the parameters that the block at `epoch_boundary_height`
    /// MUST emit in its extension.
    ///
    /// Mirrors JVM `Parameters.update`. The validator calls this BEFORE
    /// appending the boundary block; the chain's tip should be at
    /// `epoch_boundary_height - 1`.
    ///
    /// The returned `Parameters` table includes the full set: ordinary
    /// IDs 1-8, BlockVersion (123), and soft-fork lifecycle state
    /// (`Parameter::SoftForkVotesCollected` / `Parameter::SoftForkStartingHeight`) when active.
    ///
    /// `block_proposed_update` is the raw payload of the block's extension
    /// key `[0x00, 124]` (`SoftForkDisablingRules` /
    /// `ErgoValidationSettingsUpdate`), or the empty slice if absent. It is
    /// consulted ONLY to gate the `SubblocksPerBlock` auto-insert at the
    /// voting-driven v4 activation — see Step 4 below and
    /// `facts/chain.md` Phase 6.
    ///
    /// **Determinism**: For any two correct implementations given the same
    /// chain history, the output is byte-identical. This is the consensus
    /// rule. Mismatch with the actual block extension = reject the block.
    pub fn compute_expected_parameters(
        &self,
        epoch_boundary_height: u32,
        block_proposed_update: &[u8],
    ) -> Result<Parameters, ChainError> {
        let voting = &self.config.voting;
        if voting.voting_length == 0 {
            return Err(ChainError::Voting("voting_length must be > 0".into()));
        }

        let tally = self.tally_just_ended_epoch(epoch_boundary_height)?;

        let mut new_params = self.active_parameters.clone();

        // Snapshot the pre-update BlockVersion so we can detect a voting-
        // driven activation (Step 2) in Step 4. Taken before any step runs —
        // Step 1 can't touch BlockVersion but this keeps the source of the
        // snapshot obviously independent of the subsequent mutations.
        let pre_update_block_version = new_params
            .parameters_table
            .get(&Parameter::BlockVersion)
            .copied()
            .unwrap_or(0);

        // Step 1: ordinary parameter changes (IDs ±1..±8).
        for (&signed_id, &count) in &tally {
            let abs = signed_id.unsigned_abs() as i8;
            if !(1..=8).contains(&abs) {
                continue;
            }
            if voting.change_approved(count) {
                crate::voting::apply_ordinary_step(
                    &mut new_params.parameters_table,
                    signed_id,
                );
            }
        }

        // Step 2: soft-fork lifecycle (operates directly on parameters_table).
        let fork_votes = tally.get(&SOFT_FORK_VOTE).copied().unwrap_or(0);
        Self::apply_soft_fork_lifecycle(
            voting,
            epoch_boundary_height,
            fork_votes,
            &mut new_params,
        );

        // Step 3: forced v2 activation (mainnet hard-fork that pre-dates voting).
        if voting.version2_activation_height != 0
            && epoch_boundary_height == voting.version2_activation_height
        {
            let bv = new_params
                .parameters_table
                .get(&Parameter::BlockVersion)
                .copied()
                .unwrap_or(0);
            if bv == 1 {
                new_params
                    .parameters_table
                    .insert(Parameter::BlockVersion, 2);
            }
        }

        // Step 4: auto-insert SubblocksPerBlock at BlockVersion == 4,
        // gated on rule 409 in the activated update (JVM parity).
        //
        // Mirrors JVM `Parameters.scala::update` (lines 87-96): when the
        // protocol reaches v4 (the 6.0 soft-fork that introduces sub-blocks)
        // and `SubblocksPerBlock` is not yet present in the table, insert it
        // with the default value (30) — UNLESS the update being activated
        // in this call disables rule 409 (`exMatchParameters`). JVM's
        // mainnet hard-codes `proposedUpdate = [215, 409]` at launch
        // (`LaunchParameters.scala`), so when that proposal is activated at
        // the v6 BlockVersion bump (h=1,628,160 on mainnet), rule 409 is
        // present in `activatedUpdate` and the insert is skipped; it fires
        // at the NEXT boundary instead.
        //
        // `activatedUpdate` in JVM is `proposedUpdate` at activation height,
        // `empty` otherwise. We reproduce this by checking whether the
        // voting lifecycle bumped BlockVersion in this call. The forced-v2
        // activation (1 → 2 at `version2_activation_height`) is explicitly
        // excluded — JVM does NOT wire `activatedUpdate` for that path.
        let post_update_block_version = new_params
            .parameters_table
            .get(&Parameter::BlockVersion)
            .copied()
            .unwrap_or(0);
        let is_voting_activation = post_update_block_version != pre_update_block_version
            && !(voting.version2_activation_height != 0
                && epoch_boundary_height == voting.version2_activation_height
                && post_update_block_version == 2);
        let rule_409_in_activated = if is_voting_activation {
            crate::voting::parse_disabled_rules(block_proposed_update)?.contains(&409u16)
        } else {
            false
        };

        if post_update_block_version == 4
            && !new_params
                .parameters_table
                .contains_key(&Parameter::SubblocksPerBlock)
            && !rule_409_in_activated
        {
            new_params.parameters_table.insert(
                Parameter::SubblocksPerBlock,
                crate::voting::SUBBLOCKS_PER_BLOCK_DEFAULT,
            );
        }

        Ok(new_params)
    }

    /// Apply the six-state soft-fork lifecycle transition.
    ///
    /// Mirrors JVM `Parameters.updateFork`. Branches are mutually exclusive
    /// by height; at most one fires per call. After cleanup, a new voting
    /// can start in the same call (sequential, not exclusive).
    ///
    /// Operates directly on `params.parameters_table` via the new
    /// `Parameter::SoftForkVotesCollected` / `Parameter::SoftForkStartingHeight`
    /// variants — no separate state struct.
    fn apply_soft_fork_lifecycle(
        voting: &crate::voting::VotingConfig,
        height: u32,
        fork_votes: u32,
        params: &mut Parameters,
    ) {
        let voting_length = voting.voting_length;
        let soft_fork_epochs = voting.soft_fork_epochs;
        let activation_epochs = voting.activation_epochs;

        let starting_height = params
            .parameters_table
            .get(&Parameter::SoftForkStartingHeight)
            .copied();
        let votes_collected = params
            .parameters_table
            .get(&Parameter::SoftForkVotesCollected)
            .copied();

        if let (Some(starting_height), Some(votes_collected)) =
            (starting_height, votes_collected)
        {
            let starting_height = starting_height as u32;
            let votes_collected = votes_collected.max(0) as u32;
            let approved = voting.soft_fork_approved(votes_collected);
            let mid_end = starting_height + voting_length * soft_fork_epochs;
            let activation = starting_height + voting_length * (soft_fork_epochs + activation_epochs);
            let cleanup_fail = starting_height + voting_length * (soft_fork_epochs + 1);
            let cleanup_success = starting_height + voting_length * (soft_fork_epochs + activation_epochs + 1);

            if approved && height == cleanup_success {
                // Successful voting cleanup
                params.parameters_table.remove(&Parameter::SoftForkStartingHeight);
                params.parameters_table.remove(&Parameter::SoftForkVotesCollected);
            } else if !approved && height == cleanup_fail {
                // Unsuccessful voting cleanup
                params.parameters_table.remove(&Parameter::SoftForkStartingHeight);
                params.parameters_table.remove(&Parameter::SoftForkVotesCollected);
            } else if approved && height == activation {
                // Activation: bump BlockVersion
                let bv = params
                    .parameters_table
                    .get(&Parameter::BlockVersion)
                    .copied()
                    .unwrap_or(0);
                params
                    .parameters_table
                    .insert(Parameter::BlockVersion, bv + 1);
            } else if height <= mid_end {
                // Mid-voting: add this epoch's votes
                let new_total = votes_collected.saturating_add(fork_votes) as i32;
                params
                    .parameters_table
                    .insert(Parameter::SoftForkVotesCollected, new_total);
            }
            // else: activation period (between mid_end and activation), no action.
        }

        // After cleanup OR if no voting was active, start new voting if fork
        // votes are present this epoch.
        let still_inactive = !params
            .parameters_table
            .contains_key(&Parameter::SoftForkStartingHeight);
        if still_inactive && fork_votes > 0 {
            params
                .parameters_table
                .insert(Parameter::SoftForkStartingHeight, height as i32);
            // Per contract item 3: ID 121 = 0 on start. The current epoch's
            // fork votes are not double-counted; they were the trigger but
            // the running counter starts at zero.
            params
                .parameters_table
                .insert(Parameter::SoftForkVotesCollected, 0);
        }
    }

    /// Tally header vote slots across one voting epoch.
    ///
    /// Walks headers in `[epoch_end_height - voting_length + 1, epoch_end_height]`
    /// inclusive and sums the three signed-byte vote slots in each
    /// header's `votes` field. Used by [`Self::compute_expected_parameters`]
    /// and exposed for testability.
    ///
    /// Errors if any header in the requested range is missing from the
    /// chain — callers must ensure the precondition holds before calling.
    pub fn count_votes_in_epoch(
        &self,
        epoch_end_height: u32,
    ) -> Result<std::collections::HashMap<i8, u32>, ChainError> {
        let voting_length = self.config.voting.voting_length;
        if voting_length == 0 {
            return Err(ChainError::Voting(
                "voting_length must be > 0".into(),
            ));
        }

        let start = epoch_end_height
            .checked_sub(voting_length - 1)
            .ok_or_else(|| {
                ChainError::Voting(format!(
                    "epoch_end_height {epoch_end_height} < voting_length {voting_length}"
                ))
            })?;

        let mut votes: Vec<[u8; 3]> = Vec::with_capacity(voting_length as usize);
        for h in start..=epoch_end_height {
            let header = self.header_at(h).ok_or_else(|| {
                ChainError::Voting(format!(
                    "header at height {h} missing from chain (epoch end {epoch_end_height})"
                ))
            })?;
            votes.push(header.votes.0);
        }

        Ok(crate::voting::tally_votes(&votes))
    }

    // --- Light-client install ---

    /// Install a verified NiPoPoW proof's suffix as the chain's starting
    /// point for light-client mode.
    ///
    /// **Precondition**: chain is empty ([`Self::is_empty`] returns `true`).
    /// The headers in `suffix_head` + `suffix_tail` MUST already have been
    /// validated by the caller via
    /// [`crate::nipopow_proof::verify_nipopow_proof_bytes`]. This function
    /// does NOT re-verify the proof; it assumes the caller has done so and
    /// is installing the trusted suffix.
    ///
    /// **Postcondition on Ok**: chain contains `suffix_head` followed by
    /// every header in `suffix_tail`, in order. `tip()` returns the last
    /// header in `suffix_tail` (or `suffix_head` if `suffix_tail` is empty).
    /// Subsequent `try_append` calls extend the tip from there using normal
    /// parent-linkage rules. [`Self::light_client_mode`] is set to `true`
    /// and persists for the chain's lifetime.
    ///
    /// **Postcondition on Err**: chain is unchanged. Possible errors:
    /// - [`ChainError::ChainNotEmpty`] if the chain already contains headers.
    /// - [`ChainError::ParentNotFound`] if any header in `suffix_tail` does
    ///   not link to its predecessor's id.
    /// - PoW failure on any header.
    ///
    /// The genesis-parent check is bypassed — light clients install at
    /// arbitrary heights. `scores[0]` is initialized to 0 because cumulative
    /// difficulty across the install boundary is meaningless; only deltas
    /// matter post-install. The reorg floor (see [`Self::reorg_floor`])
    /// prevents reorgs that would unwind past the install point.
    ///
    /// `active_parameters` is left at the chain-internal defaults — light
    /// clients have no source for voted parameters because they don't
    /// download block extensions. See `facts/chain.md` Phase 6
    /// "Light-client parameter limitation".
    pub fn install_from_nipopow_proof(
        &mut self,
        suffix_head: Header,
        suffix_tail: Vec<Header>,
    ) -> Result<(), ChainError> {
        self.install_from_nipopow_proof_impl(suffix_head, suffix_tail, true)
    }

    fn install_from_nipopow_proof_impl(
        &mut self,
        suffix_head: Header,
        suffix_tail: Vec<Header>,
        verify_pow: bool,
    ) -> Result<(), ChainError> {
        if !self.is_empty() {
            return Err(ChainError::ChainNotEmpty);
        }

        // Verify PoW on suffix_head before mutating any state. The validator
        // contract says the caller already verified the proof, but PoW is
        // cheap to recheck and gives us a clean rollback path: if the head
        // is bad we never touched the chain.
        if verify_pow {
            crate::verify_pow(&suffix_head)?;
        }

        // Push suffix_head bypassing validate_genesis: it is rarely actually
        // genesis (height 1) and its parent_id is whatever the upstream chain
        // happens to be. scores[0] starts at 0 — see doc above.
        let head_id = suffix_head.id;
        let head_height = suffix_head.height;
        self.base_height = Some(head_height);
        self.by_id.insert(head_id, head_height);
        self.lazy.put(head_height, suffix_head, BigUint::ZERO);
        self.scores.push(BigUint::ZERO);
        self.light_client_mode = true;

        // Walk suffix_tail. On any failure, roll back EVERYTHING (including
        // suffix_head and the light_client_mode flag) so the chain is
        // unchanged on Err per the postcondition.
        for header in suffix_tail {
            let tip_id = self.tip().id;
            if header.parent_id != tip_id {
                self.rollback_install();
                return Err(ChainError::ParentNotFound {
                    parent_id: header.parent_id,
                });
            }
            if header.height != self.tip().height + 1 {
                let expected = self.tip().height + 1;
                self.rollback_install();
                return Err(ChainError::NonSequentialHeight {
                    expected,
                    got: header.height,
                });
            }
            if verify_pow {
                if let Err(e) = crate::verify_pow(&header) {
                    self.rollback_install();
                    return Err(e);
                }
            }
            // Use push_header so suffix_tail headers' scores are
            // parent_score + diff. Since scores[0] = 0, this just accumulates
            // diffs from the install boundary onwards — only deltas matter
            // for post-install reorg comparisons.
            self.push_header(header);
        }

        Ok(())
    }

    /// Test variant of [`Self::install_from_nipopow_proof`] that skips the
    /// per-header PoW check. Used by unit tests on synthetic chains where
    /// headers don't carry real Autolykos solutions.
    #[cfg(test)]
    pub(crate) fn install_from_nipopow_proof_no_pow(
        &mut self,
        suffix_head: Header,
        suffix_tail: Vec<Header>,
    ) -> Result<(), ChainError> {
        self.install_from_nipopow_proof_impl(suffix_head, suffix_tail, false)
    }

    /// Roll back a partial light-client install. Used internally only.
    fn rollback_install(&mut self) {
        self.base_height = None;
        self.by_id.clear();
        self.scores.clear();
        self.light_client_mode = false;
        self.lazy.clear();
    }

    /// The minimum fork-point height that the reorg machinery is allowed to
    /// accept.
    ///
    /// - Full chains starting at genesis: returns `1` (the existing
    ///   "can't reorg past genesis" guard, expressed structurally).
    /// - Light chains installed via [`Self::install_from_nipopow_proof`]:
    ///   returns the suffix-head height. Reorgs that would require unwinding
    ///   past the install boundary are rejected — we don't have the headers
    ///   to roll back to.
    ///
    /// Empty chains return `1` for back-compat (the value is meaningless
    /// because reorg machinery refuses empty chains anyway).
    pub fn reorg_floor(&self) -> u32 {
        self.base_height.unwrap_or(1)
    }

    /// Whether this chain has been installed from a NiPoPoW proof and is
    /// running in light-client mode (no block bodies, no transaction
    /// validation, no expected-difficulty recalculation).
    pub fn light_client_mode(&self) -> bool {
        self.light_client_mode
    }

    // --- Reorg ---

    /// Perform a 1-deep chain reorganization.
    ///
    /// Replaces the current tip with `alternative_tip` (a competing block at the
    /// same height sharing the same parent), then appends `continuation` on top.
    ///
    /// Returns the ID of the replaced tip on success.
    pub fn try_reorg(
        &mut self,
        alternative_tip: Header,
        continuation: Header,
    ) -> Result<BlockId, ChainError> {
        self.try_reorg_impl(alternative_tip, continuation, true)
    }

    /// Rewind the chain to `fork_point_height` and apply `new_branch`.
    ///
    /// Returns the IDs of demoted headers on success. On any validation
    /// failure the chain is unchanged.
    pub fn try_reorg_deep(
        &mut self,
        fork_point_height: u32,
        new_branch: Vec<Header>,
    ) -> Result<Vec<BlockId>, ChainError> {
        self.try_reorg_deep_impl(fork_point_height, new_branch, true)
    }

    // --- Internal helpers ---

    /// Push a validated header onto the chain with its cumulative
    /// score. Sets [`Self::base_height`] on the first push.
    fn push_header(&mut self, header: Header) {
        if self.base_height.is_none() {
            self.base_height = Some(header.height);
        }
        let parent_score = self.scores.last().cloned().unwrap_or_default();
        let diff = header_difficulty(&header);
        let score = parent_score + diff;
        let height = header.height;
        let id = header.id;
        self.lazy.put(height, header, score.clone());
        self.scores.push(score);
        self.by_id.insert(id, height);
    }

    /// Pop the tip header and its score. Returns both for rollback.
    /// Clears [`Self::base_height`] when the chain becomes empty.
    fn pop_header(&mut self) -> Option<(Header, BigUint)> {
        if self.scores.is_empty() {
            return None;
        }
        let tip_height = self.height();
        let header = self.lazy.get_header(tip_height).expect(
            "tip header unavailable during pop — cache evicted with no loader wired",
        );
        let score = self.scores.pop().unwrap_or_default();
        self.by_id.remove(&header.id);
        self.lazy.evict(tip_height);
        if self.scores.is_empty() {
            self.base_height = None;
        }
        Some((header, score))
    }

    /// Restore a previously popped header with its score.
    /// Reinstates [`Self::base_height`] when pushing into an empty
    /// chain.
    fn restore_header(&mut self, header: Header, score: BigUint) {
        if self.base_height.is_none() {
            self.base_height = Some(header.height);
        }
        let height = header.height;
        let id = header.id;
        self.by_id.insert(id, height);
        self.lazy.put(height, header, score.clone());
        self.scores.push(score);
    }

    fn try_reorg_impl(
        &mut self,
        alternative_tip: Header,
        continuation: Header,
        verify_pow: bool,
    ) -> Result<BlockId, ChainError> {
        // Need at least 2 headers — can't reorg genesis.
        if self.scores.len() < 2 {
            return Err(ChainError::Reorg(
                "chain too short for reorg (need at least 2 headers)".into(),
            ));
        }

        let tip_height = self.height();
        let tip_parent_id = self.tip().parent_id;

        // Alternative must compete at the same height with the same parent.
        if alternative_tip.height != tip_height {
            return Err(ChainError::Reorg(format!(
                "alternative height {} != tip height {tip_height}",
                alternative_tip.height,
            )));
        }
        if alternative_tip.parent_id != tip_parent_id {
            return Err(ChainError::Reorg(
                "alternative parent doesn't match tip's parent".into(),
            ));
        }

        // Continuation must build on the alternative.
        if continuation.parent_id != alternative_tip.id {
            return Err(ChainError::Reorg(
                "continuation doesn't build on alternative".into(),
            ));
        }
        if continuation.height != alternative_tip.height + 1 {
            return Err(ChainError::NonSequentialHeight {
                expected: alternative_tip.height + 1,
                got: continuation.height,
            });
        }

        // Pop old tip so validation runs against the correct chain state.
        let (old_tip, old_score) = self.pop_header().unwrap();

        // Validate alternative against the parent (now the chain tip).
        if let Err(e) = self.validate_reorg_header(&alternative_tip, verify_pow) {
            self.restore_header(old_tip, old_score);
            return Err(e);
        }

        // Push alternative so continuation validation sees the correct chain.
        self.push_header(alternative_tip);

        // Validate continuation against the alternative (now the chain tip).
        if let Err(e) = self.validate_reorg_header(&continuation, verify_pow) {
            // Roll back: pop alternative, restore old tip.
            let (_, _) = self.pop_header().unwrap();
            self.restore_header(old_tip, old_score);
            return Err(e);
        }

        // Push continuation.
        self.push_header(continuation);

        Ok(old_tip.id)
    }

    fn try_reorg_deep_impl(
        &mut self,
        fork_point_height: u32,
        new_branch: Vec<Header>,
        verify_pow: bool,
    ) -> Result<Vec<BlockId>, ChainError> {
        if new_branch.is_empty() {
            return Err(ChainError::Reorg("new branch is empty".into()));
        }

        // Fork point must be in the chain.
        if self.is_empty() {
            return Err(ChainError::Reorg("chain is empty".into()));
        }

        // Reorg floor: cannot fork below the chain's lowest stored header.
        // Full-mode chains starting at genesis: floor = 1, no-op for any
        // fork point ≥ 1. Light-mode chains installed at height N: floor =
        // N, load-bearing — we don't have the headers to unwind below it.
        let floor = self.reorg_floor();
        if fork_point_height < floor {
            return Err(ChainError::Reorg(format!(
                "fork point height {fork_point_height} below reorg floor {floor}"
            )));
        }

        let base = self.base_height.expect("non-empty chain has a base_height");
        let fork_idx = fork_point_height
            .checked_sub(base)
            .map(|i| i as usize)
            .filter(|&i| i < self.scores.len())
            .ok_or_else(|| {
                ChainError::Reorg(format!("fork point height {fork_point_height} not in chain"))
            })?;

        // First header's parent must match the fork point.
        let fork_point_id = self
            .lazy
            .get_header(fork_point_height)
            .ok_or_else(|| {
                ChainError::Reorg(format!(
                    "fork-point header at {fork_point_height} unavailable (cache evicted, no loader wired)"
                ))
            })?
            .id;
        if new_branch[0].parent_id != fork_point_id {
            return Err(ChainError::Reorg(
                "first header in branch doesn't connect to fork point".into(),
            ));
        }

        // Collect headers above the fork point so we can restore on
        // failure. Pull from the lazy store (cache || loader) — if
        // any is missing we can't safely reorg and we bail before
        // mutating state.
        let tip_height = self.height();
        let mut saved_headers: Vec<Header> = Vec::with_capacity(
            (tip_height - fork_point_height) as usize,
        );
        for h in (fork_point_height + 1)..=tip_height {
            let hdr = self.lazy.get_header(h).ok_or_else(|| {
                ChainError::Reorg(format!(
                    "header at height {h} unavailable for reorg drain"
                ))
            })?;
            saved_headers.push(hdr);
        }
        let saved_scores: Vec<BigUint> = self.scores.drain(fork_idx + 1..).collect();
        for h in &saved_headers {
            self.by_id.remove(&h.id);
            self.lazy.evict(h.height);
        }

        // Validate and append each header in the new branch.
        let mut failed = false;
        let mut fail_err = None;

        for (i, header) in new_branch.into_iter().enumerate() {
            // Check parent linkage: first header links to fork point (already checked),
            // subsequent headers must link to the previous (current tip).
            if i > 0 {
                let tip_id = self.tip().id;
                if header.parent_id != tip_id {
                    fail_err = Some(ChainError::ParentNotFound {
                        parent_id: header.parent_id,
                    });
                    failed = true;
                    break;
                }
            }

            // Check height is sequential.
            let expected_height = fork_point_height + 1 + i as u32;
            if header.height != expected_height {
                fail_err = Some(ChainError::NonSequentialHeight {
                    expected: expected_height,
                    got: header.height,
                });
                failed = true;
                break;
            }

            // Validate timestamp, difficulty, PoW.
            if let Err(e) = self.validate_reorg_header(&header, verify_pow) {
                fail_err = Some(e);
                failed = true;
                break;
            }

            self.push_header(header);
        }

        if failed {
            // Rollback: pop any new headers we added.
            while self.scores.len() > fork_idx + 1 {
                self.pop_header();
            }
            // Restore saved state.
            for (header, score) in saved_headers.into_iter().zip(saved_scores) {
                let height = header.height;
                let id = header.id;
                self.by_id.insert(id, height);
                self.lazy.put(height, header, score.clone());
                self.scores.push(score);
            }
            return Err(fail_err.unwrap());
        }

        let demoted_ids: Vec<BlockId> = saved_headers.iter().map(|h| h.id).collect();
        Ok(demoted_ids)
    }

    /// Validate a header against the current tip during a reorg operation.
    fn validate_reorg_header(&self, header: &Header, verify_pow: bool) -> Result<(), ChainError> {
        let tip = self.tip();

        if header.timestamp <= tip.timestamp {
            return Err(ChainError::TimestampNotIncreasing {
                parent_ts: tip.timestamp,
                got: header.timestamp,
            });
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if header.timestamp > now_ms + self.config.max_time_drift_ms {
            return Err(ChainError::TimestampTooFarInFuture {
                timestamp: header.timestamp,
                max_allowed: now_ms + self.config.max_time_drift_ms,
            });
        }

        let expected_n_bits = crate::difficulty::expected_difficulty(&tip, self)?;
        if header.n_bits != expected_n_bits {
            return Err(ChainError::WrongDifficulty {
                height: header.height,
                expected: expected_n_bits,
                got: header.n_bits,
            });
        }

        if verify_pow {
            crate::verify_pow(header)?;
        }

        Ok(())
    }

    // --- Test support ---

    /// Append a header skipping PoW verification.
    /// For tests that need to validate chain logic without real mining solutions.
    #[cfg(test)]
    pub(crate) fn try_append_no_pow(
        &mut self,
        header: Header,
    ) -> Result<AppendResult, ChainError> {
        if self.is_empty() {
            self.validate_genesis_no_pow(&header)?;
            self.push_header(header);
            return Ok(AppendResult::Extended);
        }

        let tip_id = self.tip().id;
        if header.parent_id == tip_id {
            self.validate_child_no_pow(&header)?;
            self.push_header(header);
            Ok(AppendResult::Extended)
        } else if let Some(&parent_height) = self.by_id.get(&header.parent_id) {
            if header.height != parent_height + 1 {
                return Err(ChainError::NonSequentialHeight {
                    expected: parent_height + 1,
                    got: header.height,
                });
            }
            Ok(AppendResult::Forked { fork_height: parent_height })
        } else {
            Err(ChainError::ParentNotFound {
                parent_id: header.parent_id,
            })
        }
    }

    /// Test variant of `try_reorg` that skips PoW verification.
    #[cfg(test)]
    pub(crate) fn try_reorg_no_pow(
        &mut self,
        alternative_tip: Header,
        continuation: Header,
    ) -> Result<BlockId, ChainError> {
        self.try_reorg_impl(alternative_tip, continuation, false)
    }

    /// Test variant of `try_reorg_deep` that skips PoW verification.
    #[cfg(test)]
    pub(crate) fn try_reorg_deep_no_pow(
        &mut self,
        fork_point_height: u32,
        new_branch: Vec<Header>,
    ) -> Result<Vec<BlockId>, ChainError> {
        self.try_reorg_deep_impl(fork_point_height, new_branch, false)
    }

    #[cfg(test)]
    fn validate_genesis_no_pow(&self, header: &Header) -> Result<(), ChainError> {
        if header.parent_id != genesis_parent_id() {
            return Err(ChainError::InvalidGenesisParent { got: header.parent_id });
        }
        if header.height != 1 {
            return Err(ChainError::InvalidGenesisHeight { got: header.height });
        }
        if header.n_bits != self.config.initial_n_bits {
            return Err(ChainError::WrongDifficulty {
                height: header.height,
                expected: self.config.initial_n_bits,
                got: header.n_bits,
            });
        }
        if let Some(expected_id) = &self.config.genesis_id {
            if &header.id != expected_id {
                return Err(ChainError::GenesisIdMismatch {
                    expected: *expected_id,
                    got: header.id,
                });
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn validate_child_no_pow(&self, header: &Header) -> Result<(), ChainError> {
        let tip = self.tip();

        if header.parent_id != tip.id {
            return Err(ChainError::ParentNotFound { parent_id: header.parent_id });
        }

        if header.height != tip.height + 1 {
            return Err(ChainError::NonSequentialHeight {
                expected: tip.height + 1,
                got: header.height,
            });
        }
        if header.timestamp <= tip.timestamp {
            return Err(ChainError::TimestampNotIncreasing {
                parent_ts: tip.timestamp,
                got: header.timestamp,
            });
        }
        // Skip difficulty check in light-client mode — see `validate_child`.
        if !self.light_client_mode {
            let expected_n_bits = crate::difficulty::expected_difficulty(&tip, self)?;
            if header.n_bits != expected_n_bits {
                return Err(ChainError::WrongDifficulty {
                    height: header.height,
                    expected: expected_n_bits,
                    got: header.n_bits,
                });
            }
        }
        Ok(())
    }

    // --- Validation ---

    fn validate_genesis(&self, header: &Header) -> Result<(), ChainError> {
        if header.parent_id != genesis_parent_id() {
            return Err(ChainError::InvalidGenesisParent {
                got: header.parent_id,
            });
        }

        if header.height != 1 {
            return Err(ChainError::InvalidGenesisHeight {
                got: header.height,
            });
        }

        if header.n_bits != self.config.initial_n_bits {
            return Err(ChainError::WrongDifficulty {
                height: header.height,
                expected: self.config.initial_n_bits,
                got: header.n_bits,
            });
        }

        crate::verify_pow(header)?;

        if let Some(expected_id) = &self.config.genesis_id {
            if &header.id != expected_id {
                return Err(ChainError::GenesisIdMismatch {
                    expected: *expected_id,
                    got: header.id,
                });
            }
        }

        Ok(())
    }

    fn validate_child(&self, header: &Header) -> Result<(), ChainError> {
        let tip = self.tip();

        if header.parent_id != tip.id {
            return Err(ChainError::ParentNotFound {
                parent_id: header.parent_id,
            });
        }

        if header.height != tip.height + 1 {
            return Err(ChainError::NonSequentialHeight {
                expected: tip.height + 1,
                got: header.height,
            });
        }

        if header.timestamp <= tip.timestamp {
            return Err(ChainError::TimestampNotIncreasing {
                parent_ts: tip.timestamp,
                got: header.timestamp,
            });
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if header.timestamp > now_ms + self.config.max_time_drift_ms {
            return Err(ChainError::TimestampTooFarInFuture {
                timestamp: header.timestamp,
                max_allowed: now_ms + self.config.max_time_drift_ms,
            });
        }

        // SPV: light clients cannot recompute expected_difficulty because
        // they lack the historical epoch boundaries the recalc depends on.
        // See `facts/chain.md` Phase 6 light_client_mode invariant.
        if !self.light_client_mode {
            let expected_n_bits = crate::difficulty::expected_difficulty(&tip, self)?;
            if header.n_bits != expected_n_bits {
                return Err(ChainError::WrongDifficulty {
                    height: header.height,
                    expected: expected_n_bits,
                    got: header.n_bits,
                });
            }
        }

        crate::verify_pow(header)?;

        Ok(())
    }
}

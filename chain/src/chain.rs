use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use ergo_chain_types::autolykos_pow_scheme::decode_compact_bits;
use ergo_chain_types::{BlockId, Digest32, Header};
use ergo_lib::chain::parameters::{Parameter, Parameters};
use num_bigint::BigUint;

use crate::cache::LazyHeaderStore;
use crate::config::ChainConfig;
use crate::error::{ChainError, RestoreError};
use crate::voting::{
    default_parameters, default_proposed_update_bytes, ValidationSettingsUpdate,
    SOFT_FORK_VOTE,
};

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

/// One entry in the list returned by
/// [`HeaderChain::install_from_nipopow_proof`]: a header that was just
/// installed plus the chain's internal cumulative-difficulty score
/// for it.
///
/// `score_be` is the big-endian byte encoding of [`BigUint`], suitable
/// for direct persistence via `store.put_header(id, height, fork=0,
/// score=score_be, data=...)`. The chain does NOT persist these — the
/// integrator does; otherwise the store's [`ScoreLoader`] will return
/// `None` on later queries and the chain's score-dependent paths
/// break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledHeader {
    /// Block id of the installed header.
    pub id: BlockId,
    /// Height of the installed header.
    pub height: u32,
    /// Big-endian encoding of the cumulative-difficulty score this
    /// chain assigned to the header. Starts at zero for the
    /// suffix-head entry (cumulative-difficulty across the install
    /// boundary is meaningless — see the install postcondition in
    /// `facts/chain.md`) and accumulates
    /// `decode_compact_bits(header.n_bits)` per subsequent suffix
    /// header.
    pub score_be: Vec<u8>,
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
    /// retired header Vec, which was ~1.4 GB). Also the canonical
    /// source for [`Self::height`] / [`Self::len`] — chain length is
    /// `by_id.len()` and the tip height is `base_height + by_id.len() - 1`.
    by_id: HashMap<BlockId, u32>,
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
    /// Lazy header/score store. After v0.5.0 this is the sole source
    /// of truth for both header and cumulative-score reads at heights
    /// not currently in the LRU cache — the old `Vec<Header>` and
    /// `Vec<BigUint>` safety nets are gone. Integrators MUST register
    /// both a [`HeaderLoader`] and a [`ScoreLoader`] at startup; a
    /// query that misses the cache and has no loader (or whose loader
    /// returns `None`) is treated as "absent" exactly as the contract
    /// specifies.
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
            active_parameters,
            active_proposed_update_bytes,
            extension_loader: None,
            light_client_mode: false,
            lazy: LazyHeaderStore::with_default_capacity(),
        }
    }

    /// Reconstruct a `HeaderChain` from a contiguous best-chain index
    /// of `(height, header_id)` pairs.
    ///
    /// See `facts/chain.md` "Chain restore from store index" for the
    /// full contract. Highlights:
    /// - The first yielded entry's height becomes [`Self::base_height`].
    ///   An empty iterator returns an empty chain.
    /// - Each subsequent entry's height must equal the previous height
    ///   plus one; otherwise [`RestoreError::NonContiguousHeights`] is
    ///   returned and the partially-built chain is discarded.
    /// - A duplicate height OR a duplicate header id is reported as
    ///   [`RestoreError::DuplicateHeight`] for the offending height.
    /// - `light_client_mode` is derived from the first entry's height:
    ///   a chain starting above height 1 must have been installed from
    ///   a NiPoPoW proof, and the SPV difficulty skip is preserved
    ///   across restart. (A chain whose store index starts at height 1
    ///   is treated as a normal full chain.)
    /// - `active_parameters` and `active_proposed_update_bytes` are
    ///   left at construction defaults. The integrator must call
    ///   [`Self::recompute_active_parameters_from_storage`] after
    ///   wiring an extension loader to bring them current.
    /// - The header and score caches are empty; both loaders are
    ///   unwired. The integrator MUST call [`Self::set_header_loader`]
    ///   and [`Self::set_score_loader`] before any query that may miss
    ///   the LRU cache.
    pub fn restore<I>(config: ChainConfig, entries: I) -> Result<Self, RestoreError>
    where
        I: IntoIterator<Item = (u32, BlockId)>,
    {
        let active_parameters = default_parameters(config.network);
        let active_proposed_update_bytes = default_proposed_update_bytes(config.network);

        let mut by_id: HashMap<BlockId, u32> = HashMap::new();
        let mut base_height: Option<u32> = None;
        let mut prev_height: Option<u32> = None;

        for (height, id) in entries {
            match prev_height {
                None => {
                    base_height = Some(height);
                }
                Some(prev) => {
                    let expected = prev + 1;
                    if height != expected {
                        return Err(RestoreError::NonContiguousHeights {
                            expected,
                            got: height,
                        });
                    }
                }
            }
            // Duplicate id at a different height is reported via the
            // offending entry's height — the user wants the height of
            // the duplicate, not of the original.
            if by_id.insert(id, height).is_some() {
                return Err(RestoreError::DuplicateHeight(height));
            }
            prev_height = Some(height);
        }

        let light_client_mode = base_height.unwrap_or(1) > 1;

        Ok(Self {
            config,
            base_height,
            by_id,
            active_parameters,
            active_proposed_update_bytes,
            extension_loader: None,
            light_client_mode,
            lazy: LazyHeaderStore::with_default_capacity(),
        })
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
    /// Derived from [`Self::base_height`] and the `by_id` map size,
    /// which remain in lockstep on every push/pop/reorg path. The
    /// retired `scores: Vec<BigUint>` field used to play this role —
    /// `by_id` carries the same information at no extra cost.
    pub fn height(&self) -> u32 {
        match self.base_height {
            Some(base) if !self.by_id.is_empty() => base + (self.by_id.len() as u32) - 1,
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
        self.by_id.len()
    }

    /// Whether the chain is empty.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Cumulative difficulty score at the chain tip.
    ///
    /// Returns `BigUint::ZERO` on an empty chain. Otherwise delegates
    /// to [`Self::score_at`] for the tip height — that path is a
    /// cache-resident hit in the common case (the tip was just
    /// pushed) and only falls through to the [`ScoreLoader`] when the
    /// cache has been evicted since.
    pub fn cumulative_score(&self) -> BigUint {
        if self.is_empty() {
            return BigUint::ZERO;
        }
        self.score_at(self.height()).unwrap_or_default()
    }

    /// Cumulative difficulty score at the given height, if it exists
    /// in the chain.
    ///
    /// Cache first; on miss falls through to the [`ScoreLoader`]. The
    /// retired in-memory `Vec<BigUint>` safety net is gone — a height
    /// not in cache and not returned by the loader is treated as
    /// absent (`None`). Integrators MUST wire `set_score_loader`
    /// before any query that may miss the LRU window.
    pub fn score_at(&self, height: u32) -> Option<BigUint> {
        let base = self.base_height?;
        if height < base || height > self.height() {
            return None;
        }
        self.lazy.get_score(height)
    }

    /// `true` iff `height` is the start of a new voting epoch.
    ///
    /// Mirrors JVM `(height % votingEpochLength == 0) && height > 0`.
    /// Pure computation; safe to call on an empty chain.
    pub fn is_epoch_boundary(&self, height: u32) -> bool {
        height > 0 && height.is_multiple_of(self.config.voting.voting_length)
    }

    /// Returns the voting epoch length for this network.
    ///
    /// Mainnet: 1024. Testnet: 128. Pure accessor over the chain's
    /// `VotingConfig`; safe to call on an empty chain. Used by sync to
    /// align the `blocks_to_keep` prune horizon to voting-epoch
    /// boundaries so the current epoch's extensions stay intact for
    /// parameter recomputation.
    pub fn voting_length(&self) -> u32 {
        self.config.voting.voting_length
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

    /// Tally the just-ended voting epoch's votes for an epoch boundary —
    /// SEEDED, per JVM `VotingData` (see [`crate::voting::tally_votes_seeded`]).
    ///
    /// For a boundary at `T` the window is `[T - voting_length, T - 1]`:
    /// the window's first header is the PREVIOUS boundary and seeds the
    /// tally; later headers increment only seeded ids. Since the chain's
    /// first valid block is height 1 (no block at height 0), the very
    /// first epoch's window clamps to `[1, T - 1]` — its head is not a
    /// boundary, the seed is empty, and the tally is empty.
    ///
    /// The tally is an ORDERED sequence in seed-slot order, duplicates
    /// preserved — see the order note on
    /// [`crate::voting::tally_votes_seeded`].
    fn tally_just_ended_epoch(
        &self,
        epoch_boundary_height: u32,
    ) -> Result<Vec<(i8, u32)>, ChainError> {
        let voting_length = self.config.voting.voting_length;
        let nominal_start = epoch_boundary_height.saturating_sub(voting_length);
        let start = nominal_start.max(1);
        let end = epoch_boundary_height
            .checked_sub(1)
            .ok_or_else(|| ChainError::Voting("epoch boundary cannot be 0".into()))?;

        if start > end {
            return Ok(Vec::new());
        }

        let mut window: Vec<(u32, [u8; 3])> =
            Vec::with_capacity((end - start + 1) as usize);
        for h in start..=end {
            let header = self.header_at(h).ok_or_else(|| {
                ChainError::Voting(format!(
                    "header at height {h} missing from chain (epoch boundary {epoch_boundary_height})"
                ))
            })?;
            window.push((h, header.votes.0));
        }
        Ok(crate::voting::tally_votes_seeded(
            &window,
            epoch_boundary_height,
            voting_length,
        ))
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
    /// `ErgoValidationSettingsUpdate`), or the empty slice if absent. It
    /// gates the `SubblocksPerBlock` auto-insert at the voting-driven v4
    /// activation and becomes the activated update at an activation
    /// boundary. Bytes that fail the strict parse are pre-normalized to
    /// the canonical EMPTY update — JVM `Parameters.parseExtension`
    /// swallow parity; see the shared `compute_boundary_parameters_at`
    /// and `facts/chain.md` Phase 6.
    ///
    /// **Determinism**: For any two correct implementations given the same
    /// chain history, the output is byte-identical. This is the consensus
    /// rule. Mismatch with the actual block extension = reject the block.
    pub fn compute_expected_parameters(
        &self,
        epoch_boundary_height: u32,
        block_proposed_update: &[u8],
    ) -> Result<Parameters, ChainError> {
        // JVM `forkVote`: the boundary header's OWN votes contain id 120
        // (`ErgoStateContext.process`). The boundary header is in the chain
        // on every validation path (headers sync ahead of bodies). A header
        // absent from the chain casts no vote; candidates assemble via
        // [`Self::compute_expected_parameters_for_candidate`] instead.
        let boundary_fork_vote = self
            .header_at(epoch_boundary_height)
            .is_some_and(|h| h.votes.0.contains(&(SOFT_FORK_VOTE as u8)));

        let (params, _activated_update) = self.compute_boundary_parameters_at(
            epoch_boundary_height,
            block_proposed_update,
            boundary_fork_vote,
        )?;
        Ok(params)
    }

    /// Candidate-aware sibling of [`Self::compute_expected_parameters`]:
    /// identical pipeline, but `boundary_fork_vote` derives from the
    /// SUPPLIED `candidate_votes` (id-120 membership) instead of
    /// `header_at(T)` — a mining candidate is not in the chain yet.
    ///
    /// `mining.votes` is operator config: a soft-fork-voting boundary
    /// candidate assembled via the header-reading method would omit its
    /// own fork-round start from the declared table while its header
    /// carries the vote — every validator recomputes with the vote present
    /// and rejects, so the miner self-orphans exactly when voting. This
    /// entry point threads the candidate's votes through instead.
    ///
    /// Also returns the activated update (the PARSED VALUE of
    /// `block_proposed_update` at a voting-driven activation boundary,
    /// the EMPTY value otherwise) — uniform with the pure seam. The
    /// mining caller may ignore it: the extension's `[0x00, 124]` key
    /// carries the PROPOSED update, which the caller already holds. When
    /// wire bytes of the value are needed, encode via
    /// [`crate::voting::encode_validation_settings_update`] (fallible —
    /// see its docs).
    pub fn compute_expected_parameters_for_candidate(
        &self,
        epoch_boundary_height: u32,
        block_proposed_update: &[u8],
        candidate_votes: [u8; 3],
    ) -> Result<(Parameters, ValidationSettingsUpdate), ChainError> {
        let boundary_fork_vote = candidate_votes.contains(&(SOFT_FORK_VOTE as u8));
        self.compute_boundary_parameters_at(
            epoch_boundary_height,
            block_proposed_update,
            boundary_fork_vote,
        )
    }

    /// Shared body of [`Self::compute_expected_parameters`] and
    /// [`Self::compute_expected_parameters_for_candidate`]: seeded tally
    /// over the closing epoch, then delegate to the pure
    /// [`crate::voting::compute_boundary_parameters`]. The two public
    /// entry points differ only in where `boundary_fork_vote` comes from.
    ///
    /// Pre-normalizes the block's raw ID 124 bytes through the strict
    /// parse — JVM `Parameters.parseExtension` swallow parity
    /// (`Parameters.scala:382-386`, `parseBytesTry(...).toOption
    /// .getOrElse(empty)`): on ANY parse failure the bytes are replaced
    /// by the canonical EMPTY update encoding before delegating. In-band,
    /// a hostile 124 field is INERT and the block is ACCEPTED; rejecting
    /// it would be reject-what-JVM-accepts fork bait. The pure seam
    /// itself stays strict for directly-handed bytes.
    fn compute_boundary_parameters_at(
        &self,
        epoch_boundary_height: u32,
        block_proposed_update: &[u8],
        boundary_fork_vote: bool,
    ) -> Result<(Parameters, ValidationSettingsUpdate), ChainError> {
        let tally = self.tally_just_ended_epoch(epoch_boundary_height)?;
        let normalized_empty;
        let proposed_update: &[u8] =
            if crate::voting::parse_validation_settings_update(block_proposed_update).is_ok() {
                block_proposed_update
            } else {
                normalized_empty = crate::voting::encode_disabled_rules(&[]);
                &normalized_empty
            };
        crate::voting::compute_boundary_parameters(
            &self.config.voting,
            epoch_boundary_height,
            &self.active_parameters,
            &tally,
            boundary_fork_vote,
            proposed_update,
        )
    }

    /// Tally header vote slots across one voting epoch — SEEDED, per JVM
    /// `VotingData` (see [`crate::voting::tally_votes_seeded`]).
    ///
    /// `epoch_end_height` is `T - 1` for the boundary at `T`: the window is
    /// `[T - voting_length, T - 1]` (clamped to start at height 1 — the
    /// chain-start window has an empty seed and tallies empty). The
    /// window's first header — the previous boundary — seeds the tally;
    /// later headers increment only seeded ids. Delegates to the pure
    /// [`crate::voting::tally_votes_seeded`]; exposed for testability,
    /// [`Self::compute_expected_parameters`] uses the same window
    /// internally.
    ///
    /// Returns an ORDERED sequence in seed-slot order, duplicates
    /// preserved (JVM `VotingData.epochVotes` is `Array[(Byte, Int)]`) —
    /// see the order note on [`crate::voting::tally_votes_seeded`].
    ///
    /// Errors if any header in the (clamped) window is missing from the
    /// chain — callers must ensure the precondition holds before calling.
    pub fn count_votes_in_epoch(
        &self,
        epoch_end_height: u32,
    ) -> Result<Vec<(i8, u32)>, ChainError> {
        let voting_length = self.config.voting.voting_length;
        if voting_length == 0 {
            return Err(ChainError::Voting(
                "voting_length must be > 0".into(),
            ));
        }
        let boundary_height = epoch_end_height.checked_add(1).ok_or_else(|| {
            ChainError::Voting(format!(
                "epoch_end_height {epoch_end_height} overflows the boundary height"
            ))
        })?;

        let start = boundary_height.saturating_sub(voting_length).max(1);
        if start > epoch_end_height {
            return Ok(Vec::new());
        }

        let mut window: Vec<(u32, [u8; 3])> =
            Vec::with_capacity((epoch_end_height - start + 1) as usize);
        for h in start..=epoch_end_height {
            let header = self.header_at(h).ok_or_else(|| {
                ChainError::Voting(format!(
                    "header at height {h} missing from chain (epoch end {epoch_end_height})"
                ))
            })?;
            window.push((h, header.votes.0));
        }

        Ok(crate::voting::tally_votes_seeded(
            &window,
            boundary_height,
            voting_length,
        ))
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
    ) -> Result<Vec<InstalledHeader>, ChainError> {
        self.install_from_nipopow_proof_impl(suffix_head, suffix_tail, true)
    }

    fn install_from_nipopow_proof_impl(
        &mut self,
        suffix_head: Header,
        suffix_tail: Vec<Header>,
        verify_pow: bool,
    ) -> Result<Vec<InstalledHeader>, ChainError> {
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

        let mut installed: Vec<InstalledHeader> =
            Vec::with_capacity(1 + suffix_tail.len());

        // Push suffix_head bypassing validate_genesis: it is rarely actually
        // genesis (height 1) and its parent_id is whatever the upstream chain
        // happens to be. The installed score for the suffix head starts at 0
        // — see doc above.
        let head_id = suffix_head.id;
        let head_height = suffix_head.height;
        self.base_height = Some(head_height);
        self.by_id.insert(head_id, head_height);
        self.lazy.put(head_height, suffix_head, BigUint::ZERO);
        self.light_client_mode = true;
        installed.push(InstalledHeader {
            id: head_id,
            height: head_height,
            score_be: BigUint::ZERO.to_bytes_be(),
        });

        // Walk suffix_tail. On any failure, roll back EVERYTHING (including
        // suffix_head and the light_client_mode flag) so the chain is
        // unchanged on Err per the postcondition.
        for header in suffix_tail {
            let tip = self.tip();
            if header.parent_id != tip.id {
                self.rollback_install();
                return Err(ChainError::ParentNotFound {
                    parent_id: header.parent_id,
                });
            }
            if header.height != tip.height + 1 {
                let expected = tip.height + 1;
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
            // parent_score + diff. Since the head's score = 0, this just
            // accumulates diffs from the install boundary onwards — only
            // deltas matter for post-install reorg comparisons.
            let height = header.height;
            let id = header.id;
            self.push_header(header);
            let score = self
                .score_at(height)
                .expect("score must be cache-resident immediately after push_header");
            installed.push(InstalledHeader {
                id,
                height,
                score_be: score.to_bytes_be(),
            });
        }

        Ok(installed)
    }

    /// Test variant of [`Self::install_from_nipopow_proof`] that skips the
    /// per-header PoW check. Used by unit tests on synthetic chains where
    /// headers don't carry real Autolykos solutions.
    #[cfg(test)]
    pub(crate) fn install_from_nipopow_proof_no_pow(
        &mut self,
        suffix_head: Header,
        suffix_tail: Vec<Header>,
    ) -> Result<Vec<InstalledHeader>, ChainError> {
        self.install_from_nipopow_proof_impl(suffix_head, suffix_tail, false)
    }

    /// Roll back a partial light-client install. Used internally only.
    fn rollback_install(&mut self) {
        self.base_height = None;
        self.by_id.clear();
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

    /// Push a validated header onto the chain.
    ///
    /// Looks up the parent's cumulative score via [`Self::score_at`]
    /// (cache + [`ScoreLoader`] fall-through), adds the header's
    /// difficulty contribution, and writes through to the lazy store
    /// plus `by_id`. On the first push (empty chain) the parent score
    /// is `0` and `base_height` is set to the pushed header's height.
    ///
    /// # Panics
    ///
    /// On a non-empty chain, panics if the parent's score cannot be
    /// resolved through cache or loader. That can only happen if the
    /// integrator failed to wire a [`ScoreLoader`] AND the parent has
    /// been evicted from the LRU cache — a configuration bug per the
    /// `facts/chain.md` contract.
    fn push_header(&mut self, header: Header) {
        let is_first = self.base_height.is_none();
        if is_first {
            self.base_height = Some(header.height);
        }
        let parent_score = if is_first {
            BigUint::ZERO
        } else {
            self.score_at(header.height - 1).unwrap_or_else(|| {
                panic!(
                    "parent score at height {} unavailable during push_header — \
                     cache evicted with no ScoreLoader wired",
                    header.height - 1,
                )
            })
        };
        let diff = header_difficulty(&header);
        let score = parent_score + diff;
        let height = header.height;
        let id = header.id;
        self.lazy.put(height, header, score);
        self.by_id.insert(id, height);
    }

    /// Pop the tip header and its score. Returns both for rollback.
    /// Clears [`Self::base_height`] when the chain becomes empty.
    fn pop_header(&mut self) -> Option<(Header, BigUint)> {
        if self.is_empty() {
            return None;
        }
        let tip_height = self.height();
        let header = self.lazy.get_header(tip_height).expect(
            "tip header unavailable during pop — cache evicted with no loader wired",
        );
        let score = self.score_at(tip_height).unwrap_or_default();
        self.by_id.remove(&header.id);
        self.lazy.evict(tip_height);
        if self.by_id.is_empty() {
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
        self.lazy.put(height, header, score);
    }

    fn try_reorg_impl(
        &mut self,
        alternative_tip: Header,
        continuation: Header,
        verify_pow: bool,
    ) -> Result<BlockId, ChainError> {
        // Need at least 2 headers — can't reorg genesis.
        if self.by_id.len() < 2 {
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
        let tip_height = self.height();
        if fork_point_height < base || fork_point_height > tip_height {
            return Err(ChainError::Reorg(format!(
                "fork point height {fork_point_height} not in chain"
            )));
        }

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

        // Collect headers AND scores above the fork point so we can
        // restore on failure. Pull from cache || loader — capturing
        // scores BEFORE eviction is load-bearing now that the Vec
        // safety net is gone. If any is missing we can't safely reorg
        // and we bail before mutating state.
        let saved_capacity = (tip_height - fork_point_height) as usize;
        let mut saved_headers: Vec<Header> = Vec::with_capacity(saved_capacity);
        let mut saved_scores: Vec<BigUint> = Vec::with_capacity(saved_capacity);
        for h in (fork_point_height + 1)..=tip_height {
            let hdr = self.lazy.get_header(h).ok_or_else(|| {
                ChainError::Reorg(format!(
                    "header at height {h} unavailable for reorg drain"
                ))
            })?;
            let score = self.score_at(h).ok_or_else(|| {
                ChainError::Reorg(format!(
                    "score at height {h} unavailable for reorg drain"
                ))
            })?;
            saved_headers.push(hdr);
            saved_scores.push(score);
        }
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
            // Rollback: pop any new headers we added (their heights
            // sit strictly above the fork point).
            while self.height() > fork_point_height {
                self.pop_header();
            }
            // Restore saved state.
            for (header, score) in saved_headers.into_iter().zip(saved_scores) {
                let height = header.height;
                let id = header.id;
                self.by_id.insert(id, height);
                self.lazy.put(height, header, score);
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

        // JVM validateVotes (rules 212-214): reject malformed vote fields.
        crate::voting::check_header_votes(header.votes.0)?;
        self.check_fork_vote_gate(header)?;

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
        // JVM validateVotes (rules 212-214): reject malformed vote fields.
        crate::voting::check_header_votes(header.votes.0)?;
        self.check_fork_vote_gate(header)?;
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

        // JVM validateVotes (rules 212-214): reject malformed vote fields.
        crate::voting::check_header_votes(header.votes.0)?;
        self.check_fork_vote_gate(header)?;

        Ok(())
    }

    /// JVM `ErgoStateContext.process` → `checkForkVote` (rule 407,
    /// `ErgoStateContext.scala:243`): every header carrying an id-120
    /// vote — boundary and mid-epoch alike — is checked against the
    /// ACTIVE parameters; voting for a fork inside the closing or
    /// activation window of the current round invalidates the header.
    /// Inert while no round is in progress (no 122 in the active table).
    ///
    /// Chain-entangled delegation onto the pure
    /// [`crate::voting::check_fork_vote`] seam. Both `Ok(false)` (rule
    /// 407 fires) and `Err` (orphan-122 table) reject the header —
    /// JVM-indistinguishable on the live path.
    fn check_fork_vote_gate(&self, header: &Header) -> Result<(), ChainError> {
        if crate::voting::check_fork_vote(
            &self.config.voting,
            header.height,
            header.votes.0,
            &self.active_parameters,
        )? {
            Ok(())
        } else {
            Err(ChainError::Voting(format!(
                "voting for fork is prohibited at height {}",
                header.height
            )))
        }
    }
}

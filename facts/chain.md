# Header Chain Contract

## Component: `chain/` (enr-chain)

Owns header parsing, tracking, PoW verification, difficulty adjustment, and
header chain validation. The single authority on "is this chain of headers valid?"

Primary consumers: P2P layer (feeds raw bytes), sync state machine (queries chain state),
block validation (checks header membership).

## Phase 1: Header Awareness

### `parse_header(body: &[u8]) -> Result<Header>`
- **Precondition**: `body` is the raw payload from a ModifierResponse with modifier_type = 101 (Header).
- **Postcondition**: Returns a fully populated `ergo-chain-types::Header` or an error.
  Never panics on malformed input.

### `HeaderTracker`

Stateless observer. Tracks headers seen on the network without validating chain linkage.

#### `observe(header: &Header)`
- **Postcondition**: If `header.height > best_height()`, updates best known tip.
- **Invariant**: Best tip is always the highest header observed.

#### `best_height() -> Option<u32>`
- Returns the height of the highest header seen, or None if no headers observed.

#### `best_header_id() -> Option<HeaderId>`
- Returns the ID of the highest header seen.

## Phase 2: PoW Verification

### `verify_pow(header: &Header) -> Result<()>`
- **Precondition**: Header is parsed (Phase 1).
- **Postcondition**: Ok if `pow_hit(header) <= target(header.n_bits)`. Err otherwise.
- **Uses**: `ergo-chain-types::AutolykosPowScheme::pow_hit()`.
- **Cost**: One hash computation. Cheap enough to call on every header before forwarding.
- **Invariant**: A header that fails PoW is never valid regardless of chain context.

## Phase 3: Header Chain Validation

### `HeaderChain`

Maintains a validated chain of headers. Every header in the chain has been checked for:
parent linkage, PoW validity, timestamp bounds, and correct difficulty.

#### `try_append(header: Header) -> Result<()>`
- **Precondition**: Header is parsed and PoW-verified.
- **Postcondition on Ok**: Header is added to the chain. `height()` may increase.
- **Postcondition on Err**: Chain is unchanged. Error describes which check failed.
- **Validates**:
  - `header.parent_id` matches an existing header in the chain
  - `header.timestamp` is within acceptable bounds relative to parent
  - `header.n_bits` matches the expected difficulty for this height (see difficulty adjustment)
  - PoW is valid for the claimed difficulty

#### `height() -> u32`
- Returns the height of the best validated chain tip.

#### `header_at(height: u32) -> Option<Header>`
- Returns the header at the given height on the best chain, if it exists.
- **Ownership**: Returns an owned `Header`. Chain no longer materializes
  every header in memory; lookups fall through a bounded LRU cache to a
  `HeaderLoader` (see "Lazy header store" below). Callers that previously
  held `&Header` tied to `&HeaderChain` now hold an owned value that
  outlives any chain mutation — strictly more permissive.

#### `tip() -> Header`
- **Precondition**: Chain is non-empty (at least genesis or bootstrap point).
- Returns the tip header of the best validated chain.
- **Ownership**: Same as `header_at` — returns owned. The tip is always
  resident in cache (just pushed), so this never hits the loader.
- Panics on an empty chain — unchanged behavior; only the return ownership
  changed.

#### `contains(header_id: &HeaderId) -> bool`
- Returns whether this header ID is part of the validated chain.

#### `headers_from(height: u32, count: usize) -> Vec<Header>`
- Returns up to `count` sequential owned headers starting at `height`.
- Used by sync to serve header chains to peers.
- **Ownership**: Returns owned `Header`s. Consumers using `.iter()` on the
  result still get `&Header` via the owned `Vec`, no call-site change
  required.

#### `score_at(height: u32) -> Option<BigUint>`
- Returns the cumulative difficulty score at the given height on the best
  chain, if it exists.
- **Ownership**: Owned `BigUint`. Same LRU-cache + loader pattern as
  `header_at`, via the separate `ScoreLoader` (see below). Split from the
  header loader so walks that only need headers (difficulty recalc,
  NiPoPoW) don't pay `BigUint` deserialization on every lookup.

#### `cumulative_score() -> BigUint`
- Returns the cumulative difficulty score at the chain tip.
- Returns `BigUint::ZERO` on an empty chain (pre-existing behavior).
- In the common case the tip's score is cache-resident and this does not
  hit the loader.

### Lazy header store

`HeaderChain` owns a bounded LRU cache of recently-accessed headers (and a
parallel cache for scores). Misses fall through to a loader registered by
the integrator (typically backed by `enr-store`). This replaces the prior
"materialize every header in memory" behavior; at 1.76M mainnet headers
that was ~1.4 GB of live heap.

```rust
pub type HeaderLoader =
    Arc<dyn Fn(u32) -> Option<Header> + Send + Sync + 'static>;

pub type ScoreLoader =
    Arc<dyn Fn(u32) -> Option<BigUint> + Send + Sync + 'static>;

pub const DEFAULT_CACHE_CAPACITY: usize = 16_384;
```

`DEFAULT_CACHE_CAPACITY = 16_384` is sized to cover the difficulty
adjustment walk (mainnet `use_last_epochs * epoch_length = 8 * 1024 =
8192` headers) plus a full `finalization_depth` deep reorg (1440 blocks)
with generous slack.

#### `set_header_loader<F>(loader: F)`
Where `F: Fn(u32) -> Option<Header> + Send + Sync + 'static`.
- **Postcondition**: Subsequent `header_at` / `tip` / `headers_from` calls
  that miss the in-process cache consult the loader. If the loader
  returns `None` for a queried height, the chain returns `None` for that
  query.
- **Wired by**: the integrator (main crate) to bridge `enr-store`'s
  height-indexed header reads.

#### `set_score_loader<F>(loader: F)`
Where `F: Fn(u32) -> Option<BigUint> + Send + Sync + 'static`.
- **Postcondition**: Subsequent `score_at` / `cumulative_score` calls
  that miss the cache consult the loader.

#### `has_header_loader() -> bool` / `has_score_loader() -> bool`
- Diagnostic queries so the integrator can assert wiring completeness
  before the first user query arrives.

#### `set_cache_capacity(capacity: NonZeroUsize)`
- **Postcondition**: Both the header cache and the score cache are
  resized to `capacity` in place. Excess entries beyond the new cap are
  evicted in LRU order.
- **Default**: `DEFAULT_CACHE_CAPACITY` (16_384 entries).

### Cache invariants

- **Write-through**. `push_header`, `pop_header`, `restore_header`,
  `rollback_install`, `install_from_nipopow_proof`, and the deep-reorg
  drain/restore paths all update the cache in lockstep with the canonical
  state. A cache hit is never stale relative to the best chain at the
  moment of the hit.
- A cache miss that falls through to a loader returning `None` is
  equivalent to the queried height being absent from the chain — the
  public read method returns `None` (or panics, for `tip()` on an empty
  chain).
- Reorg rollback evicts the affected heights from both caches before
  installing the new fork — post-reorg queries re-populate from the
  loader or the fork's new headers.

### Difficulty Adjustment

#### `expected_difficulty(parent: &Header, chain: &HeaderChain) -> Result<u64>`
- **Precondition**: `parent` is in the chain.
- **Postcondition**: Returns the required nBits for the next header after `parent`.
- **Algorithm**: Epoch-based recalculation. Port from JVM `ergo-core` DifficultyAdjustment.
- **Invariant**: For any two correct implementations given the same chain, the output is identical.
  This is consensus-critical — must match the JVM node exactly.

## SyncInfo Serialization

Build and parse SyncInfo messages (P2P message code 65). Used by the sync
state machine to compare chain tips with peers.

Two wire formats exist. V2 is used by all current nodes (>= 4.0.16).

### `build_sync_info(headers: &[Header]) -> Vec<u8>`
- **Precondition**: `headers` contains up to 50 recent headers from the chain tip.
- **Postcondition**: Returns V2 SyncInfo body bytes ready for framing.
- **Format**: `[0x00, 0x00][0xFF][count: 1 byte][header_size: VLQ u16, header_bytes] × count`
- The `[0x00, 0x00]` prefix (count=0 in V1 framing) signals V2 to older parsers.

### `parse_sync_info(body: &[u8]) -> Result<SyncInfo>`
- **Precondition**: `body` is the raw payload from a SyncInfo message (code 65).
- **Postcondition**: Returns parsed sync info — either V1 (header IDs only) or V2 (full headers).
- Never panics on malformed input.
- Rejects V2 messages with more than 50 headers or headers larger than 1000 bytes.

### `SyncInfo` enum
- `V1 { header_ids: Vec<BlockId> }` — legacy, list of 32-byte header IDs
- `V2 { headers: Vec<Header> }` — current, full serialized headers

## Invariants (all phases)

- No method panics on untrusted input.
- `HeaderChain` is append-only for the best chain. Forks are tracked but the best chain
  is selected by cumulative difficulty.
- The chain never contains two headers at the same height on the same fork.
- All timestamps are treated as untrusted data. Timing logic uses block height.

## State Type

### `StateType` enum
- `Utxo` — maintain the full UTXO set, validate transactions by direct input lookup.
  Does not need AD proofs.
- `Digest` — maintain only the AVL+ tree root hash, validate state transitions via
  authenticated dictionary proofs (AD proofs). Requires downloading AD proofs from peers.
- `Light` — NiPoPoW light-client mode. Downloads NO block bodies. Bootstraps the
  header chain from a verified NiPoPoW proof's suffix and follows the tip
  thereafter. No transaction validation runs in this mode.

Mirrors JVM's `StateType` enum (`Utxo`/`Digest`). The `Light` variant is a
Rust-side addition that has no direct JVM analog — JVM expresses light-client
mode as the orthogonal `nipopowBootstrap` flag layered on top of `Digest`. We
collapse the two-flag combination into a single state-type variant because the
shape of work in light mode (no block bodies, no validator) is sufficiently
different that gating it with a boolean on `Digest` would require parallel
"is light?" checks throughout sync, validator wiring, and section download. The
variant carries the distinction at the type level instead.

### `StateType::requires_proofs() -> bool`
- Returns `true` for `Digest`, `false` for `Utxo` and `Light`.
- Mirrors JVM's `stateType.requireProofs` for the JVM-equivalent variants.
- `Light` returns `false` because it downloads no block bodies at all — the
  question of whether AD proofs are needed is moot.

### `StateType::downloads_block_bodies() -> bool`
- Returns `true` for `Utxo` and `Digest`, `false` for `Light`.
- Used by sync to gate the entire block-section download phase. Light mode
  skips section queue construction, the watermark scanner, and the block
  validator wiring.

## Block Section IDs

### `section_ids(header: &Header) -> [(u8, [u8; 32]); 3]`
- **Precondition**: Header is parsed.
- **Postcondition**: Returns the modifier IDs for all three non-header block sections:
  - `(102, Blake2b256(102 || header.id || header.transaction_root))` — BlockTransactions
  - `(104, Blake2b256(104 || header.id || header.ad_proofs_root))` — ADProofs
  - `(108, Blake2b256(108 || header.id || header.extension_root))` — Extension
- **Pure computation**. No I/O, no state.
- Matches JVM `Header.sectionIds`.

### `required_section_ids(header: &Header, state_type: StateType) -> Vec<(u8, [u8; 32])>`
- **Precondition**: Header is parsed.
- **Postcondition**: Returns modifier IDs for sections required by the given state type:
  - `Utxo` → BlockTransactions + Extension (2 entries). Matches JVM `Header.sectionIdsWithNoProof`.
  - `Digest` → all three including ADProofs (3 entries). Matches JVM `Header.sectionIds`.
  - `Light` → empty `Vec` (0 entries). Light clients download no block sections.
- Mirrors JVM's `ToDownloadProcessor.requiredModifiersForHeader`. The `Light`
  case has no JVM analog (JVM gates section download via `nipopowBootstrap`
  rather than `stateType`); returning empty here lets sync's section-queue
  construction handle Light without a special case at the call site.

## Phase 6: Soft-Fork Voting

Track and apply blockchain parameter changes voted on by miners. The vote
counting and parameter computation are consensus-critical: a receiver MUST
independently compute the new parameters from the previous epoch's votes
and verify they match the parameters in the next epoch-boundary block's
extension. Mismatch = reject the block. Get this wrong and the node forks.

Soft-fork voting (BlockVersion bumps) requires multi-epoch state machinery:
voting period → activation period → version increment. The activation
machinery is part of consensus and must be implemented in full.

JVM reference: `ergo-core/src/main/scala/org/ergoplatform/settings/Parameters.scala`,
`VotingSettings.scala`. Read these before implementing.

### `VotingConfig`

```rust
pub struct VotingConfig {
    /// Length of one voting epoch in blocks. Mainnet: 1024. Testnet: 128.
    pub voting_length: u32,
    /// Voting epochs collected before a soft-fork can be approved. Both nets: 32.
    pub soft_fork_epochs: u32,
    /// Voting epochs after approval before BlockVersion is incremented. Both nets: 32.
    pub activation_epochs: u32,
    /// JVM hard-coded protocol-v2 forced activation height. Mainnet: 417792.
    pub version2_activation_height: u32,
}
```

Derived from network type — not a runtime config entry. The chain submodule
selects testnet vs mainnet values internally.

### `ActiveParameters`

The set of parameters currently in effect at the chain tip. Used by the
validator to bound transaction costs and by the mining task when assembling
epoch-boundary blocks.

Wraps `ergo_lib::chain::parameters::Parameters` (already used by `validation/`).
Chain owns the live instance; consumers query it via `active_parameters()`.

### `active_parameters() -> &Parameters`
- **Postcondition**: Returns the parameters in effect at the current tip.
- **Invariant**: The returned parameters were computed from the chain history
  ending at the current tip. After every successfully appended epoch-boundary
  block, this value advances to the new params.
- **Startup**: Recomputed from chain history during construction (see
  "Startup recomputation" below).

### `compute_expected_parameters(epoch_boundary_height: u32, block_proposed_update: &[u8]) -> Result<Parameters>`
- **Precondition**: `epoch_boundary_height` is the height of an epoch-boundary
  block (the FIRST block of a new epoch). The chain must contain all headers
  in the just-ended voting epoch (`[epoch_boundary_height - voting_length, epoch_boundary_height - 1]`).
  `block_proposed_update` is the raw payload of the key `[0x00, 124]`
  (`SoftForkDisablingRules`, i.e. `ErgoValidationSettingsUpdate`) in the
  epoch-boundary block's extension, if present; empty slice otherwise (JVM
  treats an absent field as `ErgoValidationSettingsUpdate.empty`).
- **Postcondition**: Returns the parameters that the block at
  `epoch_boundary_height` MUST emit in its extension. If the block's extension
  contains different parameters, the block is invalid.
- **Algorithm**: Port of JVM `Parameters.update`:
  1. Tally votes from headers in the just-ended voting epoch (see
     `count_votes_in_epoch`).
  2. Apply ordinary parameter changes (IDs 1-8): for each tallied param, if
     `count > voting_length / 2` (`changeApproved`), apply one step from
     `Parameters.stepsTable` clamped by `minValues`/`maxValues`. Positive
     param ID = increase, negative ID = decrease.
  3. Apply soft-fork machinery (see "Soft-fork lifecycle" below).
  4. Return the new `Parameters` table.
- **`block_proposed_update` usage**: consulted **only** to gate the
  `BlockVersion == 4` auto-insert of `SubblocksPerBlock` (ID 9), and **only**
  when the current call is a voting-driven activation (BlockVersion bumped
  from ≠4 to 4 in this update — not the forced-v2 mainnet activation at
  `version2_activation_height`). Parsed internally via a minimal
  `ErgoValidationSettingsUpdate` decoder (just `rulesToDisable`; we don't
  need `statusUpdates`). If the parsed rule list contains **409**
  (`exMatchParameters`), the auto-insert is skipped — JVM mainnet activates
  rule 409 at the same boundary as the v6 BlockVersion bump, so the insert
  fires only at the **next** boundary. At non-activation boundaries the
  payload is ignored entirely.
- **Determinism**: For any two correct implementations given the same chain,
  the output is byte-identical. This is the consensus rule.

### `count_votes_in_epoch(epoch_end_height: u32) -> Result<HashMap<i8, u32>>`
- **Precondition**: All headers in `[epoch_end_height - voting_length + 1, epoch_end_height]`
  are present in the chain.
- **Postcondition**: Returns the per-paramId vote count, summed across all
  three vote slots in each header's `votes` field. Each slot is one signed
  byte: positive = increase, negative = decrease, 0 = no vote, 120 = SoftFork.
- **Helper for `compute_expected_parameters`**, exposed for testability.

### `active_proposed_update_bytes() -> &[u8]`
- **Postcondition**: Returns the raw `ErgoValidationSettingsUpdate`
  encoding (JVM `Parameters.proposedUpdate`) in effect at the current
  chain tip. This is the exact payload of extension key `[0x00, 124]`
  (`SoftForkDisablingRules`) from the most recently applied
  epoch-boundary block.
- **Invariant**: On a fresh chain (before any boundary has been
  applied) returns `default_proposed_update_bytes(network)` — JVM
  `LaunchParameters.proposedUpdate` encoded via
  `encode_disabled_rules(&[215, 409])` (6 bytes on both nets).
  After every accepted boundary block, advances to that block's
  exact ID 124 bytes. Forms the "expected" side of JVM
  `Parameters.matchParameters60`'s `proposedUpdate` comparison;
  the main-crate validator runs that comparison gated on
  `BlockVersion >= Interpreter60Version` (no-op on mainnet until
  h=1,628,160).
- **Rationale for raw bytes**: `ErgoValidationSettingsUpdateSerializer`
  writes canonically (sorted `rulesToDisable` + `statusUpdates`), so
  byte-for-byte comparison is equivalent to structural equality, and
  we avoid pulling a full `ErgoValidationSettingsUpdate` type into
  the chain crate (sigma-rust does not yet expose one on its
  `Parameters`). `statusUpdates` modeling is future work — the
  current encoder emits empty statusUpdates, which is correct for the
  launch default but diverges from on-chain mainnet payloads that
  carry 3 status updates since before h=1,562,624.

### `apply_epoch_boundary_parameters(params: Parameters, proposed_update_bytes: Vec<u8>)`
- **Preconditions**:
  - `params` was returned by `compute_expected_parameters` for the
    height of the just-validated epoch-boundary block AND was
    confirmed to match the params parsed from that block's extension.
  - `proposed_update_bytes` is the block's exact ID 124 extension
    value (empty `Vec` if absent — JVM's
    `ErgoValidationSettingsUpdate.empty` convention). On
    `BlockVersion >= Interpreter60Version` the validator has already
    compared this byte-for-byte against
    `active_proposed_update_bytes()` before calling.
- **Postcondition**: `active_parameters()` returns `params` AND
  `active_proposed_update_bytes()` returns `proposed_update_bytes`.
  Both fields advance atomically — no partial updates.
- **Called by**: block-application pipeline after the epoch-boundary
  block has passed all checks. Validators do NOT call this (they
  are stateless w.r.t. chain state mutation).

### `is_epoch_boundary(height: u32) -> bool`
- **Postcondition**: Returns true iff `height % voting_length == 0` AND
  `height > 0`. Matches JVM's `(height % votingEpochLength == 0)`.
- **Pure computation**, no chain access. Used by validator and mining task.

### Soft-fork lifecycle

The soft-fork machinery uses three reserved param IDs:

| ID | Name | Purpose |
|----|------|---------|
| 120 | `SoftFork` | Vote slot value (not stored in parametersTable) |
| 121 | `SoftForkVotesCollected` | Running tally of soft-fork votes since voting started |
| 122 | `SoftForkStartingHeight` | Height at which the current vote period began |
| 123 | `BlockVersion` | Current block version. Bumped on successful activation. |

State transitions inside `compute_expected_parameters` (port of
`Parameters.updateFork`):

1. **Successful voting cleanup**: at
   `softForkStartingHeight + votingLength * (softForkEpochs + activationEpochs + 1)`
   AND `softForkApproved`, remove IDs 121 and 122 from the table.
2. **Unsuccessful voting cleanup**: at
   `softForkStartingHeight + votingLength * (softForkEpochs + 1)` AND NOT
   `softForkApproved`, remove IDs 121 and 122 from the table.
3. **New voting start**: when fork vote present AND no current voting OR
   prior voting cleanup just happened, set IDs 122 = current height, 121 = 0.
4. **Mid-voting epoch**: when `height <= startingHeight + votingLength * softForkEpochs`,
   add the new epoch's fork votes to ID 121.
5. **Activation**: at
   `softForkStartingHeight + votingLength * (softForkEpochs + activationEpochs)`
   AND `softForkApproved`, increment ID 123 (BlockVersion).
6. **Forced v2 activation**: at `version2_activation_height`, force ID 123 = 2 if
   currently 1. Mainnet hard-fork that pre-dates the voting machinery.

`softForkApproved(votes) = votes > voting_length * soft_fork_epochs * 9 / 10`
(90% supermajority across all soft-fork voting epochs).

### Startup recomputation

`recompute_active_parameters_from_storage(target_height: u32)` rebuilds
`active_parameters()` from storage to reflect the parameters in effect at
`target_height`.

`target_height` is the height the validator is about to resume validating
from — generally far behind the chain tip on a fresh resync, identical to
the chain tip on a normal restart. The chain tip is **not** the right
input for this function: a chain whose headers reach v6-era heights but
whose UTXO state needs to be re-validated from genesis must load the v1-era
parameter table at startup, not the v6-era one. Otherwise the locally
computed expected table at the first epoch boundary (mainnet 1024) carries
extra entries that the v1-era extension does not, and validation fails.

Algorithm:

1. If `target_height < voting_length`, no-op success — no epoch-boundary
   block exists at or before that height, so `active_parameters` is left at
   the chain-internal defaults from `HeaderChain::new`.
2. Otherwise compute `boundary_height = (target_height / voting_length) *
   voting_length` (the most recent epoch-boundary at or before
   `target_height`).
3. Read the extension at `boundary_height` via the registered extension
   loader.
4. Parse parameters from the extension via JVM-equivalent logic (key prefix
   `0x00` + 1-byte param ID + 4-byte BE i32 value, except ID 124
   `SoftForkDisablingRules` which has variable-length encoding).
5. Verify the extension's `header_id` field matches the chain's header at
   `boundary_height`; mismatch is an error.
6. Extract the raw ID 124 payload from the extension's key-value
   pairs. If present, set `active_proposed_update_bytes` to those
   bytes. If absent, fall back to
   `default_proposed_update_bytes(network)` — JVM treats an absent
   ID 124 field as `ErgoValidationSettingsUpdate.empty`, but a
   post-v6 mainnet boundary without the field would indicate a
   corrupt extension; the fallback keeps the field well-formed for
   the subsequent boundary's `apply_epoch_boundary_parameters` call
   to overwrite.
7. Set `active_parameters` to the parsed value.

Errors are returned only when a load is required (i.e., `target_height ≥
voting_length`) and one of: the loader is unset; the loader returns `None`;
the extension bytes fail to parse; the boundary header is missing from the
chain (caller misuse — `target_height` exceeds the chain's known headers);
the extension's header_id disagrees with the chain.

Cost: bounded — exactly one extension read in the load case, zero in the
no-op case. Acceptable at startup. NOT cached to disk: derived state,
divergence-prone.

The integrator (main crate) wires this on startup, passing the validator's
current state height (the highest block already applied to UTXO state). On
a fresh genesis resync the state height is `0`, the function is a no-op,
and the chain's default parameters carry through to the first epoch
boundary's `compute_expected_parameters` call — which is what JVM
`Parameters` does in the equivalent path.

### Voting invariants

- `active_parameters` and `active_proposed_update_bytes` advance ONLY
  at epoch-boundary blocks, and they advance together via a single
  `apply_epoch_boundary_parameters` call. Within an epoch both are
  constant.
- The receiver MUST verify that the params in an epoch-boundary block's
  extension match `compute_expected_parameters(block.height)` byte-for-byte
  via `Parameters.matchParameters`. Mismatch = consensus failure.
- On `BlockVersion >= Interpreter60Version` (mainnet: v4 onward,
  first at h=1,628,160), the receiver MUST additionally verify that
  the block's ID 124 bytes match `active_proposed_update_bytes()`
  byte-for-byte — JVM `matchParameters60`'s `proposedUpdate`
  comparison. Before v4 the comparison short-circuits.
- `active_proposed_update_bytes()` on a fresh chain returns
  `default_proposed_update_bytes(network)` (the launch-default
  `ErgoValidationSettingsUpdate(Seq(215, 409), Seq.empty)` encoding).
  This seed is only consulted before the first boundary; from the
  first boundary onward the field tracks each accepted block's ID
  124 bytes. Note: mainnet on-chain ID 124 at h=1,562,624+ carries 3
  `statusUpdates` that the current encoder does not model, so the
  6-byte seed does not byte-match mainnet's on-chain bytes.
  Non-blocking because the v4-gated validator comparison only starts
  firing at h=1,628,160, by which point the seed has been overwritten
  by every prior boundary.
- After reorg past an epoch-boundary block, the chain MUST roll back
  BOTH `active_parameters` AND `active_proposed_update_bytes` to the
  values at the new tip (recompute or store per-height snapshots).

## Phase 6: NiPoPoW Proofs (build + verify + install)

Build NiPoPoW proofs from the local chain on request, verify proofs received
from peers, and install a verified proof's suffix as the chain's starting
point for light-client mode. Wraps `ergo-nipopow`.

The serve-side (build + verify-only) shipped first; the install path lands
with light-client bootstrap. Both consumers share the same primitives.

JVM reference: `ergo-core/src/main/scala/org/ergoplatform/modifiers/history/popow/NipopowProof.scala`,
`NipopowAlgos.scala`, and `nodeView/history/storage/modifierprocessors/PopowProcessor.scala`
(the `applyPopowProof` flow for the install side).

### `build_nipopow_proof(m: u32, k: u32, header_id: Option<HeaderId>) -> Result<Vec<u8>>`
- **Precondition**: Chain is non-empty and contains at least `m + k`
  headers. `header_id`, if provided, must be in the chain (proof is built
  from the chain ending at that header). If `None`, the proof is built
  from the current tip.
- **Postcondition**: Returns the inner serialized NiPoPoW proof bytes (no P2P
  envelope wrapping). The bytes are exactly what JVM's
  `NipopowProofSerializer.toBytes(proof)` produces — the main crate prepends
  the message envelope when sending.
- **Algorithm**: Use `ergo_nipopow::NipopowAlgos::prove_with_reader` against
  a `PopowHeaderReader` implementation over the local chain. The reader
  walks the interlink hierarchy on demand and fetches only the popow
  headers the algorithm actually needs: genesis, the suffix, and the
  superlevel chains back from the suffix head. Do NOT materialize the
  full chain as `Vec<PoPowHeader>` and hand it to the in-memory
  `NipopowAlgos::prove` — that is the test-helper variant (port of JVM
  `NipopowAlgos.prove(Seq[PoPowHeader])`), not the production variant.
  JVM production serving uses `NipopowProverWithDbAlgs.prove`;
  `prove_with_reader` is the Rust port of that function. Security
  parameters m (min μ-level superchain length) and k (min suffix length,
  ≥ 1) pass through unchanged.
- **Cost**: `O(m + k + m · log₂ N)` popow header fetches per call, where
  N is the chain length. For `m=6, k=10` at `N=270k` this is ≈ 120
  fetches, not 270 000. Cap m + k at sane values. Reject if the chain
  has fewer than `m + k` headers.
- **Determinism**: For a given `(m, k)` and chain state,
  `build_nipopow_proof` produces byte-identical output to JVM
  `NipopowProverWithDbAlgs.prove` on the same chain — `prove_with_reader`
  is a direct port of that function. Note: the in-memory sigma-rust
  `NipopowAlgos::prove(&[PoPowHeader])` variant (a port of the JVM test
  helper `NipopowAlgos.prove(Seq[PoPowHeader])`) can produce a
  different-but-also-valid proof on the same chain — its per-level scan
  visits level-0 blocks at `level = 0` that the interlink walk never
  traverses, because level-0 blocks never appear in interlink vectors.
  sigma-rust's equivalence test between the two variants lives in
  `ergo-chain-generation/src/fake_pow_scheme.rs` and uses a fake PoW
  scheme that forces every block to `max_level ≥ 1` — the same approach
  the JVM's `PoPowAlgosWithDBSpec` takes with `DefaultFakePowScheme`.
  Use `prove_with_reader` for any production path; the in-memory
  `prove` is only appropriate for test scenarios with synthetic chains.
- **Non-scope**: JVM's `continuous = true` mode (which interleaves
  difficulty-recalculation-boundary headers into the prefix so that
  light clients can self-validate difficulty for blocks after the
  suffix) is NOT supported. sigma-rust's `NipopowProof` struct has no
  `continuous` field — adding it requires a separate change to the
  struct, serializer, and on-wire format. `build_nipopow_proof`
  produces non-continuous proofs. JVM peers applying non-continuous
  proofs still succeed (`applyPopowProof` doesn't strictly require the
  flag); they just can't self-validate post-suffix difficulty until
  they sync more headers. This is fine for P2P serve. Tracked as
  follow-up in the roadmap.
- **Genesis (height 1) special case**: The genesis block's interlinks
  vector is canonical and MUST NOT be read from the extension loader. The
  **reader implementation** (not `build_nipopow_proof` directly) is
  responsible for detecting `height == 1` and synthesizing the genesis
  `PoPowHeader` in-process, with:
  - `interlinks = [genesis_block_id]` (per JVM
    `NipopowAlgos.updateInterlinks(genesis, Seq.empty)` and
    `PoPowHeader.checkInterlinksProof` semantics for the genesis row).
  - `interlinks_proof` = the canonical interlinks merkle proof for the
    genesis row, matching the JVM's `NipopowAlgos` output. The reader
    should reuse `ergo_nipopow` helpers (`pack_interlinks` +
    `proof_for_interlink_vector` over a synthetic `ExtensionCandidate`)
    rather than handcraft bytes — the goal is byte-identical equivalence
    with the JVM proof serializer for chains starting at genesis.

  **Rationale**: testnet and mainnet genesis extensions have `fields = []`
  and `extensionHash = 0e5751c0...` (the empty merkle root) — verified via
  JVM `/blocks/{genesis_id}/extension`. Loading and unpacking the empty
  extension produces empty interlinks `[]`, which is wrong by convention
  and produces a malformed proof. The fix belongs in the reader: the
  extension loader is supposed to load real extension bytes, not
  synthesize a special-case payload. The reader's
  `popow_header_at_height(1)` (and `popow_header_by_id(genesis_id)`)
  paths both synthesize; every other height path goes through the
  loader as normal.

  **Verified by**: integration test `tests/nipopow_serve_integration.rs`
  in the main crate, which sends `GetNipopowProof(m=6, k=6)` to a running
  node and verifies the response round-trips through
  `verify_nipopow_proof_bytes`. The chain crate's
  `build_proof_skips_loader_for_genesis` unit test fixtures a chain
  whose loader has no entry for `h=1` and asserts the build still
  succeeds — a black-box check that the reader's genesis synthesis path
  is wired correctly.

### `verify_nipopow_proof_bytes(bytes: &[u8]) -> Result<NipopowVerificationResult>`
- **Precondition**: `bytes` is the inner NiPoPoW proof payload (the main
  crate has already stripped the message envelope).
- **Postcondition**: Returns `NipopowVerificationResult` if the proof is
  structurally valid AND `is_valid` returns true (heights consistent,
  connections valid, PoW valid for each header, difficulty headers present
  in continuous mode). The result includes the full extracted header chain
  (`prefix` + `suffix_head.header` + `suffix_tail`, in height order) so the
  caller can install it via [`HeaderChain::install_from_nipopow_proof`]
  without re-parsing the bytes.
- **Validation checks** (mirrors `NipopowProof.isValid`):
  1. Headers parse cleanly via `ergo_nipopow::NipopowProofSerializer`.
  2. Heights are strictly increasing across `headersChain`.
  3. Each header's PoW passes `verify_pow`.
  4. Parent connections in the chain are consistent
     (`NipopowProof::has_valid_connections`).
  5. (Continuous mode only) Difficulty-recalculation headers are present.
- **Does NOT** apply the proof to local chain state. Returning the headers
  inline is a convenience to avoid double parsing — the chain is mutated
  only via the explicit `install_from_nipopow_proof` call.

### `NipopowVerificationResult`

```rust
pub struct NipopowVerificationResult {
    /// Height of the suffix tip (the highest header in the proof).
    pub suffix_tip_height: u32,
    /// Total number of headers in the proof (prefix + suffix).
    pub total_headers: usize,
    /// Whether the proof is in continuous mode (carries difficulty headers).
    pub continuous: bool,
    /// Headers extracted from the verified proof, in strictly-increasing
    /// height order: `prefix.iter().map(|p| p.header).chain(once(suffix_head.header)).chain(suffix_tail)`.
    /// The light-client install path passes `headers.last()` as `suffix_head`
    /// and the `k - 1` headers preceding it as `suffix_tail`. Callers that
    /// only want metadata (the existing serve-side log path) can ignore the
    /// field at zero parsing cost — it's already materialized.
    pub headers: Vec<Header>,
}
```

Renamed from `NipopowProofMeta` (which only carried metadata) to reflect the
new return shape. The serve-side log path in the main crate is the only
existing call site and just gets the field rename plus an unused-headers
field; no semantic change for that consumer.

### `install_from_nipopow_proof(suffix_head: Header, suffix_tail: Vec<Header>) -> Result<()>`

Install a verified NiPoPoW proof's suffix as the chain's starting point for
light-client mode.

- **Precondition**: Chain is empty (`is_empty() == true`). The headers in
  `suffix_head` + `suffix_tail` MUST already have been validated by the
  caller via `verify_nipopow_proof_bytes`. This function does NOT re-verify
  the proof; it assumes the caller has done so and is installing the
  trusted suffix.
- **Postcondition on Ok**: Chain now contains `suffix_head` followed by every
  header in `suffix_tail`, in order. `tip()` returns the last header in
  `suffix_tail` (or `suffix_head` if `suffix_tail` is empty). `height()`
  returns `suffix_head.height + suffix_tail.len() as u32`. Subsequent
  `try_append` calls extend the tip from there using the normal
  parent-linkage rules.
- **Postcondition on Err**: Chain is unchanged (rolled back). Possible errors:
  - Chain not empty.
  - `suffix_head.parent_id` is anything other than what the caller expects
    — this function does NOT validate `parent_id` against `genesis_parent_id`
    (the suffix head is rarely actually genesis), but it MUST be self-
    consistent with `suffix_tail` (each header's `parent_id` is the previous
    header's `id`).
  - Any header's PoW fails `verify_pow`.
  - Note: the `expected_difficulty` check is NOT performed on suffix
    headers, and is permanently disabled for `try_append` after install
    via the `light_client_mode` flag. See the "Light-client difficulty
    checking" invariant below for the full rationale — light clients
    cannot independently recompute difficulty and must trust the
    `n_bits` values in incoming headers, validated only by self-contained
    PoW verification.
- **Behavior**:
  - Sets `light_client_mode = true` on the chain — this flag persists for
    the chain's lifetime and disables the difficulty-target check on all
    subsequent `try_append` calls (see invariant below).
  - `suffix_head` is pushed via the same internal `push_header` path used
    by `try_append`'s tip-extension branch, but the genesis-validation check
    is bypassed.
  - `scores[0]` (the cumulative-difficulty entry for the installed
    `suffix_head`) is initialized to **`0`**. The absolute score values are
    meaningless once the chain refuses to reorg below the install boundary
    (see "Reorg floor" below) — only deltas matter, and starting from zero
    makes that obvious.
  - For each header in `suffix_tail`, validate parent linkage (`parent_id ==
    previous.id`), validate PoW, and push. Skip the difficulty-target check.
  - `active_parameters` is left at `default_parameters(network)` — light
    clients have no source for voted parameters because they don't download
    block extensions. See "Light-client parameter limitation" below.

### `HeaderChain::reorg_floor() -> u32`

The minimum height at which a fork point can be accepted by reorg logic.

- **Postcondition**:
  - For chains built from genesis (`by_height[0].height == 1`): returns `1`,
    matching the existing "can't reorg genesis" guard.
  - For chains installed from a NiPoPoW proof: returns
    `by_height[0].height` (the suffix head's height). Reorgs that would
    require unwinding past the install boundary are rejected — we don't
    have the headers to roll back to.
- **Used by** `apply_alternative_chain` (and any deep-reorg machinery): any
  fork point at height `< reorg_floor()` MUST be rejected with a clear
  error. In full-node mode this check is a no-op (`reorg_floor() == 1` is
  always satisfied because all valid fork points are at height ≥ 1). In
  light mode it's load-bearing.
- **Implementation note**: this can be derived from `by_height[0].height`
  rather than stored as a separate field — the data structure is already
  base-relative throughout (`header_at`, `headers_from`).

### Light-client parameter limitation

When `install_from_nipopow_proof` is used, `active_parameters` is left at
`default_parameters(network)` and is NOT recomputed from extension storage.

**Why**: light clients do not download block extensions. There is no source
of truth for any parameter values that have been voted on since genesis.
Reading from `extension_loader` would return `None` for every height in
light mode (the loader is not wired in light mode at all).

**Consequences**:
- Storage rent estimates and fee/cost calculations exposed via the API
  reflect network-default parameters, not voted values. For most testnet
  use this is identical (no voted parameters in effect). For mainnet, the
  difference is bounded by what voting can change in 4 years.
- The validator never runs in light mode, so consensus-critical paths are
  unaffected.
- Mining never runs in light mode either.

**Future fix** (out of scope for first release): teach the proof to carry
the latest epoch-boundary extension fields, or fetch them lazily from a
peer on demand. Tracked as a follow-up.

### NiPoPoW invariants

- `build_nipopow_proof` and `verify_nipopow_proof_bytes` are pure functions
  over chain state (modulo `&self` for chain access in `build`).
- Building and verifying do NOT modify chain state.
- `install_from_nipopow_proof` IS a state mutation, but only legal on an
  empty chain. Calling it on a non-empty chain is an error, not a
  destructive overwrite.
- Verification rejects any proof whose internal PoW checks fail —
  consensus-critical.
- Building never produces a proof that would fail verification on the same
  implementation.
- The `PopowHeaderReader` implementation used by `build_nipopow_proof`
  MUST synthesize the genesis `PoPowHeader` in-process — the extension
  loader MUST NOT be called for `height == 1` or for the genesis block
  id. The loader remains the source of truth for `h ≥ 2`. This applies
  to any reader variant (production chain, test fixture, future remote
  reader); it is consensus-critical because real genesis extensions are
  empty and cannot produce the canonical `interlinks = [genesis_id]`
  vector via the loader path.
- **`light_client_mode` flag skips `expected_difficulty`.** `HeaderChain`
  gains an internal `light_client_mode: bool` flag, set to `true` during
  `install_from_nipopow_proof` and `false` otherwise. When true,
  `validate_child` and `validate_child_no_pow` skip the
  `expected_difficulty` check. PoW verification (`verify_pow`) and
  parent-linkage checks remain in force. Standard SPV behavior — light
  clients can't recompute `expected_difficulty` because they don't have
  the historical epoch boundaries the recalc depends on.
- `reorg_floor()` is consulted before any reorg execution. Reorgs whose
  fork point falls below the floor are rejected.

## Does NOT own

- Block bodies, transactions, AD proofs — that's `ergo-validation`.
- Persisting headers to disk — that's `store/`.
- Deciding *when* to request headers — that's `ergo-sync`.
- Network I/O — that's `p2p/`.
- P2P message envelope wrapping for NiPoPoW (codes 90/91) — the inner
  proof bytes are produced/consumed here, but the message envelope is
  the main crate's responsibility (mirrors snapshot sync).
- Validator wiring of `active_parameters` — `validation/` calls into chain.
- Soft-fork voting policy (which params to vote for) — that's the mining
  config in the main crate.

## Dependencies

- `ergo-chain-types` — Header struct, Autolykos PoW, compact nBits encoding
- `ergo-nipopow` — NiPoPoW proof construction and verification
- `ergo-lib` — `chain::parameters::Parameters` for voting state (already a
  validation dependency upstream; pulling it in here unifies the type)
- `sigma-ser` — Scorex deserialization for header bytes

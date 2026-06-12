//! Soft-fork voting state machine for Ergo.
//!
//! Tracks blockchain parameters voted on by miners across epochs. Vote counting,
//! parameter computation, and the soft-fork lifecycle (voting → activation →
//! version bump) are consensus-critical: implementations must agree byte-for-byte
//! on the parameters that the next epoch-boundary block must emit.
//!
//! Mirrors JVM `ergo-core/src/main/scala/org/ergoplatform/settings/Parameters.scala`
//! and `VotingSettings.scala`.

use std::collections::HashMap;

use ergo_lib::chain::parameters::{Parameter, Parameters};

/// Vote slot value reserved for soft-fork ballots.
///
/// Mirrors JVM `Parameters.SoftFork`. Header `votes` byte == 120 means
/// "vote for the current soft-fork." Distinct from ordinary parameter ID
/// votes (1-8) which appear as the same byte in their dedicated slot.
pub const SOFT_FORK_VOTE: i8 = 120;

/// Param ID 9: number of sub-blocks per block, on average. Lives on
/// [`Parameters`] as [`Parameter::SubblocksPerBlock`]. Introduced by the
/// 6.0 soft-fork (block version 4); auto-inserted whenever the table
/// reaches `BlockVersion == 4`.
pub const ID_SUBBLOCKS_PER_BLOCK: i8 = 9;

/// Reserved soft-fork state param IDs. Numeric values mirror JVM
/// `Parameters.scala`. Stored directly in `parameters_table` via
/// `Parameter::SoftForkVotesCollected` / `Parameter::SoftForkStartingHeight`
/// (added in the parallel sigma-rust patch).
pub const ID_SOFT_FORK_VOTES_COLLECTED: i8 = 121;
pub const ID_SOFT_FORK_STARTING_HEIGHT: i8 = 122;

/// Param ID 123: current block version. Lives on [`Parameters`] as
/// [`Parameter::BlockVersion`].
pub const ID_BLOCK_VERSION: i8 = 123;

/// Param ID 124: variable-length soft-fork validation rules.
///
/// Deferred for first release — testnet won't be voting on validation rule
/// changes. Encoding is `ErgoValidationSettingsUpdate` bytes, not 4-byte BE
/// i32 like the other IDs.
pub const ID_SOFT_FORK_DISABLING_RULES: i8 = 124;

/// One key-value field inside an extension section.
///
/// Wire layout: `key: [u8; 2]` (`SUBID || ID`) followed by `value: Vec<u8>`
/// (max 64 bytes per JVM `ExtensionSerializer`). Used as the unit of both
/// the parsed and packed forms of extension-section payloads.
pub type ExtensionField = ([u8; 2], Vec<u8>);

/// Voting epoch lengths and soft-fork timing parameters.
///
/// Derived from network type (testnet vs mainnet) — not a runtime config
/// entry. Pulled into [`crate::ChainConfig`] which selects the right preset.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct VotingConfig {
    /// Length of one voting epoch, in blocks. Mainnet 1024, testnet 128.
    pub voting_length: u32,
    /// Voting epochs collected before a soft-fork can be approved. Both nets: 32.
    pub soft_fork_epochs: u32,
    /// Voting epochs after approval before BlockVersion is incremented. Both nets: 32.
    pub activation_epochs: u32,
    /// JVM-hardcoded protocol-v2 forced activation height. Mainnet 417792.
    /// Testnet has no forced activation; set to 0 (the check is `height == this`).
    pub version2_activation_height: u32,
}

impl VotingConfig {
    /// Mainnet voting parameters. Mirrors JVM `MainnetVotingSettings`.
    pub fn mainnet() -> Self {
        Self {
            voting_length: 1024,
            soft_fork_epochs: 32,
            activation_epochs: 32,
            version2_activation_height: 417_792,
        }
    }

    /// Testnet voting parameters. Mirrors JVM `TestnetVotingSettings`.
    pub fn testnet() -> Self {
        Self {
            voting_length: 128,
            soft_fork_epochs: 32,
            activation_epochs: 32,
            version2_activation_height: 0,
        }
    }

    /// `true` iff the given vote count meets the soft-fork supermajority.
    ///
    /// JVM `softForkApproved`: `votes > votingLength * softForkEpochs * 9 / 10`
    /// (90% supermajority across all soft-fork voting epochs). The JVM
    /// operand is a signed Int — the running 121 total is stored with
    /// wrapping arithmetic, and a wrapped-negative total is never approved
    /// under the signed compare. Takes `i64` so every i32 table value
    /// widens losslessly. Threshold math stays in u64 — JVM Int-overflow
    /// on hostile SETTINGS is out of scope (vectors hand sane settings).
    pub fn soft_fork_approved(&self, votes: i64) -> bool {
        let threshold =
            (self.voting_length as u64) * (self.soft_fork_epochs as u64) * 9 / 10;
        votes > threshold as i64
    }

    /// `true` iff a vote count meets the ordinary parameter change threshold.
    ///
    /// JVM `changeApproved`: strict majority of voting epoch length:
    /// `votes > votingLength / 2`.
    pub fn change_approved(&self, votes: u32) -> bool {
        (votes as u64) > (self.voting_length as u64) / 2
    }
}

/// Hardcoded step for a parameter, if one exists.
///
/// Mirrors JVM `Parameters.stepsTable`. Only 3 params have hardcoded steps;
/// the rest use a dynamic default of `max(1, current_value / 100)`.
/// Returns `None` for params that use the dynamic formula.
pub fn parameter_step(id: i8) -> Option<i32> {
    match id.unsigned_abs() as i8 {
        1 => Some(25_000), // StorageFeeFactor
        2 => Some(10),     // MinValuePerByte
        9 => Some(1),      // SubblocksPerBlock
        _ => None,
    }
}

/// Lower bound for an ordinary parameter (mirrors JVM `minValues`).
pub fn parameter_min(id: i8) -> Option<i32> {
    match id.unsigned_abs() as i8 {
        1 => Some(0),
        2 => Some(0),
        3 => Some(16 * 1024),
        4 => Some(16 * 1024),
        5 => Some(0),
        6 => Some(0),
        7 => Some(0),
        8 => Some(0),
        9 => Some(2),
        _ => None,
    }
}

/// Upper bound for an ordinary parameter (mirrors JVM `maxValues`).
pub fn parameter_max(id: i8) -> Option<i32> {
    match id.unsigned_abs() as i8 {
        1 => Some(2_500_000),     // StorageFeeFactor
        2 => Some(10_000),        // MinValuePerByte
        9 => Some(2_048),         // SubblocksPerBlock
        3..=8 => Some(i32::MAX / 2),
        _ => None,
    }
}

/// Map an ordinary parameter ID (1-9) to its [`Parameter`] enum variant.
fn ordinary_param(id: i8) -> Option<Parameter> {
    match id.unsigned_abs() as i8 {
        1 => Some(Parameter::StorageFeeFactor),
        2 => Some(Parameter::MinValuePerByte),
        3 => Some(Parameter::MaxBlockSize),
        4 => Some(Parameter::MaxBlockCost),
        5 => Some(Parameter::TokenAccessCost),
        6 => Some(Parameter::InputCost),
        7 => Some(Parameter::DataInputCost),
        8 => Some(Parameter::OutputCost),
        9 => Some(Parameter::SubblocksPerBlock),
        _ => None,
    }
}

/// Default starting [`Parameters`] used when chain is shorter than one
/// voting epoch (no boundary block to read from).
///
/// `ergo_lib::chain::parameters::Parameters::default()` is buggy in the
/// pinned revision — it does NOT insert `MaxBlockCost`, so any code that
/// reads `params.max_block_cost()` panics. We construct a complete default
/// here, mirroring JVM `Parameters.scala` startup defaults.
///
/// Network-aware: testnet starts at protocol v4 (testnet.conf was created
/// post-6.0); mainnet started at v1 and progressed via voting. The
/// `BlockVersion` default differs accordingly.
///
/// Testnet additionally carries `Parameter::SubblocksPerBlock = 30` because
/// JVM `Parameters.scala::update` auto-inserts it whenever `BlockVersion == 4`.
/// Mainnet starts at v1 so it does NOT include the entry — it would be
/// auto-inserted by [`crate::HeaderChain::compute_expected_parameters`] if
/// mainnet ever activates protocol v4 via voting.
pub fn default_parameters(network: crate::Network) -> Parameters {
    let block_version = match network {
        crate::Network::Mainnet => 1,
        crate::Network::Testnet => 4,
    };
    let mut params = Parameters::new(
        block_version,
        1_250_000, // StorageFeeFactor
        360,       // MinValuePerByte (30 * 12)
        524_288,   // MaxBlockSize (512 KiB)
        1_000_000, // MaxBlockCost
        100,       // TokenAccessCost
        2_000,     // InputCost
        100,       // DataInputCost
        100,       // OutputCost
    );
    if matches!(network, crate::Network::Testnet) {
        params
            .parameters_table
            .insert(Parameter::SubblocksPerBlock, SUBBLOCKS_PER_BLOCK_DEFAULT);
    }
    params
}

/// Default `SubblocksPerBlock` value (mirrors JVM
/// `Parameters.SubblocksPerBlockDefault`). Auto-inserted whenever a
/// `Parameters` table is constructed at protocol version 4 or higher.
pub(crate) const SUBBLOCKS_PER_BLOCK_DEFAULT: i32 = 30;

/// Parse parameters from a sequence of extension key-value pairs.
///
/// Mirrors JVM `Parameters.parseExtension`. Walks `kv`, picks fields with
/// 2-byte key prefix `0x00` (parameter table prefix), parses the second key
/// byte as the signed parameter ID, and decodes the 4-byte BE i32 value.
///
/// **ID 124** (`SoftForkDisablingRules`) is deferred for first release —
/// testnet doesn't vote on validation rules. The parser SKIPS ID 124 entries
/// silently. The integration with `ErgoValidationSettingsUpdate` will be
/// added in a follow-up when sigma-rust exposes its serializer.
///
/// Returns a map keyed by signed param ID. The map will include ordinary
/// IDs 1-8 plus reserved IDs 121, 122, 123 when present.
pub fn parse_parameters_from_kv(
    kv: &[ExtensionField],
) -> Result<HashMap<i8, i32>, crate::ChainError> {
    let mut out = HashMap::new();
    for (key, value) in kv {
        if key[0] != 0x00 {
            continue; // Not a parameter field.
        }
        let id = key[1] as i8;
        if id == ID_SOFT_FORK_DISABLING_RULES {
            // Variable-length encoding via ErgoValidationSettingsUpdate.
            // Deferred — see function docs.
            continue;
        }
        if value.len() != 4 {
            return Err(crate::ChainError::ExtensionParse(format!(
                "param id {id} has wrong value length: expected 4, got {}",
                value.len()
            )));
        }
        let mut buf = [0u8; 4];
        buf.copy_from_slice(value);
        out.insert(id, i32::from_be_bytes(buf));
    }
    Ok(out)
}

/// Extract the `SoftForkDisablingRules` (ID 124) raw bytes from extension
/// key-value pairs, if present.
///
/// JVM stores ID 124 as a variable-length `ErgoValidationSettingsUpdate`
/// encoding on `Parameters.proposedUpdate`, separate from `parametersTable`.
/// We don't decode the structure here — we just preserve the bytes for
/// the validator's byte-for-byte epoch-boundary comparison and for
/// passing through to the next epoch's expected output.
///
/// Returns an empty `Vec` if no ID 124 entry is present.
pub fn extract_disabling_rules_from_kv(kv: &[ExtensionField]) -> Vec<u8> {
    for (key, value) in kv {
        if key[0] == 0x00 && (key[1] as i8) == ID_SOFT_FORK_DISABLING_RULES {
            return value.clone();
        }
    }
    Vec::new()
}

/// Parse just the `rulesToDisable` list from a raw
/// `ErgoValidationSettingsUpdate` payload (extension key `[0x00, 124]`).
///
/// Mirrors JVM `ErgoValidationSettingsUpdateSerializer.parse`
/// (`ErgoValidationSettingsUpdate.scala`) but stops after the
/// `disabledRulesNum` + rule IDs section — we don't need `statusUpdates`
/// for the consensus checks that use this. Encoding:
///
/// ```text
/// [disabledRulesNum: VLQ u32]
/// disabledRules × disabledRulesNum: [rule_id: VLQ u16]
/// (remainder: statusUpdates — ignored here)
/// ```
///
/// Empty input → empty `Vec` (mirrors JVM's
/// `ErgoValidationSettingsUpdate.empty`, the default when extension key
/// `[0x00, 124]` is absent from the block).
///
/// This is the LENIENT reader, used for the rule-409 auto-insert gate on
/// already-normalized bytes. The validation entry point for raw block
/// bytes is [`parse_validation_settings_update`], which additionally
/// enforces the disableability `require` and the statusUpdates-count
/// read.
pub fn parse_disabled_rules(bytes: &[u8]) -> Result<Vec<u16>, crate::ChainError> {
    use crate::ChainError;
    use sigma_ser::vlq_encode::ReadSigmaVlqExt;

    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let count = cursor
        .get_u32()
        .map_err(|e| ChainError::ExtensionParse(format!("disabled rules count: {e}")))?
        as usize;
    // Cap capacity so a bogus `count` on a small payload can't force a large
    // pre-allocation. The subsequent `get_u16` calls will bail naturally if
    // the byte stream is truncated.
    let cap = count.min(bytes.len());
    let mut rules = Vec::with_capacity(cap);
    for i in 0..count {
        let rule = cursor.get_u16().map_err(|e| {
            ChainError::ExtensionParse(format!("disabled rule id at index {i}: {e}"))
        })?;
        rules.push(rule);
    }
    Ok(rules)
}

/// `true` iff `id` is a KNOWN ergo validation rule that may NOT be
/// disabled via soft-fork.
///
/// Transcribed from JVM `ValidationRules.rulesSpec`
/// (`ValidationRules.scala:22-231`, v6.0.3): the ids below are exactly
/// the spec entries with `mayBeDisabled = false`. Ids absent from the
/// spec — the gaps 110/202, 414 (`exMatchParameters60`, which has a
/// constant but no spec entry), sigma-side ids ≥ 1000, and everything
/// else — return `false` here and pass the strict parse
/// (disableable-by-omission, JVM `rulesSpec.get(rd).forall(_.mayBeDisabled)`).
fn rule_may_not_be_disabled(id: u16) -> bool {
    matches!(
        id,
        // transaction rules: txDust(111), txBoxToSpend(118), txBoxSize(120),
        // txBoxPropositionSize(121), txReemission(123), txMonotonicHeight(124)
        // are disableable; the rest of 100-124 are mandatory.
        100..=109 | 112..=117 | 119 | 122
        // header rules: hdrVotesNumber(212) and hdrVotesUnknown(215) are
        // disableable; 202 is a gap.
        | 200 | 201 | 203..=211 | 213 | 214 | 216
        // block-section rules: bsBlockTransactionsSize(306) is disableable.
        | 300..=305 | 307
        // extension rules: all disableable except exKeyLength(403).
        | 403
        // full-block application rules.
        | 500 | 501
    )
}

/// Strict deserialization of an `ErgoValidationSettingsUpdate` payload —
/// port of `ErgoValidationSettingsUpdateSerializer.parse`
/// (`ErgoValidationSettingsUpdate.scala:41-58`) including the
/// disableability `require` (lines 47-50). This is the validation entry
/// point for extension key `[0x00, 124]` bytes; [`parse_disabled_rules`]
/// remains the lenient rules-section reader used on already-normalized
/// bytes.
///
/// Layout:
///
/// ```text
/// [disabledRulesNum: VLQ u32]
/// disabledRules × disabledRulesNum: [rule_id: VLQ u16]
/// [statusUpdatesNum: VLQ u32]
/// (statusUpdate entries — NOT validated, see below)
/// ```
///
/// - Empty input = empty update (absent-field convention), no error.
/// - Every id in `rulesToDisable` must satisfy JVM
///   `rulesSpec.get(id).forall(_.mayBeDisabled)`: a KNOWN rule that is
///   not disableable errors; ids absent from the spec pass through (see
///   the private `rule_may_not_be_disabled` table). The check runs after
///   the full id list is read and before `statusUpdatesNum` — JVM error
///   precedence.
/// - The `statusUpdates` COUNT must parse: a payload truncated before it
///   (e.g. the bare 1-byte `0x00`) errors — JVM `getUInt` underflow
///   parity. Entries after a `count > 0` pass UNVALIDATED (lenient
///   tail): mainnet's real h=1,628,160 payload carries 3 status updates
///   and must keep flowing verbatim through the live wrappers. Known
///   gap (flagged to SANTA): JVM fully decodes each entry via the
///   sigma-side `RuleStatusSerializer`, which has no Rust port — a
///   payload with malformed status entries rejects on JVM and passes
///   here. Do not guess the entry wire format; closing this needs the
///   sigma port.
/// - Counts mirror JVM `getUInt().toInt`: values ≥ 2^31 wrap negative
///   and `0 until n` reads ZERO entries — a count of `0xFFFFFFFF`
///   parses as an empty rules list, not a 4-billion-entry read.
pub fn parse_validation_settings_update(
    bytes: &[u8],
) -> Result<Vec<u16>, crate::ChainError> {
    use crate::ChainError;
    use sigma_ser::vlq_encode::ReadSigmaVlqExt;

    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let raw_count = cursor.get_u32().map_err(|e| {
        ChainError::ExtensionParse(format!("settings update: disabled rules count: {e}"))
    })?;
    // JVM `getUInt().toInt` wraps ≥ 2^31 negative; `(0 until n)` on a
    // negative n is empty.
    let count = if raw_count > i32::MAX as u32 {
        0
    } else {
        raw_count as usize
    };
    let cap = count.min(bytes.len());
    let mut rules = Vec::with_capacity(cap);
    for i in 0..count {
        let rule = cursor.get_u16().map_err(|e| {
            ChainError::ExtensionParse(format!(
                "settings update: disabled rule id at index {i}: {e}"
            ))
        })?;
        rules.push(rule);
    }
    for &rule in &rules {
        if rule_may_not_be_disabled(rule) {
            return Err(ChainError::ExtensionParse(format!(
                "settings update: trying to deactivate rule {rule}, that may not be disabled"
            )));
        }
    }
    cursor.get_u32().map_err(|e| {
        ChainError::ExtensionParse(format!("settings update: status updates count: {e}"))
    })?;
    Ok(rules)
}

/// Encode a list of disabled rule IDs as an `ErgoValidationSettingsUpdate`
/// payload with empty `statusUpdates`.
///
/// Mirrors JVM `ErgoValidationSettingsUpdateSerializer.serialize`
/// (`ErgoValidationSettingsUpdate.scala`) for the narrow case of
/// rules-only updates. Encoding:
///
/// ```text
/// [disabledRulesNum: VLQ u32]
/// disabledRules × disabledRulesNum: [rule_id: VLQ u16]
/// [statusUpdatesNum: VLQ u32 = 0]
/// ```
///
/// We do NOT encode status updates here — mainnet / testnet launch
/// parameters don't carry any, and the chain has no path to introduce
/// them yet. When a consensus rule change requires status updates the
/// encoder + storage model will need to be extended.
///
/// This is the inverse of [`parse_disabled_rules`] for the
/// empty-statusUpdates case; round-trip is stable for any `[u16]` list.
pub fn encode_disabled_rules(rules: &[u16]) -> Vec<u8> {
    use sigma_ser::vlq_encode::WriteSigmaVlqExt;

    let mut out = Vec::with_capacity(1 + rules.len() * 2 + 1);
    out.put_u32(rules.len() as u32).expect("Vec write");
    for &r in rules {
        out.put_u16(r).expect("Vec write");
    }
    out.put_u32(0).expect("Vec write"); // statusUpdatesNum = 0
    out
}

/// Default encoded `ErgoValidationSettingsUpdate` (ID 124) bytes for a
/// fresh chain on the given network.
///
/// Mirrors JVM `LaunchParameters.proposedUpdate` —
/// `ErgoValidationSettingsUpdate(rulesToDisable = Seq(215, 409),
/// statusUpdates = Seq.empty)` — for both mainnet and testnet.
///
/// Used to seed [`crate::HeaderChain::active_proposed_update_bytes`] at
/// [`crate::HeaderChain::new`] so the field carries a meaningful value
/// before any epoch-boundary block has been applied.
///
/// **Consensus note**: this seed is only consulted before the first
/// epoch-boundary block is applied. From the first boundary onward,
/// [`crate::HeaderChain::apply_epoch_boundary_parameters`] advances the
/// field to the exact ID 124 bytes of each accepted boundary block, so
/// the chain tracks on-chain state byte-for-byte. The main-session
/// validator's byte-for-byte comparison (JVM
/// `Parameters.matchParameters60`) short-circuits for `BlockVersion <
/// Interpreter60Version`, so the first comparison only runs at v4+ —
/// mainnet h=1,628,160 — by which point every boundary has been
/// processed and the seed is no longer in effect.
///
/// Forward-compat: when a vote legitimately introduces
/// `statusUpdates`, this helper (and [`encode_disabled_rules`]) must
/// be extended to emit them. Tracked as future work.
pub fn default_proposed_update_bytes(network: crate::Network) -> Vec<u8> {
    // Mainnet and testnet both use `LaunchParameters.proposedUpdate =
    // ErgoValidationSettingsUpdate(Seq(215, 409), Seq.empty)` — see JVM
    // `LaunchParameters.scala` and the testnet launch equivalent.
    let _ = network;
    encode_disabled_rules(&[215, 409])
}

/// Pack a parameter table into extension key-value pairs.
///
/// Inverse of [`parse_parameters_from_kv`]. Mirrors JVM
/// `Parameters.toExtensionCandidate` for the parameter portion. Output
/// fields all have 2-byte keys `[0x00, param_id_as_byte]` and 4-byte BE
/// i32 values.
///
/// Output ordering is sorted by param ID for determinism. Skips ID 124
/// (deferred — see [`parse_parameters_from_kv`]).
pub fn pack_parameters_to_kv(params: &HashMap<i8, i32>) -> Vec<ExtensionField> {
    let mut entries: Vec<(i8, i32)> = params
        .iter()
        .filter(|(&id, _)| id != ID_SOFT_FORK_DISABLING_RULES)
        .map(|(&id, &v)| (id, v))
        .collect();
    entries.sort_by_key(|(id, _)| *id);
    entries
        .into_iter()
        .map(|(id, v)| ([0x00u8, id as u8], v.to_be_bytes().to_vec()))
        .collect()
}

/// Parse a JVM-format Extension modifier body into key-value fields.
///
/// **Wire format** (mirror of JVM `ExtensionSerializer.parseBody`,
/// verified against testnet for ~270k blocks via the in-repo
/// `validation/src/sections.rs::parse_extension`):
///
/// ```text
/// [header_id: 32 bytes]
/// [field_count: VLQ u32]
/// fields × field_count: {
///     [key: 2 bytes]
///     [val_len: 1 byte, max 64]
///     [value: val_len bytes]
/// }
/// ```
///
/// Returns the parent header ID followed by the parsed fields. Caller
/// should validate the header ID matches the expected modifier link.
pub fn parse_extension_bytes(
    bytes: &[u8],
) -> Result<(ergo_chain_types::BlockId, Vec<ExtensionField>), crate::ChainError> {
    use crate::ChainError;
    use ergo_chain_types::{BlockId, Digest32};
    use sigma_ser::vlq_encode::ReadSigmaVlqExt;

    if bytes.len() < 32 {
        return Err(ChainError::ExtensionParse(format!(
            "extension body too short: {} bytes, need at least 32 for header_id",
            bytes.len()
        )));
    }
    let mut header_id_bytes = [0u8; 32];
    header_id_bytes.copy_from_slice(&bytes[..32]);
    let header_id = BlockId(Digest32::from(header_id_bytes));

    let mut cursor = std::io::Cursor::new(&bytes[32..]);
    let field_count = cursor
        .get_u32()
        .map_err(|e| ChainError::ExtensionParse(format!("VLQ field_count: {e}")))?
        as usize;

    let mut pos = 32 + cursor.position() as usize;
    let mut fields = Vec::with_capacity(field_count);
    for i in 0..field_count {
        if pos + 3 > bytes.len() {
            return Err(ChainError::ExtensionParse(format!(
                "truncated field header for field {i} at offset {pos}"
            )));
        }
        let key = [bytes[pos], bytes[pos + 1]];
        let value_len = bytes[pos + 2] as usize;
        pos += 3;
        if value_len > 64 {
            return Err(ChainError::ExtensionParse(format!(
                "field {i} value length {value_len} exceeds max 64"
            )));
        }
        if pos + value_len > bytes.len() {
            return Err(ChainError::ExtensionParse(format!(
                "truncated field {i} value at offset {pos}: need {value_len} bytes"
            )));
        }
        let value = bytes[pos..pos + value_len].to_vec();
        pos += value_len;
        fields.push((key, value));
    }

    Ok((header_id, fields))
}

/// Pack a parent header ID and field set into extension wire bytes.
///
/// Inverse of [`parse_extension_bytes`]. Mirrors the JVM
/// `ExtensionSerializer` body format (header_id + VLQ count + fields).
pub fn pack_extension_bytes(
    header_id: &ergo_chain_types::BlockId,
    fields: &[ExtensionField],
) -> Vec<u8> {
    use sigma_ser::vlq_encode::WriteSigmaVlqExt;

    let mut out = Vec::with_capacity(
        32 + 5 + fields.iter().map(|(_, v)| v.len() + 3).sum::<usize>(),
    );
    out.extend_from_slice(&header_id.0 .0);
    // VLQ-encoded field count
    out.put_u32(fields.len() as u32).expect("Vec write");
    for (key, value) in fields {
        out.extend_from_slice(key);
        out.push(value.len() as u8);
        out.extend_from_slice(value);
    }
    out
}

/// Seeded vote tally over one closing voting epoch — JVM `VotingData` parity.
///
/// `window` is the `(height, votes)` stream for the epoch closing at
/// `boundary_height` (`T`): exactly the headers in `[max(1, T −
/// voting_length), T − 1]`, ascending. The window's FIRST header — the
/// previous boundary — **seeds** the tally: each of its non-zero vote ids
/// enters with count 1 (the seed header's own vote counts). Every
/// subsequent window header increments **only already-seeded ids**; votes
/// for unseeded ids count for nothing (JVM `VotingData.update`,
/// `VotingData.scala:9-13`; the seed is `ErgoStateContext.scala:250-251`
/// `VotingData(proposedVotes)` with `proposedVotes = votes.map(_ -> 1)`).
///
/// The tally is an ORDERED sequence, not a map — JVM
/// `VotingData.epochVotes` is `Array[(Byte, Int)]` seeded in the boundary
/// header's vote-SLOT order (zero slots filtered, order preserved,
/// duplicates NOT deduped — `ErgoStateContext.scala:238,250`).
/// `updateParams` folds over it in sequence order, so a seed carrying a
/// contradictory pair (+id and −id, both later approved) produces a
/// last-write-wins result that depends on slot order. If the seed carries
/// a duplicated id, each copy is a separate entry and every window vote
/// for that id increments ALL matching entries (JVM `VotingData.update`
/// maps over the whole array). Unreachable on-chain —
/// `hdrVotesContradictory`/`hdrVotesDuplicates` reject such a seed header
/// — but this seam is graded over HANDED streams; legality is upstream.
///
/// If the window's first header is NOT the previous boundary — the
/// chain-start clamp, `T − voting_length < 1` with genesis at height 1 —
/// the seed is empty and the tally is empty: every vote in the window
/// drops (JVM starts from `VotingData.empty` and `update` can never add
/// ids).
///
/// The boundary header `T` itself is NOT part of the window — its votes
/// derive `forkVote` and seed the *next* epoch.
///
/// **Pure**: settings arrive as arguments, never from network presets or
/// chain state. This is a SANTA chain-tier graded seam.
pub fn tally_votes_seeded(
    window: &[(u32, [u8; 3])],
    boundary_height: u32,
    voting_length: u32,
) -> Vec<(i8, u32)> {
    let Some(((seed_height, seed_votes), rest)) = window.split_first() else {
        return Vec::new();
    };
    // Seed iff the window head is the previous boundary. At chain start
    // (T − L < 1) no previous boundary exists and nothing seeds.
    if boundary_height.checked_sub(voting_length) != Some(*seed_height) {
        return Vec::new();
    }

    let mut tally: Vec<(i8, u32)> = seed_votes
        .iter()
        .filter(|&&slot| slot != 0)
        .map(|&slot| (slot as i8, 1u32))
        .collect();
    for (_, votes) in rest {
        for &slot in votes {
            if slot != 0 {
                for entry in tally.iter_mut() {
                    if entry.0 == slot as i8 {
                        entry.1 += 1;
                    }
                }
            }
        }
    }
    tally
}

/// Compute the parameters an epoch-boundary block MUST emit, plus the
/// activated validation-settings update — JVM `Parameters.update` parity.
///
/// Pure: every input arrives as an argument; nothing is read from network
/// presets or chain state. This is a SANTA chain-tier graded seam; the
/// chain-entangled [`crate::HeaderChain::compute_expected_parameters`] is
/// tally + delegate onto this function (one implementation).
///
/// Inputs:
/// - `voting`: epoch lengths and soft-fork thresholds.
/// - `boundary_height`: the epoch-boundary height `T`
///   (`T % voting_length == 0`, `T > 0`).
/// - `current`: the parameters in force across the closing epoch (the
///   previous boundary's table).
/// - `tally`: the closing epoch's seeded vote tally (see
///   [`tally_votes_seeded`]).
/// - `boundary_fork_vote`: whether the boundary header `T`'s OWN votes
///   contain id 120 (JVM `forkVote`, `ErgoStateContext.scala:240`). The
///   boundary header is excluded from the closing tally; its fork vote
///   gates only the start of a NEW voting round.
/// - `proposed_update`: the raw `ErgoValidationSettingsUpdate` payload of
///   the boundary block's own extension key `[0x00, 124]`; empty slice if
///   absent.
///
/// Returns `(next_parameters, activated_update)` where `activated_update`
/// is `proposed_update` verbatim at a voting-driven activation boundary
/// and the canonical EMPTY encoding (`0x0000`, via
/// [`encode_disabled_rules`] on no rules) otherwise — JVM
/// `activatedUpdate`. The forced-v2 bump does NOT activate the update.
///
/// `proposed_update` is parse-validated UP FRONT via
/// [`parse_validation_settings_update`] and a failure errors (the SANTA
/// reject arm): the JVM object handed to `Parameters.update` cannot exist
/// unless these bytes deserialize. Live callers pre-swallow failures to
/// the canonical EMPTY encoding instead (JVM `Parameters.parseExtension`
/// parity — see `HeaderChain::compute_expected_parameters`).
///
/// Port of JVM `Parameters.update` / `updateFork` / `updateParams`
/// (`Parameters.scala:82-183`), including the sequential-`if` lifecycle
/// structure whose conditions all read the PRE-update table while
/// mutations accumulate in the running table.
pub fn compute_boundary_parameters(
    voting: &VotingConfig,
    boundary_height: u32,
    current: &Parameters,
    tally: &[(i8, u32)],
    boundary_fork_vote: bool,
    proposed_update: &[u8],
) -> Result<(Parameters, Vec<u8>), crate::ChainError> {
    use crate::ChainError;

    parse_validation_settings_update(proposed_update)?;

    if voting.voting_length == 0 {
        return Err(ChainError::Voting("voting_length must be > 0".into()));
    }

    let mut next = current.clone();
    let table = &mut next.parameters_table;
    // None = no voting-driven activation this boundary (JVM
    // `ErgoValidationSettingsUpdate.empty`).
    let mut activated: Option<Vec<u8>> = None;

    // --- JVM `updateFork` ---
    //
    // All branch conditions read the PRE-update state (JVM lazy vals over
    // the receiver's table); only the running `table` mutates. Branches
    // are sequential `if`s, not alternatives: cleanup and an immediate
    // restart can fire at the same boundary. Height math in i64 — the
    // starting height is an i32 table value and JVM does plain Int
    // arithmetic; i64 keeps every reachable case exact without overflow.
    let h = boundary_height as i64;
    let l = voting.voting_length as i64;
    let ve = voting.soft_fork_epochs as i64;
    let ae = voting.activation_epochs as i64;

    let starting_height = current
        .parameters_table
        .get(&Parameter::SoftForkStartingHeight)
        .copied();
    if let Some(start) = starting_height {
        let start = start as i64;
        // JVM `votesInPrevEpoch`: the FIRST id-120 entry of the ordered
        // tally (`epochVotes.find(_._1 == SoftFork)`) — find, not sum.
        let votes_in_prev_epoch = tally
            .iter()
            .find(|(id, _)| *id == SOFT_FORK_VOTE)
            .map(|(_, count)| *count)
            .unwrap_or(0);
        // JVM `lazy val votes = votesInPrevEpoch + parametersTable(
        // SoftForkVotesCollected)` (Parameters.scala:108): Int wrapping
        // add, and `Map.apply` throws iff the lazy val is FORCED with 121
        // absent. The five force sites are marked below; a 122-but-no-121
        // table at any OTHER boundary passes through without error. Do
        // NOT evaluate eagerly.
        let collected = current
            .parameters_table
            .get(&Parameter::SoftForkVotesCollected)
            .copied();
        let force_votes = || -> Result<i32, ChainError> {
            collected
                .map(|c| (votes_in_prev_epoch as i32).wrapping_add(c))
                .ok_or_else(|| {
                    ChainError::Voting(format!(
                        "SoftForkVotesCollected (121) absent from parameters table at \
                         boundary {boundary_height} (JVM lazy `votes` force)"
                    ))
                })
        };
        // Approval compares the (possibly wrapped-negative) i32 total via
        // signed widening — a negative total is never approved.
        let approved = |votes: i32| voting.soft_fork_approved(votes as i64);

        let mid_end = start + l * ve;
        let activation = start + l * (ve + ae);
        let cleanup_fail = start + l * (ve + 1);
        let cleanup_success = start + l * (ve + ae + 1);

        // Successful voting — cleanup after activation. (`votes` force
        // site: the approval operand evaluates only after the height
        // conjunct matches.)
        if h == cleanup_success && approved(force_votes()?) {
            table.remove(&Parameter::SoftForkStartingHeight);
            table.remove(&Parameter::SoftForkVotesCollected);
        }
        // Unsuccessful voting — cleanup. (`votes` force site.)
        if h == cleanup_fail && !approved(force_votes()?) {
            table.remove(&Parameter::SoftForkStartingHeight);
            table.remove(&Parameter::SoftForkVotesCollected);
        }
        // New voting starting over a just-cleaned round (the boundary
        // header itself votes for a fork). The first disjunct has NO
        // approval check — a fork vote exactly at the late-cleanup height
        // restarts the round even when the dying round was never approved
        // (the one legal zombie revival). The second disjunct is a `votes`
        // force site, short-circuited away at the cleanup-success height.
        if boundary_fork_vote
            && (h == cleanup_success || (h == cleanup_fail && !approved(force_votes()?)))
        {
            table.insert(Parameter::SoftForkStartingHeight, boundary_height as i32);
            table.insert(Parameter::SoftForkVotesCollected, 0);
        }
        // New epoch in voting — store the running total verbatim, wrapped
        // i32 like the JVM Int. (`votes` force site: the BODY forces.)
        if h <= mid_end {
            table.insert(Parameter::SoftForkVotesCollected, force_votes()?);
        }
        // Successful voting — activation: bump BlockVersion, activate the
        // proposed update. (`votes` force site.)
        if h == activation && approved(force_votes()?) {
            let bv = table
                .get(&Parameter::BlockVersion)
                .copied()
                .ok_or_else(|| {
                    ChainError::Voting(
                        "BlockVersion missing from parameters table at soft-fork activation"
                            .into(),
                    )
                })?;
            table.insert(Parameter::BlockVersion, bv.wrapping_add(1));
            activated = Some(proposed_update.to_vec());
        }
    } else if boundary_fork_vote && boundary_height.is_multiple_of(voting.voting_length) {
        // New voting with no round in progress.
        table.insert(Parameter::SoftForkStartingHeight, boundary_height as i32);
        table.insert(Parameter::SoftForkVotesCollected, 0);
    }

    // Forced version update to v2 — the non-voted mainnet hard fork.
    // Reads the RUNNING table like JVM (a voting-driven bump at the same
    // height suppresses the force).
    if boundary_height == voting.version2_activation_height {
        let bv = table.get(&Parameter::BlockVersion).copied().ok_or_else(|| {
            ChainError::Voting(
                "BlockVersion missing from parameters table at forced-v2 height".into(),
            )
        })?;
        if bv == 1 {
            table.insert(Parameter::BlockVersion, 2);
        }
    }

    // --- JVM `updateParams` ---
    //
    // Ordinary parameter steps. JVM folds over the epoch votes filtered to
    // ids `< SoftFork` (negatives included) in SEQUENCE order, reading
    // each current value from the post-fork table SNAPSHOT (the
    // `parametersTable` argument, not the fold accumulator) and writing
    // into the running table — a contradictory pair (+id and −id, both
    // approved) is last-write-wins by tally order. An approved vote for
    // an id with no table entry throws in JVM (invalid block) — mirrored
    // here as an error.
    let base = table.clone();
    for &(signed_id, count) in tally {
        if signed_id >= SOFT_FORK_VOTE {
            continue;
        }
        if !voting.change_approved(count) {
            continue;
        }
        let param = ordinary_param(signed_id).ok_or_else(|| {
            ChainError::Voting(format!(
                "approved vote for unknown parameter id {signed_id} at boundary {boundary_height}"
            ))
        })?;
        let current_value = base.get(&param).copied().ok_or_else(|| {
            ChainError::Voting(format!(
                "approved vote for parameter id {signed_id} absent from table at boundary {boundary_height}"
            ))
        })?;
        // Ids mapped by `ordinary_param` always have min/max entries.
        let min = parameter_min(signed_id).unwrap_or(0);
        let max = parameter_max(signed_id).unwrap_or(i32::MAX / 2);
        let step = parameter_step(signed_id).unwrap_or_else(|| 1.max(current_value / 100));
        let new_value = if signed_id > 0 {
            if current_value < max {
                current_value + step
            } else {
                current_value
            }
        } else if current_value > min {
            current_value - step
        } else {
            current_value
        };
        table.insert(param, new_value);
    }

    // --- JVM `update` table3 ---
    //
    // Insert sub-blocks-per-block on the epoch after the v4 (protocol
    // 6.0) activation: skipped when the update being activated RIGHT NOW
    // disables rule 409 (`exMatchParameters`), in which case it fires at
    // the next boundary instead.
    let activated_bytes = activated.unwrap_or_else(|| encode_disabled_rules(&[]));
    let bv_now = table.get(&Parameter::BlockVersion).copied().unwrap_or(-1);
    if bv_now == 4
        && !table.contains_key(&Parameter::SubblocksPerBlock)
        && !parse_disabled_rules(&activated_bytes)?.contains(&409u16)
    {
        table.insert(Parameter::SubblocksPerBlock, SUBBLOCKS_PER_BLOCK_DEFAULT);
    }

    Ok((next, activated_bytes))
}

/// Fork-vote window gate — port of JVM `ErgoStateContext.checkForkVote`
/// (`ErgoStateContext.scala:156-168`), consensus rule 407
/// (`exCheckForkVote`): voting for a fork is prohibited while the active
/// round is closing or activating.
///
/// The JVM call-site condition `if (forkVote)` (`ErgoStateContext.scala:
/// 243` — boundary and mid-epoch headers alike) is folded into the seam:
/// `header_votes` not containing 120 passes without reading `current` at
/// all. `current` is the ACTIVE parameters — the table in force at the
/// header's height, JVM `currentParameters`.
///
/// With `S = current[122]` (gate inert when absent), `finishing =
/// S + L·ve`, `afterActivation = finishing + L·(ae+1)`, and `collected =
/// current[121]` — collected ONLY, not closing-epoch + collected; a
/// different operand than `updateFork`'s `votes` — the gate fires when:
///
/// - `finishing <= h < finishing + L` and the round is NOT approved (the
///   epoch right after a failed round closes), or
/// - `finishing <= h < afterActivation` and the round IS approved (the
///   whole activation window plus one epoch).
///
/// Returns:
/// - `Ok(true)` — header passes: no 120 vote, or no round in progress,
///   or height outside both reject windows.
/// - `Ok(false)` — rule 407 fires ("Voting for fork is prohibited"):
///   header invalid.
/// - `Err` — 122 present but 121 absent. The JVM `.get` is EAGER on gate
///   entry — unlike `updateFork`'s lazy `votes`, an orphan-122 table is
///   fatal here even at heights outside the windows (it throws inside
///   `validateNoThrow`, invalidating the header).
///
/// On the live path both `Ok(false)` and `Err` reject the header
/// (JVM-indistinguishable there — both surface as rule-407 invalid); the
/// three-way split exists for the SANTA chain-tier grading granularity
/// (kind `fork_vote_gate`).
///
/// Rule 407 is votable-disableable but active on both networks for all of
/// history (launch defaults disable only 215/409) — implemented as
/// always-on; dynamic rule-status tracking is out of scope until a real
/// on-chain update disables it.
///
/// **Pure**: settings and the active table arrive as arguments
/// (`version2_activation_height` is unread but present — uniform
/// settings block). The live hook is chain-entangled delegation onto
/// this seam — one implementation, same pattern as
/// [`compute_boundary_parameters`].
pub fn check_fork_vote(
    voting: &VotingConfig,
    header_height: u32,
    header_votes: [u8; 3],
    current: &Parameters,
) -> Result<bool, crate::ChainError> {
    use crate::ChainError;

    if !header_votes.contains(&(SOFT_FORK_VOTE as u8)) {
        return Ok(true);
    }
    let Some(&start) = current
        .parameters_table
        .get(&Parameter::SoftForkStartingHeight)
    else {
        return Ok(true);
    };

    // EAGER `.get` (ErgoStateContext.scala:161): read 121 before any
    // window math — an orphan-122 table errors regardless of height.
    let collected = current
        .parameters_table
        .get(&Parameter::SoftForkVotesCollected)
        .copied()
        .ok_or_else(|| {
            ChainError::Voting(format!(
                "SoftForkVotesCollected (121) absent with SoftForkStartingHeight present \
                 at height {header_height} (JVM checkForkVote `.get` throw)"
            ))
        })?;
    let approved = voting.soft_fork_approved(collected as i64);

    let h = header_height as i64;
    let l = voting.voting_length as i64;
    let ve = voting.soft_fork_epochs as i64;
    let ae = voting.activation_epochs as i64;
    let finishing = start as i64 + l * ve;
    let after_activation = finishing + l * (ae + 1);

    // Deliberately kept in the JVM's redundant two-window shape
    // (ErgoStateContext.scala:163-164) — reviewability against the
    // reference outweighs boolean minimality in a consensus rule.
    #[allow(clippy::nonminimal_bool)]
    if (h >= finishing && h < finishing + l && !approved)
        || (h >= finishing && h < after_activation && approved)
    {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voting_config_testnet_preset() {
        let cfg = VotingConfig::testnet();
        assert_eq!(cfg.voting_length, 128);
        assert_eq!(cfg.soft_fork_epochs, 32);
        assert_eq!(cfg.activation_epochs, 32);
        assert_eq!(cfg.version2_activation_height, 0);
    }

    #[test]
    fn voting_config_mainnet_preset() {
        let cfg = VotingConfig::mainnet();
        assert_eq!(cfg.voting_length, 1024);
        assert_eq!(cfg.soft_fork_epochs, 32);
        assert_eq!(cfg.activation_epochs, 32);
        assert_eq!(cfg.version2_activation_height, 417_792);
    }

    #[test]
    fn change_approved_strict_majority() {
        let cfg = VotingConfig::testnet();
        // votingLength = 128, threshold = 64
        assert!(!cfg.change_approved(0));
        assert!(!cfg.change_approved(64), "exact half is NOT approved");
        assert!(cfg.change_approved(65), "65 is strict majority");
        assert!(cfg.change_approved(128));
    }

    #[test]
    fn soft_fork_approved_supermajority() {
        let cfg = VotingConfig::testnet();
        // 128 * 32 * 9 / 10 = 36864 / 10 = 3686 (integer div)
        let threshold = (128 * 32 * 9 / 10) as i64;
        assert!(!cfg.soft_fork_approved(threshold));
        assert!(cfg.soft_fork_approved(threshold + 1));
    }

    #[test]
    fn soft_fork_approved_negative_never() {
        // A wrapped-negative running total compares signed — never approved.
        let cfg = VotingConfig::testnet();
        assert!(!cfg.soft_fork_approved(-1));
        assert!(!cfg.soft_fork_approved(i32::MIN as i64));
    }

    /// Window builder for seeded-tally tests: heights `start..start+n`,
    /// each with the given votes.
    fn window(start: u32, votes_per_height: &[[u8; 3]]) -> Vec<(u32, [u8; 3])> {
        votes_per_height
            .iter()
            .enumerate()
            .map(|(i, v)| (start + i as u32, *v))
            .collect()
    }

    #[test]
    fn seeded_tally_seed_counts_as_one() {
        // Boundary T=256, L=128 → window [128, 255]. Seed (h=128) votes id 1;
        // 64 mid-epoch headers vote id 1 → 65 total, seed included.
        let mut w = vec![(128u32, [1u8, 0, 0])];
        w.extend(window(129, &[[1u8, 0, 0]; 64]));
        w.extend(window(193, &[[0u8, 0, 0]; 63]));
        let tally = tally_votes_seeded(&w, 256, 128);
        assert_eq!(tally, vec![(1, 65)]);
    }

    #[test]
    fn seeded_tally_chain_start_clamp_empty() {
        // First boundary T=128: window clamps to [1, 127], head height 1 ≠
        // T − L = 0 → empty seed → every vote drops, even 110 of them.
        let mut w = window(1, &[[1u8, 0, 0]; 110]);
        w.extend(window(111, &[[0u8, 0, 0]; 17]));
        assert_eq!(w.len(), 127);
        let tally = tally_votes_seeded(&w, 128, 128);
        assert!(tally.is_empty(), "chain-start window must tally empty");
    }

    #[test]
    fn seeded_tally_unseeded_id_ignored_mid_epoch() {
        // Seed votes id 1 only; 70 mid-epoch headers vote id 2 → id 2
        // accumulates nothing; 10 vote id 1 → 11 total.
        let mut w = vec![(128u32, [1u8, 0, 0])];
        w.extend(window(129, &[[2u8, 0, 0]; 70]));
        w.extend(window(199, &[[1u8, 0, 0]; 10]));
        w.extend(window(209, &[[0u8, 0, 0]; 47]));
        let tally = tally_votes_seeded(&w, 256, 128);
        assert_eq!(tally, vec![(1, 11)], "unseeded id 2 accumulates nothing");
    }

    #[test]
    fn seeded_tally_empty_window_empty() {
        assert!(tally_votes_seeded(&[], 256, 128).is_empty());
    }

    #[test]
    fn seeded_tally_zero_vote_seed_stays_empty() {
        // Seed header votes nothing → empty seed → nothing can accumulate.
        let mut w = vec![(128u32, [0u8, 0, 0])];
        w.extend(window(129, &[[1u8, 2, 3]; 127]));
        assert!(tally_votes_seeded(&w, 256, 128).is_empty());
    }

    #[test]
    fn seeded_tally_multi_slot_and_negative_ids() {
        // Seed votes ids 1, -1 (0xFF), 120 across its three slots; each
        // increments independently, entries in SLOT order. (Negative ids
        // can't appear in a valid boundary header on-chain —
        // hdrVotesUnknown — but the pure tally seeds whatever the window
        // head carries; legality is upstream.)
        let mut w = vec![(128u32, [1u8, 0xFF, 120])];
        w.extend(window(129, &[[1u8, 120, 0]; 5]));
        let tally = tally_votes_seeded(&w, 256, 128);
        assert_eq!(tally, vec![(1, 6), (-1, 1), (SOFT_FORK_VOTE, 6)]);
    }

    #[test]
    fn seeded_tally_slot_order_preserved() {
        // The tally is ordered by the seed header's vote SLOTS, not by id:
        // a seed voting [3, 1, 2] yields entries in exactly that order.
        let w = vec![(128u32, [3u8, 1, 2])];
        let tally = tally_votes_seeded(&w, 256, 128);
        assert_eq!(tally, vec![(3, 1), (1, 1), (2, 1)]);
    }

    #[test]
    fn seeded_tally_duplicate_seed_id_increments_all_copies() {
        // A duplicated id in the seed produces one entry per copy, and
        // every window vote for that id increments ALL matching entries
        // (JVM `VotingData.update` maps over the whole array). On-chain
        // such a seed is rejected by hdrVotesDuplicates; handed streams
        // are graded anyway.
        let mut w = vec![(128u32, [1u8, 1, 0])];
        w.extend(window(129, &[[1u8, 0, 0]; 4]));
        let tally = tally_votes_seeded(&w, 256, 128);
        assert_eq!(tally, vec![(1, 5), (1, 5)]);
    }

    #[test]
    fn default_parameters_mainnet_includes_max_block_cost() {
        let p = default_parameters(crate::Network::Mainnet);
        // The bug we're working around: Parameters::default() omits MaxBlockCost.
        let _ = p.max_block_cost();
        assert_eq!(p.block_version(), 1);
    }

    #[test]
    fn default_parameters_testnet_starts_at_v4() {
        let p = default_parameters(crate::Network::Testnet);
        assert_eq!(
            p.block_version(),
            4,
            "testnet was created post-6.0 and starts at protocol v4"
        );
        // Ordinary defaults match mainnet
        assert_eq!(p.storage_fee_factor(), 1_250_000);
        assert_eq!(p.min_value_per_byte(), 360);
        assert_eq!(p.max_block_size(), 524_288);
        assert_eq!(p.max_block_cost(), 1_000_000);
        assert_eq!(p.token_access_cost(), 100);
        assert_eq!(p.input_cost(), 2_000);
        assert_eq!(p.data_input_cost(), 100);
        assert_eq!(p.output_cost(), 100);
    }

    #[test]
    fn default_parameters_mainnet_starts_at_v1() {
        let p = default_parameters(crate::Network::Mainnet);
        assert_eq!(p.block_version(), 1);
    }

    #[test]
    fn default_parameters_testnet_includes_subblocks() {
        // Testnet starts at BlockVersion=4 and JVM Parameters.scala::update
        // auto-inserts SubblocksPerBlock=30 whenever the table is at v4. The
        // chain-internal startup default must reflect that, so any code that
        // queries `params.parameters_table[&Parameter::SubblocksPerBlock]`
        // before the first epoch boundary still finds the value.
        let p = default_parameters(crate::Network::Testnet);
        assert_eq!(
            p.parameters_table
                .get(&Parameter::SubblocksPerBlock)
                .copied(),
            Some(30),
            "testnet defaults must include SubblocksPerBlock=30"
        );
    }

    #[test]
    fn default_parameters_mainnet_omits_subblocks() {
        // Mainnet starts at BlockVersion=1 and only progresses to v4 via
        // soft-fork voting. SubblocksPerBlock must NOT be present in the
        // pre-v4 startup table — it gets auto-inserted by
        // `compute_expected_parameters` once voting bumps the version.
        let p = default_parameters(crate::Network::Mainnet);
        assert!(
            !p.parameters_table
                .contains_key(&Parameter::SubblocksPerBlock),
            "mainnet starts at v1 and must not have SubblocksPerBlock"
        );
    }

    // ---- compute_boundary_parameters (pure JVM Parameters.update port) ----

    /// The canonical empty `ErgoValidationSettingsUpdate` encoding ("0000").
    fn empty_update() -> Vec<u8> {
        encode_disabled_rules(&[])
    }

    fn tally_of(entries: &[(i8, u32)]) -> Vec<(i8, u32)> {
        entries.to_vec()
    }

    /// Mainnet-default table (BlockVersion 1, no SubblocksPerBlock) — keeps
    /// soft-fork lifecycle tests clear of the v4 auto-insert arm.
    fn base_params() -> Parameters {
        default_parameters(crate::Network::Mainnet)
    }

    /// `base_params` plus an in-progress soft-fork round: 122 = `start`,
    /// 121 = `collected` (or ABSENT for the hostile 122-without-121 family).
    fn params_with_round(start: i32, collected: Option<i32>) -> Parameters {
        let mut p = base_params();
        p.parameters_table
            .insert(Parameter::SoftForkStartingHeight, start);
        if let Some(c) = collected {
            p.parameters_table
                .insert(Parameter::SoftForkVotesCollected, c);
        }
        p
    }

    #[test]
    fn boundary_params_majority_step_applies() {
        let cfg = VotingConfig::testnet();
        let (next, activated) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(1, 65)]), false, &[],
        )
        .unwrap();
        assert_eq!(next.storage_fee_factor(), 1_275_000, "step +25_000 for id 1");
        assert_eq!(activated, empty_update(), "non-activation boundary → 0x0000");
    }

    #[test]
    fn boundary_params_exact_half_no_step() {
        let cfg = VotingConfig::testnet();
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(1, 64)]), false, &[],
        )
        .unwrap();
        assert_eq!(next.storage_fee_factor(), 1_250_000, "64 of 128 is not > L/2");
    }

    #[test]
    fn boundary_params_negative_vote_decreases() {
        let cfg = VotingConfig::testnet();
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(-1, 65)]), false, &[],
        )
        .unwrap();
        assert_eq!(next.storage_fee_factor(), 1_225_000);
    }

    #[test]
    fn boundary_params_guarded_at_min() {
        let cfg = VotingConfig::testnet();
        let mut params = base_params();
        params
            .parameters_table
            .insert(Parameter::MaxBlockSize, 16 * 1024); // at min
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &params, &tally_of(&[(-3, 65)]), false, &[],
        )
        .unwrap();
        assert_eq!(
            next.max_block_size(),
            16 * 1024,
            "guard prevents decrease when current == min"
        );
    }

    /// Regression: epoch 154 (height 157,696) MaxBlockCost must be 1,010,000.
    /// The old hardcoded step of 100,000 produced 1,100,000 which disagreed
    /// with the extension section on mainnet.
    #[test]
    fn boundary_params_max_block_cost_dynamic_step() {
        let cfg = VotingConfig::testnet();
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(4, 65)]), false, &[],
        )
        .unwrap();
        assert_eq!(
            next.max_block_cost(),
            1_010_000,
            "dynamic step = max(1, current/100) = 10,000"
        );
    }

    #[test]
    fn boundary_params_approved_unknown_id_errors() {
        // JVM `updateParams` reads `parametersTable(paramIdAbs)` for an
        // approved id — an unknown id throws there (invalid block). Mirror.
        let cfg = VotingConfig::testnet();
        let r = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(99, 65)]), false, &[],
        );
        assert!(r.is_err(), "approved unknown id must error like the JVM throw");
    }

    #[test]
    fn boundary_params_unapproved_unknown_id_harmless() {
        // Below the threshold the JVM guard short-circuits before the table
        // read — unknown ids without a majority are silently ignored.
        let cfg = VotingConfig::testnet();
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(99, 10)]), false, &[],
        )
        .unwrap();
        assert_eq!(next, base_params());
    }

    // ---- soft-fork lifecycle through the pure seam ----

    #[test]
    fn boundary_params_fork_votes_without_round_leave_table_unchanged() {
        // Tally full of id-120 votes, but the boundary header itself does
        // NOT vote for a fork and no round is in progress: JVM starts a
        // round only on `forkVote` (the boundary header's own vote) — the
        // closing epoch's 120 count alone must NOT create counters.
        let cfg = VotingConfig::testnet();
        let (next, activated) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(SOFT_FORK_VOTE, 128)]), false, &[],
        )
        .unwrap();
        assert_eq!(next, base_params(), "no 121/122 counters may appear");
        assert_eq!(activated, empty_update());
    }

    #[test]
    fn boundary_params_boundary_fork_vote_starts_round() {
        let cfg = VotingConfig::testnet();
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &[], true, &[],
        )
        .unwrap();
        assert_eq!(
            next.soft_fork_starting_height(),
            Some(256),
            "id 122 = boundary height on round start"
        );
        assert_eq!(next.soft_fork_votes_collected(), Some(0), "id 121 starts at 0");
    }

    #[test]
    fn boundary_params_mid_voting_accumulates_epoch_votes() {
        let cfg = VotingConfig::testnet();
        let mut params = base_params();
        params
            .parameters_table
            .insert(Parameter::SoftForkStartingHeight, 128);
        params
            .parameters_table
            .insert(Parameter::SoftForkVotesCollected, 10);
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &params, &tally_of(&[(SOFT_FORK_VOTE, 50)]), false, &[],
        )
        .unwrap();
        assert_eq!(
            next.soft_fork_votes_collected(),
            Some(60),
            "votes = closing epoch's 120 count + collected"
        );
        assert_eq!(next.soft_fork_starting_height(), Some(128));
    }

    #[test]
    fn boundary_params_activation_bumps_version_and_activates_update() {
        // start=128, L=128, ve=ae=32 → activation at 128 + 128*64 = 8320.
        // Approval needs > 128*32*9/10 = 3686 collected votes.
        let cfg = VotingConfig::testnet();
        let mut params = base_params();
        params
            .parameters_table
            .insert(Parameter::SoftForkStartingHeight, 128);
        params
            .parameters_table
            .insert(Parameter::SoftForkVotesCollected, 3_700);
        let proposed = encode_disabled_rules(&[215]);
        let (next, activated) = compute_boundary_parameters(
            &cfg, 8_320, &params, &[], false, &proposed,
        )
        .unwrap();
        assert_eq!(next.block_version(), 2, "voting-driven activation bumps BlockVersion");
        assert_eq!(
            activated, proposed,
            "activated update is the boundary's proposed update, passed through"
        );
        // Counters survive activation; cleanup happens one activation period later.
        assert_eq!(next.soft_fork_starting_height(), Some(128));
    }

    #[test]
    fn boundary_params_activation_without_approval_no_bump() {
        let cfg = VotingConfig::testnet();
        let mut params = base_params();
        params
            .parameters_table
            .insert(Parameter::SoftForkStartingHeight, 128);
        params
            .parameters_table
            .insert(Parameter::SoftForkVotesCollected, 100); // below 3686
        let (next, activated) = compute_boundary_parameters(
            &cfg, 8_320, &params, &[], false, &[],
        )
        .unwrap();
        assert_eq!(next.block_version(), 1);
        assert_eq!(activated, empty_update());
    }

    #[test]
    fn boundary_params_failed_voting_cleanup() {
        // cleanup-fail at start + L*(ve+1) = 128 + 128*33 = 4352 when not
        // approved: counters removed, nothing re-inserted without forkVote.
        let cfg = VotingConfig::testnet();
        let mut params = base_params();
        params
            .parameters_table
            .insert(Parameter::SoftForkStartingHeight, 128);
        params
            .parameters_table
            .insert(Parameter::SoftForkVotesCollected, 100);
        let (next, _) = compute_boundary_parameters(
            &cfg, 4_352, &params, &[], false, &[],
        )
        .unwrap();
        assert_eq!(next.soft_fork_starting_height(), None);
        assert_eq!(next.soft_fork_votes_collected(), None);
    }

    #[test]
    fn boundary_params_successful_voting_cleanup_and_restart() {
        // cleanup-success at start + L*(ve+ae+1) = 128 + 128*65 = 8448 with
        // approval. With the boundary header voting for a fork again, a new
        // round starts at the same boundary (JVM sequential ifs).
        let cfg = VotingConfig::testnet();
        let mut params = base_params();
        params
            .parameters_table
            .insert(Parameter::SoftForkStartingHeight, 128);
        params
            .parameters_table
            .insert(Parameter::SoftForkVotesCollected, 3_700);
        // Without forkVote: counters just clear.
        let (cleared, _) = compute_boundary_parameters(
            &cfg, 8_448, &params, &[], false, &[],
        )
        .unwrap();
        assert_eq!(cleared.soft_fork_starting_height(), None);
        assert_eq!(cleared.soft_fork_votes_collected(), None);
        // With forkVote: cleanup then immediate restart.
        let (restarted, _) = compute_boundary_parameters(
            &cfg, 8_448, &params, &[], true, &[],
        )
        .unwrap();
        assert_eq!(restarted.soft_fork_starting_height(), Some(8_448));
        assert_eq!(restarted.soft_fork_votes_collected(), Some(0));
    }

    #[test]
    fn boundary_params_forced_v2_at_activation_height() {
        // Mainnet forced v1→v2 at 417,792 (a non-voted hard fork). The
        // activated update stays empty — JVM does not wire activatedUpdate
        // for the forced path.
        let cfg = VotingConfig::mainnet();
        let (next, activated) = compute_boundary_parameters(
            &cfg, 417_792, &base_params(), &[], false, &[],
        )
        .unwrap();
        assert_eq!(next.block_version(), 2);
        assert_eq!(activated, empty_update(), "forced v2 does not activate the update");
    }

    #[test]
    fn boundary_params_subblocks_insert_gated_on_rule_409() {
        // A voting-driven 3→4 activation: when the activated update
        // disables rule 409, the SubblocksPerBlock auto-insert is skipped
        // (fires at the NEXT boundary); without 409 it fires immediately.
        let cfg = VotingConfig::testnet();
        let mut params = base_params();
        params.parameters_table.insert(Parameter::BlockVersion, 3);
        params
            .parameters_table
            .insert(Parameter::SoftForkStartingHeight, 128);
        params
            .parameters_table
            .insert(Parameter::SoftForkVotesCollected, 3_700);

        let with_409 = encode_disabled_rules(&[215, 409]);
        let (next, activated) = compute_boundary_parameters(
            &cfg, 8_320, &params, &[], false, &with_409,
        )
        .unwrap();
        assert_eq!(next.block_version(), 4);
        assert_eq!(activated, with_409);
        assert!(
            !next
                .parameters_table
                .contains_key(&Parameter::SubblocksPerBlock),
            "rule 409 in the activated update suppresses the insert"
        );

        let without_409 = encode_disabled_rules(&[215]);
        let (next, _) = compute_boundary_parameters(
            &cfg, 8_320, &params, &[], false, &without_409,
        )
        .unwrap();
        assert_eq!(next.block_version(), 4);
        assert_eq!(
            next.parameters_table
                .get(&Parameter::SubblocksPerBlock)
                .copied(),
            Some(SUBBLOCKS_PER_BLOCK_DEFAULT),
            "no 409 in the activated update → insert fires at this boundary"
        );
    }

    #[test]
    fn boundary_params_at_v4_inserts_subblocks_when_absent() {
        // Non-activation boundary with the table already at v4 and id 9
        // absent: activated update is empty (contains no 409) → insert.
        let cfg = VotingConfig::testnet();
        let mut params = base_params();
        params.parameters_table.insert(Parameter::BlockVersion, 4);
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &params, &[], false, &[],
        )
        .unwrap();
        assert_eq!(
            next.parameters_table
                .get(&Parameter::SubblocksPerBlock)
                .copied(),
            Some(SUBBLOCKS_PER_BLOCK_DEFAULT)
        );
    }

    // ---- hostile 122-without-121: the lazy `votes` force sites ----
    //
    // With testnet settings and S=128: mid-round accumulate ends at
    // 128 + 128·32 = 4224; cleanup-fail = 128 + 128·33 = 4352;
    // activation = 128 + 128·64 = 8320; cleanup-success = 128 + 128·65
    // = 8448.

    #[test]
    fn boundary_params_122_without_121_errors_at_force_heights() {
        // JVM forces `lazy val votes` (a Map.apply read of 121) at: any
        // accumulate boundary (h ≤ S+L·ve), the cleanup-fail height, the
        // activation height, and the cleanup-success height.
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, None);
        for h in [256u32, 4_352, 8_320, 8_448] {
            let r = compute_boundary_parameters(&cfg, h, &params, &[], false, &[]);
            assert!(r.is_err(), "h={h} must force `votes` and error on absent 121");
        }
    }

    #[test]
    fn boundary_params_122_without_121_passes_at_non_force_boundary() {
        // At any boundary that is NOT a force site the lifecycle passes
        // through WITHOUT error — an eager read of 121 (either defaulted
        // or erroring) diverges from the JVM in one direction or the
        // other. 4480 and 8576 are past mid-round and none of the
        // cleanup/activation heights.
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, None);
        for h in [4_480u32, 8_576] {
            let (next, activated) =
                compute_boundary_parameters(&cfg, h, &params, &[], false, &[]).unwrap();
            assert_eq!(next, params, "table passes through unchanged at h={h}");
            assert_eq!(activated, empty_update());
        }
    }

    #[test]
    fn boundary_params_votes_in_prev_epoch_is_first_120_entry() {
        // JVM `epochVotes.find(_._1 == SoftFork)` — the FIRST id-120
        // entry of the ordered tally, not a sum over duplicates.
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, Some(5));
        let (next, _) = compute_boundary_parameters(
            &cfg,
            256,
            &params,
            &tally_of(&[(SOFT_FORK_VOTE, 10), (SOFT_FORK_VOTE, 99)]),
            false,
            &[],
        )
        .unwrap();
        assert_eq!(
            next.soft_fork_votes_collected(),
            Some(15),
            "collected 5 + FIRST 120 entry (10), not the sum or the max"
        );
    }

    #[test]
    fn boundary_params_votes_wrap_like_jvm_int() {
        // JVM `votes` is Int addition: the accumulate branch stores the
        // wrapped i32 verbatim, and the signed approval compare never
        // approves a negative total.
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, Some(i32::MAX));
        let (next, _) = compute_boundary_parameters(
            &cfg,
            256,
            &params,
            &tally_of(&[(SOFT_FORK_VOTE, 1)]),
            false,
            &[],
        )
        .unwrap();
        assert_eq!(
            next.soft_fork_votes_collected(),
            Some(i32::MIN),
            "i32::MAX + 1 wraps and is stored verbatim"
        );
    }

    // ---- ordered updateParams fold ----

    #[test]
    fn boundary_params_contradictory_pair_order_determines_result() {
        // +1 and −1 both approved (handed seed slots — on-chain such a
        // seed is rejected by hdrVotesContradictory; legality is
        // upstream): JVM folds in sequence order, each step reading the
        // post-fork SNAPSHOT, so the LAST entry wins deterministically.
        let cfg = VotingConfig::testnet();
        let (plus_then_minus, _) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(1, 65), (-1, 65)]), false, &[],
        )
        .unwrap();
        assert_eq!(
            plus_then_minus.storage_fee_factor(),
            1_225_000,
            "(+1, −1): the −1 write lands last"
        );

        let (minus_then_plus, _) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(-1, 65), (1, 65)]), false, &[],
        )
        .unwrap();
        assert_eq!(
            minus_then_plus.storage_fee_factor(),
            1_275_000,
            "(−1, +1): the +1 write lands last"
        );
    }

    #[test]
    fn boundary_params_duplicate_entries_read_snapshot_not_accumulator() {
        // Duplicate approved entries for one id: each fold step reads the
        // snapshot, so the step applies once — not compounded.
        let cfg = VotingConfig::testnet();
        let (next, _) = compute_boundary_parameters(
            &cfg, 256, &base_params(), &tally_of(&[(1, 65), (1, 65)]), false, &[],
        )
        .unwrap();
        assert_eq!(next.storage_fee_factor(), 1_275_000, "one step, not two");
    }

    // ---- strict parse: parse_validation_settings_update ----

    #[test]
    fn strict_parse_mandatory_rule_rejects() {
        // 102 (txManyInputs) is mayBeDisabled=false in rulesSpec.
        let bytes = encode_disabled_rules(&[102]);
        assert!(parse_validation_settings_update(&bytes).is_err());
    }

    #[test]
    fn strict_parse_disableable_and_unknown_ids_pass() {
        // 111/409 are mayBeDisabled=true; 414 has a constant but no
        // rulesSpec entry; 1011 is a sigma-side id — both pass through
        // (JVM `rulesSpec.get(rd).forall(_.mayBeDisabled)`).
        for id in [111u16, 409, 414, 1011] {
            let bytes = encode_disabled_rules(&[id]);
            assert_eq!(
                parse_validation_settings_update(&bytes).unwrap(),
                vec![id],
                "id {id} must pass the strict parse"
            );
        }
    }

    #[test]
    fn strict_parse_empty_input_ok() {
        // Absent extension field = empty update by convention.
        assert_eq!(
            parse_validation_settings_update(&[]).unwrap(),
            Vec::<u16>::new()
        );
    }

    #[test]
    fn strict_parse_bare_rules_count_rejects() {
        // A 1-byte `0x00` payload is truncated before the statusUpdates
        // count — JVM `getUInt` underflow. The lenient
        // `parse_disabled_rules` keeps accepting it (see
        // `parse_disabled_rules_count_only_ok`).
        assert!(parse_validation_settings_update(&[0x00]).is_err());
        assert!(
            parse_disabled_rules(&[0x00]).is_ok(),
            "lenient reader contrast"
        );
    }

    #[test]
    fn strict_parse_truncated_rules_rejects() {
        // Claims 2 rules, supplies bytes for 1.
        assert!(parse_validation_settings_update(&[0x02, 0xD7, 0x01]).is_err());
    }

    #[test]
    fn strict_parse_mainnet_v6_payload_passes_with_status_tail() {
        // The real h=1,628,160 ID 124 payload: rulesToDisable=[215,409]
        // plus 3 status updates. Entries after the count pass
        // UNVALIDATED — the payload must keep flowing verbatim.
        let bytes: [u8; 18] = [
            0x02, 0xD7, 0x01, 0x99, 0x03, 0x03, 0x0B, 0x01, 0x03,
            0x10, 0x07, 0x01, 0x03, 0x11, 0x08, 0x01, 0x03, 0x12,
        ];
        assert_eq!(
            parse_validation_settings_update(&bytes).unwrap(),
            vec![215, 409]
        );
    }

    #[test]
    fn strict_parse_nonzero_status_count_lenient_tail() {
        // Rules count 0, status count 2, one garbage byte: entries are
        // not validated (no RuleStatusSerializer port) — Ok.
        assert_eq!(
            parse_validation_settings_update(&[0x00, 0x02, 0xAB]).unwrap(),
            Vec::<u16>::new()
        );
    }

    #[test]
    fn strict_parse_rules_count_wraps_like_jvm_toint() {
        // JVM `getUInt().toInt` wraps 0xFFFFFFFF to −1 and `0 until -1`
        // reads zero rule ids: the payload parses as an empty list, then
        // the status count.
        let bytes = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x00];
        assert_eq!(
            parse_validation_settings_update(&bytes).unwrap(),
            Vec::<u16>::new()
        );
    }

    #[test]
    fn boundary_params_strict_parse_reject_arm() {
        // The pure seam parse-validates `proposed_update` up front — a
        // mandatory-rule update errors (the SANTA reject arm). The live
        // wrappers pre-swallow instead; see the chain-level test.
        let cfg = VotingConfig::testnet();
        let hostile = encode_disabled_rules(&[102]);
        let r =
            compute_boundary_parameters(&cfg, 256, &base_params(), &[], false, &hostile);
        assert!(r.is_err());
    }

    // ---- check_fork_vote (JVM ErgoStateContext.checkForkVote) ----
    //
    // S=128, testnet: finishing = 128 + 128·32 = 4224; afterActivation =
    // 4224 + 128·33 = 8448.

    /// A header voting 120 in its first slot — the gate's trigger case.
    const FORK_VOTE_SLOTS: [u8; 3] = [120, 0, 0];

    #[test]
    fn check_fork_vote_no_120_vote_always_passes() {
        // The JVM call-site condition `if (forkVote)` is folded into the
        // seam: a header not voting 120 passes WITHOUT reading the table
        // — even an orphan-122 table cannot error.
        let cfg = VotingConfig::testnet();
        let orphan = params_with_round(128, None);
        assert!(check_fork_vote(&cfg, 4_224, [0, 0, 0], &orphan).unwrap());
        assert!(check_fork_vote(&cfg, 4_224, [1, 2, 3], &base_params()).unwrap());
    }

    #[test]
    fn check_fork_vote_inert_without_round() {
        // No 122 in the table: Ok(true) at any height, vote present.
        let cfg = VotingConfig::testnet();
        for h in [1u32, 4_224, 8_447, 1_000_000] {
            assert!(
                check_fork_vote(&cfg, h, FORK_VOTE_SLOTS, &base_params()).unwrap(),
                "no round in progress must pass at h={h}"
            );
        }
    }

    #[test]
    fn check_fork_vote_failed_round_window_edges() {
        // collected=100, not approved: Ok(false) exactly on [finishing,
        // finishing+L) = [4224, 4352).
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, Some(100));
        let gate = |h| check_fork_vote(&cfg, h, FORK_VOTE_SLOTS, &params).unwrap();
        assert!(gate(4_223), "before finishing");
        assert!(!gate(4_224), "window start");
        assert!(!gate(4_351), "last in window");
        assert!(gate(4_352), "past the failed-round window");
        // 120 in any vote slot triggers the gate, not just slot 0.
        assert!(!check_fork_vote(&cfg, 4_224, [0, 120, 0], &params).unwrap());
    }

    #[test]
    fn check_fork_vote_approved_round_window_edges() {
        // collected=3700 > 3686, approved: Ok(false) on [finishing,
        // afterActivation) = [4224, 8448) — the whole activation period
        // plus one epoch.
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, Some(3_700));
        let gate = |h| check_fork_vote(&cfg, h, FORK_VOTE_SLOTS, &params).unwrap();
        assert!(gate(4_223), "before finishing");
        assert!(!gate(4_224), "window start");
        assert!(!gate(8_447), "last in window");
        assert!(gate(8_448), "after the activation window");
    }

    #[test]
    fn check_fork_vote_122_without_121_errors_eagerly() {
        // The `.get` of 121 is EAGER on gate entry (unlike `updateFork`'s
        // lazy `votes`): with a 120 vote present, an orphan-122 table
        // errors even at heights OUTSIDE both reject windows.
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, None);
        for h in [10u32, 4_224, 100_000] {
            assert!(
                check_fork_vote(&cfg, h, FORK_VOTE_SLOTS, &params).is_err(),
                "orphan-122 must error at h={h}"
            );
        }
    }

    // ---- zombie family regression ----

    #[test]
    fn boundary_params_zombie_stuck_table_passes_through() {
        // A round whose cleanup heights were missed (recompute resumed
        // past them): at any later boundary no lifecycle branch can fire
        // — the stuck counters pass through unchanged, no error, no
        // rationalizing them away.
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, Some(100));
        let (next, activated) =
            compute_boundary_parameters(&cfg, 8_576, &params, &[], false, &[]).unwrap();
        assert_eq!(next, params, "stuck counters pass through AS-IS");
        assert_eq!(activated, empty_update());
    }

    #[test]
    fn boundary_params_zombie_revival_at_late_cleanup_height() {
        // A fork vote exactly at S + L·(ve+ae+1) restarts the round even
        // when the dying round was never approved — the restart's
        // cleanup-success disjunct has NO approval check (JVM
        // Parameters.scala:127-128). The one legal zombie revival.
        let cfg = VotingConfig::testnet();
        let params = params_with_round(128, Some(100)); // never approved
        let (next, _) =
            compute_boundary_parameters(&cfg, 8_448, &params, &[], true, &[]).unwrap();
        assert_eq!(
            next.soft_fork_starting_height(),
            Some(8_448),
            "round restarted at the late-cleanup boundary"
        );
        assert_eq!(next.soft_fork_votes_collected(), Some(0));
    }

    #[test]
    fn parse_parameters_roundtrip() {
        let mut input: HashMap<i8, i32> = HashMap::new();
        input.insert(1, 1_250_000); // StorageFeeFactor
        input.insert(4, 1_000_000); // MaxBlockCost
        input.insert(123, 2);       // BlockVersion

        let kv = pack_parameters_to_kv(&input);
        // Each entry is 2-byte key + 4-byte value
        for (key, value) in &kv {
            assert_eq!(key[0], 0x00);
            assert_eq!(value.len(), 4);
        }

        let parsed = parse_parameters_from_kv(&kv).unwrap();
        assert_eq!(parsed, input);
    }

    #[test]
    fn parse_parameters_skips_non_param_fields() {
        let kv = vec![
            ([0x01u8, 0x00], vec![0xAA; 33]), // Interlink field — ignored
            ([0x00u8, 0x01], 1_250_000i32.to_be_bytes().to_vec()),
        ];
        let parsed = parse_parameters_from_kv(&kv).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get(&1), Some(&1_250_000));
    }

    #[test]
    fn parse_parameters_negative_id() {
        // Negative param IDs (decrease votes) appear in vote slots, NOT in
        // the extension table. Only positive IDs are stored. But the parser
        // accepts any byte; let's just verify it round-trips.
        let kv = vec![([0x00u8, 0xFF], 100i32.to_be_bytes().to_vec())];
        let parsed = parse_parameters_from_kv(&kv).unwrap();
        assert_eq!(parsed.get(&-1), Some(&100));
    }

    #[test]
    fn parse_parameters_wrong_value_length_errors() {
        let kv = vec![([0x00u8, 0x01], vec![0u8; 3])];
        assert!(parse_parameters_from_kv(&kv).is_err());
    }

    #[test]
    fn parse_parameters_skips_id_124() {
        // ID 124 has variable-length encoding; we defer it.
        let kv = vec![
            ([0x00u8, 124], vec![0u8; 17]), // Bogus length — must be skipped, not error
            ([0x00u8, 1], 100i32.to_be_bytes().to_vec()),
        ];
        let parsed = parse_parameters_from_kv(&kv).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get(&1), Some(&100));
    }

    #[test]
    fn pack_parameters_skips_id_124() {
        let mut input: HashMap<i8, i32> = HashMap::new();
        input.insert(124, 999);
        input.insert(1, 100);
        let kv = pack_parameters_to_kv(&input);
        assert_eq!(kv.len(), 1);
        assert_eq!(kv[0].0, [0x00u8, 0x01]);
    }

    #[test]
    fn pack_parameters_deterministic_order() {
        let mut input: HashMap<i8, i32> = HashMap::new();
        input.insert(123, 2);
        input.insert(1, 1_250_000);
        input.insert(8, 100);
        let kv = pack_parameters_to_kv(&input);
        // Sorted by ID ascending
        assert_eq!(kv[0].0[1], 1);
        assert_eq!(kv[1].0[1], 8);
        assert_eq!(kv[2].0[1], 123);
    }

    #[test]
    fn extension_bytes_roundtrip() {
        use ergo_chain_types::{BlockId, Digest32};

        let header_id = BlockId(Digest32::from([0xABu8; 32]));
        let fields = vec![
            ([0x00u8, 0x01], 1_250_000i32.to_be_bytes().to_vec()),
            ([0x00u8, 0x7B], 2i32.to_be_bytes().to_vec()), // 0x7B = 123 = BlockVersion
            ([0x01u8, 0x00], vec![0xCD; 33]),                // interlink-style field
        ];

        let packed = pack_extension_bytes(&header_id, &fields);
        let (parsed_id, parsed_fields) = parse_extension_bytes(&packed).unwrap();

        assert_eq!(parsed_id.0 .0, [0xABu8; 32]);
        assert_eq!(parsed_fields, fields);
    }

    /// Hardcoded byte sequence in the corrected wire format. Verifies
    /// the parser handles the JVM-canonical layout:
    ///
    /// `[header_id: 32B][field_count: VLQ u32][fields...]`
    #[test]
    fn extension_bytes_hardcoded_layout() {
        // header_id = [0xAA; 32]
        // 2 fields:
        //   [0x00, 0x01] -> [0x00, 0x13, 0x12, 0xD0]  (4 bytes = 1_250_000 BE)
        //   [0x01, 0x00] -> [0xFF, 0xEE, 0xDD]        (3 bytes)
        // VLQ(2) = 0x02
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0xAAu8; 32]);
        bytes.push(0x02); // VLQ field_count = 2
        // Field 1
        bytes.extend_from_slice(&[0x00, 0x01]); // key
        bytes.push(4); // val_len
        bytes.extend_from_slice(&1_250_000i32.to_be_bytes()); // value
        // Field 2
        bytes.extend_from_slice(&[0x01, 0x00]); // key
        bytes.push(3); // val_len
        bytes.extend_from_slice(&[0xFF, 0xEE, 0xDD]); // value

        let (id, fields) = parse_extension_bytes(&bytes).expect("parse hardcoded");
        assert_eq!(id.0 .0, [0xAAu8; 32]);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, [0x00, 0x01]);
        assert_eq!(fields[0].1, 1_250_000i32.to_be_bytes().to_vec());
        assert_eq!(fields[1].0, [0x01, 0x00]);
        assert_eq!(fields[1].1, vec![0xFF, 0xEE, 0xDD]);
    }

    /// Verify the writer matches the hardcoded layout exactly.
    #[test]
    fn extension_bytes_writer_matches_hardcoded_layout() {
        use ergo_chain_types::{BlockId, Digest32};
        let header_id = BlockId(Digest32::from([0xAAu8; 32]));
        let fields = vec![
            ([0x00u8, 0x01], 1_250_000i32.to_be_bytes().to_vec()),
            ([0x01u8, 0x00], vec![0xFF, 0xEE, 0xDD]),
        ];
        let packed = pack_extension_bytes(&header_id, &fields);

        let mut expected = Vec::new();
        expected.extend_from_slice(&[0xAAu8; 32]);
        expected.push(0x02); // VLQ field_count
        expected.extend_from_slice(&[0x00, 0x01, 4]);
        expected.extend_from_slice(&1_250_000i32.to_be_bytes());
        expected.extend_from_slice(&[0x01, 0x00, 3]);
        expected.extend_from_slice(&[0xFF, 0xEE, 0xDD]);

        assert_eq!(packed, expected);
    }

    #[test]
    fn extension_bytes_too_short_errors() {
        let result = parse_extension_bytes(&[0u8; 10]);
        assert!(result.is_err());
    }

    #[test]
    fn extension_bytes_truncated_field_errors() {
        // Header ID + VLQ count = 1 + key (2 bytes) but no length byte
        let mut bytes = vec![0u8; 32];
        bytes.push(0x01); // VLQ count = 1
        bytes.extend_from_slice(&[0x00, 0x01]);
        let result = parse_extension_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn extension_bytes_truncated_value_errors() {
        // Header ID + VLQ count = 1 + key + len=10 but only 5 bytes follow
        let mut bytes = vec![0u8; 32];
        bytes.push(0x01);
        bytes.extend_from_slice(&[0x00, 0x01, 10]);
        bytes.extend_from_slice(&[0u8; 5]);
        let result = parse_extension_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn extension_bytes_value_over_64_errors() {
        let mut bytes = vec![0u8; 32];
        bytes.push(0x01);
        bytes.extend_from_slice(&[0x00, 0x01, 65]);
        bytes.extend_from_slice(&[0u8; 65]);
        let result = parse_extension_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn extension_bytes_empty_fields_ok() {
        use ergo_chain_types::{BlockId, Digest32};
        let header_id = BlockId(Digest32::from([0u8; 32]));
        let bytes = pack_extension_bytes(&header_id, &[]);
        let (id, fields) = parse_extension_bytes(&bytes).unwrap();
        assert_eq!(id.0 .0, [0u8; 32]);
        assert!(fields.is_empty());
        // Empty extension is exactly: 32 header bytes + 1 VLQ count byte (0)
        assert_eq!(bytes.len(), 33);
        assert_eq!(bytes[32], 0x00);
    }

    /// Regression: mainnet block 1,628,160 extension carries this exact
    /// `SoftForkDisablingRules` payload (key `[0x00, 0x7C]`). JVM's
    /// `ErgoValidationSettingsUpdateSerializer` parses it as
    /// `rulesToDisable = [215, 409]` + 3 status updates. Rule 409 in this
    /// list is what suppresses the `SubblocksPerBlock` auto-insert at the
    /// v6 BlockVersion activation — without this gate our node emits 12
    /// parameter-table entries vs JVM's 11 and diverges.
    #[test]
    fn parse_disabled_rules_mainnet_v6_activation() {
        // 02 d7 01 99 03 03 0b 01 03 10 07 01 03 11 08 01 03 12
        // ^^ ^^^^^^^^^^^^^^ ^^ -- remainder is statusUpdates (ignored)
        // |  |           |
        // |  215 (VLQ)   409 (VLQ)
        // disabledRulesNum = 2 (VLQ)
        let bytes: [u8; 18] = [
            0x02, 0xD7, 0x01, 0x99, 0x03, 0x03, 0x0B, 0x01, 0x03,
            0x10, 0x07, 0x01, 0x03, 0x11, 0x08, 0x01, 0x03, 0x12,
        ];
        let rules = parse_disabled_rules(&bytes).unwrap();
        assert_eq!(rules, vec![215u16, 409u16]);
    }

    #[test]
    fn parse_disabled_rules_empty_input_ok() {
        // Absent ID 124 field maps to empty input by convention — JVM
        // treats it as `ErgoValidationSettingsUpdate.empty`.
        assert_eq!(parse_disabled_rules(&[]).unwrap(), Vec::<u16>::new());
    }

    #[test]
    fn parse_disabled_rules_count_only_ok() {
        // `disabledRulesNum = 0` with no rules following — valid.
        assert_eq!(parse_disabled_rules(&[0x00]).unwrap(), Vec::<u16>::new());
    }

    #[test]
    fn parse_disabled_rules_truncated_errors() {
        // Claims 2 rules, supplies bytes for 1. Must not panic.
        let bytes = [0x02u8, 0xD7, 0x01];
        assert!(parse_disabled_rules(&bytes).is_err());
    }

    /// Round-trip: encode the mainnet launch default
    /// (`ErgoValidationSettingsUpdate(Seq(215, 409), Seq.empty)`) and
    /// parse it back. Any future encoder change must preserve this.
    #[test]
    fn encode_decode_mainnet_launch_round_trip() {
        let bytes = encode_disabled_rules(&[215, 409]);
        let rules = parse_disabled_rules(&bytes).unwrap();
        assert_eq!(rules, vec![215u16, 409u16]);
    }

    /// The encoder produces exactly the launch-default byte sequence:
    /// `[disabledRulesNum=2][215][409][statusUpdatesNum=0]` in VLQ, i.e.
    /// `02 D7 01 99 03 00`. This is the prefix of the mainnet block
    /// 1,628,160 ID 124 value — the trailing `03 0B 01 03 10 07 01 03 11
    /// 08 01 03 12` encodes 3 status updates we deliberately don't model.
    #[test]
    fn encode_disabled_rules_empty_status_updates_layout() {
        let bytes = encode_disabled_rules(&[215, 409]);
        assert_eq!(
            bytes,
            vec![0x02, 0xD7, 0x01, 0x99, 0x03, 0x00],
            "encoder emits rulesToDisable=[215,409] + empty statusUpdates"
        );
    }

    #[test]
    fn encode_disabled_rules_empty_input() {
        // Empty rules list + empty statusUpdates = two VLQ-zero bytes.
        let bytes = encode_disabled_rules(&[]);
        assert_eq!(bytes, vec![0x00, 0x00]);
    }

    /// Mainnet seed matches the rulesToDisable prefix of the on-chain
    /// ID 124 value at every observed boundary (h=1,562,624 / 1,627,136
    /// / 1,628,160). The 6th byte diverges (we emit `0x00` for empty
    /// statusUpdates; on-chain has `0x03` for 3 status updates that
    /// predate our first observation). This is acceptable because the
    /// main-session validator gates the byte-for-byte comparison on
    /// BlockVersion >= 4 (JVM `matchParameters60`), and by the time v4
    /// activates at h=1,628,160 the seed has been superseded by every
    /// prior boundary's `apply_epoch_boundary_parameters` call.
    #[test]
    fn default_proposed_update_bytes_mainnet_matches_rules_prefix() {
        let bytes = default_proposed_update_bytes(crate::Network::Mainnet);
        assert!(
            bytes.starts_with(&[0x02, 0xD7, 0x01, 0x99, 0x03]),
            "mainnet seed must encode rulesToDisable=[215,409]; got {bytes:?}"
        );
    }

    #[test]
    fn default_proposed_update_bytes_testnet_matches_rules_prefix() {
        let bytes = default_proposed_update_bytes(crate::Network::Testnet);
        assert!(
            bytes.starts_with(&[0x02, 0xD7, 0x01, 0x99, 0x03]),
            "testnet seed must encode rulesToDisable=[215,409]; got {bytes:?}"
        );
    }
}

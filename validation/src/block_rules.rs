//! Block-level validation rules taken from JVM `ErgoStateContext.appendFullBlock`.
//!
//! Runs before the state changes, at every height (checkpoint or not). A
//! violation is [`ValidationError::BlockRule`] with the JVM's rule id.
//!
//! Rules: 306 (block transactions size), 400 (extension size), 401
//! (interlink field encoding), 402 (interlink structure), 404 (field value
//! length), 405 (duplicate keys), 406 (empty extension).
//!
//! See `facts/validation.md` § "Block rules taken from `appendFullBlock`".

use std::collections::HashSet;

use ergo_chain_types::{BlockId, Digest32, Header};
use ergo_lib::chain::parameters::{Parameter, Parameters};
use ergo_nipopow::NipopowAlgos;

use crate::sections::ParsedExtension;
use crate::ValidationError;

/// Extension field key prefix for interlink entries (JVM `NipopowAlgos`).
const INTERLINK_PREFIX: u8 = 0x01;

/// Rule 400 cap: JVM `MaxExtensionSize` = 32,768 bytes.
pub const MAX_EXTENSION_SIZE: usize = 32_768;

/// Rule 401: an interlink value is exactly this long (qty byte + 32-byte id).
const INTERLINK_VALUE_LEN: usize = 33;

/// Rule 404: no field's value may exceed this length.
const MAX_FIELD_VALUE_LEN: usize = 64;

fn rule_err(rule: u16, reason: impl Into<String>) -> ValidationError {
    ValidationError::BlockRule {
        rule,
        reason: reason.into(),
    }
}

/// Enforce the block rules JVM `ErgoStateContext.appendFullBlock` runs before
/// any state changes.
///
/// Inputs already parsed by the caller — the extension via
/// [`crate::parse_extension`], the parent extension by unpacking whatever the
/// store holds for the block at `header.height − 1`. This function does no
/// deserialization; it decides on the parsed structures and on the raw section
/// sizes.
///
/// `parent_header` is the header at `header.height − 1` (i.e.
/// `preceding_headers[0]`). It is only used by `update_interlinks` for the
/// level and the parent id. At genesis (`header.height == 1`) it is
/// meaningless, and the interlink rules are skipped (JVM `isGenesis`).
///
/// `parent_extension` mirrors the same block. When `None` on a non-genesis
/// block the interlink rules are skipped with a `warn!` naming the height —
/// the JVM's `exIlUnableToValidate` (rule 413), a recoverable rule that
/// passes on a missing previous extension (e.g. first block after a UTXO
/// snapshot bootstrap).
///
/// `active_params` are the parameters in effect BEFORE this block —
/// `chain.active_parameters()`, the JVM's pre-block `currentParameters`.
/// Rule 306 bounds the block's own transactions with them. That is deliberate,
/// and different from the transactions' validation parameters at a boundary
/// (§ "Parameters a block's transactions are validated with").
///
/// Callable outside `apply_state`; the mainnet real-history gate runs this
/// over every block. See `facts/validation.md` § "Block rules taken from
/// `appendFullBlock`".
pub fn validate_block_rules(
    header: &Header,
    parent_header: Option<&Header>,
    extension: &ParsedExtension,
    parent_extension: Option<&ParsedExtension>,
    block_txs_len: usize,
    extension_len: usize,
    active_params: &Parameters,
) -> Result<(), ValidationError> {
    // Rule 306 — BlockTransactions section size. Pre-block parameters.
    let max_block_size = active_params
        .parameters_table
        .get(&Parameter::MaxBlockSize)
        .copied()
        .ok_or_else(|| {
            ValidationError::StateOperationFailed(
                "active parameters have no MaxBlockSize entry".to_string(),
            )
        })?;
    let max_block_size_usize = usize::try_from(max_block_size).unwrap_or(0);
    if block_txs_len > max_block_size_usize {
        return Err(rule_err(
            306,
            format!(
                "block transactions section is {block_txs_len} bytes, max_block_size is \
                 {max_block_size_usize}"
            ),
        ));
    }

    // Rule 400 — Extension section size.
    if extension_len > MAX_EXTENSION_SIZE {
        return Err(rule_err(
            400,
            format!("extension is {extension_len} bytes, max is {MAX_EXTENSION_SIZE}"),
        ));
    }

    // Rule 406 — non-genesis blocks must carry at least one field.
    if header.height != 1 && extension.fields.is_empty() {
        return Err(rule_err(
            406,
            "non-genesis extension has no fields".to_string(),
        ));
    }

    // Rule 404 — no value longer than 64 bytes. Ordered before 401 because a
    // long interlink value must be attributed to 404, not 401 (see facts
    // § pitfall).
    for field in &extension.fields {
        if field.value.len() > MAX_FIELD_VALUE_LEN {
            return Err(rule_err(
                404,
                format!(
                    "field {} value is {} bytes, max is {MAX_FIELD_VALUE_LEN}",
                    hex::encode(field.key),
                    field.value.len()
                ),
            ));
        }
    }

    // Rule 405 — duplicate keys.
    let mut seen_keys: HashSet<[u8; 2]> = HashSet::with_capacity(extension.fields.len());
    for field in &extension.fields {
        if !seen_keys.insert(field.key) {
            return Err(rule_err(
                405,
                format!("duplicate key {}", hex::encode(field.key)),
            ));
        }
    }

    // Rule 401 — every interlink value is exactly 33 bytes.
    for field in &extension.fields {
        if field.key[0] == INTERLINK_PREFIX && field.value.len() != INTERLINK_VALUE_LEN {
            return Err(rule_err(
                401,
                format!(
                    "interlink field {} value is {} bytes, must be {INTERLINK_VALUE_LEN}",
                    hex::encode(field.key),
                    field.value.len()
                ),
            ));
        }
    }

    // Rule 402 — interlink structure. Genesis has no parent; a non-genesis
    // block with `parent_extension = None` skips 401/402 with a warning
    // (JVM's `exIlUnableToValidate` 413).
    if header.height == 1 {
        return Ok(());
    }
    let Some(parent_ext) = parent_extension else {
        tracing::warn!(
            height = header.height,
            "block rules: parent extension missing; skipping interlink checks \
             (JVM exIlUnableToValidate)"
        );
        return Ok(());
    };
    let parent_header = parent_header.ok_or_else(|| {
        ValidationError::StateOperationFailed(format!(
            "block rules: parent extension present but parent header missing at height {}",
            header.height
        ))
    })?;

    // Expected interlinks: unpack the parent's extension, then update against
    // the parent header. A parent-extension unpack failure or an
    // update_interlinks Err is a rule 402 (JVM's Try comparison in
    // ExtensionValidator).
    let parent_interlinks = unpack_interlinks_from_fields(parent_ext)
        .map_err(|reason| rule_err(402, format!("parent extension interlinks: {reason}")))?;
    let expected = NipopowAlgos::update_interlinks(parent_header.clone(), parent_interlinks)
        .map_err(|e| rule_err(402, format!("update_interlinks: {e}")))?;

    // Block's own unpacked interlinks. Rule 401 above already guarantees each
    // interlink value is 33 bytes, so this cannot fail on length; it can only
    // return a differently shaped Vec than expected.
    let actual = unpack_interlinks_from_fields(extension).map_err(|reason| {
        // A length error here means our 401 check missed something — better
        // to fail loud than silently mis-map to 402.
        rule_err(401, format!("block extension interlinks: {reason}"))
    })?;

    if actual != expected {
        return Err(rule_err(
            402,
            format!(
                "interlinks differ: expected {} entries, got {}",
                expected.len(),
                actual.len()
            ),
        ));
    }

    Ok(())
}

/// Unpack interlinks directly from parsed fields (avoids
/// `ExtensionCandidate::new`, whose 64-byte value cap would misattribute a
/// too-long interlink value from rule 401 to rule 404).
///
/// A pure iteration over `key[0] == 0x01` fields, mirroring
/// `NipopowAlgos::unpack_interlinks` on the field bytes.
fn unpack_interlinks_from_fields(
    extension: &ParsedExtension,
) -> Result<Vec<BlockId>, &'static str> {
    let mut out = Vec::new();
    for field in &extension.fields {
        if field.key[0] != INTERLINK_PREFIX {
            continue;
        }
        if field.value.len() != INTERLINK_VALUE_LEN {
            return Err("interlink value must be 33 bytes");
        }
        let qty = field.value[0];
        let mut id_bytes = [0u8; 32];
        id_bytes.copy_from_slice(&field.value[1..]);
        let block_id = BlockId(Digest32::from(id_bytes));
        for _ in 0..qty {
            out.push(block_id);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::{parse_extension, serialize_extension};
    use ergo_chain_types::Header;
    use sigma_ser::ScorexSerializable;

    // ── Mainnet fixtures (heights 1–3), fetched 2026-09-30 ─────────────────
    //
    // The interlink vectors are stated as observations from the wire, not
    // recomputed through `update_interlinks`:
    // - h1's extension is empty (fields = [])
    // - h2's interlinks = [genesis.id] (packed as one [0x01, 0x00] field)
    // - h3's interlinks = [genesis.id, h2.id × 5] (packed as [0x01, 0x00] +
    //   [0x01, 0x01])
    //
    // The header bytes below are the raw Scorex serialization of the mainnet
    // headers at those heights. Frozen so the test suite does not need the
    // node. Any h1/h2/h3 field the block rules care about (height, n_bits,
    // id, autolykos solution — for `max_level_of`) is inside these bytes.
    //
    // See `../TASKBOARD.md` § 3, "Mainnet facts".

    const GENESIS_ID_HEX: &str = "b0244dfc267baca974a4caee06120321562784303a8a688976ae56170e4d175b";
    const H2_ID_HEX: &str = "855fc5c9eed868b43ea2c3df99ec17dd9d903187d891e2365a89b98125c994b2";

    const HEADER_1_HEX: &str = "\
        010000000000000000000000000000000000000000000000000000000000000000\
        766ab7a313cd2fb66d135b0be6662aa02dfa8e5b17342c05a04396268df0bfbb\
        93fb06aa44413ff57ac878fda9377207d5db0e78833556b331b4d9727b3153ba\
        18b7a08878f2a7ee4389c5a1cece1e2724abe8b8adc8916240dd1bcac069177303\
        f1f6cee9ba2d\
        0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8\
        060117650100000003be7ad70c74f691345cbedba19f4844e7fc514e1188a7929f5ae261d5bb00bb66\
        02da9385ac99014ddcffe88d2ac5f28ce817cd615f270a0a5eae58acfb9fd9f6a0\
        000000030151dc63\
        1b7207d4420062aeb54e82b0cfb160ff6ace90ab7754f942c4c3266b";

    const HEADER_2_HEX: &str = "\
        01b0244dfc267baca974a4caee06120321562784303a8a688976ae56170e4d175b\
        828b0f6a0e6cb98ed4649c6e4cc00599ae78755324c79a8cec51e94ecca339d7\
        a3a11a92de9c0ba1e95068f39bc1e08afa4ca23dff16de135fac64d0cf7dd1ab\
        6291b70477f591ee8efb8a962d36ddbe3ac57591e39fe45ffb8c51c4939e419803\
        87d9cfe9ba2d\
        6b46bcba6f750f5be67d89679e921b78c277c5546a08cdb0955376fa0ea271e3\
        0601176502000000033c46c7fd7085638bf4bc902badb4e5a1942d3251d92d0eddd6fbe5d57e915537\
        03df646d7f6138aede718a2a4f1a76d4125750e8ab496b7a8a25292d07e14cbadb\
        0000000a03d0d019\
        1b06164a2e86a170f0d8ac96cffa2e3312f2f5b0b1c3b1e082b9a0cd";

    const HEADER_3_HEX: &str = "\
        01855fc5c9eed868b43ea2c3df99ec17dd9d903187d891e2365a89b98125c994b2\
        d80fc4ec24e7874760c6e42a8bce13791f15fe7f83d4cd055f614c25527a304b\
        4202efb982197ef2b6629c2202796584a7351bbc0563b27ed35c295e95021b94\
        7bb6a177e849e45ce5313ab7fa08e90daed00ceeadec13f271eea500df3e801303\
        e0a9d0e9ba2d\
        df4ff3b77824042f5c16a5da006c992258bd8574e8429b59cd02fc59ff0d22ce\
        060117650300000002d3a9410ac758ad45dfc85af8626efdacf398439c73977b13064aa8e6c8f2ac88\
        0255d213ecba5fd74e52002e08a69a2e5e08378f2e43fbbf3f1130dde976db3426\
        0000000900cb491a\
        1b9ac9dbcdf083bb80926012e041c623adb1ed964a80eb10bbb147ac";

    fn hex32(s: &str) -> [u8; 32] {
        hex::decode(s)
            .expect("fixture hex")
            .try_into()
            .expect("32 bytes")
    }

    fn parse_header(hex_str: &str) -> Header {
        let bytes = hex::decode(hex_str.replace(char::is_whitespace, "")).expect("fixture hex");
        Header::scorex_parse_bytes(&bytes).expect("fixture header parses")
    }

    fn header_h1() -> Header {
        parse_header(HEADER_1_HEX)
    }

    fn header_h2() -> Header {
        parse_header(HEADER_2_HEX)
    }

    fn header_h3() -> Header {
        parse_header(HEADER_3_HEX)
    }

    /// The mainnet-observed max level, pinned from a separate JVM-parity
    /// computation via `NipopowAlgos::max_level_of`. `update_interlinks(h2,
    /// [genesis])` needs level = 5 to yield `[genesis, h2 × 5]`; a wrong
    /// PoW solution in the fixture would silently pass 402 with a shorter
    /// vector and hide this whole rule.
    ///
    /// Pinned rather than recomputed through the code under test, so a
    /// future bug in `max_level_of` reads as a failed assertion here rather
    /// than as a silent "expected == actual" no-op elsewhere.
    #[test]
    fn h2_max_level_is_five() {
        assert_eq!(
            NipopowAlgos::default().max_level_of(&header_h2()).unwrap(),
            5,
            "h2's max level is a load-bearing fixture — level = 5 is why h3's \
             expected interlinks contain h2×5. If this changes the interlink \
             suite below is testing a different scenario."
        );
    }

    fn parse(fields: &[([u8; 2], Vec<u8>)]) -> ParsedExtension {
        let bytes = serialize_extension(&[0u8; 32], fields).expect("serialize");
        parse_extension(&bytes).expect("parse")
    }

    fn ext_len(fields: &[([u8; 2], Vec<u8>)]) -> usize {
        serialize_extension(&[0u8; 32], fields)
            .expect("serialize")
            .len()
    }

    /// Pack an interlink field: `key = [0x01, index]`, value = `[qty || id]`.
    fn interlink(index: u8, qty: u8, id_hex: &str) -> ([u8; 2], Vec<u8>) {
        let mut value = Vec::with_capacity(33);
        value.push(qty);
        value.extend_from_slice(&hex32(id_hex));
        ([0x01, index], value)
    }

    /// Convenience: parameters with a wide `MaxBlockSize` so rule 306 doesn't
    /// fire in tests unless we want it to.
    fn params_with_max_block_size(size: i32) -> Parameters {
        let mut p = Parameters::default();
        p.parameters_table.insert(Parameter::MaxBlockSize, size);
        p
    }

    // ── Rule 306 ────────────────────────────────────────────────────────

    #[test]
    fn rule_306_accepts_at_the_limit_and_rejects_one_byte_over() {
        // h1 extension is empty, and 306 uses pre-block active_params — a
        // legal exception to rule 406 (genesis) is the only shape that lets
        // us isolate 306 cleanly.
        let ext = parse(&[]);
        let params = params_with_max_block_size(1024);

        // At the limit: accepted.
        assert!(
            validate_block_rules(&header_h1(), None, &ext, None, 1024, ext_len(&[]), &params,)
                .is_ok()
        );

        // One byte over: rejected with 306.
        let err = validate_block_rules(&header_h1(), None, &ext, None, 1025, ext_len(&[]), &params)
            .expect_err("306 must fire");
        assert!(matches!(err, ValidationError::BlockRule { rule: 306, .. }));
    }

    // ── Rule 400 ────────────────────────────────────────────────────────

    #[test]
    fn rule_400_rejects_extension_over_32_768_bytes() {
        let ext = parse(&[]);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h1(),
            None,
            &ext,
            None,
            0,
            MAX_EXTENSION_SIZE + 1,
            &params,
        )
        .expect_err("400 must fire");
        assert!(matches!(err, ValidationError::BlockRule { rule: 400, .. }));

        // At the limit — accepted.
        assert!(validate_block_rules(
            &header_h1(),
            None,
            &ext,
            None,
            0,
            MAX_EXTENSION_SIZE,
            &params,
        )
        .is_ok());
    }

    // ── Rule 401 ────────────────────────────────────────────────────────

    #[test]
    fn rule_401_rejects_32_byte_interlink_value() {
        // 32 bytes, missing the leading qty byte: illegal 401 shape but
        // legal under 404 (≤ 64 bytes).
        let fields = vec![([0x01u8, 0x00], hex32(GENESIS_ID_HEX).to_vec())];
        let ext = parse(&fields);
        let len = ext_len(&fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h2(),
            Some(&header_h1()),
            &ext,
            Some(&parse(&[])),
            0,
            len,
            &params,
        )
        .expect_err("401 must fire");
        assert!(matches!(err, ValidationError::BlockRule { rule: 401, .. }));
    }

    #[test]
    fn rule_401_rejects_34_byte_interlink_value() {
        // 34 bytes: two extra bytes past the qty + id.
        let mut value = Vec::with_capacity(34);
        value.push(1);
        value.extend_from_slice(&hex32(GENESIS_ID_HEX));
        value.push(0xff);
        let fields = vec![([0x01u8, 0x00], value)];
        let ext = parse(&fields);
        let len = ext_len(&fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h2(),
            Some(&header_h1()),
            &ext,
            Some(&parse(&[])),
            0,
            len,
            &params,
        )
        .expect_err("401 must fire on 34-byte value");
        assert!(matches!(err, ValidationError::BlockRule { rule: 401, .. }));
    }

    // ── Rule 402 ────────────────────────────────────────────────────────

    #[test]
    fn rule_402_accepts_mainnet_h2_and_h3() {
        // h2 (parent = h1 with empty extension) must produce [genesis].
        // h3 (parent = h2 with [genesis]) must produce [genesis, h2×5].
        let params = params_with_max_block_size(1_000_000);

        // h2 fixture: one interlink for genesis.
        let h2_fields = vec![interlink(0x00, 1, GENESIS_ID_HEX)];
        let h2_ext = parse(&h2_fields);
        let h2_len = ext_len(&h2_fields);
        assert!(
            validate_block_rules(
                &header_h2(),
                Some(&header_h1()),
                &h2_ext,
                Some(&parse(&[])),
                0,
                h2_len,
                &params,
            )
            .is_ok(),
            "mainnet h2's real interlinks must pass 402"
        );

        // h3 fixture: [0x01,0x00]=[qty=1, genesis], [0x01,0x01]=[qty=5, h2].
        let h3_fields = vec![
            interlink(0x00, 1, GENESIS_ID_HEX),
            interlink(0x01, 5, H2_ID_HEX),
        ];
        let h3_ext = parse(&h3_fields);
        let h3_len = ext_len(&h3_fields);
        assert!(
            validate_block_rules(
                &header_h3(),
                Some(&header_h2()),
                &h3_ext,
                Some(&parse(&h2_fields)),
                0,
                h3_len,
                &params,
            )
            .is_ok(),
            "mainnet h3's real interlinks must pass 402"
        );
    }

    #[test]
    fn rule_402_rejects_a_one_id_difference() {
        // h3's interlinks are [genesis, h2×5]. Corrupt the second slot's id
        // to an arbitrary non-matching value.
        let bogus = "1111111111111111111111111111111111111111111111111111111111111111";
        let h3_fields = vec![
            interlink(0x00, 1, GENESIS_ID_HEX),
            interlink(0x01, 5, bogus),
        ];
        let h2_fields = vec![interlink(0x00, 1, GENESIS_ID_HEX)];
        let ext = parse(&h3_fields);
        let len = ext_len(&h3_fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h3(),
            Some(&header_h2()),
            &ext,
            Some(&parse(&h2_fields)),
            0,
            len,
            &params,
        )
        .expect_err("402 must fire on a wrong interlink id");
        assert!(matches!(err, ValidationError::BlockRule { rule: 402, .. }));
    }

    #[test]
    fn rule_402_rejects_empty_parent_interlinks_for_non_genesis_parent() {
        // Parent (h2) has an empty extension. `update_interlinks` returns
        // Err(EmptyInterlinks), which maps to 402.
        let h3_fields = vec![
            interlink(0x00, 1, GENESIS_ID_HEX),
            interlink(0x01, 5, H2_ID_HEX),
        ];
        let ext = parse(&h3_fields);
        let len = ext_len(&h3_fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h3(),
            Some(&header_h2()),
            &ext,
            Some(&parse(&[])),
            0,
            len,
            &params,
        )
        .expect_err("402 must fire on an empty parent interlink vector");
        assert!(matches!(err, ValidationError::BlockRule { rule: 402, .. }));
    }

    #[test]
    fn rule_402_rejects_parent_extension_that_wont_unpack() {
        // Parent's interlink field has a wrong-length value. The parent's
        // extension "won't unpack", which the JVM treats as a 402.
        //
        // The parent extension parses fine (value len ≤ 255); the
        // unpack-from-fields step is what fails.
        let mut bad_value = vec![1u8];
        bad_value.extend_from_slice(&[0u8; 30]); // 31 bytes total — not 33
        let bad_parent_fields = vec![([0x01u8, 0x00], bad_value)];
        let h3_fields = vec![
            interlink(0x00, 1, GENESIS_ID_HEX),
            interlink(0x01, 5, H2_ID_HEX),
        ];
        let ext = parse(&h3_fields);
        let len = ext_len(&h3_fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h3(),
            Some(&header_h2()),
            &ext,
            Some(&parse(&bad_parent_fields)),
            0,
            len,
            &params,
        )
        .expect_err("402 must fire on an unparseable parent extension");
        assert!(matches!(err, ValidationError::BlockRule { rule: 402, .. }));
    }

    // ── Rule 404 ────────────────────────────────────────────────────────

    #[test]
    fn rule_404_rejects_65_byte_value() {
        let fields = vec![([0x02u8, 0x00], vec![0u8; MAX_FIELD_VALUE_LEN + 1])];
        let ext = parse(&fields);
        let len = ext_len(&fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(&header_h1(), None, &ext, None, 0, len, &params)
            .expect_err("404 must fire on 65-byte value");
        assert!(matches!(err, ValidationError::BlockRule { rule: 404, .. }));
    }

    /// The pitfall: an interlink field with a value over 64 bytes must fire
    /// 404 (value length), not 401 (interlink structure). Rule 401 must not
    /// swallow long values, and rule 404 must run before 401 for that reason.
    #[test]
    fn rule_404_wins_over_401_for_a_long_interlink_value() {
        let fields = vec![([0x01u8, 0x00], vec![0u8; MAX_FIELD_VALUE_LEN + 1])];
        let ext = parse(&fields);
        let len = ext_len(&fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h2(),
            Some(&header_h1()),
            &ext,
            Some(&parse(&[])),
            0,
            len,
            &params,
        )
        .expect_err("404 wins over 401");
        match err {
            ValidationError::BlockRule { rule, .. } => assert_eq!(rule, 404),
            other => panic!("expected BlockRule 404, got {other:?}"),
        }
    }

    // ── Rule 405 ────────────────────────────────────────────────────────

    #[test]
    fn rule_405_rejects_duplicate_keys() {
        let fields = vec![([0x02u8, 0x00], vec![0xaa]), ([0x02u8, 0x00], vec![0xbb])];
        let ext = parse(&fields);
        let len = ext_len(&fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(&header_h1(), None, &ext, None, 0, len, &params)
            .expect_err("405 must fire on duplicate keys");
        assert!(matches!(err, ValidationError::BlockRule { rule: 405, .. }));
    }

    // ── Rule 406 ────────────────────────────────────────────────────────

    #[test]
    fn rule_406_rejects_empty_non_genesis_extension() {
        let ext = parse(&[]);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h2(),
            Some(&header_h1()),
            &ext,
            Some(&parse(&[])),
            0,
            ext_len(&[]),
            &params,
        )
        .expect_err("406 must fire on a non-genesis empty extension");
        assert!(matches!(err, ValidationError::BlockRule { rule: 406, .. }));
    }

    #[test]
    fn rule_406_exempts_genesis() {
        let ext = parse(&[]);
        let params = params_with_max_block_size(1_000_000);
        // h1 (genesis) with empty fields passes.
        assert!(
            validate_block_rules(&header_h1(), None, &ext, None, 0, ext_len(&[]), &params,).is_ok()
        );
    }

    // ── The `None` parent path (JVM exIlUnableToValidate) ───────────────

    #[test]
    fn none_parent_extension_skips_401_and_402_but_still_checks_the_rest() {
        // Well-formed extension with one non-interlink field.
        let fields = vec![([0x02u8, 0x00], vec![0xff])];
        let ext = parse(&fields);
        let len = ext_len(&fields);
        let params = params_with_max_block_size(1_000_000);

        // 401/402 skipped, other rules unaffected: this passes.
        assert!(validate_block_rules(
            &header_h2(),
            Some(&header_h1()),
            &ext,
            None, // <-- parent extension unavailable
            0,
            len,
            &params,
        )
        .is_ok());

        // ...but 405 still fires with the same None parent.
        let dup_fields = vec![([0x02u8, 0x00], vec![0xaa]), ([0x02u8, 0x00], vec![0xbb])];
        let dup_ext = parse(&dup_fields);
        let dup_len = ext_len(&dup_fields);
        let err = validate_block_rules(
            &header_h2(),
            Some(&header_h1()),
            &dup_ext,
            None,
            0,
            dup_len,
            &params,
        )
        .expect_err("405 must fire even with None parent extension");
        assert!(matches!(err, ValidationError::BlockRule { rule: 405, .. }));
    }

    /// Rule 401 is a property of the block's own extension, not of the
    /// parent, so the None-parent exemption must not silently accept a
    /// malformed interlink here.
    #[test]
    fn none_parent_extension_still_fires_401_for_the_block_s_own_interlinks() {
        let fields = vec![([0x01u8, 0x00], hex32(GENESIS_ID_HEX).to_vec())]; // 32 bytes
        let ext = parse(&fields);
        let len = ext_len(&fields);
        let params = params_with_max_block_size(1_000_000);
        let err = validate_block_rules(
            &header_h2(),
            Some(&header_h1()),
            &ext,
            None,
            0,
            len,
            &params,
        )
        .expect_err("401 must fire on a malformed own interlink");
        assert!(matches!(err, ValidationError::BlockRule { rule: 401, .. }));
    }
}

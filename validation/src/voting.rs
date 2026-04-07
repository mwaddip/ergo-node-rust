//! Soft-fork voting helpers — parse and pack blockchain parameters
//! from/into block extensions.
//!
//! Used by the validator at epoch-boundary blocks to verify that the
//! parameters in the block's extension match what the chain submodule
//! computed via `chain.compute_expected_parameters`. Used by mining when
//! assembling an epoch-boundary candidate block.
//!
//! The chain submodule owns the actual computation (vote tallying, soft-fork
//! lifecycle, etc.). This module is the in-repo glue for parsing the
//! `[0x00, param_id]` extension fields.

use ergo_lib::chain::parameters::{Parameter, Parameters};

use crate::sections::ParsedExtension;
use crate::ValidationError;

/// Parse blockchain parameters from an epoch-boundary block's extension.
///
/// Returns a fresh [`Parameters`] containing the entries decoded from the
/// extension fields. Used by the validator to verify the parsed parameters
/// match `chain.compute_expected_parameters(height)`.
///
/// Field encoding (mirrors JVM `Parameters.parseExtension`):
/// - Key prefix `0x00` identifies parameter table fields
/// - Second key byte is the signed parameter ID
/// - Value is a 4-byte big-endian `i32`
/// - ID 124 (`SoftForkDisablingRules`) has variable-length encoding —
///   skipped for first release per the chain submodule's contract
pub fn parse_parameters_from_extension(
    extension: &ParsedExtension,
) -> Result<Parameters, ValidationError> {
    // Start from the chain submodule's complete default and clear it.
    // We can't construct an empty `Parameters` directly because
    // `parameters_table` uses `hashbrown::HashMap` which isn't a direct
    // dep of this crate. Cloning + clearing gives us an empty map of
    // the right type.
    let mut params = Parameters::default();
    params.parameters_table.clear();

    for field in &extension.fields {
        if field.key[0] != 0x00 {
            continue; // not a system parameter field
        }

        let signed_id = field.key[1] as i8;

        // ID 124 (SoftForkDisablingRules) — variable-length encoding,
        // deferred per chain submodule's contract.
        if signed_id == 124 {
            continue;
        }

        if field.value.len() != 4 {
            return Err(ValidationError::SectionParse {
                section_type: 108,
                reason: format!(
                    "parameter id {signed_id} has wrong value length: expected 4, got {}",
                    field.value.len()
                ),
            });
        }

        let value = i32::from_be_bytes(field.value[..4].try_into().unwrap());

        if let Some(param) = signed_id_to_parameter(signed_id) {
            params.parameters_table.insert(param, value);
        }
        // Unknown IDs are skipped silently. Future protocol versions may
        // introduce new IDs that older clients shouldn't reject.
    }

    if params.parameters_table.is_empty() {
        return Err(ValidationError::SectionParse {
            section_type: 108,
            reason: "extension contains no parameter fields".to_string(),
        });
    }

    Ok(params)
}

/// Verify that an `expected` parameter table from chain matches a `parsed`
/// table from a block's extension, using JVM v6 `matchParameters60` semantics:
///
/// - `expected.size <= parsed.size` — the local table may be smaller because
///   newer protocol versions can introduce parameters that older clients
///   don't yet know about.
/// - For every entry `(param, value)` in `expected`, `parsed` must contain
///   the same value at the same key.
/// - Entries that exist only in `parsed` (newer params) are ignored.
///
/// **Does NOT check `proposedUpdate` (param ID 124)**. JVM v6 still requires
/// it to match, but this validator doesn't track it — the chain submodule
/// holds the active disabling rules separately. ID 124 enforcement is a
/// follow-up once the chain submodule exposes it.
///
/// Mirrors JVM `Parameters.matchParameters60`.
pub fn check_parameters_v6(
    expected: &Parameters,
    parsed: &Parameters,
    height: u32,
) -> Result<(), ValidationError> {
    if expected.parameters_table.len() > parsed.parameters_table.len() {
        tracing::warn!(
            height,
            expected_size = expected.parameters_table.len(),
            parsed_size = parsed.parameters_table.len(),
            "epoch-boundary parameter check: local table larger than received"
        );
        return Err(ValidationError::ParameterMismatch {
            height,
            expected: Box::new(expected.clone()),
            actual: Box::new(parsed.clone()),
        });
    }

    for (param, expected_value) in expected.parameters_table.iter() {
        match parsed.parameters_table.get(param) {
            Some(actual_value) if actual_value == expected_value => {}
            Some(actual_value) => {
                tracing::warn!(
                    height,
                    ?param,
                    expected_value,
                    actual_value,
                    "epoch-boundary parameter check: value mismatch"
                );
                return Err(ValidationError::ParameterMismatch {
                    height,
                    expected: Box::new(expected.clone()),
                    actual: Box::new(parsed.clone()),
                });
            }
            None => {
                tracing::warn!(
                    height,
                    ?param,
                    "epoch-boundary parameter check: param missing from received"
                );
                return Err(ValidationError::ParameterMismatch {
                    height,
                    expected: Box::new(expected.clone()),
                    actual: Box::new(parsed.clone()),
                });
            }
        }
    }

    Ok(())
}

/// Pack a [`Parameters`] table into extension key-value fields for
/// embedding in an epoch-boundary block's extension section.
///
/// Inverse of [`parse_parameters_from_extension`]. Used by the mining task
/// when assembling an epoch-boundary candidate block.
///
/// Output is sorted by parameter ID for deterministic field ordering
/// (mirrors JVM `Parameters.toExtensionCandidate`'s TreeMap iteration).
pub fn pack_parameters(params: &Parameters) -> Vec<([u8; 2], Vec<u8>)> {
    let mut entries: Vec<(i8, i32)> = params
        .parameters_table
        .iter()
        .filter_map(|(param, value)| {
            parameter_to_signed_id(*param).map(|id| (id, *value))
        })
        .collect();

    entries.sort_by_key(|(id, _)| *id);

    entries
        .into_iter()
        .map(|(id, value)| ([0x00u8, id as u8], value.to_be_bytes().to_vec()))
        .collect()
}

/// Map a signed parameter ID byte to its [`Parameter`] enum variant.
///
/// Returns `None` for unknown IDs. Negative IDs (decrease votes) are not
/// stored in `parametersTable` — they're vote slot values. This function
/// only matches positive IDs that correspond to actual table entries.
fn signed_id_to_parameter(id: i8) -> Option<Parameter> {
    match id {
        1 => Some(Parameter::StorageFeeFactor),
        2 => Some(Parameter::MinValuePerByte),
        3 => Some(Parameter::MaxBlockSize),
        4 => Some(Parameter::MaxBlockCost),
        5 => Some(Parameter::TokenAccessCost),
        6 => Some(Parameter::InputCost),
        7 => Some(Parameter::DataInputCost),
        8 => Some(Parameter::OutputCost),
        121 => Some(Parameter::SoftForkVotesCollected),
        122 => Some(Parameter::SoftForkStartingHeight),
        123 => Some(Parameter::BlockVersion),
        _ => None,
    }
}

/// Inverse of [`signed_id_to_parameter`].
fn parameter_to_signed_id(param: Parameter) -> Option<i8> {
    match param {
        Parameter::StorageFeeFactor => Some(1),
        Parameter::MinValuePerByte => Some(2),
        Parameter::MaxBlockSize => Some(3),
        Parameter::MaxBlockCost => Some(4),
        Parameter::TokenAccessCost => Some(5),
        Parameter::InputCost => Some(6),
        Parameter::DataInputCost => Some(7),
        Parameter::OutputCost => Some(8),
        Parameter::SoftForkVotesCollected => Some(121),
        Parameter::SoftForkStartingHeight => Some(122),
        Parameter::BlockVersion => Some(123),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::ExtensionField;

    fn make_field(key: [u8; 2], value: Vec<u8>) -> ExtensionField {
        ExtensionField { key, value }
    }

    fn make_extension(fields: Vec<ExtensionField>) -> ParsedExtension {
        ParsedExtension {
            header_id: [0u8; 32],
            fields,
        }
    }

    #[test]
    fn parse_single_param() {
        let ext = make_extension(vec![make_field(
            [0x00, 0x03],
            524_288i32.to_be_bytes().to_vec(),
        )]);
        let params = parse_parameters_from_extension(&ext).unwrap();
        assert_eq!(
            params.parameters_table.get(&Parameter::MaxBlockSize),
            Some(&524_288)
        );
        assert_eq!(params.parameters_table.len(), 1);
    }

    #[test]
    fn parse_skips_non_param_fields() {
        let ext = make_extension(vec![
            make_field([0x01, 0x00], b"interlink".to_vec()),
            make_field([0x00, 0x03], 524_288i32.to_be_bytes().to_vec()),
        ]);
        let params = parse_parameters_from_extension(&ext).unwrap();
        assert_eq!(params.parameters_table.len(), 1);
    }

    #[test]
    fn parse_skips_id_124() {
        let ext = make_extension(vec![
            make_field([0x00, 0x03], 524_288i32.to_be_bytes().to_vec()),
            make_field([0x00, 124], vec![0xde, 0xad, 0xbe, 0xef, 0x01]),
        ]);
        let params = parse_parameters_from_extension(&ext).unwrap();
        assert_eq!(params.parameters_table.len(), 1);
    }

    #[test]
    fn parse_rejects_empty_param_table() {
        let ext = make_extension(vec![make_field([0x01, 0x00], b"interlink".to_vec())]);
        let err = parse_parameters_from_extension(&ext).unwrap_err();
        match err {
            ValidationError::SectionParse { section_type, .. } => {
                assert_eq!(section_type, 108);
            }
            _ => panic!("expected SectionParse"),
        }
    }

    #[test]
    fn parse_rejects_wrong_value_length() {
        let ext = make_extension(vec![make_field(
            [0x00, 0x03],
            vec![0x01, 0x02, 0x03], // 3 bytes — wrong
        )]);
        let err = parse_parameters_from_extension(&ext).unwrap_err();
        match err {
            ValidationError::SectionParse { .. } => {}
            _ => panic!("expected SectionParse"),
        }
    }

    #[test]
    fn parse_handles_soft_fork_state() {
        let ext = make_extension(vec![
            make_field([0x00, 121], 100i32.to_be_bytes().to_vec()),
            make_field([0x00, 122], 1024i32.to_be_bytes().to_vec()),
        ]);
        let params = parse_parameters_from_extension(&ext).unwrap();
        assert_eq!(
            params.parameters_table.get(&Parameter::SoftForkVotesCollected),
            Some(&100)
        );
        assert_eq!(
            params.parameters_table.get(&Parameter::SoftForkStartingHeight),
            Some(&1024)
        );
    }

    #[test]
    fn pack_parameters_sorted_by_id() {
        let mut params = Parameters::default();
        params.parameters_table.clear();
        params.parameters_table.insert(Parameter::BlockVersion, 1);
        params.parameters_table.insert(Parameter::StorageFeeFactor, 1_250_000);
        params.parameters_table.insert(Parameter::MaxBlockSize, 524_288);

        let fields = pack_parameters(&params);
        assert_eq!(fields.len(), 3);
        // Sorted by ID: 1, 3, 123
        assert_eq!(fields[0].0, [0x00, 1]);
        assert_eq!(fields[1].0, [0x00, 3]);
        assert_eq!(fields[2].0, [0x00, 123]);
    }

    #[test]
    fn check_v6_exact_match() {
        let mut expected = Parameters::default();
        expected.parameters_table.clear();
        expected.parameters_table.insert(Parameter::StorageFeeFactor, 1_250_000);
        expected.parameters_table.insert(Parameter::BlockVersion, 4);

        let parsed = expected.clone();
        assert!(check_parameters_v6(&expected, &parsed, 128).is_ok());
    }

    #[test]
    fn check_v6_parsed_has_more_entries_ok() {
        // expected: 2 entries; parsed: 3 entries (one new param we don't know about).
        // v6 allows local to be smaller — newer protocol can introduce params.
        let mut expected = Parameters::default();
        expected.parameters_table.clear();
        expected.parameters_table.insert(Parameter::StorageFeeFactor, 1_250_000);
        expected.parameters_table.insert(Parameter::BlockVersion, 4);

        let mut parsed = expected.clone();
        parsed.parameters_table.insert(Parameter::MaxBlockSize, 524_288);

        assert!(check_parameters_v6(&expected, &parsed, 128).is_ok());
    }

    #[test]
    fn check_v6_expected_has_more_entries_fails() {
        // The reverse: expected has more than parsed. Block is missing a
        // parameter we expected — consensus failure.
        let mut expected = Parameters::default();
        expected.parameters_table.clear();
        expected.parameters_table.insert(Parameter::StorageFeeFactor, 1_250_000);
        expected.parameters_table.insert(Parameter::BlockVersion, 4);
        expected.parameters_table.insert(Parameter::MaxBlockSize, 524_288);

        let mut parsed = Parameters::default();
        parsed.parameters_table.clear();
        parsed.parameters_table.insert(Parameter::StorageFeeFactor, 1_250_000);
        parsed.parameters_table.insert(Parameter::BlockVersion, 4);

        let err = check_parameters_v6(&expected, &parsed, 128).unwrap_err();
        assert!(matches!(err, ValidationError::ParameterMismatch { .. }));
    }

    #[test]
    fn check_v6_value_mismatch_fails() {
        let mut expected = Parameters::default();
        expected.parameters_table.clear();
        expected.parameters_table.insert(Parameter::BlockVersion, 1);

        let mut parsed = Parameters::default();
        parsed.parameters_table.clear();
        parsed.parameters_table.insert(Parameter::BlockVersion, 4);

        let err = check_parameters_v6(&expected, &parsed, 128).unwrap_err();
        assert!(matches!(err, ValidationError::ParameterMismatch { .. }));
    }

    #[test]
    fn check_v6_missing_key_fails() {
        // expected has BlockVersion but parsed doesn't (and parsed is somehow
        // not smaller — has different keys filling the slot).
        let mut expected = Parameters::default();
        expected.parameters_table.clear();
        expected.parameters_table.insert(Parameter::BlockVersion, 4);

        let mut parsed = Parameters::default();
        parsed.parameters_table.clear();
        parsed.parameters_table.insert(Parameter::StorageFeeFactor, 1_250_000);

        let err = check_parameters_v6(&expected, &parsed, 128).unwrap_err();
        assert!(matches!(err, ValidationError::ParameterMismatch { .. }));
    }

    #[test]
    fn round_trip_parse_pack() {
        let mut original = Parameters::default();
        original.parameters_table.clear();
        original.parameters_table.insert(Parameter::StorageFeeFactor, 1_250_000);
        original.parameters_table.insert(Parameter::MaxBlockSize, 524_288);
        original.parameters_table.insert(Parameter::MaxBlockCost, 1_000_000);
        original.parameters_table.insert(Parameter::BlockVersion, 4);

        let packed = pack_parameters(&original);
        let fields: Vec<ExtensionField> = packed
            .into_iter()
            .map(|(key, value)| ExtensionField { key, value })
            .collect();
        let ext = make_extension(fields);
        let parsed = parse_parameters_from_extension(&ext).unwrap();

        assert_eq!(parsed.parameters_table.len(), original.parameters_table.len());
        for (k, v) in original.parameters_table.iter() {
            assert_eq!(parsed.parameters_table.get(k), Some(v));
        }
    }
}

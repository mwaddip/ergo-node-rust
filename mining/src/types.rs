use std::time::{Duration, Instant};

use ergo_chain_types::{ADDigest, Header};
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use serde::Serialize;

use crate::emission::ReemissionRules;

/// Miner configuration loaded from node config.
#[derive(Clone)]
pub struct MinerConfig {
    /// Miner's public key (required for mining).
    pub miner_pk: ProveDlog,
    /// Miner reward maturity delay in blocks (720 on mainnet/testnet).
    pub reward_delay: i32,
    /// Voting preferences: 3 bytes [soft_fork, param_1, param_2].
    pub votes: [u8; 3],
    /// Maximum candidate lifetime before forced regeneration.
    pub candidate_ttl: Duration,
    /// EIP-27 re-emission rules. Carried in config rather than derived
    /// inside `generate_candidate` so the network-policy decision lives
    /// at the configuration boundary, not inside the assembly path.
    /// Construct from the chain's network type at config-load time.
    pub reemission_rules: ReemissionRules,
}

/// Extension section key-value pairs for a new block.
#[derive(Clone)]
pub struct ExtensionCandidate {
    /// Fields as (2-byte key, variable-length value).
    pub fields: Vec<([u8; 2], Vec<u8>)>,
}

/// All components needed to assemble a full block once a PoW solution arrives.
#[derive(Clone)]
pub struct CandidateBlock {
    /// Parent block header.
    pub parent: Header,
    /// Block version.
    pub version: u8,
    /// Encoded difficulty target (compact bits).
    pub n_bits: u32,
    /// New state root after applying selected transactions.
    pub state_root: ADDigest,
    /// Serialized AD proofs for the state transition.
    pub ad_proof_bytes: Vec<u8>,
    /// Ordered transactions: [emission_tx, mempool_txs..., fee_tx].
    pub transactions: Vec<Transaction>,
    /// Block timestamp: max(now_ms, parent.timestamp + 1).
    pub timestamp: u64,
    /// Extension section (interlinks + voting).
    pub extension: ExtensionCandidate,
    /// Voting bytes (3 bytes).
    pub votes: [u8; 3],
    /// Serialized header-without-PoW bytes (cached for WorkMessage).
    pub header_bytes: Vec<u8>,
}

/// Data sent to the miner. The miner finds nonce n such that pow_hit(msg, n, h) < b.
#[derive(Clone, Serialize)]
pub struct WorkMessage {
    /// Blake2b256(serialized HeaderWithoutPow) — hex-encoded.
    pub msg: String,
    /// Target value from nBits. Held as a decimal string internally, but
    /// serialized as a BARE JSON number to match JVM `ExternalCandidateBlock`
    /// (jsmn-based miner/pool parsers reject a quoted target). See
    /// `serialize_b_as_number`.
    #[serde(serialize_with = "serialize_b_as_number")]
    pub b: String,
    /// Block height.
    pub h: u32,
    /// Miner public key — hex-encoded compressed point.
    pub pk: String,
    /// Header pre-image and tx-membership proofs for miner verification.
    ///
    /// OPTIONAL: OMITTED from the JSON entirely (not `null`) when there are
    /// no mandatory transaction proofs. Mirrors the JVM `WorkMessage` encoder
    /// (`mining/WorkMessage.scala:27-39`), which builds the object then
    /// `.collect { case (name, Some(value)) => ... }` — dropping a `None`
    /// proof rather than emitting it. The basic `/mining/candidate` path
    /// always sets this `None` (the block carries only the emission tx, so
    /// there are no mandatory-tx proofs); a future `candidateWithTxs` path
    /// will set `Some`. Emitting a nested proof object unconditionally
    /// inflated the basic candidate past the reference Autolykos2 miner's
    /// fixed jsmn `REQ_LEN=11` token buffer → "Jsmn failed to parse latest
    /// block" and the miner never mines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<ProofOfUpcomingTransactions>,
}

/// Serialize the decimal target `b` as a bare (unquoted) JSON number.
///
/// `b` ranges up to the secp256k1 group order (~2^256), so it fits neither
/// `u64` nor `u128` and cannot go through serde's numeric data model. We wrap
/// the decimal digits in a `serde_json::value::RawValue`, which emits them
/// verbatim — yielding the arbitrary-precision number token the JVM
/// `ExternalCandidateBlock` produces and JVM-compatible miner/pool parsers
/// require. RawValue's verbatim emission only applies through serde_json's
/// serializer, which is the only serializer WorkMessage ever sees (axum's
/// `Json` in production, `serde_json::to_string` in tests).
///
/// `b` is always the decimal string from `decode_compact_bits(..).to_string()`
/// (a non-negative `BigInt`: valid JSON, no leading zeros, no sign), so the
/// RawValue construction never fails in practice — but we surface it as a
/// serialization error rather than panic if that invariant is ever violated.
fn serialize_b_as_number<S>(b: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let raw = serde_json::value::RawValue::from_string(b.to_owned())
        .map_err(serde::ser::Error::custom)?;
    raw.serialize(serializer)
}

#[derive(Clone, Serialize)]
pub struct ProofOfUpcomingTransactions {
    /// Serialized header-without-PoW — hex-encoded.
    #[serde(rename = "msgPreimage")]
    pub msg_preimage: String,
    /// Merkle proofs for mandatory txs (empty for first release).
    #[serde(rename = "txProofs")]
    pub tx_proofs: Vec<()>,
}

/// Cached candidate with metadata for invalidation.
pub struct CachedCandidate {
    pub block: CandidateBlock,
    pub work: WorkMessage,
    pub tip_height: u32,
    pub created: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A basic candidate WorkMessage (proof `None` — the only shape the node
    /// produces today), parameterized by the target `b`.
    fn work_with_b(b: &str) -> WorkMessage {
        WorkMessage {
            msg: "00".repeat(32),
            b: b.to_string(),
            h: 271_235,
            pk: format!("02{}", "11".repeat(32)),
            proof: None,
        }
    }

    #[test]
    fn b_serializes_as_bare_number() {
        let json = serde_json::to_string(&work_with_b("12237864960")).unwrap();
        assert!(
            json.contains(r#""b":12237864960"#),
            "b must be a bare JSON number, got: {json}"
        );
        assert!(
            !json.contains(r#""b":"12237864960""#),
            "b must NOT be a quoted string, got: {json}"
        );
    }

    #[test]
    fn b_preserves_arbitrary_precision_beyond_u64() {
        // ~7.5e62 (≈ 2^209): exceeds both u64 (~2e19) and u128 (~3.4e38),
        // proving the value survives without lossy integer coercion. This is
        // the JVM LiteClientExamples target.
        let huge = "748014723576678314041035877227113663879264849498014394977645987";
        let json = serde_json::to_string(&work_with_b(huge)).unwrap();
        assert!(
            json.contains(&format!(r#""b":{huge}"#)),
            "arbitrary-precision b must survive verbatim, got: {json}"
        );
        assert!(
            !json.contains(&format!(r#""b":"{huge}""#)),
            "b must not be quoted, got: {json}"
        );
    }

    #[test]
    fn surviving_fields_keep_their_shape() {
        // msg/pk stay quoted hex strings, h stays a bare number. (proof
        // omission is asserted separately in basic_candidate_omits_proof.)
        let json = serde_json::to_string(&work_with_b("12237864960")).unwrap();
        assert!(json.contains(r#""msg":"0000"#), "msg must stay a hex string: {json}");
        assert!(json.contains(r#""pk":"0211"#), "pk must stay a hex string: {json}");
        assert!(json.contains(r#""h":271235"#), "h must stay a bare number: {json}");
    }

    #[test]
    fn basic_candidate_omits_proof() {
        // The basic candidate is {msg, b, h, pk} — the JVM `WorkMessage`
        // encoder drops a `None` proof, and the reference Autolykos2 miner
        // parses with a fixed jsmn REQ_LEN=11 token buffer. A nested proof
        // object overflows it ("Jsmn failed to parse latest block"), so the
        // `proof` key must be ABSENT, not `null`.
        let json = serde_json::to_string(&work_with_b("12237864960")).unwrap();

        assert!(
            !json.contains("proof"),
            "basic candidate must omit the proof key entirely, got: {json}"
        );
        assert!(
            !json.contains("msgPreimage"),
            "no nested proof preimage on the basic candidate: {json}"
        );
        assert!(
            !json.contains("null"),
            "proof must be OMITTED, not serialized as null: {json}"
        );

        // The four expected keys are present, b still bare.
        assert!(json.contains(r#""msg":"#), "msg present: {json}");
        assert!(json.contains(r#""b":12237864960"#), "b present and bare: {json}");
        assert!(json.contains(r#""h":271235"#), "h present: {json}");
        assert!(json.contains(r#""pk":"#), "pk present: {json}");

        // Token-count sanity (nice-to-have): the candidate must fit the
        // miner's REQ_LEN=11 jsmn buffer. jsmn counts one token for the
        // object + one per key + one per scalar value = 1 + 4 + 4 = 9 <= 11.
        // Assert exactly four scalar fields and no nesting.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().expect("WorkMessage serializes to a JSON object");
        assert_eq!(obj.len(), 4, "basic candidate must have exactly 4 keys: {json}");
        assert!(
            obj.values().all(|v| !v.is_object() && !v.is_array()),
            "no nested objects/arrays in the basic candidate: {json}"
        );
    }
}

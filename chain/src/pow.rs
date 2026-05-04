use ergo_chain_types::autolykos_pow_scheme::{
    decode_compact_bits, order_bigint, AutolykosPowScheme,
};
use ergo_chain_types::Header;
use num_bigint::ToBigInt;

use crate::error::ChainError;

/// Lazily initialized PoW scheme with Ergo's standard parameters (k=32, N=2^26).
fn pow_scheme() -> &'static AutolykosPowScheme {
    use std::sync::OnceLock;
    static POW: OnceLock<AutolykosPowScheme> = OnceLock::new();
    POW.get_or_init(AutolykosPowScheme::default)
}

/// Verify that a header's proof of work is valid for its claimed difficulty.
///
/// Computes `pow_hit(header)` and checks that it is strictly less than
/// `q / decode_compact_bits(header.n_bits)`, where `q` is the secp256k1 group order.
///
/// Returns `Ok(())` if valid, `Err(ChainError::PowInvalid)` if the hit doesn't
/// meet the target, or `Err(ChainError::PowCompute)` if the hit can't be computed.
pub fn verify_pow(header: &Header) -> Result<(), ChainError> {
    let pow = pow_scheme();
    let hit = pow.pow_hit(header)?;

    let decoded_n_bits = decode_compact_bits(header.n_bits);
    let target = order_bigint() / decoded_n_bits;

    let hit_bigint = hit
        .to_bigint()
        .ok_or(ChainError::PowInvalid {
            hit: format!("{hit}"),
            target: format!("{target}"),
        })?;

    if hit_bigint < target {
        Ok(())
    } else {
        Err(ChainError::PowInvalid {
            hit: format!("{hit}"),
            target: format!("{target}"),
        })
    }
}

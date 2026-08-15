use ergo_chain_types::autolykos_pow_scheme::{
    decode_compact_bits, order_bigint, AutolykosPowScheme,
};
use ergo_chain_types::Header;
use num_bigint::{BigInt, ToBigInt};

use crate::error::ChainError;

/// Lazily initialized PoW scheme with Ergo's standard parameters (k=32, N=2^26).
fn pow_scheme() -> &'static AutolykosPowScheme {
    use std::sync::OnceLock;
    static POW: OnceLock<AutolykosPowScheme> = OnceLock::new();
    POW.get_or_init(AutolykosPowScheme::default)
}

/// The Autolykos mining target for a header's compact difficulty encoding.
///
/// `q / decode_compact_bits(n_bits)`, where `q` is the secp256k1 group order.
/// A `pow_hit` is valid when it falls **strictly below** this value.
///
/// ⚠ **`decode_compact_bits` returns the difficulty, not the target.** The name
/// invites the opposite reading — it sounds like it yields the number you compare
/// a hash against — and every other consumer in this crate binds it to a variable
/// called `difficulty`, which is correct: chain work accumulates the difficulty,
/// undivided. Only the PoW comparison takes the `q /` division, and only through
/// this function.
///
/// This exists because the division was missed once. `mining/` re-derived the
/// target from `decode_compact_bits` for the `WorkMessage.b` field it serves to
/// external miners and stopped one operation short, handing them a target tens of
/// orders of magnitude too hard. A GPU miner ran nine minutes at 143 MH/s and
/// submitted zero shares, while this crate's `verify_pow` — computing the target
/// correctly — stood ready to accept a block nobody could find. Serve-side and
/// verify-side now resolve to the same number by construction. **Do not inline
/// `order_bigint() / decode_compact_bits(..)` at a new call site; call this.**
///
/// See `../facts/chain.md` § "Phase 2: PoW Verification" for the full account.
pub fn pow_target(n_bits: u32) -> BigInt {
    order_bigint() / decode_compact_bits(n_bits)
}

/// Verify that a header's proof of work is valid for its claimed difficulty.
///
/// Computes `pow_hit(header)` and checks that it is strictly less than
/// [`pow_target`] for the header's `n_bits`.
///
/// Returns `Ok(())` if valid, `Err(ChainError::PowInvalid)` if the hit doesn't
/// meet the target, `Err(ChainError::InvalidPowTarget)` if the compact target
/// decodes to zero, or `Err(ChainError::PowCompute)` if the hit can't be computed.
pub fn verify_pow(header: &Header) -> Result<(), ChainError> {
    let pow = pow_scheme();
    let hit = pow.pow_hit(header)?;

    let decoded_n_bits = decode_compact_bits(header.n_bits);
    if decoded_n_bits == 0.into() {
        return Err(ChainError::InvalidPowTarget {
            n_bits: header.n_bits,
        });
    }
    let target = pow_target(header.n_bits);

    let hit_bigint = hit.to_bigint().ok_or(ChainError::PowInvalid {
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

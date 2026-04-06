//! Emission transaction construction and EIP-27 re-emission rules.

use ergo_lib::chain::emission::{EmissionRules, MonetarySettings};
use ergo_lib::chain::ergo_box::box_builder::ErgoBoxCandidateBuilder;
use ergo_lib::chain::ergo_tree_predef;
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::{Input, Transaction};
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::chain::token::{Token, TokenAmount};
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;

use crate::MiningError;

/// nanoERG per ERG
const COINS_IN_ONE_ERG: i64 = 1_000_000_000;

/// EIP-27 re-emission schedule.
///
/// After the initial emission period, miners receive additional ERG from
/// the re-emission contract. The amount depends on the total emission at
/// the current height (NOT the miner reward — the total including foundation).
///
/// Reference: JVM `ReemissionRules.reemissionForHeight()`.
pub struct ReemissionRules {
    /// Height at which re-emission activates (mainnet: 777_217).
    pub activation_height: u32,
    /// Fixed re-emission when total emission >= threshold (12 ERG).
    pub basic_charge_amount: i64,
}

impl ReemissionRules {
    /// Mainnet EIP-27 parameters.
    pub fn mainnet() -> Self {
        Self {
            activation_height: 777_217,
            basic_charge_amount: 12,
        }
    }

    /// Compute re-emission amount at a given height.
    ///
    /// `total_emission` is the total emission in nanoERG at this height
    /// (from `EmissionRules::emission_at_height`, which includes both
    /// miner reward and foundation share).
    ///
    /// Returns additional nanoERG from re-emission, or 0 if not active.
    pub fn reemission_for_height(&self, height: u32, total_emission: i64) -> i64 {
        if height < self.activation_height {
            return 0;
        }

        let charge = self.basic_charge_amount * COINS_IN_ONE_ERG;
        let floor = 3 * COINS_IN_ONE_ERG;

        if total_emission >= charge + floor {
            // High emission period: fixed charge
            charge
        } else if total_emission > floor {
            // Tapering: emission minus 3 ERG floor
            total_emission - floor
        } else {
            0
        }
    }
}

/// Build the emission (coinbase) transaction for a new block.
///
/// Spends the current emission box and creates:
/// - Output 0: new emission box with value reduced by miner_reward, same ErgoTree
/// - Output 1: miner reward box with time-locked reward_output_script
///
/// Post-EIP-27: reemission tokens are transferred from the emission box to the
/// miner reward box. The emission box's token[1] amount decreases; the miner box
/// receives that amount as tokens.
///
/// Reference: JVM `CandidateGenerator.collectRewards()`.
pub fn build_emission_tx(
    emission_box: &ErgoBox,
    height: u32,
    miner_pk: &ProveDlog,
    reward_delay: i32,
    reemission_rules: &ReemissionRules,
) -> Result<Transaction, MiningError> {
    let emission_rules = EmissionRules::new(MonetarySettings::default());
    let miner_reward = emission_rules.miners_reward_at_height(height as i64);

    if miner_reward <= 0 {
        return Err(MiningError::Emission("no emission remaining".into()));
    }

    // New emission box value: old value minus standard miner reward
    let new_emission_value = emission_box.value.as_i64() - miner_reward;
    if new_emission_value < 0 {
        return Err(MiningError::Emission(format!(
            "emission box value {} < reward {}",
            emission_box.value.as_i64(),
            miner_reward
        )));
    }

    // Compute re-emission amount (EIP-27)
    let total_emission = emission_rules.emission_at_height(height as i64);
    let reemission_amount = reemission_rules.reemission_for_height(height, total_emission);

    // --- Input: emission box with empty proof (self-validating contract) ---
    let input = Input::new(
        emission_box.box_id(),
        ProverResult {
            proof: ProofBytes::Empty,
            extension: ContextExtension::empty(),
        },
    );

    // --- Output 0: new emission box ---
    let mut new_emission_builder = ErgoBoxCandidateBuilder::new(
        (new_emission_value as u64)
            .try_into()
            .map_err(|e| MiningError::Emission(format!("emission box value: {e}")))?,
        emission_box.ergo_tree.clone(),
        height,
    );

    // Preserve tokens, decrementing reemission token (index 1) if EIP-27 active
    if let Some(ref tokens) = emission_box.tokens {
        let token_slice: &[Token] = tokens.as_ref();
        for (i, token) in token_slice.iter().enumerate() {
            if i == 1 && reemission_amount > 0 {
                let remaining = i64::from(token.amount) - reemission_amount;
                if remaining > 0 {
                    new_emission_builder.add_token(Token {
                        token_id: token.token_id.clone(),
                        amount: TokenAmount::try_from(remaining as u64).map_err(|e| {
                            MiningError::Emission(format!("reemission token amount: {e}"))
                        })?,
                    });
                }
                // If remaining <= 0, the reemission token is exhausted — don't add it
            } else {
                new_emission_builder.add_token(token.clone());
            }
        }
    }

    let new_emission_candidate = new_emission_builder
        .build()
        .map_err(|e| MiningError::Emission(format!("emission box build: {e}")))?;

    // --- Output 1: miner reward box (time-locked) ---
    let reward_script =
        ergo_tree_predef::reward_output_script(reward_delay, miner_pk.clone())
            .map_err(|e| MiningError::Emission(format!("reward script: {e}")))?;

    let mut reward_builder = ErgoBoxCandidateBuilder::new(
        (miner_reward as u64)
            .try_into()
            .map_err(|e| MiningError::Emission(format!("reward value: {e}")))?,
        reward_script,
        height,
    );

    // Transfer reemission tokens to miner reward box
    if reemission_amount > 0 {
        if let Some(ref tokens) = emission_box.tokens {
            let token_slice: &[Token] = tokens.as_ref();
            if token_slice.len() >= 2 {
                reward_builder.add_token(Token {
                    token_id: token_slice[1].token_id.clone(),
                    amount: TokenAmount::try_from(reemission_amount as u64).map_err(|e| {
                        MiningError::Emission(format!("miner reemission token: {e}"))
                    })?,
                });
            }
        }
    }

    let reward_candidate = reward_builder
        .build()
        .map_err(|e| MiningError::Emission(format!("reward box build: {e}")))?;

    // --- Assemble transaction ---
    Transaction::new_from_vec(
        vec![input],
        vec![],
        vec![new_emission_candidate, reward_candidate],
    )
    .map_err(|e| MiningError::Emission(format!("transaction: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_reemission_before_activation() {
        let rules = ReemissionRules::mainnet();
        assert_eq!(rules.reemission_for_height(777_216, 67_500_000_000), 0);
        assert_eq!(rules.reemission_for_height(0, 75_000_000_000), 0);
    }

    #[test]
    fn reemission_high_emission() {
        let rules = ReemissionRules::mainnet();
        // Total emission 67.5 ERG (well above 15 ERG threshold) → 12 ERG
        assert_eq!(
            rules.reemission_for_height(777_217, 67_500_000_000),
            12_000_000_000
        );
    }

    #[test]
    fn reemission_at_threshold() {
        let rules = ReemissionRules::mainnet();
        // Total emission exactly 15 ERG → 12 ERG (>= threshold)
        assert_eq!(
            rules.reemission_for_height(800_000, 15_000_000_000),
            12_000_000_000
        );
    }

    #[test]
    fn reemission_tapering() {
        let rules = ReemissionRules::mainnet();
        // Total emission 6 ERG (between 3 and 15) → 6 - 3 = 3 ERG
        assert_eq!(
            rules.reemission_for_height(1_500_000, 6_000_000_000),
            3_000_000_000
        );
        // Total emission 4 ERG → 4 - 3 = 1 ERG
        assert_eq!(
            rules.reemission_for_height(2_000_000, 4_000_000_000),
            1_000_000_000
        );
    }

    #[test]
    fn reemission_at_floor() {
        let rules = ReemissionRules::mainnet();
        // Total emission exactly 3 ERG → 0 (not > floor)
        assert_eq!(
            rules.reemission_for_height(2_500_000, 3_000_000_000),
            0
        );
        // Total emission below floor → 0
        assert_eq!(
            rules.reemission_for_height(2_500_000, 2_000_000_000),
            0
        );
    }

    #[test]
    fn reemission_matches_jvm_formula() {
        // Cross-verify: JVM formula is
        //   if emission >= (12 + 3) * 10^9 → 12 * 10^9
        //   elif emission > 3 * 10^9 → emission - 3 * 10^9
        //   else → 0
        let rules = ReemissionRules::mainnet();
        let h = 800_000;

        // Just above high threshold
        assert_eq!(
            rules.reemission_for_height(h, 15_000_000_001),
            12_000_000_000
        );
        // Just below high threshold (14.999... ERG) → tapering
        assert_eq!(
            rules.reemission_for_height(h, 14_999_999_999),
            14_999_999_999 - 3_000_000_000
        );
        // Just above floor
        assert_eq!(
            rules.reemission_for_height(h, 3_000_000_001),
            1
        );
    }
}

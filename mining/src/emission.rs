//! Emission transaction construction and EIP-27 re-emission rules.

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

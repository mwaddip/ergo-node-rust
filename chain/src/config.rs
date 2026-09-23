use ergo_chain_types::BlockId;

use crate::voting::VotingConfig;

/// Network identity for the chain.
///
/// Determines starting parameters (block version, soft-fork activation
/// state) that differ between deployments. Mainnet started at v1 and
/// progressed via voting; testnet was created post-6.0 and starts at v4.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Network {
    /// Production network.
    Mainnet,
    /// Testnet (the post-6.0 incarnation, starts at protocol v4).
    Testnet,
    /// Private development network (starts at protocol v4, difficulty 1,
    /// epoch boundaries out of reach). See `ChainConfig::devnet`.
    Devnet,
}

/// Network parameters that affect header chain validation.
///
/// Determines epoch length, block interval, difficulty adjustment behavior,
/// and whether EIP-37 applies. Passed to `HeaderChain` at construction.
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// Network identity (selects starting parameter defaults).
    pub network: Network,
    /// Blocks per difficulty epoch.
    pub epoch_length: u32,
    /// Target time between blocks, in milliseconds.
    pub block_interval_ms: u64,
    /// Number of past epochs used in difficulty recalculation.
    pub use_last_epochs: u32,
    /// Difficulty for the very first block (encoded as nBits).
    pub initial_n_bits: u32,
    /// nBits override at the Autolykos v2 activation height (mainnet only).
    /// Applied for two blocks at the PoW algorithm transition.
    pub version2_activation_n_bits: Option<u32>,
    /// Expected genesis block header ID. If set, `validate_genesis()` rejects
    /// headers whose ID doesn't match.
    pub genesis_id: Option<BlockId>,
    /// Maximum allowed clock drift for timestamps, in milliseconds.
    /// Headers with `timestamp > now + max_time_drift` are rejected.
    pub max_time_drift_ms: u64,
    /// Height at which EIP-37 difficulty adjustment activates (mainnet only).
    /// `None` means EIP-37 is never active (testnet).
    pub eip37_activation_height: Option<u32>,
    /// Epoch length after EIP-37 activation (mainnet: 128).
    pub eip37_epoch_length: Option<u32>,
    /// Soft-fork voting parameters (Phase 6).
    pub voting: VotingConfig,
}

impl ChainConfig {
    /// Testnet configuration.
    pub fn testnet() -> Self {
        Self {
            network: Network::Testnet,
            epoch_length: 128,
            block_interval_ms: 45_000,
            use_last_epochs: 8,
            initial_n_bits: 16842752, // encode_compact_bits(1)
            version2_activation_n_bits: None,
            genesis_id: None,
            max_time_drift_ms: 10 * 45_000, // 450 seconds
            eip37_activation_height: None,
            eip37_epoch_length: None,
            voting: VotingConfig::testnet(),
        }
    }

    /// Devnet configuration: the testnet chain with difficulty pinned at 1
    /// by an unreachable difficulty epoch (`1 << 25` blocks) and a 20 s
    /// block interval; the forced v2 activation stays testnet's (never
    /// reached). The Scala side is `networkType = "devnet60"` with
    /// `epochLength = 33554432`, `blockInterval = 20s`,
    /// `initialDifficultyHex = "01"`; the mixed Scala/Rust devnet of
    /// arkadianet/ergo (`scripts/devnet-mixed/genesis.conf`) uses this
    /// profile.
    pub fn devnet() -> Self {
        Self {
            network: Network::Devnet,
            epoch_length: 1 << 25,
            block_interval_ms: 20_000,
            max_time_drift_ms: 10 * 20_000,
            voting: VotingConfig::devnet(),
            ..Self::testnet()
        }
    }

    /// Mainnet configuration.
    pub fn mainnet() -> Self {
        Self {
            network: Network::Mainnet,
            epoch_length: 1024,
            block_interval_ms: 120_000,
            use_last_epochs: 8,
            initial_n_bits: 100_734_821, // encode_compact_bits(BigInt(1199990374400))
            version2_activation_n_bits: Some(107_976_917), // encode_compact_bits(BigInt(122702199259136))
            genesis_id: Some(
                "b0244dfc267baca974a4caee06120321562784303a8a688976ae56170e4d175b"
                    .parse()
                    .expect("mainnet genesis ID is valid hex"),
            ),
            max_time_drift_ms: 10 * 120_000, // 1200 seconds
            eip37_activation_height: Some(844_673),
            eip37_epoch_length: Some(128),
            voting: VotingConfig::mainnet(),
        }
    }

    /// Returns the effective epoch length for a given height.
    pub fn epoch_length_at(&self, height: u32) -> u32 {
        match (self.eip37_activation_height, self.eip37_epoch_length) {
            (Some(activation), Some(eip37_len)) if height >= activation => eip37_len,
            _ => self.epoch_length,
        }
    }

    /// Whether EIP-37 difficulty adjustment is active at the given height.
    pub fn eip37_active(&self, height: u32) -> bool {
        self.eip37_activation_height.is_some_and(|h| height >= h)
    }
}

#[cfg(test)]
mod devnet_tests {
    use super::*;

    #[test]
    fn devnet_is_testnet_with_difficulty_pinned() {
        // Scala devnet-mixed genesis.conf: epochLength 33554432, blockInterval 20s,
        // initialDifficultyHex "01", version2ActivationHeight unreachable.
        let d = ChainConfig::devnet();
        let t = ChainConfig::testnet();
        assert_eq!(d.network, Network::Devnet);
        assert_eq!(d.epoch_length, 1 << 25);
        assert_eq!(d.block_interval_ms, 20_000);
        assert_eq!(d.max_time_drift_ms, 10 * 20_000);
        assert_eq!(d.initial_n_bits, t.initial_n_bits); // difficulty 1
        assert_eq!(d.use_last_epochs, t.use_last_epochs);
        assert_eq!(d.version2_activation_n_bits, None);
        assert_eq!(d.genesis_id, None);
        assert_eq!(d.eip37_activation_height, None);
        assert_eq!(d.voting.voting_length, 1 << 25);
    }
}

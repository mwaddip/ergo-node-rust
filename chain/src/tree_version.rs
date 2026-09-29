//! The ErgoTree version a block version selects.
//!
//! The fork reads and writes a transaction at a tree version, and computes its
//! id and `bytes_to_sign()` at the version it was read at. The same bytes read
//! at 0 and at 3 can differ in what parses and in the id. Each rule is stated
//! once, here, and every reader and writer in the node calls these functions
//! (`facts/chain.md` § "The tree version a block's transactions are read at").

use ergo_lib::ergotree_ir::ergo_tree::ErgoTreeVersion;

/// The first block version whose transactions are read at version 3
/// (JVM `Header.Interpreter60Version`).
const INTERPRETER_60_BLOCK_VERSION: u8 = 4;

/// The first block version whose transactions are written at version 3
/// (JVM `VersionContext.V6SoftForkVersion`).
const V6_SOFT_FORK_BLOCK_VERSION: u8 = 3;

/// The ErgoTree version a block's transactions are read at: V3 from block
/// version 4, V0 below (JVM `BlockTransactionsSerializer.parse`).
pub fn block_transactions_read_version(block_version: u8) -> ErgoTreeVersion {
    if block_version >= INTERPRETER_60_BLOCK_VERSION {
        ErgoTreeVersion::V3
    } else {
        ErgoTreeVersion::V0
    }
}

/// The version a block's transactions are written at: V3 from block version
/// 3, V0 below (JVM `BlockTransactionsSerializer.serialize`, which wraps each
/// transaction in `withVersions(blockVersion, blockVersion)` from
/// `VersionContext.V6SoftForkVersion` = 3). A v3 block is written at 3 and
/// read at 0. The asymmetry is the JVM's own.
pub fn block_transactions_write_version(block_version: u8) -> ErgoTreeVersion {
    if block_version >= V6_SOFT_FORK_BLOCK_VERSION {
        ErgoTreeVersion::V3
    } else {
        ErgoTreeVersion::V0
    }
}

/// The script version activated at `block_version`: `block_version − 1`,
/// saturating at 0 (JVM `Header.scriptFromBlockVersion`). A pool
/// transaction is read at it (`facts/mempool.md` § P2P Transaction Intake),
/// and one the node relays is written at it (`facts/api.md` § Transaction
/// Submission Flow).
pub fn activated_tree_version(block_version: u8) -> ErgoTreeVersion {
    ErgoTreeVersion::from(block_version.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    //! Pinned to the JVM's table, not to a formula. v6.0.6
    //! `BlockTransactionsSerializer` reads a block's transactions under
    //! `scriptAndTreeFromBlockVersions(blockVersion)`, `blockVersion − 1` for
    //! both versions, from `Header.Interpreter60Version` = 4, and writes them
    //! under `withVersions(blockVersion, blockVersion)` from
    //! `VersionContext.V6SoftForkVersion` = 3. The node holds both at 3 above
    //! that (`facts/chain.md`). `Header.scriptFromBlockVersion` is
    //! `blockVersion − 1`.
    use super::*;

    #[test]
    fn read_version_is_3_from_block_version_4() {
        for (block_version, expected) in [
            (1, ErgoTreeVersion::V0),
            (2, ErgoTreeVersion::V0),
            (3, ErgoTreeVersion::V0),
            (4, ErgoTreeVersion::V3),
            (5, ErgoTreeVersion::V3),
        ] {
            assert_eq!(
                block_transactions_read_version(block_version),
                expected,
                "block version {block_version}"
            );
        }
    }

    #[test]
    fn write_version_is_3_from_block_version_3() {
        for (block_version, expected) in [
            (1, ErgoTreeVersion::V0),
            (2, ErgoTreeVersion::V0),
            (3, ErgoTreeVersion::V3),
            (4, ErgoTreeVersion::V3),
        ] {
            assert_eq!(
                block_transactions_write_version(block_version),
                expected,
                "block version {block_version}"
            );
        }
    }

    #[test]
    fn activated_version_is_one_below_the_block_version() {
        for (block_version, expected) in [
            (0, ErgoTreeVersion::V0),
            (1, ErgoTreeVersion::V0),
            (2, ErgoTreeVersion::V1),
            (3, ErgoTreeVersion::V2),
            (4, ErgoTreeVersion::V3),
        ] {
            assert_eq!(
                activated_tree_version(block_version),
                expected,
                "block version {block_version}"
            );
        }
    }
}

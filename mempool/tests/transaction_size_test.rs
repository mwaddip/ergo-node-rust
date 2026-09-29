//! The transaction size limit the P2P intake and the REST API share through
//! one constant (`facts/mempool.md` § P2P Transaction Intake).

/// Pinned to the observed JVM number, written as a literal: the reference
/// `application.conf` sets `maxTransactionSize = 98304 // 96 kb` (v6.0.6,
/// line 53). A formula here would move with the constant it is checking.
/// Named by its crate-root path, the one both consumers use.
#[test]
fn max_transaction_size_is_the_jvm_default() {
    assert_eq!(ergo_mempool::MAX_TRANSACTION_SIZE, 98_304);
}

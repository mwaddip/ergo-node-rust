//! Indexer library crate.
//!
//! Exposes the subset of internal modules the `ergo-indexer-migratedb` binary
//! needs. The long-running `ergo-indexer` daemon (`src/main.rs`) declares its
//! own module tree directly — this lib does NOT replace that. Both build paths
//! compile the underlying source files independently; their type universes
//! do not overlap. Adding lib.rs costs one extra compilation pass on these
//! modules but keeps the migratedb bin a thin, well-scoped consumer.
//!
//! Modules re-exported here are kept to what the migratedb bin actually
//! imports. If the bin grows, expose more — don't expose everything by
//! default.
//!
//! Note: `#[global_allocator]` is intentionally NOT declared here. The bin
//! crates that link against this lib (`ergo-indexer` main and
//! `ergo-indexer-migratedb`) each declare it once. Two registrations would
//! conflict at link time.

pub mod db;
pub mod migrate;
pub mod types;

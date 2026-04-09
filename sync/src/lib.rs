//! Chain sync state machine for the Ergo Rust node.
//!
//! Drives the P2P layer to request headers, building up the validated
//! header chain from genesis to the network tip.

pub mod delivery;
pub mod light_bootstrap;
pub mod snapshot;
mod state;
mod traits;

pub use state::{HeaderSync, SyncConfig};
pub use traits::{SyncChain, SyncStore, SyncTransport};

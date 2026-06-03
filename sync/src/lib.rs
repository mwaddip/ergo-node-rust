//! Chain sync state machine for the Ergo Rust node.
//!
//! Drives the P2P layer to request headers, building up the validated
//! header chain from genesis to the network tip.

pub mod apply_state_error;
pub mod delivery;
pub mod light_bootstrap;
mod retention;
pub mod snapshot;
mod state;
mod sweep_backoff;
mod traits;

pub use state::{HeaderSync, SyncConfig};
pub use traits::{SyncChain, SyncStore, SyncTransport};

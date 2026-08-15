//! Chain sync state machine for the Ergo Rust node.
//!
//! Drives the P2P layer to request headers, building up the validated
//! header chain from genesis to the network tip.

pub mod apply_state_error;
mod catchup_progress;
pub mod delivery;
pub mod light_bootstrap;
mod retention;
pub mod snapshot;
mod state;
mod sweep_backoff;
#[cfg(test)]
mod test_support;
mod traits;

pub use state::{HeaderSync, SyncConfig, SyncWindowEstimate, WINDOW_BYTES_UNSET};
pub use traits::{SyncChain, SyncStore, SyncTransport};

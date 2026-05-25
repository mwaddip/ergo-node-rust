//! P2P frame capture: file-backed ring buffer in pcap-compatible format.
//!
//! Off by default; opt-in via `[debug.p2p_capture]` in `ergo.toml`. When
//! enabled, every frame transiting the transport layer (inbound + outbound)
//! is appended to a memory-mapped ring file with per-record Ergo metadata.
//! Operators dump the ring via REST and load it in Wireshark.
//!
//! See `facts/p2p-capture.md` for the contract.

pub mod config;
pub mod pcap;
pub mod ring;
pub mod tap;

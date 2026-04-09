//! NiPoPoW light-client bootstrap state machine.
//!
//! Runs once at startup when `state_type == StateType::Light` and the chain
//! is empty. Asks one peer for a NiPoPoW proof, verifies it, and installs
//! the suffix as the chain's starting point. Subsequent tip-following uses
//! the existing header sync loop.
//!
//! Single peer per attempt — multi-peer best-arg comparison (KMZ17 §4.3) is
//! tracked as a hardening follow-up. The trust model is standard SPV: a
//! hostile peer causes a recoverable DoS, not loss of funds.
//!
//! JVM reference: `ErgoNodeViewSynchronizer.scala:1032` (outbound request),
//! `PopowProcessor.applyPopowProof` (install side).

use enr_chain::{BlockId, Header};
use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use thiserror::Error;
use tokio::time::{Duration, Instant};

use crate::snapshot::protocol::{vlq_encode, zigzag_encode};
use crate::traits::{SyncChain, SyncTransport};

/// JVM `P2PNipopowProofM` from `ErgoHistoryUtils.scala:29`.
pub const P2P_NIPOPOW_M: i32 = 6;
/// JVM `P2PNipopowProofK` from `ErgoHistoryUtils.scala:34`.
pub const P2P_NIPOPOW_K: i32 = 10;

/// P2P message code for `GetNipopowProof`.
pub const GET_NIPOPOW_PROOF: u8 = 90;
/// P2P message code for `NipopowProof`.
pub const NIPOPOW_PROOF: u8 = 91;

/// How long to wait for a peer to respond with a NiPoPoW proof before
/// rotating to the next peer.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for an outbound peer to become available at startup
/// before giving up.
const PEER_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Polling interval while waiting for outbound peers.
const PEER_WAIT_POLL: Duration = Duration::from_secs(1);
/// Maximum number of peers to attempt before declaring bootstrap failed.
const MAX_PEER_ATTEMPTS: usize = 3;

#[derive(Debug, Error)]
pub enum LightBootstrapError {
    #[error("no outbound peers available within {0:?}")]
    NoPeers(Duration),

    #[error("all {0} attempted peers stalled (no NipopowProof response within timeout)")]
    AllPeersStalled(usize),

    #[error("all {0} attempted peers returned invalid proofs")]
    AllPeersHostile(usize),

    #[error("install failed: {0}")]
    InstallFailed(enr_chain::ChainError),

    #[error("event stream closed before bootstrap completed")]
    StreamClosed,
}

/// Build the body of a `GetNipopowProof` (code 90) message.
///
/// Wire format (mirrors JVM `GetNipopowProofSpec.parse`):
/// ```text
/// m: i32 (ZigZag VLQ — putInt)
/// k: i32 (ZigZag VLQ — putInt)
/// header_id_present: u8 (raw byte: 0 or 1)
/// [if present] header_id: 32 raw bytes
/// future_pad_length: u16 (VLQ — putUShort) — always 0 from us
/// ```
fn build_get_nipopow_proof_body(m: i32, k: i32, header_id: Option<&BlockId>) -> Vec<u8> {
    let mut body = Vec::with_capacity(48);
    body.extend_from_slice(&vlq_encode(zigzag_encode(m as i64)));
    body.extend_from_slice(&vlq_encode(zigzag_encode(k as i64)));
    match header_id {
        Some(id) => {
            body.push(1);
            body.extend_from_slice(&id.0 .0);
        }
        None => body.push(0),
    }
    body.extend_from_slice(&vlq_encode(0)); // future_pad_length = 0
    body
}

/// Run the NiPoPoW light-client bootstrap.
///
/// Idempotent: returns immediately if `chain.chain_height() > 0`.
///
/// Top-level state machine:
/// 1. Wait for at least one outbound peer (60s deadline).
/// 2. For each peer (up to 3 attempts):
///    a. Send `GetNipopowProof(m=6, k=10, header_id=None)`.
///    b. Wait for a code-91 response from THAT peer (30s timeout).
///    c. Verify the proof. On verify failure → mark peer hostile, rotate.
///    d. Split the verified headers into (suffix_head, suffix_tail) using
///       the `k` we requested.
///    e. Install via `chain.install_nipopow_suffix`.
///    f. Return Ok.
/// 3. All peers exhausted → return error.
pub async fn run_light_bootstrap<T: SyncTransport, C: SyncChain>(
    transport: &mut T,
    chain: &C,
) -> Result<(), LightBootstrapError> {
    if chain.chain_height().await > 0 {
        tracing::info!("light bootstrap: chain already non-empty, skipping");
        return Ok(());
    }

    tracing::info!(
        m = P2P_NIPOPOW_M,
        k = P2P_NIPOPOW_K,
        "light bootstrap: starting"
    );

    // Step 1: wait for outbound peers.
    let peer_wait_deadline = Instant::now() + PEER_WAIT_TIMEOUT;
    let mut initial_peers = Vec::new();
    while Instant::now() < peer_wait_deadline {
        let peers = transport.outbound_peers().await;
        if !peers.is_empty() {
            initial_peers = peers;
            break;
        }
        tokio::time::sleep(PEER_WAIT_POLL).await;
    }
    if initial_peers.is_empty() {
        return Err(LightBootstrapError::NoPeers(PEER_WAIT_TIMEOUT));
    }
    tracing::info!(
        peer_count = initial_peers.len(),
        "light bootstrap: outbound peers available"
    );

    // Step 2: try peers one at a time.
    let mut tried = Vec::new();
    let mut hostile = 0usize;

    for &target_peer in initial_peers.iter().take(MAX_PEER_ATTEMPTS) {
        tried.push(target_peer);
        tracing::info!(peer = ?target_peer, "light bootstrap: requesting proof");

        // Send GetNipopowProof to this peer.
        let body = build_get_nipopow_proof_body(P2P_NIPOPOW_M, P2P_NIPOPOW_K, None);
        let msg = ProtocolMessage::Unknown {
            code: GET_NIPOPOW_PROOF,
            body,
        };
        if let Err(e) = transport.send_to(target_peer, msg).await {
            tracing::warn!(peer = ?target_peer, "light bootstrap: send failed: {e}");
            // Counted as stall for the final error decision.
            continue;
        }

        // Wait for the response from this specific peer.
        let outcome = wait_for_proof(transport, chain, target_peer, RESPONSE_TIMEOUT).await;
        match outcome {
            ProofOutcome::Verified(headers) => {
                tracing::info!(
                    peer = ?target_peer,
                    header_count = headers.len(),
                    "light bootstrap: proof verified"
                );

                // Step 2d: split into (suffix_head, suffix_tail).
                // The last k headers are the suffix; the rest is the prefix
                // and gets discarded. suffix_head is the first of the last k.
                let k = P2P_NIPOPOW_K as usize;
                if headers.len() < k {
                    tracing::warn!(
                        peer = ?target_peer,
                        header_count = headers.len(),
                        k,
                        "light bootstrap: proof has fewer headers than k — treating as hostile"
                    );
                    hostile += 1;
                    continue;
                }
                let split_idx = headers.len() - k;
                let mut suffix: Vec<Header> = headers.into_iter().skip(split_idx).collect();
                let suffix_head = suffix.remove(0);
                let suffix_tail = suffix;

                tracing::info!(
                    peer = ?target_peer,
                    suffix_head_height = suffix_head.height,
                    suffix_tail_len = suffix_tail.len(),
                    "light bootstrap: installing suffix"
                );

                // Step 2e: install.
                match chain.install_nipopow_suffix(suffix_head, suffix_tail).await {
                    Ok(()) => {
                        let final_height = chain.chain_height().await;
                        tracing::info!(
                            chain_height = final_height,
                            "light bootstrap: complete, transitioning to tip-following sync"
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        // Install errors are NOT peer-attributable —
                        // verify already passed. This is a chain-state
                        // failure (e.g., chain not actually empty due to a
                        // race). Surface immediately.
                        tracing::error!(
                            peer = ?target_peer,
                            "light bootstrap: install failed: {e}"
                        );
                        return Err(LightBootstrapError::InstallFailed(e));
                    }
                }
            }
            ProofOutcome::VerifyFailed(reason) => {
                tracing::warn!(
                    peer = ?target_peer,
                    "light bootstrap: proof verify failed ({reason}) — peer hostile, rotating"
                );
                hostile += 1;
            }
            ProofOutcome::Timeout => {
                tracing::warn!(
                    peer = ?target_peer,
                    timeout_secs = RESPONSE_TIMEOUT.as_secs(),
                    "light bootstrap: timed out waiting for response — peer stalled, rotating"
                );
                // Counted as stall for the final error decision.
            }
            ProofOutcome::StreamClosed => {
                return Err(LightBootstrapError::StreamClosed);
            }
        }
    }

    // Step 3: all peers exhausted.
    if hostile >= tried.len() {
        Err(LightBootstrapError::AllPeersHostile(tried.len()))
    } else {
        Err(LightBootstrapError::AllPeersStalled(tried.len()))
    }
}

/// Outcome of waiting for a NiPoPoW proof from a specific peer.
enum ProofOutcome {
    Verified(Vec<Header>),
    VerifyFailed(String),
    Timeout,
    StreamClosed,
}

/// Drain the transport event stream until either a code-91 message arrives
/// from `target_peer` or the timeout expires.
///
/// Events from other peers and other message codes are silently dropped —
/// the bootstrap is the only consumer that cares about them at this point
/// in the lifecycle (the main sync loop hasn't started yet, so there's
/// nothing else to route them to).
async fn wait_for_proof<T: SyncTransport, C: SyncChain>(
    transport: &mut T,
    chain: &C,
    target_peer: PeerId,
    timeout: Duration,
) -> ProofOutcome {
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => return ProofOutcome::Timeout,
        };

        let event = tokio::time::timeout(remaining, transport.next_event()).await;
        let event = match event {
            Ok(Some(e)) => e,
            Ok(None) => return ProofOutcome::StreamClosed,
            Err(_) => return ProofOutcome::Timeout,
        };

        let ProtocolEvent::Message { peer_id, message } = event else {
            continue;
        };

        if peer_id != target_peer {
            // Ignore proofs from other peers in single-peer mode.
            continue;
        }

        let ProtocolMessage::Unknown { code, body } = message else {
            continue;
        };

        if code != NIPOPOW_PROOF {
            continue;
        }

        match chain.verify_nipopow_envelope(&body).await {
            Ok(headers) => return ProofOutcome::Verified(headers),
            Err(e) => return ProofOutcome::VerifyFailed(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_get_nipopow_proof_body_no_anchor_jvm_defaults() {
        // m=6, k=10, no anchor — the wire bytes the JVM peer expects.
        // Hand-computed:
        //   m=6 → zigzag(6)=12 → vlq=[0x0c]
        //   k=10 → zigzag(10)=20 → vlq=[0x14]
        //   present=0
        //   pad_len=0 → vlq=[0x00]
        let body = build_get_nipopow_proof_body(6, 10, None);
        assert_eq!(body, vec![0x0c, 0x14, 0x00, 0x00]);
    }

    // The bootstrap only ever sends header_id=None for first release.
    // The with-anchor wire encoding is exercised by the
    // src/nipopow_serve.rs round-trip tests in the main crate, which have
    // direct access to BlockId/Digest32 construction.
}

//! NiPoPoW light-client bootstrap state machine.
//!
//! Runs once at startup when `state_type == StateType::Light` and the chain
//! is empty. Broadcasts `GetNipopowProof` to all outbound peers, collects
//! valid proofs within a timeout window, compares them via KMZ17 §4.3
//! (`is_better_than`), and installs the best as the chain's starting point.
//! Subsequent tip-following uses the existing header sync loop.
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

/// How long to wait for proof responses after broadcasting to all peers.
/// Peers that haven't responded within this window are counted as stalled.
const COLLECTION_WINDOW: Duration = Duration::from_secs(30);
/// How long to wait for an outbound peer to become available at startup
/// before giving up.
const PEER_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Polling interval while waiting for outbound peers.
const PEER_WAIT_POLL: Duration = Duration::from_secs(1);

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
/// Top-level state machine (KMZ17 §4.3 multi-peer comparison):
/// 1. Wait for at least one outbound peer (60s deadline).
/// 2. Broadcast `GetNipopowProof(m=6, k=10)` to ALL outbound peers.
/// 3. Collect valid proofs within a 30s window. Verify each on arrival.
/// 4. If multiple valid proofs, compare pairwise via `is_better_than`
///    and pick the best. If one valid proof, use it.
/// 5. Split the best proof's headers into (suffix_head, suffix_tail)
///    and install.
/// 6. No valid proofs → return error.
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
    let mut peers = Vec::new();
    while Instant::now() < peer_wait_deadline {
        peers = transport.outbound_peers().await;
        if !peers.is_empty() {
            break;
        }
        tokio::time::sleep(PEER_WAIT_POLL).await;
    }
    if peers.is_empty() {
        return Err(LightBootstrapError::NoPeers(PEER_WAIT_TIMEOUT));
    }
    tracing::info!(
        peer_count = peers.len(),
        "light bootstrap: broadcasting GetNipopowProof to all outbound peers"
    );

    // Step 2: broadcast to all outbound peers.
    let body = build_get_nipopow_proof_body(P2P_NIPOPOW_M, P2P_NIPOPOW_K, None);
    let mut sent_to = Vec::new();
    for &peer in &peers {
        let msg = ProtocolMessage::Unknown {
            code: GET_NIPOPOW_PROOF,
            body: body.clone(),
        };
        if let Err(e) = transport.send_to(peer, msg).await {
            tracing::warn!(peer = ?peer, "light bootstrap: send failed: {e}");
        } else {
            sent_to.push(peer);
        }
    }
    if sent_to.is_empty() {
        return Err(LightBootstrapError::AllPeersStalled(peers.len()));
    }

    // Step 3: collect valid proofs within the window.
    let mut valid_proofs: Vec<ValidProof> = Vec::new();
    let mut hostile = 0usize;
    let mut responded = 0usize;
    let deadline = Instant::now() + COLLECTION_WINDOW;

    loop {
        // All peers responded or window expired — stop collecting.
        if responded >= sent_to.len() {
            break;
        }
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => break,
        };

        let event = tokio::time::timeout(remaining, transport.next_event()).await;
        let event = match event {
            Ok(Some(e)) => e,
            Ok(None) => return Err(LightBootstrapError::StreamClosed),
            Err(_) => break, // timeout
        };

        let ProtocolEvent::Message { peer_id, message } = event else {
            continue;
        };
        if !sent_to.contains(&peer_id) {
            continue;
        }
        let ProtocolMessage::Unknown { code, body } = message else {
            continue;
        };
        if code != NIPOPOW_PROOF {
            continue;
        }

        responded += 1;

        match chain.verify_nipopow_envelope(&body).await {
            Ok(headers) => {
                tracing::info!(
                    peer = ?peer_id,
                    header_count = headers.len(),
                    "light bootstrap: valid proof received"
                );
                valid_proofs.push(ValidProof {
                    peer: peer_id,
                    headers,
                    envelope: body,
                });
            }
            Err(e) => {
                tracing::warn!(
                    peer = ?peer_id,
                    "light bootstrap: proof verify failed ({e}) — hostile"
                );
                hostile += 1;
            }
        }
    }

    if valid_proofs.is_empty() {
        if hostile > 0 {
            return Err(LightBootstrapError::AllPeersHostile(sent_to.len()));
        }
        return Err(LightBootstrapError::AllPeersStalled(sent_to.len()));
    }

    // Step 4: pick the best proof via KMZ17 comparison.
    let best = if valid_proofs.len() == 1 {
        valid_proofs.into_iter().next().unwrap()
    } else {
        pick_best_proof(chain, valid_proofs).await?
    };

    tracing::info!(
        peer = ?best.peer,
        header_count = best.headers.len(),
        "light bootstrap: selected best proof"
    );

    // Step 5: split into (suffix_head, suffix_tail) and install.
    let k = P2P_NIPOPOW_K as usize;
    if best.headers.len() < k {
        return Err(LightBootstrapError::AllPeersHostile(1));
    }
    let split_idx = best.headers.len() - k;
    let mut suffix: Vec<Header> = best.headers.into_iter().skip(split_idx).collect();
    let suffix_head = suffix.remove(0);
    let suffix_tail = suffix;

    tracing::info!(
        suffix_head_height = suffix_head.height,
        suffix_tail_len = suffix_tail.len(),
        "light bootstrap: installing suffix"
    );

    chain
        .install_nipopow_suffix(suffix_head, suffix_tail)
        .await
        .map_err(LightBootstrapError::InstallFailed)?;

    let final_height = chain.chain_height().await;
    tracing::info!(
        chain_height = final_height,
        "light bootstrap: complete, transitioning to tip-following sync"
    );
    Ok(())
}

/// A verified proof from a peer, kept alive for comparison.
struct ValidProof {
    peer: PeerId,
    headers: Vec<Header>,
    /// Raw P2P code-91 envelope body — retained for KMZ17 comparison.
    envelope: Vec<u8>,
}

/// Compare all valid proofs pairwise and return the best one.
///
/// Uses `SyncChain::is_better_nipopow` which delegates to
/// `NipopowProof::is_better_than` (KMZ17 §4.3).
async fn pick_best_proof<C: SyncChain>(
    chain: &C,
    mut proofs: Vec<ValidProof>,
) -> Result<ValidProof, LightBootstrapError> {
    debug_assert!(proofs.len() >= 2);

    let mut best_idx = 0;
    for i in 1..proofs.len() {
        match chain
            .is_better_nipopow(&proofs[i].envelope, &proofs[best_idx].envelope)
            .await
        {
            Ok(true) => {
                tracing::info!(
                    challenger = ?proofs[i].peer,
                    incumbent = ?proofs[best_idx].peer,
                    "light bootstrap: challenger proof is better"
                );
                best_idx = i;
            }
            Ok(false) => {}
            Err(e) => {
                // Comparison failure — skip this challenger, keep incumbent.
                tracing::warn!(
                    challenger = ?proofs[i].peer,
                    "light bootstrap: proof comparison failed ({e}), skipping"
                );
            }
        }
    }

    Ok(proofs.swap_remove(best_idx))
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

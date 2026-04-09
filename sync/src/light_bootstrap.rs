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
    use enr_chain::ChainError;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[test]
    fn build_get_nipopow_proof_body_no_anchor_jvm_defaults() {
        let body = build_get_nipopow_proof_body(6, 10, None);
        assert_eq!(body, vec![0x0c, 0x14, 0x00, 0x00]);
    }

    // --- Mock infrastructure ---

    fn fake_header(height: u32) -> Header {
        use ergo_chain_types::*;
        Header {
            version: 2,
            id: BlockId(Digest32::from([height as u8; 32])),
            parent_id: BlockId(Digest32::zero()),
            ad_proofs_root: Digest32::zero(),
            state_root: ADDigest::zero(),
            transaction_root: Digest32::zero(),
            timestamp: 1_000_000 + height as u64,
            n_bits: 100_000,
            height,
            extension_root: Digest32::zero(),
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: vec![0; 8],
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        }
    }

    fn fake_headers(count: usize) -> Vec<Header> {
        (1..=count as u32).map(fake_header).collect()
    }

    /// Mock transport that delivers scripted events.
    struct MockTransport {
        outbound: Vec<PeerId>,
        events: VecDeque<ProtocolEvent>,
        sent: Mutex<Vec<(PeerId, ProtocolMessage)>>,
    }

    impl MockTransport {
        fn new(peers: Vec<PeerId>, events: Vec<ProtocolEvent>) -> Self {
            Self {
                outbound: peers,
                events: events.into(),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl crate::traits::SyncTransport for MockTransport {
        async fn send_to(
            &self,
            peer: PeerId,
            message: ProtocolMessage,
        ) -> Result<(), Box<dyn std::error::Error + Send>> {
            self.sent.lock().unwrap().push((peer, message));
            Ok(())
        }

        async fn outbound_peers(&self) -> Vec<PeerId> {
            self.outbound.clone()
        }

        async fn next_event(&mut self) -> Option<ProtocolEvent> {
            self.events.pop_front()
        }
    }

    /// Verify outcome: Ok(headers) or Err(reason).
    #[derive(Clone)]
    enum VerifyResult {
        Ok(Vec<Header>),
        Err(String),
    }

    /// Compare outcome for is_better_nipopow.
    #[derive(Clone)]
    enum CompareResult {
        Better,
        Worse,
        Err(String),
    }

    /// Mock chain that returns scripted verification and comparison results.
    struct MockChain {
        /// Map envelope body → verification result.
        verify_results: Mutex<Vec<(Vec<u8>, VerifyResult)>>,
        /// Pairwise comparison results: (this, that) → result.
        compare_results: Mutex<Vec<(Vec<u8>, Vec<u8>, CompareResult)>>,
        installed: Mutex<Option<(Header, Vec<Header>)>>,
    }

    impl MockChain {
        fn new() -> Self {
            Self {
                verify_results: Mutex::new(Vec::new()),
                compare_results: Mutex::new(Vec::new()),
                installed: Mutex::new(None),
            }
        }

        fn add_verify(&self, body: Vec<u8>, result: VerifyResult) {
            self.verify_results.lock().unwrap().push((body, result));
        }

        fn add_compare(&self, this: Vec<u8>, than: Vec<u8>, result: CompareResult) {
            self.compare_results.lock().unwrap().push((this, than, result));
        }

        fn installed(&self) -> Option<(Header, Vec<Header>)> {
            self.installed.lock().unwrap().clone()
        }
    }

    impl crate::traits::SyncChain for MockChain {
        async fn chain_height(&self) -> u32 { 0 }
        async fn build_sync_info(&self) -> Vec<u8> { vec![] }
        async fn header_at(&self, _h: u32) -> Option<Header> { None }
        async fn header_state_root(&self, _h: u32) -> Option<[u8; 33]> { None }
        fn parse_sync_info(&self, _b: &[u8]) -> Result<enr_chain::SyncInfo, ChainError> {
            unimplemented!()
        }
        async fn active_parameters(&self) -> ergo_validation::Parameters {
            unimplemented!()
        }
        async fn is_epoch_boundary(&self, _h: u32) -> bool { false }
        async fn compute_expected_parameters(
            &self, _h: u32,
        ) -> Result<ergo_validation::Parameters, ChainError> {
            unimplemented!()
        }
        async fn apply_epoch_boundary_parameters(&self, _p: ergo_validation::Parameters) {}

        async fn verify_nipopow_envelope(
            &self,
            envelope_body: &[u8],
        ) -> Result<Vec<Header>, ChainError> {
            let results = self.verify_results.lock().unwrap();
            for (body, result) in results.iter() {
                if body == envelope_body {
                    return match result {
                        VerifyResult::Ok(h) => Ok(h.clone()),
                        VerifyResult::Err(e) => Err(ChainError::Nipopow(e.clone())),
                    };
                }
            }
            Err(ChainError::Nipopow("unexpected envelope body in mock".into()))
        }

        async fn is_better_nipopow(
            &self,
            this_envelope: &[u8],
            than_envelope: &[u8],
        ) -> Result<bool, ChainError> {
            let results = self.compare_results.lock().unwrap();
            for (this, than, result) in results.iter() {
                if this == this_envelope && than == than_envelope {
                    return match result {
                        CompareResult::Better => Ok(true),
                        CompareResult::Worse => Ok(false),
                        CompareResult::Err(e) => Err(ChainError::Nipopow(e.clone())),
                    };
                }
            }
            Err(ChainError::Nipopow("unexpected comparison in mock".into()))
        }

        async fn install_nipopow_suffix(
            &self,
            suffix_head: Header,
            suffix_tail: Vec<Header>,
        ) -> Result<(), ChainError> {
            *self.installed.lock().unwrap() = Some((suffix_head, suffix_tail));
            Ok(())
        }
    }

    fn proof_event(peer: PeerId, body: Vec<u8>) -> ProtocolEvent {
        ProtocolEvent::Message {
            peer_id: peer,
            message: ProtocolMessage::Unknown {
                code: NIPOPOW_PROOF,
                body,
            },
        }
    }

    // --- Bootstrap adversarial tests ---

    #[tokio::test]
    async fn bootstrap_no_responses_stream_exhausted() {
        let peer_a = PeerId(1);
        let peer_b = PeerId(2);
        // No events in queue — mock returns None (stream closed).
        let mut transport = MockTransport::new(vec![peer_a, peer_b], vec![]);
        let chain = MockChain::new();

        let result = run_light_bootstrap(&mut transport, &chain).await;
        assert!(matches!(result, Err(LightBootstrapError::StreamClosed)));
        assert!(chain.installed().is_none());
    }

    #[tokio::test]
    async fn bootstrap_all_peers_hostile() {
        let peer_a = PeerId(1);
        let body_a = vec![0xaa];
        let chain = MockChain::new();
        chain.add_verify(body_a.clone(), VerifyResult::Err("bad proof".into()));

        let mut transport = MockTransport::new(
            vec![peer_a],
            vec![proof_event(peer_a, body_a)],
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        assert!(matches!(result, Err(LightBootstrapError::AllPeersHostile(_))));
        assert!(chain.installed().is_none());
    }

    #[tokio::test]
    async fn bootstrap_single_valid_proof_installs() {
        let peer_a = PeerId(1);
        let body_a = vec![0xaa];
        let headers = fake_headers(15); // 15 > k=10
        let chain = MockChain::new();
        chain.add_verify(body_a.clone(), VerifyResult::Ok(headers.clone()));

        let mut transport = MockTransport::new(
            vec![peer_a],
            vec![proof_event(peer_a, body_a)],
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        assert!(result.is_ok());
        let (head, tail) = chain.installed().expect("should have installed");
        // suffix_head is the (len-k)th header, tail is the remaining k-1
        assert_eq!(head.height, 6); // headers[5] (0-indexed), height 6
        assert_eq!(tail.len(), P2P_NIPOPOW_K as usize - 1);
    }

    #[tokio::test]
    async fn bootstrap_mix_hostile_and_valid() {
        let peer_a = PeerId(1);
        let peer_b = PeerId(2);
        let body_a = vec![0xaa];
        let body_b = vec![0xbb];
        let headers = fake_headers(15);
        let chain = MockChain::new();
        chain.add_verify(body_a.clone(), VerifyResult::Err("invalid".into()));
        chain.add_verify(body_b.clone(), VerifyResult::Ok(headers));

        let mut transport = MockTransport::new(
            vec![peer_a, peer_b],
            vec![
                proof_event(peer_a, body_a),
                proof_event(peer_b, body_b),
            ],
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        assert!(result.is_ok());
        assert!(chain.installed().is_some());
    }

    #[tokio::test]
    async fn bootstrap_two_valid_proofs_picks_better() {
        let peer_a = PeerId(1);
        let peer_b = PeerId(2);
        let body_a = vec![0xaa];
        let body_b = vec![0xbb];
        let headers_a = fake_headers(15);
        let headers_b = fake_headers(15);
        let chain = MockChain::new();
        chain.add_verify(body_a.clone(), VerifyResult::Ok(headers_a));
        chain.add_verify(body_b.clone(), VerifyResult::Ok(headers_b));
        // Peer B's proof is better than A's.
        chain.add_compare(body_b.clone(), body_a.clone(), CompareResult::Better);

        let mut transport = MockTransport::new(
            vec![peer_a, peer_b],
            vec![
                proof_event(peer_a, body_a),
                proof_event(peer_b, body_b),
            ],
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        assert!(result.is_ok());
        // Should have installed (peer B was better).
        assert!(chain.installed().is_some());
    }

    #[tokio::test]
    async fn bootstrap_comparison_failure_falls_back_to_incumbent() {
        let peer_a = PeerId(1);
        let peer_b = PeerId(2);
        let body_a = vec![0xaa];
        let body_b = vec![0xbb];
        let headers_a = fake_headers(15);
        let headers_b = fake_headers(15);
        let chain = MockChain::new();
        chain.add_verify(body_a.clone(), VerifyResult::Ok(headers_a));
        chain.add_verify(body_b.clone(), VerifyResult::Ok(headers_b));
        // Comparison fails — should fall back to incumbent (peer A, first valid).
        chain.add_compare(body_b.clone(), body_a.clone(), CompareResult::Err("parse failed".into()));

        let mut transport = MockTransport::new(
            vec![peer_a, peer_b],
            vec![
                proof_event(peer_a, body_a),
                proof_event(peer_b, body_b),
            ],
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        assert!(result.is_ok());
        assert!(chain.installed().is_some());
    }

    #[tokio::test]
    async fn bootstrap_stream_closes_after_partial_collection() {
        let peer_a = PeerId(1);
        let peer_b = PeerId(2);
        let body_a = vec![0xaa];
        let chain = MockChain::new();
        // Peer A sends an invalid proof, then stream closes before B responds.
        chain.add_verify(body_a.clone(), VerifyResult::Err("bad".into()));

        let mut transport = MockTransport::new(
            vec![peer_a, peer_b],
            vec![proof_event(peer_a, body_a)],
            // After this event, next_event returns None (stream closed).
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        // Stream closed after collecting one hostile proof, so StreamClosed.
        assert!(matches!(result, Err(LightBootstrapError::StreamClosed)));
    }

    #[tokio::test]
    async fn bootstrap_ignores_unsolicited_peer() {
        let peer_a = PeerId(1);
        let peer_rogue = PeerId(99);
        let body_a = vec![0xaa];
        let body_rogue = vec![0xff];
        let headers = fake_headers(15);
        let chain = MockChain::new();
        chain.add_verify(body_a.clone(), VerifyResult::Ok(headers));
        // Don't add verify for rogue — it should never be called.

        let mut transport = MockTransport::new(
            vec![peer_a],
            vec![
                // Rogue peer sends a proof — should be ignored.
                proof_event(peer_rogue, body_rogue),
                // Real peer sends valid proof.
                proof_event(peer_a, body_a),
            ],
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        assert!(result.is_ok());
        assert!(chain.installed().is_some());
    }

    #[tokio::test]
    async fn bootstrap_ignores_non_proof_messages() {
        let peer_a = PeerId(1);
        let body_a = vec![0xaa];
        let headers = fake_headers(15);
        let chain = MockChain::new();
        chain.add_verify(body_a.clone(), VerifyResult::Ok(headers));

        let mut transport = MockTransport::new(
            vec![peer_a],
            vec![
                // Non-code-91 Unknown message — should be ignored.
                ProtocolEvent::Message {
                    peer_id: peer_a,
                    message: ProtocolMessage::Unknown {
                        code: 42,
                        body: vec![0xde, 0xad],
                    },
                },
                // Non-Unknown message — should be ignored.
                ProtocolEvent::Message {
                    peer_id: peer_a,
                    message: ProtocolMessage::Peers { body: vec![] },
                },
                // Valid proof.
                proof_event(peer_a, body_a),
            ],
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn bootstrap_proof_too_few_headers_for_k() {
        let peer_a = PeerId(1);
        let body_a = vec![0xaa];
        // Only 5 headers — less than k=10.
        let headers = fake_headers(5);
        let chain = MockChain::new();
        chain.add_verify(body_a.clone(), VerifyResult::Ok(headers));

        let mut transport = MockTransport::new(
            vec![peer_a],
            vec![proof_event(peer_a, body_a)],
        );

        let result = run_light_bootstrap(&mut transport, &chain).await;
        // Proof with fewer headers than k is treated as hostile.
        assert!(matches!(result, Err(LightBootstrapError::AllPeersHostile(_))));
        assert!(chain.installed().is_none());
    }

    #[tokio::test]
    async fn bootstrap_skips_already_populated_chain() {
        let peer_a = PeerId(1);
        let mut transport = MockTransport::new(vec![peer_a], vec![]);

        // Chain already has headers — MockChain returns 0, we need a non-zero one.
        struct PopulatedChain;
        impl crate::traits::SyncChain for PopulatedChain {
            async fn chain_height(&self) -> u32 { 100 }
            async fn build_sync_info(&self) -> Vec<u8> { vec![] }
            async fn header_at(&self, _h: u32) -> Option<Header> { None }
            async fn header_state_root(&self, _h: u32) -> Option<[u8; 33]> { None }
            fn parse_sync_info(&self, _b: &[u8]) -> Result<enr_chain::SyncInfo, ChainError> {
                unimplemented!()
            }
            async fn active_parameters(&self) -> ergo_validation::Parameters { unimplemented!() }
            async fn is_epoch_boundary(&self, _h: u32) -> bool { false }
            async fn compute_expected_parameters(
                &self, _h: u32,
            ) -> Result<ergo_validation::Parameters, ChainError> { unimplemented!() }
            async fn apply_epoch_boundary_parameters(&self, _p: ergo_validation::Parameters) {}
            async fn verify_nipopow_envelope(&self, _b: &[u8]) -> Result<Vec<Header>, ChainError> {
                unimplemented!()
            }
            async fn is_better_nipopow(&self, _a: &[u8], _b: &[u8]) -> Result<bool, ChainError> {
                unimplemented!()
            }
            async fn install_nipopow_suffix(
                &self, _h: Header, _t: Vec<Header>,
            ) -> Result<(), ChainError> { unimplemented!() }
        }

        let result = run_light_bootstrap(&mut transport, &PopulatedChain).await;
        assert!(result.is_ok()); // Should return immediately.
    }

    #[tokio::test]
    async fn bootstrap_broadcasts_to_all_peers() {
        let peers = vec![PeerId(1), PeerId(2), PeerId(3)];
        let body = vec![0xaa];
        let headers = fake_headers(15);
        let chain = MockChain::new();
        chain.add_verify(body.clone(), VerifyResult::Ok(headers));

        let mut transport = MockTransport::new(
            peers.clone(),
            vec![proof_event(PeerId(1), body)],
        );

        let _ = run_light_bootstrap(&mut transport, &chain).await;

        // Should have sent GetNipopowProof to all 3 peers.
        let sent = transport.sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        for (i, (peer, msg)) in sent.iter().enumerate() {
            assert_eq!(*peer, peers[i]);
            match msg {
                ProtocolMessage::Unknown { code, .. } => {
                    assert_eq!(*code, GET_NIPOPOW_PROOF);
                }
                _ => panic!("expected Unknown message"),
            }
        }
    }
}

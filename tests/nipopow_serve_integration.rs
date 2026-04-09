//! Integration test for the NiPoPoW serve P2P path.
//!
//! Connects to a running `ergo-node-rust` as an outbound peer, sends a
//! `GetNipopowProof` (code 90), waits for the `NipopowProof` (code 91)
//! response, parses it, and verifies it via `enr_chain::verify_nipopow_proof_bytes`.
//!
//! Exercises the full path: codec → handler dispatch → build → serialize →
//! framing → parse on receiver. The unit tests in `src/nipopow_serve.rs` cover
//! the codec; this test covers the live wire interaction with `handle_nipopow_event`.
//!
//! Marked `#[ignore]` because it requires an externally running node. Run with:
//!
//! ```text
//! NIPOPOW_TARGET=216.128.144.28:9030 cargo test --test nipopow_serve_integration \
//!     -- --ignored --nocapture
//! ```
//!
//! Default target is `127.0.0.1:9030`.

use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use ergo_chain_types::{BlockId, Digest32};
use enr_p2p::transport::connection::Connection;
use enr_p2p::transport::frame::Frame;
use enr_p2p::transport::handshake::{HandshakeConfig, ModeConfig};
use enr_p2p::types::{Network, ProxyMode, Version};

use ergo_node_rust::nipopow_serve::{
    parse_nipopow_proof, GET_NIPOPOW_PROOF, NIPOPOW_PROOF,
};

use sigma_ser::vlq_encode::WriteSigmaVlqExt;

use tokio::net::TcpStream;
use tokio::time::timeout;

const CONNECT_TIMEOUT_SECS: u64 = 10;
// Historical note: the in-memory `NipopowAlgos::prove` variant required
// materializing the full chain as `PoPowHeader`s and was `O(N)` — ~5
// minutes of wall time for a 100k-block chain. The anchored test below
// sidestepped that by using a tiny anchor. Since the switch to
// `prove_with_reader` (sigma-rust #851), builds are `O(m + k + m · log N)`
// popow header fetches per call and the full-chain path is sub-second;
// the no-anchor test at the bottom of this file exercises that.
const RESPONSE_TIMEOUT_SECS: u64 = 60;
// Upper bound on a full-chain build via the interlink walk.
// Algorithmically ~120 popow header fetches on a 270k chain — expected
// to finish in well under a second. Generous allowance for test server
// latency and serialization overhead.
const FULL_CHAIN_BUILD_TIMEOUT_SECS: u64 = 10;
// Anchor height: small enough for a fast build, large enough to satisfy
// `chain too short` (must be >= m + k).
const ANCHOR_HEIGHT: u32 = 200;
const DEFAULT_TARGET: &str = "127.0.0.1:9030";

#[tokio::test]
#[ignore = "requires a running ergo-node-rust on NIPOPOW_TARGET (default 127.0.0.1:9030)"]
async fn nipopow_serve_round_trip_against_running_node() {
    let target = env::var("NIPOPOW_TARGET").unwrap_or_else(|_| DEFAULT_TARGET.to_string());
    let addr: SocketAddr = target
        .parse()
        .expect("NIPOPOW_TARGET must be a valid SocketAddr");
    let network = Network::Testnet;

    eprintln!("connecting to {addr}");
    let stream = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), TcpStream::connect(addr))
        .await
        .expect("connect timed out")
        .expect("tcp connect failed");

    let hs_config = HandshakeConfig {
        agent_name: "ergo-rust-test-client".into(),
        peer_name: "nipopow-test-client".into(),
        version: Version::new(5, 0, 22),
        network,
        mode: ProxyMode::Light,
        declared_address: None,
        mode_config: ModeConfig::default(),
    };

    let mut conn = Connection::outbound(stream, &hs_config)
        .await
        .expect("handshake failed");
    eprintln!(
        "handshake complete: agent={} version={}",
        conn.peer_spec().agent,
        conn.peer_spec().version
    );

    // Build GetNipopowProof body: m=6, k=6, anchor at ANCHOR_HEIGHT, pad=0.
    // Format mirrors `parse_get_nipopow_proof` in src/nipopow_serve.rs.
    //
    // Hardcoded testnet header ID at height 200 (validated to exist via REST
    // `GET /blocks/at/200`). Using an explicit anchor keeps the build small —
    // ~200 extension reads instead of full validated tip — so the test
    // exercises the wire path without waiting on a multi-minute proof build.
    let anchor_id_hex = "d14ceef220e812325b008611861eede0c3bc267ba3111138452e8125c03b3303";
    let anchor_bytes: [u8; 32] = hex::decode(anchor_id_hex)
        .expect("hex decode")
        .try_into()
        .expect("32 bytes");
    let anchor = BlockId(Digest32::from(anchor_bytes));
    let _ = ANCHOR_HEIGHT; // documented above; the hash above is anchored at this height

    let mut body = Vec::new();
    body.put_i32(6).expect("vec write");
    body.put_i32(6).expect("vec write");
    body.push(1); // header_id_present = 1
    body.extend_from_slice(&anchor.0 .0);
    body.put_u16(0).expect("vec write"); // pad_len = 0

    eprintln!("sending GetNipopowProof code={GET_NIPOPOW_PROOF} body_len={}", body.len());
    conn.write_frame(&Frame { code: GET_NIPOPOW_PROOF, body })
        .await
        .expect("write frame failed");

    // Read frames until we see code 91 NipopowProof or time out. The peer
    // may interleave SyncInfo, Inv, and other messages — ignore them.
    let proof_bytes = timeout(Duration::from_secs(RESPONSE_TIMEOUT_SECS), async {
        loop {
            let frame = conn
                .read_frame()
                .await
                .expect("read frame failed");
            eprintln!("received frame code={} body_len={}", frame.code, frame.body.len());
            if frame.code == NIPOPOW_PROOF {
                return parse_nipopow_proof(&frame.body)
                    .expect("parse_nipopow_proof failed");
            }
        }
    })
    .await
    .expect("timed out waiting for NipopowProof response");

    eprintln!("got NipopowProof inner bytes: {} bytes", proof_bytes.len());

    let meta = enr_chain::verify_nipopow_proof_bytes(&proof_bytes)
        .expect("verify_nipopow_proof_bytes failed");

    eprintln!(
        "verified: suffix_tip_height={} total_headers={} continuous={}",
        meta.suffix_tip_height, meta.total_headers, meta.continuous
    );

    // m=6 k=6 means at minimum the proof carries the k-suffix (6 headers).
    // The prefix can be empty on a short chain, so 6 is the floor we can rely on.
    assert!(
        meta.total_headers >= 6,
        "expected at least k=6 headers in the proof, got {}",
        meta.total_headers
    );
    assert!(
        meta.suffix_tip_height > 0,
        "suffix tip height should be > 0"
    );
}

/// Full-chain serve perf check: send `GetNipopowProof` with an explicit
/// anchor deep in the validated range (~228k on the test server), so
/// the server builds a proof over a ~228k-block prefix. With the
/// pre-#851 in-memory `prove` path this would take ~4 minutes of wall
/// time; with `prove_with_reader` it's sub-second because the
/// interlink walk only visits ~m·log₂(N) ≈ 120 headers regardless of
/// chain length.
///
/// Asserts the round-trip completes inside
/// [`FULL_CHAIN_BUILD_TIMEOUT_SECS`] and prints the measured wall time
/// so it's visible in `--nocapture` output.
///
/// Note: we use an explicit deep anchor (not `header_id=None`) because
/// main.rs's no-anchor path currently requires `shared_validated_height`
/// to be non-zero, and that atomic is reset to 0 on every binary
/// restart — it's only populated as the validator processes new blocks
/// after startup. Using an explicit anchor bypasses that check while
/// still exercising the full-chain prove path. The pre-existing
/// `shared_validated_height` initialization bug is tracked separately.
#[tokio::test]
#[ignore = "requires a running ergo-node-rust on NIPOPOW_TARGET (default 127.0.0.1:9030)"]
async fn nipopow_serve_full_chain_round_trip() {
    let target = env::var("NIPOPOW_TARGET").unwrap_or_else(|_| DEFAULT_TARGET.to_string());
    let addr: SocketAddr = target
        .parse()
        .expect("NIPOPOW_TARGET must be a valid SocketAddr");
    let network = Network::Testnet;

    eprintln!("[full-chain] connecting to {addr}");
    let stream = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), TcpStream::connect(addr))
        .await
        .expect("connect timed out")
        .expect("tcp connect failed");

    let hs_config = HandshakeConfig {
        agent_name: "ergo-rust-test-client".into(),
        peer_name: "nipopow-perf-test-client".into(),
        version: Version::new(5, 0, 22),
        network,
        mode: ProxyMode::Light,
        declared_address: None,
        mode_config: ModeConfig::default(),
    };

    let mut conn = Connection::outbound(stream, &hs_config)
        .await
        .expect("handshake failed");
    eprintln!(
        "[full-chain] handshake complete: agent={} version={}",
        conn.peer_spec().agent,
        conn.peer_spec().version
    );

    // Build GetNipopowProof body: m=6, k=6, explicit deep anchor, pad=0.
    // Anchor at testnet height 50000 — deep enough to force a meaningful
    // interlink walk (~16 levels) but under the 69029 modifiers.redb
    // gap on the current test server (see project_validator_stuck_apr4).
    let anchor_id_hex = "f323bca8a5367839836cd071cc9d1305e16810fe7855d0f286feb6d1ede96b12";
    let anchor_bytes: [u8; 32] = hex::decode(anchor_id_hex)
        .expect("hex decode")
        .try_into()
        .expect("32 bytes");
    let anchor = BlockId(Digest32::from(anchor_bytes));

    let mut body = Vec::new();
    body.put_i32(6).expect("vec write");
    body.put_i32(6).expect("vec write");
    body.push(1); // header_id_present = 1
    body.extend_from_slice(&anchor.0 .0);
    body.put_u16(0).expect("vec write"); // pad_len = 0

    eprintln!(
        "[full-chain] sending GetNipopowProof code={GET_NIPOPOW_PROOF} body_len={} (anchor=height~228000)",
        body.len()
    );
    let send_at = std::time::Instant::now();
    conn.write_frame(&Frame { code: GET_NIPOPOW_PROOF, body })
        .await
        .expect("write frame failed");

    let proof_bytes = timeout(
        Duration::from_secs(FULL_CHAIN_BUILD_TIMEOUT_SECS),
        async {
            loop {
                let frame = conn
                    .read_frame()
                    .await
                    .expect("read frame failed");
                eprintln!(
                    "[full-chain] received frame code={} body_len={}",
                    frame.code,
                    frame.body.len()
                );
                if frame.code == NIPOPOW_PROOF {
                    return parse_nipopow_proof(&frame.body)
                        .expect("parse_nipopow_proof failed");
                }
            }
        },
    )
    .await
    .expect("full-chain build timed out — perf regression?");
    let elapsed = send_at.elapsed();

    eprintln!(
        "[full-chain] got NipopowProof inner bytes: {} bytes in {:.3}s",
        proof_bytes.len(),
        elapsed.as_secs_f64()
    );

    let meta = enr_chain::verify_nipopow_proof_bytes(&proof_bytes)
        .expect("verify_nipopow_proof_bytes failed");

    eprintln!(
        "[full-chain] verified: suffix_tip_height={} total_headers={} continuous={} wall_time={:.3}s",
        meta.suffix_tip_height, meta.total_headers, meta.continuous, elapsed.as_secs_f64()
    );

    assert!(
        meta.total_headers >= 6,
        "expected at least k=6 headers in the proof, got {}",
        meta.total_headers
    );
    assert!(
        meta.suffix_tip_height > 200,
        "full-chain suffix tip height should be much larger than the 200-anchor test, got {}",
        meta.suffix_tip_height
    );
}

/// Regression test for the `has_valid_connections` JVM-tolerance bug.
///
/// Sends `GetNipopowProof(m=6, k=10, header_id=None)` to a running node,
/// which on our test server is forwarded by the router to JVM peers (per
/// `project_unknown_message_forwarding`). The JVM peers reply with proofs
/// whose prefix has skipped intermediate entries — adjacent sorted-by-
/// height pairs that don't directly reference each other via interlinks
/// because they come from different superlevel walks.
///
/// Pre-fix sigma-rust `NipopowProof::has_valid_connections` was strict —
/// it required immediate-predecessor connectivity and rejected these
/// proofs with `Nipopow("invalid connections")`. The fix
/// (`mwaddip/sigma-rust:fix/nipopow-prefix-connection-lookback`,
/// integrated as `1e3fe28` on `ergo-node-integration`) ports JVM's
/// `useLastEpochs + 2` lookback window so the verifier accepts proofs
/// where each entry connects to ANY of the up-to-10 immediately preceding
/// entries.
///
/// This test passes against any peer (JVM or Rust) that produces valid
/// JVM-shape proofs. Pre-fix it failed; post-fix it passes. Keeping it
/// as a permanent regression check for the connection-tolerance fix.
#[tokio::test]
#[ignore = "requires a running ergo-node-rust on NIPOPOW_TARGET (default 127.0.0.1:9030)"]
async fn nipopow_serve_no_anchor_repro() {
    let target = env::var("NIPOPOW_TARGET").unwrap_or_else(|_| DEFAULT_TARGET.to_string());
    let addr: SocketAddr = target.parse().expect("NIPOPOW_TARGET must be a valid SocketAddr");

    eprintln!("[no-anchor] connecting to {addr}");
    let stream = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), TcpStream::connect(addr))
        .await
        .expect("connect timed out")
        .expect("tcp connect failed");

    let hs_config = HandshakeConfig {
        agent_name: "ergo-rust-test-client".into(),
        peer_name: "nipopow-no-anchor-client".into(),
        version: Version::new(5, 0, 22),
        network: Network::Testnet,
        mode: ProxyMode::Light,
        declared_address: None,
        mode_config: ModeConfig::default(),
    };

    let mut conn = Connection::outbound(stream, &hs_config)
        .await
        .expect("handshake failed");

    // m=6, k=10, header_id=None — exact light-client bootstrap params.
    let mut body = Vec::new();
    body.put_i32(6).expect("vec write");
    body.put_i32(10).expect("vec write");
    body.push(0); // header_id_present = 0
    body.put_u16(0).expect("vec write"); // pad_len = 0

    conn.write_frame(&Frame { code: GET_NIPOPOW_PROOF, body })
        .await
        .expect("write frame failed");

    let proof_bytes = timeout(Duration::from_secs(FULL_CHAIN_BUILD_TIMEOUT_SECS), async {
        loop {
            let frame = conn.read_frame().await.expect("read frame failed");
            if frame.code == NIPOPOW_PROOF {
                return parse_nipopow_proof(&frame.body).expect("parse_nipopow_proof failed");
            }
        }
    })
    .await
    .expect("timed out waiting for NipopowProof response");

    let meta = enr_chain::verify_nipopow_proof_bytes(&proof_bytes)
        .expect("verify_nipopow_proof_bytes failed (regression: tolerant lookback?)");

    eprintln!(
        "[no-anchor] verified OK: proof_bytes={} suffix_tip_height={} total_headers={}",
        proof_bytes.len(),
        meta.suffix_tip_height,
        meta.total_headers,
    );

    assert!(
        meta.total_headers >= 10,
        "expected at least k=10 headers in the proof, got {}",
        meta.total_headers
    );
    assert!(
        meta.suffix_tip_height > 200,
        "no-anchor request should resolve to a deep tip, got {}",
        meta.suffix_tip_height
    );
}

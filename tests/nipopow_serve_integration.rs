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
// build_nipopow_proof walks 1..=anchor_height pulling extensions. With a
// small explicit anchor, the build is milliseconds. Without an anchor
// it defaults to the validated tip, which on a 100k-block chain is
// currently ~5 minutes of single-threaded work (perf issue tracked
// separately). This test uses a small anchor so it verifies the wire
// path without blocking on the full-chain build.
const RESPONSE_TIMEOUT_SECS: u64 = 60;
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

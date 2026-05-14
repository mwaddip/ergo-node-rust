//! Ergo P2P handshake: build and parse.
//!
//! # Contract
//! - `build`: given a `HandshakeConfig`, produces raw handshake bytes.
//!   Postcondition: bytes are parseable by `parse`, and contain the configured
//!   Mode and Session features with correct encoding.
//! - `parse`: given raw bytes, extracts a `PeerSpec`.
//!   Precondition: bytes are a valid Ergo handshake payload.
//!   Postcondition: all fields populated; features parsed but unknown IDs are preserved.
//! - `validate_peer`: checks version >= 4.0.100 and session magic matches network.

use crate::transport::vlq;
use crate::types::{Network, ProxyMode, Version};
use std::io::{self, Cursor, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

const FEATURE_MODE: u8 = 16;
const FEATURE_SESSION: u8 = 3;
/// Custom feature identifying this peer as a proxy. JVM nodes ignore unknown features.
/// Other proxies detect this and avoid treating each other as outbound content sources.
pub const FEATURE_PROXY: u8 = 64;

/// What to advertise in the Mode feature (ID 16) during handshake.
///
/// The main crate constructs this from the node config — the P2P crate
/// encodes these values but doesn't interpret them.
#[derive(Debug, Clone, Copy)]
pub struct ModeConfig {
    /// State type byte: 0 = UTXO, 1 = Digest.
    pub state_type_id: u8,
    /// Whether this node verifies transactions (false = SPV/header-only).
    pub verifying: bool,
    /// How many blocks to keep. -1 = all (archival), 0 = none (header-only).
    pub blocks_to_keep: i32,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            state_type_id: 0, // UTXO
            verifying: true,
            blocks_to_keep: -1, // keep all
        }
    }
}

/// Configuration for building a handshake.
pub struct HandshakeConfig {
    pub agent_name: String,
    pub peer_name: String,
    pub version: Version,
    pub network: Network,
    pub mode: ProxyMode,
    pub declared_address: Option<SocketAddr>,
    /// Mode feature advertisement — what capabilities to claim in the handshake.
    pub mode_config: ModeConfig,
}

/// A parsed peer feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feature {
    pub id: u8,
    pub body: Vec<u8>,
}

/// Parsed peer specification from a handshake.
#[derive(Debug, Clone)]
pub struct PeerSpec {
    pub agent: String,
    pub version: Version,
    pub name: String,
    pub address: Option<SocketAddr>,
    pub features: Vec<Feature>,
}

/// Build handshake bytes from config.
pub fn build(config: &HandshakeConfig) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);

    // Timestamp
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    vlq::write_vlq(&mut buf, now);

    write_peer_entry_from_config(config, &mut buf);
    buf
}

/// Serialize a `PeerSpec` in the wire-format used inside a `Peers` message
/// body (and after the timestamp inside a handshake). Inverse of
/// [`parse_peer_entry`].
pub fn serialize_peer_entry(spec: &PeerSpec, buf: &mut Vec<u8>) {
    vlq::write_short_string(buf, &spec.agent);
    buf.push(spec.version.major);
    buf.push(spec.version.minor);
    buf.push(spec.version.patch);
    vlq::write_short_string(buf, &spec.name);

    write_declared_address(spec.address.as_ref(), buf);

    debug_assert!(spec.features.len() <= u8::MAX as usize, "feature count overflow");
    buf.push(spec.features.len() as u8);
    for feature in &spec.features {
        buf.push(feature.id);
        // Feature body length is VLQ-encoded (Scorex putUShort → putUInt → putULong → VLQ).
        vlq::write_vlq(buf, feature.body.len() as u64);
        buf.extend_from_slice(&feature.body);
    }
}

fn write_peer_entry_from_config(config: &HandshakeConfig, buf: &mut Vec<u8>) {
    vlq::write_short_string(buf, &config.agent_name);

    buf.push(config.version.major);
    buf.push(config.version.minor);
    buf.push(config.version.patch);

    vlq::write_short_string(buf, &config.peer_name);

    write_declared_address(config.declared_address.as_ref(), buf);

    buf.push(2); // feature count: Mode + Session

    buf.push(FEATURE_MODE);
    let mode_body = build_mode_body(config);
    vlq::write_vlq(buf, mode_body.len() as u64);
    buf.extend_from_slice(&mode_body);

    buf.push(FEATURE_SESSION);
    let session_body = build_session_body(config.network);
    vlq::write_vlq(buf, session_body.len() as u64);
    buf.extend_from_slice(&session_body);
}

fn write_declared_address(addr: Option<&SocketAddr>, buf: &mut Vec<u8>) {
    match addr {
        None => buf.push(0x00),
        Some(addr) => {
            buf.push(0x01);
            let ip_bytes: Vec<u8> = match addr.ip() {
                IpAddr::V4(ip) => ip.octets().to_vec(),
                IpAddr::V6(ip) => ip.octets().to_vec(),
            };
            buf.push((ip_bytes.len() + 4) as u8);
            buf.extend_from_slice(&ip_bytes);
            // Port is VLQ-encoded (Scorex putUInt → putULong → VLQ).
            vlq::write_vlq(buf, addr.port() as u64);
        }
    }
}

fn build_mode_body(config: &HandshakeConfig) -> Vec<u8> {
    let mc = &config.mode_config;
    let mut body = Vec::with_capacity(8);

    // stateType: 0 = UTXO, 1 = Digest
    body.push(mc.state_type_id);

    // verifying: whether we verify transactions
    body.push(if mc.verifying { 0x01 } else { 0x00 });

    // nipopow: NiPoPoW bootstrap flag
    match config.mode {
        ProxyMode::Full => body.push(0x00),    // None
        ProxyMode::Light => {
            body.push(0x01);                    // Some
            vlq::write_vlq(&mut body, vlq::zigzag_encode_i64(1)); // value=1 (KMZ17)
        }
    }

    // blocksToKeep: zigzag-encoded VLQ (JVM putInt → zigzag → VLQ)
    vlq::write_vlq(&mut body, vlq::zigzag_encode_i64(mc.blocks_to_keep as i64));

    body
}

fn build_session_body(network: Network) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&network.magic());
    // Session ID is putLong = ZigZag encode then VLQ
    let session_id = rand_u64() as i64;
    let zigzag = vlq::zigzag_encode_i64(session_id);
    vlq::write_vlq(&mut body, zigzag);
    body
}

fn rand_u64() -> u64 {
    let mut buf = [0u8; 8];
    #[cfg(unix)]
    {
        use std::fs::File;
        let mut f = File::open("/dev/urandom").expect("/dev/urandom");
        f.read_exact(&mut buf).expect("read urandom");
    }
    #[cfg(not(unix))]
    {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        buf = t.to_le_bytes();
    }
    u64::from_le_bytes(buf)
}

/// Parse handshake bytes into a PeerSpec.
pub fn parse(data: &[u8]) -> io::Result<PeerSpec> {
    let mut cursor = Cursor::new(data);

    // Timestamp (consume but don't use)
    let _timestamp = vlq::read_vlq(&mut cursor)?;

    // Agent name
    let agent = vlq::read_short_string(&mut cursor)?;

    // Version
    let mut ver = [0u8; 3];
    cursor.read_exact(&mut ver)?;
    let version = Version::new(ver[0], ver[1], ver[2]);

    // Peer name
    let name = vlq::read_short_string(&mut cursor)?;

    // Declared address
    let mut has_addr = [0u8; 1];
    cursor.read_exact(&mut has_addr)?;
    let address = if has_addr[0] != 0 {
        parse_address(&mut cursor)?
    } else {
        None
    };

    // Features
    let features = parse_features(&mut cursor)?;

    Ok(PeerSpec { agent, version, name, address, features })
}

/// Parse a peer entry from a Peers message body.
/// Same format as handshake PeerSpec but without the timestamp prefix.
pub fn parse_peer_entry(cursor: &mut Cursor<&[u8]>) -> io::Result<PeerSpec> {
    let agent = vlq::read_short_string(cursor)?;

    let mut ver = [0u8; 3];
    cursor.read_exact(&mut ver)?;
    let version = Version::new(ver[0], ver[1], ver[2]);

    let name = vlq::read_short_string(cursor)?;

    let mut has_addr = [0u8; 1];
    cursor.read_exact(&mut has_addr)?;
    let address = if has_addr[0] != 0 {
        parse_address(cursor)?
    } else {
        None
    };

    let features = parse_features(cursor)?;

    Ok(PeerSpec { agent, version, name, address, features })
}

fn parse_address<R: Read>(reader: &mut R) -> io::Result<Option<SocketAddr>> {
    let mut addr_len_byte = [0u8; 1];
    reader.read_exact(&mut addr_len_byte)?;
    let ip_len = (addr_len_byte[0] as usize).saturating_sub(4);
    let mut ip_bytes = vec![0u8; ip_len];
    reader.read_exact(&mut ip_bytes)?;
    // Port is VLQ-encoded (Scorex getUInt → VLQ)
    let port = vlq::read_vlq(reader)? as u16;

    match ip_len {
        4 => Ok(Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3])),
            port,
        ))),
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&ip_bytes);
            Ok(Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port)))
        }
        _ => Ok(None),
    }
}

fn parse_features<R: Read>(reader: &mut R) -> io::Result<Vec<Feature>> {
    let mut features = Vec::new();
    let mut feat_count_buf = [0u8; 1];
    reader.read_exact(&mut feat_count_buf)?;
    let feat_count = feat_count_buf[0] as usize;
    for _ in 0..feat_count {
        let mut fid = [0u8; 1];
        reader.read_exact(&mut fid)?;
        // Feature body length is VLQ-encoded (Scorex getUShort → VLQ)
        let flen = vlq::read_vlq_length(reader)?;
        let mut fbody = vec![0u8; flen];
        reader.read_exact(&mut fbody)?;
        features.push(Feature { id: fid[0], body: fbody });
    }
    Ok(features)
}

/// Validate a peer's handshake.
///
/// # Contract
/// - **Precondition**: `spec` was produced by `parse`.
/// - **Postcondition**: returns Ok(()) if version >= 4.0.100 and session magic matches, Err otherwise.
pub fn validate_peer(spec: &PeerSpec, network: &Network) -> Result<(), String> {
    if spec.version < Version::EIP37_MIN {
        return Err(format!(
            "Peer version {} is below minimum {} (EIP-37)",
            spec.version,
            Version::EIP37_MIN
        ));
    }

    if let Some(session) = spec.features.iter().find(|f| f.id == FEATURE_SESSION) {
        if session.body.len() >= 4 {
            let peer_magic = &session.body[0..4];
            let expected = network.magic();
            if peer_magic != expected {
                return Err(format!(
                    "Session magic mismatch: {:?} (expected {:?})",
                    peer_magic, expected
                ));
            }
        }
    }

    Ok(())
}

impl PeerSpec {
    /// Extract the REST API URL advertised by this peer (feature ID 4).
    ///
    /// JVM peers encode this as length-prefixed UTF-8 in the feature body.
    /// Returns `None` if the feature is absent or the body is malformed.
    pub fn rest_api_url(&self) -> Option<String> {
        self.features.iter()
            .find(|f| f.id == 4)
            .and_then(|f| {
                let body = &f.body;
                if body.is_empty() { return None; }
                let len = body[0] as usize;
                if body.len() < 1 + len { return None; }
                std::str::from_utf8(&body[1..1 + len]).ok().map(String::from)
            })
    }
}

/// Check if a peer's handshake indicates it is a proxy.
pub fn is_proxy(spec: &PeerSpec) -> bool {
    spec.features.iter().any(|f| f.id == FEATURE_PROXY)
}

/// Measure the exact byte size of a handshake by parsing it with a cursor.
pub fn measure_size(data: &[u8]) -> io::Result<usize> {
    let mut cursor = Cursor::new(data);

    vlq::read_vlq(&mut cursor)?; // timestamp
    vlq::read_short_string(&mut cursor)?; // agent name

    let mut ver = [0u8; 3];
    cursor.read_exact(&mut ver)?; // version

    vlq::read_short_string(&mut cursor)?; // peer name

    // Declared address
    let mut has_addr = [0u8; 1];
    cursor.read_exact(&mut has_addr)?;
    if has_addr[0] != 0 {
        let mut addr_len_byte = [0u8; 1];
        cursor.read_exact(&mut addr_len_byte)?;
        let ip_len = (addr_len_byte[0] as usize).saturating_sub(4);
        let mut ip_bytes = vec![0u8; ip_len];
        cursor.read_exact(&mut ip_bytes)?;
        vlq::read_vlq(&mut cursor)?; // port
    }

    // Features
    let mut feat_count_buf = [0u8; 1];
    cursor.read_exact(&mut feat_count_buf)?;
    let feat_count = feat_count_buf[0] as usize;
    for _ in 0..feat_count {
        let mut fid = [0u8; 1];
        cursor.read_exact(&mut fid)?;
        let flen = vlq::read_vlq_length(&mut cursor)?;
        let mut fbody = vec![0u8; flen];
        cursor.read_exact(&mut fbody)?;
    }

    Ok(cursor.position() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(features: Vec<Feature>) -> PeerSpec {
        PeerSpec {
            agent: "test".into(),
            version: Version::new(5, 0, 0),
            name: "node".into(),
            address: None,
            features,
        }
    }

    #[test]
    fn rest_api_url_valid() {
        let url = "http://1.2.3.4:9052";
        let mut body = vec![url.len() as u8];
        body.extend_from_slice(url.as_bytes());
        let spec = make_spec(vec![Feature { id: 4, body }]);
        assert_eq!(spec.rest_api_url(), Some(url.to_string()));
    }

    #[test]
    fn rest_api_url_empty_body() {
        let spec = make_spec(vec![Feature { id: 4, body: vec![] }]);
        assert_eq!(spec.rest_api_url(), None);
    }

    #[test]
    fn rest_api_url_truncated_body() {
        // Length byte says 20, but only 5 bytes follow
        let mut body = vec![20];
        body.extend_from_slice(b"short");
        let spec = make_spec(vec![Feature { id: 4, body }]);
        assert_eq!(spec.rest_api_url(), None);
    }

    #[test]
    fn rest_api_url_no_feature() {
        let spec = make_spec(vec![Feature { id: 16, body: vec![0] }]);
        assert_eq!(spec.rest_api_url(), None);
    }
}


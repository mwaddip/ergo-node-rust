use std::fmt;

/// Unique identifier for a connected peer. Wraps a u64 counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub u64);

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer-{}", self.0)
    }
}

/// 32-byte modifier identifier (block, transaction, header, etc.).
pub type ModifierId = [u8; 32];

/// Protocol version as three bytes: major.minor.patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

impl Version {
    pub const EIP37_MIN: Version = Version { major: 4, minor: 0, patch: 100 };

    pub const fn new(major: u8, minor: u8, patch: u8) -> Self {
        Self { major, minor, patch }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Direction of a peer connection relative to this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// We initiated the connection (we connect to them).
    Outbound,
    /// They initiated the connection (they connect to us).
    Inbound,
}

/// Network identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    pub const fn magic(&self) -> [u8; 4] {
        match self {
            Network::Mainnet => [1, 0, 2, 4],
            Network::Testnet => [2, 3, 2, 3],
        }
    }
}

/// Proxy mode for a listener — controls handshake advertising and routing behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Full proxy: forward all messages, advertise as full archival node.
    Full,
    /// Light: gossip only, advertise as NiPoPoW-bootstrapped.
    Light,
}

use crate::types::{Network, ProxyMode};
use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub listen: ListenConfig,
    pub outbound: OutboundConfig,
    pub identity: IdentityConfig,
    pub network: Option<NetworkConfig>,
    /// UPnP port mapping configuration. Defaults to disabled.
    #[serde(default)]
    pub upnp: UpnpConfig,
}

/// Network-level settings matching the JVM's `scorex.network` config.
/// All fields have defaults matching JVM behavior.
#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// Interval between GetPeers keepalive messages.
    #[serde(default = "default_get_peers_interval")]
    pub get_peers_interval_secs: u64,
    /// Timeout for modifier delivery.
    #[serde(default = "default_delivery_timeout")]
    pub delivery_timeout_secs: u64,
    /// Max re-request attempts before abandoning a modifier.
    #[serde(default = "default_max_delivery_checks")]
    pub max_delivery_checks: u32,
    /// Desired number of modifier IDs per Inv/Request batch.
    #[serde(default = "default_desired_inv_objects")]
    pub desired_inv_objects: u32,
    /// Maximum PeerSpec objects in one Peers message.
    #[serde(default = "default_max_peer_spec_objects")]
    pub max_peer_spec_objects: u32,
    /// Handshake timeout.
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_secs: u64,
    /// Drop connections inactive for this duration.
    #[serde(default = "default_inactive_connection_deadline")]
    pub inactive_connection_deadline_secs: u64,
    /// Default temporary ban duration in minutes.
    #[serde(default = "default_temporal_ban_duration")]
    pub temporal_ban_duration_mins: u64,
    /// Filter bogus peer addresses (CGNAT, RFC1918, link-local, loopback,
    /// etc.) out of `Peers` intake and `GetPeers` responses. Default `true`
    /// = JVM 6.0.3 parity (filter, no penalty). When `false`, every
    /// syntactically-valid address is ingested, eligible for outbound fill,
    /// and gossiped onward. Never affects the malformed-Peers ban or the
    /// self-address filter.
    #[serde(default = "default_filter_bogus_addresses")]
    pub filter_bogus_addresses: bool,
}

/// UPnP configuration. IPv4 only — IPv6 addresses are globally routable.
#[derive(Debug, Clone, Deserialize)]
pub struct UpnpConfig {
    /// Enable UPnP port mapping discovery. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Gateway discovery timeout in seconds. Default: 5.
    #[serde(default = "default_discover_timeout")]
    pub discover_timeout_secs: u64,
}

fn default_discover_timeout() -> u64 {
    5
}

impl Default for UpnpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            discover_timeout_secs: default_discover_timeout(),
        }
    }
}

fn default_get_peers_interval() -> u64 {
    120
}
fn default_delivery_timeout() -> u64 {
    10
}
fn default_max_delivery_checks() -> u32 {
    100
}
fn default_desired_inv_objects() -> u32 {
    400
}
fn default_max_peer_spec_objects() -> u32 {
    64
}
fn default_handshake_timeout() -> u64 {
    30
}
fn default_inactive_connection_deadline() -> u64 {
    600
}
fn default_temporal_ban_duration() -> u64 {
    60
}
fn default_filter_bogus_addresses() -> bool {
    true
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            get_peers_interval_secs: default_get_peers_interval(),
            delivery_timeout_secs: default_delivery_timeout(),
            max_delivery_checks: default_max_delivery_checks(),
            desired_inv_objects: default_desired_inv_objects(),
            max_peer_spec_objects: default_max_peer_spec_objects(),
            handshake_timeout_secs: default_handshake_timeout(),
            inactive_connection_deadline_secs: default_inactive_connection_deadline(),
            temporal_ban_duration_mins: default_temporal_ban_duration(),
            filter_bogus_addresses: default_filter_bogus_addresses(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ProxyConfig {
    pub network: Network,
}

#[derive(Debug, Deserialize)]
pub struct ListenConfig {
    pub ipv6: Option<ListenerConfig>,
    pub ipv4: Option<ListenerConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ListenerConfig {
    pub address: SocketAddr,
    pub mode: ProxyMode,
    pub max_inbound: usize,
}

#[derive(Debug, Deserialize)]
pub struct OutboundConfig {
    pub min_peers: usize,
    pub max_peers: usize,
    pub seed_peers: Vec<SocketAddr>,
}

#[derive(Debug, Deserialize)]
pub struct IdentityConfig {
    pub agent_name: String,
    pub peer_name: String,
    pub protocol_version: String,
}

impl Config {
    /// Parse a config from TOML text **already in memory**.
    ///
    /// The unit of configuration is a document, not a file. A layered
    /// `/etc/ergo-node/conf.d/` merges several files into one effective
    /// config that exists nowhere on disk; this is the entry point that can
    /// express it. Do not serialise a merged document to a temp file to reach
    /// [`Config::load`] — that is the adapter the Interface Integrity rule
    /// forbids.
    ///
    /// # Contract
    /// - **Precondition**: none. The caller owns where the text came from.
    /// - **Postcondition**: identical to what [`Config::load`] produces for a
    ///   file with the same contents — there is exactly one parse, and this is
    ///   it. Returns a valid `Config` with at least one listener and at least
    ///   one seed peer, or an error.
    pub fn from_toml_str(toml: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config: Config = toml::from_str(toml)?;

        if config.listen.ipv4.is_none() && config.listen.ipv6.is_none() {
            return Err("At least one listener (ipv4 or ipv6) must be configured".into());
        }

        if config.outbound.seed_peers.is_empty() {
            return Err("At least one seed peer must be configured".into());
        }

        if config.outbound.min_peers > config.outbound.max_peers {
            return Err("min_peers must be <= max_peers".into());
        }

        Ok(config)
    }

    /// Load config from a TOML file.
    ///
    /// Defined as [`Config::from_toml_str`] over the file's contents, so the
    /// two entry points cannot drift.
    ///
    /// # Contract
    /// - **Precondition**: `path` points to a readable TOML file.
    /// - **Postcondition**: Returns a valid `Config` with at least one listener
    ///   and at least one seed peer, or an error.
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        Self::from_toml_str(&content)
    }

    /// Returns network settings, using defaults if the `[network]` section is absent.
    pub fn network_settings(&self) -> NetworkConfig {
        self.network.clone().unwrap_or_default()
    }

    /// Parse the protocol version string into (major, minor, patch).
    ///
    /// # Contract
    /// - **Precondition**: `identity.protocol_version` is "X.Y.Z" format.
    /// - **Postcondition**: Returns three u8 values.
    pub fn version_bytes(&self) -> Result<(u8, u8, u8), Box<dyn std::error::Error>> {
        let parts: Vec<&str> = self.identity.protocol_version.split('.').collect();
        if parts.len() != 3 {
            return Err(format!("Invalid version: {}", self.identity.protocol_version).into());
        }
        Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid TOML without a [upnp] section — upnp should default to disabled.
    const MINIMAL_TOML: &str = r#"
[proxy]
network = "testnet"

[listen.ipv4]
address = "0.0.0.0:9020"
mode = "full"
max_inbound = 10

[outbound]
min_peers = 1
max_peers = 5
seed_peers = ["213.239.193.208:9030"]

[identity]
agent_name = "ergo-test"
peer_name = "test-node"
protocol_version = "5.0.25"
"#;

    #[test]
    fn upnp_defaults_to_disabled_when_absent() {
        let config: Config = toml::from_str(MINIMAL_TOML).unwrap();
        assert!(!config.upnp.enabled);
        assert_eq!(config.upnp.discover_timeout_secs, 5);
    }

    #[test]
    fn upnp_enabled_from_toml() {
        let toml_str = format!(
            "{}\n[upnp]\nenabled = true\ndiscover_timeout_secs = 10\n",
            MINIMAL_TOML
        );
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert!(config.upnp.enabled);
        assert_eq!(config.upnp.discover_timeout_secs, 10);
    }

    #[test]
    fn upnp_enabled_uses_default_timeout() {
        let toml_str = format!("{}\n[upnp]\nenabled = true\n", MINIMAL_TOML);
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert!(config.upnp.enabled);
        assert_eq!(config.upnp.discover_timeout_secs, 5);
    }

    /// Exercises every section, so the equivalence check below compares more
    /// than the handful of fields validation happens to touch.
    const FULL_TOML: &str = r#"
[proxy]
network = "mainnet"

[listen.ipv4]
address = "0.0.0.0:9030"
mode = "full"
max_inbound = 30

[listen.ipv6]
address = "[::]:9030"
mode = "full"
max_inbound = 30

[outbound]
min_peers = 4
max_peers = 20
seed_peers = ["213.239.193.208:9030", "[2a01:4f8:1c17:6bf::2]:9030"]

[identity]
agent_name = "ergo-node-rust"
peer_name = "equivalence-fixture"
protocol_version = "5.0.25"

[network]
get_peers_interval_secs = 90
desired_inv_objects = 200
filter_bogus_addresses = false

[upnp]
enabled = true
discover_timeout_secs = 7
"#;

    #[test]
    fn from_toml_str_parses_minimal_config() {
        let config = Config::from_toml_str(MINIMAL_TOML).expect("minimal config is valid");

        assert_eq!(config.proxy.network, Network::Testnet);
        assert_eq!(config.outbound.min_peers, 1);
        assert_eq!(config.outbound.max_peers, 5);
        assert_eq!(config.outbound.seed_peers.len(), 1);
        assert_eq!(config.identity.agent_name, "ergo-test");
        assert_eq!(config.identity.protocol_version, "5.0.25");
        assert!(config.listen.ipv4.is_some());
        assert!(config.listen.ipv6.is_none());
        // No [network] section: absent, with `network_settings()` filling defaults.
        assert!(config.network.is_none());
        assert!(!config.upnp.enabled);
    }

    /// The property this whole split exists to protect: identical text must
    /// produce an identical `Config` whether it arrived via a path or in
    /// memory. `Config` derives `Debug` but not `PartialEq`; the derived
    /// `Debug` rendering covers every field of every nested struct, so
    /// comparing it is a full structural comparison — not merely the three
    /// fields validation happens to read — without adding a trait the crate
    /// does not otherwise need.
    #[test]
    fn from_toml_str_matches_load_for_identical_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ergo.toml");
        std::fs::write(&path, FULL_TOML).unwrap();

        let from_file = Config::load(path.to_str().unwrap()).expect("fixture loads from disk");
        let from_memory = Config::from_toml_str(FULL_TOML).expect("fixture parses in memory");

        assert_eq!(format!("{:?}", from_file), format!("{:?}", from_memory));
    }

    /// Equivalence on the error side too: a `from_toml_str` that accepted a
    /// document `load` rejects would hand the caller a config the node was
    /// never meant to run with.
    #[test]
    fn from_toml_str_enforces_the_same_validation_as_load() {
        let no_listener = MINIMAL_TOML.replace("[listen.ipv4]", "[listen.unused]");
        let no_seeds = MINIMAL_TOML.replace(
            r#"seed_peers = ["213.239.193.208:9030"]"#,
            "seed_peers = []",
        );
        let inverted_peers = MINIMAL_TOML.replace("min_peers = 1", "min_peers = 9");

        // The expected message is asserted too — without it the test would
        // still pass if both entry points failed in the *parser* instead,
        // which is not the equivalence being claimed.
        let cases = [
            (&no_listener, "At least one listener"),
            (&no_seeds, "At least one seed peer"),
            (&inverted_peers, "min_peers must be <= max_peers"),
        ];

        for (invalid, expected) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ergo.toml");
            std::fs::write(&path, invalid).unwrap();

            let file_err = Config::load(path.to_str().unwrap())
                .expect_err("load must reject this document")
                .to_string();
            let memory_err = Config::from_toml_str(invalid)
                .expect_err("from_toml_str must reject this document")
                .to_string();

            assert!(
                memory_err.contains(expected),
                "expected validation error containing {expected:?}, got {memory_err:?}"
            );
            assert_eq!(file_err, memory_err);
        }
    }

    #[test]
    fn from_toml_str_rejects_malformed_toml() {
        // Unterminated table header — never a valid document.
        assert!(Config::from_toml_str("[proxy\nnetwork = \"mainnet\"").is_err());
        // Well-formed TOML, but not a Config.
        assert!(Config::from_toml_str("hello = \"world\"").is_err());
        // Empty input.
        assert!(Config::from_toml_str("").is_err());
    }
}

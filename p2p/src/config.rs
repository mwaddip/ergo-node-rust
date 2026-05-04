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

fn default_discover_timeout() -> u64 { 5 }

impl Default for UpnpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            discover_timeout_secs: default_discover_timeout(),
        }
    }
}

fn default_get_peers_interval() -> u64 { 120 }
fn default_delivery_timeout() -> u64 { 10 }
fn default_max_delivery_checks() -> u32 { 100 }
fn default_desired_inv_objects() -> u32 { 400 }
fn default_max_peer_spec_objects() -> u32 { 64 }
fn default_handshake_timeout() -> u64 { 30 }
fn default_inactive_connection_deadline() -> u64 { 600 }
fn default_temporal_ban_duration() -> u64 { 60 }

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
    /// Load config from a TOML file.
    ///
    /// # Contract
    /// - **Precondition**: `path` points to a readable TOML file.
    /// - **Postcondition**: Returns a valid `Config` with at least one listener
    ///   and at least one seed peer, or an error.
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;

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
        let toml_str = format!("{}\n[upnp]\nenabled = true\ndiscover_timeout_secs = 10\n", MINIMAL_TOML);
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
}

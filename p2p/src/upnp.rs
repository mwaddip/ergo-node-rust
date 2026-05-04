//! UPnP IGD port mapping and external address discovery.
//!
//! IPv4 only — IPv6 addresses are globally routable and don't need NAT traversal.
//! Every operation is fallible and non-fatal: if UPnP doesn't work, the node
//! continues without a declared address.

use igd_next::aio::tokio as igd_tokio;
use igd_next::PortMappingProtocol;
use igd_next::SearchOptions;
use std::net::SocketAddr;
use tokio::time::Duration;

use crate::config::UpnpConfig;

/// The async gateway type parameterized for tokio.
type AsyncGateway = igd_next::aio::Gateway<igd_tokio::Tokio>;

/// A successful UPnP port mapping. Holds the gateway handle for cleanup.
pub struct UpnpMapping {
    gateway: AsyncGateway,
    pub external_addr: SocketAddr,
    mapped_port: u16,
}

impl UpnpMapping {
    /// Remove the port mapping from the gateway. Best-effort — logs on failure.
    pub async fn remove(&self) {
        if let Err(e) = self.gateway.remove_port(PortMappingProtocol::TCP, self.mapped_port).await {
            tracing::warn!(port = self.mapped_port, error = %e, "UPnP: port mapping removal failed");
        } else {
            tracing::info!(port = self.mapped_port, "UPnP: port mapping removed");
        }
    }
}

/// Attempt UPnP gateway discovery, port mapping, and external IP query.
///
/// Returns `Some(UpnpMapping)` on success, `None` on any failure.
/// Never panics or blocks indefinitely — all steps are timeout-wrapped.
pub async fn attempt(config: &UpnpConfig, listen_port: u16, local_addr: SocketAddr) -> Option<UpnpMapping> {
    // Skip if the listen address is IPv6 — UPnP IGD is IPv4 NAT traversal only
    if local_addr.ip().is_ipv6() {
        tracing::debug!("UPnP: skipping for IPv6 listener");
        return None;
    }

    let timeout = Duration::from_secs(config.discover_timeout_secs);

    // Step 1: discover gateway
    let search_opts = SearchOptions {
        timeout: Some(timeout),
        ..Default::default()
    };

    let gateway = match tokio::time::timeout(timeout, igd_tokio::search_gateway(search_opts)).await {
        Ok(Ok(gw)) => {
            tracing::info!("UPnP: gateway found at {}", gw.addr);
            gw
        }
        Ok(Err(e)) => {
            tracing::info!(error = %e, "UPnP: no gateway found");
            return None;
        }
        Err(_) => {
            tracing::info!("UPnP: gateway discovery timed out");
            return None;
        }
    };

    // Step 2: add port mapping
    if let Err(e) = gateway.add_port(
        PortMappingProtocol::TCP,
        listen_port,
        local_addr,
        0, // permanent until removed
        "ergo-node-rust",
    ).await {
        tracing::info!(port = listen_port, error = %e, "UPnP: port mapping failed");
        return None;
    }
    tracing::info!(port = listen_port, "UPnP: port mapping added");

    // Step 3: query external IP
    let external_ip = match gateway.get_external_ip().await {
        Ok(ip) => ip,
        Err(e) => {
            tracing::info!(error = %e, "UPnP: external IP query failed");
            // Clean up the mapping we just created since we can't use it
            let _ = gateway.remove_port(PortMappingProtocol::TCP, listen_port).await;
            return None;
        }
    };

    let external_addr = SocketAddr::new(external_ip, listen_port);
    tracing::info!(addr = %external_addr, "UPnP: external address discovered");

    Some(UpnpMapping {
        gateway,
        external_addr,
        mapped_port: listen_port,
    })
}

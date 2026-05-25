//! Classifier for addresses that have no business being gossiped as
//! a public peer.
//!
//! # Contract
//! See `facts/p2p-routing.md::Bogus address classification`.
//!
//! Two sub-predicates:
//!
//! * `is_always_bogus` — classes no Ergo peer can legitimately advertise
//!   regardless of network. Loopback, link-local, multicast,
//!   unspecified, broadcast, benchmark (RFC 2544), reserved Class E,
//!   IPv4-mapped IPv6.
//! * `is_mainnet_only_bogus` — classes that may legitimately appear on
//!   a testnet running inside a private network, but never on mainnet.
//!   RFC 1918 private IPv4, CGN (RFC 6598), IPv6 ULA, documentation
//!   ranges (RFC 5737 + RFC 3849).
//!
//! The classifier uses stable `std::net` predicates where they exist
//! (`is_loopback`, `is_link_local`, `is_multicast`, `is_broadcast`,
//! `is_unspecified`, `is_private`) and hand-rolls the rest (CGN,
//! documentation, benchmark, reserved 240/4, IPv6 link-local / ULA /
//! v4-mapped / documentation) with bit-mask checks. The unstable
//! `is_global` family is deliberately not used so this builds on stable
//! Rust.
//!
//! Used by the router's `Peers` ingest arm (to punish gossipers of
//! unroutable junk) and by the `GetPeers` response builder + outbound
//! fill-phase candidate selection (defensive egress filters, so a
//! legacy or buggy entry in the PeerDb never escapes).

use crate::types::Network;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Returns `true` if `addr` is not a routable public peer for the given
/// `network`. Combines the unconditional and mainnet-only sub-predicates.
pub fn is_bogus_address(addr: SocketAddr, network: Network) -> bool {
    is_always_bogus(addr) || (network == Network::Mainnet && is_mainnet_only_bogus(addr))
}

/// Always-bogus: addresses no Ergo peer can legitimately advertise
/// regardless of network. Loopback, link-local, multicast,
/// unspecified, broadcast, benchmark (RFC 2544), reserved Class E,
/// IPv4-mapped IPv6.
fn is_always_bogus(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => is_always_bogus_v4(v4),
        IpAddr::V6(v6) => is_always_bogus_v6(v6),
    }
}

/// Mainnet-only bogus: addresses that could be legitimate on a
/// testnet running inside a LAN, but never on mainnet. RFC 1918
/// (private IPv4), CGN 100.64/10, IPv6 ULA fc00::/7, documentation
/// ranges (192.0.2/24, 198.51.100/24, 203.0.113/24, 2001:db8::/32).
fn is_mainnet_only_bogus(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => is_mainnet_only_bogus_v4(v4),
        IpAddr::V6(v6) => is_mainnet_only_bogus_v6(v6),
    }
}

fn is_always_bogus_v4(ip: Ipv4Addr) -> bool {
    if ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
    {
        return true;
    }
    let o = ip.octets();
    // Benchmark: 198.18.0.0/15 — RFC 2544
    if o[0] == 198 && (o[1] & 0xfe) == 18 {
        return true;
    }
    // Reserved Class E: 240.0.0.0/4 — RFC 1112
    if o[0] >= 240 {
        return true;
    }
    false
}

fn is_mainnet_only_bogus_v4(ip: Ipv4Addr) -> bool {
    if ip.is_private() {
        return true;
    }
    let o = ip.octets();
    // CGN: 100.64.0.0/10 — RFC 6598
    if o[0] == 100 && (o[1] & 0xc0) == 0x40 {
        return true;
    }
    // Documentation ranges — RFC 5737
    if (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
    {
        return true;
    }
    false
}

fn is_always_bogus_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    // IPv4-mapped (::ffff:0:0/96) — bogus as a class. Legitimate peers
    // use native v4 or native v6; a mapped form on the wire is wrong
    // regardless of the underlying v4.
    if ip.to_ipv4_mapped().is_some() {
        return true;
    }
    let s = ip.segments();
    // Link-local: fe80::/10
    if (s[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    false
}

fn is_mainnet_only_bogus_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Unique-local (ULA): fc00::/7
    if (s[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // Documentation: 2001:db8::/32 — RFC 3849
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    // ---- IPv4 always-bogus classes (both networks) ----

    #[test]
    fn v4_loopback_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("127.0.0.1:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("127.0.0.1:9030"), Network::Testnet));
        assert!(is_bogus_address(
            sa("127.255.255.254:9030"),
            Network::Mainnet
        ));
        assert!(is_bogus_address(
            sa("127.255.255.254:9030"),
            Network::Testnet
        ));
    }

    #[test]
    fn v4_link_local_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("169.254.0.2:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("169.254.0.2:9030"), Network::Testnet));
        assert!(is_bogus_address(
            sa("169.254.255.254:9030"),
            Network::Mainnet
        ));
        assert!(is_bogus_address(
            sa("169.254.255.254:9030"),
            Network::Testnet
        ));
    }

    #[test]
    fn v4_multicast_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("224.0.0.1:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("224.0.0.1:9030"), Network::Testnet));
        assert!(is_bogus_address(
            sa("239.255.255.255:9030"),
            Network::Mainnet
        ));
        assert!(is_bogus_address(
            sa("239.255.255.255:9030"),
            Network::Testnet
        ));
    }

    #[test]
    fn v4_broadcast_is_bogus_both_networks() {
        assert!(is_bogus_address(
            sa("255.255.255.255:9030"),
            Network::Mainnet
        ));
        assert!(is_bogus_address(
            sa("255.255.255.255:9030"),
            Network::Testnet
        ));
    }

    #[test]
    fn v4_unspecified_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("0.0.0.0:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("0.0.0.0:9030"), Network::Testnet));
    }

    #[test]
    fn v4_benchmark_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("198.18.0.1:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("198.18.0.1:9030"), Network::Testnet));
        assert!(is_bogus_address(
            sa("198.19.255.254:9030"),
            Network::Mainnet
        ));
        assert!(is_bogus_address(
            sa("198.19.255.254:9030"),
            Network::Testnet
        ));
        // Just outside: 198.20/* is not benchmark.
        assert!(!is_bogus_address(sa("198.20.0.1:9030"), Network::Mainnet));
        assert!(!is_bogus_address(sa("198.20.0.1:9030"), Network::Testnet));
    }

    #[test]
    fn v4_class_e_reserved_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("240.0.0.1:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("240.0.0.1:9030"), Network::Testnet));
        assert!(is_bogus_address(sa("250.1.2.3:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("250.1.2.3:9030"), Network::Testnet));
    }

    // ---- IPv4 mainnet-only-bogus classes ----

    #[test]
    fn v4_private_rfc1918_is_bogus_mainnet_only() {
        for s in [
            "10.0.0.1:9030",
            "172.16.0.1:9030",
            "172.31.255.254:9030",
            "192.168.1.1:9030",
        ] {
            assert!(
                is_bogus_address(sa(s), Network::Mainnet),
                "{s} should be bogus on mainnet"
            );
            assert!(
                !is_bogus_address(sa(s), Network::Testnet),
                "{s} should not be bogus on testnet"
            );
        }
    }

    #[test]
    fn v4_cgn_is_bogus_mainnet_only() {
        assert!(is_bogus_address(sa("100.64.0.1:9030"), Network::Mainnet));
        assert!(!is_bogus_address(sa("100.64.0.1:9030"), Network::Testnet));
        assert!(is_bogus_address(
            sa("100.127.255.254:9030"),
            Network::Mainnet
        ));
        assert!(!is_bogus_address(
            sa("100.127.255.254:9030"),
            Network::Testnet
        ));
        // Just outside CGN: routable on both.
        assert!(!is_bogus_address(
            sa("100.63.255.255:9030"),
            Network::Mainnet
        ));
        assert!(!is_bogus_address(sa("100.128.0.0:9030"), Network::Mainnet));
    }

    #[test]
    fn v4_documentation_is_bogus_mainnet_only() {
        for s in ["192.0.2.1:9030", "198.51.100.1:9030", "203.0.113.1:9030"] {
            assert!(
                is_bogus_address(sa(s), Network::Mainnet),
                "{s} should be bogus on mainnet"
            );
            assert!(
                !is_bogus_address(sa(s), Network::Testnet),
                "{s} should not be bogus on testnet"
            );
        }
    }

    #[test]
    fn v4_public_is_not_bogus_either_network() {
        // Hetzner-ish + Google DNS — routable public addresses.
        for s in ["78.46.1.2:9030", "8.8.8.8:9030"] {
            assert!(
                !is_bogus_address(sa(s), Network::Mainnet),
                "{s} should be fine on mainnet"
            );
            assert!(
                !is_bogus_address(sa(s), Network::Testnet),
                "{s} should be fine on testnet"
            );
        }
    }

    // ---- IPv6 always-bogus classes (both networks) ----

    #[test]
    fn v6_loopback_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("[::1]:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("[::1]:9030"), Network::Testnet));
    }

    #[test]
    fn v6_unspecified_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("[::]:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("[::]:9030"), Network::Testnet));
    }

    #[test]
    fn v6_multicast_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("[ff00::1]:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("[ff00::1]:9030"), Network::Testnet));
        assert!(is_bogus_address(sa("[ff02::1]:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("[ff02::1]:9030"), Network::Testnet));
    }

    #[test]
    fn v6_link_local_is_bogus_both_networks() {
        assert!(is_bogus_address(sa("[fe80::1]:9030"), Network::Mainnet));
        assert!(is_bogus_address(sa("[fe80::1]:9030"), Network::Testnet));
        assert!(is_bogus_address(
            sa("[febf:ffff::1]:9030"),
            Network::Mainnet
        ));
        assert!(is_bogus_address(
            sa("[febf:ffff::1]:9030"),
            Network::Testnet
        ));
        // fec0::/10 — site-local, deprecated; falls outside fe80::/10.
        assert!(!is_bogus_address(sa("[fec0::1]:9030"), Network::Mainnet));
        assert!(!is_bogus_address(sa("[fec0::1]:9030"), Network::Testnet));
    }

    #[test]
    fn v6_v4_mapped_is_bogus_both_networks() {
        // The whole ::ffff:0:0/96 range is bogus as a class, regardless
        // of the underlying v4.
        for s in [
            "[::ffff:10.0.0.1]:9030",
            "[::ffff:127.0.0.1]:9030",
            "[::ffff:8.8.8.8]:9030",
        ] {
            assert!(
                is_bogus_address(sa(s), Network::Mainnet),
                "{s} should be bogus on mainnet"
            );
            assert!(
                is_bogus_address(sa(s), Network::Testnet),
                "{s} should be bogus on testnet"
            );
        }
    }

    // ---- IPv6 mainnet-only-bogus classes ----

    #[test]
    fn v6_ula_is_bogus_mainnet_only() {
        for s in [
            "[fc00::1]:9030",
            "[fd12:3456:789a::1]:9030",
            "[fdff:ffff:ffff:ffff::1]:9030",
        ] {
            assert!(
                is_bogus_address(sa(s), Network::Mainnet),
                "{s} should be bogus on mainnet"
            );
            assert!(
                !is_bogus_address(sa(s), Network::Testnet),
                "{s} should not be bogus on testnet"
            );
        }
    }

    #[test]
    fn v6_documentation_is_bogus_mainnet_only() {
        for s in ["[2001:db8::1]:9030", "[2001:db8:dead:beef::1]:9030"] {
            assert!(
                is_bogus_address(sa(s), Network::Mainnet),
                "{s} should be bogus on mainnet"
            );
            assert!(
                !is_bogus_address(sa(s), Network::Testnet),
                "{s} should not be bogus on testnet"
            );
        }
        // Just outside: 2001:db9 is not documentation.
        assert!(!is_bogus_address(
            sa("[2001:db9::1]:9030"),
            Network::Mainnet
        ));
        assert!(!is_bogus_address(
            sa("[2001:db9::1]:9030"),
            Network::Testnet
        ));
    }

    #[test]
    fn v6_public_is_not_bogus_either_network() {
        // Hurricane Electric 2001:470::/32 and Cloudflare 2606:4700::/32 —
        // real routable v6 ranges.
        for s in ["[2001:470:1f0b:1234::1]:9030", "[2606:4700::1111]:9030"] {
            assert!(
                !is_bogus_address(sa(s), Network::Mainnet),
                "{s} should be fine on mainnet"
            );
            assert!(
                !is_bogus_address(sa(s), Network::Testnet),
                "{s} should be fine on testnet"
            );
        }
    }
}

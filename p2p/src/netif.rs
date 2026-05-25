//! Network interface address discovery.
//!
//! Enumerates system interfaces to find addresses suitable for advertising
//! as `declared_address` in the handshake. Currently: IPv6 global unicast only.
//! IPv4 NAT traversal is handled by the UPnP module.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

// Linux netlink interface address flags (see <linux/if_addr.h>).
// /proc/net/if_inet6 currently exposes only the low 8 bits of `inet6_ifaddr.flags`,
// so IFA_F_MANAGETEMPADDR (0x100) won't appear there on most kernels — the
// preference is documented and forward-compatible if a kernel ever widens it.
const IFA_F_TEMPORARY: u32 = 0x001;
const IFA_F_DEPRECATED: u32 = 0x020;
const IFA_F_TENTATIVE: u32 = 0x040;
const IFA_F_MANAGETEMPADDR: u32 = 0x100;

const IFA_F_UNSTABLE: u32 = IFA_F_TEMPORARY | IFA_F_DEPRECATED | IFA_F_TENTATIVE;

/// Find a stable global unicast IPv6 address on any system interface,
/// suitable for advertising in the handshake's `declared_address`.
///
/// Skips link-local (fe80::), loopback (::1), and other non-global scopes.
/// On Linux, reads /proc/net/if_inet6 to filter out RFC 4941 temporary
/// addresses (which rotate every ~24h, so peers caching them for reconnect
/// would fail) and prefers SLAAC-managed parents (`mngtmpaddr` in
/// `ip -6 addr` output). On non-Linux or if /proc is unreadable, falls back
/// to the first global unicast address found. If only unstable candidates
/// exist, returns one anyway — better to announce a temporary than nothing.
pub fn find_global_ipv6(port: u16) -> Option<SocketAddr> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(addrs) => addrs,
        Err(e) => {
            tracing::info!(error = %e, "IPv6 auto-detect: failed to enumerate interfaces");
            return None;
        }
    };

    let candidates: Vec<(Ipv6Addr, String)> = ifaces
        .iter()
        .filter_map(|iface| match iface.ip() {
            IpAddr::V6(v6) if is_global_unicast(v6) => Some((v6, iface.name.clone())),
            _ => None,
        })
        .collect();

    if candidates.is_empty() {
        tracing::info!("IPv6 auto-detect: no global unicast address found");
        return None;
    }

    let flags = linux_addr_flags();
    let (addr, iface) = select_address(&candidates, flags.as_ref());

    tracing::info!(addr = %addr, iface = %iface, "IPv6 auto-detect: found global unicast address");
    Some(SocketAddr::new(IpAddr::V6(addr), port))
}

/// Pick the best candidate, preferring stable (mngtmpaddr) addresses over
/// RFC 4941 temporary ones when Linux flag info is available.
///
/// Selection order:
/// 1. Stable + IFA_F_MANAGETEMPADDR — the SLAAC parent, doesn't rotate.
/// 2. Stable (no temporary/deprecated/tentative bits) — likely also non-rotating.
/// 3. First candidate, even if temporary — better than announcing nothing.
fn select_address(
    candidates: &[(Ipv6Addr, String)],
    flags: Option<&HashMap<Ipv6Addr, u32>>,
) -> (Ipv6Addr, String) {
    if let Some(map) = flags {
        // Address absent from /proc map (race window between getifaddrs and proc read,
        // or non-IP interface) is treated as flags=0, i.e. assumed stable.
        let stable: Vec<&(Ipv6Addr, String)> = candidates
            .iter()
            .filter(|(addr, _)| map.get(addr).copied().unwrap_or(0) & IFA_F_UNSTABLE == 0)
            .collect();

        if let Some(c) = stable
            .iter()
            .find(|(addr, _)| map.get(addr).copied().unwrap_or(0) & IFA_F_MANAGETEMPADDR != 0)
        {
            return (**c).clone();
        }

        if let Some(c) = stable.first() {
            return (**c).clone();
        }
    }

    candidates[0].clone()
}

#[cfg(target_os = "linux")]
fn linux_addr_flags() -> Option<HashMap<Ipv6Addr, u32>> {
    let content = std::fs::read_to_string("/proc/net/if_inet6").ok()?;
    Some(parse_if_inet6(&content))
}

#[cfg(not(target_os = "linux"))]
fn linux_addr_flags() -> Option<HashMap<Ipv6Addr, u32>> {
    None
}

/// Parse /proc/net/if_inet6 lines into an address-to-flags map.
///
/// Per `man 5 proc`: each line is
/// `<addr_32hex> <ifindex_hex> <prefix_hex> <scope_hex> <flags_hex> <device>`.
/// Only global-scope (scope == 0) entries are kept; malformed lines skip silently.
fn parse_if_inet6(content: &str) -> HashMap<Ipv6Addr, u32> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 {
            continue;
        }
        let scope = match u32::from_str_radix(cols[3], 16) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if scope != 0 {
            continue;
        }
        let flags = match u32::from_str_radix(cols[4], 16) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if let Some(addr) = parse_proc_ipv6(cols[0]) {
            map.insert(addr, flags);
        }
    }
    map
}

/// Convert /proc/net/if_inet6's 32-hex-char address to Ipv6Addr.
fn parse_proc_ipv6(hex: &str) -> Option<Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(Ipv6Addr::from(bytes))
}

/// Check if an IPv6 address is global unicast (not link-local, loopback,
/// multicast, or other special-purpose).
fn is_global_unicast(addr: Ipv6Addr) -> bool {
    // Reject loopback (::1)
    if addr.is_loopback() {
        return false;
    }
    // Reject unspecified (::)
    if addr.is_unspecified() {
        return false;
    }
    // Reject multicast (ff00::/8)
    if addr.is_multicast() {
        return false;
    }
    // Reject link-local (fe80::/10)
    let segments = addr.segments();
    if segments[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // Reject unique-local (fc00::/7) — not globally routable
    if segments[0] & 0xfe00 == 0xfc00 {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_unicast_accepted() {
        // 2001:db8::1 is documentation range, but structurally global unicast
        let addr: Ipv6Addr = "2001:db8::1".parse().unwrap();
        assert!(is_global_unicast(addr));
    }

    #[test]
    fn link_local_rejected() {
        let addr: Ipv6Addr = "fe80::1".parse().unwrap();
        assert!(!is_global_unicast(addr));
    }

    #[test]
    fn loopback_rejected() {
        let addr: Ipv6Addr = "::1".parse().unwrap();
        assert!(!is_global_unicast(addr));
    }

    #[test]
    fn unique_local_rejected() {
        let addr: Ipv6Addr = "fd00::1".parse().unwrap();
        assert!(!is_global_unicast(addr));
    }

    #[test]
    fn multicast_rejected() {
        let addr: Ipv6Addr = "ff02::1".parse().unwrap();
        assert!(!is_global_unicast(addr));
    }

    #[test]
    fn unspecified_rejected() {
        let addr: Ipv6Addr = "::".parse().unwrap();
        assert!(!is_global_unicast(addr));
    }

    #[test]
    fn parse_proc_ipv6_valid() {
        let addr = parse_proc_ipv6("2a02a46d09a600003b376ed8db919e84").unwrap();
        let expected: Ipv6Addr = "2a02:a46d:9a6:0:3b37:6ed8:db91:9e84".parse().unwrap();
        assert_eq!(addr, expected);
    }

    #[test]
    fn parse_proc_ipv6_loopback() {
        let addr = parse_proc_ipv6("00000000000000000000000000000001").unwrap();
        assert_eq!(addr, Ipv6Addr::LOCALHOST);
    }

    #[test]
    fn parse_proc_ipv6_wrong_length() {
        assert!(parse_proc_ipv6("").is_none());
        assert!(parse_proc_ipv6("abc").is_none());
        assert!(parse_proc_ipv6(&"a".repeat(31)).is_none());
        assert!(parse_proc_ipv6(&"a".repeat(33)).is_none());
    }

    #[test]
    fn parse_proc_ipv6_invalid_hex() {
        assert!(parse_proc_ipv6(&"z".repeat(32)).is_none());
    }

    #[test]
    fn parse_if_inet6_real_sample() {
        // Captured from a Linux laptop with both temporary and stable addresses
        // in the same /64 prefix — the case we're trying to disambiguate.
        let sample = "\
2a02a46d09a60000c406a62c632f1472 02 40 00 01     wlo1
fd5a544545470000ef3363646b944d24 02 40 00 01     wlo1
00000000000000000000000000000001 01 80 10 80       lo
fd5a5445454700009892851527b16fe8 02 40 00 00     wlo1
2a02a46d09a600003b376ed8db919e84 02 40 00 00     wlo1
fe8000000000000055b794ab6ef52d06 02 40 20 80     wlo1
";
        let map = parse_if_inet6(sample);

        // Loopback (scope 0x10) and link-local (scope 0x20) excluded.
        assert_eq!(map.len(), 4);

        let temp_global: Ipv6Addr = "2a02:a46d:9a6:0:c406:a62c:632f:1472".parse().unwrap();
        assert_eq!(map.get(&temp_global), Some(&IFA_F_TEMPORARY));

        let stable_global: Ipv6Addr = "2a02:a46d:9a6:0:3b37:6ed8:db91:9e84".parse().unwrap();
        assert_eq!(map.get(&stable_global), Some(&0));
    }

    #[test]
    fn parse_if_inet6_skips_malformed_lines() {
        let sample = "\
short
2a02a46d09a600003b376ed8db919e84 02 40 00 00     wlo1
not enough cols here
not32hex 02 40 00 00 wlo1
";
        let map = parse_if_inet6(sample);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn parse_if_inet6_empty() {
        assert!(parse_if_inet6("").is_empty());
    }

    #[test]
    fn select_address_prefers_stable_over_temporary() {
        let temp: Ipv6Addr = "2a02:a46d:9a6:0:c406:a62c:632f:1472".parse().unwrap();
        let stable: Ipv6Addr = "2a02:a46d:9a6:0:3b37:6ed8:db91:9e84".parse().unwrap();
        // Temporary first — it would win the unfiltered "first global unicast" race.
        let candidates = vec![(temp, "wlo1".to_string()), (stable, "wlo1".to_string())];
        let mut flags = HashMap::new();
        flags.insert(temp, IFA_F_TEMPORARY);
        flags.insert(stable, 0);

        let (chosen, _) = select_address(&candidates, Some(&flags));
        assert_eq!(chosen, stable);
    }

    #[test]
    fn select_address_prefers_managetempaddr_among_stable() {
        let plain: Ipv6Addr = "2a02:a46d:9a6:0:1::1".parse().unwrap();
        let mngtmp: Ipv6Addr = "2a02:a46d:9a6:0:2::2".parse().unwrap();
        // plain first — would win the simple "first stable" tiebreak.
        let candidates = vec![(plain, "wlo1".to_string()), (mngtmp, "wlo1".to_string())];
        let mut flags = HashMap::new();
        flags.insert(plain, 0);
        flags.insert(mngtmp, IFA_F_MANAGETEMPADDR);

        let (chosen, _) = select_address(&candidates, Some(&flags));
        assert_eq!(chosen, mngtmp);
    }

    #[test]
    fn select_address_falls_back_when_all_temporary() {
        let temp1: Ipv6Addr = "2a02:a46d:9a6:0:c406:a62c:632f:1472".parse().unwrap();
        let temp2: Ipv6Addr = "2a02:a46d:9a6:0:c407::1".parse().unwrap();
        let candidates = vec![(temp1, "wlo1".to_string()), (temp2, "wlo1".to_string())];
        let mut flags = HashMap::new();
        flags.insert(temp1, IFA_F_TEMPORARY);
        flags.insert(temp2, IFA_F_TEMPORARY);

        let (chosen, _) = select_address(&candidates, Some(&flags));
        assert_eq!(
            chosen, temp1,
            "should fall back to first candidate when none stable"
        );
    }

    #[test]
    fn select_address_rejects_deprecated_and_tentative() {
        let deprecated: Ipv6Addr = "2a02:a46d:9a6:0:1::1".parse().unwrap();
        let tentative: Ipv6Addr = "2a02:a46d:9a6:0:2::2".parse().unwrap();
        let stable: Ipv6Addr = "2a02:a46d:9a6:0:3::3".parse().unwrap();
        let candidates = vec![
            (deprecated, "wlo1".to_string()),
            (tentative, "wlo1".to_string()),
            (stable, "wlo1".to_string()),
        ];
        let mut flags = HashMap::new();
        flags.insert(deprecated, IFA_F_DEPRECATED);
        flags.insert(tentative, IFA_F_TENTATIVE);
        flags.insert(stable, 0);

        let (chosen, _) = select_address(&candidates, Some(&flags));
        assert_eq!(chosen, stable);
    }

    #[test]
    fn select_address_no_flag_info_returns_first() {
        let a: Ipv6Addr = "2a02:a46d:9a6:0:1::1".parse().unwrap();
        let b: Ipv6Addr = "2a02:a46d:9a6:0:2::2".parse().unwrap();
        let candidates = vec![(a, "wlo1".to_string()), (b, "wlo1".to_string())];

        let (chosen, _) = select_address(&candidates, None);
        assert_eq!(chosen, a);
    }

    #[test]
    fn select_address_treats_unknown_as_stable() {
        // Address not in /proc map (race window or non-IP interface) is treated
        // as flags=0 → considered stable. We don't want to demote it just because
        // we couldn't see its flags.
        let known_temp: Ipv6Addr = "2a02:a46d:9a6:0:c406:a62c:632f:1472".parse().unwrap();
        let unknown: Ipv6Addr = "2a02:a46d:9a6:0:9::9".parse().unwrap();
        let candidates = vec![
            (known_temp, "wlo1".to_string()),
            (unknown, "wlo1".to_string()),
        ];
        let mut flags = HashMap::new();
        flags.insert(known_temp, IFA_F_TEMPORARY);

        let (chosen, _) = select_address(&candidates, Some(&flags));
        assert_eq!(chosen, unknown);
    }
}

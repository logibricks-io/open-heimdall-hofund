//! Outbound proxy helpers for Bun-based CLIs (Claude Code).
//!
//! Bun accepts only `http://` and `https://` proxies. SOCKS5 addresses are
//! bridged to loopback HTTP proxies, matching the reference `EnvForBunWith`
//! behavior.

mod bridge;
mod env;

pub use env::{ProxyError, env_for_bun_with, env_with, for_bun, is_socks, parse_proxy};

/// Local interface addresses on Linux, macOS, and Windows.
///
/// `if-addrs` calls `getifaddrs` on Linux and macOS and `GetAdaptersAddresses`
/// on Windows. An unreadable list is empty, so a network change then waits
/// for the next timed lookup. The strings are IP addresses only.
#[must_use]
pub fn interface_addrs() -> Vec<String> {
    let Ok(list) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out: Vec<String> = list
        .into_iter()
        .map(|iface| iface.ip().to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::interface_addrs;

    #[test]
    fn d245_interface_addrs_are_sorted_unique_ips() {
        let addrs = interface_addrs();
        let mut sorted = addrs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(addrs, sorted);
        for addr in &addrs {
            assert!(
                addr.parse::<std::net::IpAddr>().is_ok(),
                "each observed address is an IP"
            );
        }
    }
}

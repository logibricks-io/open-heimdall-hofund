//! Canonical IP addresses and CIDR membership.
//!
//! A prefix is accepted only when its host bits are clear and its length fits
//! the address. An IPv4-mapped IPv6 address is stored as IPv4.

use std::net::IpAddr;

/// Returns the canonical text of one IP address.
///
/// IPv4-mapped IPv6 text becomes the IPv4 address. Anything that is not one
/// address is [`None`].
#[must_use]
pub fn canonical_ip(text: &str) -> Option<String> {
    let addr: IpAddr = text.trim().parse().ok()?;
    Some(addr.to_canonical().to_string())
}

/// Whether `address` is inside `prefix`.
///
/// A prefix with host bits set, an over-long length, or a different address
/// family does not contain the address.
#[must_use]
pub fn prefix_contains(prefix: &str, address: &str) -> bool {
    let Some((network, bits)) = parse_prefix(prefix) else {
        return false;
    };
    let Some(address) = canonical_ip(address) else {
        return false;
    };
    let Ok(address) = address.parse::<IpAddr>() else {
        return false;
    };
    if network.is_ipv4() != address.is_ipv4() {
        return false;
    }
    mask(address, bits) == network
}

/// Whether `text` is one IP address or one prefix with clear host bits.
#[must_use]
pub fn is_ip_or_prefix(text: &str) -> bool {
    canonical_ip(text).is_some() || parse_prefix(text).is_some()
}

fn parse_prefix(text: &str) -> Option<(IpAddr, u32)> {
    let (address, bits) = text.split_once('/')?;
    if bits.contains('/') {
        return None;
    }
    let address: IpAddr = address.parse().ok()?;
    let bits: u32 = bits.parse().ok()?;
    let max = if address.is_ipv4() { 32 } else { 128 };
    if bits > max || !host_bits_clear(address, bits) {
        return None;
    }
    Some((mask(address, bits), bits))
}

fn host_bits_clear(address: IpAddr, bits: u32) -> bool {
    mask(address, bits) == address
}

fn mask(address: IpAddr, bits: u32) -> IpAddr {
    match address {
        IpAddr::V4(address) => {
            let ip = u32::from(address);
            let masked = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            IpAddr::V4((ip & masked).into())
        }
        IpAddr::V6(address) => {
            let ip = u128::from(address);
            let masked = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            IpAddr::V6((ip & masked).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_ip, is_ip_or_prefix, prefix_contains};

    #[test]
    fn d279_mapped_address_is_ipv4_and_host_bits_are_refused() {
        assert_eq!(
            canonical_ip("::ffff:203.0.113.9").as_deref(),
            Some("203.0.113.9")
        );
        assert!(prefix_contains("203.0.113.0/24", "203.0.113.200"));
        assert!(prefix_contains("2001:db8::/32", "2001:db8::1"));
        assert!(!prefix_contains("203.0.113.1/24", "203.0.113.9"));
        assert!(!prefix_contains("203.0.113.0/33", "203.0.113.9"));
        assert!(is_ip_or_prefix("203.0.113.9"));
        assert!(is_ip_or_prefix("2001:db8::/32"));
        assert!(!is_ip_or_prefix("203.0.113.1/24"));
        assert!(!is_ip_or_prefix("USA"));
    }
}

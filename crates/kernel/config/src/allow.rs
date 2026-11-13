use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::{Config, ConfigError};

/// Environment variable that replaces the configured allow list when set.
pub const ALLOW_IPS_ENV: &str = "HOFUND_ALLOW_IPS";
/// Environment variable that overrides `trust_proxy` when set.
pub const TRUST_PROXY_ENV: &str = "HOFUND_TRUST_PROXY";

/// The current `HOFUND_ALLOW_IPS` value, converted lossily when not UTF-8.
#[must_use]
pub fn allow_ips_env() -> Option<String> {
    std::env::var_os(ALLOW_IPS_ENV).map(|value| value.to_string_lossy().into_owned())
}

/// The current `HOFUND_TRUST_PROXY` value, converted lossily when not UTF-8.
#[must_use]
pub fn trust_proxy_env() -> Option<String> {
    std::env::var_os(TRUST_PROXY_ENV).map(|value| value.to_string_lossy().into_owned())
}

/// One validated IP address or CIDR network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IpNetwork {
    network: IpAddr,
    prefix: u8,
}

impl IpNetwork {
    /// Parses an IP address or CIDR network and masks any host bits.
    ///
    /// # Errors
    ///
    /// Returns a message naming `raw` when it is not an address or network.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let invalid = || {
            format!(
                "{raw:?} is not an IP address or CIDR range; use 203.0.113.7, 10.0.0.0/8, or 2001:db8::/32"
            )
        };
        if raw.is_empty() {
            return Err(invalid());
        }
        let (address, prefix) = match raw.split_once('/') {
            Some((address, prefix)) if !prefix.contains('/') => {
                let address = address.parse::<IpAddr>().map_err(|_| invalid())?;
                let prefix = prefix.parse::<u8>().map_err(|_| invalid())?;
                let bits = if address.is_ipv4() { 32 } else { 128 };
                if prefix > bits {
                    return Err(invalid());
                }
                (address, prefix)
            }
            Some(_) => return Err(invalid()),
            None => {
                let unbracketed = raw
                    .strip_prefix('[')
                    .and_then(|address| address.strip_suffix(']'))
                    .unwrap_or(raw);
                let address = unbracketed.parse::<IpAddr>().map_err(|_| invalid())?;
                let prefix = if address.is_ipv4() { 32 } else { 128 };
                (address, prefix)
            }
        };
        let (address, prefix) = match address {
            IpAddr::V6(address) if prefix >= 96 => address
                .to_ipv4_mapped()
                .map(|address| (IpAddr::V4(address), prefix - 96))
                .unwrap_or((IpAddr::V6(address), prefix)),
            address => (address, prefix),
        };
        Ok(Self {
            network: mask(address, prefix),
            prefix,
        })
    }

    /// Whether this network contains `address`. IPv4-mapped IPv6 addresses
    /// are compared as IPv4.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        let address = unmap(address);
        match (self.network, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                mask(IpAddr::V4(address), self.prefix) == IpAddr::V4(network)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                mask(IpAddr::V6(address), self.prefix) == IpAddr::V6(network)
            }
            _ => false,
        }
    }
}

impl fmt::Display for IpNetwork {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bits = if self.network.is_ipv4() { 32 } else { 128 };
        if self.prefix == bits {
            self.network.fmt(formatter)
        } else {
            write!(formatter, "{}/{}", self.network, self.prefix)
        }
    }
}

/// Effective listener access settings after environment precedence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkSettings {
    /// Effective entries as written, after config canonicalization. An invalid
    /// environment entry remains visible here for diagnostics.
    pub allow_ips: Vec<String>,
    /// Whether forwarding headers on loopback retain local-only privileges.
    pub trust_proxy: bool,
    /// Whether the environment allow list is malformed and refuses all remote peers.
    pub invalid_allow_ips: bool,
}

/// Resolves listener access settings without reading process-global state.
/// Setting `allow_env` replaces the configured list, including with an empty
/// list. Any malformed environment entry marks the whole list invalid.
#[must_use]
pub fn effective_network(
    config: &Config,
    allow_env: Option<&str>,
    trust_env: Option<&str>,
) -> NetworkSettings {
    let trust_proxy = trust_env.map_or(config.trust_proxy, |value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "t" | "true"
        )
    });
    let Some(value) = allow_env else {
        return NetworkSettings {
            allow_ips: config.allow_ips.clone(),
            trust_proxy,
            invalid_allow_ips: false,
        };
    };
    let entries: Vec<String> = value
        .split(|character: char| character == ',' || character == ';' || character.is_whitespace())
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    match clean_allow_ips(&entries) {
        Ok(allow_ips) => NetworkSettings {
            allow_ips,
            trust_proxy,
            invalid_allow_ips: false,
        },
        Err(_) => NetworkSettings {
            allow_ips: entries,
            trust_proxy,
            invalid_allow_ips: true,
        },
    }
}

pub(crate) fn clean_allow_ips(entries: &[String]) -> Result<Vec<String>, ConfigError> {
    let mut cleaned = Vec::new();
    for raw in entries {
        if raw.trim().is_empty() {
            continue;
        }
        let network = IpNetwork::parse(raw).map_err(|_| {
            ConfigError::Invalid(format!(
                "allow_ips entry `{raw}` is not an IP address or CIDR range"
            ))
        })?;
        let text = network.to_string();
        if !cleaned.contains(&text) {
            cleaned.push(text);
        }
    }
    Ok(cleaned)
}

fn unmap(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    }
}

fn mask(address: IpAddr, prefix: u8) -> IpAddr {
    match address {
        IpAddr::V4(address) => {
            let bits = u32::from(address);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            IpAddr::V4(Ipv4Addr::from(bits & mask))
        }
        IpAddr::V6(address) => {
            let bits = u128::from(address);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            IpAddr::V6(Ipv6Addr::from(bits & mask))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::{IpNetwork, effective_network};
    use crate::load;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("IP address")
    }

    #[test]
    fn r22_config_parses_cleans_and_deduplicates_allow_ips() {
        let config = load(
            r#"
            allow_ips = [
              " 192.168.1.7 ",
              "10.0.0.9/8",
              "2001:db8::1/32",
              "::ffff:192.0.2.1",
              "192.168.1.7",
              "",
            ]
            trust_proxy = true
            "#,
        )
        .expect("valid allow list");
        assert_eq!(
            config.allow_ips,
            ["192.168.1.7", "10.0.0.0/8", "2001:db8::/32", "192.0.2.1"]
        );
        assert!(config.trust_proxy);
        assert!(!format!("{config:?}").is_empty());
    }

    #[test]
    fn r22_cidr_edges_mask_host_bits_and_match_mapped_ipv4() {
        for (entry, inside, outside) in [
            ("0.0.0.0/0", "203.0.113.9", "2001:db8::1"),
            ("192.0.2.255/32", "192.0.2.255", "192.0.2.254"),
            ("::/0", "2001:db8::1", "192.0.2.1"),
            ("2001:db8::1/128", "2001:db8::1", "2001:db8::2"),
            ("192.168.1.99/24", "192.168.1.200", "192.168.2.1"),
            ("::ffff:192.0.2.99/120", "192.0.2.200", "192.0.3.1"),
        ] {
            let network =
                IpNetwork::parse(entry).unwrap_or_else(|error| panic!("{entry}: {error}"));
            assert!(network.contains(ip(inside)), "{entry} contains {inside}");
            assert!(!network.contains(ip(outside)), "{entry} excludes {outside}");
        }
        let v4 = IpNetwork::parse("10.0.0.5").expect("bare IPv4");
        assert!(v4.contains(ip("::ffff:10.0.0.5")));
        assert!(!v4.contains(ip("::ffff:10.0.0.6")));
        assert_eq!(
            IpNetwork::parse("2001:db8::5").expect("IPv6").to_string(),
            "2001:db8::5"
        );
    }

    #[test]
    fn r22_short_ipv6_prefix_over_mapped_range_stays_ipv6() {
        let network = IpNetwork::parse("::ffff:0:0/64").expect("IPv6 /64");
        assert_eq!(network.to_string(), "::/64");
        assert!(network.contains(ip("::5")));
        assert!(!network.contains(ip("2001:db8::5")));
        assert!(IpNetwork::parse("10.0.0.0/8/9").is_err());
    }

    #[test]
    fn r22_invalid_config_entry_names_the_entry() {
        for entry in ["not-an-ip", "10.0.0.0/33", "2001:db8::/129", "[::1"] {
            let error = load(&format!("allow_ips = [\"{entry}\"]\n"))
                .expect_err("invalid allow-list entry");
            assert!(error.to_string().contains("allow_ips"), "{error}");
            assert!(error.to_string().contains(entry), "{error}");
        }
    }

    #[test]
    fn r22_environment_replaces_config_and_invalid_entries_fail_closed() {
        let config = load("allow_ips = [\"192.0.2.1\"]\ntrust_proxy = true\n").expect("config");
        let effective = effective_network(
            &config,
            Some(" 203.0.113.0/24,10.0.0.5\n2001:db8::/32;198.51.100.1 "),
            Some("false"),
        );
        assert_eq!(
            effective.allow_ips,
            [
                "203.0.113.0/24",
                "10.0.0.5",
                "2001:db8::/32",
                "198.51.100.1"
            ]
        );
        assert!(!effective.trust_proxy);
        assert!(!effective.invalid_allow_ips);

        let empty = effective_network(&config, Some(" , \t"), None);
        assert!(empty.allow_ips.is_empty());
        assert!(!empty.invalid_allow_ips);
        assert!(empty.trust_proxy);

        for value in ["not-an-ip", "203.0.113.0/24 invalid", "10.0.0.0/33"] {
            let invalid = effective_network(&config, Some(value), None);
            assert!(invalid.invalid_allow_ips, "{value}");
            assert!(!invalid.allow_ips.is_empty(), "{value}");
        }
        let unset = effective_network(&config, None, None);
        assert_eq!(unset.allow_ips, ["192.0.2.1"]);
        assert!(unset.trust_proxy);
    }

    #[test]
    fn r22_trust_proxy_environment_parses_booleans_and_invalid_is_false() {
        let config = load("trust_proxy = true\n").expect("config");
        for value in ["1", "t", "TRUE", "true", "True"] {
            assert!(
                effective_network(&config, None, Some(value)).trust_proxy,
                "{value}"
            );
        }
        for value in ["0", "f", "FALSE", "false", "False", "", "yes", "invalid"] {
            assert!(
                !effective_network(&config, None, Some(value)).trust_proxy,
                "{value}"
            );
        }
    }
}

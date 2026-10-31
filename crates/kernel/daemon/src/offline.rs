//! Tells a provider that could not be reached from a computer with no network.
//!
//! A failed connection usually means the provider is down, and the member
//! rests. When this computer itself has no network, every provider fails the
//! same way, and resting them all would leave the routes empty once the
//! network is back. So a connection failure, while no upstream has answered in
//! the last [`HEARD_FRESH`], asks whether this computer is online. Offline, the
//! member is passed over without a rest, and the last member's failure goes
//! back to the client.
//!
//! The answer is kept for [`VERDICT_KEEP`], and one request asks at a time:
//! the others wait for its answer.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use hofund_plugin_api::{PluginError, PluginErrorKind};
use tokio::time::Instant;

/// How long an answer from any upstream vouches for the network.
pub(crate) const HEARD_FRESH: Duration = Duration::from_secs(15);
/// How long one network check's answer is kept.
pub(crate) const VERDICT_KEEP: Duration = Duration::from_secs(5);
/// How long each dial of another provider may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(2);
/// How many other providers are dialed at once.
const DIAL_MAX: usize = 3;

/// Words in an error that say no connection was made.
static CONN_FAILED: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        "dial tcp |dial udp |lookup [^ ]+|TLS handshake timeout|network is unreachable|no route to host|Unable to connect|ConnectionRefused|FailedToOpenSocket|ENOTFOUND|EAI_AGAIN|ECONNREFUSED|ENETUNREACH|EHOSTUNREACH",
    )
    .expect("connection failure words")
});

/// Answers whether this computer is online.
pub(crate) trait NetworkCheck: Send + Sync {
    /// `hosts` are other providers' `host:port`, at most [`DIAL_MAX`] of them.
    fn online(&self, hosts: Vec<String>) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
}

/// The computer's own network: a route out, then a dial of other providers.
pub(crate) struct SystemNetwork;

impl NetworkCheck for SystemNetwork {
    fn online(&self, hosts: Vec<String>) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            if !has_route() {
                return false;
            }
            if hosts.is_empty() {
                // Nothing else to dial; a route out counts as online.
                return true;
            }
            let mut dials: FuturesUnordered<_> = hosts
                .into_iter()
                .map(|host| async move {
                    matches!(
                        tokio::time::timeout(DIAL_TIMEOUT, tokio::net::TcpStream::connect(host))
                            .await,
                        Ok(Ok(_))
                    )
                })
                .collect();
            while let Some(connected) = dials.next().await {
                if connected {
                    return true;
                }
            }
            false
        })
    }
}

/// Whether the routing table has a way out with a usable address.
///
/// Connecting a UDP socket sends nothing: it only picks the route and the
/// local address. The targets are documentation addresses.
fn has_route() -> bool {
    [("0.0.0.0:0", "192.0.2.1:9"), ("[::]:0", "[2001:db8::1]:9")]
        .into_iter()
        .any(|(local, remote)| {
            UdpSocket::bind(local)
                .and_then(|socket| {
                    socket.connect(remote)?;
                    socket.local_addr()
                })
                .is_ok_and(|address| usable(address.ip()))
        })
}

fn usable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => !(v4.is_loopback() || v4.is_unspecified() || v4.is_link_local()),
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified() || v6.is_unicast_link_local()),
    }
}

/// Whether `error` says no connection to the upstream was made.
pub(crate) fn no_connection(error: &PluginError) -> bool {
    match error.kind() {
        PluginErrorKind::Connect => true,
        PluginErrorKind::Status if error.status() != Some(502) => false,
        PluginErrorKind::Status | PluginErrorKind::Transport | PluginErrorKind::Io => {
            CONN_FAILED.is_match(error.message())
        }
        _ => false,
    }
}

/// The `host:port` of a provider's base URL, unless it is on this computer
/// or its local network, where a dial says nothing about the internet.
pub(crate) fn far_host(base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url.trim()).ok()?;
    let port = url.port_or_known_default()?;
    match url.host()? {
        url::Host::Domain(name) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            let near = name == "localhost"
                || name.ends_with(".localhost")
                || name.ends_with(".local")
                || name.ends_with(".internal")
                || !name.contains('.');
            (!near).then(|| format!("{name}:{port}"))
        }
        url::Host::Ipv4(v4) => {
            let near =
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified();
            (!near).then(|| SocketAddr::from((v4, port)).to_string())
        }
        url::Host::Ipv6(v6) => {
            let near = v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
                || v6.is_unique_local();
            (!near).then(|| SocketAddr::from((v6, port)).to_string())
        }
    }
}

/// Last-heard time, the kept answer, and the hosts to dial.
pub(crate) struct Offline {
    check: Arc<dyn NetworkCheck>,
    heard: Mutex<Option<Instant>>,
    verdict: Mutex<Option<(Instant, bool)>>,
    asking: tokio::sync::Mutex<()>,
    hosts: Mutex<BTreeMap<String, String>>,
}

impl Offline {
    pub(crate) fn new(check: Arc<dyn NetworkCheck>) -> Self {
        Self {
            check,
            heard: Mutex::new(None),
            verdict: Mutex::new(None),
            asking: tokio::sync::Mutex::new(()),
            hosts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replaces the dialable host of each provider, keyed by provider id.
    pub(crate) fn set_hosts<'a>(&self, providers: impl IntoIterator<Item = (&'a str, &'a str)>) {
        let hosts = providers
            .into_iter()
            .filter_map(|(id, base_url)| far_host(base_url).map(|host| (id.to_owned(), host)))
            .collect();
        *self.hosts.lock().unwrap_or_else(|error| error.into_inner()) = hosts;
    }

    /// Records that an upstream answered.
    pub(crate) fn heard(&self) {
        *self.heard.lock().unwrap_or_else(|error| error.into_inner()) = Some(Instant::now());
    }

    fn heard_recently(&self) -> bool {
        self.heard
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some_and(|at| at.elapsed() < HEARD_FRESH)
    }

    /// Whether `error` from `provider` came from this computer having no network.
    pub(crate) async fn offline_failure(&self, provider: &str, error: &PluginError) -> bool {
        if !no_connection(error) || self.heard_recently() {
            return false;
        }
        let _turn = self.asking.lock().await;
        if let Some((at, offline)) = *self
            .verdict
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            && at.elapsed() < VERDICT_KEEP
        {
            return offline;
        }
        let offline = !self.check.online(self.others(provider)).await;
        *self
            .verdict
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((Instant::now(), offline));
        offline
    }

    /// Up to [`DIAL_MAX`] hosts of providers other than `provider`.
    fn others(&self, provider: &str) -> Vec<String> {
        let hosts = self.hosts.lock().unwrap_or_else(|error| error.into_inner());
        let own = hosts.get(provider);
        let mut others: Vec<String> = Vec::new();
        for (id, host) in hosts.iter() {
            if id != provider && Some(host) != own && !others.contains(host) {
                others.push(host.clone());
            }
            if others.len() == DIAL_MAX {
                break;
            }
        }
        others
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A network check with a fixed answer that records what it was asked.
    pub(crate) struct Fixed {
        pub online: std::sync::atomic::AtomicBool,
        pub asked: Mutex<Vec<Vec<String>>>,
    }

    impl NetworkCheck for Fixed {
        fn online(&self, hosts: Vec<String>) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
            self.asked.lock().expect("asked").push(hosts);
            let online = self.online.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { online })
        }
    }

    fn fixed(online: bool) -> Arc<Fixed> {
        Arc::new(Fixed {
            online: std::sync::atomic::AtomicBool::new(online),
            asked: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn b8x_connection_failures_are_told_by_kind_and_words() {
        assert!(no_connection(&PluginError::connect("refused")));
        assert!(no_connection(&PluginError::status_error(
            502,
            "dial tcp 1.2.3.4:443: connect: network is unreachable"
        )));
        assert!(no_connection(&PluginError::transport(
            "getaddrinfo ENOTFOUND api.example.com"
        )));
        assert!(!no_connection(&PluginError::status_error(
            502,
            "Bad Gateway"
        )));
        assert!(!no_connection(&PluginError::status_error(
            503,
            "dial tcp 1.2.3.4:443: connect: network is unreachable"
        )));
        assert!(!no_connection(&PluginError::timeout("read timed out")));
    }

    #[test]
    fn b8x_only_far_hosts_are_dialed() {
        assert_eq!(
            far_host("https://api.example.com/v1").as_deref(),
            Some("api.example.com:443")
        );
        assert_eq!(
            far_host("http://8.8.8.8:8080").as_deref(),
            Some("8.8.8.8:8080")
        );
        assert_eq!(
            far_host("https://[2606:4700::1111]/").as_deref(),
            Some("[2606:4700::1111]:443")
        );
        for near in [
            "http://localhost:11434",
            "http://127.0.0.1:8000",
            "http://192.168.1.4:1234",
            "http://10.0.0.2",
            "http://169.254.1.1",
            "http://[::1]:8080",
            "http://[fd00::1]",
            "http://[fe80::1]",
            "http://studio.local:1234",
            "http://gpu-box:8000",
            "",
            "not a url",
        ] {
            assert_eq!(far_host(near), None, "{near}");
        }
    }

    #[test]
    fn b8x_other_providers_are_dialed_not_the_failing_one() {
        let offline = Offline::new(fixed(true));
        offline.set_hosts([
            ("a", "https://a.example.com"),
            ("b", "https://b.example.com"),
            ("c", "https://a.example.com/other"),
            ("d", "http://localhost:1"),
            ("e", "https://e.example.com"),
            ("f", "https://f.example.com"),
            ("g", "https://g.example.com"),
        ]);
        assert_eq!(
            offline.others("a"),
            vec![
                "b.example.com:443".to_owned(),
                "e.example.com:443".to_owned(),
                "f.example.com:443".to_owned()
            ]
        );
        assert_eq!(
            offline.others("b"),
            vec![
                "a.example.com:443".to_owned(),
                "e.example.com:443".to_owned(),
                "f.example.com:443".to_owned()
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn b8x_a_fresh_answer_skips_the_check_and_a_verdict_is_kept() {
        let check = fixed(false);
        let offline = Offline::new(Arc::clone(&check) as Arc<dyn NetworkCheck>);
        let refused = PluginError::connect("refused");

        offline.heard();
        assert!(!offline.offline_failure("a", &refused).await);
        assert!(check.asked.lock().expect("asked").is_empty());

        tokio::time::advance(HEARD_FRESH).await;
        assert!(offline.offline_failure("a", &refused).await);
        assert_eq!(check.asked.lock().expect("asked").len(), 1);

        // Kept: the network comes back, but the kept answer stands a while.
        check
            .online
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::advance(VERDICT_KEEP - Duration::from_millis(1)).await;
        assert!(offline.offline_failure("a", &refused).await);
        assert_eq!(check.asked.lock().expect("asked").len(), 1);

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(!offline.offline_failure("a", &refused).await);
        assert_eq!(check.asked.lock().expect("asked").len(), 2);

        // A failure that is not about the connection never asks.
        tokio::time::advance(VERDICT_KEEP).await;
        assert!(
            !offline
                .offline_failure("a", &PluginError::status_error(503, "busy"))
                .await
        );
        assert_eq!(check.asked.lock().expect("asked").len(), 2);
    }

    #[test]
    fn b8x_loopback_and_link_local_are_not_a_way_out() {
        assert!(!usable("127.0.0.1".parse().expect("ip")));
        assert!(!usable("0.0.0.0".parse().expect("ip")));
        assert!(!usable("169.254.3.4".parse().expect("ip")));
        assert!(!usable("::1".parse().expect("ip")));
        assert!(!usable("fe80::1".parse().expect("ip")));
        assert!(usable("192.168.1.9".parse().expect("ip")));
        assert!(usable("2001:db8::9".parse().expect("ip")));
    }
}

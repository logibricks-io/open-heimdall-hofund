//! The Codex client version named on ChatGPT requests.
//!
//! The effective version is the newest plain `x.y.z` among a fixed floor,
//! npm's latest `@openai/codex` release, and a Codex client already seen on
//! a routed request. The npm document is read lazily, at most once every
//! six hours on the host monotonic clock, with a short timeout, through the
//! host HTTP client. A failed or non-release read keeps the last good value,
//! or the floor when none
//! is stored. Tests leave [`crate::Endpoints::npm_latest`] empty, so they
//! do not call the registry.
//!
//! Account discovery reads `auth.json` and does not report a Codex CLI
//! version, so an installed CLI is not a source.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use hofund_plugin_api::{HostContext, Request};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};

use crate::Endpoints;

/// Oldest Codex client version this plugin will name.
///
/// The ChatGPT backend lists a model only to a client at or past that
/// model's `minimal_client_version`. GPT-6.2 Sol and Astra need `0.166.0`.
pub(crate) const CODEX_VERSION_FLOOR: &str = "0.169.1";

/// npm document whose `version` field is the latest `@openai/codex` release.
pub(crate) const NPM_LATEST: &str = "https://registry.npmjs.org/@openai/codex/latest";

/// Header map the gateway copies onto a routed request.
pub(crate) const CLIENT_HEADERS: &str = "hofund_client_request_headers";

const PUBLISHED_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const NPM_READ_TIMEOUT: Duration = Duration::from_secs(2);
const BODY_CAP: usize = 4 << 20;

#[derive(Default)]
struct State {
    published: String,
    asked_at: Option<Duration>,
    asking: bool,
    seen: String,
}

/// Versions this subscription instance has read or seen.
#[derive(Default)]
pub(crate) struct ClientVersion {
    inner: Mutex<State>,
}

struct FetchGuard<'a> {
    state: &'a Mutex<State>,
    armed: bool,
}

impl Drop for FetchGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.asking = false;
    }
}

impl ClientVersion {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The version to send now: the floor, the last npm release, and the
    /// newest Codex client already seen, whichever is newest.
    #[must_use]
    pub(crate) fn current(&self) -> String {
        let guard = self.lock();
        let floor = pick(CODEX_VERSION_FLOOR, &guard.published);
        pick(&floor, &guard.seen)
    }

    /// Reads npm when `npm` is set and the last read is older than six hours.
    ///
    /// Returns the current version. A missing URL, which tests use, does not
    /// touch the network. The read waits at most [`NPM_READ_TIMEOUT`].
    pub(crate) async fn refresh(&self, host: &HostContext, npm: Option<&str>) {
        let Some(url) = npm.map(str::trim).filter(|url| !url.is_empty()) else {
            return;
        };
        let now = host.clock().monotonic();
        {
            let mut guard = self.lock();
            if guard.asking || fresh(&guard, now) {
                return;
            }
            guard.asking = true;
        }
        let mut hold = FetchGuard {
            state: &self.inner,
            armed: true,
        };
        let found = fetch_published(host, url).await;
        {
            let mut guard = self.lock();
            if let Some(version) = found {
                guard.published = version;
            }
            guard.asked_at = Some(host.clock().monotonic());
            guard.asking = false;
        }
        hold.armed = false;
    }

    /// Notes a Codex client from the headers on a routed request.
    pub(crate) fn note_request(&self, request: &Request) {
        let Some(map) = request
            .extensions
            .get(CLIENT_HEADERS)
            .and_then(Value::as_object)
        else {
            return;
        };
        self.note_headers(
            field(map, "originator").unwrap_or(""),
            field(map, "user-agent").unwrap_or(""),
            field(map, "version").unwrap_or(""),
        );
    }

    /// Notes the version header, or else the User-Agent, of one Codex client.
    ///
    /// A request that does not name Codex (no originator, and a User-Agent
    /// that does not start with `codex`) is ignored, so an unrelated
    /// `version` header cannot raise what this plugin signs as.
    pub(crate) fn note_headers(&self, originator: &str, user_agent: &str, version: &str) {
        let originator = originator.trim();
        let user_agent = user_agent.trim();
        if originator.is_empty() && !user_agent.to_ascii_lowercase().starts_with("codex") {
            return;
        }
        let version = version.trim();
        if release(version).is_some() {
            self.note_release(version);
            return;
        }
        if let Some(found) = user_agent_version(user_agent) {
            self.note_release(found);
        }
    }

    fn note_release(&self, raw: &str) {
        if release(raw).is_none() {
            return;
        }
        let mut guard = self.lock();
        if is_newer(raw, &guard.seen) {
            guard.seen = raw.to_owned();
        }
    }
}

/// Refreshes npm when `endpoints` names a document, then returns [`ClientVersion::current`].
pub(crate) async fn prepare(
    host: &HostContext,
    endpoints: &Endpoints,
    versions: &ClientVersion,
) -> String {
    versions
        .refresh(host, endpoints.npm_latest.as_deref())
        .await;
    versions.current()
}

/// Whether `candidate` is a newer plain release than `than`.
#[must_use]
pub(crate) fn is_newer(candidate: &str, than: &str) -> bool {
    match (release(candidate), release(than)) {
        (Some(next), Some(have)) => next > have,
        (Some(_), None) => true,
        _ => false,
    }
}

fn pick(left: &str, right: &str) -> String {
    if is_newer(right, left) {
        right.to_owned()
    } else {
        left.to_owned()
    }
}

fn fresh(state: &State, now: Duration) -> bool {
    // A reading behind `asked_at` is not fresh: the interval is monotonic, and
    // a step backwards must not stretch the six hours.
    state
        .asked_at
        .is_some_and(|at| now >= at && now.saturating_sub(at) < PUBLISHED_EVERY)
}

fn field<'a>(map: &'a json::Map<String, Value>, name: &str) -> Option<&'a str> {
    map.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn user_agent_version(user_agent: &str) -> Option<&str> {
    let (_, rest) = user_agent.split_once('/')?;
    let token = rest.split([' ', '(']).next().unwrap_or(rest);
    release(token)?;
    Some(token)
}

/// A plain `x.y.z` release. A suffix (`-alpha`, `-beta`, `-rc`, or anything
/// else) is not one: the backend's lists are a release's.
fn release(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some((
        component(major, 4)?,
        component(minor, 4)?,
        component(patch, 6)?,
    ))
}

fn component(text: &str, max_digits: usize) -> Option<u64> {
    if text.is_empty() || text.len() > max_digits || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

async fn fetch_published(host: &HostContext, url: &str) -> Option<String> {
    let call = Call::get(url)
        .header("accept", "application/json")
        .read_timeout(NPM_READ_TIMEOUT);
    let reply = hofund_plugin_sdk::timeout(NPM_READ_TIMEOUT, call.fetch(host.http()))
        .await?
        .ok()?;
    if reply.status() != 200 {
        return None;
    }
    parse_published(reply.body())
}

fn parse_published(body: &[u8]) -> Option<String> {
    let capped = &body[..body.len().min(BODY_CAP)];
    let value: Value = json::from_slice(capped).ok()?;
    let version = value.get("version")?.as_str()?.trim();
    release(version)?;
    Some(version.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{PluginId, PluginKind, Request};
    use hofund_plugin_sdk::json::json;

    use super::{CLIENT_HEADERS, CODEX_VERSION_FLOOR, ClientVersion, is_newer, release};

    const NPM: &str = "https://npm.test/@openai/codex/latest";

    fn host_at(monotonic: Duration) -> (tempfile::TempDir, TestHost) {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for_with_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("codex"),
            dir.path(),
            monotonic,
        );
        (dir, host)
    }

    fn npm_body(version: &str) -> Vec<u8> {
        format!(r#"{{"name":"@openai/codex","version":"{version}"}}"#).into_bytes()
    }

    #[test]
    fn floor_wins_when_no_other_source_is_set() {
        let versions = ClientVersion::default();
        let (_dir, host) = host_at(Duration::ZERO);
        block_on(versions.refresh(host.context(), None));
        assert_eq!(versions.current(), CODEX_VERSION_FLOOR);
        assert!(
            host.http().calls().is_empty(),
            "a missing npm source must not be fetched"
        );
    }

    #[test]
    fn npm_release_newer_than_the_floor_wins() {
        let versions = ClientVersion::default();
        let (_dir, host) = host_at(Duration::ZERO);
        host.http().enqueue(npm_body("9.1.0"));
        block_on(versions.refresh(host.context(), Some(NPM)));
        assert_eq!(versions.current(), "9.1.0");
        assert_eq!(host.http().calls()[0].uri(), NPM);
    }

    #[test]
    fn npm_release_older_than_the_floor_keeps_the_floor() {
        let versions = ClientVersion::default();
        let (_dir, host) = host_at(Duration::ZERO);
        host.http().enqueue(npm_body("0.100.0"));
        block_on(versions.refresh(host.context(), Some(NPM)));
        assert_eq!(versions.current(), CODEX_VERSION_FLOOR);
    }

    #[test]
    fn prerelease_npm_versions_are_ignored() {
        let base = Duration::from_secs(10_000);
        for suffix in ["9.2.0-alpha.3", "9.2.0-beta.1", "9.2.0-rc.1", "v9.2.0"] {
            let versions = ClientVersion::default();
            let (_dir, host) = host_at(base);
            host.http().enqueue(npm_body(suffix));
            block_on(versions.refresh(host.context(), Some(NPM)));
            assert_eq!(versions.current(), CODEX_VERSION_FLOOR, "{suffix}");
            assert!(release(suffix).is_none(), "{suffix}");
            assert_eq!(host.http().calls().len(), 1, "{suffix}");
        }
    }

    #[test]
    fn failed_npm_read_keeps_the_last_good_release() {
        let versions = ClientVersion::default();
        let base = Duration::from_secs(10_000);
        let (_early_dir, early) = host_at(base);
        early.http().enqueue(npm_body("9.1.0"));
        block_on(versions.refresh(early.context(), Some(NPM)));
        assert_eq!(versions.current(), "9.1.0");

        let (_late_dir, late) = host_at(base + Duration::from_secs(6 * 60 * 60));
        late.http().enqueue_response(500, &[], b"<html>nope</html>");
        block_on(versions.refresh(late.context(), Some(NPM)));
        assert_eq!(versions.current(), "9.1.0");
        assert_eq!(late.http().calls().len(), 1);

        let (_bad_dir, bad) = host_at(base + Duration::from_secs(12 * 60 * 60));
        bad.http().enqueue(b"not-json".to_vec());
        block_on(versions.refresh(bad.context(), Some(NPM)));
        assert_eq!(versions.current(), "9.1.0");
    }

    #[test]
    fn npm_is_read_at_most_once_every_six_hours() {
        let versions = ClientVersion::default();
        let base = Duration::from_secs(10_000);
        let (_early_dir, early) = host_at(base);
        early.http().enqueue(npm_body("9.1.0"));
        block_on(versions.refresh(early.context(), Some(NPM)));
        assert_eq!(early.http().calls().len(), 1);

        let (_mid_dir, mid) = host_at(base + Duration::from_secs(6 * 60 * 60 - 1));
        mid.http().enqueue(npm_body("9.9.0"));
        block_on(versions.refresh(mid.context(), Some(NPM)));
        assert!(
            mid.http().calls().is_empty(),
            "a read inside six hours must not run"
        );
        assert_eq!(versions.current(), "9.1.0");

        let (_late_dir, late) = host_at(base + Duration::from_secs(6 * 60 * 60));
        late.http().enqueue(npm_body("9.3.0"));
        block_on(versions.refresh(late.context(), Some(NPM)));
        assert_eq!(late.http().calls().len(), 1);
        assert_eq!(versions.current(), "9.3.0");
        assert!(is_newer("9.3.0", "9.1.0"));
        assert!(!is_newer("9.1.0", "9.3.0"));
    }

    #[test]
    fn a_newer_codex_client_wins_over_the_floor_and_npm() {
        let versions = ClientVersion::default();
        let (_dir, host) = host_at(Duration::ZERO);
        host.http().enqueue(npm_body("0.170.0"));
        block_on(versions.refresh(host.context(), Some(NPM)));
        assert_eq!(versions.current(), "0.170.0");

        versions.note_headers("", "Mozilla/5.0", "9.9.9");
        versions.note_headers("", "claude-cli/2.1.300 (external, cli)", "");
        assert_eq!(versions.current(), "0.170.0");

        versions.note_headers(
            "codex_cli_rs",
            "codex_cli_rs/0.159.0 (Linux; x86_64) unknown",
            "0.159.0",
        );
        assert_eq!(
            versions.current(),
            "0.170.0",
            "an older client must not lower the version"
        );

        versions.note_headers(
            "codex_cli_rs",
            "codex_cli_rs/0.200.0 (Linux; x86_64) unknown",
            "0.200.0",
        );
        assert_eq!(versions.current(), "0.200.0");

        versions.note_headers(
            "Codex Desktop",
            "Codex Desktop/0.172.3 (Windows 10.0.26100; x86_64) unknown",
            "",
        );
        assert_eq!(versions.current(), "0.200.0");

        let mut request = Request::new("gpt-5");
        request.extensions.insert(
            CLIENT_HEADERS,
            json!({
                "user-agent": "codex_cli_rs/0.220.0 (Linux; x86_64) unknown",
                "version": "0.220.0",
                "originator": "codex_cli_rs",
            }),
        );
        versions.note_request(&request);
        assert_eq!(versions.current(), "0.220.0");
    }

    #[test]
    fn floor_is_0_169_1_and_covers_gpt_6_2() {
        assert_eq!(CODEX_VERSION_FLOOR, "0.169.1");
        assert!(
            !is_newer("0.166.0", CODEX_VERSION_FLOOR),
            "the floor must not be older than GPT-6.2's minimal client version"
        );
    }

    #[test]
    fn a_backwards_monotonic_clock_reads_npm_again() {
        let versions = ClientVersion::default();
        let (_early_dir, early) = host_at(Duration::from_secs(6 * 60 * 60));
        early.http().enqueue(npm_body("9.1.0"));
        block_on(versions.refresh(early.context(), Some(NPM)));
        assert_eq!(versions.current(), "9.1.0");

        let (_back_dir, back) = host_at(Duration::from_secs(1));
        back.http().enqueue(npm_body("9.4.0"));
        block_on(versions.refresh(back.context(), Some(NPM)));
        assert_eq!(back.http().calls().len(), 1);
        assert_eq!(versions.current(), "9.4.0");
    }
}

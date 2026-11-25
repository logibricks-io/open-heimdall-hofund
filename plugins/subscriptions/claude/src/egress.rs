//! Where a Claude account's requests leave this computer, and whether that
//! place is on the supported-region list effective 2026-11-12.
//!
//! The lookup reads Cloudflare's trace page through the same proxy choice the
//! account uses. It runs at start, when the network or a proxy choice changes,
//! every 15 minutes, and when Check again asks. It does not run on each
//! request, and it does not run when no account is stored. `XX`, `T1`, and a
//! failed or odd answer are unknown: they warn nothing and hold nothing.
//!
//! The hold is opt-in (`region-hold`, off by default). A fresh unsupported
//! reading stops a Claude request before Claude Code starts. Nobody rests for
//! that try. A rule-based proxy can send the trace host and the vendor through
//! different nodes, so a wrong unsupported reading must not stop requests
//! unless the operator asked.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::SecretObject;

use crate::proxy::PROXY_KEY;
use crate::regions::supported_loc;

/// How often a choice is looked up again when nothing else changed.
const EVERY: Duration = Duration::from_secs(15 * 60);

/// How old an unsupported reading may be before a request looks again.
const FRESH: Duration = Duration::from_secs(30);

/// How long one lookup may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Trace page. Tests pass their own address to [`fetch_trace`].
pub(crate) const TRACE_URL: &str = "https://www.cloudflare.com/cdn-cgi/trace";

/// Header the host strips and uses to pick the outbound proxy.
const OUTBOUND_PROXY: &str = hofund_plugin_api::OUTBOUND_PROXY;

/// Words every held error starts with. A held try never left, so nobody rests.
pub(crate) const HELD_WORDS: &str = "hofund held this Claude request";

/// The policy page text this build was checked against.
#[cfg(test)]
const POLICY_PAGE: &str = include_str!("supported-regions.txt");

/// What the last lookup through one proxy choice found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// The code is on the list, or a listed country's territory.
    Supported,
    /// The code is a real region the list does not name.
    Unsupported,
    /// Not looked up, or the lookup failed, or the code is not a country.
    Unknown,
}

/// One stored lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reading {
    /// Classification.
    pub state: State,
    /// Uppercase code from the trace, when one was present.
    pub loc: String,
    /// Proxy choice as it may be shown, with any password removed.
    pub via: String,
    /// When the lookup finished. Absent when this choice was never looked up.
    pub at: Option<SystemTime>,
    /// Why a lookup did not yield a region. Empty when it did.
    pub error: String,
}

/// Body or failure from one trace read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TraceAnswer {
    /// HTTP status. Zero when the call did not receive one.
    pub status: u16,
    /// Response body, capped by the caller.
    pub body: String,
    /// Transport failure, already redacted. Empty when a status arrived.
    pub error: String,
}

impl TraceAnswer {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            status: 0,
            body: String::new(),
            error: error.into(),
        }
    }

    /// A 200 body whose `loc` line is `loc`.
    #[cfg(test)]
    fn loc(loc: &str) -> Self {
        Self {
            status: 200,
            body: format!("fl=1\nloc={loc}\n"),
            error: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Allow,
    Kick,
    Refresh,
    Deny,
}

struct Inner {
    rows: BTreeMap<String, Reading>,
    choices: Vec<String>,
    fingerprint: String,
    checked_at: Option<SystemTime>,
    kicked: bool,
}

impl Inner {
    fn new() -> Self {
        Self {
            rows: BTreeMap::new(),
            choices: Vec::new(),
            fingerprint: String::new(),
            checked_at: None,
            kicked: false,
        }
    }
}

/// Lookups for one subscription instance, and the opt-in hold.
pub(crate) struct Egress {
    hold: bool,
    inner: Mutex<Inner>,
}

impl Egress {
    /// `hold` is the `region-hold` option. Off, lookups still warn.
    #[must_use]
    pub(crate) fn new(hold: bool) -> Self {
        Self {
            hold,
            inner: Mutex::new(Inner::new()),
        }
    }

    /// Whether the operator asked to hold requests.
    #[must_use]
    pub(crate) fn holding(&self) -> bool {
        self.hold
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Stores `answer` for `choice`.
    pub(crate) fn note(&self, choice: &str, answer: &TraceAnswer, now: SystemTime) -> Reading {
        let choice = choice.trim();
        let mut reading = judge_answer(answer, now);
        reading.via = shown(choice);
        self.lock().rows.insert(choice.to_owned(), reading.clone());
        reading
    }

    /// Whether `choices` should be looked up at `now`.
    ///
    /// Empty `choices` is never due: no account, no lookup. A kick, a new
    /// fingerprint, the first pass, and a reading older than [`EVERY`] are due.
    #[must_use]
    pub(crate) fn due(&self, choices: &[String], fingerprint: &str, now: SystemTime) -> bool {
        if choices.is_empty() {
            return false;
        }
        let inner = self.lock();
        if inner.kicked {
            return true;
        }
        let Some(checked) = inner.checked_at else {
            return true;
        };
        if fingerprint != inner.fingerprint {
            return true;
        }
        age(now, checked) >= EVERY
    }

    /// Records that `choices` were just looked up.
    pub(crate) fn mark(&self, choices: &[String], fingerprint: &str, now: SystemTime) {
        let mut inner = self.lock();
        inner.choices = choices.to_vec();
        inner.fingerprint = fingerprint.to_owned();
        inner.checked_at = Some(now);
        inner.kicked = false;
    }

    /// Asks the next maintain pass to look up again.
    pub(crate) fn kick(&self) {
        self.lock().kicked = true;
    }

    /// Warning for the worst current choice, or nothing when that is not unsupported.
    #[must_use]
    pub(crate) fn warning(&self) -> Option<String> {
        let inner = self.lock();
        if inner.choices.is_empty() {
            return None;
        }
        let reading = worst(&inner.choices, &inner.rows);
        if reading.state != State::Unsupported {
            return None;
        }
        Some(warning_text(&reading, self.hold))
    }

    /// Looks up each current account proxy when the cadence says to.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the account list cannot be read. A failed
    /// trace is an unknown reading, not an error.
    pub(crate) async fn maintain(&self, host: &HostContext) -> Result<(), PluginError> {
        let choices = account_choices(host).await?;
        let now = host.clock().now();
        let fingerprint = fingerprint(&choices);
        if !self.due(&choices, &fingerprint, now) {
            let mut inner = self.lock();
            inner.choices = choices;
            return Ok(());
        }
        for choice in &choices {
            let answer = fetch_trace(host, choice, TRACE_URL).await;
            self.note(choice, &answer, now);
        }
        self.mark(&choices, &fingerprint, now);
        Ok(())
    }

    /// Looks up every current choice now (Check again).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the account list cannot be read.
    pub(crate) async fn recheck(&self, host: &HostContext) -> Result<(), PluginError> {
        let choices = account_choices(host).await?;
        let now = host.clock().now();
        for choice in &choices {
            let answer = fetch_trace(host, choice, TRACE_URL).await;
            self.note(choice, &answer, now);
        }
        self.mark(&choices, &fingerprint(&choices), now);
        Ok(())
    }

    /// Holds `choice` when the operator asked and the last reading is unsupported.
    ///
    /// An unknown reading holds nothing. A choice never looked up is allowed
    /// and kicked so the next pass looks it up. An unsupported reading older
    /// than [`FRESH`] is looked up again first.
    ///
    /// # Errors
    ///
    /// Returns the held error when the request must not start Claude Code.
    pub(crate) async fn hold(&self, host: &HostContext, choice: &str) -> Result<(), PluginError> {
        if !self.hold {
            return Ok(());
        }
        let choice = choice.trim();
        let now = host.clock().now();
        match self.step(choice, now) {
            Step::Allow => Ok(()),
            Step::Kick => {
                self.kick();
                Ok(())
            }
            Step::Deny => Err(held_error(&self.stored(choice))),
            Step::Refresh => {
                let answer = fetch_trace(host, choice, TRACE_URL).await;
                let reading = self.note(choice, &answer, now);
                if reading.state == State::Unsupported {
                    Err(held_error(&reading))
                } else {
                    Ok(())
                }
            }
        }
    }

    fn step(&self, choice: &str, now: SystemTime) -> Step {
        if !self.hold {
            return Step::Allow;
        }
        let inner = self.lock();
        let Some(reading) = inner.rows.get(choice) else {
            return Step::Kick;
        };
        let Some(at) = reading.at else {
            return Step::Kick;
        };
        if reading.state == State::Unsupported && age(now, at) > FRESH {
            return Step::Refresh;
        }
        if reading.state == State::Unsupported {
            Step::Deny
        } else {
            Step::Allow
        }
    }

    fn stored(&self, choice: &str) -> Reading {
        self.lock()
            .rows
            .get(choice)
            .cloned()
            .unwrap_or_else(|| unseen(choice))
    }
}

/// Holds before a Claude Code run when `egress` says to.
///
/// # Errors
///
/// Returns the held error, or an account-store error.
pub(crate) async fn guard(
    egress: &Egress,
    host: &HostContext,
    account: &AccountId,
) -> Result<(), PluginError> {
    if !egress.holding() {
        return Ok(());
    }
    let choice = proxy_of(host, account).await?;
    egress.hold(host, &choice).await
}

fn worst(choices: &[String], rows: &BTreeMap<String, Reading>) -> Reading {
    let mut unknown = None;
    let mut supported = None;
    for choice in choices {
        let reading = rows.get(choice).cloned().unwrap_or_else(|| unseen(choice));
        match reading.state {
            State::Unsupported => return reading,
            State::Unknown => {
                if unknown.is_none() {
                    unknown = Some(reading);
                }
            }
            State::Supported => {
                if supported.is_none() {
                    supported = Some(reading);
                }
            }
        }
    }
    unknown.or(supported).unwrap_or_else(|| unseen(""))
}

fn unseen(choice: &str) -> Reading {
    Reading {
        state: State::Unknown,
        loc: String::new(),
        via: shown(choice),
        at: None,
        error: String::new(),
    }
}

fn judge_answer(answer: &TraceAnswer, now: SystemTime) -> Reading {
    if !answer.error.is_empty() || answer.status != 200 {
        let error = if answer.error.is_empty() {
            format!("HTTP {}", answer.status)
        } else {
            answer.error.clone()
        };
        return Reading {
            state: State::Unknown,
            loc: String::new(),
            via: String::new(),
            at: Some(now),
            error,
        };
    }
    judge_loc(&loc_from_trace(&answer.body), now)
}

fn judge_loc(loc: &str, now: SystemTime) -> Reading {
    let letters = loc.len() == 2 && loc.bytes().all(|byte| byte.is_ascii_uppercase());
    if !letters || loc == "XX" {
        return Reading {
            state: State::Unknown,
            loc: loc.to_owned(),
            via: String::new(),
            at: Some(now),
            error: "no region in the answer".to_owned(),
        };
    }
    let state = if supported_loc(loc) {
        State::Supported
    } else {
        State::Unsupported
    };
    Reading {
        state,
        loc: loc.to_owned(),
        via: String::new(),
        at: Some(now),
        error: String::new(),
    }
}

fn loc_from_trace(body: &str) -> String {
    let body = body.get(..body.len().min(16 << 10)).unwrap_or(body);
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "loc" {
            return value.trim().to_ascii_uppercase();
        }
    }
    String::new()
}

fn warning_text(reading: &Reading, hold: bool) -> String {
    let mut text = format!(
        "Claude's way out to the internet is in {}, outside Anthropic's supported regions. {} is not on Anthropic's list of supported regions. A proxy or VPN that turned off, or a node in another country, can cause this.",
        reading.loc, reading.loc
    );
    if hold {
        text.push_str(" Claude requests are held until it changes.");
    }
    text
}

fn held_error(reading: &Reading) -> PluginError {
    let via = match reading.via.as_str() {
        "" => "as the process proxy sends it".to_owned(),
        "direct" => "going direct".to_owned(),
        other => format!("through {other}"),
    };
    let message = format!(
        "{HELD_WORDS}: this computer's way out to the internet, {via}, is in region {}, which is not on Anthropic's list of supported regions. Turn the proxy or VPN back on, or set region-hold to false under the subscription options",
        reading.loc
    );
    PluginError::status_error(502, message).without_cooldown()
}

fn shown(choice: &str) -> String {
    let choice = choice.trim();
    match choice {
        "" | "direct" => choice.to_owned(),
        other => redact_url(other),
    }
}

fn redact_url(value: &str) -> String {
    let Some(rest) = value.split_once("://").map(|(_, rest)| rest) else {
        return value.to_owned();
    };
    let Some((userinfo, host)) = rest.split_once('@') else {
        return value.to_owned();
    };
    if userinfo.contains('/') || userinfo.contains(' ') {
        return value.to_owned();
    }
    let scheme = &value[..value.len() - rest.len()];
    format!("{scheme}{host}")
}

pub(crate) fn redact_error(text: &str, choice: &str) -> String {
    let mut out = text.to_owned();
    let choice = choice.trim();
    if !choice.is_empty() && choice != "direct" {
        out = out.replace(choice, &shown(choice));
    }
    if let Some(start) = out.find("://") {
        let after = start + 3;
        if let Some(at) = out[after..].find('@') {
            let userinfo = &out[after..after + at];
            if !userinfo.contains(' ') && !userinfo.contains('/') {
                out.replace_range(after..after + at + 1, "");
            }
        }
    }
    out
}

fn age(now: SystemTime, earlier: SystemTime) -> Duration {
    now.duration_since(earlier).unwrap_or_default()
}

/// Addresses and proxy choices. A change of either makes the next pass look up.
///
/// Local addresses come from `/proc` on Linux. When that is unreadable the
/// fingerprint is only the proxy choices, and a network change waits for the
/// 15 minute pass.
#[must_use]
pub(crate) fn fingerprint(choices: &[String]) -> String {
    let mut addrs = local_addrs();
    addrs.sort();
    addrs.dedup();
    let mut out = addrs.join(",");
    for choice in choices {
        out.push('|');
        out.push_str(choice.trim());
    }
    out
}

fn local_addrs() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/proc/net/fib_trie") {
        let mut previous = "";
        for line in text.lines() {
            if line.contains("host LOCAL")
                && let Some(addr) = previous.split_whitespace().last()
            {
                out.push(addr.to_owned());
            }
            previous = line;
        }
    }
    if let Ok(text) = std::fs::read_to_string("/proc/net/if_inet6") {
        for line in text.lines() {
            if let Some(hex) = line.split_whitespace().next() {
                out.push(hex.to_owned());
            }
        }
    }
    out
}

async fn account_choices(host: &HostContext) -> Result<Vec<String>, PluginError> {
    let accounts = host.accounts().list().await?;
    let mut out = Vec::new();
    for account in accounts {
        let choice = proxy_of(host, account.id()).await?;
        if !out.iter().any(|seen| seen == &choice) {
            out.push(choice);
        }
    }
    Ok(out)
}

async fn proxy_of(host: &HostContext, account: &AccountId) -> Result<String, PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Ok(String::new());
    };
    let Ok(blob) = SecretObject::parse(&secret) else {
        return Ok(String::new());
    };
    Ok(blob.text(PROXY_KEY).unwrap_or("").trim().to_owned())
}

/// Reads the trace at `url` through `choice`.
///
/// `direct` uses no proxy. An empty choice uses the process proxy. Any other
/// choice is that proxy URL. The proxy header is not forwarded.
pub(crate) async fn fetch_trace(host: &HostContext, choice: &str, url: &str) -> TraceAnswer {
    let mut call = Call::get(url).read_timeout(TIMEOUT);
    let trimmed = choice.trim();
    if trimmed.eq_ignore_ascii_case("direct") {
        call = call.header(OUTBOUND_PROXY, "direct");
    } else if !trimmed.is_empty() {
        call = call.sensitive_header(OUTBOUND_PROXY, trimmed);
    }
    match call.fetch(host.http()).await {
        Ok(reply) => {
            let mut body = String::from_utf8_lossy(reply.body()).into_owned();
            if body.len() > 16 << 10 {
                body.truncate(16 << 10);
            }
            TraceAnswer {
                status: reply.status(),
                body,
                error: String::new(),
            }
        }
        Err(error) => TraceAnswer::failed(redact_error(&error.to_string(), trimmed)),
    }
}

/// Names on the policy page, in file order.
#[cfg(test)]
pub(crate) fn policy_names() -> Vec<String> {
    POLICY_PAGE
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime};

    use hofund_plugin_api::{AccountId, PluginId, PluginKind, ProtocolId, UpstreamProvider};
    use hofund_plugin_sdk::oauth::SecretObject;

    use super::*;
    use crate::regions::{SUPPORTED_CODES, TERRITORY_CODES, supported_name, territory_of};

    fn epoch() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    #[test]
    fn d245_supported_names_match_the_policy_page() {
        let mut page = policy_names();
        let mut ours = Vec::new();
        for code in SUPPORTED_CODES {
            let name = supported_name(code).unwrap_or_else(|| panic!("missing {code}"));
            assert_eq!(code.len(), 2);
            assert!(code.bytes().all(|byte| byte.is_ascii_uppercase()));
            ours.push(name.to_owned());
        }
        assert_eq!(page.len(), 185, "policy page");
        assert_eq!(ours.len(), 185, "compiled list");
        page.sort();
        ours.sort();
        assert_eq!(page, ours);
        for code in TERRITORY_CODES {
            let parent = territory_of(code).expect("parent");
            assert!(supported_name(parent).is_some(), "{code} -> {parent}");
            assert!(supported_name(code).is_none(), "{code} listed itself");
        }
    }

    #[test]
    fn d245_judge_known_unknown_and_territories() {
        let now = epoch();
        let cases = [
            ("US", State::Supported),
            ("JP", State::Supported),
            ("PR", State::Supported),
            ("GB", State::Supported),
            ("TW", State::Supported),
            ("SG", State::Supported),
            ("UA", State::Supported),
            ("CN", State::Unsupported),
            ("HK", State::Unsupported),
            ("MO", State::Unsupported),
            ("RU", State::Unsupported),
            ("IR", State::Unsupported),
            ("KP", State::Unsupported),
            ("", State::Unknown),
            ("XX", State::Unknown),
            ("T1", State::Unknown),
            ("USA", State::Unknown),
            ("1A", State::Unknown),
        ];
        for (loc, want) in cases {
            let got = judge_loc(loc, now);
            assert_eq!(got.state, want, "{loc}");
        }
    }

    #[test]
    fn d245_trace_loc_is_uppercased_and_a_failure_is_unknown() {
        let egress = Egress::new(false);
        let now = epoch();
        let reading = egress.note("direct", &TraceAnswer::loc("cn"), now);
        assert_eq!(reading.state, State::Unsupported);
        assert_eq!(reading.loc, "CN");
        assert_eq!(reading.via, "direct");
        let failed = TraceAnswer {
            status: 503,
            body: String::new(),
            error: String::new(),
        };
        let reading = egress.note("", &failed, now);
        assert_eq!(reading.state, State::Unknown);
        assert_eq!(reading.error, "HTTP 503");
    }

    #[test]
    fn d245_worst_prefers_unsupported_then_unknown() {
        let egress = Egress::new(false);
        let now = epoch();
        egress.note("", &TraceAnswer::loc("US"), now);
        let choices = vec![String::new(), "direct".to_owned()];
        egress.mark(&choices, "fp", now);
        assert!(egress.warning().is_none(), "one choice not looked up");
        egress.note("direct", &TraceAnswer::loc("JP"), now);
        assert!(egress.warning().is_none());
        egress.note("direct", &TraceAnswer::loc("RU"), now);
        let warning = egress.warning().expect("warning");
        assert!(warning.contains("RU"));
        assert!(!warning.contains("ban") && !warning.contains("safe"));
    }

    #[test]
    fn d245_hold_is_off_by_default_and_unknown_holds_nothing() {
        let now = epoch();
        let off = Egress::new(false);
        off.note("", &TraceAnswer::loc("CN"), now);
        assert_eq!(off.step("", now), Step::Allow);
        let on = Egress::new(true);
        on.note("", &failed_trace(), now);
        assert_eq!(on.step("", now), Step::Allow);
        assert_eq!(on.step("socks5://127.0.0.1:9", now), Step::Kick);
    }

    #[test]
    fn d245_hold_refuses_a_fresh_unsupported_reading_and_refreshes_a_stale_one() {
        let now = epoch() + Duration::from_secs(100);
        let egress = Egress::new(true);
        egress.note("", &TraceAnswer::loc("CN"), now);
        assert_eq!(egress.step("", now), Step::Deny);
        let error = held_error(&egress.stored(""));
        assert!(!error.cools());
        assert_eq!(error.status(), Some(502));
        let text = error.to_string();
        assert!(text.starts_with(HELD_WORDS), "{text}");
        assert!(text.contains("CN"), "{text}");
        for word in ["ban", "Ban", "safe"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
        egress.note("", &TraceAnswer::loc("CN"), now - Duration::from_secs(31));
        assert_eq!(
            egress.step("", now),
            Step::Refresh,
            "a stale unsupported reading is looked up again"
        );
    }

    #[test]
    fn d245_cadence_runs_at_start_on_change_every_fifteen_minutes_and_on_kick() {
        let egress = Egress::new(false);
        let start = epoch() + Duration::from_secs(1_000);
        let choices = vec![String::new(), "direct".to_owned()];
        assert!(egress.due(&choices, "wifi-a", start));
        assert!(!egress.due(&[], "wifi-a", start), "no account, no lookup");
        egress.mark(&choices, "wifi-a", start);
        assert!(!egress.due(&choices, "wifi-a", start + Duration::from_secs(20)));
        assert!(egress.due(&choices, "wifi-b", start + Duration::from_secs(20)));
        assert!(egress.due(&choices, "wifi-a", start + Duration::from_secs(15 * 60)));
        egress.kick();
        assert!(egress.due(&choices, "wifi-a", start + Duration::from_secs(1)));
    }

    #[test]
    fn d245_redact_strips_a_proxy_password() {
        let raw = "proxyconnect tcp: dial http://bob:hunter2@10.0.0.1:8080: refused";
        let got = redact_error(raw, "http://bob:hunter2@10.0.0.1:8080");
        assert!(!got.contains("hunter2"), "{got}");
        assert_eq!(
            shown("http://bob:hunter2@10.0.0.1:8080"),
            "http://10.0.0.1:8080"
        );
    }

    #[test]
    fn d245_loopback_trace_reads_loc_and_drops_the_proxy_header() {
        hofund_kit_subscription::block_on(async {
            let server = hofund_kit_subscription::Http1Server::start(vec![
                hofund_kit_subscription::Http1Response::new(200).body(b"fl=1\nloc=CN\n".to_vec()),
            ])
            .await;
            let dir = tempfile::tempdir().expect("dir");
            let host =
                hofund_kit_subscription::TestHost::open_for_with_pooled_http_and_tokio_monotonic(
                    PluginKind::Subscription,
                    PluginId::from_static("claude"),
                    dir.path(),
                );
            let answer = fetch_trace(
                host.context(),
                "direct",
                &format!("{}/cdn-cgi/trace", server.base_url()),
            )
            .await;
            assert_eq!(answer.status, 200, "{answer:?}");
            let reading = judge_answer(&answer, epoch());
            assert_eq!(reading.state, State::Unsupported);
            assert_eq!(reading.loc, "CN");
            let recorded = server.requests();
            assert_eq!(recorded.len(), 1);
            assert!(
                recorded[0]
                    .headers()
                    .iter()
                    .all(|(name, _)| !name.eq_ignore_ascii_case("x-hofund-outbound-proxy"))
            );
        });
    }

    fn failed_trace() -> TraceAnswer {
        TraceAnswer {
            status: 0,
            body: String::new(),
            error: "connection refused".to_owned(),
        }
    }

    #[test]
    fn d245_fingerprint_is_stable_and_includes_the_choice() {
        let one = fingerprint(&["direct".to_owned()]);
        let two = fingerprint(&["direct".to_owned()]);
        assert_eq!(one, two);
        assert!(one.ends_with("|direct"), "{one}");
        assert_ne!(one, fingerprint(&[String::new()]));
    }

    #[test]
    fn d245_hold_stops_before_the_runner() {
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        let held = Arc::new(Egress::new(true));
        held.note("", &TraceAnswer::loc("CN"), epoch());
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let provider = crate::upstream::ClaudeUpstream::new(
            id.clone(),
            host.context().clone(),
            None,
            crate::bridge::Bridge::new(),
            crate::limits::Store::new(),
        )
        .with_egress(Arc::clone(&held))
        .with_runner(Arc::new(move |_| {
            flag.store(true, Ordering::SeqCst);
            Err(hofund_plugin_api::PluginError::internal("ran"))
        }));
        let error = match hofund_kit_subscription::block_on(
            provider.execute(text_request(), ProtocolId::AnthropicMessages),
        ) {
            Err(error) => error,
            Ok(_) => panic!("held request was served"),
        };
        assert!(!ran.load(Ordering::SeqCst));
        assert!(
            error.message().starts_with(HELD_WORDS),
            "{}",
            error.message()
        );
        assert!(!error.cools());
        assert_eq!(error.status(), Some(502));
    }

    #[test]
    fn d245_warmup_is_held_and_refresh_is_not() {
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        let blob = SecretObject::new().with_text("credentials", "not-a-token");
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "ada@example.test")
            .expect("account");
        hofund_kit_subscription::block_on(
            host.context().accounts().upsert(account, blob.to_secret()),
        )
        .expect("upsert");
        let egress = Arc::new(Egress::new(true));
        egress.note("", &TraceAnswer::loc("CN"), epoch());
        let error = hofund_kit_subscription::block_on(crate::account::ask(
            host.context(),
            None,
            &id,
            "haiku",
            &egress,
        ))
        .expect_err("held");
        assert!(
            error.message().starts_with(HELD_WORDS),
            "{}",
            error.message()
        );
        assert!(host.process().piped_spawns().is_empty());
        let refresh =
            hofund_kit_subscription::block_on(crate::account::refresh(host.context(), None, &id))
                .expect_err("refresh still runs");
        assert!(
            !refresh.message().contains(HELD_WORDS),
            "{}",
            refresh.message()
        );
    }

    fn text_request() -> hofund_plugin_api::Request {
        let mut message = hofund_plugin_api::Message::new(hofund_plugin_api::Role::User);
        message.content = vec![hofund_plugin_api::ContentPart::text("Hello")];
        hofund_plugin_api::Request::new("claude-sonnet-4-5").with_messages(vec![message])
    }
}

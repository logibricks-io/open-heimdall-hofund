//! Where a Claude account's requests leave this computer, and whether that
//! place is allowed.
//!
//! The lookup reads Cloudflare's trace page through the same proxy choice the
//! account uses, including `loc=` and `ip=`. It runs at start, when the
//! network or a proxy choice changes, every 15 minutes, and when Check asks.
//! It does not run when no account is stored. `XX`, `T1`, and a failed or odd
//! answer are unknown: they warn nothing and hold nothing.
//!
//! An empty `allowed-ways-out` list, the default, is judged by the
//! supported-region list effective 2026-11-12. A non-empty list of region
//! codes, addresses, and CIDR ranges judges alone. A stored address and region
//! are judged again against the current list on every read.
//!
//! The hold is opt-in (`region-hold`, off by default). While it is on, a
//! reading older than 30 seconds is looked up again first, within 5 seconds,
//! and requests that arrive during that lookup share it. A fresh unsupported
//! reading stops a Claude request before Claude Code starts. Nobody rests for
//! that try. A rule-based proxy can send the trace host and the vendor through
//! different nodes, so a wrong unsupported reading must not stop requests
//! unless the operator asked.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::SecretObject;

use crate::proxy::PROXY_KEY;
use crate::regions::supported_loc;

/// How often a choice is looked up again when nothing else changed.
const EVERY: Duration = Duration::from_secs(15 * 60);

/// How old a reading may be before a held request looks again.
const FRESH: Duration = Duration::from_secs(30);

/// How long one background lookup may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long the lookup a held request waits on may take.
const HOLD_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The code or address is allowed.
    Supported,
    /// The code or address is known and not allowed.
    Unsupported,
    /// Not looked up, or the lookup failed, or the answer cannot be judged.
    Unknown,
}

/// One judged lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reading {
    /// Classification.
    pub state: State,
    /// Uppercase code from the trace, when one was present.
    pub loc: String,
    /// Address from the trace, when one was present.
    pub ip: String,
    /// The user's allowed list judged this reading.
    pub mine: bool,
    /// Proxy choice as it may be shown, with any password removed.
    pub via: String,
    /// When the lookup finished. Absent when this choice was never looked up.
    pub at: Option<SystemTime>,
    /// Why a lookup did not yield something that could be judged. Empty when it did.
    pub error: String,
}

/// Facts kept for one choice. The list in force is applied on each read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stored {
    loc: String,
    ip: String,
    via: String,
    at: Option<SystemTime>,
    error: String,
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
    rows: BTreeMap<String, Stored>,
    choices: Vec<String>,
    fingerprint: String,
    checked_at: Option<SystemTime>,
    kicked: bool,
    ways: Vec<String>,
}

/// One lookup shared by every held request that asks for it.
struct Flight {
    result: Mutex<Option<Reading>>,
    wakers: Mutex<Vec<Waker>>,
    followers: AtomicU32,
}

impl Inner {
    fn new() -> Self {
        Self {
            rows: BTreeMap::new(),
            choices: Vec::new(),
            fingerprint: String::new(),
            checked_at: None,
            kicked: false,
            ways: Vec::new(),
        }
    }
}

impl Flight {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            wakers: Mutex::new(Vec::new()),
            followers: AtomicU32::new(0),
        }
    }

    fn current(&self) -> Option<Reading> {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn finish(&self, reading: Reading) {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return;
        }
        *slot = Some(reading);
        drop(slot);
        let wakers = std::mem::take(
            &mut *self
                .wakers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for waker in wakers {
            waker.wake();
        }
    }

    /// Keeps one waker per waiter. A later poll replaces that slot.
    fn remember(&self, slot: &mut Option<usize>, waker: &Waker) {
        let mut wakers = self
            .wakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = *slot
            && let Some(existing) = wakers.get_mut(index)
        {
            *existing = waker.clone();
            return;
        }
        *slot = Some(wakers.len());
        wakers.push(waker.clone());
    }

    fn wait(self: &Arc<Self>) -> FlightWait {
        FlightWait {
            flight: Arc::clone(self),
            slot: None,
        }
    }
}

struct FlightWait {
    flight: Arc<Flight>,
    slot: Option<usize>,
}

impl Future for FlightWait {
    type Output = Reading;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Reading> {
        let this = self.get_mut();
        if let Some(reading) = this.flight.current() {
            return Poll::Ready(reading);
        }
        this.flight.remember(&mut this.slot, context.waker());
        this.flight.current().map_or(Poll::Pending, Poll::Ready)
    }
}

/// Removes the shared lookup and wakes its waiters if the leader is dropped.
struct LeaderGuard<'a> {
    egress: &'a Egress,
    choice: String,
    flight: Arc<Flight>,
    armed: bool,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        self.egress
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.choice);
        let reading = self.egress.stored(&self.choice);
        self.flight.finish(reading);
    }
}

/// Lookups for one subscription instance, and the opt-in hold.
pub(crate) struct Egress {
    hold: bool,
    inner: Mutex<Inner>,
    flights: Mutex<BTreeMap<String, Arc<Flight>>>,
    trace_url: Mutex<String>,
}

impl Egress {
    /// `hold` is the `region-hold` option. Off, lookups still warn.
    #[must_use]
    pub(crate) fn new(hold: bool) -> Self {
        Self {
            hold,
            inner: Mutex::new(Inner::new()),
            flights: Mutex::new(BTreeMap::new()),
            trace_url: Mutex::new(TRACE_URL.to_owned()),
        }
    }

    /// Replaces the allowed list. An empty list uses the supported-region list.
    ///
    /// Stored readings are judged again on the next read.
    pub(crate) fn set_ways(&self, ways: Vec<String>) {
        self.lock().ways = ways;
    }

    /// Points later lookups at `url`. Tests pass a loopback trace.
    #[cfg(test)]
    fn set_trace_url(&self, url: &str) {
        *self
            .trace_url
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = url.to_owned();
    }

    /// Followers waiting on `choice`'s current lookup.
    #[cfg(test)]
    fn followers(&self, choice: &str) -> u32 {
        self.flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(choice.trim())
            .map(|flight| flight.followers.load(Ordering::SeqCst))
            .unwrap_or(0)
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

    /// Stores `answer` for `choice`, judged by the list in force.
    pub(crate) fn note(&self, choice: &str, answer: &TraceAnswer, now: SystemTime) -> Reading {
        let choice = choice.trim();
        let mut stored = store_answer(answer, now);
        stored.via = shown(choice);
        let mut inner = self.lock();
        let reading = project(&stored, &inner.ways);
        inner.rows.insert(choice.to_owned(), stored);
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
        let reading = worst(&inner.choices, &inner.rows, &inner.ways);
        if reading.state != State::Unsupported {
            return None;
        }
        Some(warning_text(&reading, self.hold))
    }

    /// Address and region of the worst current choice.
    ///
    /// `Not looked up yet` when accounts exist and no address or region has
    /// been read. Absent when there is nothing to show.
    #[must_use]
    pub(crate) fn place(&self) -> Option<String> {
        let inner = self.lock();
        if inner.choices.is_empty() && inner.rows.is_empty() {
            return None;
        }
        let reading = worst(&inner.choices, &inner.rows, &inner.ways);
        let line = place_line(&reading);
        if !line.is_empty() {
            return Some(line);
        }
        if inner.choices.is_empty() {
            None
        } else {
            Some("Not looked up yet".to_owned())
        }
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
            let answer = fetch_trace(host, choice, &self.trace_target(), TIMEOUT).await;
            self.note(choice, &answer, now);
        }
        self.mark(&choices, &fingerprint, now);
        Ok(())
    }

    /// Looks up every current choice now (Check).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the account list cannot be read.
    pub(crate) async fn recheck(&self, host: &HostContext) -> Result<(), PluginError> {
        let choices = account_choices(host).await?;
        let now = host.clock().now();
        for choice in &choices {
            let answer = fetch_trace(host, choice, &self.trace_target(), TIMEOUT).await;
            self.note(choice, &answer, now);
        }
        self.mark(&choices, &fingerprint(&choices), now);
        Ok(())
    }

    /// Holds `choice` when the operator asked and the last reading is unsupported.
    ///
    /// An unknown reading holds nothing. A choice never looked up is allowed
    /// and kicked so the next pass looks it up. Any reading older than
    /// [`FRESH`] is looked up again first, within [`HOLD_TIMEOUT`], and every
    /// request that arrives during that lookup shares it.
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
                let reading = self.refresh(host, choice, now).await;
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
        let reading = self.stored(choice);
        let Some(at) = reading.at else {
            return Step::Kick;
        };
        if age(now, at) > FRESH {
            return Step::Refresh;
        }
        if reading.state == State::Unsupported {
            Step::Deny
        } else {
            Step::Allow
        }
    }

    fn stored(&self, choice: &str) -> Reading {
        let inner = self.lock();
        inner
            .rows
            .get(choice)
            .map(|stored| project(stored, &inner.ways))
            .unwrap_or_else(|| unseen(choice))
    }

    fn trace_target(&self) -> String {
        self.trace_url
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn refresh(&self, host: &HostContext, choice: &str, now: SystemTime) -> Reading {
        let choice = choice.trim().to_owned();
        let (flight, leader) = {
            let mut flights = self
                .flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = flights.get(&choice) {
                (Arc::clone(existing), false)
            } else {
                let flight = Arc::new(Flight::new());
                flights.insert(choice.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };
        if !leader {
            flight.followers.fetch_add(1, Ordering::SeqCst);
            let _ = hofund_plugin_sdk::timeout(HOLD_TIMEOUT, flight.wait()).await;
            return self.stored(&choice);
        }
        let mut guard = LeaderGuard {
            egress: self,
            choice: choice.clone(),
            flight: Arc::clone(&flight),
            armed: true,
        };
        let answer = match hofund_plugin_sdk::timeout(
            HOLD_TIMEOUT,
            fetch_trace(host, &choice, &self.trace_target(), TIMEOUT),
        )
        .await
        {
            Some(answer) => answer,
            None => TraceAnswer::failed("the lookup timed out"),
        };
        let reading = self.note(&choice, &answer, now);
        self.flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&choice);
        flight.finish(reading.clone());
        guard.armed = false;
        reading
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

fn worst(choices: &[String], rows: &BTreeMap<String, Stored>, ways: &[String]) -> Reading {
    let mut unknown = None;
    let mut supported = None;
    for choice in choices {
        let reading = rows
            .get(choice)
            .map(|stored| project(stored, ways))
            .unwrap_or_else(|| unseen(choice));
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
        ip: String::new(),
        mine: false,
        via: shown(choice),
        at: None,
        error: String::new(),
    }
}

fn store_answer(answer: &TraceAnswer, now: SystemTime) -> Stored {
    if !answer.error.is_empty() || answer.status != 200 {
        let error = if answer.error.is_empty() {
            format!("HTTP {}", answer.status)
        } else {
            answer.error.clone()
        };
        return Stored {
            loc: String::new(),
            ip: String::new(),
            via: String::new(),
            at: Some(now),
            error,
        };
    }
    let (loc, ip) = trace_facts(&answer.body);
    Stored {
        loc,
        ip,
        via: String::new(),
        at: Some(now),
        error: String::new(),
    }
}

fn project(stored: &Stored, ways: &[String]) -> Reading {
    if stored.loc.is_empty() && stored.ip.is_empty() {
        let error = if stored.error.is_empty() {
            "no region in the answer".to_owned()
        } else {
            stored.error.clone()
        };
        return Reading {
            state: State::Unknown,
            loc: String::new(),
            ip: String::new(),
            mine: false,
            via: stored.via.clone(),
            at: stored.at,
            error,
        };
    }
    let mut reading = judge(&stored.loc, &stored.ip, stored.at, ways);
    reading.via.clone_from(&stored.via);
    reading
}

#[cfg(test)]
fn judge_loc(loc: &str, now: SystemTime) -> Reading {
    judge(loc, "", Some(now), &[])
}

fn judge(loc: &str, ip: &str, at: Option<SystemTime>, ways: &[String]) -> Reading {
    let valid_loc =
        loc.len() == 2 && loc.bytes().all(|byte| byte.is_ascii_uppercase()) && loc != "XX";
    if ways.is_empty() {
        if !valid_loc {
            return Reading {
                state: State::Unknown,
                loc: loc.to_owned(),
                ip: ip.to_owned(),
                mine: false,
                via: String::new(),
                at,
                error: "no region in the answer".to_owned(),
            };
        }
        let state = if supported_loc(loc) {
            State::Supported
        } else {
            State::Unsupported
        };
        return Reading {
            state,
            loc: loc.to_owned(),
            ip: ip.to_owned(),
            mine: false,
            via: String::new(),
            at,
            error: String::new(),
        };
    }
    let observed = canonical_ip(ip);
    let mut regions = false;
    let mut addresses = false;
    for way in ways {
        if way.len() == 2 {
            regions = true;
            if valid_loc && way == loc {
                return allowed(loc, ip, at);
            }
            continue;
        }
        addresses = true;
        let Some(observed) = observed.clone() else {
            continue;
        };
        if let Some(listed) = canonical_ip(way) {
            if listed == observed {
                return allowed(loc, ip, at);
            }
            continue;
        }
        if prefix_contains(way, &observed) {
            return allowed(loc, ip, at);
        }
    }
    if (regions && !valid_loc) || (addresses && observed.is_none()) {
        return Reading {
            state: State::Unknown,
            loc: loc.to_owned(),
            ip: ip.to_owned(),
            mine: true,
            via: String::new(),
            at,
            error: "no region or address in the answer".to_owned(),
        };
    }
    Reading {
        state: State::Unsupported,
        loc: loc.to_owned(),
        ip: ip.to_owned(),
        mine: true,
        via: String::new(),
        at,
        error: String::new(),
    }
}

fn allowed(loc: &str, ip: &str, at: Option<SystemTime>) -> Reading {
    Reading {
        state: State::Supported,
        loc: loc.to_owned(),
        ip: ip.to_owned(),
        mine: true,
        via: String::new(),
        at,
        error: String::new(),
    }
}

fn trace_facts(body: &str) -> (String, String) {
    let body = body.get(..body.len().min(16 << 10)).unwrap_or(body);
    let mut loc = String::new();
    let mut ip = String::new();
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "loc" => loc = value.trim().to_ascii_uppercase(),
            "ip" => {
                if let Some(parsed) = canonical_ip(value) {
                    ip = parsed;
                }
            }
            _ => {}
        }
    }
    (loc, ip)
}

fn canonical_ip(text: &str) -> Option<String> {
    hofund_plugin_sdk::address::canonical_ip(text)
}

fn prefix_contains(text: &str, address: &str) -> bool {
    hofund_plugin_sdk::address::prefix_contains(text, address)
}

/// Keeps region codes, addresses, and CIDR ranges. Anything else is refused.
///
/// A two-letter code is stored in capitals. Blanks and repeats are dropped.
pub(crate) fn parse_ways(values: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for raw in values {
        let way = raw.trim();
        if way.is_empty() {
            continue;
        }
        let way = if way.len() == 2 {
            way.to_ascii_uppercase()
        } else {
            way.to_owned()
        };
        if !way_out_ok(&way) {
            return Err(format!(
                "a way out must be a region code (US), an IP or a CIDR range, not {way:?}"
            ));
        }
        if !out.contains(&way) {
            out.push(way);
        }
    }
    Ok(out)
}

fn way_out_ok(way: &str) -> bool {
    if way.len() == 2 && way.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return true;
    }
    hofund_plugin_sdk::address::is_ip_or_prefix(way)
}

fn place_line(reading: &Reading) -> String {
    match (reading.ip.is_empty(), reading.loc.is_empty()) {
        (false, false) => format!("{} · {}", reading.ip, reading.loc),
        (false, true) => reading.ip.clone(),
        (true, false) => reading.loc.clone(),
        (true, true) => String::new(),
    }
}

fn warning_where(reading: &Reading) -> String {
    if !reading.ip.is_empty() && !reading.loc.is_empty() {
        format!("{} ({})", reading.ip, reading.loc)
    } else if !reading.ip.is_empty() {
        reading.ip.clone()
    } else {
        reading.loc.clone()
    }
}

fn warning_text(reading: &Reading, hold: bool) -> String {
    let mut text = if reading.mine {
        let where_at = warning_where(reading);
        format!(
            "Claude's way out to the internet is {where_at}, outside the ways out allowed for Claude. {where_at} is not one of the ways out allowed for Claude. A proxy or VPN that turned off, or a node switched, can cause this."
        )
    } else {
        format!(
            "Claude's way out to the internet is in {}, outside Anthropic's supported regions. {} is not on Anthropic's list of supported regions. A proxy or VPN that turned off, or a node in another country, can cause this.",
            reading.loc, reading.loc
        )
    };
    if hold {
        text.push_str(" Claude requests are held until it changes.");
    }
    text
}

fn held_at(reading: &Reading) -> String {
    if reading.ip.is_empty() {
        format!("in region {}", reading.loc)
    } else if reading.loc.is_empty() {
        reading.ip.clone()
    } else {
        format!("{} in region {}", reading.ip, reading.loc)
    }
}

fn held_error(reading: &Reading) -> PluginError {
    let via = match reading.via.as_str() {
        "" => "as the process proxy sends it".to_owned(),
        "direct" => "going direct".to_owned(),
        other => format!("through {other}"),
    };
    let at = held_at(reading);
    let message = if reading.mine {
        format!(
            "{HELD_WORDS}: this computer's way out to the internet, {via}, is {at}, which is not one of the ways out allowed for Claude. Switch the proxy or VPN back to one of them, or change allowed-ways-out under the subscription options"
        )
    } else {
        format!(
            "{HELD_WORDS}: this computer's way out to the internet, {via}, is {at}, which is not on Anthropic's list of supported regions. Turn the proxy or VPN back on, or set region-hold to false under the subscription options"
        )
    };
    PluginError::status_error(502, message).without_cooldown()
}

fn shown(choice: &str) -> String {
    choice_label(choice)
}

fn redact_url(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    if value.contains("://") || value.contains('@') {
        if let Ok(parsed) = hofund_plugin_sdk::netproxy::parse_proxy(value)
            && !userinfo_was_lost(value, parsed.host_str())
        {
            return host_url(parsed.scheme(), parsed.host_str(), parsed.port());
        }
        return fail_closed_host(value);
    }
    if value.contains('/') {
        return "unparsed-proxy".to_owned();
    }
    value.to_owned()
}

/// `parse_proxy` can treat a slash inside userinfo as the path and drop the
/// real host. That parse is not safe to display.
fn userinfo_was_lost(value: &str, parsed_host: Option<&str>) -> bool {
    let Some(rest) = value.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let Some(at) = rest.rfind('@') else {
        return false;
    };
    let host = host_portion(&rest[at + 1..]);
    parsed_host != Some(host.split(':').next().unwrap_or(host))
}

fn host_url(scheme: &str, host: Option<&str>, port: Option<u16>) -> String {
    let Some(host) = host else {
        return "unparsed-proxy".to_owned();
    };
    let port = port.map(|port| format!(":{port}")).unwrap_or_default();
    format!("{scheme}://{host}{port}")
}

fn fail_closed_host(value: &str) -> String {
    let (scheme, rest) = value
        .split_once("://")
        .map(|(scheme, rest)| (Some(scheme), rest))
        .unwrap_or((None, value));
    if let Some(at) = rest.rfind('@') {
        let host = host_portion(&rest[at + 1..]);
        if host_ok(host) {
            return match scheme {
                Some(scheme) => format!("{scheme}://{host}"),
                None => host.to_owned(),
            };
        }
    }
    "unparsed-proxy".to_owned()
}

fn host_portion(value: &str) -> &str {
    value.split(['/', '?', ' ', '#']).next().unwrap_or("")
}

fn host_ok(host: &str) -> bool {
    !host.is_empty() && !host.contains(' ') && !host.contains('/') && !host.contains('@')
}

/// External transport text is not forwarded because URL token boundaries cannot make it safe.
pub(crate) fn redact_error(_text: &str, choice: &str) -> String {
    let choice = choice_label(choice);
    if choice.is_empty() {
        "way-out lookup failed using the process proxy".to_owned()
    } else {
        format!("way-out lookup failed through {choice}")
    }
}

fn age(now: SystemTime, earlier: SystemTime) -> Duration {
    now.duration_since(earlier).unwrap_or_default()
}

/// Addresses, the process proxy, and proxy choices.
///
/// A change of any of them makes the next pass look up. Interface addresses
/// come from `netproxy::interface_addrs` (Linux, macOS,
/// and Windows). The process proxy is the effective `HTTPS_PROXY`,
/// `HTTP_PROXY`, `ALL_PROXY`, and `NO_PROXY` value, each taking the uppercase
/// name before the lowercase one. Values are redacted before they are stored.
#[must_use]
pub(crate) fn fingerprint(choices: &[String]) -> String {
    let owned = capture_proxy_env();
    let env: Vec<(&str, &str)> = owned
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    fingerprint_from(
        choices,
        &env,
        &hofund_plugin_sdk::netproxy::interface_addrs(),
    )
}

/// Fingerprint from an injected proxy environment and address list.
///
/// Tests pass dummy values. This does not read the process environment.
#[must_use]
pub(crate) fn fingerprint_from(
    choices: &[String],
    env: &[(&str, &str)],
    addrs: &[String],
) -> String {
    let mut addresses = addrs.to_vec();
    addresses.sort();
    addresses.dedup();
    let mut out = addresses.join(",");
    for (label, keys) in [
        ("https", &["HTTPS_PROXY", "https_proxy"][..]),
        ("http", &["HTTP_PROXY", "http_proxy"][..]),
        ("all", &["ALL_PROXY", "all_proxy"][..]),
        ("no", &["NO_PROXY", "no_proxy"][..]),
    ] {
        out.push('|');
        out.push_str(label);
        out.push('=');
        out.push_str(&redact_proxy_env(env_slot(env, keys)));
    }
    for choice in choices {
        out.push('|');
        out.push_str(&choice_label(choice));
    }
    out
}

fn capture_proxy_env() -> Vec<(String, String)> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ]
    .into_iter()
    .filter_map(|key| {
        let value = std::env::var(key).ok()?;
        if value.is_empty() {
            None
        } else {
            Some((key.to_owned(), value))
        }
    })
    .collect()
}

/// First non-empty value whose name equals a key, in key order.
///
/// Uppercase is listed before lowercase, so it wins when both are set even
/// if the lowercase entry appears earlier in `env`.
fn env_slot<'a>(env: &'a [(&'a str, &'a str)], keys: &[&str]) -> &'a str {
    for key in keys {
        if let Some((_, value)) = env
            .iter()
            .find(|(name, value)| *name == *key && !value.is_empty())
        {
            return value;
        }
    }
    ""
}

fn redact_proxy_env(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    if value.contains("://") || value.contains('@') || value.contains('/') {
        return redact_url(value);
    }
    value.to_owned()
}

fn choice_label(choice: &str) -> String {
    let choice = choice.trim();
    if choice.is_empty() {
        return String::new();
    }
    if choice.eq_ignore_ascii_case("direct") {
        return "direct".to_owned();
    }
    redact_url(choice)
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
/// `direct` uses no proxy. An empty choice uses the process proxy. A SOCKS
/// choice uses the same loopback HTTP bridge URL the process path uses.
/// Any other choice is that proxy URL. The proxy header is not forwarded.
pub(crate) async fn fetch_trace(
    host: &HostContext,
    choice: &str,
    url: &str,
    within: Duration,
) -> TraceAnswer {
    let mut call = Call::get(url).read_timeout(within);
    let trimmed = choice.trim();
    if trimmed.eq_ignore_ascii_case("direct") {
        call = call.header(OUTBOUND_PROXY, "direct");
    } else if !trimmed.is_empty() {
        let proxy = match hofund_plugin_sdk::netproxy::for_bun(trimmed) {
            Ok(value) => value,
            Err(error) => return TraceAnswer::failed(redact_error(&error.to_string(), trimmed)),
        };
        call = call.sensitive_header(OUTBOUND_PROXY, proxy);
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
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime};

    use hofund_plugin_api::{AccountId, PluginId, PluginKind, ProtocolId, UpstreamProvider};
    use hofund_plugin_sdk::oauth::SecretObject;

    use super::*;
    use crate::regions::{SUPPORTED_CODES, TERRITORY_CODES, supported_name, territory_of};

    type Held = Pin<Box<dyn Future<Output = Result<(), hofund_plugin_api::PluginError>>>>;

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
                TIMEOUT,
            )
            .await;
            assert_eq!(answer.status, 200, "{answer:?}");
            let reading = project(&store_answer(&answer, epoch()), &[]);
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
        let addrs = ["203.0.113.10".to_owned()];
        let one = fingerprint_from(&["direct".to_owned()], &[], &addrs);
        let two = fingerprint_from(&["direct".to_owned()], &[], &addrs);
        assert_eq!(one, two);
        assert!(one.ends_with("|direct"));
        assert_ne!(one, fingerprint_from(&[String::new()], &[], &addrs));
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

    #[test]
    fn d245_socks_trace_dials_the_sdk_bridge_with_fake_auth() {
        hofund_kit_subscription::block_on(async {
            let socks = hofund_kit_subscription::FirstPacket::start().await;
            let origin = hofund_kit_subscription::Http1Server::start(vec![
                hofund_kit_subscription::Http1Response::new(200).body(b"loc=CN\n".to_vec()),
            ])
            .await;
            let password = "d245-fake-secret";
            let choice = format!("socks5://alice:{password}@127.0.0.1:{}", socks.port());
            let bridged = hofund_plugin_sdk::netproxy::for_bun(&choice).expect("bridge url");
            assert!(
                bridged.starts_with("127.0.0.1:"),
                "bridge url is the loopback HTTP listener"
            );
            assert!(!bridged.to_ascii_lowercase().contains("socks"));
            assert!(!bridged.contains(password));
            let dir = tempfile::tempdir().expect("dir");
            let host =
                hofund_kit_subscription::TestHost::open_for_with_pooled_http_and_tokio_monotonic(
                    PluginKind::Subscription,
                    PluginId::from_static("claude"),
                    dir.path(),
                );
            let answer = fetch_trace(
                host.context(),
                &choice,
                &format!("{}/cdn-cgi/trace", origin.base_url()),
                TIMEOUT,
            )
            .await;
            let packet = socks.packet();
            assert!(
                !packet
                    .windows(password.len())
                    .any(|window| window == password.as_bytes()),
                "fake password must stay off the recorded dial"
            );
            assert!(
                !answer.error.contains(password),
                "error text must not contain the fake password"
            );
            assert!(
                !answer.body.contains(password),
                "response body must not contain the fake password"
            );
            assert_eq!(
                packet.first().copied(),
                Some(5),
                "the dial must be a SOCKS greeting from the HTTP bridge, not a raw socks URL"
            );
            assert!(
                packet.len() >= 4 && packet[2] == 0 && packet[3] == 2,
                "fake user and password must be offered as a SOCKS method"
            );
            assert!(
                origin.requests().is_empty(),
                "the trace host must not be dialed directly"
            );
            assert!(!answer.error.contains(password));
            assert!(!answer.error.to_ascii_lowercase().contains("socks5://"));
        });
    }

    #[test]
    fn d245_env_or_address_change_is_due_before_fifteen_minutes() {
        let choices = vec!["direct".to_owned()];
        let start = epoch() + Duration::from_secs(100);
        let base = fingerprint_from(&choices, &[], &["203.0.113.10".to_owned()]);
        let other_addr = fingerprint_from(&choices, &[], &["203.0.113.11".to_owned()]);
        let lower = fingerprint_from(
            &choices,
            &[("https_proxy", "http://10.9.9.9:8080")],
            &["203.0.113.10".to_owned()],
        );
        let upper = fingerprint_from(
            &choices,
            &[("HTTPS_PROXY", "http://10.9.9.8:8080")],
            &["203.0.113.10".to_owned()],
        );
        let same_either_case = fingerprint_from(
            &choices,
            &[("https_proxy", "http://10.9.9.9:8080")],
            &["203.0.113.10".to_owned()],
        );
        let upper_same = fingerprint_from(
            &choices,
            &[("HTTPS_PROXY", "http://10.9.9.9:8080")],
            &["203.0.113.10".to_owned()],
        );
        let secret = fingerprint_from(
            &choices,
            &[("https_proxy", "http://bob:d245-fake/secret@10.9.9.9:8080")],
            &["203.0.113.10".to_owned()],
        );
        let http_slot = fingerprint_from(
            &choices,
            &[("HTTP_PROXY", "http://10.8.8.8:8080")],
            &["203.0.113.10".to_owned()],
        );
        let all_slot = fingerprint_from(
            &choices,
            &[("all_proxy", "http://10.7.7.7:8080")],
            &["203.0.113.10".to_owned()],
        );
        let no_slot = fingerprint_from(
            &choices,
            &[("NO_PROXY", "localhost,127.0.0.1")],
            &["203.0.113.10".to_owned()],
        );
        assert_ne!(base, other_addr, "address change");
        assert_ne!(base, lower, "lowercase proxy env");
        assert_ne!(base, upper, "uppercase proxy env");
        assert_ne!(base, http_slot, "HTTP_PROXY");
        assert_ne!(base, all_slot, "all_proxy");
        assert_ne!(base, no_slot, "NO_PROXY");
        assert_eq!(
            same_either_case, upper_same,
            "case of the name is one variable"
        );
        let both = fingerprint_from(
            &choices,
            &[
                ("https_proxy", "http://10.1.1.1:8080"),
                ("HTTPS_PROXY", "http://10.2.2.2:8080"),
            ],
            &["203.0.113.10".to_owned()],
        );
        let upper_wins = fingerprint_from(
            &choices,
            &[("HTTPS_PROXY", "http://10.2.2.2:8080")],
            &["203.0.113.10".to_owned()],
        );
        assert_eq!(both, upper_wins, "uppercase proxy name wins");
        assert!(!both.contains("10.1.1.1"));
        assert!(
            !secret.contains("d245-fake"),
            "fingerprint stores no password"
        );
        assert_ne!(base, secret);
        let egress = Egress::new(true);
        egress.mark(&choices, &base, start);
        let soon = start + Duration::from_secs(20);
        assert!(!egress.due(&choices, &base, soon));
        assert!(egress.due(&choices, &other_addr, soon));
        assert!(egress.due(&choices, &lower, soon));
    }

    #[test]
    fn d245_malformed_userinfo_never_echoes_a_password() {
        let password = "d245-fake/secret word";
        let raw = format!("http://bob:{password}@10.0.0.1:8080");
        let via = shown(&raw);
        assert!(!via.contains(password), "shown");
        assert!(!via.contains("d245-fake"), "shown fragment");
        let err = redact_error(&format!("dial {raw} refused"), &raw);
        assert!(!err.contains(password), "redact_error");
        assert!(!err.contains("d245-fake"), "redact_error fragment");
        let reading = Reading {
            state: State::Unsupported,
            loc: "CN".to_owned(),
            ip: String::new(),
            mine: false,
            via,
            at: Some(epoch()),
            error: String::new(),
        };
        let held = held_error(&reading);
        assert!(!held.message().contains(password), "held");
        assert!(!held.message().contains("d245-fake"), "held fragment");
        let broken = format!("not a url {password}");
        assert!(!redact_url(&broken).contains(password));
        assert!(!redact_error(&broken, &broken).contains("d245-fake"));
    }

    #[test]
    fn d245_spaced_userinfo_differs_from_the_stored_choice() {
        let tail = "d245-fake-tail";
        let choice = format!("socks5://alice:prefix {tail}@10.0.0.1:1080");
        let observed = format!("connect to http://alice:prefix {tail}@10.0.0.1:1080 failed");
        let out = redact_error(&observed, &choice);
        assert!(
            !out.contains(tail),
            "redact_error left a userinfo tail from a url that is not the stored choice"
        );
        assert!(
            !out.contains("prefix"),
            "redact_error left userinfo from a url that is not the stored choice"
        );
        assert!(
            out.contains("10.0.0.1"),
            "the host of a split userinfo url stays visible"
        );
    }

    #[test]
    fn d245_external_errors_and_held_text_never_expose_split_userinfo() {
        let cases = [
            (
                "numeric first piece",
                "socks5://alice:1234 d245-numeric-tail@10.0.0.1:1080",
                "http://alice:1234 d245-numeric-tail@10.0.0.1:1080",
                vec!["1234", "d245-numeric-tail"],
            ),
            (
                "more than eight pieces",
                "socks5h://alice:prefix d245-long-01 d245-long-02 d245-long-03 d245-long-04 d245-long-05 d245-long-06 d245-long-07 d245-long-08 d245-long-09 d245-long-10@10.0.0.1:1080",
                "http://alice:prefix d245-long-01 d245-long-02 d245-long-03 d245-long-04 d245-long-05 d245-long-06 d245-long-07 d245-long-08 d245-long-09 d245-long-10@10.0.0.1:1080",
                vec![
                    "prefix",
                    "d245-long-01",
                    "d245-long-02",
                    "d245-long-03",
                    "d245-long-04",
                    "d245-long-05",
                    "d245-long-06",
                    "d245-long-07",
                    "d245-long-08",
                    "d245-long-09",
                    "d245-long-10",
                ],
            ),
            (
                "tab separator",
                "socks5://alice:prefix\td245-tab-tail@10.0.0.1:1080",
                "http://alice:prefix\td245-tab-tail@10.0.0.1:1080",
                vec!["prefix", "d245-tab-tail"],
            ),
            (
                "newline and Unicode separators",
                "socks5://alice:prefix\nd245-line-tail\u{2003}d245-unicode-tail@10.0.0.1:1080",
                "http://alice:prefix\nd245-line-tail\u{2003}d245-unicode-tail@10.0.0.1:1080",
                vec!["prefix", "d245-line-tail", "d245-unicode-tail"],
            ),
            (
                "scheme variant",
                "socks5://alice:prefix d245-scheme-tail@10.0.0.1:1080",
                "http://alice:prefix d245-scheme-tail@10.0.0.1:1080",
                vec!["prefix", "d245-scheme-tail"],
            ),
        ];
        let mut failures = Vec::new();
        for (name, choice, observed, secrets) in cases {
            let error = redact_error(&format!("connect to {observed} failed"), choice);
            let reading = Reading {
                state: State::Unsupported,
                loc: "CN".to_owned(),
                ip: String::new(),
                mine: false,
                via: shown(choice),
                at: Some(epoch()),
                error,
            };
            let visible = format!("{} {}", reading.error, held_error(&reading).message());
            if secrets.iter().any(|secret| visible.contains(secret)) {
                failures.push(name);
            }
        }
        assert!(
            failures.is_empty(),
            "credential material remained visible in cases: {failures:?}"
        );
    }

    #[test]
    fn d245_last_trace_loc_wins() {
        let (loc, ip) = trace_facts("ip=198.51.100.1\nloc=US\nip=203.0.113.9\nloc=CN\n");
        assert_eq!(loc, "CN");
        assert_eq!(ip, "203.0.113.9");
    }

    #[test]
    fn d245_direct_any_case_is_going_direct() {
        let reading = Reading {
            state: State::Unsupported,
            loc: "CN".to_owned(),
            ip: String::new(),
            mine: false,
            via: shown("Direct"),
            at: Some(epoch()),
            error: String::new(),
        };
        let message = held_error(&reading).message().to_owned();
        assert!(
            message.contains("going direct"),
            "held text should say going direct"
        );
        assert!(!message.contains("through Direct"));
    }

    fn text_request() -> hofund_plugin_api::Request {
        let mut message = hofund_plugin_api::Message::new(hofund_plugin_api::Role::User);
        message.content = vec![hofund_plugin_api::ContentPart::text("Hello")];
        hofund_plugin_api::Request::new("claude-sonnet-4-5").with_messages(vec![message])
    }

    #[test]
    fn d279_hold_looks_again_when_a_supported_reading_is_older_than_thirty_seconds() {
        let now = epoch() + Duration::from_secs(100);
        let egress = Egress::new(true);
        egress.note("", &TraceAnswer::loc("US"), now - Duration::from_secs(31));
        assert_eq!(
            egress.step("", now),
            Step::Refresh,
            "a held request looks up any reading older than 30 seconds"
        );
    }

    #[test]
    fn d279_held_error_names_the_trace_address() {
        let egress = Egress::new(true);
        let answer = TraceAnswer {
            status: 200,
            body: "ip=203.0.113.9\nloc=CN\n".to_owned(),
            error: String::new(),
        };
        egress.note("", &answer, epoch());
        let text = held_error(&egress.stored("")).message().to_owned();
        assert!(
            text.contains("203.0.113.9"),
            "the held error should name the address beside the region, got {text}"
        );
    }

    #[test]
    fn d279_allowed_ways_out_is_a_subscription_option() {
        use hofund_plugin_api::{RawTable, RawValue, SubscriptionFactory};

        let mut raw = RawTable::new();
        raw.insert(
            "allowed-ways-out",
            RawValue::Array(vec![RawValue::String("US".to_owned())]),
        );
        let config = crate::ClaudeFactory
            .validate(&raw)
            .expect("a region list is a valid subscription option");
        assert!(!config.region_hold);
        assert_eq!(config.allowed_ways_out, ["US".to_owned()]);
    }

    #[test]
    fn d279_parse_ways_keeps_regions_addresses_and_rejects_the_rest() {
        let kept = parse_ways(&[
            " us".to_owned(),
            "JP".to_owned(),
            "203.0.113.9".to_owned(),
            "2001:db8::/32".to_owned(),
            String::new(),
            "US".to_owned(),
        ])
        .expect("ways");
        assert_eq!(
            kept,
            [
                "US".to_owned(),
                "JP".to_owned(),
                "203.0.113.9".to_owned(),
                "2001:db8::/32".to_owned(),
            ]
        );
        for bad in [
            "USA",
            "United States",
            "203.0.113.300",
            "203.0.113.0/33",
            "203.0.113.1/24",
            "U1",
        ] {
            assert!(parse_ways(&[bad.to_owned()]).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn d279_the_users_list_alone_judges_regions_addresses_and_ranges() {
        let now = Some(epoch());
        let cases = [
            (vec!["US"], "US", "203.0.113.9", State::Supported),
            (vec!["JP"], "JP", "", State::Supported),
            (vec!["US"], "GB", "203.0.113.9", State::Unsupported),
            (vec!["HK"], "HK", "203.0.113.9", State::Supported),
            (vec!["203.0.113.9"], "CN", "203.0.113.9", State::Supported),
            (
                vec!["203.0.113.0/24"],
                "XX",
                "203.0.113.200",
                State::Supported,
            ),
            (vec!["2001:db8::/32"], "US", "2001:db8::1", State::Supported),
            (
                vec!["203.0.113.9"],
                "US",
                "198.51.100.1",
                State::Unsupported,
            ),
            (
                vec!["US", "203.0.113.9"],
                "US",
                "198.51.100.1",
                State::Supported,
            ),
            (
                vec!["US", "203.0.113.9"],
                "JP",
                "203.0.113.9",
                State::Supported,
            ),
            (
                vec!["US", "203.0.113.9"],
                "JP",
                "198.51.100.1",
                State::Unsupported,
            ),
            (vec!["US"], "XX", "203.0.113.9", State::Unknown),
            (vec!["203.0.113.9"], "US", "", State::Unknown),
            (vec!["US"], "JP", "", State::Unsupported),
            (
                vec!["203.0.113.0/24"],
                "",
                "198.51.100.1",
                State::Unsupported,
            ),
        ];
        for (ways, loc, ip, want) in cases {
            let owned: Vec<String> = ways.iter().map(|way| (*way).to_owned()).collect();
            let reading = judge(loc, ip, now, &owned);
            assert_eq!(reading.state, want, "ways {ways:?} loc {loc} ip {ip}");
            assert!(reading.mine, "ways {ways:?}");
        }
    }

    #[test]
    fn d279_a_changed_list_is_applied_at_the_next_read() {
        let egress = Egress::new(false);
        let now = epoch();
        egress.note("", &trace_body("ip=203.0.113.9\nloc=US\n"), now);
        egress.mark(&[String::new()], "fp", now);
        assert_eq!(egress.stored("").state, State::Supported);
        assert!(!egress.stored("").mine);
        egress.set_ways(parse_ways(&["JP".to_owned()]).expect("ways"));
        let reading = egress.stored("");
        assert_eq!(reading.state, State::Unsupported);
        assert!(reading.mine);
        assert_eq!(reading.ip, "203.0.113.9");
        let warning = egress.warning().expect("warning");
        assert!(warning.contains("203.0.113.9 (US)"), "{warning}");
        assert!(
            !warning.contains("Anthropic"),
            "the user's list must not name the supported-region list: {warning}"
        );
        egress.set_ways(Vec::new());
        let cleared = egress.stored("");
        assert_eq!(cleared.state, State::Supported);
        assert!(!cleared.mine);
        assert!(egress.warning().is_none());
    }

    #[test]
    fn d279_place_shows_the_address_and_region() {
        let egress = Egress::new(false);
        assert!(egress.place().is_none());
        egress.mark(&[String::new()], "fp", epoch());
        assert_eq!(egress.place().as_deref(), Some("Not looked up yet"));
        egress.note("", &trace_body("ip=203.0.113.9\nloc=US\n"), epoch());
        assert_eq!(egress.place().as_deref(), Some("203.0.113.9 · US"));
    }

    #[test]
    fn d279_an_answer_with_no_region_keeps_that_error() {
        let egress = Egress::new(false);
        egress.note("", &trace_body("fl=1\n"), epoch());
        assert_eq!(egress.stored("").error, "no region in the answer");
        egress.note("", &failed_trace(), epoch());
        assert_eq!(egress.stored("").error, "connection refused");
    }

    #[test]
    fn d279_dropping_the_leader_starts_a_fresh_lookup() {
        hofund_kit_subscription::block_on_paused(async {
            let (gate, gate_rx) = hofund_kit_subscription::header_park();
            let server = hofund_kit_subscription::Http1Server::start(vec![
                hofund_kit_subscription::Http1Response::new(200)
                    .body(b"ip=198.51.100.1\nloc=CN\n".to_vec())
                    .park_headers(gate_rx),
                hofund_kit_subscription::Http1Response::new(200)
                    .body(b"ip=203.0.113.9\nloc=JP\n".to_vec()),
            ])
            .await;
            let dir = tempfile::tempdir().expect("dir");
            let host =
                hofund_kit_subscription::TestHost::open_for_with_pooled_http_and_tokio_monotonic(
                    PluginKind::Subscription,
                    PluginId::from_static("claude"),
                    dir.path(),
                );
            let egress = Egress::new(true);
            egress.set_trace_url(&format!("{}/cdn-cgi/trace", server.base_url()));
            let now = host.context().clock().now();
            egress.note("", &TraceAnswer::loc("US"), now);
            hofund_kit_subscription::advance_time(Duration::from_secs(31)).await;
            let context = host.context().clone();
            let mut leader = Box::pin(egress.hold(&context, ""));
            let mut started = false;
            for _ in 0..80 {
                match futures_util::future::select(
                    leader,
                    Box::pin(hofund_kit_subscription::yield_times(1)),
                )
                .await
                {
                    futures_util::future::Either::Left((result, _)) => {
                        panic!("the leader finished while the trace was parked: {result:?}");
                    }
                    futures_util::future::Either::Right((_, pending)) => {
                        leader = pending;
                        if server.requests().len() == 1 {
                            started = true;
                            break;
                        }
                    }
                }
            }
            assert!(started, "the leader reached the parked trace");
            drop(leader);
            let mut next = Box::pin(egress.hold(&context, ""));
            let mut done = None;
            for _ in 0..80 {
                match futures_util::future::select(
                    next,
                    Box::pin(hofund_kit_subscription::yield_times(1)),
                )
                .await
                {
                    futures_util::future::Either::Left((result, _)) => {
                        done = Some(result);
                        break;
                    }
                    futures_util::future::Either::Right((_, pending)) => {
                        next = pending;
                    }
                }
            }
            let result = done.expect("the next hold finished");
            result.expect("a fresh allowed reading is not held");
            assert_eq!(
                server.requests().len(),
                2,
                "the next hold starts a new lookup"
            );
            assert_eq!(egress.stored("").loc, "JP");
            drop(gate);
        });
    }

    #[test]
    fn d279_hold_returns_when_five_seconds_elapse() {
        hofund_kit_subscription::block_on_paused(async {
            let (gate, gate_rx) = hofund_kit_subscription::header_park();
            let server = hofund_kit_subscription::Http1Server::start(vec![
                hofund_kit_subscription::Http1Response::new(200)
                    .body(b"ip=203.0.113.9\nloc=US\n".to_vec())
                    .park_headers(gate_rx),
            ])
            .await;
            let dir = tempfile::tempdir().expect("dir");
            let host =
                hofund_kit_subscription::TestHost::open_for_with_pooled_http_and_tokio_monotonic(
                    PluginKind::Subscription,
                    PluginId::from_static("claude"),
                    dir.path(),
                );
            let egress = Egress::new(true);
            egress.set_trace_url(&format!("{}/cdn-cgi/trace", server.base_url()));
            let now = host.context().clock().now();
            egress.note("", &TraceAnswer::loc("US"), now);
            hofund_kit_subscription::advance_time(Duration::from_secs(31)).await;
            let context = host.context().clone();
            let mut hold = Box::pin(egress.hold(&context, ""));
            let mut started = false;
            for _ in 0..80 {
                match futures_util::future::select(
                    hold,
                    Box::pin(hofund_kit_subscription::yield_times(1)),
                )
                .await
                {
                    futures_util::future::Either::Left((result, _)) => {
                        panic!("hold returned before the deadline: {result:?}");
                    }
                    futures_util::future::Either::Right((_, pending)) => {
                        hold = pending;
                        if server.requests().len() == 1 {
                            started = true;
                            break;
                        }
                    }
                }
            }
            assert!(started, "the lookup reached the parked trace");
            let before = host.context().clock().now();
            let mut outcome = None;
            for _ in 0..2 {
                let step = async {
                    hofund_plugin_sdk::sleep(Duration::from_millis(5)).await;
                    hofund_kit_subscription::advance_time(Duration::from_secs(5)).await;
                };
                match futures_util::future::select(hold, Box::pin(step)).await {
                    futures_util::future::Either::Left((result, _)) => {
                        outcome = Some(result);
                        break;
                    }
                    futures_util::future::Either::Right((_, pending)) => hold = pending,
                }
            }
            let result = outcome.expect("hold did not return when five seconds elapsed");
            let elapsed = host
                .context()
                .clock()
                .now()
                .duration_since(before)
                .expect("clock moved forward");
            assert!(
                elapsed >= Duration::from_secs(5),
                "returned before five seconds ({elapsed:?})"
            );
            assert!(
                elapsed <= Duration::from_secs(5) + Duration::from_millis(50),
                "returned after {elapsed:?}"
            );
            result.expect("unknown holds nothing");
            let reading = egress.stored("");
            assert_eq!(reading.state, State::Unknown);
            assert_eq!(reading.error, "the lookup timed out");
            assert!(!reading.error.contains("http"));
            assert_eq!(server.requests().len(), 1);
            drop(gate);
        });
    }

    #[test]
    fn d279_hold_off_does_not_look_again() {
        let now = epoch() + Duration::from_secs(100);
        let egress = Egress::new(false);
        egress.note("", &TraceAnswer::loc("JP"), now - Duration::from_secs(31));
        assert_eq!(egress.step("", now), Step::Allow);
    }

    #[test]
    fn d279_a_mapped_address_matches_the_ipv4_entry() {
        let (loc, ip) = trace_facts("ip=::ffff:203.0.113.9\nloc=CN\n");
        assert_eq!(loc, "CN");
        assert_eq!(ip, "203.0.113.9");
        let reading = judge(&loc, &ip, Some(epoch()), &["203.0.113.9".to_owned()]);
        assert_eq!(reading.state, State::Supported);
        assert!(reading.mine);
    }

    #[test]
    fn d279_a_failed_lookup_stays_unknown_when_a_list_is_set() {
        let egress = Egress::new(true);
        egress.set_ways(vec!["US".to_owned()]);
        egress.note("", &failed_trace(), epoch());
        let reading = egress.stored("");
        assert_eq!(reading.state, State::Unknown);
        assert!(!reading.mine);
        assert_eq!(egress.step("", epoch()), Step::Allow);
    }

    #[test]
    fn d279_hold_looks_again_after_a_node_switch() {
        hofund_kit_subscription::block_on_paused(async {
            let server = hofund_kit_subscription::Http1Server::start(vec![
                hofund_kit_subscription::Http1Response::new(200)
                    .body(b"ip=203.0.113.9\nloc=JP\n".to_vec()),
            ])
            .await;
            let dir = tempfile::tempdir().expect("dir");
            let host =
                hofund_kit_subscription::TestHost::open_for_with_pooled_http_and_tokio_monotonic(
                    PluginKind::Subscription,
                    PluginId::from_static("claude"),
                    dir.path(),
                );
            let egress = Egress::new(true);
            egress.set_ways(vec!["US".to_owned()]);
            egress.set_trace_url(&format!("{}/cdn-cgi/trace", server.base_url()));
            let now = host.context().clock().now();
            egress.note("", &trace_body("ip=203.0.113.9\nloc=US\n"), now);
            egress
                .hold(host.context(), "")
                .await
                .expect("a fresh allowed reading is not held");
            assert!(
                server.requests().is_empty(),
                "a fresh reading is not looked up"
            );
            hofund_kit_subscription::advance_time(Duration::from_secs(31)).await;
            let error = egress
                .hold(host.context(), "")
                .await
                .expect_err("the switched node is held");
            let text = error.message();
            assert!(text.contains("203.0.113.9 in region JP"), "{text}");
            assert!(text.contains("allowed for Claude"), "{text}");
            assert!(!text.contains("Anthropic"), "{text}");
            assert!(!error.cools());
            assert_eq!(server.requests().len(), 1);
        });
    }

    #[test]
    fn d279_hold_looks_again_once_for_many() {
        hofund_kit_subscription::block_on_paused(async {
            let (gate, gate_rx) = hofund_kit_subscription::header_park();
            let server = hofund_kit_subscription::Http1Server::start(vec![
                hofund_kit_subscription::Http1Response::new(200)
                    .body(b"ip=203.0.113.9\nloc=CN\n".to_vec())
                    .park_headers(gate_rx),
            ])
            .await;
            let dir = tempfile::tempdir().expect("dir");
            let host =
                hofund_kit_subscription::TestHost::open_for_with_pooled_http_and_tokio_monotonic(
                    PluginKind::Subscription,
                    PluginId::from_static("claude"),
                    dir.path(),
                );
            let egress = Arc::new(Egress::new(true));
            egress.set_trace_url(&format!("{}/cdn-cgi/trace", server.base_url()));
            let now = host.context().clock().now();
            egress.note("", &TraceAnswer::loc("US"), now);
            hofund_kit_subscription::advance_time(Duration::from_secs(31)).await;
            let context = host.context().clone();
            let mut tasks: Vec<Held> = Vec::new();
            for _ in 0..8 {
                let egress = Arc::clone(&egress);
                let context = context.clone();
                tasks.push(Box::pin(async move { egress.hold(&context, "").await }));
            }
            let holds = hofund_kit_subscription::join_all(tasks);
            let release = async {
                let mut spins = 0u32;
                loop {
                    spins += 1;
                    assert!(
                        spins <= 400,
                        "the shared lookup did not start within 400 yields"
                    );
                    let seen = server.requests().len();
                    assert!(
                        seen <= 1,
                        "looked up {seen} times before the shared lookup finished"
                    );
                    if seen == 1 && egress.followers("") >= 7 {
                        gate.release();
                        return;
                    }
                    hofund_kit_subscription::yield_times(1).await;
                }
            };
            let (results, ()) = futures_util::future::join(holds, release).await;
            assert_eq!(server.requests().len(), 1);
            for result in results {
                let error = result.expect_err("held");
                assert!(error.message().contains("CN"), "{}", error.message());
            }
        });
    }

    fn trace_body(body: &str) -> TraceAnswer {
        TraceAnswer {
            status: 200,
            body: body.to_owned(),
            error: String::new(),
        }
    }
}

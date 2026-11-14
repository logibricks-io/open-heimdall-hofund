//! Cross-step dashboard usage debt and deferred usage (R-9k).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hofund_plugin_api::{HostContext, ProtocolId, SecretString, Usage};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::sleep;

use crate::connect;
use crate::endpoints::Endpoints;
use crate::run::Limits;
use crate::tokens;
use crate::usage::{self, DashboardTotals, PollArgs};

/// Raw dashboard counters before inbound mapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RawUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl From<DashboardTotals> for RawUsage {
    fn from(totals: DashboardTotals) -> Self {
        Self {
            input: totals.input,
            output: totals.output,
            cache_read: totals.cache_read,
            cache_write: totals.cache_write,
            reasoning: totals.reasoning,
        }
    }
}

impl From<RawUsage> for DashboardTotals {
    fn from(raw: RawUsage) -> Self {
        Self {
            input: raw.input,
            output: raw.output,
            cache_read: raw.cache_read,
            cache_write: raw.cache_write,
            reasoning: raw.reasoning,
        }
    }
}

impl RawUsage {
    fn is_empty(self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_write == 0
            && self.reasoning == 0
    }

    fn add_assign(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }
}

fn add_usage(left: Option<RawUsage>, right: Option<RawUsage>) -> Option<RawUsage> {
    match (left, right) {
        (None, None) => None,
        (Some(a), None) | (None, Some(a)) => Some(a),
        (Some(mut a), Some(b)) => {
            a.add_assign(b);
            Some(a)
        }
    }
}

/// Process-wide dashboard claim and conversation debt state.
#[derive(Clone, Debug, Default)]
pub(crate) struct UsageLedger {
    inner: Arc<Mutex<LedgerInner>>,
}

#[derive(Debug, Default)]
struct LedgerInner {
    claimed: HashMap<String, i64>,
    owed: HashMap<String, Debt>,
    misses: HashMap<String, u32>,
    unreadable: HashMap<String, i64>,
}

#[derive(Clone, Debug)]
struct Debt {
    since: i64,
    n: u32,
    ready_ms: i64,
    carry: Option<RawUsage>,
    before: Option<i64>,
}

#[derive(Clone, Debug)]
struct TimedUsage {
    at: i64,
    usage: RawUsage,
}

impl UsageLedger {
    pub(crate) fn book(
        self: &Arc<Self>,
        token: &SecretString,
        conversation_id: &str,
        since_ms: i64,
        kept: bool,
    ) -> UsageBook {
        UsageBook {
            ledger: Arc::clone(self),
            token_fingerprint: token_fingerprint(token),
            conversation_id: conversation_id.to_owned(),
            since_ms,
            kept,
            mine: None,
            theirs: None,
            finished: false,
        }
    }

    fn usage_key(tok: &str, conv: &str) -> String {
        format!("{tok}\0{conv}")
    }

    fn claim_key(debt_key: &str, timestamp: i64) -> String {
        format!("{debt_key}\0{timestamp}")
    }

    fn owe(&self, debt_key: &str, patch: OwePatch, limits: Limits) {
        let now = patch.since;
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        expire_debts(&mut inner, now, limits);
        if patch.n == 0 && patch.carry.is_none() {
            return;
        }
        match inner.owed.get_mut(debt_key) {
            None => {
                inner.owed.insert(
                    debt_key.to_owned(),
                    Debt {
                        since: patch.since,
                        n: patch.n,
                        ready_ms: patch.ready_ms,
                        carry: patch.carry,
                        before: patch.before,
                    },
                );
            }
            Some(debt) => {
                debt.since = debt.since.min(patch.since);
                debt.n = debt.n.saturating_add(patch.n);
                debt.ready_ms = debt.ready_ms.min(patch.ready_ms);
                debt.carry = add_usage(debt.carry, patch.carry);
            }
        }
    }

    fn can_read(&self, token_fingerprint: &str, limits: Limits, now_ms: i64) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(marked) = inner.unreadable.get(token_fingerprint).copied() else {
            return true;
        };
        let now = now_ms;
        if now.saturating_sub(marked) < millis(limits.usage_owed_for) {
            return false;
        }
        inner.unreadable.remove(token_fingerprint);
        true
    }

    fn mark_unreadable(&self, token_fingerprint: &str, now_ms: i64) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner
            .unreadable
            .insert(token_fingerprint.to_owned(), now_ms);
    }

    async fn usage_events(
        &self,
        args: &LedgerPoll<'_>,
        want: u32,
        polling: bool,
        first_wait: Duration,
        before_ms: i64,
    ) -> FetchedEvents {
        let debt_key = Self::usage_key(args.token_fingerprint, args.conversation_id);
        let started = args.host.clock().monotonic();
        let end = started.saturating_add(args.limits.usage_give_up);
        if polling {
            sleep(first_wait).await;
        }
        let mut events = Vec::new();
        let mut got = 0u32;
        let floor = args.since_ms.saturating_sub(1000);
        loop {
            let remaining = end.saturating_sub(args.host.clock().monotonic());
            if remaining.is_zero() {
                break;
            }
            let budget = args.limits.usage_request.min(remaining);
            let reply = match one_dashboard_fetch(args, budget).await {
                Ok(body) => body,
                Err(EventsError::Unreadable) => {
                    self.mark_unreadable(
                        args.token_fingerprint,
                        tokens::unix_ms(args.host.clock().now()),
                    );
                    return FetchedEvents {
                        refused: true,
                        ..FetchedEvents::default()
                    };
                }
                Err(EventsError::Transport) => break,
            };
            let now_ms = tokens::unix_ms(args.host.clock().now());
            let mut pending: Vec<TimedUsage> = Vec::new();
            {
                let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
                purge_stale_claims(&mut inner, args.limits, now_ms);
                for event in usage::events_of(&reply) {
                    if usage::conversation_of(event) != Some(args.conversation_id) {
                        continue;
                    }
                    let Some(timestamp) = usage::timestamp_of(event) else {
                        continue;
                    };
                    if timestamp < floor || timestamp >= before_ms {
                        continue;
                    }
                    if inner
                        .claimed
                        .contains_key(&Self::claim_key(&debt_key, timestamp))
                    {
                        continue;
                    }
                    pending.push(TimedUsage {
                        at: timestamp,
                        usage: raw_from_event(event),
                    });
                }
            }
            pending.sort_by_key(|event| event.at);
            for event in pending {
                if got >= want {
                    break;
                }
                {
                    let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
                    inner
                        .claimed
                        .insert(Self::claim_key(&debt_key, event.at), now_ms);
                }
                events.push(event);
                got += 1;
            }
            if got >= want {
                break;
            }
            if args.limits.usage_interval.is_zero() {
                break;
            }
            if !polling
                || args
                    .host
                    .clock()
                    .monotonic()
                    .saturating_add(args.limits.usage_interval)
                    > end
            {
                break;
            }
            sleep(args.limits.usage_interval).await;
        }
        let usage = events
            .iter()
            .fold(None, |sum, event| add_usage(sum, Some(event.usage)));
        FetchedEvents {
            usage,
            events,
            got,
            refused: false,
        }
    }

    async fn collect(
        &self,
        args: &LedgerPoll<'_>,
        polling: bool,
        first_wait: Duration,
        over: bool,
        limits: Limits,
    ) -> CollectResult {
        let debt_key = Self::usage_key(args.token_fingerprint, args.conversation_id);
        let debt = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            inner.owed.remove(&debt_key)
        };
        let Some(debt) = debt else {
            return CollectResult::default();
        };
        let want = debt.n;
        let since = debt.since;
        let before_ms = debt.before.unwrap_or(i64::MAX);
        let floor_args = LedgerPoll {
            host: args.host,
            endpoints: args.endpoints,
            token: args.token,
            token_fingerprint: args.token_fingerprint,
            conversation_id: args.conversation_id,
            team_id: args.team_id,
            since_ms: since,
            limits: args.limits,
        };
        let mut fetched = FetchedEvents::default();
        let now_ms = tokens::unix_ms(args.host.clock().now());
        if want > 0 && self.can_read(args.token_fingerprint, limits, now_ms) {
            fetched = self
                .usage_events(&floor_args, want, polling, first_wait, before_ms)
                .await;
        }
        let short = want.saturating_sub(fetched.got) > 0 && !fetched.refused;
        if short && self.can_read(args.token_fingerprint, limits, now_ms) {
            self.owe(
                &debt_key,
                OwePatch {
                    since,
                    n: want.saturating_sub(fetched.got),
                    ready_ms: debt.ready_ms,
                    carry: None,
                    before: debt.before,
                },
                limits,
            );
        }
        let carry = debt.carry;
        if over {
            if let Some(usage) = add_usage(carry, fetched.usage) {
                self.owe(
                    &debt_key,
                    OwePatch {
                        since: debt.since,
                        n: 0,
                        ready_ms: debt.ready_ms,
                        carry: Some(usage),
                        before: debt.before,
                    },
                    limits,
                );
            }
            return CollectResult::default();
        }
        CollectResult {
            carry,
            events: fetched.events,
            short,
        }
    }
}

#[derive(Default)]
struct CollectResult {
    carry: Option<RawUsage>,
    events: Vec<TimedUsage>,
    short: bool,
}

#[derive(Default)]
struct FetchedEvents {
    usage: Option<RawUsage>,
    events: Vec<TimedUsage>,
    got: u32,
    refused: bool,
}

enum EventsError {
    Unreadable,
    Transport,
}

struct OwePatch {
    since: i64,
    n: u32,
    ready_ms: i64,
    carry: Option<RawUsage>,
    before: Option<i64>,
}

struct LedgerPoll<'a> {
    host: &'a HostContext,
    endpoints: &'a Endpoints,
    token: &'a SecretString,
    token_fingerprint: &'a str,
    conversation_id: &'a str,
    team_id: Option<&'a str>,
    since_ms: i64,
    limits: Limits,
}

/// Per-Run dashboard accounting handle.
pub(crate) struct UsageBook {
    ledger: Arc<UsageLedger>,
    token_fingerprint: String,
    conversation_id: String,
    since_ms: i64,
    kept: bool,
    mine: Option<RawUsage>,
    theirs: Option<RawUsage>,
    finished: bool,
}

/// Result of [`UsageBook::own`].
pub(crate) enum StepOwn {
    Found(RawUsage),
    Owed,
}

impl UsageBook {
    fn debt_key(&self) -> String {
        UsageLedger::usage_key(&self.token_fingerprint, &self.conversation_id)
    }

    fn poll_args<'a>(
        &'a self,
        host: &'a HostContext,
        endpoints: &'a Endpoints,
        token: &'a SecretString,
        team_id: Option<&'a str>,
        limits: Limits,
    ) -> LedgerPoll<'a> {
        LedgerPoll {
            host,
            endpoints,
            token,
            token_fingerprint: &self.token_fingerprint,
            conversation_id: &self.conversation_id,
            team_id,
            since_ms: self.since_ms,
            limits,
        }
    }

    pub(crate) async fn own(
        &mut self,
        host: &HostContext,
        endpoints: &Endpoints,
        token: &SecretString,
        team_id: Option<&str>,
        limits: Limits,
    ) -> StepOwn {
        if !self.kept {
            return match usage::poll_dashboard(PollArgs {
                host,
                endpoints,
                token,
                conversation_id: &self.conversation_id,
                team_id,
                since_ms: self.since_ms,
                inbound: ProtocolId::OPENAI_CHAT,
                limits,
            })
            .await
            {
                Some(usage) => StepOwn::Found(raw_from_mapped_usage(&usage)),
                None => StepOwn::Owed,
            };
        }
        let now_ms = tokens::unix_ms(host.clock().now());
        if !self
            .ledger
            .can_read(&self.token_fingerprint, limits, now_ms)
        {
            return StepOwn::Owed;
        }
        let args = self.poll_args(host, endpoints, token, team_id, limits);
        self.ledger.owe(
            &self.debt_key(),
            OwePatch {
                since: self.since_ms,
                n: 1,
                ready_ms: tokens::unix_ms(host.clock().now())
                    .saturating_add(millis(limits.usage_owed_at)),
                carry: None,
                before: None,
            },
            limits,
        );
        let miss_key = self.token_fingerprint.clone();
        let misses = {
            let inner = self
                .ledger
                .inner
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            inner.misses.get(&miss_key).copied().unwrap_or(0)
        };
        let waits =
            misses < limits.usage_skip_after || misses.is_multiple_of(limits.usage_retry_every);
        if waits {
            let collected = self
                .ledger
                .collect(&args, true, limits.usage_first_wait, self.finished, limits)
                .await;
            {
                let mut inner = self
                    .ledger
                    .inner
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if collected.short {
                    inner.misses.insert(miss_key, misses.saturating_add(1));
                } else {
                    inner.misses.remove(&miss_key);
                }
            }
            self.absorb(collected);
        } else {
            let mut inner = self
                .ledger
                .inner
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            inner.misses.insert(miss_key, misses.saturating_add(1));
        }
        if let Some(mine) = self.mine.take() {
            StepOwn::Found(mine)
        } else {
            StepOwn::Owed
        }
    }

    pub(crate) async fn settle_deferred(
        &mut self,
        host: &HostContext,
        endpoints: &Endpoints,
        token: &SecretString,
        team_id: Option<&str>,
        limits: Limits,
        final_step: bool,
    ) {
        if !self.kept {
            return;
        }
        if final_step {
            let args = self.poll_args(host, endpoints, token, team_id, limits);
            let debt_key = self.debt_key();
            let still_owed = {
                let inner = self
                    .ledger
                    .inner
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                inner.owed.contains_key(&debt_key)
            };
            if still_owed {
                let collected = self
                    .ledger
                    .collect(&args, true, Duration::ZERO, self.finished, limits)
                    .await;
                self.absorb(collected);
            }
        }
    }

    pub(crate) fn take_deferred(&mut self) -> Option<RawUsage> {
        self.theirs.take()
    }

    pub(crate) fn done(&mut self, limits: Limits) {
        self.finished = true;
        let leftover = add_usage(self.theirs.take(), self.mine.take());
        if let Some(carry) = leftover {
            self.ledger.owe(
                &self.debt_key(),
                OwePatch {
                    since: self.since_ms,
                    n: 0,
                    ready_ms: self.since_ms,
                    carry: Some(carry),
                    before: None,
                },
                limits,
            );
        }
    }

    fn absorb(&mut self, collected: CollectResult) {
        self.theirs = add_usage(self.theirs, collected.carry);
        for event in collected.events {
            if event.at >= self.since_ms.saturating_sub(1000) {
                self.mine = add_usage(self.mine, Some(event.usage));
            } else {
                self.theirs = add_usage(self.theirs, Some(event.usage));
            }
        }
    }
}

fn token_fingerprint(token: &SecretString) -> String {
    crate::proto::to_hex(&sha256(secret_text(token).as_bytes()))
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn purge_stale_claims(inner: &mut LedgerInner, limits: Limits, now_ms: i64) {
    let now = now_ms;
    let ceiling = millis(limits.usage_owed_for).saturating_mul(2);
    inner
        .claimed
        .retain(|_, at| now.saturating_sub(*at) <= ceiling);
}

fn expire_debts(inner: &mut LedgerInner, now: i64, limits: Limits) {
    let owed_for = millis(limits.usage_owed_for);
    inner.owed.retain(|_, debt| {
        if now.saturating_sub(debt.since) > owed_for {
            return false;
        }
        debt.n > 0 || debt.carry.is_some()
    });
}

async fn one_dashboard_fetch(
    args: &LedgerPoll<'_>,
    budget: Duration,
) -> Result<Value, EventsError> {
    let now_ms = tokens::unix_ms(args.host.clock().now());
    let start = args.since_ms.saturating_sub(60_000);
    let end = now_ms.saturating_add(60_000);
    let mut body = json!({
        "startDate": start.to_string(),
        "endDate": end.to_string(),
        "page": 1,
        "pageSize": 50,
    });
    if let Some(team) = args.team_id
        && let Ok(id) = team.parse::<u64>()
    {
        body["teamId"] = json!(id);
    }
    let reply = connect::unary_opts(
        args.host,
        args.endpoints,
        "GetFilteredUsageEvents",
        args.token,
        &body.to_string(),
        args.team_id,
        Some(budget),
    )
    .await;
    match reply {
        Ok(body) => Ok(body),
        Err(error) => {
            let status = error.status().unwrap_or(0);
            if (400..500).contains(&status) && status != 429 {
                Err(EventsError::Unreadable)
            } else {
                Err(EventsError::Transport)
            }
        }
    }
}

fn raw_from_event(event: &Value) -> RawUsage {
    RawUsage {
        input: usage::number_of(event, &["inputTokens", "input_tokens"]),
        output: usage::number_of(event, &["outputTokens", "output_tokens"]),
        cache_read: usage::number_of(event, &["cacheReadTokens", "cache_read_tokens"]),
        cache_write: usage::number_of(event, &["cacheWriteTokens", "cache_write_tokens"]),
        reasoning: usage::number_of(
            event,
            &[
                "thinkingTokens",
                "reasoningTokens",
                "thinking_tokens",
                "reasoning_tokens",
            ],
        ),
    }
}

fn raw_from_mapped_usage(usage: &Usage) -> RawUsage {
    let cache_read = usage.cache_read_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_tokens.unwrap_or(0);
    let input = usage.input_tokens.unwrap_or(0);
    let uncached = input.saturating_sub(cache_read);
    RawUsage {
        input: uncached,
        output: usage.output_tokens.unwrap_or(0),
        cache_read,
        cache_write,
        reasoning: usage.reasoning_tokens.unwrap_or(0),
    }
}

pub(crate) fn attach_deferred(usage: &mut Usage, deferred: RawUsage, inbound: ProtocolId) {
    if deferred.is_empty() {
        return;
    }
    let mapped = usage::from_dashboard(deferred.into(), inbound);
    usage.extensions.insert(
        "deferred_usage",
        json!({
            "input_tokens": mapped.input_tokens,
            "output_tokens": mapped.output_tokens,
            "cache_read_tokens": mapped.cache_read_tokens,
            "cache_write_tokens": mapped.cache_write_tokens,
            "reasoning_tokens": mapped.reasoning_tokens,
        }),
    );
}

pub(crate) fn step_usage_from_own(own: StepOwn, inbound: ProtocolId) -> Usage {
    match own {
        StepOwn::Found(raw) => usage::from_dashboard(raw.into(), inbound),
        StepOwn::Owed => usage::zeros(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_sdk::json::json;

    const E1: RawUsage = RawUsage {
        input: 2,
        output: 1913,
        cache_read: 177_533,
        cache_write: 3735,
        reasoning: 0,
    };

    #[test]
    fn r9k_deferred_extension_uses_mapped_counters() {
        let mut usage = usage::zeros();
        attach_deferred(&mut usage, E1, ProtocolId::OPENAI_CHAT);
        let deferred = usage
            .extensions
            .get("deferred_usage")
            .expect("deferred_usage");
        let own = usage::from_dashboard(E1.into(), ProtocolId::OPENAI_CHAT);
        assert_eq!(deferred["input_tokens"], json!(own.input_tokens));
        assert_eq!(deferred["output_tokens"], json!(own.output_tokens));
    }
}

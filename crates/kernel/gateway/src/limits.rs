//! Gateway-key budgets, reservations, and the caller's limit route.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use chrono::{DateTime, Local};
use hofund_codec::Codec;
use hofund_ir::Usage;
use hofund_plugin_api::ProtocolId;
use hofund_secrets::{Identity, KeyLimit};
use hofund_usage::{KeyUsage, Ledger};
use serde::Serialize;

use super::{Inner, authenticate_request, coded, json_response, list_error};

#[derive(Clone)]
pub(super) struct Budget {
    usage: Arc<Ledger>,
    held: Arc<Mutex<HashMap<WindowKey, Held>>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
struct WindowKey {
    key_id: String,
    start: i64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Held {
    requests: u64,
    tokens: u64,
    cost: f64,
}

/// A reservation remains in the window where its request started. Dropping it
/// releases every window exactly once.
pub(super) struct Reservation {
    budget: Budget,
    holds: Vec<(String, i64, u64)>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut held = self
            .budget
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key_id, start, tokens) in &self.holds {
            let key = WindowKey {
                key_id: key_id.clone(),
                start: *start,
            };
            if let Some(current) = held.get_mut(&key) {
                current.requests = current.requests.saturating_sub(1);
                current.tokens = current.tokens.saturating_sub(*tokens);
                if current.requests == 0 {
                    held.remove(&key);
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct LimitStatus {
    pub period: String,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub days: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub hours: u32,
    #[serde(serialize_with = "serialize_time")]
    pub start: i64,
    #[serde(serialize_with = "serialize_time")]
    pub reset: i64,
    pub calls: u64,
    pub tokens: u64,
    pub cost: f64,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub unpriced: u64,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub in_flight: u64,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub reserved: u64,
    #[serde(skip_serializing_if = "is_zero_f64")]
    pub reserved_cost: f64,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub token_limit: u64,
    #[serde(skip_serializing_if = "is_zero_f64")]
    pub cost_limit: f64,
    pub cache_reads: bool,
    pub tokens_left: u64,
    pub cost_left: f64,
    pub spent: bool,
}

impl LimitStatus {
    fn share(&self) -> f64 {
        let tokens = if self.token_limit > 0 {
            self.tokens as f64 / self.token_limit as f64
        } else {
            0.0
        };
        let cost = if self.cost_limit > 0.0 {
            self.cost / self.cost_limit
        } else {
            0.0
        };
        tokens.max(cost)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Refusal {
    pub status: LimitStatus,
    key: String,
}

impl Refusal {
    pub fn message(&self) -> String {
        let status = &self.status;
        let what = if status.token_limit > 0
            && status.tokens.saturating_add(status.reserved) >= status.token_limit
        {
            let counted = if status.cache_reads {
                "input, output and cache tokens"
            } else {
                "input, output and cache-write tokens"
            };
            format!(
                "{} of its {} {}",
                status.tokens, status.token_limit, counted
            )
        } else {
            format!(
                "${:.2} of its ${:.2} estimated cost",
                status.cost, status.cost_limit
            )
        };
        let held = if status.spent {
            String::new()
        } else {
            format!(
                ", with {} requests in flight holding the rest",
                status.in_flight
            )
        };
        let reset = local_time(status.reset)
            .map(|time| time.format("%Y-%m-%d %H:%M %Z").to_string())
            .unwrap_or_else(|| status.reset.to_string());
        format!(
            "hofund gateway key {:?} has used {}{} for this {}; it resets at {}",
            self.key,
            what,
            held,
            KeyLimit {
                period: status.period.clone(),
                days: status.days,
                hours: status.hours,
                ..KeyLimit::default()
            }
            .per(),
            reset
        )
    }

    pub fn retry_after(&self, now: DateTime<Local>) -> u64 {
        let seconds = self.status.reset.saturating_sub(now.timestamp());
        u64::try_from(seconds.max(1)).unwrap_or(u64::MAX)
    }
}

#[derive(Debug)]
pub(super) enum ReserveFailure {
    Refused(Box<Refusal>),
    Unavailable,
}

impl Budget {
    pub fn new(usage: Arc<Ledger>) -> Self {
        Self {
            usage,
            held: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn reserve(
        &self,
        identity: &Identity,
        body_bytes: usize,
        now: DateTime<Local>,
    ) -> Result<Option<Reservation>, ReserveFailure> {
        let mut identities = vec![identity.clone()];
        if !identity.issuer.is_empty() {
            identities.push(Identity {
                key_id: identity.issuer.clone(),
                key_name: identity.issuer_name.clone(),
                limits: identity.issuer_limits.clone(),
                ..Identity::default()
            });
        }
        let mut callers = Vec::new();
        for caller in identities {
            let limits: Vec<KeyLimit> = caller
                .limits
                .iter()
                .filter(|limit| limit.limited())
                .cloned()
                .collect();
            if caller.key_id.is_empty() || limits.is_empty() {
                continue;
            }
            let windows: Vec<_> = limits.iter().map(|limit| limit.window(now)).collect();
            let usage = self.usage_for(&caller.key_id, &windows)?;
            callers.push((caller, limits, windows, usage));
        }
        if callers.is_empty() {
            return Ok(None);
        }

        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut refusal = None;
        for (caller, limits, windows, usage) in &callers {
            for (index, limit) in limits.iter().enumerate() {
                let window = windows[index];
                let current = held
                    .get(&WindowKey {
                        key_id: caller.key_id.clone(),
                        start: window.start,
                    })
                    .copied();
                let status = make_status(limit, window, usage[index], current);
                let reserved = current.unwrap_or_default();
                let spent = (limit.tokens > 0
                    && usage[index]
                        .tokens(limit.cache_reads)
                        .saturating_add(reserved.tokens)
                        >= limit.tokens)
                    || (limit.cost > 0.0 && reserved.cost >= limit.cost);
                if spent
                    && refusal
                        .as_ref()
                        .is_none_or(|current: &Refusal| window.reset > current.status.reset)
                {
                    refusal = Some(Refusal {
                        status,
                        key: caller.key_name.clone(),
                    });
                }
            }
        }
        if let Some(refusal) = refusal {
            return Err(ReserveFailure::Refused(Box::new(refusal)));
        }

        let mut holds = Vec::new();
        for (caller, _limits, windows, usage) in &callers {
            for (index, window) in windows.iter().enumerate() {
                if holds
                    .iter()
                    .any(|(key_id, start, _)| *start == window.start && key_id == &caller.key_id)
                {
                    continue;
                }
                let mean_output = usage[index]
                    .output_tokens
                    .checked_div(usage[index].calls)
                    .unwrap_or(0);
                let estimate = u64::try_from(body_bytes / 4)
                    .unwrap_or(u64::MAX)
                    .saturating_add(mean_output);
                holds.push((caller.key_id.clone(), window.start, estimate));
            }
        }
        for (key_id, start, tokens) in &holds {
            let current = held
                .entry(WindowKey {
                    key_id: key_id.clone(),
                    start: *start,
                })
                .or_default();
            current.requests = current.requests.saturating_add(1);
            current.tokens = current.tokens.saturating_add(*tokens);
        }
        drop(held);
        Ok(Some(Reservation {
            budget: self.clone(),
            holds,
        }))
    }

    pub fn status(
        &self,
        identity: &Identity,
        now: DateTime<Local>,
    ) -> Result<Vec<LimitStatus>, ReserveFailure> {
        let limits: Vec<&KeyLimit> = identity
            .limits
            .iter()
            .filter(|limit| limit.limited())
            .collect();
        if identity.key_id.is_empty() || limits.is_empty() {
            return Ok(Vec::new());
        }
        let windows: Vec<_> = limits.iter().map(|limit| limit.window(now)).collect();
        let usage = self.usage_for(&identity.key_id, &windows)?;
        let held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(limits
            .iter()
            .enumerate()
            .map(|(index, limit)| {
                let window = windows[index];
                let current = held
                    .get(&WindowKey {
                        key_id: identity.key_id.clone(),
                        start: window.start,
                    })
                    .copied();
                make_status(limit, window, usage[index], current)
            })
            .collect())
    }

    fn usage_for(
        &self,
        key_id: &str,
        windows: &[hofund_secrets::LimitWindow],
    ) -> Result<Vec<KeyUsage>, ReserveFailure> {
        let mut unique = HashMap::<i64, KeyUsage>::new();
        for window in windows {
            if let std::collections::hash_map::Entry::Vacant(entry) = unique.entry(window.start) {
                let usage = self
                    .usage
                    .key_usage(key_id, window.start)
                    .map_err(|_| ReserveFailure::Unavailable)?;
                entry.insert(usage);
            }
        }
        Ok(windows
            .iter()
            .map(|window| unique.get(&window.start).copied().unwrap_or_default())
            .collect())
    }
}

pub(super) fn binding(statuses: &[LimitStatus]) -> Option<LimitStatus> {
    let mut binding: Option<LimitStatus> = None;
    for status in statuses {
        let replace = match &binding {
            None => true,
            Some(current) if status.spent != current.spent => status.spent,
            Some(current) if status.spent => status.reset > current.reset,
            Some(current) => status.share() > current.share(),
        };
        if replace {
            binding = Some(status.clone());
        }
    }
    binding
}

pub(super) fn refusal_response(
    codec: &dyn Codec,
    refusal: &Refusal,
    now: DateTime<Local>,
) -> Response {
    let mut response = coded(codec, StatusCode::TOO_MANY_REQUESTS, &refusal.message());
    let headers = response.headers_mut();
    headers.insert(
        header::RETRY_AFTER,
        HeaderValue::from(refusal.retry_after(now)),
    );
    headers.insert("x-should-retry", HeaderValue::from_static("false"));
    if let Some(reset) = local_time(refusal.status.reset)
        .and_then(|time| HeaderValue::from_str(&time.to_rfc3339()).ok())
    {
        headers.insert("x-hofund-limit-reset", reset);
    }
    response
}

pub(super) async fn limit_route(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let inherited = request.extensions().get::<Identity>().cloned();
    let identity =
        match authenticate_request(&app, request.headers(), request.uri(), inherited, None) {
            Ok(identity) => identity,
            Err(response) => return *response,
        };
    let now = Local::now();
    let statuses = match app.budget.status(&identity, now) {
        Ok(statuses) => statuses,
        Err(ReserveFailure::Unavailable) => {
            return json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                list_error("gateway key limits are temporarily unavailable"),
            );
        }
        Err(ReserveFailure::Refused(_)) => unreachable!("status does not refuse"),
    };
    let issuer_statuses = if identity.issuer.is_empty() {
        Vec::new()
    } else {
        let issuer = Identity {
            key_id: identity.issuer.clone(),
            key_name: identity.issuer_name.clone(),
            limits: identity.issuer_limits.clone(),
            ..Identity::default()
        };
        match app.budget.status(&issuer, now) {
            Ok(statuses) => statuses,
            Err(ReserveFailure::Unavailable) => {
                return json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    list_error("gateway key limits are temporarily unavailable"),
                );
            }
            Err(ReserveFailure::Refused(_)) => unreachable!("status does not refuse"),
        }
    };
    let mut out = serde_json::json!({
        "limited": !statuses.is_empty(),
        "key": identity.key_name,
    });
    let object = out.as_object_mut().expect("limit object");
    if let Some(expires) = identity.expires {
        object.insert(
            "expires".to_owned(),
            serde_json::json!(hofund_secrets::expiry_text(expires)),
        );
    }
    if !statuses.is_empty() {
        object.insert("limit".to_owned(), serde_json::json!(binding(&statuses)));
        object.insert("limits".to_owned(), serde_json::json!(statuses));
    }
    if !issuer_statuses.is_empty() {
        object.insert(
            "issuerLimits".to_owned(),
            serde_json::json!(issuer_statuses),
        );
    }
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&out).unwrap_or_else(|_| b"{\"limited\":false}".to_vec()),
    )
}

pub(super) fn observe_usage(
    meta: &mut super::UsageMeta,
    usage: &Usage,
    protocol: &str,
    saw_reasoning: bool,
) {
    if saw_reasoning {
        meta.saw_reasoning = true;
    }
    let input = super::as_i64(usage.input_tokens);
    let output = super::as_i64(usage.output_tokens);
    if input.is_some() {
        meta.input = input;
    }
    if output.is_some() {
        meta.output = output;
    }
    if let Some(value) = super::as_i64(usage.cache_read_tokens) {
        meta.details.cache_read_tokens = Some(value);
    }
    let (total, five, hour) = hofund_ir::normalize_cache_write(
        usage.cache_write_tokens,
        usage.cache_write_5m_tokens,
        usage.cache_write_1h_tokens,
    );
    if let Some(value) = super::as_i64(total) {
        meta.details.cache_write_tokens = Some(value);
    }
    if let Some(value) = super::as_i64(five) {
        meta.details.cache_write_5m_tokens = Some(value);
    }
    if let Some(value) = super::as_i64(hour) {
        meta.details.cache_write_1h_tokens = Some(value);
    }
    if let Some(value) = super::as_i64(usage.reasoning_tokens) {
        meta.details.reasoning_tokens = Some(value);
    }
    if let Some(input) = input {
        let cached = if input_includes_cache(protocol) {
            meta.details.cache_read_tokens.unwrap_or(0)
        } else {
            0
        };
        meta.details.uncached_input_tokens = Some(input.saturating_sub(cached));
    }
    meta.details.reasoning_hidden =
        super::reasoning_hidden(protocol, meta.saw_reasoning, meta.details.reasoning_tokens);
}

pub(super) fn input_includes_cache(protocol: &str) -> bool {
    matches!(
        protocol,
        id if id == ProtocolId::OPENAI_CHAT.as_str()
            || id == ProtocolId::OPENAI_RESPONSES.as_str()
            || id == ProtocolId::GENERATIVE_LANGUAGE.as_str()
    )
}

fn make_status(
    limit: &KeyLimit,
    window: hofund_secrets::LimitWindow,
    usage: KeyUsage,
    held: Option<Held>,
) -> LimitStatus {
    let held = held.unwrap_or_default();
    let tokens = usage.tokens(limit.cache_reads);
    LimitStatus {
        period: limit.period.clone(),
        days: limit.days,
        hours: limit.hours,
        start: window.start,
        reset: window.reset,
        calls: usage.calls,
        tokens,
        cost: usage.cost_usd,
        unpriced: usage.unpriced,
        in_flight: held.requests,
        reserved: held.tokens,
        reserved_cost: held.cost,
        token_limit: limit.tokens,
        cost_limit: limit.cost,
        cache_reads: limit.cache_reads,
        tokens_left: limit.tokens.saturating_sub(tokens),
        cost_left: (limit.cost - usage.cost_usd).max(0.0),
        spent: (limit.tokens > 0 && tokens >= limit.tokens)
            || (limit.cost > 0.0 && usage.cost_usd >= limit.cost),
    }
}

fn local_time(timestamp: i64) -> Option<DateTime<Local>> {
    DateTime::from_timestamp(timestamp, 0).map(|time| time.with_timezone(&Local))
}

fn serialize_time<S>(timestamp: &i64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match local_time(*timestamp) {
        Some(time) => serializer.serialize_str(&time.to_rfc3339()),
        None => serializer.serialize_i64(*timestamp),
    }
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_zero_f64(value: &f64) -> bool {
    *value == 0.0
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use chrono::TimeZone;
    use hofund_secrets::KeyLimit;
    use hofund_usage::{Row, UsageDetails};

    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<Ledger>, Budget) {
        let dir = tempfile::tempdir().expect("dir");
        let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger"));
        (dir, Arc::clone(&usage), Budget::new(usage))
    }

    fn identity(limits: Vec<KeyLimit>) -> Identity {
        Identity {
            key_id: "key".to_owned(),
            key_name: "Busy".to_owned(),
            limits,
            ..Identity::default()
        }
    }

    fn token_limit(tokens: u64) -> KeyLimit {
        KeyLimit {
            period: "month".to_owned(),
            tokens,
            ..KeyLimit::default()
        }
    }

    #[test]
    fn l4_parallel_reservations_cannot_overshoot_a_token_limit() {
        let (_dir, _usage, budget) = fixture();
        let identity = identity(vec![token_limit(100)]);
        let now = Local::now();
        let threads = 20;
        let barrier = Arc::new(Barrier::new(threads));
        let mut workers = Vec::new();
        for _ in 0..threads {
            let budget = budget.clone();
            let identity = identity.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                budget.reserve(&identity, 40, now)
            }));
        }
        let reservations: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker"))
            .filter_map(|result| result.ok().flatten())
            .collect();
        assert_eq!(reservations.len(), 10);
        let status = budget.status(&identity, now).expect("status").remove(0);
        assert_eq!(status.in_flight, 10);
        assert_eq!(status.reserved, 100);
        drop(reservations);
        let status = budget.status(&identity, now).expect("released").remove(0);
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.reserved, 0);

        let mut first = identity.clone();
        first.key_id = "first".to_owned();
        first.limits = vec![token_limit(10)];
        let mut second = identity.clone();
        second.key_id = "other".to_owned();
        second.limits = vec![token_limit(10)];
        let old = budget
            .reserve(&first, 40, now)
            .expect("first key")
            .expect("reservation");
        let other = budget
            .reserve(&second, 40, now)
            .expect("second key")
            .expect("reservation");
        assert_eq!(
            budget.status(&first, now).expect("first status")[0].reserved,
            10
        );
        assert_eq!(
            budget.status(&second, now).expect("second status")[0].reserved,
            10
        );
        drop(old);
        assert_eq!(
            budget.status(&first, now).expect("first release")[0].reserved,
            0
        );
        assert_eq!(
            budget.status(&second, now).expect("second held")[0].reserved,
            10
        );
        drop(other);
    }

    #[test]
    fn l5_refusal_reports_the_latest_binding_reset() {
        let (_dir, _usage, budget) = fixture();
        let identity = identity(vec![
            KeyLimit {
                period: "day".to_owned(),
                tokens: 1,
                ..KeyLimit::default()
            },
            KeyLimit {
                period: "week".to_owned(),
                tokens: 1,
                ..KeyLimit::default()
            },
        ]);
        let now = Local
            .with_ymd_and_hms(2026, 10, 7, 12, 0, 0)
            .single()
            .expect("Wednesday");
        let _first = budget
            .reserve(&identity, 4, now)
            .expect("first")
            .expect("reservation");
        let error = match budget.reserve(&identity, 4, now) {
            Err(error) => error,
            Ok(_) => panic!("a reserved window was not spent"),
        };
        let ReserveFailure::Refused(refusal) = error else {
            panic!("expected refusal")
        };
        assert_eq!(refusal.status.period, "week");
        assert!(refusal.message().contains("resets at"));
        assert!(refusal.retry_after(now) >= 1);
        // A reset already past still asks the client to wait one second.
        assert_eq!(refusal.retry_after(now + chrono::Duration::days(30)), 1);
        assert_eq!(
            binding(std::slice::from_ref(&refusal.status)).expect("binding"),
            refusal.status
        );
    }

    #[test]
    fn l6_usage_sums_survive_budget_restart() {
        let (dir, usage, budget) = fixture();
        let identity = identity(vec![token_limit(100)]);
        let now = Local::now();
        let window = identity.limits[0].window(now);
        usage
            .record_with_details(
                &Row {
                    created_unix: window.start,
                    model: "demo".to_owned(),
                    upstream: "member".to_owned(),
                    input_tokens: Some(90),
                    output_tokens: Some(10),
                    latency_ms: 1,
                    status: "200".to_owned(),
                    caller_key_id: "key".to_owned(),
                    caller_key_name: "Busy".to_owned(),
                    caller_issuer_id: String::new(),
                    ..Default::default()
                },
                None,
                UsageDetails {
                    uncached_input_tokens: Some(90),
                    ..UsageDetails::default()
                },
                None,
            )
            .expect("record");
        let restarted = Budget::new(usage);
        let status = restarted.status(&identity, now).expect("status").remove(0);
        assert_eq!(status.tokens, 100);
        assert_eq!(status.calls, 1);
        assert!(restarted.reserve(&identity, 200, now).is_err());
        drop((budget, dir));
    }

    #[test]
    fn l8_unpriced_calls_are_reported_without_cost() {
        let (_dir, usage, budget) = fixture();
        let identity = identity(vec![KeyLimit {
            period: "day".to_owned(),
            cost: 0.01,
            ..KeyLimit::default()
        }]);
        let now = Local::now();
        let window = identity.limits[0].window(now);
        usage
            .record(&Row {
                created_unix: window.start,
                model: "demo".to_owned(),
                upstream: "member".to_owned(),
                input_tokens: Some(1_000_000),
                output_tokens: Some(1_000_000),
                latency_ms: 1,
                status: "200".to_owned(),
                caller_key_id: "key".to_owned(),
                caller_key_name: "Busy".to_owned(),
                caller_issuer_id: String::new(),
                ..Default::default()
            })
            .expect("record");
        let status = budget.status(&identity, now).expect("status").remove(0);
        assert_eq!(status.cost, 0.0);
        assert_eq!(status.unpriced, 1);
        assert!(!status.spent);
        assert!(budget.reserve(&identity, 1_000_000, now).is_ok());
    }

    #[test]
    fn l4_reservation_stays_in_its_original_window() {
        let (_dir, _usage, budget) = fixture();
        let identity = identity(vec![KeyLimit {
            period: "hours".to_owned(),
            hours: 1,
            tokens: 10,
            since: 0,
            ..KeyLimit::default()
        }]);
        let before = Local.timestamp_opt(3_500, 0).single().expect("before");
        let after = Local.timestamp_opt(3_700, 0).single().expect("after");
        let old = budget
            .reserve(&identity, 40, before)
            .expect("old")
            .expect("reservation");
        assert!(budget.reserve(&identity, 4, before).is_err());
        let current = budget.reserve(&identity, 40, after).expect("new window");
        assert!(current.is_some());
        drop(old);
        assert!(budget.reserve(&identity, 40, after).is_err());
        drop(current);
        assert!(budget.reserve(&identity, 40, after).is_ok());
    }

    fn limit_status(period: &str, reset: i64, tokens: u64, spent: bool) -> LimitStatus {
        LimitStatus {
            period: period.to_owned(),
            days: 0,
            hours: 0,
            start: 0,
            reset,
            calls: 1,
            tokens,
            cost: 0.0,
            unpriced: 0,
            in_flight: 0,
            reserved: 0,
            reserved_cost: 0.0,
            token_limit: 100,
            cost_limit: 0.0,
            cache_reads: false,
            tokens_left: 100_u64.saturating_sub(tokens),
            cost_left: 0.0,
            spent,
        }
    }

    #[test]
    fn l7_binding_is_the_spent_limit_resetting_last_else_the_fullest() {
        let day = limit_status("day", 10, 100, true);
        let week = limit_status("week", 70, 100, true);
        let month = limit_status("month", 300, 20, false);
        for order in [
            vec![day.clone(), week.clone(), month.clone()],
            vec![month.clone(), week.clone(), day.clone()],
        ] {
            assert_eq!(binding(&order).expect("binding").period, "week");
        }
        let fuller = limit_status("week", 70, 60, false);
        for order in [
            vec![month.clone(), fuller.clone()],
            vec![fuller.clone(), month.clone()],
        ] {
            assert_eq!(binding(&order).expect("binding").period, "week");
        }
        assert!(binding(&[]).is_none());
    }

    #[test]
    fn l4_estimate_adds_the_mean_output_and_holds_a_shared_window_once() {
        let (_dir, usage, budget) = fixture();
        let now = Local
            .with_ymd_and_hms(2026, 10, 7, 12, 0, 0)
            .single()
            .expect("noon");
        let day = KeyLimit {
            period: "day".to_owned(),
            tokens: 1_000,
            ..KeyLimit::default()
        };
        let midnight = day.window(now).start;
        let cycle = KeyLimit {
            period: "hours".to_owned(),
            hours: 24,
            tokens: 1_000,
            since: midnight,
            ..KeyLimit::default()
        };
        assert_eq!(cycle.window(now).start, midnight);
        let identity = identity(vec![day, cycle]);
        usage
            .record(&Row {
                created_unix: midnight,
                model: "demo".to_owned(),
                upstream: "member".to_owned(),
                input_tokens: Some(0),
                output_tokens: Some(30),
                latency_ms: 1,
                status: "200".to_owned(),
                caller_key_id: "key".to_owned(),
                caller_key_name: "Busy".to_owned(),
                caller_issuer_id: String::new(),
                ..Default::default()
            })
            .expect("record");
        let held = budget
            .reserve(&identity, 40, now)
            .expect("reserve")
            .expect("reservation");
        let statuses = budget.status(&identity, now).expect("status");
        for status in &statuses {
            // 40 bytes / 4 plus a mean output of 30, held once for the shared start.
            assert_eq!(status.reserved, 40);
            assert_eq!(status.in_flight, 1);
        }
        drop(held);
    }

    #[test]
    fn s7_issued_usage_and_reservations_count_against_both_keys() {
        let (_dir, usage, budget) = fixture();
        let now = Local::now();
        let parent = identity(vec![KeyLimit {
            period: "month".to_owned(),
            tokens: 10,
            ..KeyLimit::default()
        }]);
        let child = Identity {
            key_id: "child".to_owned(),
            key_name: "Sandbox".to_owned(),
            issuer: parent.key_id.clone(),
            issuer_name: parent.key_name.clone(),
            issuer_limits: parent.limits.clone(),
            ..Identity::default()
        };
        let held = budget
            .reserve(&child, 40, now)
            .expect("first reservation")
            .expect("held");
        let status = budget
            .status(&parent, now)
            .expect("issuer status")
            .remove(0);
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.reserved, 10);
        let error = budget.reserve(&child, 40, now);
        assert!(matches!(error, Err(ReserveFailure::Refused(_))));
        drop(held);
        usage
            .record(&Row {
                created_unix: now.timestamp(),
                model: "demo".to_owned(),
                upstream: "member".to_owned(),
                input_tokens: Some(10),
                output_tokens: Some(0),
                latency_ms: 1,
                status: "200".to_owned(),
                caller_key_id: child.key_id.clone(),
                caller_key_name: child.key_name.clone(),
                caller_issuer_id: parent.key_id.clone(),
                ..Default::default()
            })
            .expect("issued usage");
        let status = budget.status(&parent, now).expect("persisted").remove(0);
        assert_eq!(status.calls, 1);
        assert_eq!(status.tokens, 10);
        assert!(budget.reserve(&child, 40, now).is_err());
    }

    #[test]
    fn l3_responses_and_chat_inputs_include_cache_reads() {
        assert!(input_includes_cache(ProtocolId::OPENAI_RESPONSES.as_str()));
        assert!(input_includes_cache(ProtocolId::OPENAI_CHAT.as_str()));
        assert!(!input_includes_cache("plain"));
    }
}

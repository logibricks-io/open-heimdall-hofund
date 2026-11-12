//! Read-only projection of gateway usage rows into sessions.

use std::collections::BTreeMap;

use chrono::{DateTime, Local, Utc};

use crate::{Ledger, Row, UsageError, counts_as_request};

/// One model's token totals inside a gateway session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GatewayModelTotals {
    /// Model id.
    pub model: String,
    /// Sum of input tokens.
    pub input_tokens: u64,
    /// Sum of output tokens.
    pub output_tokens: u64,
    /// Sum of cache-read tokens.
    pub cache_read_tokens: u64,
    /// Sum of five-minute cache-write tokens.
    pub cache_write_5m_tokens: u64,
    /// Sum of one-hour cache-write tokens.
    pub cache_write_1h_tokens: u64,
    /// Sum of reasoning tokens.
    pub reasoning_tokens: u64,
    /// Stored USD for priced rows.
    pub cost_usd: f64,
    /// Whether every token-bearing row had a price.
    pub priced: bool,
}

/// A gateway session aggregated from the usage ledger.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GatewaySession {
    /// Detected client agent id.
    pub agent: String,
    /// Session id from `sessionOf`.
    pub id: String,
    /// First request time in Unix seconds.
    pub start_unix: i64,
    /// Last request end time in Unix seconds.
    pub last_unix: i64,
    /// Title until a recording store exists.
    pub title: String,
    /// Tokens by model.
    pub models: Vec<GatewayModelTotals>,
    /// Total input tokens.
    pub input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Total cache-read tokens.
    pub cache_read_tokens: u64,
    /// Total five-minute cache-write tokens.
    pub cache_write_5m_tokens: u64,
    /// Total one-hour cache-write tokens.
    pub cache_write_1h_tokens: u64,
    /// Total reasoning tokens.
    pub reasoning_tokens: u64,
    /// Sum of priced USD.
    pub cost_usd: f64,
    /// Token-bearing rows without a stored price.
    pub unpriced: u64,
}

/// Groups ledger rows into gateway sessions, newest first.
///
/// Rows without a session id or without a counted request are omitted.
///
/// # Errors
///
/// Returns [`UsageError`] when the ledger cannot be read.
pub fn gateway_sessions(
    ledger: &Ledger,
    since: Option<i64>,
) -> Result<Vec<GatewaySession>, UsageError> {
    let rows = ledger.rows()?;
    let mut by_key: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for row in rows {
        if since.is_some_and(|since| row.created_unix < since) {
            continue;
        }
        if row.session_id.is_empty() || !counts_as_request(&row.outcome) {
            continue;
        }
        let key = (row.client_agent.clone(), row.session_id.clone());
        by_key.entry(key).or_default().push(&row);
    }
    let mut out: Vec<GatewaySession> = by_key.into_values().filter_map(Acc::finish).collect();
    out.sort_by(|left, right| {
        right
            .last_unix
            .cmp(&left.last_unix)
            .then_with(|| left.agent.cmp(&right.agent))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(out)
}

#[derive(Default)]
struct Acc {
    agent: String,
    id: String,
    start_unix: i64,
    last_unix: i64,
    models: BTreeMap<String, GatewayModelTotals>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
    reasoning_tokens: u64,
    cost_usd: f64,
    unpriced: u64,
}

impl Acc {
    fn push(&mut self, row: &Row) {
        if self.agent.is_empty() {
            self.agent = row.client_agent.clone();
            self.id = row.session_id.clone();
        }
        let end = row
            .created_unix
            .saturating_add(row.latency_ms.max(0) / 1000);
        if self.start_unix == 0 || row.created_unix < self.start_unix {
            self.start_unix = row.created_unix;
        }
        if end > self.last_unix {
            self.last_unix = end;
        }
        let input = row.input_tokens.filter(|value| *value > 0).unwrap_or(0) as u64;
        let output = row.output_tokens.filter(|value| *value > 0).unwrap_or(0) as u64;
        let cache_read = row
            .cache_read_tokens
            .filter(|value| *value > 0)
            .unwrap_or(0) as u64;
        let cache_write_5m = row
            .cache_write_5m_tokens
            .filter(|value| *value > 0)
            .unwrap_or(0) as u64;
        let cache_write_1h = row
            .cache_write_1h_tokens
            .filter(|value| *value > 0)
            .unwrap_or(0) as u64;
        let reasoning = row.reasoning_tokens.filter(|value| *value > 0).unwrap_or(0) as u64;
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.output_tokens = self.output_tokens.saturating_add(output);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(cache_read);
        self.cache_write_5m_tokens = self.cache_write_5m_tokens.saturating_add(cache_write_5m);
        self.cache_write_1h_tokens = self.cache_write_1h_tokens.saturating_add(cache_write_1h);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(reasoning);
        let model = self
            .models
            .entry(row.model.clone())
            .or_insert_with(|| GatewayModelTotals {
                model: row.model.clone(),
                priced: true,
                ..GatewayModelTotals::default()
            });
        model.input_tokens = model.input_tokens.saturating_add(input);
        model.output_tokens = model.output_tokens.saturating_add(output);
        model.cache_read_tokens = model.cache_read_tokens.saturating_add(cache_read);
        model.cache_write_5m_tokens = model.cache_write_5m_tokens.saturating_add(cache_write_5m);
        model.cache_write_1h_tokens = model.cache_write_1h_tokens.saturating_add(cache_write_1h);
        model.reasoning_tokens = model.reasoning_tokens.saturating_add(reasoning);
        if row.priced {
            let cost = row.cost_usd.unwrap_or(0.0);
            model.cost_usd += cost;
            self.cost_usd += cost;
        } else if input + output > 0 {
            model.priced = false;
            self.unpriced += 1;
        }
    }

    fn finish(self) -> Option<GatewaySession> {
        if self.id.is_empty() {
            return None;
        }
        let mut models: Vec<GatewayModelTotals> = self.models.into_values().collect();
        models.sort_by(|left, right| {
            let left_total = left.input_tokens + left.output_tokens;
            let right_total = right.input_tokens + right.output_tokens;
            right_total
                .cmp(&left_total)
                .then_with(|| left.model.cmp(&right.model))
        });
        Some(GatewaySession {
            agent: self.agent,
            id: self.id.clone(),
            start_unix: self.start_unix,
            last_unix: self.last_unix,
            title: self.id,
            models,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_5m_tokens: self.cache_write_5m_tokens,
            cache_write_1h_tokens: self.cache_write_1h_tokens,
            reasoning_tokens: self.reasoning_tokens,
            cost_usd: self.cost_usd,
            unpriced: self.unpriced,
        })
    }
}

/// Local calendar date for one Unix second.
#[must_use]
pub fn local_date(unix: i64) -> String {
    DateTime::<Utc>::from_timestamp(unix, 0)
        .map(|utc| utc.with_timezone(&Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ledger, Row};

    fn row(at: i64, agent: &str, session: &str, model: &str, input: i64, output: i64) -> Row {
        Row {
            created_unix: at,
            model: model.to_owned(),
            upstream: "up".to_owned(),
            input_tokens: Some(input),
            output_tokens: Some(output),
            latency_ms: 1000,
            status: "200".to_owned(),
            client_agent: agent.to_owned(),
            session_id: session.to_owned(),
            ..Row::default()
        }
    }

    fn priced() -> crate::RowPricing {
        crate::RowPricing {
            priced: true,
            cost_usd: 0.01,
            price: crate::TokenPrice::default(),
        }
    }

    #[test]
    fn r33_gateway_sessions_groups_by_agent_and_session() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        ledger
            .record_with_details(
                &row(10, "fixture-a", "s1", "m1", 1, 2),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("first");
        ledger
            .record_with_details(
                &row(20, "fixture-a", "s1", "m2", 3, 4),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("second");
        ledger
            .record_with_details(
                &row(30, "fixture-b", "s2", "m1", 5, 0),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("other");
        ledger
            .record(&Row {
                created_unix: 40,
                model: "m".to_owned(),
                upstream: "up".to_owned(),
                latency_ms: 1,
                status: "200".to_owned(),
                client_agent: "fixture-a".to_owned(),
                ..Row::default()
            })
            .expect("no session");
        let sessions = gateway_sessions(&ledger, None).expect("sessions");
        assert_eq!(sessions.len(), 2);
        let s1 = sessions
            .iter()
            .find(|session| session.id == "s1")
            .expect("s1");
        assert_eq!(s1.input_tokens, 4);
        assert_eq!(s1.output_tokens, 6);
        assert!((s1.cost_usd - 0.02).abs() < f64::EPSILON);
    }

    #[test]
    fn r33_gateway_sessions_respects_since_cutoff() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        ledger
            .record_with_details(
                &row(5, "fixture-a", "old", "m1", 1, 0),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("old");
        ledger
            .record_with_details(
                &row(50, "fixture-a", "new", "m1", 2, 0),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("new");
        let filtered = gateway_sessions(&ledger, Some(40)).expect("sessions");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "new");

        // A row at exactly `since` is kept (`created < since`, not `<=`).
        ledger
            .record_with_details(
                &row(40, "fixture-a", "edge", "m1", 3, 0),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("edge");
        let filtered = gateway_sessions(&ledger, Some(40)).expect("sessions");
        assert!(filtered.iter().any(|session| session.id == "edge"));
        assert!(filtered.iter().any(|session| session.id == "new"));
        assert!(filtered.iter().all(|session| session.id != "old"));
    }

    #[test]
    fn r33_gateway_sessions_start_last_and_zero_tokens() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        let mut early = row(30, "fixture-a", "s1", "m1", 4, 0);
        early.latency_ms = 2500;
        ledger
            .record_with_details(&early, None, Default::default(), Some(priced()))
            .expect("early");
        let mut later = row(10, "fixture-a", "s1", "m1", -1, 0);
        later.latency_ms = 100;
        later.input_tokens = Some(-1);
        later.output_tokens = Some(0);
        ledger
            .record_with_details(&later, None, Default::default(), Some(priced()))
            .expect("later earlier timestamp");
        let sessions = gateway_sessions(&ledger, None).expect("sessions");
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.start_unix, 10);
        // 30 + 2500ms/1000 = 32
        assert_eq!(session.last_unix, 32);
        assert_eq!(session.input_tokens, 4);
        assert_eq!(session.output_tokens, 0);
    }

    #[test]
    fn r33_gateway_sessions_projects_cache_and_reasoning_tokens() {
        // Cache and reasoning are written through UsageDetails, not by
        // setting the same fields on Row and passing Default details.
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        let details = crate::UsageDetails {
            cache_read_tokens: Some(2),
            cache_write_5m_tokens: Some(3),
            cache_write_1h_tokens: Some(4),
            reasoning_tokens: Some(5),
            ..crate::UsageDetails::default()
        };
        ledger
            .record_with_details(
                &row(10, "fixture-a", "s1", "m1", 7, 8),
                None,
                details,
                Some(priced()),
            )
            .expect("row");
        let zeroed = crate::UsageDetails {
            cache_read_tokens: Some(0),
            reasoning_tokens: Some(-1),
            ..crate::UsageDetails::default()
        };
        ledger
            .record_with_details(
                &row(11, "fixture-a", "s1", "m1", 1, 1),
                None,
                zeroed,
                Some(priced()),
            )
            .expect("zeroed");
        let stored = ledger
            .rows()
            .expect("rows")
            .into_iter()
            .find(|row| row.cache_read_tokens == Some(2))
            .expect("details row persisted");
        assert_eq!(stored.cache_write_5m_tokens, Some(3));
        assert_eq!(stored.cache_write_1h_tokens, Some(4));
        assert_eq!(stored.reasoning_tokens, Some(5));
        let session = &gateway_sessions(&ledger, None).expect("sessions")[0];
        assert_eq!(session.cache_read_tokens, 2);
        assert_eq!(session.cache_write_5m_tokens, 3);
        assert_eq!(session.cache_write_1h_tokens, 4);
        assert_eq!(session.reasoning_tokens, 5);
        assert_eq!(session.input_tokens, 8);
        assert_eq!(session.output_tokens, 9);
        let model = session
            .models
            .iter()
            .find(|model| model.model == "m1")
            .expect("m1");
        assert_eq!(model.cache_read_tokens, 2);
        assert_eq!(model.reasoning_tokens, 5);
    }

    #[test]
    fn r33_gateway_sessions_drops_non_request_outcomes() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        let mut refused = row(20, "fixture-a", "only-refusal", "m1", 99, 99);
        refused.outcome = "refusal".to_owned();
        ledger
            .record_with_details(&refused, None, Default::default(), Some(priced()))
            .expect("refusal");

        ledger
            .record_with_details(
                &row(10, "fixture-a", "s1", "m1", 4, 5),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("ok");
        let mut handed = row(15, "fixture-a", "s1", "m1", 80, 80);
        handed.outcome = crate::OUTCOME_HANDED_ON.to_owned();
        ledger
            .record_with_details(&handed, None, Default::default(), Some(priced()))
            .expect("handed_on");

        let sessions = gateway_sessions(&ledger, None).expect("sessions");
        assert!(sessions.iter().all(|session| session.id != "only-refusal"));
        let s1 = sessions
            .iter()
            .find(|session| session.id == "s1")
            .expect("s1");
        assert_eq!(s1.input_tokens, 4);
        assert_eq!(s1.output_tokens, 5);
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn local_date_formats_yyyy_mm_dd() {
        let text = local_date(1_600_000_000);
        assert_eq!(text.len(), 10, "{text}");
        assert_ne!(text, "xyzzy");
        let bytes = text.as_bytes();
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        assert!(bytes[..4].iter().all(u8::is_ascii_digit), "{text}");
        assert!(bytes[5..7].iter().all(u8::is_ascii_digit), "{text}");
        assert!(bytes[8..].iter().all(u8::is_ascii_digit), "{text}");
    }

    #[test]
    fn r33_gateway_sessions_sorts_models_by_token_sum() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        // m-high: 3+0=3; m-low: 1+1=2. Replacing + with * would sort m-low first.
        ledger
            .record_with_details(
                &row(10, "fixture-a", "s1", "m-high", 3, 0),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("high");
        ledger
            .record_with_details(
                &row(11, "fixture-a", "s1", "m-low", 1, 1),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("low");
        let session = &gateway_sessions(&ledger, None).expect("sessions")[0];
        let models: Vec<_> = session
            .models
            .iter()
            .map(|model| model.model.as_str())
            .collect();
        assert_eq!(models, ["m-high", "m-low"]);
    }

    #[test]
    fn r33_gateway_sessions_tracks_unpriced_rows_and_model_cost() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        ledger
            .record_with_details(
                &row(10, "fixture-a", "s1", "priced", 2, 0),
                None,
                Default::default(),
                Some(priced()),
            )
            .expect("priced");
        ledger
            .record(&row(11, "fixture-a", "s1", "tokens", 0, 5))
            .expect("unpriced tokens");
        ledger
            .record(&row(12, "fixture-a", "s1", "tokens", 0, 5))
            .expect("second unpriced");
        ledger
            .record(&row(13, "fixture-a", "s1", "zero", 0, 0))
            .expect("unpriced zeros");
        let session = &gateway_sessions(&ledger, None).expect("sessions")[0];
        assert_eq!(session.unpriced, 2);
        assert!((session.cost_usd - 0.01).abs() < f64::EPSILON);
        let priced_model = session
            .models
            .iter()
            .find(|model| model.model == "priced")
            .expect("priced model");
        assert!(priced_model.priced);
        assert!((priced_model.cost_usd - 0.01).abs() < f64::EPSILON);
        let tokens = session
            .models
            .iter()
            .find(|model| model.model == "tokens")
            .expect("unpriced model");
        assert!(!tokens.priced);
        let zero = session
            .models
            .iter()
            .find(|model| model.model == "zero")
            .expect("zero model");
        assert!(zero.priced);
    }
}

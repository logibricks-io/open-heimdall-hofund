//! Cache-aware usage records and per-key budget sums.

use rusqlite::params;

use super::{Ledger, Row, RowPricing, UsageError};

/// Token details needed by gateway-key budgets and usage reporting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageDetails {
    /// Input tokens that were not served from a cache.
    pub uncached_input_tokens: Option<i64>,
    /// Prompt tokens served from a cache.
    pub cache_read_tokens: Option<i64>,
    /// Total prompt tokens written into a cache (five-minute plus one-hour).
    pub cache_write_tokens: Option<i64>,
    /// Prompt cache writes at the five-minute TTL.
    pub cache_write_5m_tokens: Option<i64>,
    /// Prompt cache writes at the one-hour TTL.
    pub cache_write_1h_tokens: Option<i64>,
    /// Output tokens the upstream counted as reasoning.
    pub reasoning_tokens: Option<i64>,
    /// The stream carried reasoning but the usage block did not count it.
    pub reasoning_hidden: bool,
}

/// One key's forwarded calls and token sums at or after a window start.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KeyUsage {
    /// Number of forwarded calls.
    pub calls: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens, reasoning included in the protocol's output count.
    pub output_tokens: u64,
    /// Cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cache-write tokens (five-minute and one-hour combined).
    pub cache_write_tokens: u64,
    /// Sum of stored USD costs for priced rows.
    pub cost_usd: f64,
    /// Token-bearing rows without a stored list price.
    pub unpriced: u64,
}

impl KeyUsage {
    /// Tokens counted by a limit with the selected cache-read policy.
    #[must_use]
    pub fn tokens(self, cache_reads: bool) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(if cache_reads {
                self.cache_read_tokens
            } else {
                0
            })
    }
}

impl Ledger {
    /// Inserts a row with cache-aware budget details.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the insert fails.
    pub fn record_with_details(
        &self,
        row: &Row,
        ttft_ms: Option<i64>,
        details: UsageDetails,
        pricing: Option<RowPricing>,
    ) -> Result<(), UsageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        connection
            .execute(
                "INSERT INTO requests (
                    created_unix, model, upstream, input_tokens, output_tokens, latency_ms, status,
                    caller_key_id, caller_key_name, caller_issuer_id, ttft_ms,
                    uncached_input_tokens, cache_read_tokens, cache_write_tokens,
                    cache_write_5m_tokens, cache_write_1h_tokens, reasoning_tokens, reasoning_hidden,
                    priced, cost_usd, price_input, price_output, price_cache_read, price_cache_write,
                    price_cache_write_1h
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
                params![
                    row.created_unix,
                    row.model,
                    row.upstream,
                    row.input_tokens,
                    row.output_tokens,
                    row.latency_ms,
                    row.status,
                    row.caller_key_id,
                    row.caller_key_name,
                    row.caller_issuer_id,
                    ttft_ms,
                    details.uncached_input_tokens,
                    details.cache_read_tokens,
                    details.cache_write_tokens,
                    details.cache_write_5m_tokens,
                    details.cache_write_1h_tokens,
                    details.reasoning_tokens,
                    i64::from(details.reasoning_hidden),
                    i64::from(pricing.is_some_and(|pricing| pricing.priced)),
                    pricing.filter(|pricing| pricing.priced).map(|pricing| pricing.cost_usd),
                    pricing.filter(|pricing| pricing.priced).map(|pricing| pricing.price.input),
                    pricing.filter(|pricing| pricing.priced).map(|pricing| pricing.price.output),
                    pricing.filter(|pricing| pricing.priced).map(|pricing| pricing.price.cache_read),
                    pricing.filter(|pricing| pricing.priced).map(|pricing| pricing.price.cache_write),
                    pricing
                        .filter(|pricing| pricing.priced)
                        .map(|pricing| pricing.price.cache_write_1h),
                ],
            )
            .map(|_| ())
            .map_err(|error| UsageError(error.to_string()))
    }

    /// Sums one key's forwarded usage from `start` onward. Local rows that
    /// never reached an upstream do not count; failed upstream calls do.
    /// Calls made by keys issued under this key count too.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the query fails.
    pub fn key_usage(&self, key_id: &str, start: i64) -> Result<KeyUsage, UsageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        connection
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(MAX(COALESCE(uncached_input_tokens, input_tokens), 0)), 0),
                        COALESCE(SUM(MAX(output_tokens, 0)), 0),
                        COALESCE(SUM(MAX(cache_read_tokens, 0)), 0),
                        COALESCE(SUM(MAX(cache_write_tokens, 0)), 0),
                        COALESCE(SUM(CASE WHEN priced = 1 THEN cost_usd ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN priced = 0 AND (MAX(COALESCE(input_tokens, 0), 0) + MAX(COALESCE(output_tokens, 0), 0)) > 0 THEN 1 ELSE 0 END), 0)
                 FROM requests
                 WHERE (caller_key_id = ?1 OR caller_issuer_id = ?1) AND created_unix >= ?2
                   AND (upstream != '' OR status LIKE '2%')",
                params![key_id, start],
                |row| {
                    let calls: i64 = row.get(0)?;
                    let input: i64 = row.get(1)?;
                    let output: i64 = row.get(2)?;
                    let cache_read: i64 = row.get(3)?;
                    let cache_write: i64 = row.get(4)?;
                    let cost: f64 = row.get(5)?;
                    let unpriced: i64 = row.get(6)?;
                    Ok(KeyUsage {
                        calls: u64::try_from(calls).unwrap_or(0),
                        input_tokens: u64::try_from(input).unwrap_or(0),
                        output_tokens: u64::try_from(output).unwrap_or(0),
                        cache_read_tokens: u64::try_from(cache_read).unwrap_or(0),
                        cache_write_tokens: u64::try_from(cache_write).unwrap_or(0),
                        cost_usd: cost,
                        unpriced: u64::try_from(unpriced).unwrap_or(0),
                    })
                },
            )
            .map_err(|error| UsageError(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Row;

    fn row(status: &str, upstream: &str) -> Row {
        Row {
            created_unix: 100,
            model: "demo".to_owned(),
            upstream: upstream.to_owned(),
            input_tokens: Some(20),
            output_tokens: Some(7),
            latency_ms: 1,
            status: status.to_owned(),
            caller_key_id: "key".to_owned(),
            caller_key_name: "Key".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn l3_token_accounting_includes_the_selected_counters() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("usage.sqlite");
        let ledger = Ledger::open(&path).expect("open");
        ledger
            .record_with_details(
                &row("200", "member"),
                None,
                UsageDetails {
                    uncached_input_tokens: Some(12),
                    cache_read_tokens: Some(8),
                    cache_write_tokens: Some(3),
                    cache_write_5m_tokens: Some(3),
                    cache_write_1h_tokens: Some(0),
                    ..UsageDetails::default()
                },
                None,
            )
            .expect("cached call");
        ledger.record(&row("429", "")).expect("local refusal");
        let failed = Row {
            status: "500".to_owned(),
            upstream: "member".to_owned(),
            ..row("200", "member")
        };
        ledger.record(&failed).expect("failed upstream");
        let other = Row {
            caller_key_id: "other".to_owned(),
            ..row("200", "member")
        };
        ledger.record(&other).expect("other key");
        let usage = ledger.key_usage("key", 0).expect("sums");
        assert_eq!(usage.calls, 2);
        assert_eq!(usage.input_tokens, 32);
        assert_eq!(usage.output_tokens, 14);
        assert_eq!(usage.cache_read_tokens, 8);
        assert_eq!(usage.cache_write_tokens, 3);
        assert_eq!(usage.tokens(false), 49);
        assert_eq!(usage.tokens(true), 57);
        assert_eq!(ledger.key_usage("key", 101).expect("next").calls, 0);
        drop(ledger);
        let reopened = Ledger::open(&path).expect("reopen");
        assert_eq!(reopened.key_usage("key", 0).expect("persisted"), usage);
    }
}

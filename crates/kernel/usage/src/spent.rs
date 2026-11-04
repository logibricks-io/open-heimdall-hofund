//! Provider API-key spend totals from priced usage rows.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, TimeZone};

use super::{Ledger, UsageError};

const MEMO_TTL: Duration = Duration::from_secs(5 * 60);

/// Five-byte key fingerprint spend at list prices, memoized for five minutes.
pub struct KeySpent {
    ledger: Arc<Ledger>,
    memo: Mutex<HashMap<(String, i64), (Instant, f64)>>,
}

impl KeySpent {
    /// Shares `ledger` across callers.
    #[must_use]
    pub fn new(ledger: Arc<Ledger>) -> Self {
        Self {
            ledger,
            memo: Mutex::new(HashMap::new()),
        }
    }

    /// Sum of stored USD for `provider_key_id` at or after `since`.
    ///
    /// `since` is `i64::MIN` for a lifetime total. Rows without
    /// `provider_key_id` do not count.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the query fails.
    pub fn usd(&self, provider_key_id: &str, since: i64) -> Result<f64, UsageError> {
        if provider_key_id.is_empty() {
            return Ok(0.0);
        }
        let key = (provider_key_id.to_owned(), since);
        let now = Instant::now();
        if let Ok(memo) = self.memo.lock()
            && let Some((at, total)) = memo.get(&key)
            && now.duration_since(*at) < MEMO_TTL
        {
            return Ok(*total);
        }
        let total = self.sum(provider_key_id, since)?;
        if let Ok(mut memo) = self.memo.lock() {
            memo.insert(key, (now, total));
        }
        Ok(total)
    }

    fn sum(&self, provider_key_id: &str, since: i64) -> Result<f64, UsageError> {
        self.ledger.provider_key_cost(provider_key_id, since)
    }
}

/// Unix second when a spend-cap period starts in the local calendar.
#[must_use]
pub fn spend_cap_since(period: SpendCapPeriod, set_at: i64) -> i64 {
    match period {
        SpendCapPeriod::Total => set_at,
        SpendCapPeriod::Day => {
            crate::window_start(crate::Window::Today, Local::now()).unwrap_or(set_at)
        }
        SpendCapPeriod::Month => month_start_local(Local::now()).unwrap_or(set_at),
    }
}

/// Seconds until the cap's period resets, or `None` for a lifetime cap.
#[must_use]
pub fn spend_cap_retry_after(period: SpendCapPeriod) -> Option<u64> {
    match period {
        SpendCapPeriod::Total => None,
        SpendCapPeriod::Day => {
            let now = Local::now();
            let start = crate::window_start(crate::Window::Today, now)?;
            let next = start + 86_400;
            let now_secs = now.timestamp();
            u64::try_from(next.saturating_sub(now_secs)).ok()
        }
        SpendCapPeriod::Month => {
            let now = Local::now();
            let date = now.date_naive();
            let (year, month) = if date.month() == 12 {
                (date.year() + 1, 1)
            } else {
                (date.year(), date.month() + 1)
            };
            let next_month = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
            let next = now
                .timezone()
                .from_local_datetime(&next_month.and_hms_opt(0, 0, 0)?)
                .earliest()?;
            u64::try_from(next.timestamp().saturating_sub(now.timestamp())).ok()
        }
    }
}

/// Spend-cap window preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpendCapPeriod {
    /// Local calendar day.
    Day,
    /// Local calendar month.
    Month,
    /// Since the cap was set.
    Total,
}

fn month_start_local<Tz: TimeZone>(now: chrono::DateTime<Tz>) -> Option<i64> {
    let date = now.date_naive();
    let first = chrono::NaiveDate::from_ymd_opt(date.year(), date.month(), 1)?;
    now.timezone()
        .from_local_datetime(&first.and_hms_opt(0, 0, 0)?)
        .earliest()
        .map(|start| start.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Row, RowPricing, TokenPrice, UsageDetails};

    fn priced_row(unix: i64, key: &str) -> Row {
        Row {
            created_unix: unix,
            model: "m".to_owned(),
            upstream: format!("p#{key}"),
            input_tokens: Some(1),
            output_tokens: Some(1),
            latency_ms: 1,
            status: "200".to_owned(),
            provider_key_id: key.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn d61_1_key_spent_sums_priced_rows_and_memoizes() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger"));
        let price = RowPricing {
            priced: true,
            cost_usd: 1.25,
            price: TokenPrice::default(),
        };
        ledger
            .record_with_details(
                &priced_row(100, "abc"),
                None,
                UsageDetails::default(),
                Some(price),
            )
            .expect("row");
        let spent = KeySpent::new(Arc::clone(&ledger));
        assert!((spent.usd("abc", 0).expect("sum") - 1.25).abs() < 1e-9);
        assert!((spent.usd("abc", 0).expect("memo") - 1.25).abs() < 1e-9);
        assert_eq!(spent.usd("", 0).expect("empty"), 0.0);
        assert_eq!(spent.usd("abc", 200).expect("window"), 0.0);
    }

    #[test]
    fn d61_2_spend_follows_the_stored_key_id() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger"));
        let price = RowPricing {
            priced: true,
            cost_usd: 2.0,
            price: TokenPrice::default(),
        };
        let mut hashed = priced_row(100, "");
        hashed.upstream = "primary#0123456789".to_owned();
        ledger
            .record_with_details(&hashed, None, UsageDetails::default(), Some(price))
            .expect("hashed");
        ledger
            .record_with_details(
                &priced_row(100, "0123456789"),
                None,
                UsageDetails::default(),
                Some(price),
            )
            .expect("keyed");
        let spent = KeySpent::new(Arc::clone(&ledger));
        assert!((spent.usd("0123456789", 0).expect("keyed") - 2.0).abs() < 1e-9);
        assert_eq!(spent.usd("", 0).expect("empty"), 0.0);
    }
}

//! Gateway-key limit validation and time windows.

use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveTime, TimeZone};

use super::{Key, KeyLimit, SecretError};

/// The greatest number of independent limits one key may hold.
pub const MAX_LIMITS: usize = 5;
/// The longest repeating day cycle.
pub const MAX_DAYS: u32 = 3650;
/// The longest repeating hour cycle.
pub const MAX_HOURS: u32 = 8760;

/// A limit window as Unix-second boundaries. `start` is inclusive and `reset`
/// is the first instant of the next window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitWindow {
    /// First instant whose usage counts.
    pub start: i64,
    /// Instant at which usage starts over.
    pub reset: i64,
}

impl KeyLimit {
    /// Whether either cap holds the key.
    #[must_use]
    pub fn limited(&self) -> bool {
        self.tokens > 0 || self.cost > 0.0
    }

    /// The normalized limit, or `None` when neither cap is set.
    ///
    /// # Errors
    ///
    /// Returns the reference's validation message for an invalid cap, period,
    /// or cycle length.
    pub fn validated(&self) -> Result<Option<Self>, SecretError> {
        if self.cost < 0.0 || !self.cost.is_finite() {
            return Err(limit_error("A limit can't be negative"));
        }
        if !self.limited() {
            return Ok(None);
        }
        let mut out = self.clone();
        if out.period.is_empty() {
            out.period = "day".to_owned();
        }
        match out.period.as_str() {
            "day" | "week" | "month" => {
                out.days = 0;
                out.hours = 0;
            }
            "days" => {
                out.hours = 0;
                if !(1..=MAX_DAYS).contains(&out.days) {
                    return Err(limit_error(format!(
                        "A limit's cycle is a whole number of days from 1 to {MAX_DAYS}"
                    )));
                }
            }
            "hours" => {
                out.days = 0;
                if !(1..=MAX_HOURS).contains(&out.hours) {
                    return Err(limit_error(format!(
                        "A limit's cycle is a whole number of hours from 1 to {MAX_HOURS}"
                    )));
                }
            }
            _ => {
                return Err(limit_error(
                    "A limit's period is day, week, month, every N days or every N hours",
                ));
            }
        }
        out.cost = (out.cost * 10_000.0).round() / 10_000.0;
        Ok(Some(out))
    }

    /// The window phrase used in status and refusal text.
    #[must_use]
    pub fn per(&self) -> String {
        match self.period.as_str() {
            "days" => format!("{}-day cycle", self.days),
            "hours" => format!("{}-hour cycle", self.hours),
            _ => self.period.clone(),
        }
    }

    /// Whether two limits describe the same kind of window.
    #[must_use]
    pub fn same_window(&self, other: &Self) -> bool {
        self.period == other.period && self.days == other.days && self.hours == other.hours
    }

    /// The window containing `now`, using `now`'s zone for calendar periods.
    #[must_use]
    pub fn window<Tz: TimeZone>(&self, now: DateTime<Tz>) -> LimitWindow {
        let timestamp = now.timestamp();
        if let Some(cycle) = self.cycle_seconds() {
            let anchor = self.since.max(0);
            if timestamp < anchor {
                return LimitWindow {
                    start: anchor,
                    reset: anchor.saturating_add(cycle),
                };
            }
            let start = anchor.saturating_add((timestamp - anchor) / cycle * cycle);
            return LimitWindow {
                start,
                reset: start.saturating_add(cycle),
            };
        }
        let mut window = calendar_window(&self.period, now);
        if self.since > window.start && self.since < window.reset {
            window.start = self.since;
        }
        window
    }

    fn cycle_seconds(&self) -> Option<i64> {
        match self.period.as_str() {
            "days" => Some(i64::from(self.days.clamp(1, MAX_DAYS)) * 86_400),
            "hours" => Some(i64::from(self.hours.clamp(1, MAX_HOURS)) * 3_600),
            _ => None,
        }
    }
}

pub(super) fn set_limit(
    key: &mut Key,
    limit: Option<KeyLimit>,
    index: Option<usize>,
    now: i64,
) -> Result<(), SecretError> {
    let limit = limit
        .as_ref()
        .map(KeyLimit::validated)
        .transpose()?
        .flatten();
    let previous: Vec<KeyLimit> = key
        .limits
        .iter()
        .filter(|limit| limit.limited())
        .cloned()
        .collect();
    let mut next = previous.clone();
    match index {
        None => {
            next.clear();
            if let Some(mut limit) = limit {
                limit.since = cycle_since(previous.first(), &limit, now);
                next.push(limit);
            }
        }
        Some(index) if index > next.len() || (index == next.len() && limit.is_none()) => {
            return Err(limit_error("This key has no such limit"));
        }
        Some(index) => match limit {
            None => {
                next.remove(index);
            }
            Some(mut limit) if index == next.len() => {
                if next.len() >= MAX_LIMITS {
                    return Err(limit_error(format!(
                        "A key can have at most {MAX_LIMITS} limits"
                    )));
                }
                limit.since = cycle_since(None, &limit, now);
                next.push(limit);
            }
            Some(mut limit) => {
                limit.since = cycle_since(next.get(index), &limit, now);
                next[index] = limit;
            }
        },
    }
    for (position, limit) in next.iter().enumerate() {
        if next[position + 1..]
            .iter()
            .any(|other| limit.same_window(other))
        {
            return Err(limit_error(format!(
                "This key already has a limit {}: change that one instead",
                limit.per()
            )));
        }
    }
    key.limits = next;
    Ok(())
}

pub(super) fn reset_limits(
    key: &mut Key,
    index: Option<usize>,
    now: i64,
) -> Result<(), SecretError> {
    let mut limits: Vec<KeyLimit> = key
        .limits
        .iter()
        .filter(|limit| limit.limited())
        .cloned()
        .collect();
    if limits.is_empty() {
        return Err(limit_error("This key has no limit to reset"));
    }
    match index {
        Some(index) => {
            let Some(limit) = limits.get_mut(index) else {
                return Err(limit_error("This key has no such limit"));
            };
            limit.since = now;
        }
        None => {
            for limit in &mut limits {
                limit.since = now;
            }
        }
    }
    key.limits = limits;
    Ok(())
}

fn cycle_since(previous: Option<&KeyLimit>, next: &KeyLimit, now: i64) -> i64 {
    let cyclic = next.cycle_seconds().is_some();
    if let Some(previous) = previous
        && previous.same_window(next)
        && (!cyclic || previous.since != 0)
    {
        return previous.since;
    }
    if cyclic { now } else { 0 }
}

fn calendar_window<Tz: TimeZone>(period: &str, now: DateTime<Tz>) -> LimitWindow {
    let zone = now.timezone();
    let date = now.date_naive();
    let first = match period {
        "week" => date - Days::new(u64::from(date.weekday().num_days_from_monday())),
        "month" => NaiveDate::from_ymd_opt(date.year(), date.month(), 1).expect("month first"),
        _ => date,
    };
    let next = match period {
        "week" => first + Days::new(7),
        "month" => {
            let (year, month) = if date.month() == 12 {
                (date.year() + 1, 1)
            } else {
                (date.year(), date.month() + 1)
            };
            NaiveDate::from_ymd_opt(year, month, 1).expect("next month")
        }
        _ => first + Days::new(1),
    };
    LimitWindow {
        start: local_midnight(&zone, first),
        reset: local_midnight(&zone, next),
    }
}

fn local_midnight<Tz: TimeZone>(zone: &Tz, date: NaiveDate) -> i64 {
    let midnight = date.and_time(NaiveTime::MIN);
    zone.from_local_datetime(&midnight)
        .earliest()
        .or_else(|| {
            (1..=24 * 60).find_map(|minutes| {
                zone.from_local_datetime(&(midnight + chrono::Duration::minutes(minutes)))
                    .earliest()
            })
        })
        .map_or_else(
            || date.and_time(NaiveTime::MIN).and_utc().timestamp(),
            |at| at.timestamp(),
        )
}

fn limit_error(message: impl Into<String>) -> SecretError {
    SecretError::Key(message.into())
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;
    use crate::KeyStore;

    fn limit(period: &str) -> KeyLimit {
        KeyLimit {
            period: period.to_owned(),
            tokens: 100,
            ..KeyLimit::default()
        }
    }

    #[test]
    fn l1_limit_validation_bounds_and_messages() {
        let longest = KeyLimit {
            days: MAX_DAYS,
            ..limit("days")
        };
        assert_eq!(
            longest.validated().expect("ten years").expect("cap").days,
            MAX_DAYS
        );
        assert_eq!(
            limit("").validated().expect("default").expect("cap").period,
            "day"
        );
        assert!(KeyLimit::default().validated().expect("no cap").is_none());
        assert_eq!(
            limit("week").validated().expect("week").expect("cap").days,
            0
        );
        let rounded = KeyLimit {
            cost: 1.234_567,
            ..limit("day")
        };
        assert_eq!(
            rounded.validated().expect("valid").expect("cap").cost,
            1.2346
        );
        for (bad, message) in [
            (
                KeyLimit {
                    cost: -1.0,
                    ..limit("day")
                },
                "A limit can't be negative",
            ),
            (
                KeyLimit {
                    cost: f64::NAN,
                    ..limit("day")
                },
                "A limit can't be negative",
            ),
            (
                limit("hour"),
                "A limit's period is day, week, month, every N days or every N hours",
            ),
            (
                limit("days"),
                "A limit's cycle is a whole number of days from 1 to 3650",
            ),
            (
                KeyLimit {
                    hours: MAX_HOURS + 1,
                    ..limit("hours")
                },
                "A limit's cycle is a whole number of hours from 1 to 8760",
            ),
        ] {
            assert_eq!(bad.validated().expect_err("invalid").to_string(), message);
        }

        let dir = tempfile::tempdir().expect("dir");
        let store = KeyStore::beside_token(dir.path().join("token"));
        let key = store.add("Limited", Some("limit-secret-123")).expect("add");
        for (index, period) in ["day", "week", "month"].iter().enumerate() {
            store
                .set_limit(&key.key.id, Some(limit(period)), Some(index), 100)
                .expect("append");
        }
        assert_eq!(
            store
                .set_limit(&key.key.id, Some(limit("week")), Some(3), 100)
                .expect_err("duplicate")
                .to_string(),
            "This key already has a limit week: change that one instead"
        );
        store
            .set_limit(
                &key.key.id,
                Some(KeyLimit {
                    days: 2,
                    ..limit("days")
                }),
                Some(3),
                100,
            )
            .expect("days");
        store
            .set_limit(
                &key.key.id,
                Some(KeyLimit {
                    hours: 5,
                    ..limit("hours")
                }),
                Some(4),
                100,
            )
            .expect("hours");
        assert!(
            store
                .set_limit(
                    &key.key.id,
                    Some(KeyLimit {
                        days: 3,
                        ..limit("days")
                    }),
                    Some(5),
                    100,
                )
                .expect_err("sixth")
                .to_string()
                .contains("at most 5")
        );
    }

    #[test]
    fn l2_calendar_cycles_and_reset_windows() {
        let zone = FixedOffset::east_opt(8 * 3600).expect("zone");
        let december = zone
            .with_ymd_and_hms(2026, 12, 15, 9, 0, 0)
            .single()
            .expect("December");
        assert_eq!(
            limit("month").window(december).reset,
            zone.with_ymd_and_hms(2027, 1, 1, 0, 0, 0)
                .single()
                .expect("New Year")
                .timestamp()
        );
        // The clock set back well before a cycle's start keeps the first window.
        let anchored = KeyLimit {
            hours: 2,
            since: december.timestamp(),
            ..limit("hours")
        };
        let early = december - chrono::Duration::hours(5);
        assert_eq!(
            anchored.window(early),
            LimitWindow {
                start: december.timestamp(),
                reset: december.timestamp() + 2 * 3_600,
            }
        );
        let now = zone
            .with_ymd_and_hms(2026, 10, 4, 23, 0, 0)
            .single()
            .expect("now");
        assert_eq!(
            limit("week").window(now),
            LimitWindow {
                start: zone
                    .with_ymd_and_hms(2026, 9, 28, 0, 0, 0)
                    .single()
                    .expect("Monday")
                    .timestamp(),
                reset: zone
                    .with_ymd_and_hms(2026, 10, 5, 0, 0, 0)
                    .single()
                    .expect("next Monday")
                    .timestamp(),
            }
        );
        let cycle = KeyLimit {
            days: 10,
            since: now.timestamp() - 57 * 86_400 - 3 * 3_600,
            ..limit("days")
        };
        assert_eq!(
            cycle.window(now),
            LimitWindow {
                start: now.timestamp() - 7 * 86_400 - 3 * 3_600,
                reset: now.timestamp() + 3 * 86_400 - 3 * 3_600,
            }
        );
        let hours = KeyLimit {
            hours: 5,
            since: now.timestamp() - 6 * 3_600,
            ..limit("hours")
        };
        assert_eq!(hours.window(now).start, now.timestamp() - 3_600);
        assert_eq!(hours.per(), "5-hour cycle");
        let calendar = KeyLimit {
            since: now.timestamp() - 3_600,
            ..limit("week")
        };
        assert_eq!(calendar.window(now).start, now.timestamp() - 3_600);
        let next_week = now + chrono::Duration::days(7);
        assert_eq!(
            calendar.window(next_week).start,
            limit("week").window(next_week).start
        );
        assert!(
            cycle
                .window(DateTime::from_timestamp(0, 0).expect("epoch"))
                .start
                >= 0
        );

        let dir = tempfile::tempdir().expect("dir");
        let store = KeyStore::beside_token(dir.path().join("token"));
        let key = store.add("Cycle", Some("cycle-secret-123")).expect("add");
        store
            .set_limit(
                &key.key.id,
                Some(KeyLimit {
                    days: 5,
                    since: 1,
                    ..limit("days")
                }),
                None,
                1_000,
            )
            .expect("set");
        store
            .set_limit(
                &key.key.id,
                Some(KeyLimit {
                    days: 5,
                    tokens: 200,
                    ..limit("days")
                }),
                None,
                2_000,
            )
            .expect("change cap");
        let stored = store.list().expect("list").remove(1);
        assert_eq!(
            stored.limits[0].since, 1_000,
            "a cap change keeps its cycle"
        );
        store.reset_limits(&key.key.id, None, 3_000).expect("reset");
        assert_eq!(store.list().expect("list").remove(1).limits[0].since, 3_000);
        store
            .set_limit(&key.key.id, None, None, 4_000)
            .expect("off");
        assert!(store.reset_limits(&key.key.id, None, 5_000).is_err());
    }
}

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_plugin_api::ModelSpec;
use hofund_router::quota::{Allowance, QuotaStore};
use hofund_router::strategy::{Measurements, Needs, Seat};
use hofund_usage::{Ledger, RoutingRow};

use crate::ModelsHost;

const USAGE_HALF_LIFE: f64 = 3600.0;
const SPEED_HALF_LIFE: f64 = 1200.0;
const SPEED_STALE: f64 = 3600.0;
/// How far back recent use is read: past a day, a one-hour half-life
/// leaves under a ten-millionth of what a member served.
const USAGE_HORIZON: i64 = 24 * 3600;
const FRESH_WEEK_PACE: f64 = 100.0 / 168.0;

pub(crate) struct DaemonMeasurements {
    quotas: Arc<QuotaStore>,
    usage: Arc<Ledger>,
    models: Arc<ModelsHost>,
}

impl DaemonMeasurements {
    pub(crate) fn new(
        quotas: Arc<QuotaStore>,
        usage: Arc<Ledger>,
        models: Arc<ModelsHost>,
    ) -> Self {
        Self {
            quotas,
            usage,
            models,
        }
    }
}

impl Measurements for DaemonMeasurements {
    fn weigh(
        &self,
        seats: &mut [Seat],
        wire_models: &[String],
        usage_model: &str,
        needs: Needs,
        now: SystemTime,
    ) {
        if seats.len() != wire_models.len() {
            return;
        }
        let allowances = self
            .quotas
            .allowances(now)
            .into_iter()
            .map(|allowance| (allowance.member.clone(), allowance))
            .collect::<BTreeMap<_, _>>();
        let now_unix = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);
        // Only the strategies that weigh recent use or speed read the
        // ledger, and only as far back as those still count.
        let rows = if needs.usage || needs.speed {
            let horizon = if needs.usage {
                USAGE_HORIZON
            } else {
                SPEED_STALE as i64
            };
            self.usage
                .routing_rows(now_unix.saturating_sub(horizon))
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "routing measurements could not read usage");
                    Vec::new()
                })
        } else {
            Vec::new()
        };
        for (index, seat) in seats.iter_mut().enumerate() {
            if let Some(allowance) = allowances.get(&seat.key) {
                fill_allowance(seat, allowance, &wire_models[index], now);
            } else {
                seat.known = false;
                seat.pace = if seat.account { FRESH_WEEK_PACE } else { 0.0 };
            }
            if needs.usage {
                seat.tokens = tokens_of(&rows, &seat.key, now_unix);
            }
            if needs.speed {
                let speed = speed_of(&rows, &seat.key, usage_model, now_unix);
                seat.ttft = speed.ttft;
                seat.tps = speed.tps;
            }
            if needs.price {
                let listed = if seat.account {
                    None
                } else {
                    crate::listed_model(self.models.cached(&seat.key).models, &wire_models[index])
                };
                (seat.priced, seat.cost, seat.cache_cost) =
                    price_of(seat.free, listed.as_ref().map(ModelSpec::prices));
            }
        }
    }
}

fn fill_allowance(seat: &mut Seat, allowance: &Allowance, model: &str, now: SystemTime) {
    seat.known = !allowance.windows.is_empty();
    seat.learns &= !seat.known;
    seat.used = allowance.used_for_routing(model, now);
    seat.soon = allowance.routing_renewals(model, now);
    seat.lapse = allowance.lapse(model, now);
    seat.pace = allowance.pace(model, now);
    seat.credits = allowance.spends_credits(model);
    if let Some((left, left_in)) = allowance.left(model) {
        seat.left = left;
        seat.left_in = left_in;
    }
}

/// What `member` served lately, every model counted, each answer at least
/// one token, halved for every hour since.
fn tokens_of(rows: &[RoutingRow], member: &str, now: i64) -> f64 {
    rows.iter()
        .filter(|row| row.upstream == member)
        .map(|row| {
            let age = now.saturating_sub(row.created_unix).max(0) as f64;
            let tokens =
                row.input_tokens.unwrap_or(0).max(0) + row.output_tokens.unwrap_or(0).max(0);
            tokens.max(1) as f64 * 2.0_f64.powf(-age / USAGE_HALF_LIFE)
        })
        .sum()
}

#[derive(Default)]
struct Speed {
    ttft: f64,
    tps: f64,
}

/// `member`'s recent first-token latency and output rate for `model`.
fn speed_of(rows: &[RoutingRow], member: &str, model: &str, now: i64) -> Speed {
    let mut out = Speed::default();
    let mut ttft_sum = 0.0;
    let mut ttft_weight = 0.0;
    let mut tps_sum = 0.0;
    let mut tps_weight = 0.0;
    for row in rows
        .iter()
        .filter(|row| row.upstream == member && row.model == model)
    {
        let age = now.saturating_sub(row.created_unix).max(0) as f64;
        let Some(ttft) = row.ttft_ms.filter(|ttft| *ttft > 0 && age <= SPEED_STALE) else {
            continue;
        };
        let weight = 2.0_f64.powf(-age / SPEED_HALF_LIFE);
        ttft_sum += ttft as f64 * weight;
        ttft_weight += weight;
        let writing = row.latency_ms.saturating_sub(ttft);
        if let Some(output) = row
            .output_tokens
            .filter(|output| *output > 0 && writing > 0)
        {
            let tps = output as f64 * 1000.0 / writing as f64;
            tps_sum += tps * weight;
            tps_weight += weight;
        }
    }
    if ttft_weight > 0.0 {
        out.ttft = ttft_sum / ttft_weight;
    }
    if tps_weight > 0.0 {
        out.tps = tps_sum / tps_weight;
    }
    out
}

type Prices = (Option<u64>, Option<u64>, Option<u64>);

fn price_of(free: bool, listed: Option<Prices>) -> (bool, f64, f64) {
    if free {
        return (true, 0.0, 0.0);
    }
    // Priced on its input and output prices; an unlisted cache read costs
    // nothing, as the reference counts it.
    let Some((Some(input), Some(output), cache)) = listed else {
        return (false, 0.0, 0.0);
    };
    (
        true,
        (input as f64 + output as f64) / 1_000_000.0,
        cache.unwrap_or(0) as f64 / 1_000_000.0,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use hofund_control_proto::{Quota, QuotaKind, QuotaSpan};
    use hofund_plugin_api::ModelSpec;
    use hofund_router::quota::{QuotaStore, WindowInfo, quota_time};
    use hofund_router::strategy::{Measurements, Needs, Seat};
    use hofund_usage::{Ledger, Row};

    use super::{DaemonMeasurements, price_of};
    use crate::ModelsHost;

    fn at(seconds: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    }

    fn fixture(dir: &std::path::Path) -> (DaemonMeasurements, Arc<QuotaStore>, Arc<Ledger>) {
        let quotas = Arc::new(QuotaStore::new(dir));
        let usage = Arc::new(Ledger::open(&dir.join("usage.sqlite")).expect("ledger"));
        let models = Arc::new(ModelsHost::new(Duration::ZERO, Duration::ZERO));
        (
            DaemonMeasurements::new(Arc::clone(&quotas), Arc::clone(&usage), models),
            quotas,
            usage,
        )
    }

    fn row(at: i64, member: &str, input: i64, output: i64, latency: i64) -> Row {
        Row {
            created_unix: at,
            model: "demo".to_owned(),
            upstream: member.to_owned(),
            input_tokens: Some(input),
            output_tokens: Some(output),
            latency_ms: latency,
            status: "200".to_owned(),
        }
    }

    #[test]
    fn b2b_usage_measurements_decay_tokens_by_one_hour_per_member() {
        let dir = tempfile::tempdir().expect("dir");
        let now = 1_900_000_000;
        let (measurements, _, usage) = fixture(dir.path());
        usage
            .record(&row(now - 7200, "p", 40, 60, 1))
            .expect("older");
        usage.record(&row(now - 3600, "p", 40, 60, 1)).expect("old");
        usage.record(&row(now, "p", 40, 60, 1)).expect("new");
        usage
            .record(&row(now, "other", 500, 500, 1))
            .expect("other");
        // another model on the same member counts, at least one token
        let mut other_model = row(now, "p", 0, 0, 1);
        other_model.model = "else".to_owned();
        usage.record(&other_model).expect("other model");
        // past the day read, a large count no longer weighs
        usage
            .record(&row(now - 25 * 3600, "p", 100_000_000, 0, 1))
            .expect("past horizon");
        let mut seats = vec![Seat::new("p"), Seat::new("q")];
        let needs = Needs {
            usage: true,
            ..Needs::default()
        };
        measurements.weigh(
            &mut seats,
            &vec!["demo".to_owned(); 2],
            "demo",
            needs,
            at(now),
        );
        assert_eq!(seats[0].tokens, 176.0);
        assert_eq!(seats[1].tokens, 0.0);
        // a strategy that does not weigh use leaves it unread
        let mut seats = vec![Seat::new("p")];
        measurements.weigh(
            &mut seats,
            &["demo".to_owned()],
            "demo",
            Needs::default(),
            at(now),
        );
        assert_eq!(seats[0].tokens, 0.0);
    }

    #[test]
    fn b4d_speed_measurements_use_decayed_ttft_and_tps_per_member_and_go_stale() {
        let dir = tempfile::tempdir().expect("dir");
        let now = 1_900_000_000;
        let (measurements, _, usage) = fixture(dir.path());
        usage
            .record_with_ttft(&row(now - 1200, "p", 0, 100, 2000), Some(1000))
            .expect("old speed");
        usage
            .record_with_ttft(&row(now, "p", 0, 100, 2000), Some(4000))
            .expect("new speed");
        usage
            .record_with_ttft(&row(now - 3601, "p", 0, 1, 100), Some(1))
            .expect("stale speed");
        usage
            .record_with_ttft(&row(now, "other", 0, 100, 2000), Some(50))
            .expect("other speed");
        let mut seats = vec![Seat::new("p"), Seat::new("q")];
        let needs = Needs {
            speed: true,
            ..Needs::default()
        };
        measurements.weigh(
            &mut seats,
            &vec!["demo".to_owned(); 2],
            "demo",
            needs,
            at(now),
        );
        assert!((seats[0].ttft - 3000.0).abs() < 0.001, "{}", seats[0].ttft);
        assert_eq!(seats[0].tps, 100.0);
        assert_eq!(seats[1].ttft, 0.0);
        assert_eq!(seats[1].tps, 0.0);
        let mut seats = vec![Seat::new("p")];
        let needs = Needs {
            usage: true,
            price: true,
            ..Needs::default()
        };
        measurements.weigh(&mut seats, &["demo".to_owned()], "demo", needs, at(now));
        assert_eq!(seats[0].ttft, 0.0);
    }

    #[test]
    fn b5b_price_measurements_use_catalog_units_and_free_zero() {
        let priced = ModelSpec::new("demo", None)
            .expect("model")
            .with_prices(150_000, 600_000, 10_000);
        assert_eq!(price_of(false, Some(priced.prices())), (true, 0.75, 0.01));
        // an unlisted cache read is free; input and output must both be known
        assert_eq!(
            price_of(false, Some((Some(150_000), Some(600_000), None))),
            (true, 0.75, 0.0)
        );
        assert_eq!(
            price_of(false, Some((Some(150_000), None, Some(1)))),
            (false, 0.0, 0.0)
        );
        assert_eq!(
            price_of(false, Some((None, Some(600_000), Some(1)))),
            (false, 0.0, 0.0)
        );
        assert_eq!(price_of(false, None), (false, 0.0, 0.0));
        assert_eq!(price_of(true, None), (true, 0.0, 0.0));
    }

    #[test]
    fn b1e_allowance_measurements_distinguish_accounts_keys_and_unknown_accounts() {
        let dir = tempfile::tempdir().expect("dir");
        let now = 1_900_000_000;
        let (measurements, quotas, _) = fixture(dir.path());
        quotas.record_success(
            Quota {
                provider: "p".to_owned(),
                name: "p".to_owned(),
                kind: QuotaKind::Subscription,
                user: "a".to_owned(),
                ..Quota::default()
            },
            vec![WindowInfo::new(QuotaSpan {
                name: "week".to_owned(),
                used: 25.0,
                remaining: 75.0,
                resets_at: Some(quota_time(at(now + 3600))),
                ..QuotaSpan::default()
            })],
            at(now),
        );
        let mut account = Seat::new("account:p:a");
        account.account = true;
        let key = Seat::new("key");
        let mut unknown = Seat::new("account:p:b");
        unknown.account = true;
        unknown.learns = true;
        let mut unsupported = Seat::new("account:p:c");
        unsupported.account = true;
        let mut seats = vec![account, key, unknown, unsupported];
        measurements.weigh(
            &mut seats,
            &vec!["demo".to_owned(); 4],
            "demo",
            Needs::default(),
            at(now),
        );
        assert!(seats[0].known);
        assert!(!seats[0].learns);
        assert_eq!(seats[0].used, 25.0);
        assert!(!seats[1].known);
        assert!(!seats[1].learns);
        assert!(!seats[2].known);
        assert!(seats[2].learns);
        assert!(!seats[3].learns);
    }
}

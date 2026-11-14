//! Qoder daily campaign check-in.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::json;
use crate::transport::Call;
use chrono::{FixedOffset, TimeZone};
use hofund_plugin_api::{CheckinOutcome, CheckinResult, PluginError};

use super::{CheckinTransport, expired_sign_in, result_failed};

const RECHECK: Duration = Duration::from_mins(30);

fn beijing_offset() -> FixedOffset {
    FixedOffset::east_opt(8 * 60 * 60).expect("Beijing UTC+8 is fixed")
}

/// Beijing calendar day (`YYYY-MM-DD`) for `now`, fixed UTC+8 with no TZ database.
#[must_use]
pub fn beijing_day_key(now: SystemTime) -> String {
    beijing_datetime(now).format("%Y-%m-%d").to_string()
}

/// Next check-in instant: 30 minutes, the Beijing midnight after `now`, and
/// `valid_until`, whichever comes first.
#[must_use]
pub fn qoder_recheck_at(now: SystemTime, valid_until: Option<SystemTime>) -> SystemTime {
    let epoch = unix_secs(now);
    let mut next = epoch.saturating_add(i64::try_from(RECHECK.as_secs()).unwrap_or(30 * 60));
    let midnight = next_beijing_midnight_epoch(epoch);
    next = next.min(midnight);
    if let Some(until) = valid_until {
        let until_epoch = unix_secs(until);
        if until_epoch > epoch {
            next = next.min(until_epoch);
        }
    }
    epoch_to_system(next)
}

fn beijing_datetime(now: SystemTime) -> chrono::DateTime<FixedOffset> {
    let epoch = unix_secs(now);
    beijing_offset()
        .timestamp_opt(epoch, 0)
        .single()
        .expect("Beijing timestamp is in range")
}

fn next_beijing_midnight_epoch(epoch: i64) -> i64 {
    let beijing = beijing_offset()
        .timestamp_opt(epoch, 0)
        .single()
        .expect("Beijing timestamp is in range");
    let date = beijing.date_naive();
    let next = date.succ_opt().expect("date succeeds").and_hms_opt(0, 0, 0);
    beijing_offset()
        .from_local_datetime(&next.expect("midnight exists"))
        .single()
        .expect("Beijing midnight is unambiguous")
        .timestamp()
}

fn with_qoder_recheck(
    result: CheckinResult,
    now: SystemTime,
    valid_until: Option<SystemTime>,
) -> CheckinResult {
    if matches!(result.outcome(), CheckinOutcome::Captcha) {
        return result;
    }
    result.with_recheck_at(qoder_recheck_at(now, valid_until))
}

fn earliest_active_end(daily: &[Campaign], now: SystemTime) -> Option<SystemTime> {
    daily
        .iter()
        .filter(|campaign| active(campaign, now))
        .map(|campaign| qoder_when(campaign.end_at))
        .min_by_key(|end| unix_secs(*end))
}

fn unix_secs(time: SystemTime) -> i64 {
    i64::try_from(
        time.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

fn epoch_to_system(epoch: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(epoch.max(0)).unwrap_or(u64::MAX))
}

/// Which Qoder openapi host an account uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QoderSite {
    /// `openapi.qoder.sh`
    Global,
    /// `openapi.qoder.com.cn`
    China,
}

impl QoderSite {
    fn campaigns_url(self) -> &'static str {
        match self {
            Self::Global => "https://openapi.qoder.sh/sash/api/v1/me/campaigns",
            Self::China => "https://openapi.qoder.com.cn/sash/api/v1/me/campaigns",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct CampaignList {
    campaigns: Vec<Campaign>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Benefit {
    amount: f64,
}

#[derive(Debug, serde::Deserialize)]
struct Campaign {
    #[serde(rename = "campaignId")]
    id: String,
    #[serde(rename = "actionType")]
    action_type: String,
    #[serde(rename = "claimStatus")]
    state: String,
    #[serde(rename = "startAt")]
    start_at: f64,
    #[serde(rename = "endAt")]
    end_at: f64,
    benefit: Benefit,
}

#[derive(Debug, serde::Deserialize)]
struct ClaimBody {
    status: String,
    benefit: Benefit,
    data: Option<Box<ClaimBody>>,
}

/// Lists claimable daily campaigns and POSTs `/claim` for each.
///
/// # Errors
///
/// Returns [`PluginError`] only when the transport fails before a vendor body is read.
pub async fn run<T: CheckinTransport>(
    transport: &T,
    site: QoderSite,
    now: std::time::SystemTime,
) -> Result<CheckinResult, PluginError> {
    let list: CampaignList = match fetch_campaigns(transport, site).await {
        Ok(list) => list,
        Err(error) => {
            return Ok(with_qoder_recheck(result_failed(error), now, None));
        }
    };
    let daily: Vec<_> = list
        .campaigns
        .into_iter()
        .filter(|row| row.action_type == "CLAIM_BENEFIT")
        .collect();
    let valid_until = earliest_active_end(&daily, now);
    let mut credit = 0.0;
    let mut claimed = 0;
    let mut failed = 0;
    let mut why = String::new();
    for campaign in &daily {
        if campaign.state != "CLAIMABLE" || campaign.id.is_empty() {
            continue;
        }
        match claim_one(transport, site, &campaign.id).await {
            Ok(amount) => {
                claimed += 1;
                credit += amount;
            }
            Err(error) => {
                failed += 1;
                why = error;
            }
        }
    }
    if failed > 0 {
        return Ok(with_qoder_recheck(
            CheckinResult::new(CheckinOutcome::Failed, why),
            now,
            valid_until,
        ));
    }
    if claimed > 0 {
        return Ok(with_qoder_recheck(
            CheckinResult::new(CheckinOutcome::Claimed, "").with_rewards(credit, 0),
            now,
            valid_until,
        ));
    }
    if daily.is_empty() {
        return Ok(with_qoder_recheck(
            CheckinResult::new(CheckinOutcome::Inactive, ""),
            now,
            valid_until,
        ));
    }
    for campaign in &daily {
        if campaign.state == "CLAIMED" && active(campaign, now) {
            return Ok(with_qoder_recheck(
                CheckinResult::new(CheckinOutcome::Done, "")
                    .with_rewards(campaign.benefit.amount, 0),
                now,
                valid_until,
            ));
        }
    }
    Ok(with_qoder_recheck(
        CheckinResult::new(CheckinOutcome::Failed, "today's credits aren't offered yet"),
        now,
        valid_until,
    ))
}

async fn fetch_campaigns<T: CheckinTransport>(
    transport: &T,
    site: QoderSite,
) -> Result<CampaignList, String> {
    let call = Call::get(site.campaigns_url())
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    let reply = transport
        .send(call)
        .await
        .map_err(|error| error.to_string())?;
    if expired_sign_in(&reply) {
        return Err("Qoder's sign-in has expired — sign in again".to_owned());
    }
    let body = std::str::from_utf8(reply.body()).unwrap_or("");
    if reply.status() == 404 && body.contains("only chat completions") {
        return Err(
            "the Qoder plugin can't check in: update it to a build that supports check-in"
                .to_owned(),
        );
    }
    if !(200..300).contains(&reply.status()) {
        return Err(format!("HTTP {}", reply.status()));
    }
    json::from_str(body).map_err(|error| error.to_string())
}

async fn claim_one<T: CheckinTransport>(
    transport: &T,
    site: QoderSite,
    campaign_id: &str,
) -> Result<f64, String> {
    let url = format!("{}/{}/claim", site.campaigns_url(), campaign_id);
    let call = Call::post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .bytes(b"{}");
    let reply = transport
        .send(call)
        .await
        .map_err(|error| error.to_string())?;
    if expired_sign_in(&reply) {
        return Err("Qoder's sign-in has expired — sign in again".to_owned());
    }
    let body = std::str::from_utf8(reply.body()).unwrap_or("");
    if !(200..300).contains(&reply.status()) {
        return Err(format!("HTTP {}", reply.status()));
    }
    let mut parsed: ClaimBody = json::from_str(body).map_err(|error| error.to_string())?;
    if let Some(inner) = parsed.data.take() {
        parsed = *inner;
    }
    if parsed.status != "CLAIMED" {
        return Err(format!("the claim answered {:?}", parsed.status));
    }
    Ok(parsed.benefit.amount)
}

fn active(campaign: &Campaign, now: std::time::SystemTime) -> bool {
    let start = qoder_when(campaign.start_at);
    let end = qoder_when(campaign.end_at);
    now >= start && now < end
}

fn qoder_when(value: f64) -> std::time::SystemTime {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn from_vendor_timestamp(value: f64) -> std::time::SystemTime {
        if value > 1e12 {
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(value as u64)
        } else {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(value as u64)
        }
    }
    from_vendor_timestamp(value)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use chrono::TimeZone;
    use hofund_plugin_api::{CheckinOutcome, PluginError};

    use super::{QoderSite, RECHECK, beijing_day_key, qoder_recheck_at, run};
    use crate::checkin::CheckinTransport;
    use crate::transport::{Call, Reply};

    fn beijing_time(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
    ) -> std::time::SystemTime {
        let offset = chrono::FixedOffset::east_opt(8 * 60 * 60).expect("Beijing offset");
        offset
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("fixture time")
            .into()
    }

    struct Mock {
        seen: Arc<Mutex<Vec<String>>>,
        responses: Vec<Reply>,
    }

    impl CheckinTransport for Mock {
        async fn send(&self, call: Call) -> Result<Reply, PluginError> {
            tokio::task::yield_now().await;
            let (uri, _) = call.uri_and_body();
            self.seen.lock().expect("lock").push(uri.to_owned());
            let index = self.seen.lock().expect("lock").len() - 1;
            Ok(self
                .responses
                .get(index)
                .cloned()
                .unwrap_or_else(|| Reply::from_parts(500, b"missing mock response")))
        }
    }

    #[test]
    fn r54_recheck_at_uses_thirty_minute_boundary() {
        let now = beijing_time(2026, 10, 8, 10, 0);
        let at = qoder_recheck_at(now, None);
        assert_eq!(at, now + RECHECK);
    }

    #[test]
    fn r54_recheck_at_uses_beijing_midnight_when_sooner() {
        let now = beijing_time(2026, 10, 8, 23, 50);
        let at = qoder_recheck_at(now, None);
        assert_eq!(at, beijing_time(2026, 10, 9, 0, 0));
        assert!(at.duration_since(now).unwrap() < RECHECK);
    }

    #[test]
    fn r54_recheck_at_honors_valid_until() {
        let now = beijing_time(2026, 10, 8, 10, 0);
        let until = beijing_time(2026, 10, 8, 10, 5);
        let at = qoder_recheck_at(now, Some(until));
        assert_eq!(at, until);
    }

    #[test]
    fn r54_beijing_day_key_matches_utc_plus_eight() {
        let now = beijing_time(2026, 10, 8, 23, 30);
        assert_eq!(beijing_day_key(now), "2026-10-08");
        let utc_same_instant = std::time::UNIX_EPOCH
            + Duration::from_secs(
                beijing_time(2026, 10, 8, 23, 30)
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            );
        assert_eq!(beijing_day_key(utc_same_instant), "2026-10-08");
    }

    #[tokio::test]
    async fn r54_not_offered_yet_sets_recheck_at() {
        let transport = Mock {
            seen: Arc::new(Mutex::new(Vec::new())),
            responses: vec![Reply::from_parts(
                200,
                br#"{"campaigns":[{"campaignId":"daily","actionType":"CLAIM_BENEFIT","claimStatus":"PENDING","startAt":0,"endAt":4102444800,"benefit":{"amount":100}}]}"#,
            )],
        };
        let now = beijing_time(2026, 10, 8, 9, 0);
        let result = run(&transport, QoderSite::Global, now).await.expect("run");
        assert_eq!(result.outcome(), CheckinOutcome::Failed);
        assert_eq!(result.message(), "today's credits aren't offered yet");
        let end = std::time::UNIX_EPOCH + Duration::from_hours(1_139_568);
        assert_eq!(result.recheck_at(), Some(qoder_recheck_at(now, Some(end))));
    }

    #[tokio::test]
    async fn a3b_9_qoder_checkin_lists_campaigns_then_claims() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let transport = Mock {
            seen: Arc::clone(&seen),
            responses: vec![
                Reply::from_parts(
                    200,
                    br#"{"campaigns":[{"campaignId":"daily","actionType":"CLAIM_BENEFIT","claimStatus":"CLAIMABLE","startAt":0,"endAt":4102444800,"benefit":{"amount":100}}]}"#,
                ),
                Reply::from_parts(
                    200,
                    br#"{"status":"CLAIMED","benefit":{"amount":100}}"#,
                ),
            ],
        };
        let now = beijing_time(2026, 10, 8, 12, 0);
        let result = run(&transport, QoderSite::Global, now).await.expect("run");
        assert_eq!(result.outcome(), CheckinOutcome::Claimed);
        assert!(result.recheck_at().is_some());
        let urls = seen.lock().expect("lock");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("openapi.qoder.sh"));
        assert!(urls[0].contains("/campaigns"));
        assert!(urls[1].contains("/claim"));
    }
}

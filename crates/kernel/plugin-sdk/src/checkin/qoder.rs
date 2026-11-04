//! Qoder daily campaign check-in.

use crate::json;
use crate::transport::Call;
use hofund_plugin_api::{CheckinOutcome, CheckinResult, PluginError};

use super::{CheckinTransport, expired_sign_in, result_failed};

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
        Err(error) => return Ok(result_failed(error)),
    };
    let daily: Vec<_> = list
        .campaigns
        .into_iter()
        .filter(|row| row.action_type == "CLAIM_BENEFIT")
        .collect();
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
        return Ok(CheckinResult::new(CheckinOutcome::Failed, why));
    }
    if claimed > 0 {
        return Ok(CheckinResult::new(CheckinOutcome::Claimed, "").with_rewards(credit, 0));
    }
    if daily.is_empty() {
        return Ok(CheckinResult::new(CheckinOutcome::Inactive, ""));
    }
    for campaign in &daily {
        if campaign.state == "CLAIMED" && active(campaign, now) {
            return Ok(CheckinResult::new(CheckinOutcome::Done, "")
                .with_rewards(campaign.benefit.amount, 0));
        }
    }
    Ok(CheckinResult::new(
        CheckinOutcome::Failed,
        "today's credits aren't offered yet",
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

    use hofund_plugin_api::{CheckinOutcome, PluginError};

    use super::{QoderSite, run};
    use crate::checkin::CheckinTransport;
    use crate::transport::{Call, Reply};

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
        let result = run(&transport, QoderSite::Global, std::time::UNIX_EPOCH)
            .await
            .expect("run");
        assert_eq!(result.outcome(), CheckinOutcome::Claimed);
        let urls = seen.lock().expect("lock");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("openapi.qoder.sh"));
        assert!(urls[0].contains("/campaigns"));
        assert!(urls[1].contains("/claim"));
    }
}

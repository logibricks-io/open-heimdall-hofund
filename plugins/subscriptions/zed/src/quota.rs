//! Plan and billing period from `/client/users/me`.

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::time::parse_rfc3339;

use crate::creds::{self, Stored};
use crate::errors;
use crate::headers;
use crate::org;

pub(crate) async fn windows(
    host: &hofund_plugin_api::HostContext,
    endpoints: std::sync::Arc<crate::Endpoints>,
    stored: &Stored,
) -> Result<Vec<QuotaWindow>, PluginError> {
    let url = format!("{}/client/users/me", endpoints.cloud);
    let mut call = hofund_plugin_sdk::Call::get(url)
        .header("Accept", "application/json")
        .sensitive_header("authorization", creds::auth_header(&stored.credential));
    for (name, value) in headers::cloud_headers() {
        call = call.header(name, value);
    }
    if !stored.credential.system_id.is_empty() {
        call = call.header("x-zed-system-id", stored.credential.system_id.clone());
    }
    let reply = call.fetch(host.http()).await?;
    match reply.status() {
        401 | 403 => {
            return Err(errors::sign_in_expired(&creds::label(&stored.credential)));
        }
        200..=299 => {}
        status => {
            return Err(PluginError::status_error(
                status,
                "Zed did not return account details",
            ));
        }
    }
    let me = org::parse_me(reply.body())?;
    let plan = org::plan_of(&me, &stored.credential.org);
    if me
        .get("plan")
        .and_then(|p| p.get("has_overdue_invoices"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Err(PluginError::quota(
            "Zed: this account has an overdue invoice, so its models are paused (see zed.dev/account)",
        ));
    }
    let label = if plan == "zed_free" || plan.is_empty() {
        "No plan".to_owned()
    } else {
        org::plan_name(&plan)
    };
    let resets = me
        .get("plan")
        .and_then(|p| p.get("subscription_period"))
        .and_then(|p| p.get("ended_at"))
        .and_then(Value::as_str)
        .and_then(parse_rfc3339);
    Ok(vec![
        QuotaWindow::new(label, 0, None, resets)?.with_blocking(false),
    ])
}

//! Config routing labels `qoder` and `qoder-cn` over one account store.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SignInCtx};

use crate::creds::{self, CN_SITE, Site};

/// The site a routing label selects when `site=` is not set.
#[must_use]
pub(crate) fn site_for_routing_label(label: &str) -> Option<Site> {
    match label {
        "qoder" => Some(Site::Global),
        CN_SITE => Some(Site::China),
        _ => None,
    }
}

/// Site for sign-in: explicit `site=` wins, then the routing label, then Global.
#[must_use]
pub(crate) fn sign_in_site(ctx: &SignInCtx) -> Site {
    match ctx.option("site") {
        Some(CN_SITE | "cn" | "china") => Site::China,
        Some("qoder" | "global") => Site::Global,
        Some(_) => Site::Global,
        None => site_for_routing_label(ctx.plugin().as_str()).unwrap_or(Site::Global),
    }
}

/// Whether `account` is stored for `routing_id`.
pub(crate) async fn serves_routing_label(
    routing_id: &str,
    account: &AccountId,
    host: &HostContext,
) -> Result<bool, PluginError> {
    let Some(expected) = site_for_routing_label(routing_id) else {
        return Ok(true);
    };
    let stored = creds::Stored::load(host, account).await?;
    Ok(stored.credential.site == expected)
}

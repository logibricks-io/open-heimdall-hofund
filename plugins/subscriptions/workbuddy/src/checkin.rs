//! Daily check-in for WorkBuddy China accounts.

use std::sync::Arc;

use hofund_plugin_api::{AccountId, CheckinResult, HostContext, PluginError};
use hofund_plugin_sdk::checkin::{CheckinTransport, WorkBuddySite, run_workbuddy};
use hofund_plugin_sdk::transport::{Call, Reply};

use crate::Endpoints;
use crate::creds;
use crate::refresh;
use crate::site::Site;

struct Transport<'a> {
    host: &'a HostContext,
    credential: &'a creds::Credential,
    base: String,
}

impl CheckinTransport for Transport<'_> {
    async fn send(&self, call: Call) -> Result<Reply, PluginError> {
        creds::apply_signed_headers(call, self.credential, &self.base)
            .fetch(self.host.http())
            .await
    }
}

pub(crate) async fn run(
    account: &AccountId,
    host: &HostContext,
    endpoints: Arc<Endpoints>,
) -> Result<CheckinResult, PluginError> {
    let stored = refresh::ensure_fresh(host, Arc::clone(&endpoints), account).await?;
    if stored.credential.site != Site::China || stored.credential.api_key {
        return Ok(CheckinResult::new(
            hofund_plugin_api::CheckinOutcome::Inactive,
            "",
        ));
    }
    let transport = Transport {
        host,
        credential: &stored.credential,
        base: endpoints.base_for(stored.credential.site).to_owned(),
    };
    run_workbuddy(&transport, WorkBuddySite::China).await
}

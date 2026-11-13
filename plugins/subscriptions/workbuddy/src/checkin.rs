//! Daily check-in for WorkBuddy China accounts.

use std::sync::Arc;

use hofund_plugin_api::{AccountId, CheckinResult, HostContext, PluginError};
use hofund_plugin_sdk::checkin::{CheckinTransport, WorkBuddySite, run_workbuddy};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::transport::{Call, Reply};

use crate::Endpoints;
use crate::creds;
use crate::refresh;
use crate::site::Site;

struct Transport<'a> {
    host: &'a HostContext,
    credential: &'a creds::Credential,
}

impl CheckinTransport for Transport<'_> {
    async fn send(&self, mut call: Call) -> Result<Reply, PluginError> {
        call = call
            .sensitive_header(
                "authorization",
                format!("Bearer {}", secret_text(&self.credential.access)),
            )
            .header("X-User-Id", self.credential.uid.clone())
            .header("User-Agent", self.credential.site.user_agent());
        call.fetch(self.host.http()).await
    }
}

pub(crate) async fn run(
    account: &AccountId,
    host: &HostContext,
    endpoints: Arc<Endpoints>,
) -> Result<CheckinResult, PluginError> {
    let stored = refresh::ensure_fresh(host, endpoints, account).await?;
    if stored.credential.site != Site::China {
        return Ok(CheckinResult::new(
            hofund_plugin_api::CheckinOutcome::Inactive,
            "",
        ));
    }
    let transport = Transport {
        host,
        credential: &stored.credential,
    };
    run_workbuddy(&transport, WorkBuddySite::China).await
}

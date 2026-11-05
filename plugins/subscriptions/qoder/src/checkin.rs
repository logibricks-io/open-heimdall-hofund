//! Daily check-in through the shared Qoder helper.

use hofund_plugin_api::{AccountId, CheckinResult, HostContext, PluginError};
use hofund_plugin_sdk::checkin::{CheckinTransport, QoderSite, run_qoder};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::transport::{Call, Reply};

use crate::creds::{Site, Stored};

struct Transport<'a> {
    host: &'a HostContext,
    token: String,
}

impl CheckinTransport for Transport<'_> {
    async fn send(&self, mut call: Call) -> Result<Reply, PluginError> {
        call = call.sensitive_header("authorization", format!("Bearer {}", self.token));
        call.fetch(self.host.http()).await
    }
}

pub(crate) async fn run(
    account: &AccountId,
    host: &HostContext,
) -> Result<CheckinResult, PluginError> {
    let stored = Stored::load(host, account).await?;
    let site = match stored.credential.site {
        Site::Global => QoderSite::Global,
        Site::China => QoderSite::China,
    };
    let transport = Transport {
        host,
        token: secret_text(&stored.credential.token).to_owned(),
    };
    run_qoder(&transport, site, host.clock().now()).await
}

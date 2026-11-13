//! Daily check-in through the shared Qoder helper.

use std::sync::Arc;

use hofund_plugin_api::{AccountId, CheckinResult, HostContext, PluginError};
use hofund_plugin_sdk::checkin::{CheckinTransport, QoderSite, run_qoder};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::transport::{Call, Reply};

use crate::Endpoints;
use crate::creds::Site;
use crate::identity::{self, Devices, NativeIdentity};
use crate::refresh;

struct Transport<'a> {
    host: &'a HostContext,
    token: String,
    identity: Option<NativeIdentity>,
}

impl CheckinTransport for Transport<'_> {
    async fn send(&self, call: Call) -> Result<Reply, PluginError> {
        identity::apply_checkin(call, self.identity.as_ref())
            .sensitive_header("authorization", format!("Bearer {}", self.token))
            .fetch(self.host.http())
            .await
    }
}

pub(crate) async fn run(
    account: &AccountId,
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    devices: Arc<Devices>,
) -> Result<CheckinResult, PluginError> {
    let stored = refresh::ensure_fresh(host, endpoints, account).await?;
    let site = match stored.credential.site {
        Site::Global => QoderSite::Global,
        Site::China => QoderSite::China,
    };
    let identity = devices.checkin_identity(host, &stored.credential).await?;
    let transport = Transport {
        host,
        token: secret_text(&stored.credential.token).to_owned(),
        identity,
    };
    run_qoder(&transport, site, host.clock().now()).await
}

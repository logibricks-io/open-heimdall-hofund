//! Daily check-in through the shared Qoder helper.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    public_openapi: String,
    account_openapi: String,
    refused: AtomicBool,
}

impl CheckinTransport for Transport<'_> {
    async fn send(&self, mut call: Call) -> Result<Reply, PluginError> {
        if self.public_openapi != self.account_openapi {
            let uri = call.uri().to_owned();
            let path = uri
                .strip_prefix(self.public_openapi.trim_end_matches('/'))
                .ok_or_else(|| {
                    PluginError::config("Qoder check-in URL is outside the account site")
                })?;
            call = call.with_uri(format!(
                "{}{path}",
                self.account_openapi.trim_end_matches('/')
            ));
        }
        let reply = identity::apply_checkin(call, self.identity.as_ref())
            .sensitive_header("authorization", format!("Bearer {}", self.token))
            .fetch(self.host.http())
            .await?;
        if matches!(reply.status(), 401 | 403) {
            self.refused.store(true, Ordering::Relaxed);
        }
        Ok(reply)
    }
}

pub(crate) async fn run(
    account: &AccountId,
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    devices: Arc<Devices>,
) -> Result<CheckinResult, PluginError> {
    let mut stored = refresh::ensure_fresh(host, Arc::clone(&endpoints), account).await?;
    let site = match stored.credential.site {
        Site::Global => QoderSite::Global,
        Site::China => QoderSite::China,
    };
    for attempt in 0..2 {
        let resolved = endpoints.resolve(&stored.credential)?;
        let identity = devices.checkin_identity(host, &stored.credential).await?;
        let transport = Transport {
            host,
            token: if secret_text(&stored.credential.device_token).is_empty() {
                secret_text(&stored.credential.token).to_owned()
            } else {
                secret_text(&stored.credential.device_token).to_owned()
            },
            identity,
            public_openapi: match stored.credential.site {
                Site::Global => "https://openapi.qoder.sh".to_owned(),
                Site::China => "https://openapi.qoder.com.cn".to_owned(),
            },
            account_openapi: resolved.openapi,
            refused: AtomicBool::new(false),
        };
        let result = run_qoder(&transport, site, host.clock().now()).await?;
        if !transport.refused.load(Ordering::Relaxed) || attempt == 1 {
            return Ok(result);
        }
        stored = refresh::force_device(host, Arc::clone(&endpoints), account).await?;
    }
    Err(PluginError::internal(
        "Qoder check-in retry ended unexpectedly",
    ))
}

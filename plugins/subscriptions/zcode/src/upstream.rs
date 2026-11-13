//! Anthropic Messages upstream on coding-plan and Start Plan base URLs.

use std::path::PathBuf;
use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, SecretString, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::TokenCache;
use hofund_plugin_sdk::wire::{self, Target};

use crate::session::{self, Stored};
use crate::{APP_VERSION, Endpoints, api, gift, plan_auth_style, start};

pub(crate) struct ZcodeUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    device_file: PathBuf,
    team_keys: Arc<TokenCache<SecretString>>,
}

impl ZcodeUpstream {
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        device_file: PathBuf,
        team_keys: Arc<TokenCache<SecretString>>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            device_file,
            team_keys,
        }
    }

    async fn stored(&self) -> Result<session::Stored, PluginError> {
        session::load(
            &self.host,
            &self.endpoints,
            &self.account,
            &self.device_file,
            &self.team_keys,
        )
        .await
    }

    async fn target(
        &self,
        stored: &Stored,
        request: &Request,
    ) -> Result<(Target, SecretString, String), PluginError> {
        let trial = gift::model_for_trial(&request.model);
        let start_only = stored.auth.api_key.is_none() && !stored.auth.team();
        let mut use_start = start_only || trial.is_some();
        if !use_start && stored.auth.jwt.is_some() {
            use_start = !self.has_coding_plan(stored).await;
        }
        let session = start::session_id(&stored.device);
        let mut model = request.model.clone();
        if use_start {
            let jwt = stored.auth.jwt.as_ref().ok_or_else(|| {
                PluginError::config("Z Code gift models require a Z Code browser sign-in")
            })?;
            gift::ensure_live_jwt(&self.host, jwt)?;
            if let Some(raw) = trial {
                let balance =
                    gift::read(&self.host, &self.endpoints, stored, &self.device_file).await?;
                let available = balance
                    .usable_models
                    .iter()
                    .any(|model| model.eq_ignore_ascii_case(&raw));
                if !available {
                    return Err(PluginError::status_error(
                        429,
                        format!(
                            "the Z Code limited quota for {raw} is exhausted or temporarily unavailable"
                        ),
                    ));
                }
                model = gift::canonical_model(&raw);
            }
            let device = stored.device.clone();
            let site = stored.auth.site;
            let now = self.host.clock().now();
            let rewrite = Arc::new(
                move |root: &mut hofund_plugin_sdk::json::Map<
                    String,
                    hofund_plugin_sdk::json::Value,
                >| {
                    start::reasoning(root);
                    start::dress(root, site, &device, &session, now);
                    Vec::new()
                },
            );
            let mut headers = start::headers()?;
            headers.push(("x-session-id".to_owned(), start::session_id(&stored.device)));
            let target = Target::new(ProtocolId::ANTHROPIC_MESSAGES, self.endpoints.start_base())
                .credential(CredentialStyle::Bearer)
                .headers(headers)
                .rewrite(rewrite);
            return Ok((target, jwt.clone(), model));
        }
        let device = stored.device.clone();
        let rewrite = Arc::new(
            move |root: &mut hofund_plugin_sdk::json::Map<
                String,
                hofund_plugin_sdk::json::Value,
            >| {
                start::reasoning(root);
                start::mark_plan(root, &device, &session);
                Vec::new()
            },
        );
        let target = Target::new(
            ProtocolId::ANTHROPIC_MESSAGES,
            stored.auth.base.trim_end_matches('/').to_owned(),
        )
        .credential(plan_auth_style())
        .headers(vec![
            ("user-agent".to_owned(), format!("ZCode/{APP_VERSION}")),
            ("x-session-id".to_owned(), start::session_id(&stored.device)),
        ])
        .rewrite(rewrite);
        Ok((target, stored.api_key.clone(), request.model.clone()))
    }

    async fn has_coding_plan(&self, stored: &Stored) -> bool {
        let Ok(parsed) = hofund_plugin_sdk::url::Url::parse(&stored.auth.base) else {
            return true;
        };
        let Some(host) = parsed.host_str() else {
            return true;
        };
        let root = format!("{}://{host}", parsed.scheme());
        let url = format!("{root}/api/biz/subscription/list");
        let Ok(data) = api::get_envelope(&api::EnvelopeRequest::get(
            &self.host,
            &self.endpoints,
            &url,
            Some(&stored.api_key),
            &[],
            &self.device_file,
        ))
        .await
        else {
            return true;
        };
        data.as_array().is_some_and(|plans| {
            plans.iter().any(|plan| {
                plan.get("status")
                    .and_then(hofund_plugin_sdk::json::Value::as_str)
                    .is_some_and(|status| status.eq_ignore_ascii_case("VALID"))
            })
        })
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for ZcodeUpstream {
    async fn execute(
        &self,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        if inbound != ProtocolId::ANTHROPIC_MESSAGES {
            return Err(PluginError::config(
                "Z Code upstream only serves Anthropic Messages on this path",
            ));
        }
        let stored = self.stored().await?;
        let (target, credential, model) = self.target(&stored, &request).await?;
        request.model = model;
        let result = wire::execute(
            self.host.http(),
            &target,
            Some(&credential),
            request,
            inbound,
        )
        .await
        .map_err(crate::http_auth::classify_upstream_error);
        if let Err(error) = &result
            && matches!(error.kind(), hofund_plugin_api::PluginErrorKind::Auth)
            && stored.auth.team()
            && stored.auth.api_key.is_none()
        {
            session::drop_team_key_cache(&self.team_keys, &self.account);
        }
        result
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}

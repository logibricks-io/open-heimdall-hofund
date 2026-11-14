//! Anthropic Messages upstream on coding-plan and Start Plan base URLs.

use std::path::PathBuf;
use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, BodyStream, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery,
    ProbeReport, ProtocolId, Request, SecretString, UpstreamProvider,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::TokenCache;
use hofund_plugin_sdk::wire::{self, Target};

use crate::routing::{GiftRoute, Route, Routing};
use crate::session::{self, Stored};
use crate::{APP_VERSION, Endpoints, api, gift, http_auth, plan_auth_style, start, tool_search};

pub(crate) struct ZcodeUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    device_file: PathBuf,
    team_keys: Arc<TokenCache<SecretString>>,
    free_first: bool,
    routing: Arc<Routing>,
}

struct Selection {
    target: Target,
    credential: SecretString,
    model: String,
    kind: SelectionKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionKind {
    Plan,
    StartOnly,
    ForcedGift,
    FreeGift,
}

impl ZcodeUpstream {
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        device_file: PathBuf,
        team_keys: Arc<TokenCache<SecretString>>,
        free_first: bool,
        routing: Arc<Routing>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            device_file,
            team_keys,
            free_first,
            routing,
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

    async fn route(&self, stored: &Stored) -> Result<Route, PluginError> {
        if stored.auth.jwt.is_none() {
            return Ok(Route {
                start: false,
                gift: None,
            });
        }
        if stored.auth.api_key.is_none() && !stored.auth.team() {
            return Ok(Route {
                start: true,
                gift: None,
            });
        }
        let now = self.host.clock().monotonic();
        if let Some(route) = self.routing.cached(&self.account, stored, now) {
            return Ok(route);
        }
        let previous_plan = self.routing.previously_had_coding_plan(&self.account);
        let mut certain = true;
        let start = if stored.auth.team() {
            false
        } else {
            match self.has_coding_plan(stored).await {
                Ok(has_plan) => {
                    if !has_plan && previous_plan {
                        certain = false;
                    }
                    !has_plan
                }
                Err(_) => {
                    certain = false;
                    false
                }
            }
        };
        let gift = if start {
            None
        } else {
            match gift::read(&self.host, &self.endpoints, stored, &self.device_file).await {
                Ok(balance) if balance.has_plan => Some(GiftRoute::new(balance.usable_models)),
                Ok(_) => None,
                Err(_) => {
                    certain = false;
                    None
                }
            }
        };
        let route = Route { start, gift };
        self.routing
            .remember(&self.account, stored, now, route.clone(), certain);
        Ok(route)
    }

    async fn target(&self, stored: &Stored, request: &Request) -> Result<Selection, PluginError> {
        let rename_search = tool_search::needed(request);
        let trial = gift::model_for_trial(&request.model);
        let route = self.route(stored).await?;
        let now = self.host.clock().monotonic();
        let mut model = request.model.clone();
        let kind = if let Some(raw) = trial {
            let gift = match route.gift {
                Some(gift) => gift,
                None => {
                    let balance =
                        gift::read(&self.host, &self.endpoints, stored, &self.device_file).await?;
                    GiftRoute::new(balance.usable_models)
                }
            };
            if !gift.serves(&raw) || self.routing.gift_rested(&self.account, &raw, now) {
                return Err(PluginError::status_error(
                    429,
                    format!(
                        "the Z Code limited quota for {raw} is exhausted or temporarily unavailable"
                    ),
                ));
            }
            model = gift::canonical_model(&raw);
            SelectionKind::ForcedGift
        } else if route.start {
            SelectionKind::StartOnly
        } else if self.free_first
            && route.gift.as_ref().is_some_and(|gift| {
                gift.serves(&request.model)
                    && !self.routing.gift_rested(&self.account, &request.model, now)
                    && !self
                        .routing
                        .free_first_sidelined(&self.account, &request.model, now)
            })
            && stored
                .auth
                .jwt
                .as_ref()
                .is_some_and(|jwt| gift::ensure_live_jwt(&self.host, jwt).is_ok())
        {
            SelectionKind::FreeGift
        } else {
            SelectionKind::Plan
        };
        if kind == SelectionKind::Plan {
            let (target, credential) = self.plan_target(stored, rename_search);
            return Ok(Selection {
                target,
                credential,
                model,
                kind,
            });
        }
        let (target, credential) = self.start_target(stored, rename_search)?;
        Ok(Selection {
            target,
            credential,
            model,
            kind,
        })
    }

    fn start_target(
        &self,
        stored: &Stored,
        rename_search: bool,
    ) -> Result<(Target, SecretString), PluginError> {
        let jwt = stored.auth.jwt.as_ref().ok_or_else(|| {
            PluginError::config("Z Code gift models require a Z Code browser sign-in")
        })?;
        gift::ensure_live_jwt(&self.host, jwt)?;
        let device = stored.device.clone();
        let site = stored.auth.site;
        let session = start::session_id(&stored.device);
        let now = self.host.clock().now();
        let rewrite = Arc::new(
            move |root: &mut hofund_plugin_sdk::json::Map<
                String,
                hofund_plugin_sdk::json::Value,
            >| {
                tool_search::request(root);
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
        let target = if rename_search {
            target.unwrap(Arc::new(tool_search::response))
        } else {
            target
        };
        Ok((target, jwt.clone()))
    }

    fn plan_target(&self, stored: &Stored, rename_search: bool) -> (Target, SecretString) {
        let device = stored.device.clone();
        let session = start::session_id(&stored.device);
        let rewrite = Arc::new(
            move |root: &mut hofund_plugin_sdk::json::Map<
                String,
                hofund_plugin_sdk::json::Value,
            >| {
                tool_search::request(root);
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
        let target = if rename_search {
            target.unwrap(Arc::new(tool_search::response))
        } else {
            target
        };
        (target, stored.api_key.clone())
    }

    async fn has_coding_plan(&self, stored: &Stored) -> Result<bool, PluginError> {
        let Ok(parsed) = hofund_plugin_sdk::url::Url::parse(&stored.auth.base) else {
            return Ok(true);
        };
        let Some(host) = parsed.host_str() else {
            return Ok(true);
        };
        let root = format!("{}://{host}", parsed.scheme());
        let url = format!("{root}/api/biz/subscription/list");
        let data = api::get_envelope(&api::EnvelopeRequest::get(
            &self.host,
            &self.endpoints,
            &url,
            Some(&stored.api_key),
            &[],
            &self.device_file,
        ))
        .await?;
        Ok(data.as_array().is_some_and(|plans| {
            plans.iter().any(|plan| {
                plan.get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status.eq_ignore_ascii_case("VALID"))
            })
        }))
    }

    async fn send(
        &self,
        target: &Target,
        credential: &SecretString,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        wire::execute(self.host.http(), target, Some(credential), request, inbound).await
    }

    async fn replay_on_plan(
        &self,
        stored: &Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let rename_search = tool_search::needed(&request);
        let (target, credential) = self.plan_target(stored, rename_search);
        self.send(&target, &credential, request, inbound).await
    }
}

fn spent_up(status: u16, error: &PluginError) -> bool {
    !http_auth::busy_error(error) && status == 429 || spent_codes(error.message())
}

fn spent_body(body: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    spent_codes(text)
}

fn spent_codes(text: &str) -> bool {
    let Ok(body) = hofund_plugin_sdk::json::from_str::<Value>(text) else {
        return false;
    };
    [
        body.get("code"),
        body.get("error")
            .and_then(Value::as_object)
            .and_then(|error| error.get("code")),
    ]
    .into_iter()
    .flatten()
    .filter_map(|code| http_auth::parse_envelope_code(Some(code)))
    .any(|code| matches!(code, 1005 | 1113))
}

fn start_refused(error: &PluginError) -> bool {
    if error.status().is_none_or(|status| status < 400) {
        return false;
    }
    let text = error.message().to_ascii_lowercase();
    error.status() == Some(405)
        || text.contains("unusual activity")
        || text.contains("request has been blocked")
        || error
            .message()
            .split(|character: char| !character.is_ascii_digit())
            .any(|digits| digits == "3012")
}

async fn unary_spent(completion: Completion) -> Result<(Completion, bool), PluginError> {
    let Completion::Raw(mut body) = completion else {
        return Ok((completion, false));
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        bytes.extend_from_slice(&chunk?);
    }
    let spent = spent_body(&bytes);
    Ok((Completion::Raw(BodyStream::from_bytes(bytes)), spent))
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
        let original = request.clone();
        let selection = self.target(&stored, &request).await?;
        request.model = selection.model;
        let streaming = request.stream;
        let mut result = self
            .send(&selection.target, &selection.credential, request, inbound)
            .await;
        let now = self.host.clock().monotonic();
        match selection.kind {
            SelectionKind::FreeGift => {
                let mut replay = false;
                match result {
                    Err(error) => {
                        let status = error.status().unwrap_or(0);
                        let spent = spent_up(status, &error);
                        let refused = start_refused(&error);
                        if spent || refused {
                            self.routing
                                .rest_gift(&self.account, &original.model, now, spent);
                        } else if status == 0 || matches!(status, 401 | 403) {
                            self.routing.invalidate(&self.account);
                        }
                        result = Err(error);
                        replay = true;
                    }
                    Ok(completion) if !streaming => {
                        let (completion, spent) = unary_spent(completion).await?;
                        result = Ok(completion);
                        if spent {
                            self.routing
                                .rest_gift(&self.account, &original.model, now, true);
                            replay = true;
                        }
                    }
                    Ok(completion) => result = Ok(completion),
                }
                if replay {
                    result = self.replay_on_plan(&stored, original, inbound).await;
                }
            }
            SelectionKind::ForcedGift => match result {
                Err(error) => {
                    if spent_up(error.status().unwrap_or(0), &error) {
                        self.routing
                            .rest_gift(&self.account, &original.model, now, true);
                    }
                    result = Err(error);
                }
                Ok(completion) if !streaming => {
                    let (completion, spent) = unary_spent(completion).await?;
                    result = if spent {
                        self.routing
                            .rest_gift(&self.account, &original.model, now, true);
                        let Completion::Raw(body) = completion else {
                            unreachable!("unary spent returns a raw body");
                        };
                        let mut bytes = Vec::new();
                        let mut body = body;
                        while let Some(chunk) = body.next_chunk().await {
                            bytes.extend_from_slice(&chunk?);
                        }
                        Err(PluginError::status_error(
                            429,
                            String::from_utf8_lossy(&bytes).into_owned(),
                        ))
                    } else {
                        Ok(completion)
                    };
                }
                Ok(completion) => result = Ok(completion),
            },
            SelectionKind::Plan | SelectionKind::StartOnly => {}
        }
        let result = result.map_err(crate::http_auth::classify_upstream_error);
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

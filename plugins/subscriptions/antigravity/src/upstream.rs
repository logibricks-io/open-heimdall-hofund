//! Antigravity requests on Code Assist's daily `v1internal` endpoint.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, ContentPart, CredentialStyle, HostContext, PluginError, ProbeQuery,
    ProbeReport, ProtocolId, Request, Role, SecretString, UpstreamProvider,
};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::{TokenCache, new_state};
use hofund_plugin_sdk::wire::{self, Target};

use crate::{Endpoints, client, models, project, tokens};

const ROOT: &str = "v1internal";
const ZERO_WIDTH: &str = "\u{200b}";
const TURNED_AWAY_LINES: &[&str] = &[
    "x-anthropic-billing-header:",
    "built on anthropic's claude agent sdk",
    "you are codex, an agent based on gpt-5",
    "you are codex, a coding agent based on gpt-5",
    "hermes agent, an intelligent ai assistant created by nous research",
];
const TURNED_AWAY_HINT: &str = "Antigravity answers this 429 to the system prompt of Claude Code and the Claude Agent SDK (Claude Desktop's chats) whatever quota is left, so it is not a quota and waiting won't help; use another provider for these chats, or put one after Antigravity in a routing group";
const CODEX_HINT: &str = "Antigravity answers this 429 to Codex's own system prompt for GPT-5 models whatever quota is left, so it is not a quota and waiting won't help; a Codex chat begun on a GPT model keeps that prompt after a model switch or a fork, so start a new chat on this model";
const HERMES_HINT: &str = "Antigravity answers this 429 to Hermes Agent's earlier default persona (its SOUL.md) whatever quota is left, so it is not a quota and waiting won't help; Hermes' current default persona goes through, so put it in SOUL.md, or use another provider for these chats";

pub(crate) struct AntigravityUpstream {
    pub(crate) account: AccountId,
    pub(crate) host: HostContext,
    pub(crate) endpoints: Arc<Endpoints>,
    pub(crate) oauth: Arc<client::OauthClient>,
    pub(crate) tokens: Arc<TokenCache<SecretString>>,
    pub(crate) catalog: Arc<models::Catalog>,
    pub(crate) project: Option<String>,
}

impl AntigravityUpstream {
    async fn send(
        &self,
        access: &SecretString,
        project_id: &str,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let effort = request.params.effort.map(|effort| effort.openai_label());
        let model = self
            .catalog
            .sent_id(&self.account, bare(&request.model), effort);
        let request_id = format!("agent-{}", new_state()?);
        let session = session_id(&request)?;
        let mut request = cloak_request(request);
        request.model = model.clone();
        let envelope_project = project_id.to_owned();
        let target = Target::new(
            ProtocolId::GENERATIVE_LANGUAGE,
            self.endpoints.code_assist.clone(),
        )
        .credential(CredentialStyle::Bearer)
        .method_root(ROOT)
        .headers(vec![("user-agent".to_owned(), project::user_agent())])
        .rewrite(Arc::new(move |body| {
            hofund_plugin_sdk::code_assist::shape_code_assist(body, &model, true);
            hofund_plugin_sdk::plain_code_assist_tools(body);
            envelope(body, &model, &envelope_project, &request_id, &session);
            Vec::new()
        }))
        .unwrap(Arc::new(lift));
        wire::execute(self.host.http(), &target, Some(access), request, inbound).await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for AntigravityUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let original = request.clone();
        let access = tokens::access(
            &self.host,
            &self.endpoints,
            &self.oauth,
            &self.tokens,
            &self.account,
        )
        .await?;
        let project_id = project::resolve(
            &self.host,
            &self.endpoints,
            &self.account,
            &access,
            self.project.as_deref(),
        )
        .await?;
        let result = match self
            .send(&access, &project_id, request.clone(), inbound)
            .await
        {
            Err(error) if error.status() == Some(401) => {
                self.tokens.invalidate_if(&self.account, |_| true).await;
                let renewed = tokens::access(
                    &self.host,
                    &self.endpoints,
                    &self.oauth,
                    &self.tokens,
                    &self.account,
                )
                .await?;
                self.send(&renewed, &project_id, request, inbound).await
            }
            other => other,
        };
        result.map_err(|error| classify_turned_away(&original, error))
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let mut request = query.minimal_request();
        request.stream = true;
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}

pub(crate) fn bare(model: &str) -> &str {
    let model = model.trim();
    model.strip_prefix("models/").unwrap_or(model)
}

pub(crate) fn envelope(
    body: &mut Map<String, Value>,
    model: &str,
    project: &str,
    request_id: &str,
    session: &str,
) {
    let mut request = std::mem::take(body);
    request.remove("model");
    request.insert("sessionId".to_owned(), Value::String(session.to_owned()));
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert("project".to_owned(), Value::String(project.to_owned()));
    body.insert(
        "userAgent".to_owned(),
        Value::String("antigravity".to_owned()),
    );
    body.insert("requestType".to_owned(), Value::String("agent".to_owned()));
    body.insert("requestId".to_owned(), Value::String(request_id.to_owned()));
    body.insert("request".to_owned(), Value::Object(request));
}

pub(crate) fn lift(value: Value) -> Value {
    match value {
        Value::Object(mut map) if map.contains_key("response") => {
            map.remove("response").unwrap_or_default()
        }
        other => other,
    }
}

fn session_id(request: &Request) -> Result<String, PluginError> {
    let mut first = request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .find_map(|part| match part {
            ContentPart::Text(text) if !text.text.is_empty() => Some(text.text.clone()),
            _ => None,
        });
    if first.is_none() {
        first = Some(new_state()?);
    }
    let digest = sha256(first.unwrap_or_default().as_bytes());
    let number = u64::from_be_bytes(digest[..8].try_into().expect("eight digest bytes"))
        & 0x7fff_ffff_ffff_ffff;
    Ok(format!("-{number}"))
}

fn cloak_request(mut request: Request) -> Request {
    for message in &mut request.messages {
        if !matches!(message.role, Role::System | Role::Developer) {
            continue;
        }
        for part in &mut message.content {
            if let ContentPart::Text(text) = part {
                text.text = cloak(&text.text);
            }
        }
    }
    request
}

pub(crate) fn cloak(system: &str) -> String {
    let lower = system.to_ascii_lowercase();
    let mut at = Vec::new();
    for line in TURNED_AWAY_LINES {
        let mut from = 0;
        while let Some(found) = lower[from..].find(line) {
            let start = from + found;
            at.push(start + 1);
            from = start + line.len();
        }
    }
    at.sort_unstable();
    at.dedup();
    let mut out = String::with_capacity(system.len() + at.len() * ZERO_WIDTH.len());
    let mut previous = 0;
    for index in at {
        out.push_str(&system[previous..index]);
        out.push_str(ZERO_WIDTH);
        previous = index;
    }
    out.push_str(&system[previous..]);
    out
}

fn turns_away(system: &str) -> bool {
    let system = system.to_ascii_lowercase();
    TURNED_AWAY_LINES.iter().any(|line| system.contains(line))
}

fn original_system(request: &Request) -> String {
    request
        .messages
        .iter()
        .filter(|message| matches!(message.role, Role::System | Role::Developer))
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn classify_turned_away(request: &Request, error: PluginError) -> PluginError {
    let system = original_system(request);
    if !matches!(error.status(), Some(429 | 502))
        || !turns_away(&system)
        || !error
            .message()
            .to_ascii_lowercase()
            .contains("resource has been exhausted")
    {
        if error.status() == Some(429) && error.kind() != hofund_plugin_api::PluginErrorKind::Quota
        {
            let retry = error.retry_after();
            let error = PluginError::quota_status(429, error.message());
            return match retry {
                Some(after) => error.with_retry_after(after),
                None => error,
            };
        }
        return error;
    }
    let lower = system.to_ascii_lowercase();
    let hint = if lower.contains("you are codex, an agent based on gpt-5")
        || lower.contains("you are codex, a coding agent based on gpt-5")
    {
        CODEX_HINT
    } else if lower.contains("hermes agent, an intelligent ai assistant created by nous research") {
        HERMES_HINT
    } else {
        TURNED_AWAY_HINT
    };
    PluginError::status_error(400, format!("Antigravity: {} — {hint}", error.message()))
        .without_cooldown()
}

#[cfg(test)]
mod tests;

//! The Copilot API as an OpenAI chat completions upstream.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, ContentPart, CredentialStyle, HostContext, PluginError, ProbeQuery,
    ProbeReport, ProtocolId, Request, Role, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::{TokenCache, same_secret};
use hofund_plugin_sdk::wire::{self, Target};

use crate::session::{self, EDITOR_HEADERS, Session};

/// The GitHub API version Copilot chat calls name.
const API_VERSION: &str = "2026-01-09";

pub(crate) struct CopilotUpstream {
    account: AccountId,
    host: HostContext,
    sessions: Arc<TokenCache<Session>>,
}

impl CopilotUpstream {
    pub(crate) const fn new(
        account: AccountId,
        host: HostContext,
        sessions: Arc<TokenCache<Session>>,
    ) -> Self {
        Self {
            account,
            host,
            sessions,
        }
    }

    async fn send(
        &self,
        session: &Session,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let target = Target::new(ProtocolId::OPENAI_CHAT, session.base.clone())
            .credential(CredentialStyle::Bearer)
            .headers(request_headers(&request));
        wire::execute(
            self.host.http(),
            &target,
            Some(&session.token),
            request,
            inbound,
        )
        .await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for CopilotUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let first = session::session(&self.sessions, &self.host, &self.account).await?;
        match self.send(&first, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) => {
                // The session token was revoked before its expiry: trade
                // again once, unless another caller already did.
                self.sessions
                    .invalidate_if(&self.account, |cached| {
                        same_secret(&cached.token, &first.token)
                    })
                    .await;
                let renewed = session::session(&self.sessions, &self.host, &self.account).await?;
                self.send(&renewed, request, inbound).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}

/// The headers a Copilot chat call carries, besides the bearer token.
pub(crate) fn request_headers(request: &Request) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = EDITOR_HEADERS
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    headers.push(("X-GitHub-Api-Version".to_owned(), API_VERSION.to_owned()));
    headers.push(("Openai-Intent".to_owned(), "conversation-panel".to_owned()));
    headers.push(("X-Initiator".to_owned(), initiator(request).to_owned()));
    if has_image(request) {
        headers.push(("Copilot-Vision-Request".to_owned(), "true".to_owned()));
    }
    headers
}

/// `user` when the last message is the user typing, else `agent`. A user
/// message that only returns tool results is the agent continuing.
pub(crate) fn initiator(request: &Request) -> &'static str {
    let typed = request.messages.last().is_some_and(|message| {
        message.role == Role::User
            && !message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::ToolResult(_)))
    });
    if typed { "user" } else { "agent" }
}

/// Whether any message carries an image, including inside a tool result.
pub(crate) fn has_image(request: &Request) -> bool {
    request.messages.iter().any(|message| {
        message.content.iter().any(|part| match part {
            ContentPart::Image(_) => true,
            ContentPart::ToolResult(result) => result
                .content
                .iter()
                .any(|inner| matches!(inner, ContentPart::Image(_))),
            _ => false,
        })
    })
}

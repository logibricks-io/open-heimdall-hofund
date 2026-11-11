//! The Copilot API as an upstream: Chat Completions, Responses, or
//! Messages, as the account's model list says each model is served.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, ContentPart, CredentialStyle, HostContext, PluginError, ProbeQuery,
    ProbeReport, ProtocolId, Request, Role, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::{TokenCache, same_secret, secret_text};
use hofund_plugin_sdk::wire::{self, Target};

use crate::auto::{self, AutoSession};
use crate::models::{self, Book, Listed};
use crate::session::{self, Session};

/// The GitHub API version Copilot chat calls name.
const API_VERSION: &str = "2026-01-09";

pub(crate) struct CopilotUpstream {
    account: AccountId,
    host: HostContext,
    sessions: Arc<TokenCache<Session>>,
    book: Arc<Book>,
}

impl CopilotUpstream {
    pub(crate) const fn new(
        account: AccountId,
        host: HostContext,
        sessions: Arc<TokenCache<Session>>,
        book: Arc<Book>,
    ) -> Self {
        Self {
            account,
            host,
            sessions,
            book,
        }
    }

    /// What the account's model list says, listing it once when the plugin
    /// has not yet. A list that fails leaves every model on Chat
    /// Completions; the request's own answer then says what is wrong.
    async fn listed(&self, session: &Session) -> Listed {
        if let Some(listed) = self.book.get(&self.account) {
            return listed;
        }
        let _ = models::list(&self.host, session, &self.account, &self.book).await;
        self.book.get(&self.account).unwrap_or_default()
    }

    /// Resolves Auto and picks the protocol for `request`, accepting terms
    /// when the concrete model needs it.
    async fn prepare(
        &self,
        session: &Session,
        request: &Request,
        inbound: ProtocolId,
    ) -> Result<Prepared, PluginError> {
        let listed = self.listed(session).await;
        let mut request = request.clone();
        let auto = if request.model == auto::AUTO_ID {
            let picked = auto::resolve(
                &self.host,
                session,
                &self.account,
                &self.book,
                &listed,
                &request,
            )
            .await?;
            request.model = picked.model.clone();
            Some(picked)
        } else {
            None
        };
        if listed.waits(&request.model) {
            models::accept(
                &self.host,
                session,
                &self.account,
                &self.book,
                &request.model,
            )
            .await;
        }
        let protocol = auto.as_ref().map_or_else(
            || listed.protocol(&request.model, inbound),
            |picked| picked.protocol(inbound),
        );
        Ok(Prepared {
            request,
            protocol,
            auto,
        })
    }

    async fn send(
        &self,
        session: &Session,
        protocol: ProtocolId,
        request: Request,
        inbound: ProtocolId,
        auto: Option<&AutoSession>,
    ) -> Result<Completion, PluginError> {
        let mut headers = session_request_headers(&request, session);
        if let Some(token) = auto.and_then(|picked| picked.token.as_ref()) {
            headers.push((
                "Copilot-Session-Token".to_owned(),
                secret_text(token).to_owned(),
            ));
        }
        let target = Target::new(protocol, session.base.clone())
            .credential(CredentialStyle::Bearer)
            .headers(headers);
        wire::execute(
            self.host.http(),
            &target,
            Some(&session.token),
            request,
            inbound,
        )
        .await
    }

    async fn send_with_remint(
        &self,
        first: &Session,
        prepared: Prepared,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        match self
            .send(
                first,
                prepared.protocol,
                prepared.request.clone(),
                inbound,
                prepared.auto.as_ref(),
            )
            .await
        {
            Err(error) if error.status() == Some(401) => {
                self.sessions
                    .invalidate_if(&self.account, |cached| {
                        same_secret(&cached.token, &first.token)
                    })
                    .await;
                let renewed = session::session(&self.sessions, &self.host, &self.account).await?;
                self.send(
                    &renewed,
                    prepared.protocol,
                    prepared.request,
                    inbound,
                    prepared.auto.as_ref(),
                )
                .await
            }
            other => other,
        }
    }
}

struct Prepared {
    request: Request,
    protocol: ProtocolId,
    auto: Option<AutoSession>,
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for CopilotUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let first = session::session(&self.sessions, &self.host, &self.account).await?;
        let mut request = request;
        let mut last_error = None;
        for _ in 0..4 {
            let prepared = self.prepare(&first, &request, inbound).await?;
            let automatic = prepared.auto.is_some();
            let session_token = prepared
                .auto
                .as_ref()
                .and_then(|picked| picked.token.clone());
            let model = prepared.request.model.clone();
            match self.send_with_remint(&first, prepared, inbound).await {
                Err(error)
                    if automatic && session_token.is_some() && auto::session_gone(&error) =>
                {
                    auto::forget_session(
                        &self.book,
                        &self.account,
                        session_token.as_ref().expect("checked session token"),
                    );
                    request.model = auto::AUTO_ID.to_owned();
                    last_error = Some(error);
                }
                Err(error) if auto::model_refusal(&error) => {
                    auto::refuse(&self.book, &self.account, &model, self.host.clock().now());
                    if !automatic {
                        return Err(error);
                    }
                    request.model = auto::AUTO_ID.to_owned();
                    last_error = Some(error);
                }
                other => return other,
            }
        }
        Err(last_error.unwrap_or_else(|| {
            PluginError::status_error(400, "Copilot Auto could not pick a served model")
        }))
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}

/// The headers a Copilot chat call carries, besides the bearer token.
#[cfg(test)]
pub(crate) fn request_headers(request: &Request) -> Vec<(String, String)> {
    headers_with_client(request, &crate::session::EDITOR_HEADERS, true)
}

fn session_request_headers(request: &Request, session: &Session) -> Vec<(String, String)> {
    headers_with_client(request, session.headers(), !session.direct)
}

fn headers_with_client(
    request: &Request,
    client: &[(&str, &str)],
    editor_version: bool,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = client
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    if editor_version {
        headers.push(("X-GitHub-Api-Version".to_owned(), API_VERSION.to_owned()));
    }
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

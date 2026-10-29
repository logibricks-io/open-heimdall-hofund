//! The ChatGPT Codex backend as an OpenAI Responses upstream.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::wire::{self, Target};

use crate::tokens::{self, Access, Need};
use crate::{CODEX_VERSION, Endpoints};

/// Body fields the Codex backend refuses or ignores, as the reference drops
/// them. `store` is dropped too: the Responses codec then writes
/// `"store": false`, which the backend requires.
const DROPPED_FIELDS: [&str; 9] = [
    "max_output_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "previous_response_id",
    "user",
    "safety_identifier",
    "prompt_cache_options",
    "store",
];

pub(crate) struct CodexUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl CodexUpstream {
    pub(crate) const fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
        }
    }

    async fn send(
        &self,
        access: &Access,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let target = Target::new(ProtocolId::OPENAI_RESPONSES, self.endpoints.base.clone())
            .credential(CredentialStyle::Bearer)
            .headers(headers(access.account.as_deref()));
        wire::execute(
            self.host.http(),
            &target,
            Some(&access.token),
            request,
            inbound,
        )
        .await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for CodexUpstream {
    async fn execute(
        &self,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        if !request.stream {
            return Err(PluginError::unsupported(
                "the ChatGPT Codex backend only streams; send a streaming request",
            ));
        }
        shape(&mut request);
        let access =
            tokens::access(&self.host, &self.endpoints, &self.account, Need::IfExpiring).await?;
        match self.send(&access, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) && token_expired(error.message()) => {
                // The backend called the token expired before its `exp`:
                // refresh once and send again.
                let renewed = tokens::access(
                    &self.host,
                    &self.endpoints,
                    &self.account,
                    Need::Replace(access.token),
                )
                .await?;
                self.send(&renewed, request, inbound).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let mut request = query.minimal_request();
        request.stream = true;
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}

/// Drops what the Codex backend refuses from `request`.
pub(crate) fn shape(request: &mut Request) {
    request.params.temperature = None;
    request.params.top_p = None;
    request.params.max_tokens = None;
    for field in DROPPED_FIELDS {
        request.extensions.remove(field);
        request.params.extensions.remove(field);
    }
}

/// The headers Codex CLI sends, besides the bearer token.
pub(crate) fn headers(account: Option<&str>) -> Vec<(String, String)> {
    let mut headers = Vec::with_capacity(6);
    if let Some(account) = account.filter(|account| !account.is_empty()) {
        headers.push(("chatgpt-account-id".to_owned(), account.to_owned()));
    }
    headers.extend(
        [
            ("OpenAI-Beta", "responses=experimental".to_owned()),
            ("originator", "codex_cli_rs".to_owned()),
            ("version", CODEX_VERSION.to_owned()),
            (
                "User-Agent",
                user_agent(std::env::consts::OS, std::env::consts::ARCH),
            ),
            ("Accept", "text/event-stream".to_owned()),
        ]
        .map(|(name, value)| (name.to_owned(), value)),
    );
    headers
}

/// The Codex CLI User-Agent for `os` on `arch`.
pub(crate) fn user_agent(os: &str, arch: &str) -> String {
    let os = match os {
        "macos" => "Mac OS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    // The reference names Apple silicon and ARM Linux `arm64`.
    let arch = match arch {
        "aarch64" => "arm64",
        other => other,
    };
    let terminal = std::env::var("TERM_PROGRAM")
        .ok()
        .filter(|term| !term.is_empty() && term.is_ascii())
        .unwrap_or_else(|| "unknown".to_owned());
    format!("codex_cli_rs/{CODEX_VERSION} ({os}; {arch}) {terminal}")
}

/// Says an error message is the backend calling the access token expired.
/// `refresh_token_expired` does not count: that sign-in is gone.
pub(crate) fn token_expired(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("authentication token is expired")
        || lower.contains("authentication token has expired")
        || lower.contains("please try refreshing it")
        || has_word(&lower, "token_expired")
}

fn has_word(text: &str, word: &str) -> bool {
    let word_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let bytes = text.as_bytes();
    text.match_indices(word).any(|(start, found)| {
        let end = start + found.len();
        let before = start == 0 || !word_byte(bytes[start - 1]);
        let after = end == bytes.len() || !word_byte(bytes[end]);
        before && after
    })
}

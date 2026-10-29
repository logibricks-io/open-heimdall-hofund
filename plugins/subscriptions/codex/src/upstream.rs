//! The ChatGPT Codex backend as an OpenAI Responses upstream.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::wire::{self, Target};

use crate::models::{self, Prompts};
use crate::tokens::{self, Access, Need};
use crate::{CODEX_VERSION, Endpoints, limits, shape};

/// Body fields the Codex backend refuses or ignores, dropped before
/// encoding; [`shape::body`] reshapes what the codec then writes.
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
    prompts: Arc<Prompts>,
}

impl CodexUpstream {
    pub(crate) const fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        prompts: Arc<Prompts>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            prompts,
        }
    }

    /// The instructions Codex CLI sends `model` with: kept from a model
    /// list, else from the account's list asked for now, at most once per
    /// [`models::ASK_AGAIN`].
    async fn instructions(&self, access: &Access, model: &str) -> Option<String> {
        if let Some(text) = self.prompts.get(&self.host, model) {
            return Some(text);
        }
        if !self
            .prompts
            .may_ask(&self.account, self.host.clock().monotonic())
        {
            return None;
        }
        // A list that fails leaves the client's instructions in place.
        let _ = models::list(&self.host, &self.endpoints, access, &self.prompts).await;
        self.prompts.get(&self.host, model)
    }

    /// Sends `request` as a stream and records the usage limits the reply
    /// tells. A client that asked for one reply gets the stream folded
    /// into one.
    async fn send(
        &self,
        access: &Access,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let codex = self.instructions(access, &request.model).await;
        let target = Target::new(ProtocolId::OPENAI_RESPONSES, self.endpoints.base.clone())
            .credential(CredentialStyle::Bearer)
            .headers(headers(access.account.as_deref()))
            .rewrite(Arc::new(move |body| shape::body(body, codex.as_deref())));
        let unary = !request.stream;
        request.stream = true;
        let sent = wire::send(
            self.host.http(),
            &target,
            Some(&access.token),
            request,
            inbound,
        )
        .await?;
        let credits = limits::header_credits(sent.headers());
        limits::observe(
            &self.host,
            &limits::header_limits(sent.headers(), self.host.clock().now()),
            credits,
        );
        let host = self.host.clone();
        let sent = sent.watch(
            limits::SCAN,
            Arc::new(move |data| limits::watch(&host, data, credits)),
        );
        if unary {
            sent.fold().await
        } else {
            sent.complete().await
        }
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for CodexUpstream {
    async fn execute(
        &self,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
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

/// A `GET` of `url` as Codex CLI sends one for the account: the bearer
/// token and [`headers`], asking for JSON.
pub(crate) fn account_call(url: String, access: &Access) -> Call {
    headers(access.account.as_deref())
        .into_iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("accept"))
        .fold(
            Call::get(url).sensitive_header(
                "authorization",
                format!("Bearer {}", secret_text(&access.token)),
            ),
            |call, (name, value)| call.sensitive_header(name, value),
        )
        .header("accept", "application/json")
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

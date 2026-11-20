//! The agy prompt bridge as an upstream.
//!
//! Every inbound shape is answered from the CLI's JSON, so the gateway's
//! codecs are not involved. A scripted child stands in for `agy`.

use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use hofund_plugin_api::{
    AccountId, BlockStart, Completion, ContentBlockStart, ContentBlockStop, ContentPart,
    Extensions, HostContext, Message, MessageDelta, MessageStart, MessageStop, PluginError,
    ProbeQuery, ProbeReport, Request, Response, Role, Stop, StopReason, StreamEvent, TextDelta,
    ToolArguments, ToolArgumentsDelta, ToolCallPart, UpstreamProvider,
};
use hofund_plugin_sdk::json;

use crate::cli::{self, PromptReply};

pub(crate) struct AgyUpstream {
    account: AccountId,
    host: HostContext,
    command: Option<PathBuf>,
}

impl AgyUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext, command: Option<PathBuf>) -> Self {
        Self {
            account,
            host,
            command,
        }
    }

    async fn reply(&self, request: &Request) -> Result<PromptReply, PluginError> {
        let stored = load(&self.host, &self.account).await?;
        let program = cli::program(self.command.as_ref())?;
        let provider = cli::read_provider_proxy(&self.host)?;
        let via = cli::via(&stored.proxy, &provider);
        let body = json::json!({
            "model": request.model,
            "stream": request.stream,
            "text": prompt_text(request),
            "tools": request.tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
        });
        cli::prompt(&self.host, &program, &stored.home, via, &body).await
    }
}

struct Stored {
    home: PathBuf,
    proxy: String,
}

async fn load(host: &HostContext, account: &AccountId) -> Result<Stored, PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth("agy account is missing"));
    };
    let object = hofund_plugin_sdk::oauth::SecretObject::parse(&secret)?;
    let home = object
        .text("home")
        .ok_or_else(|| PluginError::auth("agy account has no home"))?;
    Ok(Stored {
        home: PathBuf::from(home),
        proxy: object.text("proxy").unwrap_or("").to_owned(),
    })
}

fn prompt_text(request: &Request) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn completion(reply: PromptReply, model: &str, stream: bool) -> Completion {
    if stream {
        Completion::Stream(Box::pin(EventList {
            events: events(reply, model).into_iter(),
        }))
    } else {
        Completion::Unary(Box::new(unary(reply, model)))
    }
}

fn unary(reply: PromptReply, model: &str) -> Response {
    let mut message = Message::new(Role::Assistant);
    let reason = if let Some(tool) = reply.tool {
        message.content.push(ContentPart::ToolCall(ToolCallPart {
            id: tool.id,
            name: tool.name,
            arguments: ToolArguments::from_text(&tool.arguments),
            extensions: Extensions::new(),
        }));
        StopReason::ToolUse
    } else {
        message.content.push(ContentPart::text(reply.text));
        StopReason::EndTurn
    };
    Response::new(message)
        .with_model(Some(model.to_owned()))
        .with_stop(Some(Stop {
            reason,
            sequence: None,
        }))
}

fn events(reply: PromptReply, model: &str) -> Vec<Result<StreamEvent, PluginError>> {
    let mut events = vec![Ok(StreamEvent::MessageStart(MessageStart {
        id: None,
        model: Some(model.to_owned()),
        usage: None,
        extensions: Extensions::new(),
    }))];
    let (block, delta, reason) = if let Some(tool) = reply.tool {
        (
            BlockStart::ToolCall {
                id: tool.id,
                name: tool.name,
            },
            StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index: 0,
                partial_json: tool.arguments,
                extensions: Extensions::new(),
            }),
            StopReason::ToolUse,
        )
    } else {
        (
            BlockStart::Text,
            StreamEvent::TextDelta(TextDelta {
                index: 0,
                text: reply.text,
                extensions: Extensions::new(),
            }),
            StopReason::EndTurn,
        )
    };
    events.push(Ok(StreamEvent::ContentBlockStart(ContentBlockStart {
        index: 0,
        block,
        extensions: Extensions::new(),
    })));
    events.push(Ok(delta));
    events.push(Ok(StreamEvent::ContentBlockStop(ContentBlockStop {
        index: 0,
        extensions: Extensions::new(),
    })));
    events.push(Ok(StreamEvent::MessageDelta(MessageDelta {
        stop: Some(Stop {
            reason,
            sequence: None,
        }),
        usage: None,
        extensions: Extensions::new(),
    })));
    events.push(Ok(StreamEvent::MessageStop(MessageStop {
        extensions: Extensions::new(),
    })));
    events
}

struct EventList {
    events: std::vec::IntoIter<Result<StreamEvent, PluginError>>,
}

impl hofund_plugin_sdk::futures::Stream for EventList {
    type Item = Result<StreamEvent, PluginError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().events.next())
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for AgyUpstream {
    async fn execute(
        &self,
        request: Request,
        _inbound: hofund_plugin_api::ProtocolId,
    ) -> Result<Completion, PluginError> {
        let model = request.model.clone();
        let stream = request.stream;
        let reply = self.reply(&request).await?;
        Ok(completion(reply, &model, stream))
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let mut request = query.minimal_request();
        request.stream = false;
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}

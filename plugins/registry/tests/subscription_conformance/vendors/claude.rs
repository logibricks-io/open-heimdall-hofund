use hofund_kit_subscription::PipedScript;
use hofund_plugin_api::{RawTable, RawValue};
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_claude::ClaudeFactory;

use crate::harness::{execute_async, host, seed, subscription};
use crate::matrix::Cell;

const CREDENTIALS: &str = r#"{"claudeAiOauth":{"accessToken":"matrix-access","refreshToken":"matrix-refresh","expiresAt":4000000000000,"scopes":["user:inference"],"subscriptionType":"max"}}"#;
const TEXT: &str =
    include_str!("../../../../subscriptions/claude/src/bridge/fixtures/stream/message_delta.jsonl");
const TOOL: &str =
    include_str!("../../../../subscriptions/claude/src/bridge/fixtures/stream/tool_use.jsonl");

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("claude", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_text("credentials", CREDENTIALS),
    )
    .await;
    let fixture = if cell.tools { TOOL } else { TEXT };
    host.process().script_piped(
        PipedScript::new().on_stdin(move |_, _| fixture.lines().map(str::to_owned).collect()),
    );
    let mut raw = RawTable::new();
    raw.insert(
        "command",
        RawValue::String(if cfg!(windows) {
            r"C:\claude\bin\claude.exe".to_owned()
        } else {
            "/opt/claude/bin/claude".to_owned()
        }),
    );
    let subscription = subscription(&ClaudeFactory, &host, raw);
    execute_async(&subscription, &account, &host, cell).await
}

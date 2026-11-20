use hofund_plugin_api::{RawTable, RawValue};
use hofund_plugin_subscription_agy_cli::matrix_sign_in;

use crate::harness::{execute_async, host, seed_as, subscription};
use crate::matrix::Cell;

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("agy-cli", &dir);
    let home = dir.path().join("agy-home");
    let (account, blob) =
        matrix_sign_in(&home, "matrix@example.test").map_err(|error| error.to_string())?;
    seed_as(&host, account.clone(), blob).await;
    host.process().script_piped(hofund_kit_subscription::PipedScript::new().on_stdin(
        move |_, stdin| {
            let text = String::from_utf8_lossy(stdin);
            if text.contains("lookup") {
                vec![
                    r#"{"tool":{"name":"lookup","id":"call-1","arguments":"{\"query\":\"fixture\"}"}}"#
                        .to_owned(),
                ]
            } else {
                vec![r#"{"text":"matrix-ok"}"#.to_owned()]
            }
        },
    ));
    let mut raw = RawTable::new();
    raw.insert("command", RawValue::String("/opt/agy/agy".to_owned()));
    let subscription = subscription(
        &hofund_plugin_subscription_agy_cli::AgyCliFactory,
        &host,
        raw,
    );
    execute_async(&subscription, &account, &host, cell).await
}

use hofund_plugin_api::{RawTable, RawValue};
use hofund_plugin_subscription_grok::{Endpoints, factory_with_endpoints, matrix_sign_in};

use crate::harness::{execute_async, host, seed_as, subscription, wire_response};
use crate::matrix::{Cell, Shape};

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("grok", &dir);
    let home = dir.path().join("grok-home");
    let (account, blob) =
        matrix_sign_in(&home, "matrix-access").map_err(|error| error.to_string())?;
    seed_as(&host, account.clone(), blob).await;
    host.http().enqueue_response(
        200,
        &[(
            "content-type",
            if cell.stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )],
        wire_response(Shape::Responses, cell.stream, cell.tools),
    );
    let endpoints = Endpoints {
        base: "https://grok.test/v1".to_owned(),
    };
    let mut raw = RawTable::new();
    raw.insert(
        "command",
        RawValue::String(if cfg!(windows) {
            r"C:\grok\bin\grok.exe".to_owned()
        } else {
            "/opt/grok/bin/grok".to_owned()
        }),
    );
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, raw);
    execute_async(&subscription, &account, &host, cell).await
}

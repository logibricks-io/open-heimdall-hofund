use std::time::Duration;

use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_gemini::{Endpoints, factory_with_endpoints};

use crate::harness::{code_assist_response, execute_async, host, secret, seed, subscription};
use crate::matrix::Cell;

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("gemini", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_secret("refresh_token", &secret("matrix-refresh"))
            .with_text("email", "matrix@example.test")
            .with_text("project", "project-matrix"),
    )
    .await;
    host.http().enqueue(
        br#"{"access_token":"matrix-access","token_type":"Bearer","expires_in":3600}"#.to_vec(),
    );
    host.http().enqueue(br#"{"flags":[]}"#.to_vec());
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
        code_assist_response(cell.stream, cell.tools),
    );
    let endpoints = Endpoints {
        authorize: "https://accounts.test/authorize".to_owned(),
        token: "https://oauth.test/token".to_owned(),
        userinfo: "https://oauth.test/userinfo".to_owned(),
        code_assist: "https://code-assist.test".to_owned(),
        onboard_poll: Duration::from_millis(1),
    };
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}

use std::time::Duration;

use hofund_plugin_api::{RawTable, RawValue};
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_antigravity::{Endpoints, factory_with_endpoints};

use crate::harness::{code_assist_response, execute_async, host, secret, seed, subscription};
use crate::matrix::Cell;

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("antigravity", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_secret("refresh_token", &secret("matrix-refresh"))
            .with_text("email", "matrix@example.test")
            .with_text("project", "project-matrix")
            .with_text("tier", "free-tier")
            .with_text("paid_tier", ""),
    )
    .await;
    host.http().enqueue(
        br#"{"access_token":"matrix-access","token_type":"Bearer","expires_in":3600}"#.to_vec(),
    );
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
        load_code_assist: "https://code-assist-prod.test".to_owned(),
        code_assist: "https://code-assist-daily.test".to_owned(),
        version_manifest: "https://antigravity-version.test/manifest/latest-arm64-mac.yml"
            .to_owned(),
        onboard_poll: Duration::from_millis(1),
    };
    let mut raw = RawTable::new();
    raw.insert("client_id", RawValue::String("matrix-client".to_owned()));
    raw.insert(
        "client_secret",
        RawValue::String("matrix-client-secret".to_owned()),
    );
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, raw);
    execute_async(&subscription, &account, &host, cell).await
}

use std::collections::BTreeMap;

use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_codex::{Endpoints, factory_with_endpoints};

use crate::harness::{execute_async, host, secret, seed, subscription, wire_response};
use crate::matrix::{Cell, Shape};

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | (u32::from(chunk.get(2).copied().unwrap_or(0)));
        for index in 0..chunk.len() + 1 {
            out.push(char::from(
                ALPHABET[((value >> (18 - 6 * index)) & 63) as usize],
            ));
        }
    }
    out
}

fn jwt(claims: &str) -> String {
    format!("e30.{}.sig", base64url(claims.as_bytes()))
}

pub async fn run(cell: Cell) -> Result<(), String> {
    let _ = hofund_plugin_sdk::codex_zstd::codex_zstd_for_tests(false);
    let dir = tempfile::tempdir().expect("dir");
    let host = host("codex", &dir);
    let access = jwt(r#"{"exp":10000}"#);
    let identity = jwt(
        r#"{"email":"matrix@example.test","sub":"subject","https://api.openai.com/auth":{"chatgpt_account_id":"account","chatgpt_plan_type":"plus"}}"#,
    );
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("id_token", &secret(&identity))
        .with_secret("access_token", &secret(&access))
        .with_secret("refresh_token", &secret("matrix-refresh"))
        .with_text("account_id", "account")
        .with_number("last_refresh", 0);
    let account = seed(&host, blob).await;
    let prompts = BTreeMap::from([("gpt-5", "matrix instructions")]);
    std::fs::write(
        host.context()
            .dirs()
            .cache_dir()
            .expect("cache")
            .join("codex-prompts.json"),
        json::to_vec(&prompts).expect("prompts"),
    )
    .expect("prompts file");
    host.http().enqueue_response(
        200,
        &[("content-type", "text/event-stream")],
        wire_response(Shape::Responses, true, cell.tools),
    );
    let endpoints = Endpoints {
        authorize: "https://auth.test/authorize".to_owned(),
        token: "https://auth.test/token".to_owned(),
        device_issuer: "https://auth.test".to_owned(),
        base: "https://codex.test/backend-api/codex".to_owned(),
        port: None,
    };
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}

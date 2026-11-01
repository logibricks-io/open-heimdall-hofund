//! Provider upstream-key control API against a running daemon.

use hofund_control_client::Client;
use hofund_control_proto::{ProviderKeyAction, ProviderKeyRequest};
use hofund_daemon::{ControlRuntime, StartOptions, start_with_options};

#[tokio::test]
async fn k6_control_api_manages_provider_keys_without_returning_secrets() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("hofund.toml");
    std::fs::write(
        &config,
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"primary\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"first-key\"\n",
    )
    .expect("config");
    let running = start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("address")),
            control_runtime: Some(ControlRuntime::new(None, dir.path().to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon");
    let address = running.control_local_addr().expect("control");
    let token = std::fs::read_to_string(dir.path().join("hofund.token")).expect("token");
    let client = Client::tcp(address, token.trim());

    let mut add = ProviderKeyRequest::new(ProviderKeyAction::Add);
    add.keys = vec!["second-key,third-key second-key first-key".to_owned()];
    let added = client
        .change_provider_key("primary", add)
        .await
        .expect("add");
    assert_eq!(added.added, 2);
    assert_eq!(added.already_had, 1);
    assert_eq!(added.keys.len(), 3);
    assert!(added.keys.iter().all(|key| key.masked.starts_with('…')));
    let second = added.keys[1].id.clone();

    let mut rename = ProviderKeyRequest::new(ProviderKeyAction::Rename).for_id(&second);
    rename.name = "Team".to_owned();
    client
        .change_provider_key("primary", rename)
        .await
        .expect("rename");
    let used = client
        .change_provider_key(
            "primary",
            ProviderKeyRequest::new(ProviderKeyAction::Use).for_id(&second),
        )
        .await
        .expect("use");
    assert_eq!(used.keys[0].id, second);
    assert_eq!(used.keys[0].name, "Team");
    client
        .change_provider_key(
            "primary",
            ProviderKeyRequest::new(ProviderKeyAction::Off).for_id(&second),
        )
        .await
        .expect("off");
    let on = client
        .change_provider_key(
            "primary",
            ProviderKeyRequest::new(ProviderKeyAction::On).for_id(&second),
        )
        .await
        .expect("on");
    assert!(on.keys.iter().find(|key| key.id == second).expect("key").on);

    let third = on
        .keys
        .iter()
        .find(|key| key.name.is_empty() && key.id != second)
        .expect("third");
    let mut replace = ProviderKeyRequest::new(ProviderKeyAction::Replace).for_id(&third.id);
    replace.keys = vec!["replacement-key".to_owned()];
    let replaced = client
        .change_provider_key("primary", replace)
        .await
        .expect("replace");
    assert_eq!(replaced.before, third.id);
    assert_eq!(replaced.after.len(), 10);
    assert_ne!(replaced.before, replaced.after);
    let listed = client.provider_keys("primary").await.expect("list");
    assert_eq!(listed.keys.len(), 3);
    let text = std::fs::read_to_string(&config).expect("written config");
    assert!(text.contains("replacement-key"));
    running.shutdown().await;
}

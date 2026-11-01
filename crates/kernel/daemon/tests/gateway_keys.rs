//! Named-key control API against a running daemon.

use hofund_control_client::Client;
use hofund_control_proto::{GatewayKeyAction, GatewayKeyRequest, GatewayKeyRestoreRequest};
use hofund_daemon::{ControlRuntime, StartOptions, start_with_options};

fn request(
    action: GatewayKeyAction,
    configure: impl FnOnce(&mut GatewayKeyRequest),
) -> GatewayKeyRequest {
    let mut request = GatewayKeyRequest::new(action);
    configure(&mut request);
    request
}

#[tokio::test]
async fn k12_control_api_supports_every_action_and_backup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("hofund.toml");
    std::fs::write(
        &config,
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"primary\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"test-key\"\n",
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
    let added = client
        .change_gateway_key(request(GatewayKeyAction::Add, |request| {
            request.name = "Control".to_owned();
            request.secret = Some("control-secret-123".to_owned());
        }))
        .await
        .expect("add");
    assert_eq!(added.secret.as_deref(), Some("control-secret-123"));
    let id = added.keys.last().expect("added key").id.clone();
    let viewer = client
        .change_gateway_key(request(GatewayKeyAction::Add, |request| {
            request.name = "Viewer".to_owned();
            request.read_only = true;
        }))
        .await
        .expect("add read-only");
    let viewer_key = viewer
        .keys
        .iter()
        .find(|key| key.name == "Viewer")
        .expect("viewer key");
    assert!(viewer_key.read_only, "add keeps the read-only flag");
    let change = |action| GatewayKeyRequest::new(action).for_id(&id);
    let changed = |action, configure: &dyn Fn(&mut GatewayKeyRequest)| {
        let mut request = change(action);
        configure(&mut request);
        request
    };
    client
        .change_gateway_key(changed(GatewayKeyAction::Rename, &|request| {
            request.name = "Renamed".to_owned();
        }))
        .await
        .expect("rename");
    client
        .change_gateway_key(changed(GatewayKeyAction::Move, &|request| {
            request.place = Some(1);
        }))
        .await
        .expect("move");
    client
        .change_gateway_key(changed(GatewayKeyAction::Models, &|request| {
            request.models = vec!["primary/*".to_owned()];
        }))
        .await
        .expect("models");
    client
        .change_gateway_key(changed(GatewayKeyAction::Accounts, &|request| {
            request.accounts = vec!["primary/preset:primary:default".to_owned()];
        }))
        .await
        .expect("accounts");
    client
        .change_gateway_key(changed(GatewayKeyAction::Quotas, &|request| {
            request.quotas = true;
        }))
        .await
        .expect("quotas");
    client
        .change_gateway_key(changed(GatewayKeyAction::Access, &|request| {
            request.read_only = true;
        }))
        .await
        .expect("access");
    client
        .change_gateway_key(changed(GatewayKeyAction::Issuer, &|request| {
            request.issues = true;
        }))
        .await
        .expect("issuer");
    client
        .change_gateway_key(change(GatewayKeyAction::Off))
        .await
        .expect("off");
    client
        .change_gateway_key(change(GatewayKeyAction::On))
        .await
        .expect("on");
    let rotated = client
        .change_gateway_key(change(GatewayKeyAction::Rotate))
        .await
        .expect("rotate");
    let listed = client.gateway_keys().await.expect("list");
    let key = listed.keys.iter().find(|key| key.id == id).expect("listed");
    assert_eq!(key.name, "Renamed");
    assert!(key.quotas && key.read_only && key.issues);
    assert!(key.secret.is_empty(), "a listing carries no secret");
    assert!(!key.masked.is_empty());
    let rotated_secret = rotated.secret.expect("rotated secret");
    let copied = client
        .change_gateway_key(change(GatewayKeyAction::Copy))
        .await
        .expect("copy");
    assert_eq!(copied.secret.as_deref(), Some(rotated_secret.as_str()));
    assert!(copied.keys.iter().all(|key| key.secret.is_empty()));
    assert!(!format!("{copied:?}").contains(&rotated_secret));
    let exported = client.export_gateway_keys().await.expect("export");
    assert!(exported.keys.iter().all(|key| !key.secret.is_empty()));
    assert!(
        !format!("{exported:?}").contains(&rotated_secret),
        "an exported key's Debug output redacts its secret"
    );
    assert!(
        exported
            .keys
            .iter()
            .any(|key| key.id == id && key.secret == rotated_secret)
    );
    client
        .change_gateway_key(change(GatewayKeyAction::Remove))
        .await
        .expect("remove");
    client
        .restore_gateway_keys(GatewayKeyRestoreRequest::new(exported.keys))
        .await
        .expect("restore");
    assert!(
        client
            .gateway_keys()
            .await
            .expect("restored list")
            .keys
            .iter()
            .any(|key| key.id == id)
    );
    running.shutdown().await;
}

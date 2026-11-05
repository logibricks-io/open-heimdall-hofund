//! R-7a control routes against a live daemon (presets, status, providers, groups, profiles).

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hofund_agent::Session;
use hofund_control_proto::{
    API_VERSION, AgentConnectRequest, ConfigResponse, ControlErrorKind, GroupCreateRequest,
    GroupListResponse, GroupMemberCandidatesResponse, GroupPatchRequest, GroupRestoreRequest,
    GroupWriteResponse, PresetListResponse, ProfileRenameRequest, ProfileSaveRequest,
    ProviderCreateRequest, ProviderDeleteRequest, ProviderDeleteResponse, ProviderListResponse,
    ProviderPatchRequest, ProviderWriteResponse, RouteModelListResponse, ServeRole, StatusResponse,
    decode_error_envelope, decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use serde_json::json;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "sk-control-r7a-provider-secret";

fn config(extra: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n{extra}\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n"
    )
}

fn revision(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Scratch {
    config_dir: PathBuf,
    runtime_dir: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Scratch {
    fn new(label: &str) -> Self {
        let make = |kind: &str| {
            tempfile::Builder::new()
                .prefix(&format!("hofund-daemon-r7a-{label}-{kind}-"))
                .tempdir()
                .expect("scratch dir")
        };
        let config = make("config");
        let runtime = make("runtime");
        std::fs::create_dir_all(runtime.path().join("home")).expect("scratch home");
        Self {
            config_dir: config.path().to_path_buf(),
            runtime_dir: runtime.path().to_path_buf(),
            _dirs: (config, runtime),
        }
    }

    fn path(&self) -> PathBuf {
        self.config_dir.join("hofund.toml")
    }
}

fn options(scratch: &Scratch) -> StartOptions {
    options_with(scratch, StartOptions::default())
}

fn options_with(scratch: &Scratch, mut base: StartOptions) -> StartOptions {
    base.control_listen = Some("127.0.0.1:0".parse().expect("control address"));
    base.control_runtime = Some(ControlRuntime::new(None, scratch.runtime_dir.clone()));
    base.skip_config_watch = true;
    base.hofund_addr = Some(String::new());
    base
}

async fn start(scratch: &Scratch, text: &str) -> Running {
    start_with(scratch, text, options(scratch)).await
}

async fn start_with(scratch: &Scratch, text: &str, options: StartOptions) -> Running {
    let path = scratch.path();
    std::fs::write(&path, text).expect("config");
    start_with_options(&path, options).await.expect("daemon")
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    fn new(running: &Running, scratch: &Scratch) -> Self {
        let token = std::fs::read_to_string(scratch.runtime_dir.join("hofund.token"))
            .or_else(|_| std::fs::read_to_string(scratch.config_dir.join("hofund.token")))
            .expect("token")
            .trim()
            .to_owned();
        Self {
            http: support::client(),
            base: format!("http://{}", running.control_local_addr().expect("control")),
            token,
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (reqwest::StatusCode, Vec<u8>) {
        let response = request.bearer_auth(&self.token).send().await.expect("send");
        let status = response.status();
        (status, response.bytes().await.expect("body").to_vec())
    }

    async fn get(&self, path: &str) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    async fn post<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
    ) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body))
            .await
    }

    async fn post_raw(&self, path: &str, body: Vec<u8>) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(
            self.http
                .post(format!("{}{path}", self.base))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
        )
        .await
    }

    async fn patch<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
    ) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(self.http.patch(format!("{}{path}", self.base)).json(body))
            .await
    }

    async fn delete<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
    ) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(self.http.delete(format!("{}{path}", self.base)).json(body))
            .await
    }

    async fn config(&self) -> ConfigResponse {
        let (status, body) = self.get("/v1/hofund/config").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        decode_response(API_VERSION, &body).expect("config DTO")
    }

    async fn status(&self) -> StatusResponse {
        let (status, body) = self.get("/v1/hofund/status").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        decode_response(API_VERSION, &body).expect("status DTO")
    }

    async fn groups(&self) -> GroupListResponse {
        let (status, body) = self.get("/v1/hofund/groups").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        decode_response(API_VERSION, &body).expect("groups DTO")
    }
}

fn failure(status: reqwest::StatusCode, body: &[u8], expected: u16) -> ControlErrorKind {
    assert_eq!(
        status.as_u16(),
        expected,
        "{}",
        String::from_utf8_lossy(body)
    );
    decode_error_envelope(API_VERSION, body)
        .expect("error envelope")
        .error
        .kind
}

fn field_issue(body: &[u8], field: &str) -> bool {
    decode_error_envelope(API_VERSION, body)
        .expect("error envelope")
        .error
        .details
        .iter()
        .any(|issue| issue.field == field)
}

fn provider_create(value: serde_json::Value) -> ProviderCreateRequest {
    serde_json::from_value(value).expect("provider create request")
}

fn provider_patch(value: serde_json::Value) -> ProviderPatchRequest {
    serde_json::from_value(value).expect("provider patch request")
}

fn provider_delete(expected_revision: &str) -> ProviderDeleteRequest {
    serde_json::from_value(json!({
        "version": API_VERSION,
        "expected_revision": expected_revision,
    }))
    .expect("provider delete request")
}

fn group_create(value: serde_json::Value) -> GroupCreateRequest {
    serde_json::from_value(value).expect("group create request")
}

fn group_patch(value: serde_json::Value) -> GroupPatchRequest {
    serde_json::from_value(value).expect("group patch request")
}

fn group_restore(value: serde_json::Value) -> GroupRestoreRequest {
    serde_json::from_value(value).expect("group restore request")
}

fn profile_save(name: &str) -> ProfileSaveRequest {
    serde_json::from_value(json!({
        "version": API_VERSION,
        "name": name,
        "from_current": false,
        "fields": {},
    }))
    .expect("profile save request")
}

fn profile_rename(from: &str, to: &str) -> ProfileRenameRequest {
    serde_json::from_value(json!({
        "version": API_VERSION,
        "from": from,
        "to": to,
    }))
    .expect("profile rename request")
}

fn seed_models(dir: &Path, provider: &str, models: serde_json::Value) {
    let root = dir.join(".hofund/host/models");
    std::fs::create_dir_all(&root).expect("model state directory");
    let slug = provider
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .take(40)
        .collect::<String>();
    let hash = format!("{:x}", Sha256::digest(provider.as_bytes()));
    let path = root.join(format!("{slug}-{}.json", &hash[..16]));
    let state = json!({
        "version": 1,
        "id": provider,
        "fetched_at": 1234,
        "list": models,
        "delisted": []
    });
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&state).expect("state JSON"),
    )
    .expect("model state");
}

#[tokio::test]
async fn presets_google_vertex_row_lists_project_options() {
    let scratch = Scratch::new("presets-vertex");
    let running = support::within(
        "start daemon",
        support::REQUEST,
        start(&scratch, &config("")),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let (status, body) = support::within(
        "GET presets",
        support::REQUEST,
        client.get("/v1/hofund/presets"),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let response: PresetListResponse = decode_response(API_VERSION, &body).expect("presets body");
    let vertex = response
        .presets
        .iter()
        .find(|entry| entry.id == "google-vertex")
        .expect("google-vertex preset");
    assert!(vertex.needs_project);
    assert!(!vertex.needs_address);
    for key in ["project", "location", "credentials", "impersonate"] {
        assert!(
            vertex.option_keys.iter().any(|option| option == key),
            "missing option key `{key}`"
        );
    }
    support::shutdown(running).await;
}

#[tokio::test]
async fn status_reports_serve_role_from_environment() {
    let scratch = Scratch::new("status-serve");
    let text = config("");
    let serve_options = options_with(
        &scratch,
        StartOptions {
            hofund_serves: Some("serve".to_owned()),
            hofund_owner_pid: Some(424_242),
            ..StartOptions::default()
        },
    );
    let running = support::within(
        "start daemon",
        support::REQUEST,
        start_with(&scratch, &text, serve_options),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let status = support::within("GET status", support::REQUEST, client.status()).await;
    assert_eq!(status.serves, Some(ServeRole::Serve));
    assert_eq!(status.owned_by_pid, 424_242);
    assert_eq!(status.disk_revision, revision(&text));
    assert_eq!(status.runtime_revision, status.disk_revision);
    assert!(status.pid > 0);
    assert!(status.started_at_ms > 0);
    support::shutdown(running).await;
}

#[tokio::test]
async fn provider_list_json_has_no_credential_fields() {
    let scratch = Scratch::new("providers-list");
    let running = support::within(
        "start daemon",
        support::REQUEST,
        start(&scratch, &config("")),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let (status, body) = support::within(
        "GET providers",
        support::REQUEST,
        client.get("/v1/hofund/providers"),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("api_key"));
    assert!(!text.contains("credentials"));
    assert!(!text.contains(SECRET));
    let list: ProviderListResponse = decode_response(API_VERSION, &body).expect("provider list");
    assert_eq!(list.providers.len(), 1);
    assert_eq!(list.providers[0].id, "chat");
    support::shutdown(running).await;
}

#[tokio::test]
async fn provider_create_patch_and_validation_errors() {
    let scratch = Scratch::new("providers-write");
    let text = config("");
    let running = support::within("start daemon", support::REQUEST, start(&scratch, &text)).await;
    let client = Client::new(&running, &scratch);
    let read = client.config().await;
    let disk_revision = read.disk_revision.clone();

    let unknown = format!(
        r#"{{"version":{},"expected_revision":"{}","preset":"openai","extra":true}}"#,
        API_VERSION, read.disk_revision
    );
    let (status, body) = client
        .post_raw("/v1/hofund/providers", unknown.into_bytes())
        .await;
    assert_eq!(failure(status, &body, 400), ControlErrorKind::BadRequest);

    let stale = provider_create(json!({
        "version": API_VERSION,
        "expected_revision": "0".repeat(64),
        "preset": "openai",
        "id": "added",
        "base_url": "http://127.0.0.1:9/v1",
        "api_key": "sk-added",
    }));
    let (status, body) = client.post("/v1/hofund/providers", &stale).await;
    assert_eq!(failure(status, &body, 409), ControlErrorKind::Conflict);

    let bad_url = provider_create(json!({
        "version": API_VERSION,
        "expected_revision": disk_revision,
        "preset": "openai",
        "id": "bad-url",
        "base_url": "http://user:pass@127.0.0.1/v1",
        "api_key": "sk-added",
    }));
    let (status, body) = client.post("/v1/hofund/providers", &bad_url).await;
    assert_eq!(failure(status, &body, 422), ControlErrorKind::Unprocessable);

    let create = provider_create(json!({
        "version": API_VERSION,
        "expected_revision": disk_revision,
        "preset": "openai",
        "id": "added",
        "name": "Added",
        "base_url": "http://127.0.0.1:9/v1",
        "api_key": "sk-added",
    }));
    let (status, body) = client.post("/v1/hofund/providers", &create).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let created: ProviderWriteResponse = decode_response(API_VERSION, &body).expect("create body");
    assert_eq!(created.provider.id, "added");
    assert_eq!(created.provider.preset.as_deref(), Some("openai"));

    let disable = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": created.disk_revision,
        "enabled": false,
    }));
    let (status, body) = client.patch("/v1/hofund/providers/added", &disable).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let disabled: ProviderWriteResponse =
        decode_response(API_VERSION, &body).expect("disable body");
    assert!(!disabled.provider.enabled);

    let list: ProviderListResponse =
        decode_response(API_VERSION, &client.get("/v1/hofund/providers").await.1)
            .expect("providers after disable");
    assert!(
        list.providers
            .iter()
            .find(|provider| provider.id == "added")
            .is_some_and(|provider| !provider.enabled),
        "disable must persist when agent session falls back to the process environment"
    );

    support::shutdown(running).await;
}

#[tokio::test]
async fn groups_create_patch_rename_restore_and_candidates() {
    let scratch = Scratch::new("groups");
    seed_models(
        &scratch.config_dir,
        "chat",
        json!([
            {"id": "plain"},
            {"id": "classifier", "decides": true}
        ]),
    );
    let text = config("");
    let running = support::within("start daemon", support::REQUEST, start(&scratch, &text)).await;
    let client = Client::new(&running, &scratch);
    let revision_before = client.groups().await.disk_revision;

    let create = group_create(json!({
        "version": API_VERSION,
        "expected_revision": revision_before,
        "name": "Team One",
        "first_member": "chat/plain",
    }));
    let (status, body) = client.post("/v1/hofund/groups", &create).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let created: GroupWriteResponse = decode_response(API_VERSION, &body).expect("group create");
    assert_eq!(created.group.id, "team-one");

    let invalid_rules = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": created.disk_revision,
        "rules": ["not-a-clause=yes"],
    }));
    let (status, body) = client
        .patch("/v1/hofund/groups/team-one", &invalid_rules)
        .await;
    assert_eq!(failure(status, &body, 422), ControlErrorKind::Unprocessable);
    assert!(field_issue(&body, "rules"));

    let rename = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": client.groups().await.disk_revision,
        "rename_to": "team-alpha",
    }));
    let (status, body) = client.patch("/v1/hofund/groups/team-one", &rename).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let renamed: GroupWriteResponse = decode_response(API_VERSION, &body).expect("group rename");
    assert_eq!(renamed.group.id, "team-alpha");

    let restore = group_restore(json!({
        "version": API_VERSION,
        "expected_revision": renamed.disk_revision,
    }));
    let (status, body) = client
        .post("/v1/hofund/groups/no-such-group/restore", &restore)
        .await;
    assert_eq!(failure(status, &body, 404), ControlErrorKind::NotFound);

    let (status, body) = client.get("/v1/hofund/groups/member-candidates").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let candidates: GroupMemberCandidatesResponse =
        decode_response(API_VERSION, &body).expect("member candidates");
    assert!(
        candidates
            .deciders
            .iter()
            .any(|entry| entry.reference == "chat/classifier"),
        "expected classifier model in deciders"
    );
    assert!(
        candidates
            .served
            .iter()
            .any(|entry| entry.reference == "chat/plain"),
        "expected plain model in served"
    );
    for decider in &candidates.deciders {
        assert!(
            !candidates
                .served
                .iter()
                .any(|served| served.reference == decider.reference),
            "decider `{}` must not appear in served",
            decider.reference
        );
    }

    support::shutdown(running).await;
}

#[tokio::test]
async fn group_patch_empty_members_is_rejected() {
    let scratch = Scratch::new("groups-members");
    let text = config("");
    let running = support::within("start daemon", support::REQUEST, start(&scratch, &text)).await;
    let client = Client::new(&running, &scratch);
    let revision_before = client.groups().await.disk_revision;
    let create = group_create(json!({
        "version": API_VERSION,
        "expected_revision": revision_before,
        "name": "Solo",
        "first_member": "chat/m",
    }));
    let (status, body) = client.post("/v1/hofund/groups", &create).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let created: GroupWriteResponse = decode_response(API_VERSION, &body).expect("group create");

    let strip = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": created.disk_revision,
        "members": [],
    }));
    let (status, body) = client.patch("/v1/hofund/groups/solo", &strip).await;
    assert_eq!(failure(status, &body, 422), ControlErrorKind::Unprocessable);
    assert!(field_issue(&body, "members"));

    support::shutdown(running).await;
}

#[tokio::test]
async fn profile_rename_collision_returns_422() {
    let scratch = Scratch::new("profiles-rename");
    let running = support::within(
        "start daemon",
        support::REQUEST,
        start(&scratch, &config("")),
    )
    .await;
    let client = Client::new(&running, &scratch);

    for name in ["alpha", "beta"] {
        let save = profile_save(name);
        let (status, body) = client.post("/v1/hofund/profiles/save", &save).await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    let rename = profile_rename("alpha", "beta");
    let (status, body) = client.post("/v1/hofund/profiles/rename", &rename).await;
    assert_eq!(failure(status, &body, 422), ControlErrorKind::Unprocessable);

    support::shutdown(running).await;
}

#[tokio::test]
async fn hidden_group_list_marks_found_true() {
    let scratch = Scratch::new("groups-hidden-found");
    let text = format!(
        "{}\n[[routes]]\nmodel = \"group/discovered\"\nhidden = true\nname = \"Discovered\"\nupstreams = [\"chat\"]\n",
        config("")
    );
    let running = support::within("start daemon", support::REQUEST, start(&scratch, &text)).await;
    let client = Client::new(&running, &scratch);
    let groups = support::within("GET groups", support::REQUEST, client.groups()).await;
    let group = groups
        .groups
        .iter()
        .find(|group| group.id == "discovered")
        .expect("hidden group row");
    assert!(group.hidden);
    assert!(group.found);
    support::shutdown(running).await;
}

#[tokio::test]
async fn group_rename_rewrites_agent_group_ref_in_scratch_home() {
    let scratch = Scratch::new("groups-agent-rename");
    let settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings.parent().expect("settings parent")).expect("claude dir");
    std::fs::write(&settings, "{\"model\": \"group/team-one\"}\n").expect("agent settings");
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let options = options_with(
        &scratch,
        StartOptions {
            agent_session: Some(session),
            ..StartOptions::default()
        },
    );
    let text = config("");
    let running = support::within(
        "start daemon",
        support::REQUEST,
        start_with(&scratch, &text, options),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let revision_before = client.groups().await.disk_revision;
    let create = group_create(json!({
        "version": API_VERSION,
        "expected_revision": revision_before,
        "name": "Team One",
        "first_member": "chat/plain",
    }));
    let (status, body) = client.post("/v1/hofund/groups", &create).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let created: GroupWriteResponse = decode_response(API_VERSION, &body).expect("group create");
    assert_eq!(created.group.id, "team-one");

    let rename = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": created.disk_revision,
        "rename_to": "team-alpha",
    }));
    let (status, _body) = client.patch("/v1/hofund/groups/team-one", &rename).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let settings_text = std::fs::read_to_string(&settings).expect("settings after rename");
    assert!(
        settings_text.contains("group/team-alpha"),
        "expected renamed group ref in agent settings, got: {settings_text}"
    );
    support::shutdown(running).await;
}

fn config_with_added_routes() -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"added\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"on-added\"\nupstreams = [\"added\"]\n\
[[routes]]\nmodel = \"safe\"\nupstreams = [\"chat\"]\n"
    )
}

#[tokio::test]
async fn disabling_provider_reseats_agent_off_route_alias() {
    let scratch = Scratch::new("providers-reseat");
    let settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings.parent().expect("settings parent")).expect("claude dir");
    std::fs::write(&settings, "{\"model\": \"on-added\"}\n").expect("agent settings");
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let options = options_with(
        &scratch,
        StartOptions {
            agent_session: Some(session),
            ..StartOptions::default()
        },
    );
    let text = config_with_added_routes();
    let running = support::within(
        "start daemon",
        support::REQUEST,
        start_with(&scratch, &text, options),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let read = client.config().await;
    let disable = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "enabled": false,
    }));
    let (status, body) = client.patch("/v1/hofund/providers/added", &disable).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let settings_text = std::fs::read_to_string(&settings).expect("settings after disable");
    assert!(
        settings_text.contains("\"safe\""),
        "agent must move to a route that no longer uses the disabled provider, got: {settings_text}"
    );
    let gateway_origin = format!("http://{}", running.listen_addr());
    assert!(
        !gateway_origin.is_empty() && gateway_origin != "http://",
        "expected a live listen origin"
    );
    let settings_json: serde_json::Value =
        serde_json::from_str(&settings_text).expect("settings JSON");
    assert_eq!(
        settings_json
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(|value| value.as_str()),
        Some(gateway_origin.as_str()),
        "gateway slot must use the listening address, got: {settings_text}"
    );
    support::shutdown(running).await;
}

#[tokio::test]
async fn deleting_provider_reseats_agent_and_leaves_loadable_config() {
    let scratch = Scratch::new("providers-delete");
    let settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings.parent().expect("settings parent")).expect("claude dir");
    std::fs::write(&settings, "{\"model\": \"on-added\"}\n").expect("agent settings");
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let options = options_with(
        &scratch,
        StartOptions {
            agent_session: Some(session),
            ..StartOptions::default()
        },
    );
    let text = config_with_added_routes();
    let running = support::within(
        "start daemon",
        support::REQUEST,
        start_with(&scratch, &text, options),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let read = client.config().await;
    let delete = provider_delete(&read.disk_revision);
    let (status, body) = client.delete("/v1/hofund/providers/added", &delete).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let _: ProviderDeleteResponse = decode_response(API_VERSION, &body).expect("delete body");
    let list: ProviderListResponse =
        decode_response(API_VERSION, &client.get("/v1/hofund/providers").await.1)
            .expect("providers after delete");
    assert!(
        list.providers.iter().all(|provider| provider.id != "added"),
        "deleted provider must disappear from the list"
    );
    let config = client.config().await;
    assert!(
        !config.text.contains("on-added"),
        "route alias for the deleted provider must be removed"
    );
    let settings_text = std::fs::read_to_string(&settings).expect("settings after delete");
    assert!(
        settings_text.contains("\"safe\""),
        "agent must reseat off the deleted provider's route, got: {settings_text}"
    );
    support::shutdown(running).await;
}

fn delete_reseat_fixture_config() -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"primary\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"removed\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"group/pair\"\nrouting = \"order\"\n\
upstreams = [{{ provider = \"primary\", model = \"m\" }}, {{ provider = \"removed\", model = \"m\" }}]\n\
[[routes]]\nmodel = \"on-removed\"\nupstreams = [\"removed\"]\n\
[[routes]]\nmodel = \"safe\"\nupstreams = [\"primary\"]\n"
    )
}

#[tokio::test]
async fn deleting_provider_reseat_keeps_sendable_group_moves_exclusive_route() {
    let scratch_group = Scratch::new("delete-reseat-group");
    let settings_group = scratch_group
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings_group.parent().expect("group parent")).expect("dir");
    std::fs::write(&settings_group, "{\"model\": \"group/pair\"}\n").expect("group settings");
    let session_group = Session::new(scratch_group.runtime_dir.join("home")).with_path("");
    let running_group = support::within(
        "start daemon group agent",
        support::REQUEST,
        start_with(
            &scratch_group,
            &delete_reseat_fixture_config(),
            options_with(
                &scratch_group,
                StartOptions {
                    agent_session: Some(session_group),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client_group = Client::new(&running_group, &scratch_group);
    let read_group = client_group.config().await;
    let delete_group = provider_delete(&read_group.disk_revision);
    let (status, body) = client_group
        .delete("/v1/hofund/providers/removed", &delete_group)
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let group_settings = std::fs::read_to_string(&settings_group).expect("group after delete");
    assert!(
        group_settings.contains("group/pair"),
        "agent on a still-sendable group must keep its model after one member provider is deleted, got: {group_settings}"
    );
    support::shutdown(running_group).await;

    let scratch_solo = Scratch::new("delete-reseat-solo");
    let settings_solo = scratch_solo
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings_solo.parent().expect("solo parent")).expect("dir");
    std::fs::write(&settings_solo, "{\"model\": \"on-removed\"}\n").expect("solo settings");
    let session_solo = Session::new(scratch_solo.runtime_dir.join("home")).with_path("");
    let running_solo = support::within(
        "start daemon solo agent",
        support::REQUEST,
        start_with(
            &scratch_solo,
            &delete_reseat_fixture_config(),
            options_with(
                &scratch_solo,
                StartOptions {
                    agent_session: Some(session_solo),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client_solo = Client::new(&running_solo, &scratch_solo);
    let read_solo = client_solo.config().await;
    let delete_solo = provider_delete(&read_solo.disk_revision);
    let (status, _) = client_solo
        .delete("/v1/hofund/providers/removed", &delete_solo)
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let solo_settings = std::fs::read_to_string(&settings_solo).expect("solo after delete");
    assert!(
        !solo_settings.contains("on-removed"),
        "agent must leave the deleted provider's exclusive route, got: {solo_settings}"
    );
    assert!(
        solo_settings.contains("\"safe\"") || solo_settings.contains("group/pair"),
        "agent must move to a still-sendable route, got: {solo_settings}"
    );
    support::shutdown(running_solo).await;
}

fn unlisted_reseat_fixture_config() -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"hidden\"\nlisted = false\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"spare\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"secret-route\"\nupstreams = [\"hidden\"]\n\
[[routes]]\nmodel = \"on-spare\"\nupstreams = [\"spare\"]\n\
[[routes]]\nmodel = \"normal\"\nupstreams = [\"chat\"]\n"
    )
}

#[tokio::test]
async fn reseat_skips_unlisted_exclusive_routes_on_disable_and_delete() {
    let scratch = Scratch::new("unlisted-reseat-disable");
    let settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings.parent().expect("settings parent")).expect("dir");
    std::fs::write(&settings, "{\"model\": \"on-spare\"}\n").expect("agent settings");
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let running = support::within(
        "start daemon for disable reseat",
        support::REQUEST,
        start_with(
            &scratch,
            &unlisted_reseat_fixture_config(),
            options_with(
                &scratch,
                StartOptions {
                    agent_session: Some(session),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let read = client.config().await;
    let disable = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "enabled": false,
    }));
    let (status, _) = client.patch("/v1/hofund/providers/spare", &disable).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let settings_text = std::fs::read_to_string(&settings).expect("settings after disable");
    assert!(
        settings_text.contains("\"normal\""),
        "agent must reseat to a visible route, got: {settings_text}"
    );
    assert!(
        !settings_text.contains("secret-route"),
        "agent settings must not contain the unlisted exclusive route, got: {settings_text}"
    );
    support::shutdown(running).await;

    let scratch_delete = Scratch::new("unlisted-reseat-delete");
    let settings_delete = scratch_delete
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings_delete.parent().expect("settings parent")).expect("dir");
    std::fs::write(&settings_delete, "{\"model\": \"on-spare\"}\n").expect("agent settings");
    let session_delete = Session::new(scratch_delete.runtime_dir.join("home")).with_path("");
    let running_delete = support::within(
        "start daemon for delete reseat",
        support::REQUEST,
        start_with(
            &scratch_delete,
            &unlisted_reseat_fixture_config(),
            options_with(
                &scratch_delete,
                StartOptions {
                    agent_session: Some(session_delete),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client_delete = Client::new(&running_delete, &scratch_delete);
    let read_delete = client_delete.config().await;
    let delete = provider_delete(&read_delete.disk_revision);
    let (status, _) = client_delete
        .delete("/v1/hofund/providers/spare", &delete)
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let settings_text = std::fs::read_to_string(&settings_delete).expect("settings after delete");
    assert!(
        settings_text.contains("\"normal\""),
        "agent must reseat to a visible route after delete, got: {settings_text}"
    );
    assert!(
        !settings_text.contains("secret-route"),
        "agent settings must not contain the unlisted exclusive route after delete, got: {settings_text}"
    );
    support::shutdown(running_delete).await;
}

#[tokio::test]
async fn disabling_exclusive_provider_rejects_requests_and_mixed_group_keeps_agent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let scratch = Scratch::new("disabled-hops");
    let settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings.parent().expect("settings parent")).expect("claude dir");
    std::fs::write(&settings, "{\"model\": \"group/pair\"}\n").expect("agent settings");
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let text = format!(
        "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"primary\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"spare\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"only-spare\"\nupstreams = [\"spare\"]\n\
[[routes]]\nmodel = \"group/pair\"\nrouting = \"order\"\n\
upstreams = [{{ provider = \"primary\", model = \"m\" }}, {{ provider = \"spare\", model = \"m\" }}]\n",
        server.uri()
    );
    let options = options_with(
        &scratch,
        StartOptions {
            agent_session: Some(session),
            ..StartOptions::default()
        },
    );
    let running = support::within(
        "start daemon",
        support::REQUEST,
        start_with(&scratch, &text, options),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let token = std::fs::read_to_string(scratch.runtime_dir.join("hofund.token"))
        .or_else(|_| std::fs::read_to_string(scratch.config_dir.join("hofund.token")))
        .expect("token")
        .trim()
        .to_owned();
    let gateway = format!("http://{}", running.local_addr);

    let read = client.config().await;
    let disable = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "enabled": false,
    }));
    let (status, _) = client.patch("/v1/hofund/providers/spare", &disable).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let blocked = support::client()
        .post(format!("{gateway}/v1/chat/completions"))
        .bearer_auth(&token)
        .json(&json!({"model": "only-spare", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("exclusive route request");
    assert_eq!(blocked.status(), reqwest::StatusCode::NOT_FOUND);

    let routed = support::client()
        .post(format!("{gateway}/v1/chat/completions"))
        .bearer_auth(&token)
        .json(&json!({"model": "group/pair", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("group route request");
    assert_eq!(routed.status(), reqwest::StatusCode::OK);

    let settings_text = std::fs::read_to_string(&settings).expect("settings after disable");
    assert!(
        settings_text.contains("group/pair"),
        "agent on a group with another enabled member must not reseat, got: {settings_text}"
    );

    support::shutdown(running).await;
    server.verify().await;
}

#[tokio::test]
async fn unlisted_provider_hides_exclusive_route_not_member_candidates() {
    let scratch = Scratch::new("listed-false");
    seed_models(&scratch.config_dir, "hidden", json!([{"id": "x"}]));
    let text = format!(
        "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"hidden\"\nlisted = false\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n\
[[routes]]\nmodel = \"secret-route\"\nupstreams = [\"hidden\"]\n\
[[routes]]\nmodel = \"group/work\"\nupstreams = [{{ group = \"inner\" }}]\n\
[[routes]]\nmodel = \"group/inner\"\nupstreams = [\"hidden\"]\n"
    );
    let running = support::within("start daemon", support::REQUEST, start(&scratch, &text)).await;
    let client = Client::new(&running, &scratch);

    let (status, body) = client.get("/v1/hofund/models").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let models: RouteModelListResponse = decode_response(API_VERSION, &body).expect("models");
    let ids: Vec<_> = models.models.into_iter().map(|entry| entry.id).collect();
    assert!(ids.contains(&"m".to_owned()));
    assert!(ids.contains(&"group/work".to_owned()));
    assert!(!ids.iter().any(|id| id == "secret-route"));

    let (status, body) = client.get("/v1/hofund/groups/member-candidates").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let candidates: GroupMemberCandidatesResponse =
        decode_response(API_VERSION, &body).expect("candidates");
    assert!(
        candidates
            .served
            .iter()
            .any(|entry| entry.reference.starts_with("hidden/")),
        "unlisted providers must still appear in member candidates"
    );

    support::shutdown(running).await;
}

fn reseat_fixture_config() -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"dead-off\"\nenabled = false\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"spare\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"dead-only\"\nupstreams = [\"dead-off\"]\n\
[[routes]]\nmodel = \"on-spare\"\nupstreams = [\"spare\"]\n\
[[routes]]\nmodel = \"safe\"\nupstreams = [\"chat\"]\n"
    )
}

#[tokio::test]
async fn disabling_provider_reseat_skips_already_dead_routes_in_config_order() {
    let scratch = Scratch::new("reseat-dead-first");
    let settings_spare = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings_spare.parent().expect("spare parent")).expect("dir");
    std::fs::write(&settings_spare, "{\"model\": \"on-spare\"}\n").expect("spare settings");

    let text = reseat_fixture_config();
    let session_spare = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let running = support::within(
        "start daemon spare agent",
        support::REQUEST,
        start_with(
            &scratch,
            &text,
            options_with(
                &scratch,
                StartOptions {
                    agent_session: Some(session_spare),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let read = client.config().await;
    let disable = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "enabled": false,
    }));
    let (status, _) = client.patch("/v1/hofund/providers/spare", &disable).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let spare_settings = std::fs::read_to_string(&settings_spare).expect("spare after disable");
    assert!(
        spare_settings.contains("\"safe\""),
        "agent on a route that lost hops must move to a sendable route, not dead-only first in config, got: {spare_settings}"
    );
    assert!(
        !spare_settings.contains("dead-only"),
        "must not reseat onto an already-dead route: {spare_settings}"
    );
    support::shutdown(running).await;

    let scratch_dead = Scratch::new("reseat-dead-agent");
    let settings_dead = scratch_dead
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings_dead.parent().expect("dead parent")).expect("dir");
    std::fs::write(&settings_dead, "{\"model\": \"dead-only\"}\n").expect("dead settings");
    let session_dead = Session::new(scratch_dead.runtime_dir.join("home")).with_path("");
    let running_dead = support::within(
        "start daemon dead agent",
        support::REQUEST,
        start_with(
            &scratch_dead,
            &reseat_fixture_config(),
            options_with(
                &scratch_dead,
                StartOptions {
                    agent_session: Some(session_dead),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client_dead = Client::new(&running_dead, &scratch_dead);
    let read_dead = client_dead.config().await;
    let disable_dead = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": read_dead.disk_revision,
        "enabled": false,
    }));
    let (status, _) = client_dead
        .patch("/v1/hofund/providers/spare", &disable_dead)
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let dead_settings = std::fs::read_to_string(&settings_dead).expect("dead agent settings");
    assert!(
        dead_settings.contains("dead-only"),
        "agent already on a dead route must not move when another provider is disabled, got: {dead_settings}"
    );
    support::shutdown(running_dead).await;
}

fn agent_hidden_models_reseat_fixture() -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"spare\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"secret-route\"\nupstreams = [\"chat\"]\n\
[[routes]]\nmodel = \"on-spare\"\nupstreams = [\"spare\"]\n\
[[routes]]\nmodel = \"normal\"\nupstreams = [\"chat\"]\n\
[agents.hidden_models]\nclaude-code = [\"secret-route\"]\n"
    )
}

#[tokio::test]
async fn r7a_reseat_honors_agent_hidden_models_on_provider_disable() {
    let scratch = Scratch::new("hidden-models-reseat");
    let settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(settings.parent().expect("settings parent")).expect("dir");
    std::fs::write(&settings, "{\"model\": \"on-spare\"}\n").expect("agent settings");
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let running = support::within(
        "start daemon for hidden-models reseat",
        support::REQUEST,
        start_with(
            &scratch,
            &agent_hidden_models_reseat_fixture(),
            options_with(
                &scratch,
                StartOptions {
                    agent_session: Some(session),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let read = client.config().await;
    let disable = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "enabled": false,
    }));
    let (status, _) = client.patch("/v1/hofund/providers/spare", &disable).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let settings_text = std::fs::read_to_string(&settings).expect("settings after disable");
    assert!(
        settings_text.contains("\"normal\""),
        "agent must reseat to a visible route, got: {settings_text}"
    );
    assert!(
        !settings_text.contains("secret-route"),
        "hidden route must not be written to the agent file, got: {settings_text}"
    );
    support::shutdown(running).await;
}

fn reseat_sidecar_rollback_fixture() -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"spare\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"on-spare\"\nupstreams = [\"spare\"]\n\
[[routes]]\nmodel = \"normal\"\nupstreams = [\"chat\"]\n"
    )
}

fn read_optional_file_bytes(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn agent_connect_sidecar_snapshot(main: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut snapshot = BTreeMap::new();
    snapshot.insert("main".to_owned(), read_optional_file_bytes(main));
    for suffix in [
        ".hofund.bak",
        ".hofund.absent",
        ".hofund.wrote",
        ".hofund.credential",
    ] {
        let mut name = main.as_os_str().to_os_string();
        name.push(suffix);
        snapshot.insert(
            suffix.to_owned(),
            read_optional_file_bytes(&PathBuf::from(name)),
        );
    }
    snapshot
}

async fn connect_claude_for_sidecars(client: &Client) {
    let (status, body) = client
        .post(
            "/v1/hofund/agents/claude-code/connect",
            &AgentConnectRequest::new(None),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
}

struct RestoreFailGuard;

impl RestoreFailGuard {
    fn new() -> Self {
        hofund_agent::test_force_sidecar_restore_fail(true);
        Self
    }
}

impl Drop for RestoreFailGuard {
    fn drop(&mut self) {
        hofund_agent::test_force_sidecar_restore_fail(false);
    }
}

#[tokio::test]
async fn r7a_provider_disable_reseat_rolls_back_config_and_first_agent_when_second_write_fails() {
    let scratch = Scratch::new("reseat-sidecar-rollback");
    let claude_settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(claude_settings.parent().expect("claude parent")).expect("dir");
    std::fs::write(&claude_settings, "{\"model\": \"on-spare\"}\n").expect("claude settings");
    let codex_config = scratch
        .runtime_dir
        .join("home")
        .join(".codex")
        .join("config.toml");
    std::fs::create_dir_all(codex_config.parent().expect("codex parent")).expect("dir");
    std::fs::write(
        &codex_config,
        "model = \"on-spare\"\nmodel_provider = \"openai\"\n",
    )
    .expect("codex config");
    let config_before = reseat_sidecar_rollback_fixture();
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let running = support::within(
        "start daemon for reseat rollback",
        support::REQUEST,
        start_with(
            &scratch,
            &config_before,
            options_with(
                &scratch,
                StartOptions {
                    agent_session: Some(session),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client = Client::new(&running, &scratch);
    connect_claude_for_sidecars(&client).await;
    let sidecars_before = agent_connect_sidecar_snapshot(&claude_settings);
    std::fs::set_permissions(&codex_config, std::fs::Permissions::from_mode(0o000))
        .expect("codex inaccessible");
    let read = client.config().await;
    let disable = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "enabled": false,
    }));
    let (status, _) = client.patch("/v1/hofund/providers/spare", &disable).await;
    assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    let config_after = std::fs::read_to_string(scratch.path()).expect("config after");
    assert_eq!(
        config_after, config_before,
        "config must roll back on reseat failure"
    );
    assert_eq!(
        agent_connect_sidecar_snapshot(&claude_settings),
        sidecars_before,
        "first agent main file and connect sidecars must roll back"
    );
    support::shutdown(running).await;
}

#[tokio::test]
async fn r7a_group_rename_rolls_back_config_and_first_agent_when_second_write_fails() {
    let scratch = Scratch::new("group-rename-sidecar-rollback");
    let text = format!(
        "{base}\
[[routes]]\nmodel = \"group/old\"\nname = \"Old\"\nrouting = \"smart\"\nupstreams = [\"chat\"]\n",
        base = reseat_sidecar_rollback_fixture()
    );
    let claude_settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(claude_settings.parent().expect("claude parent")).expect("dir");
    std::fs::write(&claude_settings, "{\"model\": \"group/old\"}\n").expect("claude settings");
    let codex_config = scratch
        .runtime_dir
        .join("home")
        .join(".codex")
        .join("config.toml");
    std::fs::create_dir_all(codex_config.parent().expect("codex parent")).expect("dir");
    std::fs::write(
        &codex_config,
        "model = \"group/old\"\nmodel_provider = \"openai\"\n",
    )
    .expect("codex config");
    let config_before = text.clone();
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let running = support::within(
        "start daemon for group rename rollback",
        support::REQUEST,
        start_with(
            &scratch,
            &text,
            options_with(
                &scratch,
                StartOptions {
                    agent_session: Some(session),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client = Client::new(&running, &scratch);
    let sidecars_before = agent_connect_sidecar_snapshot(&claude_settings);
    std::fs::set_permissions(&codex_config, std::fs::Permissions::from_mode(0o000))
        .expect("codex inaccessible");
    let read = client.config().await;
    let patch = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "rename_to": "new",
    }));
    let (status, _) = client.patch("/v1/hofund/groups/old", &patch).await;
    assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    let config_after = std::fs::read_to_string(scratch.path()).expect("config after");
    assert_eq!(
        config_after, config_before,
        "config must roll back on rename failure"
    );
    assert_eq!(
        agent_connect_sidecar_snapshot(&claude_settings),
        sidecars_before,
        "first agent main file and connect sidecars must roll back"
    );
    support::shutdown(running).await;
}

#[tokio::test]
async fn r7a_reseat_sidecar_restore_failure_rolls_back_config_and_scrubs_secrets() {
    let _restore_fail = RestoreFailGuard::new();
    let scratch = Scratch::new("reseat-restore-fail");
    let claude_settings = scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(claude_settings.parent().expect("claude parent")).expect("dir");
    std::fs::write(&claude_settings, "{\"model\": \"on-spare\"}\n").expect("claude settings");
    let codex_config = scratch
        .runtime_dir
        .join("home")
        .join(".codex")
        .join("config.toml");
    std::fs::create_dir_all(codex_config.parent().expect("codex parent")).expect("dir");
    std::fs::write(
        &codex_config,
        "model = \"on-spare\"\nmodel_provider = \"openai\"\n",
    )
    .expect("codex config");
    let config_before = reseat_sidecar_rollback_fixture();
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let running = support::within(
        "start daemon for restore failure",
        support::REQUEST,
        start_with(
            &scratch,
            &config_before,
            options_with(
                &scratch,
                StartOptions {
                    agent_session: Some(session),
                    ..StartOptions::default()
                },
            ),
        ),
    )
    .await;
    let client = Client::new(&running, &scratch);
    std::fs::set_permissions(&codex_config, std::fs::Permissions::from_mode(0o000))
        .expect("codex inaccessible");
    let read = client.config().await;
    let disable = provider_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "enabled": false,
    }));
    let (status, body) = client.patch("/v1/hofund/providers/spare", &disable).await;
    assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    let config_after = std::fs::read_to_string(scratch.path()).expect("config after");
    assert_eq!(
        config_after, config_before,
        "config must roll back when sidecar restore fails"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains(SECRET),
        "response must not contain provider secret"
    );
    assert!(
        !text.contains(&client.token),
        "response must not contain gateway bearer token"
    );
    support::shutdown(running).await;
}

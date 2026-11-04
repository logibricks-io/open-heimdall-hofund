//! Profile snapshot and apply tests (G-1).

use std::fs;

use hofund_agent::{
    Session, apply_fields, catalog_from, in_use, is_profile_secret, snapshot_fields,
};
use hofund_plugin_api::{
    AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, PluginId, Slot, Visible,
};

static ROUTE_FILES: &[AgentFile] = &[AgentFile::from_static(
    "config",
    FileKind::Json,
    "~/.route/config.json",
    "~/.route/config.json",
    "~/.route/config.json",
    None,
)];
static ROUTE_SETS: &[AgentSet] = &[
    AgentSet::from_static("config", "origin", Slot::Origin),
    AgentSet::from_static("config", "token", Slot::Token),
    AgentSet::from_static("config", "models", Slot::CatalogModels),
    AgentSet::from_static("config", "model", Slot::model_if_unset(None)),
    AgentSet::from_static("config", "literal", Slot::Literal("kept")),
    AgentSet::from_static("config", "api_key", Slot::Literal("")),
    AgentSet::from_static("config", "note", Slot::Literal("")),
    AgentSet::from_static("config", "provider", Slot::Literal("")),
];
static ROUTE_NONE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("route-none"),
    "Route None",
    "route-none",
    ROUTE_FILES,
    ROUTE_SETS,
    Some("model"),
    None,
    Visible::Only(&["shown"]),
    AgentRoute::None,
    &[],
);

static LIST: &[&AgentDescription] = &[&ROUTE_NONE];

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp directory")
}

fn install(home: &std::path::Path) {
    let config = home.join(".route").join("config.json");
    fs::create_dir_all(config.parent().expect("parent")).expect("mkdir");
    fs::write(
        &config,
        r#"{"model":"before","literal":"kept","extra":"stay"}"#,
    )
    .expect("write");
}

fn read_model(home: &std::path::Path) -> String {
    let text = fs::read_to_string(home.join(".route").join("config.json")).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    value["model"].as_str().unwrap_or("").to_owned()
}

#[test]
fn g1_1_save_then_switch_round_trip() {
    let home = scratch();
    install(home.path());
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let mut snapshot = snapshot_fields(&catalog, &session).expect("snapshot");
    snapshot.insert("route-none.model".to_owned(), "picked".to_owned());
    let mut notes = Vec::new();
    let applied = apply_fields(&catalog, &session, &snapshot, &mut notes).expect("apply");
    assert_eq!(applied.changed, 1, "{notes:?}");
    assert_eq!(read_model(home.path()), "picked");
    assert!(in_use(&snapshot, &catalog, &session).expect("in use"));
}

#[test]
fn g1_2_apply_rolls_back_on_failure() {
    let home = scratch();
    install(home.path());
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let mut fields = snapshot_fields(&catalog, &session).expect("snapshot");
    fields.insert("route-none.model".to_owned(), "first".to_owned());
    fields.insert("route-none.models".to_owned(), "not-json".to_owned());
    let before = fs::read(home.path().join(".route").join("config.json")).expect("before");
    let mut notes = Vec::new();
    let error = apply_fields(&catalog, &session, &fields, &mut notes).expect_err("fail");
    assert!(error.to_string().contains("models"), "{error}");
    let after = fs::read(home.path().join(".route").join("config.json")).expect("after");
    assert_eq!(before, after);
}

#[test]
fn g1_3_in_use_detects_drift() {
    let home = scratch();
    install(home.path());
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let snapshot = snapshot_fields(&catalog, &session).expect("snapshot");
    assert!(in_use(&snapshot, &catalog, &session).expect("compare"));
    let mut saved = snapshot;
    saved.insert("route-none.model".to_owned(), "saved".to_owned());
    assert!(!in_use(&saved, &catalog, &session).expect("compare"));
    let mut notes = Vec::new();
    apply_fields(&catalog, &session, &saved, &mut notes).expect("apply");
    assert!(in_use(&saved, &catalog, &session).expect("compare"));
}

#[test]
fn g1_4_secrets_never_snapshotted() {
    let home = scratch();
    install(home.path());
    fs::write(
        home.path().join(".route").join("config.json"),
        r#"{"model":"m","token":"sk-test12345678901234567890123456789012","literal":"kept"}"#,
    )
    .expect("write");
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let snapshot = snapshot_fields(&catalog, &session).expect("snapshot");
    assert!(!snapshot.contains_key("route-none.token"));
    assert!(is_profile_secret(
        "route-none.token",
        "sk-test12345678901234567890123456789012"
    ));
}

#[test]
fn g1_5_empty_value_removes_field() {
    let home = scratch();
    install(home.path());
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let mut fields = snapshot_fields(&catalog, &session).expect("snapshot");
    fields.insert("route-none.model".to_owned(), String::new());
    let mut notes = Vec::new();
    let applied = apply_fields(&catalog, &session, &fields, &mut notes).expect("apply");
    assert_eq!(applied.changed, 1);
    let text = fs::read_to_string(home.path().join(".route").join("config.json")).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert!(value.get("model").is_none());
    assert_eq!(value["extra"], "stay");
}

#[test]
fn g1_6_partial_profile_leaves_missing_fields() {
    let home = scratch();
    install(home.path());
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let mut fields = hofund_agent::ProfileFields::new();
    fields.insert("route-none.model".to_owned(), String::new());
    let mut notes = Vec::new();
    apply_fields(&catalog, &session, &fields, &mut notes).expect("apply");
    let text = fs::read_to_string(home.path().join(".route").join("config.json")).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert!(value.get("model").is_none());
    assert_eq!(value["extra"], "stay");
}

#[test]
fn g1_8_token_slot_never_snapshotted_even_with_plain_value() {
    let home = scratch();
    install(home.path());
    fs::write(
        home.path().join(".route").join("config.json"),
        r#"{"model":"m","token":"plain-session-label","literal":"kept"}"#,
    )
    .expect("write");
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let snapshot = snapshot_fields(&catalog, &session).expect("snapshot");
    assert!(!snapshot.contains_key("route-none.token"));
}

#[test]
fn g1_9_secret_name_excludes_plain_value() {
    let home = scratch();
    install(home.path());
    fs::write(
        home.path().join(".route").join("config.json"),
        r#"{"model":"m","api_key":"plain-not-a-token","literal":"kept"}"#,
    )
    .expect("write");
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let snapshot = snapshot_fields(&catalog, &session).expect("snapshot");
    assert!(!snapshot.contains_key("route-none.api_key"));
    assert!(is_profile_secret("route-none.api_key", "plain-not-a-token"));
}

#[test]
fn g1_10_secret_value_excludes_key_like_strings() {
    let sk = "sk-test12345678901234567890123456789012";
    let opaque = "abcdefghijklmnopqrstuvwxyz0123456789AB";
    assert!(opaque.len() >= 32);
    assert!(is_profile_secret("route-none.note", sk));
    assert!(is_profile_secret("route-none.note", opaque));
    assert!(!is_profile_secret("route-none.literal", "kept-value"));

    let home = scratch();
    install(home.path());
    fs::write(
        home.path().join(".route").join("config.json"),
        format!(r#"{{"model":"m","note":"{sk}","literal":"kept-value"}}"#),
    )
    .expect("write");
    let session = Session::new(home.path());
    let catalog = catalog_from(LIST).expect("catalog");
    let snapshot = snapshot_fields(&catalog, &session).expect("snapshot");
    assert!(!snapshot.contains_key("route-none.note"));
    assert_eq!(
        snapshot.get("route-none.literal").map(String::as_str),
        Some("kept-value")
    );
}

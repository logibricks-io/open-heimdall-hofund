//! Legacy `qoder.json` migration (`r54d_*`).

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    PluginId, PluginKind, RawTable, SecretString, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::secret_text;

use crate::creds::{self, Credential, Site};
use crate::import;
use crate::legacy_migrate;
use crate::{Endpoints, factory_with_endpoints};

fn host_with_config(dir: &tempfile::TempDir) -> (std::path::PathBuf, TestHost) {
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let root = config_dir.join(".hofund");
    std::fs::create_dir_all(&root).expect("hofund root");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("qoder"),
        &root,
    );
    (config_dir, host)
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(Endpoints::default());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn legacy_body(uid: &str, token: &str, machine_id: &str) -> String {
    let machine = if machine_id.is_empty() {
        String::new()
    } else {
        format!(r#","machine_id":"{machine_id}""#)
    };
    format!(r#"{{"uid":"{uid}","token":"{token}","refresh_token":"refresh-1"{machine}}}"#)
}

#[test]
fn r54d_1_missing_legacy_file_is_a_no_op() {
    let dir = tempfile::tempdir().expect("dir");
    let (config_dir, host) = host_with_config(&dir);
    block_on(legacy_migrate::run(&config_dir, host.context())).expect("migrate");
    assert!(
        block_on(host.context().accounts().list())
            .expect("list")
            .is_empty()
    );
}

#[test]
fn r54d_2_legacy_migrates_into_store_and_removes_file() {
    let dir = tempfile::tempdir().expect("dir");
    let (config_dir, host) = host_with_config(&dir);
    let legacy = legacy_migrate::legacy_path(&config_dir);
    std::fs::write(&legacy, legacy_body("uid-legacy", "token-legacy", "")).expect("write");
    block_on(legacy_migrate::run(&config_dir, host.context())).expect("migrate");
    assert!(!legacy.exists());
    let accounts = block_on(host.context().accounts().list()).expect("list");
    assert_eq!(accounts.len(), 1);
    let stored = block_on(creds::Stored::load(host.context(), accounts[0].id()))
        .expect("load")
        .credential;
    assert_eq!(stored.uid, "uid-legacy");
    assert_eq!(secret_text(&stored.token), "token-legacy");
    assert!(!stored.machine_id.is_empty());
    assert_eq!(stored.site, Site::Global);
}

#[test]
fn r54d_3_same_uid_skips_store_and_still_removes_file() {
    let dir = tempfile::tempdir().expect("dir");
    let (config_dir, host) = host_with_config(&dir);
    let credential = Credential {
        uid: "uid-dup".to_owned(),
        email: String::new(),
        name: String::new(),
        token: SecretString::from("stored-token".to_owned()),
        refresh_token: SecretString::from("stored-refresh".to_owned()),
        expires_at: 0,
        machine_id: "machine".to_owned(),
        site: Site::Global,
        vpc: String::new(),
        device_chat: false,
        device_token: SecretString::from(String::new()),
        device_refresh_token: SecretString::from(String::new()),
        pat_auth: false,
        pat: SecretString::from(String::new()),
    };
    block_on(import::store(&credential, host.context())).expect("seed");
    let legacy = legacy_migrate::legacy_path(&config_dir);
    std::fs::write(
        &legacy,
        legacy_body("uid-dup", "other-token", "other-machine"),
    )
    .expect("write");
    block_on(legacy_migrate::run(&config_dir, host.context())).expect("migrate");
    assert!(!legacy.exists());
    let id = creds::account_id(&credential);
    let stored = block_on(creds::Stored::load(host.context(), &id))
        .expect("load")
        .credential;
    assert_eq!(secret_text(&stored.token), "stored-token");
}

#[test]
fn r54d_4_unreadable_legacy_fails_closed_on_prepare() {
    let dir = tempfile::tempdir().expect("dir");
    let (config_dir, host) = host_with_config(&dir);
    std::fs::write(
        legacy_migrate::legacy_path(&config_dir),
        r#"{"token":"only-token"}"#,
    )
    .expect("write");
    let sub = subscription(&host);
    let error = block_on(sub.prepare_accounts(&config_dir, host.context())).expect_err("fail");
    assert!(error.to_string().contains(legacy_migrate::UNREADABLE));
}

#[test]
fn r54d_5_failed_store_leaves_legacy_file() {
    let dir = tempfile::tempdir().expect("dir");
    let (config_dir, host) = host_with_config(&dir);
    let legacy = legacy_migrate::legacy_path(&config_dir);
    std::fs::write(&legacy, legacy_body("uid-bad", "token-bad", "")).expect("write");
    let accounts_path = config_dir.join(".hofund/accounts.json");
    let backup = std::fs::read_to_string(&accounts_path).expect("read accounts");
    std::fs::remove_file(&accounts_path).expect("remove accounts");
    std::fs::create_dir(&accounts_path).expect("accounts path becomes a directory");
    let error = block_on(legacy_migrate::run(&config_dir, host.context())).expect_err("write");
    std::fs::remove_dir(&accounts_path).ok();
    std::fs::write(&accounts_path, backup).ok();
    assert!(
        legacy.exists(),
        "legacy file remains after failed store: {error}"
    );
}

#[test]
fn r54d_6_hofund_no_accounts_skips_migrate() {
    let dir = tempfile::tempdir().expect("dir");
    let (config_dir, host) = host_with_config(&dir);
    std::fs::write(
        legacy_migrate::legacy_path(&config_dir),
        legacy_body("uid-env", "token-env", ""),
    )
    .expect("write");
    let _off = legacy_migrate::TestDiscoveryOffGuard::on();
    block_on(legacy_migrate::run(&config_dir, host.context())).expect("skip");
    assert!(legacy_migrate::legacy_path(&config_dir).exists());
    assert!(
        block_on(host.context().accounts().list())
            .expect("list")
            .is_empty()
    );
}

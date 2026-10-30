//! Tests for agents that read hofund's key from the OS credential store.
//!
//! Every test uses an in-memory fake; none touches the real store.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use hofund_agent::{
    AgentAdapter, CREDENTIAL_CHANGE_PREFIX, Compiled, Credential, CredentialError, CredentialStore,
    GatewayTarget, Session,
};
use hofund_plugin_api::{
    AgentCredential, AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, PluginId,
    SecretString, Slot, Visible,
};

static FILES: &[AgentFile] = &[AgentFile::from_static(
    "settings",
    FileKind::Json,
    "~/.keyed/settings.json",
    "~/.keyed/settings.json",
    "~/.keyed/settings.json",
    None,
)];
static SETS: &[AgentSet] = &[AgentSet::from_static(
    "settings",
    "models.hofund.api_url",
    Slot::V1,
)];
const SPEC: AgentCredential =
    AgentCredential::from_static("settings", "{v1}", "Bearer", "keyed-app", "app:url={v1}");
static KEYED: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("keyed"),
    "Keyed",
    "keyed",
    FILES,
    SETS,
    None,
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_credential(SPEC);
static KEYED_NONE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("keyed-none"),
    "Keyed None",
    "keyed-none",
    FILES,
    SETS,
    None,
    None,
    Visible::All,
    AgentRoute::None,
    &[],
)
.with_credential(SPEC);

const TOKEN: &str = "local-token";
const URL: &str = "http://127.0.0.1:3425/v1";

#[derive(Default)]
struct FakeStore {
    items: Mutex<BTreeMap<String, (Credential, String)>>,
    writes: AtomicUsize,
    deletes: AtomicUsize,
    broken: AtomicBool,
}

impl FakeStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, (Credential, String)>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn secret(&self, url: &str) -> Option<String> {
        self.lock().get(url).map(|(_, secret)| secret.clone())
    }

    fn item(&self, url: &str) -> Option<Credential> {
        self.lock().get(url).map(|(item, _)| item.clone())
    }

    fn set(&self, url: &str, secret: &str) {
        let item = Credential::new(url, "Bearer", "keyed-app", format!("app:url={url}"));
        self.lock()
            .insert(url.to_owned(), (item, secret.to_owned()));
    }

    fn check(&self) -> Result<(), CredentialError> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(CredentialError::new("store locked"));
        }
        Ok(())
    }
}

impl CredentialStore for FakeStore {
    fn read(&self, item: &Credential) -> Result<Option<SecretString>, CredentialError> {
        self.check()?;
        Ok(self
            .lock()
            .get(item.url())
            .filter(|(held, _)| held.account() == item.account())
            .map(|(_, secret)| SecretString::from(secret.clone())))
    }

    fn write(&self, item: &Credential, secret: &str) -> Result<(), CredentialError> {
        self.check()?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.lock()
            .insert(item.url().to_owned(), (item.clone(), secret.to_owned()));
        Ok(())
    }

    fn delete(&self, item: &Credential) -> Result<(), CredentialError> {
        self.check()?;
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.lock().remove(item.url());
        Ok(())
    }
}

struct Scratch(tempfile::TempDir);

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

fn scratch() -> Scratch {
    Scratch(tempfile::tempdir().expect("temp dir"))
}

fn setup() -> (Scratch, Arc<FakeStore>, Session) {
    let home = scratch();
    let store = Arc::new(FakeStore::default());
    let session = Session::new(&*home).with_credential_store(store.clone());
    (home, store, session)
}

fn target(base: &str, token: &str) -> GatewayTarget {
    GatewayTarget::new(base, token, vec!["a".to_owned()])
}

fn local() -> GatewayTarget {
    target("http://127.0.0.1:3425/", TOKEN)
}

fn settings(home: &Path) -> PathBuf {
    home.join(".keyed").join("settings.json")
}

fn record(home: &Path) -> PathBuf {
    home.join(".keyed").join("settings.json.hofund.credential")
}

fn credential_paths(plan: &hofund_agent::Plan) -> Vec<String> {
    plan.changes()
        .iter()
        .filter_map(|change| change.path().to_str())
        .filter(|path| path.starts_with(CREDENTIAL_CHANGE_PREFIX))
        .map(ToOwned::to_owned)
        .collect()
}

fn assert_no_token(plan: &hofund_agent::Plan, token: &str) {
    for change in plan.changes() {
        if change
            .path()
            .to_str()
            .is_some_and(|path| path.starts_with(CREDENTIAL_CHANGE_PREFIX))
        {
            assert!(!change.diff().contains(token), "{}", change.diff());
            assert!(change.before().is_empty() && change.after().is_empty());
        }
    }
}

#[test]
fn connect_stores_the_key_once_and_disconnect_removes_it() {
    let (home, store, session) = setup();
    let agent = Compiled::new(&KEYED);
    let target = local();
    assert!(!agent.status(&session, &target).expect("status").connected());

    let plan = agent.plan(&session, &target).expect("plan");
    assert_eq!(
        credential_paths(&plan),
        vec![format!("{CREDENTIAL_CHANGE_PREFIX}{URL}")]
    );
    assert_no_token(&plan, TOKEN);
    assert_eq!(
        store.writes.load(Ordering::SeqCst),
        0,
        "plan writes nothing"
    );
    assert!(!settings(&home).exists());

    let applied = agent.apply(&session, &target).expect("apply");
    assert_eq!(credential_paths(&applied).len(), 1);
    assert_no_token(&applied, TOKEN);
    assert_eq!(store.secret(URL).as_deref(), Some(TOKEN));
    let item = store.item(URL).expect("item");
    assert_eq!(item.account(), "Bearer");
    assert_eq!(item.label(), "keyed-app");
    assert_eq!(item.windows_target(), format!("app:url={URL}"));
    let kept = fs::read_to_string(record(&home)).expect("record");
    assert!(!kept.contains(TOKEN), "{kept}");
    assert!(kept.contains(URL), "{kept}");
    assert!(agent.status(&session, &target).expect("status").connected());

    // Idempotent: a second connect neither writes nor reports the item.
    let record_before = fs::read(record(&home)).expect("record");
    let again = agent.apply(&session, &target).expect("apply again");
    assert!(credential_paths(&again).is_empty());
    assert_eq!(store.writes.load(Ordering::SeqCst), 1);
    assert_eq!(fs::read(record(&home)).expect("record"), record_before);
    assert!(credential_paths(&agent.plan(&session, &target).expect("plan")).is_empty());

    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.changed());
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(store.secret(URL), None);
    assert!(!record(&home).exists());
    assert!(!settings(&home).exists());
    let quiet = agent.disconnect(&session, false).expect("disconnect again");
    assert!(!quiet.changed());
    assert_eq!(store.deletes.load(Ordering::SeqCst), 1);
}

#[test]
fn a_key_the_user_replaced_is_drift_and_survives_disconnect() {
    let (home, store, session) = setup();
    let agent = Compiled::new(&KEYED);
    let target = local();
    agent.apply(&session, &target).expect("apply");
    store.set(URL, "user-key");
    assert!(!agent.status(&session, &target).expect("status").connected());

    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(outcome.warnings().len(), 1, "{:?}", outcome.warnings());
    let warning = &outcome.warnings()[0];
    assert!(warning.contains("did not write"), "{warning}");
    assert!(!warning.contains("user-key"), "{warning}");
    assert_eq!(store.secret(URL).as_deref(), Some("user-key"));
    assert!(!record(&home).exists());

    // A missing item is drift too, and connect puts it back.
    store.lock().clear();
    agent.apply(&session, &target).expect("apply");
    store.lock().clear();
    assert!(!agent.status(&session, &target).expect("status").connected());
    let plan = agent.apply(&session, &target).expect("repair");
    assert_eq!(credential_paths(&plan).len(), 1);
    assert!(agent.status(&session, &target).expect("status").connected());
}

#[test]
fn forced_disconnect_removes_the_item_whatever_it_holds() {
    let (home, store, session) = setup();
    let agent = Compiled::new(&KEYED);
    agent.apply(&session, &local()).expect("apply");
    store.set(URL, "user-key");
    let outcome = agent.disconnect(&session, true).expect("force");
    assert!(outcome.changed());
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(store.secret(URL), None);
    assert!(!record(&home).exists());
}

#[test]
fn rotation_and_a_new_gateway_address_move_the_key() {
    let (home, store, session) = setup();
    let agent = Compiled::new(&KEYED);
    // Nothing recorded: refresh leaves the store alone.
    assert!(
        agent
            .refresh_token(&session, &target("http://127.0.0.1:3425/", "rotated"))
            .expect("refresh")
            .is_empty()
    );
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);

    agent.apply(&session, &local()).expect("apply");
    let updated = agent
        .refresh_token(&session, &target("http://127.0.0.1:3425/", "rotated"))
        .expect("refresh");
    assert!(
        updated
            .iter()
            .any(|path| path == Path::new(&format!("{CREDENTIAL_CHANGE_PREFIX}{URL}"))),
        "{updated:?}"
    );
    assert_eq!(store.secret(URL).as_deref(), Some("rotated"));
    let kept = fs::read_to_string(record(&home)).expect("record");
    assert!(!kept.contains("rotated"), "{kept}");
    let unchanged = agent
        .refresh_token(&session, &target("http://127.0.0.1:3425/", "rotated"))
        .expect("refresh again");
    assert!(unchanged.is_empty(), "{unchanged:?}");

    let moved = "http://127.0.0.1:4000/v1";
    agent
        .apply(&session, &target("http://127.0.0.1:4000", "rotated"))
        .expect("apply on a new port");
    assert_eq!(store.secret(URL), None, "the old item goes");
    assert_eq!(store.secret(moved).as_deref(), Some("rotated"));

    // An old item the user took over stays.
    store.set(moved, "user-key");
    agent.apply(&session, &local()).expect("apply back");
    assert_eq!(store.secret(moved).as_deref(), Some("user-key"));
    assert_eq!(store.secret(URL).as_deref(), Some(TOKEN));
}

#[test]
fn without_a_store_connect_fails_before_writing_files() {
    let home = scratch();
    let session = Session::new(&*home);
    let agent = Compiled::new(&KEYED);
    let error = agent.apply(&session, &local()).expect_err("no store");
    let text = error.to_string();
    assert!(text.contains("credential store"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
    assert!(agent.plan(&session, &local()).is_err());
    assert!(!settings(&home).exists());
    assert!(!record(&home).exists());
    assert!(
        !agent
            .status(&session, &local())
            .expect("status")
            .connected()
    );
    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(!outcome.changed());
}

#[test]
fn an_unreachable_store_does_not_block_disconnect() {
    let (home, store, session) = setup();
    let agent = Compiled::new(&KEYED);
    agent.apply(&session, &local()).expect("apply");
    store.broken.store(true, Ordering::SeqCst);
    let error = agent.apply(&session, &local()).expect_err("store locked");
    assert!(error.to_string().contains("store locked"), "{error}");
    assert!(!error.to_string().contains(TOKEN), "{error}");

    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.changed(), "files are still restored");
    assert_eq!(outcome.warnings().len(), 1, "{:?}", outcome.warnings());
    assert!(
        outcome.warnings()[0].contains("retry"),
        "{:?}",
        outcome.warnings()
    );
    assert!(!settings(&home).exists());
    assert!(record(&home).exists(), "kept for a retry");

    store.broken.store(false, Ordering::SeqCst);
    let retry = agent.disconnect(&session, false).expect("retry");
    assert!(retry.changed());
    assert!(retry.warnings().is_empty(), "{:?}", retry.warnings());
    assert_eq!(store.secret(URL), None);
    assert!(!record(&home).exists());
}

#[test]
fn route_none_stores_no_key() {
    let (_home, store, session) = setup();
    let agent = Compiled::new(&KEYED_NONE);
    let plan = agent.apply(&session, &local()).expect("apply");
    assert!(credential_paths(&plan).is_empty());
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    assert!(store.lock().is_empty());
}

#[test]
fn revert_removes_the_key_hofund_wrote() {
    let (home, store, session) = setup();
    let agent = Compiled::new(&KEYED);
    agent.apply(&session, &local()).expect("apply");
    agent.revert(&session).expect("revert");
    assert_eq!(store.secret(URL), None);
    assert!(!record(&home).exists());
    assert!(!settings(&home).exists());
}

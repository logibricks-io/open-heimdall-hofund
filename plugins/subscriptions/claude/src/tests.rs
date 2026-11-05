//! Unit tests against recording host backends; nothing is executed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_kit_subscription::{
    PipedScript, PipedSpawn, TestHost, advance_time, block_on, block_on_paused,
};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, RawTable, RawValue,
    SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::{ClaudeFactory, account, cli, creds, pasted_code, setup_token};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
// Config needs an absolute path, which on Windows starts with a drive.
#[cfg(not(windows))]
const PROGRAM: &str = "/opt/claude/bin/claude";
#[cfg(windows)]
const PROGRAM: &str = r"C:\claude\bin\claude.exe";
const LINK: &str = "https://claude.ai/oauth/authorize?code=true&client_id=c&state=s1";
const UUID: &str = "4f6c1a52-1111-4222-8333-944455556666";
const EMAIL: &str = "ada@example.test";
const CREDENTIALS: &str = r#"{"claudeAiOauth":{"accessToken":"at-1","refreshToken":"rt-1","expiresAt":1900000000000,"scopes":["user:inference"],"subscriptionType":"max"}}"#;
const ROTATED: &str = r#"{"claudeAiOauth":{"accessToken":"at-2","refreshToken":"rt-2","expiresAt":1900000500000,"scopes":["user:inference"],"subscriptionType":"max"}}"#;

fn plugin() -> PluginId {
    PluginId::from_static("claude")
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(PluginKind::Subscription, plugin(), dir.path())
}

fn host_tokio_clock(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for_with_tokio_monotonic(PluginKind::Subscription, plugin(), dir.path())
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let mut raw = RawTable::new();
    raw.insert("command", RawValue::String(PROGRAM.to_owned()));
    let cfg = ClaudeFactory.validate(&raw).expect("config");
    ClaudeFactory.build(&cfg, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(plugin())
        .with_session(SESSION)
        .expect("session")
}

fn state(host: &TestHost) -> PathBuf {
    host.context().dirs().state_dir().expect("state dir")
}

fn sign_in_dir(host: &TestHost) -> PathBuf {
    state(host).join("signin").join(SESSION)
}

fn config_dir(spawn: &PipedSpawn) -> PathBuf {
    PathBuf::from(spawn.env_var(cli::CONFIG_DIR_VAR).expect("config dir set"))
}

fn profile_json() -> String {
    format!(
        r#"{{"oauthAccount":{{"accountUuid":"{UUID}","emailAddress":"{EMAIL}","organizationName":"Ada's Org"}},"numStartups":1}}"#
    )
}

/// A login that prints its link, then signs in when a code arrives.
fn login_script(write_profile: bool) -> PipedScript {
    PipedScript::new()
        .line("\u{1b}[1mWelcome to Claude Code\u{1b}[0m")
        .line("Browser didn't open? Use the url below to sign in:")
        .line(format!("\u{1b}[4m{LINK}\u{1b}[24m"))
        .line("Paste code here if prompted >")
        .on_stdin(move |spawn, _code| {
            let dir = config_dir(spawn);
            std::fs::write(dir.join(creds::CREDENTIALS_FILE), CREDENTIALS).expect("credentials");
            if write_profile {
                std::fs::write(dir.join(creds::PROFILE_FILE), profile_json()).expect("profile");
            }
            vec!["Login successful.".to_owned()]
        })
}

fn start(host: &TestHost, sub: &Arc<dyn Subscription>) -> SignInChallenge {
    block_on(sub.start_sign_in(&ctx(), host.context())).expect("start")
}

fn paste(
    host: &TestHost,
    sub: &Arc<dyn Subscription>,
    code: &str,
) -> Result<AccountId, PluginErrorKind> {
    block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from(code.to_owned())),
        host.context(),
    ))
    .map_err(|error| error.kind())
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn store(host: &TestHost, id: &AccountId, blob: &SecretObject) {
    let account = host
        .context()
        .accounts()
        .account(id.clone(), EMAIL)
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
}

fn signed_in_blob() -> SecretObject {
    SecretObject::new()
        .with_number(creds::VERSION, 1)
        .with_text(creds::CREDENTIALS, CREDENTIALS)
        .with_text(
            creds::PROFILE,
            &format!(r#"{{"accountUuid":"{UUID}","emailAddress":"{EMAIL}"}}"#),
        )
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn login_spawned_without_shell_with_scrubbed_env() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(true));
    start(&host, &sub);

    let [spawn] = host.process().piped_spawns().try_into().expect("one spawn");
    assert_eq!(spawn.program, PathBuf::from(PROGRAM));
    assert_eq!(spawn.arg_strings(), ["auth", "login", "--claudeai"]);
    let signin = sign_in_dir(&host);
    // Only the config directory is set; the kernel clears everything else.
    assert_eq!(
        spawn.env,
        vec![(cli::CONFIG_DIR_VAR.into(), signin.clone().into_os_string())]
    );
    assert_eq!(spawn.cwd, signin);
    assert!(signin.is_dir());
    #[cfg(unix)]
    assert_eq!(mode(&signin), 0o700);
}

#[test]
fn printed_url_becomes_challenge() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(true));
    let SignInChallenge::Instructions { text } = start(&host, &sub) else {
        panic!("instructions");
    };
    assert!(text.contains(&format!("\n  {LINK}\n")), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(text.contains("paste the code"), "{text}");
}

#[test]
fn pasted_code_written_to_stdin() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(true));
    start(&host, &sub);
    paste(&host, &sub, "  code-1#state-1 \n").expect("signed in");
    assert_eq!(
        host.process().stdin_writes(),
        vec![b"code-1#state-1\n".to_vec()]
    );
}

#[test]
fn credentials_stored_and_label_is_email() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(true));
    start(&host, &sub);
    let id = paste(&host, &sub, "code-1#state-1").expect("signed in");

    assert_eq!(id, account_id_from(&["claude", UUID]));
    let accounts = block_on(host.context().accounts().list()).expect("list");
    let [account] = accounts.as_slice() else {
        panic!("one account: {accounts:?}");
    };
    assert_eq!(account.label(), EMAIL);
    let blob = stored(&host, &id);
    assert_eq!(blob.text(creds::CREDENTIALS), Some(CREDENTIALS));
    assert_eq!(blob.number(creds::VERSION), Some(1));
    assert_eq!(blob.text("plan"), Some("max"));
    let profile = creds::Profile::parse(blob.text(creds::PROFILE).expect("profile"));
    assert_eq!(profile.email(), Some(EMAIL));
    assert_eq!(profile.uuid(), Some(UUID));
    // The sign-in directory and its copy of the tokens are gone; no
    // `auth status` was needed.
    assert!(!sign_in_dir(&host).exists());
    assert_eq!(host.process().piped_spawns().len(), 1);
    assert_eq!(host.process().piped_dropped(), 1);
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(status, AccountStatus::Ready);
}

#[test]
fn email_comes_from_auth_status_when_the_profile_has_none() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(false));
    host.process().script_piped(PipedScript::new().exits(|_| {
        vec![
            "{".to_owned(),
            format!(r#""loggedIn": true, "email": "{EMAIL}", "orgName": "Ada's Org""#),
            "}".to_owned(),
        ]
    }));
    start(&host, &sub);
    let id = paste(&host, &sub, "code-1#state-1").expect("signed in");

    assert_eq!(id, account_id_from(&["claude-email", EMAIL]));
    let spawns = host.process().piped_spawns();
    assert_eq!(spawns.len(), 2);
    assert_eq!(spawns[1].arg_strings(), ["auth", "status", "--json"]);
    assert_eq!(spawns[1].env, spawns[0].env);
    let blob = stored(&host, &id);
    let profile = creds::Profile::parse(blob.text(creds::PROFILE).expect("profile"));
    assert_eq!(profile.email(), Some(EMAIL));
    assert_eq!(
        profile
            .fields
            .get("organizationName")
            .and_then(|v| v.as_str()),
        Some("Ada's Org")
    );
}

#[test]
fn email_is_lowercased_for_the_fallback_id() {
    let mut profile = creds::Profile::default();
    profile.add_status(&creds::parse_status(r#"{"email":"Ada@Example.Test"}"#).expect("json"));
    assert_eq!(
        profile.account_id().expect("id"),
        account_id_from(&["claude-email", "ada@example.test"])
    );
    assert_eq!(profile.email(), Some("Ada@Example.Test"));
}

#[test]
fn no_email_anywhere_fails_and_cleans_up() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(false));
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec!["not json".to_owned()]));
    start(&host, &sub);
    assert_eq!(
        paste(&host, &sub, "code-1#state-1"),
        Err(PluginErrorKind::Auth)
    );
    assert!(!sign_in_dir(&host).exists());
    assert!(
        block_on(host.context().accounts().list())
            .expect("list")
            .is_empty()
    );
}

#[test]
fn malformed_code_is_refused_before_claude_sees_it() {
    for code in ["", "no-state", "#state", "code#", "code#a#b", "code #state"] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        host.process().script_piped(login_script(true));
        start(&host, &sub);
        assert_eq!(
            paste(&host, &sub, code),
            Err(PluginErrorKind::Config),
            "{code:?}"
        );
        assert!(host.process().stdin_writes().is_empty(), "{code:?}");
        assert_eq!(host.process().piped_dropped(), 1, "{code:?}");
        assert!(!sign_in_dir(&host).exists(), "{code:?}");
    }
}

#[test]
fn pasted_code_shape() {
    assert_eq!(pasted_code(" abc#def\n"), Some("abc#def"));
    assert_eq!(pasted_code("abc#def\u{7}"), None);
    assert_eq!(pasted_code("abc\t#def"), None);
}

#[test]
fn login_that_prints_no_link_fails_with_its_last_line() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(
        PipedScript::new()
            .line("Checking connectivity...")
            .exits(|_| vec!["\u{1b}[31mError: network unreachable\u{1b}[0m".to_owned()]),
    );
    let error = block_on(sub.start_sign_in(&ctx(), host.context())).expect_err("no link");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(
        error
            .to_string()
            .contains("it said: Error: network unreachable"),
        "{error}"
    );
    assert!(!sign_in_dir(&host).exists());
    assert_eq!(host.process().piped_dropped(), 1);
    // Nothing is pending afterwards.
    assert_eq!(paste(&host, &sub, "a#b"), Err(PluginErrorKind::Config));
}

#[test]
fn spawn_failure_removes_the_sign_in_dir() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    // No script: the recording backend refuses the spawn.
    assert!(block_on(sub.start_sign_in(&ctx(), host.context())).is_err());
    assert!(!sign_in_dir(&host).exists());
}

#[test]
fn login_that_exits_without_credentials_says_why() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(
        PipedScript::new()
            .line(LINK)
            .on_stdin(|_, _| vec!["OAuth error: invalid code".to_owned()]),
    );
    start(&host, &sub);
    let error = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from("bad#code".to_owned())),
        host.context(),
    ))
    .expect_err("not signed in");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(
        error.to_string().contains("OAuth error: invalid code"),
        "{error}"
    );
    assert!(!sign_in_dir(&host).exists());
}

#[test]
fn awaiting_the_browser_reads_the_credentials_claude_wrote() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    // Claude Code's own browser callback finished the sign-in: it exits
    // without input.
    host.process()
        .script_piped(PipedScript::new().line(LINK).exits(|spawn| {
            let dir = config_dir(spawn);
            std::fs::write(dir.join(creds::CREDENTIALS_FILE), CREDENTIALS).expect("credentials");
            std::fs::write(dir.join(creds::PROFILE_FILE), profile_json()).expect("profile");
            vec!["Login successful.".to_owned()]
        }));
    start(&host, &sub);
    let id = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect("signed in");
    assert_eq!(id, account_id_from(&["claude", UUID]));
    assert!(host.process().stdin_writes().is_empty());
}

#[test]
fn a_callback_proof_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(true));
    start(&host, &sub);
    let error = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Callback {
            code: SecretString::from("c".to_owned()),
            state: "s".to_owned(),
        },
        host.context(),
    ))
    .expect_err("refused");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(!sign_in_dir(&host).exists());
}

#[test]
fn cancel_kills_child_and_removes_dir() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(true));
    start(&host, &sub);
    assert!(sign_in_dir(&host).is_dir());
    assert_eq!(host.process().piped_dropped(), 0);

    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel");
    assert_eq!(host.process().piped_dropped(), 1);
    assert!(!sign_in_dir(&host).exists());
    assert_eq!(paste(&host, &sub, "a#b"), Err(PluginErrorKind::Config));
    // A second cancel is harmless.
    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel again");
}

#[test]
fn restarting_a_session_kills_the_first_login() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(true));
    host.process().script_piped(login_script(true));
    start(&host, &sub);
    std::fs::write(sign_in_dir(&host).join("stale"), "x").expect("stale file");
    start(&host, &sub);
    assert_eq!(host.process().piped_dropped(), 1);
    assert!(!sign_in_dir(&host).join("stale").exists());
    paste(&host, &sub, "code-1#state-1").expect("signed in");
    assert_eq!(host.process().stdin_writes().len(), 1);
}

fn status_script(
    rotate: Option<&'static str>,
    logged_in: bool,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
) -> PipedScript {
    PipedScript::new().exits(move |spawn| {
        let dir = config_dir(spawn);
        let file = dir.join(creds::CREDENTIALS_FILE);
        #[cfg(unix)]
        {
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&file), 0o600);
        }
        let profile = std::fs::read_to_string(dir.join(creds::PROFILE_FILE)).expect("settings");
        assert!(profile.contains(EMAIL), "{profile}");
        seen.lock()
            .expect("seen")
            .push(std::fs::read_to_string(&file).expect("materialized"));
        if let Some(rotated) = rotate {
            std::fs::write(&file, rotated).expect("rotate");
        }
        vec![format!(
            r#"{{"loggedIn": {logged_in}, "email": "{EMAIL}"}}"#
        )]
    })
}

/// What changes when a file is written again: inode, length and mtime.
fn file_stamp(path: &Path) -> (u64, u64, std::time::SystemTime) {
    let meta = std::fs::metadata(path).expect("metadata");
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    (inode, meta.len(), meta.modified().expect("mtime"))
}

#[test]
fn rotated_credentials_committed_once() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

    host.process()
        .script_piped(status_script(Some(ROTATED), true, Arc::clone(&seen)));
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let blob = stored(&host, &id);
    assert_eq!(blob.text(creds::CREDENTIALS), Some(ROTATED));
    assert!(blob.text(creds::PROFILE).is_some(), "the profile is kept");

    // The next run gets the rotated file, and an unchanged file is not
    // written back: the account file is left as it was.
    let accounts_file = dir.path().join("accounts.json");
    let before = file_stamp(&accounts_file);
    host.process()
        .script_piped(status_script(None, true, Arc::clone(&seen)));
    block_on(sub.refresh(&id, host.context())).expect("refresh again");
    assert_eq!(file_stamp(&accounts_file), before, "no commit");
    assert_eq!(*seen.lock().expect("seen"), vec![CREDENTIALS, ROTATED]);
    assert_eq!(stored(&host, &id).text(creds::CREDENTIALS), Some(ROTATED));

    let [first, second] = host.process().piped_spawns().try_into().expect("two runs");
    let account_dir = state(&host).join("accounts").join(id.as_str());
    assert_eq!(config_dir(&first), account_dir);
    assert_eq!(first.cwd, account_dir);
    assert_eq!(second.arg_strings(), ["auth", "status", "--json"]);
    // The tokens rest only in the account store.
    assert!(!account_dir.join(creds::CREDENTIALS_FILE).exists());
    assert!(account_dir.join(creds::PROFILE_FILE).exists());
}

#[test]
fn a_rotated_file_that_is_not_credentials_is_not_kept() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    host.process()
        .script_piped(status_script(Some("{}"), true, seen));
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    assert_eq!(
        stored(&host, &id).text(creds::CREDENTIALS),
        Some(CREDENTIALS)
    );
}

#[test]
fn signed_out_status_marks_the_account_lapsed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    host.process()
        .script_piped(status_script(Some(ROTATED), false, seen));

    let error = block_on(sub.refresh(&id, host.context())).expect_err("signed out");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(stored(&host, &id).flag(creds::LAPSED));
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(
        status,
        AccountStatus::NeedsSignIn {
            reason: creds::LAPSED_REASON.to_owned()
        }
    );
    // A lapsed account runs nothing.
    let again = block_on(sub.refresh(&id, host.context())).expect_err("still lapsed");
    assert_eq!(again.kind(), PluginErrorKind::Auth);
    assert_eq!(host.process().piped_spawns().len(), 1);
}

#[test]
fn status_that_fails_keeps_the_account_signed_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec!["Error: something broke".to_owned()]));
    let error = block_on(sub.refresh(&id, host.context())).expect_err("no status");
    assert_eq!(error.kind(), PluginErrorKind::Io);
    let blob = stored(&host, &id);
    assert!(!blob.flag(creds::LAPSED));
    assert_eq!(blob.text(creds::CREDENTIALS), Some(CREDENTIALS));
}

#[test]
fn status_that_fails_after_rotating_keeps_the_rotated_file() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process()
        .script_piped(PipedScript::new().exits(|spawn| {
            std::fs::write(config_dir(spawn).join(creds::CREDENTIALS_FILE), ROTATED)
                .expect("rotate");
            vec!["Error: something broke".to_owned()]
        }));
    let error = block_on(sub.refresh(&id, host.context())).expect_err("no status");
    assert_eq!(error.kind(), PluginErrorKind::Io);
    let blob = stored(&host, &id);
    assert!(!blob.flag(creds::LAPSED));
    assert_eq!(blob.text(creds::CREDENTIALS), Some(ROTATED));
}

#[test]
fn a_missing_program_fails_the_refresh() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    // Nothing is scripted, so the spawn is refused.
    let error = block_on(sub.refresh(&id, host.context())).expect_err("no child");
    assert_eq!(error.kind(), PluginErrorKind::Io);
    assert_eq!(
        stored(&host, &id).text(creds::CREDENTIALS),
        Some(CREDENTIALS)
    );
    let account_dir = state(&host).join("accounts").join(id.as_str());
    assert!(!account_dir.join(creds::CREDENTIALS_FILE).exists());
}

#[test]
fn status_without_a_refresh_token_needs_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    let blob = signed_in_blob().with_text(
        creds::CREDENTIALS,
        r#"{"claudeAiOauth":{"accessToken":"at-1"}}"#,
    );
    store(&host, &id, &blob);
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(
        matches!(status, AccountStatus::NeedsSignIn { .. }),
        "{status:?}"
    );

    store(
        &host,
        &id,
        &SecretObject::new().with_text(creds::CREDENTIALS, "{}"),
    );
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(
        matches!(status, AccountStatus::NeedsSignIn { .. }),
        "{status:?}"
    );
}

#[test]
fn config_needs_an_absolute_command() {
    for value in [RawValue::String("claude".to_owned()), RawValue::Integer(1)] {
        let mut raw = RawTable::new();
        raw.insert("command", value);
        assert!(ClaudeFactory.validate(&raw).is_err());
    }
}

#[test]
fn strip_ansi_removes_csi_osc_and_controls() {
    assert_eq!(cli::strip_ansi("\u{1b}[1;31mred\u{1b}[0m"), "red");
    assert_eq!(
        cli::strip_ansi("\u{1b}]8;;https://x.test\u{7}link\u{1b}]8;;\u{1b}\\ end"),
        "link end"
    );
    assert_eq!(cli::strip_ansi("a\rb\u{8}c\td"), "abc\td");
    // Other escapes are two bytes: `ESC 7` saves the cursor.
    assert_eq!(cli::strip_ansi("\u{1b}7plain\u{1b}"), "plain");
}

#[test]
fn find_link_takes_the_first_https_word() {
    assert_eq!(
        cli::find_link("visit: https://claude.ai/a?b=c then https://other.test"),
        Some("https://claude.ai/a?b=c")
    );
    assert_eq!(cli::find_link("http://claude.ai/insecure"), None);
    assert_eq!(cli::find_link("https:// nothing"), None);
}

#[test]
fn d27_7_write_agent_sign_in_creates_claude_files() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    block_on(sub.write_agent_sign_in(&id, &agent_home, host.context())).expect("switch");
    assert!(agent_home.join(creds::CREDENTIALS_FILE).is_file());
    assert!(agent_home.join(creds::PROFILE_FILE).is_file());
}

fn warmup_script(rotate: Option<&'static str>, lines: Vec<String>) -> PipedScript {
    PipedScript::new().on_stdin(move |spawn, stdin| {
        assert_eq!(stdin, b"hi\n");
        let dir = config_dir(spawn);
        let file = dir.join(creds::CREDENTIALS_FILE);
        if let Some(rotated) = rotate {
            std::fs::write(&file, rotated).expect("rotate");
        }
        lines
    })
}

#[test]
fn a3a_19_warmup_commits_rotation_and_removes_credentials_file() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let _sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process().script_piped(warmup_script(
        Some(ROTATED),
        vec![r#"{"type":"result","is_error":false,"result":"Hi"}"#.to_owned()],
    ));
    block_on(_sub.daily_warmup(&id, host.context())).expect("warmup");
    assert_eq!(stored(&host, &id).text(creds::CREDENTIALS), Some(ROTATED));
    let account_dir = state(&host).join("accounts").join(id.as_str());
    assert!(!account_dir.join(creds::CREDENTIALS_FILE).exists());
    assert!(!account_dir.join("warmup-run").exists());
}

#[test]
fn a3a_19_warmup_commits_rotation_when_the_run_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process().script_piped(warmup_script(
        Some(ROTATED),
        vec![r#"{"type":"result","is_error":true,"result":"nope"}"#.to_owned()],
    ));
    let error = block_on(sub.daily_warmup(&id, host.context())).expect_err("refused");
    assert_eq!(error.kind(), PluginErrorKind::Transport);
    assert_eq!(stored(&host, &id).text(creds::CREDENTIALS), Some(ROTATED));
    let account_dir = state(&host).join("accounts").join(id.as_str());
    assert!(!account_dir.join(creds::CREDENTIALS_FILE).exists());
}

#[test]
fn a3a_20_warmup_empty_stdout_fails_and_removes_credentials_file() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process().script_piped(warmup_script(None, Vec::new()));
    let error = block_on(sub.daily_warmup(&id, host.context())).expect_err("empty");
    assert_eq!(error.kind(), PluginErrorKind::Transport);
    assert!(error.to_string().contains("produced no output"), "{error}");
    let account_dir = state(&host).join("accounts").join(id.as_str());
    assert!(!account_dir.join(creds::CREDENTIALS_FILE).exists());
}

#[test]
fn a3a_24_warmup_empty_stdout_exits_zero_still_fails_with_no_output_message() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process()
        .script_piped(PipedScript::new().on_stdin(|_, _| Vec::new()));
    let error = block_on(sub.daily_warmup(&id, host.context())).expect_err("empty");
    assert!(error.to_string().contains("produced no output"), "{error}");
}

#[test]
fn a3a_25_warmup_timeout_fails_even_when_stdout_has_a_result_line() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process().script_piped(
        PipedScript::new().line(r#"{"type":"result","is_error":false,"result":"Hi"}"#),
    );
    let error = block_on(sub.daily_warmup(&id, host.context())).expect_err("timeout");
    assert!(error.to_string().contains("did not finish"), "{error}");
}

#[test]
fn d36_1_switch_waits_for_refresh_lock_then_succeeds() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    let host = host(&dir);
    let sub = subscription(&host);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    let lock_path = agent_home.join(".oauth_refresh.lock");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir(&lock_path);
        let _ = ready_tx.send(());
        let _ = start_rx.recv();
        let _ = std::fs::remove_dir(&lock_path);
    });
    ready_rx.recv().expect("lock held");
    crate::lock::test_lock_timing(
        Duration::from_secs(2),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let write = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let _ = start_tx.send(());
    });
    block_on(sub.write_agent_sign_in(&spare, &agent_home, host.context())).expect("write");
    write.join().expect("join");
    assert!(agent_home.join(creds::CREDENTIALS_FILE).is_file());
}

#[test]
fn d36_2_held_refresh_lock_times_out_with_nothing_written() {
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    std::fs::write(agent_home.join(creds::CREDENTIALS_FILE), "live").expect("live");
    let host = host(&dir);
    let sub = subscription(&host);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    let lock_path = agent_home.join(".oauth_refresh.lock");
    let _hold = std::fs::create_dir(&lock_path);
    crate::lock::test_lock_timing(
        Duration::from_millis(500),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let error = block_on(sub.write_agent_sign_in(&spare, &agent_home, host.context()))
        .expect_err("timeout");
    assert!(
        error.to_string().contains("refreshing its sign-in"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(agent_home.join(creds::CREDENTIALS_FILE)).expect("read"),
        "live"
    );
}

#[test]
fn d36_3_stale_refresh_lock_is_taken_over() {
    use std::time::{Duration, SystemTime};

    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    let lock_path = agent_home.join(".oauth_refresh.lock");
    std::fs::create_dir(&lock_path).expect("lock");
    let file = crate::lock::open_dir_for_times(&lock_path).expect("open");
    file.set_modified(SystemTime::now() - Duration::from_secs(120))
        .expect("backdate");
    crate::lock::test_lock_timing(
        Duration::from_secs(1),
        Duration::from_millis(50),
        Duration::from_secs(10),
    );
    let _locks = block_on(crate::lock::refresh_locks(&agent_home)).expect("take stale");
}

#[test]
fn d36_4_refresh_locks_release_in_reverse_order() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    {
        let _locks = block_on(crate::lock::refresh_locks(&agent_home)).expect("locks");
    }
    assert_eq!(crate::lock::take_test_releases(), ["legacy", "refresh"]);
}

#[test]
fn d36_5_switch_waits_for_legacy_realpath_lock_then_succeeds() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    let host = host(&dir);
    let sub = subscription(&host);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    let resolved = std::fs::canonicalize(&agent_home).expect("canonical");
    let lock_path = PathBuf::from(format!("{}.lock", resolved.display()));
    let (ready_tx, ready_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir(&lock_path);
        let _ = ready_tx.send(());
        let _ = start_rx.recv();
        let _ = std::fs::remove_dir(&lock_path);
    });
    ready_rx.recv().expect("lock held");
    crate::lock::test_lock_timing(
        Duration::from_secs(2),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let _ = start_tx.send(());
    });
    block_on(sub.write_agent_sign_in(&spare, &agent_home, host.context())).expect("write");
    release.join().expect("join");
}

#[test]
fn d36_6_switch_waits_for_profile_save_lock_then_succeeds() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    std::fs::write(agent_home.join(creds::PROFILE_FILE), profile_json()).expect("profile");
    let profile = std::fs::canonicalize(agent_home.join(creds::PROFILE_FILE)).expect("profile");
    let lock_path = PathBuf::from(format!("{}.lock", profile.display()));
    let host = host(&dir);
    let sub = subscription(&host);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    let (ready_tx, ready_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir(&lock_path);
        let _ = ready_tx.send(());
        let _ = start_rx.recv();
        let _ = std::fs::remove_dir(&lock_path);
    });
    ready_rx.recv().expect("lock held");
    crate::lock::test_lock_timing(
        Duration::from_secs(2),
        Duration::from_secs(60),
        Duration::from_millis(50),
    );
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        let _ = start_tx.send(());
    });
    block_on(sub.write_agent_sign_in(&spare, &agent_home, host.context())).expect("write");
    release.join().expect("join");
}

#[test]
fn d36_7_switch_waits_for_account_run_mark_then_succeeds() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    let host = host(&dir);
    let sub = subscription(&host);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    let account_dir = crate::account::account_dir(host.context(), &spare).expect("dir");
    std::fs::create_dir_all(&account_dir).expect("account dir");
    let ctx = host.context().clone();
    let spare_id = spare.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _hold = crate::run_lock::mark(&spare_id, &ctx).expect("mark");
        let _ = ready_tx.send(());
        let _ = start_rx.recv();
    });
    ready_rx.recv().expect("mark held");
    crate::run_lock::test_run_wait(Duration::from_secs(2));
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let _ = start_tx.send(());
    });
    block_on(sub.write_agent_sign_in(&spare, &agent_home, host.context())).expect("write");
    release.join().expect("join");
}

#[test]
fn d36_8_concurrent_switches_on_current_thread_do_not_block_each_other() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let home_a = dir.path().join("home-a");
    let home_b = dir.path().join("home-b");
    std::fs::create_dir_all(&home_a).expect("a");
    std::fs::create_dir_all(&home_b).expect("b");
    let host = host(&dir);
    let sub = subscription(&host);
    let id_a = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-111111111111"]);
    let id_b = account_id_from(&["claude", "bbbbbbbb-cccc-dddd-eeee-222222222222"]);
    store(&host, &id_a, &signed_in_blob());
    store(&host, &id_b, &signed_in_blob());

    let lock_path = home_a.join(".oauth_refresh.lock");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir(&lock_path);
        let _ = ready_tx.send(());
        let _ = release_rx.recv();
        let _ = std::fs::remove_dir(&lock_path);
    });
    ready_rx.recv().expect("lock held");

    crate::lock::test_lock_timing(
        Duration::from_secs(3),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );

    let ctx = host.context().clone();
    let (b_done_tx, b_done_rx) = mpsc::channel();
    let results = block_on(async {
        hofund_kit_subscription::join_all(vec![
            Box::pin(async {
                let result = sub.write_agent_sign_in(&id_b, &home_b, &ctx).await;
                let _ = b_done_tx.send(());
                let _ = release_tx.send(());
                result
            }),
            Box::pin(sub.write_agent_sign_in(&id_a, &home_a, &ctx)),
        ])
        .await
    });
    b_done_rx
        .recv_timeout(Duration::from_millis(800))
        .expect("second switch finished while the first waited on the lock");
    results[0].as_ref().expect("switch to home b");
    results[1].as_ref().expect("switch to home a");
}

#[test]
fn d36_14_two_marks_with_same_seq_different_pids_succeed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    let account_dir = crate::account::account_dir(host.context(), &spare).expect("dir");
    std::fs::create_dir_all(&account_dir).expect("account dir");
    crate::run_lock::test_mark_identity(100, 7);
    let first = crate::run_lock::mark(&spare, host.context()).expect("first mark");
    crate::run_lock::test_mark_identity(200, 7);
    let second = crate::run_lock::mark(&spare, host.context()).expect("second mark");
    drop(first);
    drop(second);
    crate::run_lock::clear_test_mark_identity();
}

#[test]
fn d36_16_credentials_write_refuses_a_token_rotated_under_it() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join(creds::CREDENTIALS_FILE);
    std::fs::write(&path, ROTATED).expect("live");
    let error = creds::write_credentials_cas(&path, CREDENTIALS, Some("rt-1"))
        .expect_err("rotated token must block the write");
    assert!(error.to_string().contains("refreshed its sign-in"));
    assert_eq!(std::fs::read_to_string(&path).expect("read"), ROTATED);
    creds::write_credentials_cas(&path, CREDENTIALS, Some("rt-2")).expect("unchanged token");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), CREDENTIALS);
}

#[test]
fn d36_17_switch_fails_when_run_mark_held_past_wait_limit() {
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    let host = host(&dir);
    let sub = subscription(&host);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    let account_dir = crate::account::account_dir(host.context(), &spare).expect("dir");
    std::fs::create_dir_all(&account_dir).expect("account dir");
    let _hold = crate::run_lock::mark(&spare, host.context()).expect("mark");
    crate::run_lock::test_run_wait(Duration::from_millis(400));
    let error = block_on(sub.write_agent_sign_in(&spare, &agent_home, host.context()))
        .expect_err("run mark timeout");
    crate::run_lock::test_run_wait(Duration::ZERO);
    assert!(
        error.to_string().contains("still running on this account"),
        "{error}"
    );
    assert!(
        !agent_home.join(creds::CREDENTIALS_FILE).exists(),
        "agent home must not be written"
    );
}

#[test]
fn d36_18_switch_fails_when_profile_save_lock_held_past_wait_limit() {
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    let profile_path = agent_home.join(creds::PROFILE_FILE);
    std::fs::write(&profile_path, "before").expect("profile");
    let lock_target = std::fs::canonicalize(&profile_path).unwrap_or_else(|_| profile_path.clone());
    let lock_path = PathBuf::from(format!("{}.lock", lock_target.display()));
    let _hold = std::fs::create_dir(&lock_path);
    let host = host(&dir);
    let sub = subscription(&host);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    crate::lock::test_lock_timing(
        Duration::from_millis(400),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let error = block_on(sub.write_agent_sign_in(&spare, &agent_home, host.context()))
        .expect_err("profile lock timeout");
    crate::lock::test_lock_timing(Duration::ZERO, Duration::ZERO, Duration::ZERO);
    assert!(
        error.to_string().contains("refreshing its sign-in"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(&profile_path).expect("read profile"),
        "before"
    );
}

#[cfg(unix)]
#[test]
fn d36_19_switch_fails_when_legacy_realpath_lock_held_past_wait_limit() {
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("dir");
    let real_home = dir.path().join("real-claude");
    std::fs::create_dir_all(&real_home).expect("real home");
    let agent_home = dir.path().join("claude-link");
    symlink(&real_home, &agent_home).expect("symlink");
    let resolved = std::fs::canonicalize(&agent_home).expect("canonical");
    let legacy_lock = PathBuf::from(format!("{}.lock", resolved.display()));
    let _hold = std::fs::create_dir(&legacy_lock);
    let host = host(&dir);
    let sub = subscription(&host);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    crate::lock::test_lock_timing(
        Duration::from_millis(400),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let error = block_on(sub.write_agent_sign_in(&spare, &agent_home, host.context()))
        .expect_err("legacy lock timeout");
    crate::lock::test_lock_timing(Duration::ZERO, Duration::ZERO, Duration::ZERO);
    assert!(
        error.to_string().contains("refreshing its sign-in"),
        "{error}"
    );
    assert!(
        !agent_home.join(creds::CREDENTIALS_FILE).exists(),
        "agent home must not be written"
    );
}

#[test]
fn d36_20_mark_file_name_includes_injected_pid_and_seq() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let spare = account_id_from(&["claude", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
    store(&host, &spare, &signed_in_blob());
    let account_dir = crate::account::account_dir(host.context(), &spare).expect("dir");
    std::fs::create_dir_all(&account_dir).expect("account dir");
    crate::run_lock::test_mark_identity(4242, 9);
    let account_name = account_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("account dir name");
    let expected = account_dir
        .parent()
        .expect("accounts parent")
        .join(format!("{account_name}.run-4242-9.lock"));
    assert!(!expected.exists(), "mark must not exist before mark()");
    let mark = crate::run_lock::mark(&spare, host.context()).expect("mark");
    assert!(
        expected.is_file(),
        "mark file must be `{expected:?}`, not run-0-9"
    );
    drop(mark);
    crate::run_lock::clear_test_mark_identity();
}

#[test]
fn d36_21_heartbeat_refreshes_the_lock_directory_time() {
    use std::time::{Duration, SystemTime};

    let dir = tempfile::tempdir().expect("dir");
    let lock_path = dir.path().join(".oauth_refresh.lock");
    std::fs::create_dir(&lock_path).expect("lock");
    let old = SystemTime::now() - Duration::from_secs(600);
    crate::lock::open_dir_for_times(&lock_path)
        .expect("open")
        .set_modified(old)
        .expect("backdate");
    crate::lock::touch_dir(&lock_path);
    let modified = std::fs::metadata(&lock_path)
        .expect("meta")
        .modified()
        .expect("modified");
    assert!(modified > old + Duration::from_secs(300), "{modified:?}");
}

#[test]
fn r5_9_pending_sign_in_expires_after_idle() {
    use std::time::Duration;

    const IDLE_SESSION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = host_tokio_clock(&dir);
        let sub = subscription(&host);
        host.process().script_piped(login_script(true));
        host.process().script_piped(login_script(true));
        let idle_ctx = SignInCtx::new(plugin())
            .with_session(IDLE_SESSION)
            .expect("session");
        sub.start_sign_in(&idle_ctx, host.context())
            .await
            .expect("first start");
        let idle_dir = state(&host).join("signin").join(IDLE_SESSION);
        assert!(idle_dir.is_dir());
        advance_time(Duration::from_millis(80)).await;
        sub.start_sign_in(&ctx(), host.context())
            .await
            .expect("second start");
        assert!(
            !idle_dir.exists(),
            "idle sign-in directory should be removed"
        );
    });
}

#[test]
fn r5_10_remove_account_deletes_profile_dir() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    let account_dir = crate::account::account_dir(host.context(), &id).expect("dir");
    std::fs::create_dir_all(&account_dir).expect("create");
    block_on(sub.remove_account(&id, host.context())).expect("remove");
    assert!(!account_dir.exists());
}

#[test]
fn r5_11_import_cli_proxy_claude_json() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let text = r#"{"type":"claude","access_token":"sk-ant-oat01-old","refresh_token":"sk-ant-ort01-cpa","email":"max@example.com"}"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("import");
    assert_eq!(ids.len(), 1);
}

const SETUP_TOKEN: &str = "sk-ant-oat01-abcdefghijklmnopqrstuvwxyz0123456789";

fn usage_script(rotate_to: Option<&'static str>) -> PipedScript {
    PipedScript::new().exits(move |spawn| {
        let dir = config_dir(spawn);
        if let Some(rotated) = rotate_to {
            std::fs::write(dir.join(creds::CREDENTIALS_FILE), rotated).expect("rotate");
        }
        vec![
            "Current session: 5% used · resets 3pm (UTC)\n".to_owned(),
            "Current week (all models): 5% used · resets Nov 6 at 11am (UTC)\n".to_owned(),
        ]
    })
}

#[test]
fn r5_18_complete_sign_in_rejects_idle_pending() {
    use std::time::Duration;

    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = host_tokio_clock(&dir);
        let sub = subscription(&host);
        host.process().script_piped(login_script(true));
        sub.start_sign_in(&ctx(), host.context())
            .await
            .expect("start");
        advance_time(Duration::from_millis(80)).await;
        let error = sub
            .complete_sign_in(
                &ctx(),
                SignInProof::Token(SecretString::from("code#state".to_owned())),
                host.context(),
            )
            .await
            .expect_err("expired");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        assert!(
            error
                .to_string()
                .contains("this Claude Code sign-in timed out")
        );
    });
}

#[test]
fn r5_19_quota_removes_credentials_file() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process().script_piped(usage_script(None));
    block_on(sub.quota(&id, host.context())).expect("quota");
    let creds_path = crate::account::account_dir(host.context(), &id)
        .expect("dir")
        .join(creds::CREDENTIALS_FILE);
    assert!(!creds_path.exists());
}

#[test]
fn r5_20_quota_commits_rotated_credentials() {
    const ROTATED: &str = r#"{"claudeAiOauth":{"accessToken":"at-new","refreshToken":"rt-new","expiresAt":1900000000000}}"#;
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process().script_piped(usage_script(Some(ROTATED)));
    block_on(sub.quota(&id, host.context())).expect("quota");
    let blob = stored(&host, &id);
    assert!(
        blob.text(creds::CREDENTIALS)
            .is_some_and(|t| t.contains("rt-new"))
    );
}

#[test]
fn r5_21_setup_token_dedupes_same_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let first = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from(SETUP_TOKEN.to_owned())),
        host.context(),
    ))
    .expect("first");
    let second = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from(SETUP_TOKEN.to_owned())),
        host.context(),
    ))
    .expect("second");
    assert_eq!(first, second);
}

#[test]
fn r5_22_refresh_no_op_for_setup_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from(SETUP_TOKEN.to_owned())),
        host.context(),
    ))
    .expect("store");
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    assert!(host.process().piped_spawns().is_empty());
}

#[test]
fn r5_40_setup_token_daily_warmup_is_no_op() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from(SETUP_TOKEN.to_owned())),
        host.context(),
    ))
    .expect("store");
    block_on(sub.daily_warmup(&id, host.context())).expect("warmup");
    assert!(host.process().piped_spawns().is_empty());
}

#[test]
fn r5_23_agent_switch_refuses_setup_token() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    std::fs::create_dir_all(&agent_home).expect("home");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from(SETUP_TOKEN.to_owned())),
        host.context(),
    ))
    .expect("store");
    let error =
        block_on(sub.write_agent_sign_in(&id, &agent_home, host.context())).expect_err("switch");
    assert!(
        error.to_string().contains(crate::setup_token::AGENT_SWITCH),
        "{error}"
    );
}

#[test]
fn r5_24_setup_token_account_status_ready() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from(SETUP_TOKEN.to_owned())),
        host.context(),
    ))
    .expect("store");
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::Ready));
}

#[test]
fn r5_25_setup_token_run_env_sets_oauth_var() {
    let blob = setup_token::blob(SETUP_TOKEN);
    let env = account::run_env(&blob);
    assert_eq!(env.len(), 1);
    assert_eq!(env[0].0, cli::OAUTH_TOKEN_VAR);

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(SecretString::from(SETUP_TOKEN.to_owned())),
        host.context(),
    ))
    .expect("store");
    let account_dir = account::account_dir(host.context(), &id).expect("account dir");
    creds::private_dir(&account_dir).expect("private");
    host.process().script_piped(
        PipedScript::new().exits(|_| vec![r#"{"loggedIn":true,"email":"setup@test"}"#.to_owned()]),
    );
    let blob = stored(&host, &id);
    block_on(account::status(
        host.context(),
        &PathBuf::from(PROGRAM),
        &account_dir,
        &blob,
    ))
    .expect("status json");
    let spawn = &host.process().piped_spawns()[0];
    assert!(spawn.env.iter().any(|(k, _)| k == cli::OAUTH_TOKEN_VAR));
}

#[test]
fn r5_26_oauth_env_absent_for_oauth_account_quota() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    host.process().script_piped(usage_script(None));
    block_on(sub.quota(&id, host.context())).expect("quota");
    let spawn = &host.process().piped_spawns()[0];
    assert!(!spawn.env.iter().any(|(k, _)| k == cli::OAUTH_TOKEN_VAR));
}

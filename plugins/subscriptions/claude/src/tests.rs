//! Unit tests against recording host backends; nothing is executed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_kit_subscription::{PipedScript, PipedSpawn, TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, RawTable, RawValue,
    SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::{ClaudeFactory, cli, creds, pasted_code};

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

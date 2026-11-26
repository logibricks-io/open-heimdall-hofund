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

fn subscription_default(host: &TestHost) -> Arc<dyn Subscription> {
    let cfg = ClaudeFactory.validate(&RawTable::new()).expect("config");
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
    assert!(
        spawn
            .env
            .iter()
            .any(|(k, v)| k == cli::CONFIG_DIR_VAR && v == signin.as_os_str())
    );
    assert!(spawn.env.iter().any(|(k, _)| k == "BROWSER"));
    assert!(
        spawn
            .env
            .iter()
            .any(|(k, _)| k == hofund_plugin_sdk::opened_url::OPENED_URL_ENV)
    );
    assert_eq!(spawn.cwd, signin);
    assert!(signin.is_dir());
    #[cfg(unix)]
    assert_eq!(mode(&signin), 0o700);
}

fn wsl_discovery_script(kind: &str) -> PipedScript {
    let wsl = crate::wslrun::wsl_exe();
    let kind = kind.to_owned();
    PipedScript::new().exits(move |spawn| {
        if spawn.program != wsl {
            return Vec::new();
        }
        match (kind.as_str(), spawn.arg_strings().join(" ").as_str()) {
            ("running", "-l --running -q") => vec!["Ubuntu\r\n".to_owned()],
            ("all", "-l -q") => vec!["Ubuntu\r\n".to_owned()],
            ("ubuntu", _)
                if spawn.args.get(1).map(|a| a.to_string_lossy()) == Some("Ubuntu".into())
                    && spawn.args.get(2).map(|a| a.to_string_lossy()) == Some("-e".into()) =>
            {
                vec!["bin:/home/me/.local/bin/claude\nlpath:/home/me/.local/bin\nmount:/mnt/c/\nnet:mirrored\n".to_owned()]
            }
            _ => Vec::new(),
        }
    })
}

#[test]
fn r6_wsl_sign_in_probes_through_spawn_request() {
    let _guard = crate::wslrun::test_guard();
    crate::launcher::set_skip_native_find(true);
    crate::wslrun::set_test_on(true);
    crate::wslrun::forget();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription_default(&host);
    let wsl = crate::wslrun::wsl_exe();
    host.process().script_piped(wsl_discovery_script("running"));
    host.process().script_piped(wsl_discovery_script("all"));
    host.process().script_piped(wsl_discovery_script("ubuntu"));
    host.process().script_piped(login_script(true));
    start(&host, &sub);
    let spawns = host.process().piped_spawns();
    assert_eq!(spawns.len(), 4);
    assert!(spawns[..3].iter().all(|spawn| spawn.program == wsl));
    let login = &spawns[3];
    assert_eq!(login.program, wsl);
    crate::wslrun::set_test_on(false);
    crate::launcher::set_skip_native_find(false);
    crate::wslrun::forget();
}

#[test]
fn r6_wsl_sign_in_spawns_through_wsl_exe() {
    let _guard = crate::wslrun::test_guard();
    crate::launcher::set_skip_native_find(true);
    crate::wslrun::set_test_on(true);
    crate::wslrun::forget();
    crate::wslrun::set_run(Some(Box::new(
        |_timeout: std::time::Duration, args: &[&str]| {
            match args.join(" ") {
            args if args == "-l --running -q" => {
                Ok(b"Ubuntu\r\n".to_vec())
            }
            args if args == "-l -q" => Ok(b"Ubuntu\r\n".to_vec()),
            _ if args.get(1) == Some(&"Ubuntu") => Ok(
                b"bin:/home/me/.local/bin/claude\nlpath:/home/me/.local/bin\nmount:/mnt/c/\nnet:mirrored\n"
                    .to_vec(),
            ),
            _ => Err(()),
        }
        },
    )));
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription_default(&host);
    host.process().script_piped(login_script(true));
    start(&host, &sub);
    let [spawn] = host.process().piped_spawns().try_into().expect("one spawn");
    assert_eq!(spawn.program, crate::wslrun::wsl_exe());
    let args = spawn.arg_strings();
    assert_eq!(args[0], "-d");
    assert_eq!(args[1], "Ubuntu");
    assert_eq!(args[2], "--exec");
    assert_eq!(args.last().map(String::as_str), Some("--claudeai"));
    assert!(
        spawn.env.iter().any(|(key, _)| key == "WSLENV"),
        "WSL sign-in should cross CLAUDE_CONFIG_DIR: {spawn:?}"
    );
    crate::wslrun::set_run(None);
    crate::wslrun::set_test_on(false);
    crate::launcher::set_skip_native_find(false);
    crate::wslrun::forget();
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
    let signin = sign_in_dir(&host);
    assert!(
        spawns[0]
            .env
            .iter()
            .any(|(k, v)| k == cli::CONFIG_DIR_VAR && v == signin.as_os_str()),
        "{:?}",
        spawns[0].env
    );
    assert!(
        spawns[1]
            .env
            .iter()
            .any(|(k, v)| k == cli::CONFIG_DIR_VAR && v == signin.as_os_str()),
        "{:?}",
        spawns[1].env
    );
    assert!(
        spawns[0].env.iter().any(|(k, _)| k == "BROWSER"),
        "login sets BROWSER"
    );
    assert!(
        spawns[0]
            .env
            .iter()
            .any(|(k, _)| k == hofund_plugin_sdk::opened_url::OPENED_URL_ENV),
        "login sets the opened-URL relay"
    );
    // `auth status` must share the sign-in config dir, but must not open a
    // browser or write the relay file.
    assert!(!spawns[1].env.iter().any(|(k, _)| k == "BROWSER"));
    assert!(
        !spawns[1]
            .env
            .iter()
            .any(|(k, _)| k == hofund_plugin_sdk::opened_url::OPENED_URL_ENV)
    );
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
            issued_client_id: None,
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
fn d254_a_limit_line_counts_as_spent_and_a_401_does_not() {
    let dir = tempfile::tempdir().expect("dir");
    let limit_host = host(&dir);
    let sub = subscription(&limit_host);
    let id = account_id_from(&["claude", UUID]);
    store(&limit_host, &id, &signed_in_blob());
    limit_host
        .process()
        .script_piped(PipedScript::new().exits(|_| {
            vec!["You've hit your weekly limit · resets Nov 24, 2027 at 5am (UTC)\n".to_owned()]
        }));
    let windows = block_on(sub.quota(&id, limit_host.context())).expect("limit counts as spent");
    assert!(
        windows
            .iter()
            .any(|window| window.held() && window.used() == 100)
    );

    let denied = tempfile::tempdir().expect("dir");
    let denied_host = host(&denied);
    let sub = subscription(&denied_host);
    store(&denied_host, &id, &signed_in_blob());
    denied_host
        .process()
        .script_piped(PipedScript::new().exits(|_| vec!["HTTP 401 Unauthorized\n".to_owned()]));
    let error = block_on(sub.quota(&id, denied_host.context())).expect_err("401 stays an error");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
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
fn r5_23_agent_switch_writes_setup_token_env() {
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
    let before = agent_home.join(creds::CREDENTIALS_FILE);
    std::fs::write(&before, "stored-sign-in").expect("sentinel");
    block_on(sub.write_agent_sign_in(&id, &agent_home, host.context())).expect("switch");
    let settings = std::fs::read_to_string(agent_home.join("settings.json")).expect("settings");
    assert!(settings.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{settings}");
    assert!(settings.contains(SETUP_TOKEN), "{settings}");
    assert_eq!(
        std::fs::read_to_string(&before).expect("sentinel"),
        "stored-sign-in"
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
fn r5_59_sign_in_reads_opened_url_from_browser_relay() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let browser = dir.path().join("fake-hofund");
    std::fs::write(&browser, "#!/bin/sh\n").expect("browser stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).expect("mode");
    }
    crate::set_test_browser(Some(browser.clone()));
    let opened_link = "https://claude.ai/oauth/relay?state=relay";
    host.process()
        .script_piped(PipedScript::new().exits(move |spawn| {
            let opened = spawn
                .env
                .iter()
                .find(|(k, _)| k == hofund_plugin_sdk::opened_url::OPENED_URL_ENV)
                .map(|(_, v)| PathBuf::from(v))
                .expect("opened url env");
            assert!(
                spawn.env.iter().any(|(k, _)| k == "BROWSER"),
                "BROWSER must be set"
            );
            // Write the relay file directly: a plugin test must not call
            // std::process (check-plugins). Same effect as the BROWSER stub.
            std::fs::write(&opened, opened_link).expect("opened url");
            vec!["Opening browser…".to_owned()]
        }));
    let challenge = start(&host, &sub);
    let SignInChallenge::Instructions { text } = challenge else {
        panic!("expected instructions");
    };
    assert!(text.contains(opened_link), "{text}");
}

#[test]
fn r5_60_sign_in_records_http_proxy_for_bun() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.process().script_piped(login_script(true));
    let ctx = ctx()
        .with_option("proxy", "http://127.0.0.1:18080")
        .expect("proxy");
    block_on(sub.start_sign_in(&ctx, host.context())).expect("start");
    let spawn = &host.process().piped_spawns()[0];
    assert!(spawn.env.iter().any(|(k, v)| {
        k.to_str() == Some("HTTPS_PROXY") && v.to_string_lossy() == "http://127.0.0.1:18080"
    }));
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

mod usage_cadence_poll {
    use std::time::Duration;

    use super::*;
    use crate::usage_cadence::test_hooks::{lock, reset_hooks, set_used_since, set_wait_draw};

    const TEST_WAIT: Duration = Duration::from_secs(10 * 60);

    fn wire_active_account(host: &TestHost, agent_home: &std::path::Path, id: &AccountId) {
        crate::creds::private_dir(agent_home).expect("agent home");
        std::fs::write(agent_home.join(creds::PROFILE_FILE), profile_json()).expect("profile");
        crate::cli::test_configured_dir(Some(agent_home.to_path_buf()));
        store(host, id, &signed_in_blob());
    }

    #[test]
    fn r5_cadence_poll_1_unasked_inside_wait_keeps_prior_windows() {
        let _lock = lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        set_used_since(|_, _| false);
        let dir = tempfile::tempdir().expect("dir");
        let host = host_tokio_clock(&dir);
        let sub = subscription(&host);
        let id = account_id_from(&["claude", UUID]);
        let agent_home = dir.path().join("agent-claude");
        wire_active_account(&host, &agent_home, &id);
        host.process().script_piped(usage_script(None));
        block_on_paused(async {
            let seeded = sub.quota(&id, host.context()).await.expect("seed");
            let runs = host.process().piped_spawns().len();
            assert_eq!(runs, 1);
            let kept = sub
                .quota_poll(&id, host.context())
                .await
                .expect("kept")
                .windows()
                .to_vec();
            assert_eq!(kept[0].used(), seeded[0].used());
            advance_time(Duration::from_secs(60)).await;
            let _ = sub.quota_poll(&id, host.context()).await.expect("deferred");
            assert_eq!(host.process().piped_spawns().len(), runs);
        });
    }

    #[test]
    fn r5_cadence_poll_2_wait_due_and_use_runs_usage_once() {
        let _lock = lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        set_used_since(|_, _| true);
        let dir = tempfile::tempdir().expect("dir");
        let host = host_tokio_clock(&dir);
        let sub = subscription(&host);
        let id = account_id_from(&["claude", UUID]);
        let agent_home = dir.path().join("agent-claude");
        wire_active_account(&host, &agent_home, &id);
        host.process().script_piped(usage_script(None));
        block_on_paused(async {
            sub.quota(&id, host.context()).await.expect("seed");
            let runs = host.process().piped_spawns().len();
            advance_time(TEST_WAIT).await;
            host.process().script_piped(usage_script(None));
            let _ = sub.quota_poll(&id, host.context()).await.expect("redraw");
            assert_eq!(host.process().piped_spawns().len(), runs + 1);
        });
    }

    #[test]
    fn r5_cadence_poll_3_ask_floor_between_user_quota_calls() {
        let _lock = lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        set_used_since(|_, _| true);
        let dir = tempfile::tempdir().expect("dir");
        let host = host_tokio_clock(&dir);
        let sub = subscription(&host);
        let id = account_id_from(&["claude", UUID]);
        let agent_home = dir.path().join("agent-claude");
        wire_active_account(&host, &agent_home, &id);
        host.process().script_piped(usage_script(None));
        block_on_paused(async {
            sub.quota(&id, host.context()).await.expect("first");
            let runs = host.process().piped_spawns().len();
            sub.quota(&id, host.context()).await.expect("kept");
            assert_eq!(host.process().piped_spawns().len(), runs);
        });
    }

    #[test]
    fn r5_cadence_poll_4_saved_account_unasked_tick_skips_usage() {
        let _lock = lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        set_used_since(|_, _| false);
        let dir = tempfile::tempdir().expect("dir");
        let host = host_tokio_clock(&dir);
        let sub = subscription(&host);
        let active = account_id_from(&["claude", UUID]);
        let saved = account_id_from(&["claude", "00000000-1111-2222-3333-444455556666"]);
        let agent_home = dir.path().join("agent-claude");
        wire_active_account(&host, &agent_home, &saved);
        store(&host, &saved, &signed_in_blob());
        host.process().script_piped(usage_script(None));
        block_on(sub.quota(&saved, host.context())).expect("seed saved");
        let runs = host.process().piped_spawns().len();
        std::fs::write(
            agent_home.join(creds::PROFILE_FILE),
            format!(
                r#"{{"oauthAccount":{{"accountUuid":"{UUID}","emailAddress":"{EMAIL}","organizationName":"Ada's Org"}},"numStartups":1}}"#
            ),
        )
        .expect("switch active profile");
        store(&host, &active, &signed_in_blob());
        let _ = block_on(sub.quota_poll(&saved, host.context())).expect("kept");
        assert_eq!(host.process().piped_spawns().len(), runs);
    }
}

mod d226 {
    use super::*;

    use hofund_plugin_sdk::digest::sha256;
    use hofund_plugin_sdk::json::Value;

    const USER_TOKEN: &str = "sk-ant-oat01-users-own";
    const OTHER_TOKEN: &str = "sk-ant-oat01-other-account-token";

    fn setup_id(host: &TestHost, sub: &Arc<dyn Subscription>, token: &str) -> AccountId {
        block_on(sub.complete_sign_in(
            &ctx(),
            SignInProof::Token(SecretString::from(token.to_owned())),
            host.context(),
        ))
        .expect("setup account")
    }

    fn oauth_id(host: &TestHost) -> AccountId {
        let id = account_id_from(&["claude", UUID]);
        store(host, &id, &signed_in_blob());
        id
    }

    fn settings_value(home: &Path) -> Value {
        let text = std::fs::read_to_string(home.join("settings.json")).expect("settings");
        hofund_plugin_sdk::json::from_str(&text).expect("settings json")
    }

    fn env_token(value: &Value) -> Option<&str> {
        value
            .get("env")
            .and_then(|env| env.get(cli::OAUTH_TOKEN_VAR))
            .and_then(Value::as_str)
    }

    fn token_hash(token: &str) -> String {
        sha256(token.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn record_text(host: &TestHost) -> String {
        std::fs::read_to_string(state(host).join("claude-token-env.json")).expect("hash record")
    }

    fn write_settings(home: &Path, text: &str) {
        std::fs::create_dir_all(home).expect("home");
        std::fs::write(home.join("settings.json"), text).expect("settings");
    }

    #[test]
    fn d226_use_writes_env_token_and_keeps_other_settings() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        write_settings(
            &home,
            "{\n  // kept\n  \"model\": \"opus\",\n  \"env\": { \"FOO\": \"bar\", },\n}\n",
        );
        let host = host(&dir);
        let sub = subscription(&host);
        let id = setup_id(&host, &sub, SETUP_TOKEN);
        block_on(sub.write_agent_sign_in(&id, &home, host.context())).expect("use");
        let value = settings_value(&home);
        assert_eq!(value.get("model").and_then(Value::as_str), Some("opus"));
        assert_eq!(
            value
                .get("env")
                .and_then(|env| env.get("FOO"))
                .and_then(Value::as_str),
            Some("bar")
        );
        assert_eq!(env_token(&value), Some(SETUP_TOKEN));
        let record = record_text(&host);
        assert!(record.contains(&token_hash(SETUP_TOKEN)), "{record}");
        assert!(!record.contains(SETUP_TOKEN), "{record}");
        #[cfg(unix)]
        {
            assert_eq!(mode(&state(&host).join("claude-token-env.json")) & 0o077, 0);
        }
    }

    #[test]
    fn d226_use_leaves_a_stored_sign_in_file_alone() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        std::fs::create_dir_all(&home).expect("home");
        let creds = home.join(creds::CREDENTIALS_FILE);
        std::fs::write(&creds, "stored-sign-in").expect("creds");
        let host = host(&dir);
        let sub = subscription(&host);
        let id = setup_id(&host, &sub, SETUP_TOKEN);
        block_on(sub.write_agent_sign_in(&id, &home, host.context())).expect("use");
        assert_eq!(
            std::fs::read_to_string(&creds).expect("creds"),
            "stored-sign-in"
        );
    }

    #[test]
    fn d226_use_writes_the_env_token_without_a_named_live_profile() {
        let creds_text =
            r#"{"claudeAiOauth":{"accessToken":"at-live","refreshToken":"rt-live","expiresAt":1}}"#;
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        let id = setup_id(&host, &sub, SETUP_TOKEN);

        let over = dir.path().join("over-cap");
        std::fs::create_dir_all(&over).expect("over");
        let over_creds = over.join(creds::CREDENTIALS_FILE);
        std::fs::write(&over_creds, creds_text).expect("creds");
        let mut big = vec![b'x'; (1024 * 1024) + 1];
        big[0] = b'{';
        std::fs::write(over.join(creds::PROFILE_FILE), &big).expect("profile");
        let before = std::fs::read(&over_creds).expect("before");
        block_on(sub.write_agent_sign_in(&id, &over, host.context())).expect("over cap");
        assert_eq!(std::fs::read(&over_creds).expect("after"), before);
        assert_eq!(env_token(&settings_value(&over)), Some(SETUP_TOKEN));

        let unnamed = dir.path().join("no-oauth");
        std::fs::create_dir_all(&unnamed).expect("unnamed");
        let unnamed_creds = unnamed.join(creds::CREDENTIALS_FILE);
        std::fs::write(&unnamed_creds, creds_text).expect("creds");
        std::fs::write(unnamed.join(creds::PROFILE_FILE), r#"{"oauthAccount":{}}"#)
            .expect("profile");
        let before = std::fs::read(&unnamed_creds).expect("before");
        block_on(sub.write_agent_sign_in(&id, &unnamed, host.context())).expect("no oauth");
        assert_eq!(std::fs::read(&unnamed_creds).expect("after"), before);
        assert_eq!(env_token(&settings_value(&unnamed)), Some(SETUP_TOKEN));
    }

    #[test]
    fn d226_user_set_token_is_left_alone() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        write_settings(
            &home,
            &format!(
                r#"{{"env":{{"{key}":"{USER_TOKEN}"}}}}"#,
                key = cli::OAUTH_TOKEN_VAR
            ),
        );
        let host = host(&dir);
        let sub = subscription(&host);
        let setup = setup_id(&host, &sub, SETUP_TOKEN);
        let oauth = oauth_id(&host);
        let error = block_on(sub.write_agent_sign_in(&setup, &home, host.context()))
            .expect_err("use over a user value");
        let message = error.to_string();
        assert!(message.contains("of your own"), "{message}");
        assert!(!message.contains(SETUP_TOKEN), "{message}");
        assert!(!message.contains(USER_TOKEN), "{message}");
        assert!(!state(&host).join("claude-token-env.json").exists());
        block_on(sub.write_agent_sign_in(&oauth, &home, host.context())).expect("signed-in use");
        assert_eq!(env_token(&settings_value(&home)), Some(USER_TOKEN));
    }

    #[test]
    fn d226_user_edit_after_use_is_not_removed() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        std::fs::create_dir_all(&home).expect("home");
        let host = host(&dir);
        let sub = subscription(&host);
        let setup = setup_id(&host, &sub, SETUP_TOKEN);
        let oauth = oauth_id(&host);
        block_on(sub.write_agent_sign_in(&setup, &home, host.context())).expect("use");
        write_settings(
            &home,
            &format!(
                r#"{{"env":{{"FOO":"bar","{key}":"sk-ant-oat01-users-later"}}}}"#,
                key = cli::OAUTH_TOKEN_VAR
            ),
        );
        block_on(sub.write_agent_sign_in(&oauth, &home, host.context())).expect("switch back");
        let value = settings_value(&home);
        assert_eq!(env_token(&value), Some("sk-ant-oat01-users-later"));
        assert_eq!(
            value
                .get("env")
                .and_then(|env| env.get("FOO"))
                .and_then(Value::as_str),
            Some("bar")
        );
        let record = record_text(&host);
        assert!(record.contains(&token_hash(SETUP_TOKEN)), "{record}");
        assert!(!record.contains(SETUP_TOKEN), "{record}");
        assert!(!record.contains("sk-ant-oat01-users-later"), "{record}");
    }

    #[test]
    fn d226_clear_on_another_home_keeps_the_marker() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        let other = dir.path().join("other");
        write_settings(&home, r#"{"env":{"FOO":"bar"}}"#);
        let host = host(&dir);
        let sub = subscription(&host);
        let setup = setup_id(&host, &sub, SETUP_TOKEN);
        let oauth = oauth_id(&host);
        block_on(sub.write_agent_sign_in(&setup, &home, host.context())).expect("use");
        block_on(sub.write_agent_sign_in(&oauth, &other, host.context())).expect("other home");
        assert_eq!(env_token(&settings_value(&home)), Some(SETUP_TOKEN));
        let record = record_text(&host);
        assert!(record.contains(&token_hash(SETUP_TOKEN)), "{record}");
        let home_json = serde_json::to_string(&home.display().to_string()).expect("home json");
        assert!(record.contains(&home_json), "{record}");
        assert!(!record.contains(SETUP_TOKEN), "{record}");
        let other_settings = other.join("settings.json");
        if other_settings.is_file() {
            let text = std::fs::read_to_string(&other_settings).expect("other settings");
            assert!(!text.contains(SETUP_TOKEN), "{text}");
            assert!(!text.contains(cli::OAUTH_TOKEN_VAR));
        }
    }

    #[test]
    fn d226_unreadable_settings_blocks_the_sign_in_move() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        std::fs::create_dir_all(&home).expect("home");
        let host = host(&dir);
        let sub = subscription(&host);
        let setup = setup_id(&host, &sub, SETUP_TOKEN);
        let oauth = oauth_id(&host);
        let broken = home.join("settings.json");
        std::fs::write(&broken, "{\"env\": {").expect("broken");
        let error =
            block_on(sub.write_agent_sign_in(&setup, &home, host.context())).expect_err("use");
        assert!(error.to_string().contains("left on the account"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&broken).expect("broken"),
            "{\"env\": {"
        );
        assert!(!state(&host).join("claude-token-env.json").exists());

        write_settings(&home, r#"{"model":"opus"}"#);
        block_on(sub.write_agent_sign_in(&setup, &home, host.context())).expect("use");
        std::fs::write(&broken, "{\"env\": {").expect("broken again");
        let creds = home.join(creds::CREDENTIALS_FILE);
        std::fs::write(&creds, "KEEP").expect("sentinel");
        let error =
            block_on(sub.write_agent_sign_in(&oauth, &home, host.context())).expect_err("switch");
        assert!(error.to_string().contains("can't be read"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&broken).expect("broken"),
            "{\"env\": {"
        );
        assert_eq!(std::fs::read_to_string(&creds).expect("sentinel"), "KEEP");
        assert!(record_text(&host).contains(&token_hash(SETUP_TOKEN)));

        write_settings(
            &home,
            &format!(
                r#"{{"env":{{"FOO":"bar","{key}":"{SETUP_TOKEN}"}}}}"#,
                key = cli::OAUTH_TOKEN_VAR
            ),
        );
        block_on(sub.write_agent_sign_in(&oauth, &home, host.context())).expect("switch");
        let value = settings_value(&home);
        assert!(env_token(&value).is_none());
        assert_eq!(
            value
                .get("env")
                .and_then(|env| env.get("FOO"))
                .and_then(Value::as_str),
            Some("bar")
        );
        assert!(!state(&host).join("claude-token-env.json").exists());
        assert!(
            std::fs::read_to_string(&creds)
                .expect("creds")
                .contains("rt-1")
        );
    }

    #[test]
    fn d226_switch_back_removes_only_our_token() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        write_settings(&home, r#"{"env":{"FOO":"bar"},"model":"opus"}"#);
        let host = host(&dir);
        let sub = subscription(&host);
        let setup = setup_id(&host, &sub, SETUP_TOKEN);
        let oauth = oauth_id(&host);
        block_on(sub.write_agent_sign_in(&setup, &home, host.context())).expect("use");
        block_on(sub.write_agent_sign_in(&setup, &home, host.context())).expect("again");
        assert_eq!(env_token(&settings_value(&home)), Some(SETUP_TOKEN));
        block_on(sub.write_agent_sign_in(&oauth, &home, host.context())).expect("back");
        let value = settings_value(&home);
        assert!(env_token(&value).is_none());
        assert_eq!(
            value
                .get("env")
                .and_then(|env| env.get("FOO"))
                .and_then(Value::as_str),
            Some("bar")
        );
        assert_eq!(value.get("model").and_then(Value::as_str), Some("opus"));
        assert!(!state(&host).join("claude-token-env.json").exists());
    }

    #[test]
    fn d226_remove_clears_our_token_and_keeps_a_user_value() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        write_settings(&home, r#"{"env":{"FOO":"bar"}}"#);
        let host = host(&dir);
        let sub = subscription(&host);
        let setup = setup_id(&host, &sub, SETUP_TOKEN);
        block_on(sub.write_agent_sign_in(&setup, &home, host.context())).expect("use");
        std::fs::write(home.join("settings.json"), "{\"env\": {").expect("broken");
        let error = block_on(sub.remove_account(&setup, host.context())).expect_err("remove");
        assert!(error.to_string().contains("can't be read"), "{error}");
        assert!(!error.to_string().contains(SETUP_TOKEN), "{error}");
        assert_eq!(
            std::fs::read_to_string(home.join("settings.json")).expect("settings"),
            "{\"env\": {"
        );
        assert!(state(&host).join("claude-token-env.json").exists());

        write_settings(
            &home,
            &format!(
                r#"{{"env":{{"FOO":"bar","{key}":"{SETUP_TOKEN}"}}}}"#,
                key = cli::OAUTH_TOKEN_VAR
            ),
        );
        block_on(sub.remove_account(&setup, host.context())).expect("remove");
        let value = settings_value(&home);
        assert!(env_token(&value).is_none());
        assert_eq!(
            value
                .get("env")
                .and_then(|env| env.get("FOO"))
                .and_then(Value::as_str),
            Some("bar")
        );
        assert!(!state(&host).join("claude-token-env.json").exists());

        let other = setup_id(&host, &sub, OTHER_TOKEN);
        block_on(sub.write_agent_sign_in(&other, &home, host.context())).expect("other");
        write_settings(
            &home,
            &format!(
                r#"{{"env":{{"{key}":"{USER_TOKEN}"}}}}"#,
                key = cli::OAUTH_TOKEN_VAR
            ),
        );
        block_on(sub.remove_account(&other, host.context())).expect("remove other");
        assert_eq!(env_token(&settings_value(&home)), Some(USER_TOKEN));
    }

    #[test]
    fn d226_use_replaces_our_previous_token() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        std::fs::create_dir_all(&home).expect("home");
        let host = host(&dir);
        let sub = subscription(&host);
        let first = setup_id(&host, &sub, SETUP_TOKEN);
        let second = setup_id(&host, &sub, OTHER_TOKEN);
        block_on(sub.write_agent_sign_in(&first, &home, host.context())).expect("first");
        block_on(sub.write_agent_sign_in(&second, &home, host.context())).expect("second");
        assert_eq!(env_token(&settings_value(&home)), Some(OTHER_TOKEN));
        let record = record_text(&host);
        assert!(record.contains(&token_hash(OTHER_TOKEN)), "{record}");
        assert!(!record.contains(OTHER_TOKEN), "{record}");
        assert!(!record.contains(SETUP_TOKEN), "{record}");
    }

    #[cfg(unix)]
    #[test]
    fn d226_settings_file_is_owner_readable_only() {
        for existed in [true, false] {
            let dir = tempfile::tempdir().expect("dir");
            let home = dir.path().join("claude");
            std::fs::create_dir_all(&home).expect("home");
            let settings = home.join("settings.json");
            if existed {
                std::fs::write(&settings, r#"{"model":"opus"}"#).expect("settings");
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o644))
                    .expect("mode");
            }
            let host = host(&dir);
            let sub = subscription(&host);
            let id = setup_id(&host, &sub, SETUP_TOKEN);
            block_on(sub.write_agent_sign_in(&id, &home, host.context())).expect("use");
            assert_eq!(env_token(&settings_value(&home)), Some(SETUP_TOKEN));
            assert_eq!(mode(&settings) & 0o077, 0, "mode {:o}", mode(&settings));
        }
    }

    #[cfg(unix)]
    #[test]
    fn d226_symlink_target_receives_the_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("claude");
        let real_dir = dir.path().join("real");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&real_dir).expect("real");
        let target = real_dir.join("settings.json");
        std::fs::write(&target, r#"{"model":"opus"}"#).expect("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).expect("mode");
        std::os::unix::fs::symlink(&target, home.join("settings.json")).expect("link");
        let host = host(&dir);
        let sub = subscription(&host);
        let id = setup_id(&host, &sub, SETUP_TOKEN);
        block_on(sub.write_agent_sign_in(&id, &home, host.context())).expect("use");
        assert!(
            home.join("settings.json")
                .symlink_metadata()
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert_eq!(env_token(&settings_value(&home)), Some(SETUP_TOKEN));
        assert_eq!(mode(&target) & 0o077, 0);
        assert_eq!(
            settings_value(&home).get("model").and_then(Value::as_str),
            Some("opus")
        );
    }
}

fn claude_for(host: &TestHost) -> crate::Claude {
    crate::Claude {
        command: Some(PathBuf::from(PROGRAM)),
        flow: hofund_plugin_api::SignInFlow::Bridge {
            command: crate::COMMAND,
        },
        spawn_host: host.context().clone(),
        pending: std::sync::Mutex::new(std::collections::HashMap::new()),
        bridge: crate::bridge::Bridge::new(),
        limits: crate::limits::Store::new(),
        egress: std::sync::Arc::new(crate::egress::Egress::new(false)),
    }
}

#[test]
fn d253_agent_sign_in_switch_hands_over_the_bridge_account() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("claude");
    let host = host(&dir);
    let claude = claude_for(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    let run = claude.bridge.park_run(id.clone(), Vec::new());
    claude.bridge.ensure_session_run(run, &id, Vec::new());
    let request: hofund_plugin_api::Request = serde_json::from_value(serde_json::json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        "tools": [{"name": "read", "input_schema": {"type": "object"}}]
    }))
    .expect("request");
    claude.bridge.ended(
        run,
        &id,
        &request,
        "hello",
        crate::bridge::session::TurnStop::Stop,
        true,
    );
    assert_eq!(claude.bridge.idle_len(), 1);
    block_on(claude.write_agent_sign_in(&id, &agent_home, host.context())).expect("switch");
    assert_eq!(claude.bridge.idle_len(), 0);
}

#[test]
fn d237_header_during_usage_does_not_refresh_the_previous_reading() {
    use std::time::Duration;

    use hofund_plugin_api::QuotaWindow;

    let reset = hofund_plugin_sdk::time::parse_rfc3339("2026-11-26T13:00:00Z").expect("reset");
    let now = reset - Duration::from_secs(24 * 60 * 60);
    let earlier = now - Duration::from_secs(5 * 60 * 60);
    let dir = tempfile::tempdir().expect("dir");
    let host = TestHost::open_for_at(PluginKind::Subscription, plugin(), dir.path(), now);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    let claude = claude_for(&host);
    let session = QuotaWindow::new(
        "5 hours",
        1,
        Some(100),
        Some(earlier + Duration::from_secs(60 * 60)),
    )
    .expect("session");
    let week = QuotaWindow::new("7 days", 10, Some(100), Some(reset)).expect("week");
    claude
        .limits
        .record_usage(&id, vec![session, week], earlier, &[], None);
    claude.limits.clear_tried(&id);

    let limits = claude.limits.clone();
    let noted = id.clone();
    host.process().script_piped(
        PipedScript::new().exits(move |_| {
            let mut decoder = crate::bridge::decode::Decoder::new();
            let step = decoder
                .push_line(
                    r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"seven_day","utilization":0.12,"resetsAt":1800000000}}"#,
                )
                .expect("line");
            let crate::bridge::decode::Step::RateLimit(info) = step else {
                panic!("rate limit");
            };
            limits.note(&noted, &info, now);
            vec![
                "Current week (all models): 12% used · resets 2026-11-26T13:00:00Z\n".to_owned(),
            ]
        }),
    );

    let windows = block_on(claude.quota_windows(
        &id,
        host.context(),
        crate::usage_cadence::QuotaIntent::Asked,
    ))
    .expect("quota");
    let five = windows
        .iter()
        .find(|window| window.name() == "5 hours")
        .expect("unstarted after a stale reading");
    assert_eq!(five.used(), 0);
}

#[test]
fn d237_save_reading_reports_a_failure() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = account_id_from(&["claude", UUID]);
    store(&host, &id, &signed_in_blob());
    let account_dir = crate::account::account_dir(host.context(), &id).expect("dir");
    std::fs::create_dir_all(&account_dir).expect("account");
    std::fs::create_dir(account_dir.join(crate::limits::READING_FILE)).expect("blocker");
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec!["Current session: 5% used\n".to_owned()]));
    let err = block_on(sub.quota(&id, host.context())).expect_err("save");
    assert_eq!(err.kind(), PluginErrorKind::Io);
    assert_eq!(err.message(), "cannot store the Claude usage reading");
    assert!(!err.message().contains("at-1"));
    assert!(!err.message().contains("rt-1"));
}

use std::path::Path;
use std::sync::{Arc, Mutex};

use hofund_kit_subscription::{PipedScript, TestHost, block_on};
use hofund_plugin_api::{PluginId, PluginKind, SecretString};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential};
use crate::discover;
use crate::sign_in::{self, Lookup};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("devin"),
        dir.path(),
    )
}

fn credential(key: &str) -> Credential {
    Credential {
        key: SecretString::from(key.to_owned()),
        api_server: sign_in::SERVER.to_owned(),
        devin_api: "https://api.devin.ai".to_owned(),
        label: "old file label".to_owned(),
        plan: String::new(),
        org_id: String::new(),
        team_id: String::new(),
    }
}

fn program(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let program = dir.path().join("devin-test-binary");
    std::fs::write(&program, b"scripted by the recording host").expect("program");
    program
}

#[test]
fn status_identity_distinguishes_organizations_and_formats_tiers() {
    let status = |org: &str, org_id: &str, team_id: &str| {
        json!({
            "name": "sky",
            "email": " m@example.test ",
            "teamId": team_id,
            "teamsTier": "TEAMS_TIER_DEVIN_TEAMS_V2",
            "planStatus": {"planInfo": {
                "planName": "Max",
                "devinInfo": {"orgId": org_id, "accountDisplayName": org}
            }}
        })
    };
    let first = sign_in::who_of_status(&status("opt", "org-1", "team-1")).expect("identity");
    let second = sign_in::who_of_status(&status("lab", "org-2", "team-2")).expect("identity");
    assert_eq!(first.user, "m@example.test · opt");
    assert_eq!(first.email.as_deref(), Some("m@example.test"));
    assert_eq!(first.plan, "Devin Teams");
    assert_eq!(first.org_id.as_deref(), Some("org-1"));
    assert_eq!(first.team_id.as_deref(), Some("team-1"));
    assert_ne!(first.user, second.user);

    let mut same = status("M@EXAMPLE.TEST", "org-1", "team-1");
    same["teamsTier"] = json!("TEAMS_TIER_UNSPECIFIED");
    let same = sign_in::who_of_status(&same).expect("identity");
    assert_eq!(same.user, "m@example.test");
    assert_eq!(same.email, None);
    assert_eq!(same.plan, "Max");
    assert!(sign_in::who_of_status(&json!({"name": "  "})).is_none());

    for (tier, expected) in [
        ("TEAMS_TIER_DEVIN_PRO", "Devin Pro"),
        ("TEAMS_TIER_DEVIN_TEAMS_V2", "Devin Teams"),
        ("TEAMS_TIER_UNSPECIFIED", ""),
        ("", ""),
    ] {
        assert_eq!(sign_in::tier_name(tier), expected);
    }
}

#[test]
fn rejected_key_never_runs_the_cli() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let program = program(&dir);
    host.http().enqueue_response(
        401,
        &[],
        br#"{"code":"resource_exhausted","message":"quota exhausted"}"#,
    );
    let error = block_on(sign_in::success_with_program(
        host.context(),
        &credential("rejected-key"),
        Some(&program),
    ))
    .expect_err("rejected");
    assert!(error.to_string().contains("nobody is signed in"));
    assert!(host.process().piped_spawns().is_empty());
    assert!(
        block_on(host.context().accounts().list())
            .expect("accounts")
            .is_empty()
    );
}

#[test]
fn transport_http_json_and_empty_identity_results_are_inconclusive() {
    for kind in 0..6 {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        match kind {
            0 => host.http().enqueue_timeout("slow"),
            1 => host.http().enqueue_connect("offline"),
            2 => host.http().enqueue_response(503, &[], b"unavailable"),
            3 => host.http().enqueue_response(500, &[], b"unavailable"),
            4 => host.http().enqueue_response(200, &[], b"not JSON"),
            _ => host
                .http()
                .enqueue_response(200, &[], br#"{"userStatus":{}}"#),
        }
        assert_eq!(
            block_on(sign_in::who_by_key(
                host.context(),
                &credential("unknown-key")
            )),
            Lookup::Unknown
        );
        assert!(host.process().piped_spawns().is_empty());
    }
}

#[test]
fn status_identity_skips_cli_and_preserves_metadata() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let program = program(&dir);
    host.http().enqueue_response(
        200,
        &[],
        br#"{"userStatus":{"email":"browser@example.test","teamsTier":"TEAMS_TIER_DEVIN_PRO","teamId":"team-1","planStatus":{"planInfo":{"devinInfo":{"orgId":"org-1"}}}}}"#,
    );
    let signed_in = block_on(sign_in::success_with_program(
        host.context(),
        &credential("browser-key"),
        Some(&program),
    ))
    .expect("identity");
    assert_eq!(signed_in.label, "browser@example.test");
    assert_eq!(signed_in.plan, "Devin Pro");
    assert_eq!(signed_in.org_id, "org-1");
    assert_eq!(signed_in.team_id, "team-1");
    assert!(host.process().piped_spawns().is_empty());
    assert_eq!(
        host.http().calls()[0].uri(),
        format!(
            "{}/exa.seat_management_pb.SeatManagementService/GetUserStatus",
            sign_in::SERVER
        )
    );
}

#[test]
fn inconclusive_status_uses_only_isolated_auth_status() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let program = program(&dir);
    host.http().enqueue_response(503, &[], b"unavailable");
    let temp = Arc::new(Mutex::new(None));
    let recorded = Arc::clone(&temp);
    host.process().script_piped(PipedScript::new().exits(move |spawn| {
        assert_eq!(spawn.arg_strings(), ["auth", "status"]);
        assert_eq!(spawn.env_var("XDG_DATA_HOME"), Some(spawn.cwd.as_os_str()));
        assert_eq!(spawn.env_var("APPDATA"), Some(spawn.cwd.as_os_str()));
        let file = spawn.cwd.join("devin").join("credentials.toml");
        let body = std::fs::read_to_string(&file).expect("credentials");
        assert!(body.contains("windsurf_api_key"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).expect("file").permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&spawn.cwd).expect("root").permissions().mode() & 0o777, 0o700);
        }
        *recorded.lock().expect("temp") = Some(spawn.cwd.clone());
        vec![format!(
            "Logged in\n  File: {}\n  Email: cli@example.test\n  Tier: Devin Pro\n  Team ID: cli-team",
            file.display()
        )]
    }));
    let signed_in = block_on(sign_in::success_with_program(
        host.context(),
        &credential("fallback-key"),
        Some(&program),
    ))
    .expect("CLI identity");
    assert_eq!(signed_in.label, "cli@example.test");
    assert_eq!(signed_in.plan, "Devin Pro");
    assert_eq!(signed_in.org_id, "cli-team");
    assert_eq!(signed_in.team_id, "cli-team");
    let temp = temp.lock().expect("temp").clone().expect("CLI ran");
    assert!(!temp.exists());
    assert_eq!(host.process().piped_spawns().len(), 1);
}

#[test]
fn cli_outside_the_isolated_home_is_not_an_identity() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let program = program(&dir);
    host.http().enqueue_response(503, &[], b"unavailable");
    let other = dir.path().join("other").join("credentials.toml");
    host.process().script_piped(
        PipedScript::new()
            .line(format!(
                "Logged in\n  File: {}\n  Email: other@example.test",
                other.display()
            ))
            .exits(|_| Vec::new()),
    );
    let signed_in = block_on(sign_in::success_with_program(
        host.context(),
        &credential("browser-key"),
        Some(&program),
    ))
    .expect("unknown identity falls back");
    assert_eq!(signed_in.label, "Devin");
}

#[test]
fn cli_signed_out_rejects_and_two_unknown_sources_use_devin() {
    for mode in 0..3 {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let program = program(&dir);
        host.http().enqueue_response(503, &[], b"unavailable");
        match mode {
            0 => host.process().script_piped(
                PipedScript::new()
                    .line("Not logged in")
                    .exits(|_| Vec::new()),
            ),
            1 => host.process().script_piped(
                PipedScript::new()
                    .line("unrecognized status")
                    .exits(|_| Vec::new()),
            ),
            _ => {}
        }
        let result = block_on(sign_in::success_with_program(
            host.context(),
            &credential("unknown-key"),
            Some(&program),
        ));
        if mode == 0 {
            assert!(result.is_err());
        } else {
            assert_eq!(result.expect("generic label").label, "Devin");
        }
    }
}

#[test]
fn cli_import_runs_identity_and_marks_the_source_path() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("agent").join("devin");
    std::fs::create_dir_all(&home).expect("home");
    let path = home.join("credentials.toml");
    std::fs::write(&path, "windsurf_api_key = \"cli-import-key\"\n").expect("credentials");
    let host = host(&dir);
    host.http().enqueue_response(
        200,
        &[],
        br#"{"userStatus":{"email":"import@example.test","planStatus":{"planInfo":{"planName":"Pro"}}}}"#,
    );
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("secret")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(blob.text("label"), Some("import@example.test"));
    assert_eq!(blob.text("plan"), Some("Pro"));
    assert_eq!(blob.text("cli"), Some("true"));
    assert_eq!(blob.text("cli_path"), path.to_str());
    assert_eq!(
        blob.secret("api_key").as_ref().map(secret_text),
        Some("cli-import-key")
    );
    let stored = block_on(creds::Stored::load(host.context(), &id)).expect("stored");
    assert_eq!(stored.cli_path.as_deref(), Some(Path::new(&path)));
}

#[test]
fn organization_labels_produce_distinct_account_ids() {
    let mut first = credential("key-1");
    first.label = "same@example.test · one".to_owned();
    let mut second = credential("key-2");
    second.label = "same@example.test · two".to_owned();
    assert_ne!(creds::account_id(&first), creds::account_id(&second));
}

#[test]
fn legacy_cli_marker_without_a_path_uses_the_os_credentials_location() {
    let blob = SecretObject::new()
        .with_secret("api_key", &SecretString::from("legacy-key".to_owned()))
        .with_text("cli", "true");
    let stored = creds::parse_secret(&blob.to_secret()).expect("legacy CLI account");
    let path = stored.cli_path.expect("default CLI path");
    assert!(path.is_absolute());
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("credentials.toml")
    );
    assert_eq!(
        path.parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        Some("devin")
    );
}

#[test]
fn cli_path_with_parent_components_cannot_escape_the_temporary_home() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let program = program(&dir);
    host.http().enqueue_response(503, &[], b"unavailable");
    host.process()
        .script_piped(PipedScript::new().exits(|spawn| {
            let file = spawn
                .cwd
                .join("devin")
                .join("..")
                .join("..")
                .join("outside")
                .join("credentials.toml");
            vec![format!(
                "Logged in\n  File: {}\n  Email: outside@example.test",
                file.display()
            )]
        }));
    let signed_in = block_on(sign_in::success_with_program(
        host.context(),
        &credential("browser-key"),
        Some(&program),
    ))
    .expect("unknown identity falls back");
    assert_eq!(signed_in.label, "Devin");
}

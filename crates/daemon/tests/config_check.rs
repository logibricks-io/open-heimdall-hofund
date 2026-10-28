//! Resolved Vertex option validation through `hofund config check`'s daemon path.

use hofund_daemon::check;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
const CHILD_CONFIG: &str = "HOFUND_VERTEX_TEST_CHILD_CONFIG";
const CHILD_OUTPUT: &str = "HOFUND_VERTEX_TEST_CHILD_OUTPUT";
const ENV_URL: &str = "HOFUND_VERTEX_TEST_URL";

struct Scratch(tempfile::TempDir);

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

fn scratch() -> Scratch {
    Scratch(tempfile::tempdir().expect("temp dir"))
}

fn config(dir: &Path, option: &str) -> PathBuf {
    let path = dir.join("hofund.toml");
    fs::write(
        &path,
        format!(
            "[[providers]]\nid = \"vertex-check\"\npreset = \"google-vertex\"\n[providers.options]\nproject = \"my-project\"\n{option}\n"
        ),
    )
    .expect("config");
    path
}

#[test]
fn vertex_urls_and_credentials_are_checked() {
    let dir = scratch();
    for key in ["token_url", "iam_root"] {
        for value in ["not a url", "http://example.com/token"] {
            let path = config(&dir, &format!("{key} = {value:?}"));
            let error = check(&path).expect_err("invalid URL");
            let text = error.to_string();
            assert!(text.contains("provider `vertex-check`"), "{text}");
            assert!(text.contains(&format!("option `{key}`")), "{text}");
            assert!(text.contains("https URL"), "{text}");
        }
        for value in ["https://example.com/token", "http://127.0.0.1:9/token"] {
            let path = config(&dir, &format!("{key} = {value:?}"));
            check(&path).expect("valid URL");
        }
    }

    let missing = dir.join("missing-credentials.json");
    let path = config(
        &dir,
        &format!("credentials = {:?}", missing.to_string_lossy()),
    );
    let error = check(&path).expect_err("missing credentials");
    let text = error.to_string();
    assert!(text.contains("provider `vertex-check`"), "{text}");
    assert!(text.contains("option `credentials`"), "{text}");
    assert!(text.contains("names no file"), "{text}");
    assert!(
        text.contains(
            "create it, correct the path, or clear it to use Application Default Credentials"
        ),
        "{text}"
    );
    assert!(!text.contains(missing.to_string_lossy().as_ref()), "{text}");
    fs::write(&missing, "existence is the only preflight check").expect("credentials");
    check(&path).expect("existing credentials");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn credentials_preflight_error_keeps_its_config_key_path() {
    let dir = scratch();
    let missing = dir.join("missing-key-path-credentials.json");
    let path = config(
        &dir,
        &format!("credentials = {:?}", missing.to_string_lossy()),
    );
    let error = check(&path).expect_err("missing credentials");
    let text = error.to_string();
    assert!(text.starts_with("credentials: "), "{text}");
    assert!(text.contains("option `credentials`"), "{text}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_resolved_environment_url_is_checked() {
    let dir = scratch();
    let path = config(&dir, &format!("token_url = \"env:{ENV_URL}\""));
    let output_path = dir.join("child-error.txt");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--ignored",
            "--exact",
            "resolved_environment_url_child",
            "--nocapture",
        ])
        .env(CHILD_CONFIG, &path)
        .env(CHILD_OUTPUT, &output_path)
        .env(ENV_URL, "not a url")
        .output()
        .expect("child test");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(output_path).expect("child error");
    assert!(text.contains("provider `vertex-check`"), "{text}");
    assert!(text.contains("option `token_url`"), "{text}");
    assert!(text.contains("https URL"), "{text}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[ignore = "helper: run by a_resolved_environment_url_is_checked in a child process"]
fn resolved_environment_url_child() {
    let (Some(path), Some(output)) = (
        std::env::var_os(CHILD_CONFIG),
        std::env::var_os(CHILD_OUTPUT),
    ) else {
        return;
    };
    let error = check(Path::new(&path)).expect_err("resolved invalid URL");
    fs::write(output, error.to_string()).expect("error output");
}

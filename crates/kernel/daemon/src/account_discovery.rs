//! Account discovery from agents' own sign-in files.

use std::collections::BTreeSet;
use std::path::Path;

use crate::agent_login::agent_home;
use crate::registry::DaemonRegistry;
use crate::sign_in::{Opened, with_opened};
use crate::{DaemonError, show};
use hofund_config::Config;

/// One discovered account line for printing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoverLine {
    /// Configured subscription id.
    pub subscription: String,
    /// Display label from the plugin.
    pub label: String,
    /// Account id string.
    pub id: String,
    /// Already stored before this run.
    pub already_stored: bool,
}

/// Outcome of a discovery pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoverReport {
    /// Rows found in agent homes.
    pub lines: Vec<DiscoverLine>,
    /// Account ids newly imported.
    pub imported: Vec<String>,
}

/// Whether `HOFUND_NO_ACCOUNTS=1` disables every discovery read.
#[must_use]
pub fn discovery_blocked() -> bool {
    discovery_blocked_value(std::env::var("HOFUND_NO_ACCOUNTS").ok().as_deref())
}

fn discovery_blocked_value(value: Option<&str>) -> bool {
    matches!(value, Some("1"))
}

fn discovery_enabled_inner(
    config: &Config,
    cli_invoked: bool,
    blocked: bool,
) -> Result<(), DaemonError> {
    if blocked {
        return Err(DaemonError::new(
            "account discovery is off: HOFUND_NO_ACCOUNTS is set",
        ));
    }
    if cli_invoked || config.discover_agent_accounts {
        return Ok(());
    }
    Err(DaemonError::new(
        "account discovery is off: set discover = true under [subscriptions.options] on a subscription, or run hofund accounts discover",
    ))
}

/// Runs `hofund accounts discover` or `hofund accounts discover --add`.
///
/// # Errors
///
/// Returns [`DaemonError`] when discovery is disabled or a read fails.
pub fn accounts_discover_in(path: &Path, import: bool) -> Result<DiscoverReport, DaemonError> {
    accounts_discover_in_inner(path, import, discovery_blocked())
}

fn accounts_discover_in_inner(
    path: &Path,
    import: bool,
    blocked: bool,
) -> Result<DiscoverReport, DaemonError> {
    let config =
        hofund_config::load_path(path).map_err(|error| DaemonError::new(error.to_string()))?;
    discovery_enabled_inner(&config, true, blocked)?;
    with_opened(path, async |opened| {
        discover_opened(opened, &crate::RegistryAdapter, &config, import).await
    })
}

/// Imports agent sign-ins when enabled in config and not blocked by the environment.
///
/// # Errors
///
/// Returns [`DaemonError`] when a configured read fails.
pub fn import_if_configured(path: &Path, config: &Config) -> Result<(), DaemonError> {
    if discovery_blocked() || !config.discover_agent_accounts {
        return Ok(());
    }
    with_opened(path, async |opened| {
        discover_opened(opened, &crate::RegistryAdapter, config, true).await?;
        Ok(())
    })
}

async fn discover_opened(
    opened: &[Opened],
    registry: &dyn DaemonRegistry,
    _config: &Config,
    import: bool,
) -> Result<DiscoverReport, DaemonError> {
    let mut stored = BTreeSet::new();
    for entry in opened {
        for (id, _) in crate::sign_in::sorted_accounts(entry).await? {
            stored.insert(id);
        }
    }
    let mut report = DiscoverReport::default();
    for entry in opened {
        let Some(decl) = registry.agent_sign_in(&entry.id) else {
            continue;
        };
        let home = agent_home(&decl)?;
        let found = entry
            .instance
            .discover_agent_sign_in(&home)
            .await
            .map_err(show)?;
        for row in found {
            let id = row.id().clone();
            let already_stored = stored.contains(&id);
            report.lines.push(DiscoverLine {
                subscription: entry.id.clone(),
                label: row.label().to_owned(),
                id: id.to_string(),
                already_stored,
            });
            if import && !already_stored {
                let imported = entry
                    .instance
                    .import_agent_sign_in(&home, row.reference(), &entry.host)
                    .await
                    .map_err(show)?;
                stored.insert(imported.clone());
                report.imported.push(imported.to_string());
            }
        }
    }
    Ok(report)
}

/// Formats a discovery report for stdout.
#[must_use]
pub fn format_report(report: &DiscoverReport, import: bool) -> String {
    let mut out = String::new();
    for line in &report.lines {
        let suffix = if line.already_stored {
            " (already stored)"
        } else {
            ""
        };
        out.push_str(&format!(
            "{}  {}  {}{suffix}\n",
            line.subscription, line.label, line.id
        ));
    }
    if import {
        out.push_str(&format!("imported {} account(s)\n", report.imported.len()));
    } else {
        out.push_str(&format!("discovered {} account(s)\n", report.lines.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use hofund_plugin_sdk::oauth::account_id_from;

    use crate::agent_login::push_test_agent_homes;

    use super::*;

    fn minimal_config(discover: bool) -> Config {
        Config {
            listen: "127.0.0.1:9".parse().expect("listen"),
            token_file: "token".to_owned(),
            usage_file: "usage.db".to_owned(),
            failover: hofund_config::Failover::default(),
            connect_timeout: std::time::Duration::from_secs(1),
            read_timeout: std::time::Duration::from_secs(1),
            providers: Vec::new(),
            routes: Vec::new(),
            subscriptions: Vec::new(),
            agents: Default::default(),
            discover_agent_accounts: discover,
            warnings: Vec::new(),
            catalog: hofund_config::CatalogSettings::default(),
        }
    }

    #[test]
    fn a1_1_hofund_no_accounts_blocks_discovery() {
        assert!(discovery_blocked_value(Some("1")));
        assert!(!discovery_blocked_value(Some("")));
        assert!(discovery_enabled_inner(&minimal_config(true), true, true).is_err());
    }

    #[test]
    fn a1_2_config_off_requires_cli_invocation() {
        discovery_enabled_inner(&minimal_config(false), false, false).expect_err("off");
        discovery_enabled_inner(&minimal_config(false), true, false).expect("cli");
    }

    #[test]
    fn a1_11_config_alone_enables_discovery_without_cli() {
        discovery_enabled_inner(&minimal_config(true), false, false).expect("config");
    }

    #[test]
    fn a1_15_accounts_discover_in_honours_blocked_flag() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("hofund.toml");
        fs::write(
            &config,
            r#"
            listen = "127.0.0.1:9"
            token_file = "hofund.token"
            usage_file = "usage.db"
            [[subscriptions]]
            id = "claude"
            accounts = "*"
            "#,
        )
        .expect("config");
        fs::write(dir.path().join("hofund.token"), "test-token\n").expect("token");
        let error = accounts_discover_in_inner(&config, false, true).expect_err("blocked");
        assert!(error.to_string().contains("HOFUND_NO_ACCOUNTS"));
    }

    struct AgentEnv {
        _guard: crate::agent_login::TestHomeGuard,
        uuid: String,
    }

    /// A Claude account uuid unique to this test and process. Tests run in
    /// parallel and the macOS keychain is shared, so a fixed identity makes
    /// concurrent imports race on the same keychain item.
    fn unique_claude_uuid() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{:08x}-2222-3333-4444-{n:012x}", std::process::id())
    }

    impl AgentEnv {
        fn claude(dir: &tempfile::TempDir) -> Self {
            let home = dir.path().join("agent-claude");
            fs::create_dir_all(&home).expect("home");
            let uuid = unique_claude_uuid();
            write_claude_sign_in(&home, &uuid);
            let mut map = BTreeMap::new();
            map.insert("CLAUDE_CONFIG_DIR".to_owned(), home);
            let guard = push_test_agent_homes(map);
            Self {
                _guard: guard,
                uuid,
            }
        }

        fn codex(_dir: &tempfile::TempDir, codex_home: PathBuf) -> Self {
            write_codex_sign_in(&codex_home);
            let mut map = BTreeMap::new();
            map.insert("CODEX_HOME".to_owned(), codex_home);
            let guard = push_test_agent_homes(map);
            Self {
                _guard: guard,
                uuid: String::new(),
            }
        }
    }

    fn write_claude_sign_in(home: &Path, uuid: &str) {
        fs::write(
            home.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"access-example","refreshToken":"refresh-example"}}"#,
        )
        .expect("creds");
        fs::write(
            home.join(".claude.json"),
            format!(
                r#"{{"oauthAccount":{{"emailAddress":"user@example.test","accountUuid":"{uuid}"}}}}"#
            ),
        )
        .expect("profile");
    }

    fn write_codex_sign_in(home: &Path) {
        fs::create_dir_all(home).expect("home");
        let id = codex_id_token("u@example.test", "sub-1", "acct-1", "plus");
        let text = format!(
            r#"{{"tokens":{{"access_token":"access-example","refresh_token":"refresh-example","id_token":"{id}"}}}}"#
        );
        fs::write(home.join("auth.json"), text).expect("auth");
    }

    fn codex_b64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
            let count = chunk.len() + 1;
            for i in 0..count {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            }
        }
        out
    }

    fn codex_id_token(email: &str, sub: &str, account: &str, plan: &str) -> String {
        let claims = format!(
            r#"{{"email":"{email}","sub":"{sub}","https://api.openai.com/auth":{{"chatgpt_account_id":"{account}","chatgpt_plan_type":"{plan}"}}}}"#
        );
        format!("e30.{}.sig", codex_b64url(claims.as_bytes()))
    }

    fn write_config(dir: &Path, discover: bool) -> PathBuf {
        let path = dir.join("hofund.toml");
        let discover_block = if discover {
            "[subscriptions.options]\ndiscover = true\n"
        } else {
            ""
        };
        fs::write(
            &path,
            format!(
                r#"
                listen = "127.0.0.1:9"
                token_file = "hofund.token"
                usage_file = "usage.db"
                [[subscriptions]]
                id = "claude"
                accounts = "*"
                {discover_block}
                "#
            ),
        )
        .expect("config");
        fs::write(dir.join("hofund.token"), "test-token\n").expect("token");
        path
    }

    fn write_codex_config(dir: &Path, discover: bool) -> PathBuf {
        let path = dir.join("hofund.toml");
        let discover_block = if discover {
            "[subscriptions.options]\ndiscover = true\n"
        } else {
            ""
        };
        fs::write(
            &path,
            format!(
                r#"
                listen = "127.0.0.1:9"
                token_file = "hofund.token"
                usage_file = "usage.db"
                [[subscriptions]]
                id = "codex"
                accounts = "*"
                {discover_block}
                "#
            ),
        )
        .expect("config");
        fs::write(dir.join("hofund.token"), "test-token\n").expect("token");
        path
    }

    fn claude_account_id(uuid: &str) -> hofund_plugin_api::AccountId {
        account_id_from(&["claude", uuid])
    }

    #[test]
    fn a1_12_daemon_list_marks_already_stored_identity() {
        let dir = tempfile::tempdir().expect("dir");
        let config = write_config(dir.path(), false);
        let env = AgentEnv::claude(&dir);
        let id = claude_account_id(&env.uuid);
        let first = accounts_discover_in_inner(&config, true, false).expect("import");
        assert_eq!(first.imported.len(), 1);
        assert_eq!(first.imported[0], id.to_string());
        let listed = accounts_discover_in_inner(&config, false, false).expect("list");
        assert_eq!(listed.lines.len(), 1);
        assert!(listed.lines[0].already_stored);
        assert!(listed.imported.is_empty());
    }

    #[test]
    fn a1_13_daemon_import_adds_only_new_accounts() {
        let dir = tempfile::tempdir().expect("dir");
        let config = write_config(dir.path(), false);
        let _env = AgentEnv::claude(&dir);
        let second = accounts_discover_in_inner(&config, true, false).expect("import");
        assert_eq!(second.imported.len(), 1);
        let again = accounts_discover_in_inner(&config, true, false).expect("again");
        assert!(again.imported.is_empty());
    }

    #[test]
    fn a1_14_import_if_configured_skips_when_config_discovery_off() {
        let dir = tempfile::tempdir().expect("dir");
        let config_path = write_config(dir.path(), false);
        let _env = AgentEnv::claude(&dir);
        let config = hofund_config::load_path(&config_path).expect("load");
        import_if_configured(&config_path, &config).expect("noop");
        let list = accounts_discover_in_inner(&config_path, false, false).expect("list");
        assert_eq!(list.lines.len(), 1);
        assert!(!list.lines[0].already_stored);
    }

    #[test]
    fn a1_24_codex_discovery_honours_codex_home_override() {
        let dir = tempfile::tempdir().expect("dir");
        let default_home = dir.path().join(".codex");
        fs::create_dir_all(&default_home).expect("default");
        let held_home = dir.path().join("codex-held");
        let config = write_codex_config(dir.path(), false);
        let _env = AgentEnv::codex(&dir, held_home);
        let report = accounts_discover_in_inner(&config, false, false).expect("discover");
        assert_eq!(report.lines.len(), 1);
        assert_eq!(report.lines[0].label, "u@example.test");
        assert!(!default_home.join("auth.json").exists());
    }
}

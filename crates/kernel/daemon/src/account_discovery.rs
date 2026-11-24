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
    /// Why this row was not imported, when enrollment refused it.
    pub refused: Option<String>,
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
        let report = discover_opened(opened, &crate::RegistryAdapter, &config, import).await?;
        reject_refused_only_add(report, import)
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
        import_opened(opened, &crate::RegistryAdapter, config).await?;
        Ok(())
    })
}

pub(crate) async fn import_opened(
    opened: &[Opened],
    registry: &dyn DaemonRegistry,
    config: &Config,
) -> Result<DiscoverReport, DaemonError> {
    discover_opened(opened, registry, config, true).await
}

/// Returns the refusal when an explicit add imported nothing and every
/// discovered row was refused. A configured import keeps going.
pub(crate) fn reject_refused_only_add(
    report: DiscoverReport,
    import: bool,
) -> Result<DiscoverReport, DaemonError> {
    if import
        && report.imported.is_empty()
        && !report.lines.is_empty()
        && report.lines.iter().all(|line| line.refused.is_some())
    {
        let message = report
            .lines
            .iter()
            .find_map(|line| line.refused.clone())
            .unwrap_or_else(|| "enrollment refused the discovered account".to_owned());
        return Err(DaemonError::new(message));
    }
    Ok(report)
}

pub(crate) async fn discover_opened(
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
            let refused = if import && !already_stored {
                entry
                    .instance
                    .account_enrollment()
                    .refusal()
                    .map(str::to_owned)
            } else {
                None
            };
            if refused.is_none() && import && !already_stored {
                let imported = entry
                    .instance
                    .import_agent_sign_in(&home, row.reference(), &entry.host)
                    .await
                    .map_err(show)?;
                stored.insert(imported.clone());
                report.imported.push(imported.to_string());
            }
            report.lines.push(DiscoverLine {
                subscription: entry.id.clone(),
                label: row.label().to_owned(),
                id: id.to_string(),
                already_stored,
                refused,
            });
        }
    }
    Ok(report)
}

/// Formats a discovery report for stdout.
#[must_use]
pub fn format_report(report: &DiscoverReport, import: bool) -> String {
    let mut out = String::new();
    for line in &report.lines {
        let suffix = if let Some(reason) = &line.refused {
            format!(" (refused: {reason})")
        } else if line.already_stored {
            " (already stored)".to_owned()
        } else {
            String::new()
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

    use hofund_plugin_api::SubscriptionFactory;
    use hofund_plugin_sdk::oauth::account_id_from;

    use crate::agent_login::push_test_agent_homes;

    use super::*;

    fn minimal_config(discover: bool) -> Config {
        Config {
            listen: "127.0.0.1:9".parse().expect("listen"),
            allow_ips: Vec::new(),
            trust_proxy: false,
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
            usage: hofund_config::UsageSettings::default(),
            lan_allow: Vec::new(),
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
        _secrets: hofund_plugin_host::MemoryAccountSecretsGuard,
        uuid: String,
    }

    /// A Claude account uuid unique to this test and process keeps parallel
    /// fixtures independent even when they share an in-memory secret backend.
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
                _secrets: hofund_plugin_host::memory_account_secrets(),
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
                _secrets: hofund_plugin_host::memory_account_secrets(),
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

    const OPEN_ID: &str = "open-sub";
    const WITHDRAWN_ID: &str = "withdrawn-sub";
    const REFUSAL: &str = "new-account refusal line";

    static OPEN_SPEC: hofund_plugin_api::SubscriptionSpec =
        hofund_plugin_api::SubscriptionSpec::from_static(
            hofund_plugin_api::PluginId::from_static(OPEN_ID),
            "Open",
            hofund_plugin_api::SignInFlowKind::Pkce,
        )
        .with_agent_sign_in(hofund_plugin_api::AgentSignIn::new(
            &["open-agent"],
            "D221_OPEN_HOME",
            "d221-open",
        ));
    static WITHDRAWN_SPEC: hofund_plugin_api::SubscriptionSpec =
        hofund_plugin_api::SubscriptionSpec::from_static(
            hofund_plugin_api::PluginId::from_static(WITHDRAWN_ID),
            "Withdrawn",
            hofund_plugin_api::SignInFlowKind::Pkce,
        )
        .with_agent_sign_in(hofund_plugin_api::AgentSignIn::new(
            &["withdrawn-agent"],
            "D221_WITHDRAWN_HOME",
            "d221-withdrawn",
        ));

    struct DiscoverCalls {
        imports: std::sync::atomic::AtomicUsize,
    }

    struct DiscoverFactory {
        withdrawn: bool,
        calls: std::sync::Arc<DiscoverCalls>,
    }

    struct DiscoverEmpty;
    impl hofund_plugin_api::ValidConfig for DiscoverEmpty {}

    impl hofund_plugin_api::SubscriptionFactory for DiscoverFactory {
        type Config = DiscoverEmpty;

        fn spec(&self) -> &'static hofund_plugin_api::SubscriptionSpec {
            if self.withdrawn {
                &WITHDRAWN_SPEC
            } else {
                &OPEN_SPEC
            }
        }

        fn config_schema(&self) -> hofund_plugin_api::Schema {
            hofund_plugin_api::Schema::empty()
        }

        fn validate(
            &self,
            _raw: &hofund_plugin_api::RawTable,
        ) -> Result<Self::Config, hofund_plugin_api::ConfigError> {
            Ok(DiscoverEmpty)
        }

        fn build(
            &self,
            _cfg: &Self::Config,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<
            std::sync::Arc<dyn hofund_plugin_api::Subscription>,
            hofund_plugin_api::BuildError,
        > {
            Ok(std::sync::Arc::new(DiscoverFake {
                id: hofund_plugin_api::PluginId::from_static(if self.withdrawn {
                    WITHDRAWN_ID
                } else {
                    OPEN_ID
                }),
                flow: hofund_plugin_api::SignInFlow::Pkce,
                withdrawn: self.withdrawn,
                calls: std::sync::Arc::clone(&self.calls),
            }))
        }
    }

    struct DiscoverFake {
        id: hofund_plugin_api::PluginId,
        flow: hofund_plugin_api::SignInFlow,
        withdrawn: bool,
        calls: std::sync::Arc<DiscoverCalls>,
    }

    #[hofund_plugin_sdk::async_trait]
    impl hofund_plugin_api::Subscription for DiscoverFake {
        fn id(&self) -> &hofund_plugin_api::PluginId {
            &self.id
        }

        fn flow(&self) -> &hofund_plugin_api::SignInFlow {
            &self.flow
        }

        fn account_enrollment(&self) -> hofund_plugin_api::AccountEnrollment {
            if self.withdrawn {
                hofund_plugin_api::AccountEnrollment::Withdrawn {
                    risk: "saved-account risk line",
                    refusal: REFUSAL,
                }
            } else {
                hofund_plugin_api::AccountEnrollment::Open
            }
        }

        async fn start_sign_in(
            &self,
            _ctx: &hofund_plugin_api::SignInCtx,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<hofund_plugin_api::SignInChallenge, hofund_plugin_api::PluginError> {
            Err(hofund_plugin_api::PluginError::unsupported("unused"))
        }

        async fn complete_sign_in(
            &self,
            _ctx: &hofund_plugin_api::SignInCtx,
            _proof: hofund_plugin_api::SignInProof,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<hofund_plugin_api::AccountId, hofund_plugin_api::PluginError> {
            Err(hofund_plugin_api::PluginError::unsupported("unused"))
        }

        async fn account_status(
            &self,
            _account: &hofund_plugin_api::AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<hofund_plugin_api::AccountStatus, hofund_plugin_api::PluginError> {
            Ok(hofund_plugin_api::AccountStatus::Ready)
        }

        async fn refresh(
            &self,
            _account: &hofund_plugin_api::AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<(), hofund_plugin_api::PluginError> {
            Ok(())
        }

        async fn quota(
            &self,
            _account: &hofund_plugin_api::AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<Vec<hofund_plugin_api::QuotaWindow>, hofund_plugin_api::PluginError> {
            Ok(Vec::new())
        }

        async fn list_models(
            &self,
            _account: &hofund_plugin_api::AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
            Ok(hofund_plugin_api::ModelList::new(Vec::new(), None))
        }

        fn upstream(
            &self,
            _account: &hofund_plugin_api::AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<
            std::sync::Arc<dyn hofund_plugin_api::UpstreamProvider>,
            hofund_plugin_api::PluginError,
        > {
            Err(hofund_plugin_api::PluginError::unsupported("unused"))
        }

        async fn discover_agent_sign_in(
            &self,
            _home: &Path,
        ) -> Result<Vec<hofund_plugin_api::DiscoveredAgentAccount>, hofund_plugin_api::PluginError>
        {
            let (id, label, reference) = if self.withdrawn {
                ("a-00000000000000d2", "Withdrawn", "withdrawn-reference")
            } else {
                ("a-00000000000000d1", "Open", "open-reference")
            };
            Ok(vec![hofund_plugin_api::DiscoveredAgentAccount::new(
                hofund_plugin_api::AccountId::from_static(id),
                label,
                reference,
            )?])
        }

        async fn import_agent_sign_in(
            &self,
            _home: &Path,
            reference: &str,
            host: &hofund_plugin_api::HostContext,
        ) -> Result<hofund_plugin_api::AccountId, hofund_plugin_api::PluginError> {
            self.calls
                .imports
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let id = hofund_plugin_api::AccountId::from_static("a-00000000000000d1");
            let account = host.accounts().account(id.clone(), "Open")?;
            host.accounts()
                .upsert(
                    account,
                    hofund_plugin_api::SecretString::from(reference.to_owned()),
                )
                .await?;
            Ok(id)
        }
    }

    struct DiscoverRegistry {
        calls: std::sync::Arc<DiscoverCalls>,
    }

    impl crate::DaemonRegistry for DiscoverRegistry {
        fn factory(
            &self,
            _id: &str,
        ) -> Option<std::sync::Arc<dyn hofund_plugin_api::DynUpstreamFactory>> {
            None
        }

        fn is_kernel_factory(&self, _id: &str) -> bool {
            false
        }

        fn builtin_factory_ids(&self) -> &'static [&'static str] {
            &[]
        }

        fn builtin_protocols(&self) -> Vec<hofund_plugin_api::ProtocolId> {
            Vec::new()
        }

        fn find_preset(
            &self,
            _id: &str,
        ) -> Option<(&'static crate::CompiledPreset, Option<&'static str>)> {
            None
        }

        fn agents(&self) -> &'static [&'static hofund_plugin_api::AgentDescription] {
            &[]
        }

        fn surfaces(&self) -> Vec<std::sync::Arc<dyn hofund_plugin_api::InboundSurface>> {
            Vec::new()
        }

        fn subscription(
            &self,
            id: &str,
        ) -> Option<std::sync::Arc<dyn hofund_plugin_api::DynSubscriptionFactory>> {
            let withdrawn = match id {
                OPEN_ID => false,
                WITHDRAWN_ID => true,
                _ => return None,
            };
            Some(std::sync::Arc::new(DiscoverFactory {
                withdrawn,
                calls: std::sync::Arc::clone(&self.calls),
            }))
        }
    }

    fn discover_opened_pair(
        include_open: bool,
    ) -> (
        tempfile::TempDir,
        Vec<crate::sign_in::Opened>,
        DiscoverRegistry,
        std::sync::Arc<DiscoverCalls>,
        crate::agent_login::TestHomeGuard,
    ) {
        let root = tempfile::tempdir().expect("root");
        let open_home = root.path().join("open");
        let withdrawn_home = root.path().join("withdrawn");
        fs::create_dir_all(&open_home).expect("open home");
        fs::create_dir_all(&withdrawn_home).expect("withdrawn home");
        let mut homes = BTreeMap::new();
        homes.insert("D221_OPEN_HOME".to_owned(), open_home);
        homes.insert("D221_WITHDRAWN_HOME".to_owned(), withdrawn_home);
        let guard = push_test_agent_homes(homes);
        let calls = std::sync::Arc::new(DiscoverCalls {
            imports: std::sync::atomic::AtomicUsize::new(0),
        });
        let registry = DiscoverRegistry {
            calls: std::sync::Arc::clone(&calls),
        };
        let mut opened = Vec::new();
        if include_open {
            opened.push(discover_entry(root.path(), OPEN_ID, false, &calls));
        }
        opened.push(discover_entry(root.path(), WITHDRAWN_ID, true, &calls));
        (root, opened, registry, calls, guard)
    }

    fn discover_entry(
        root: &Path,
        id: &str,
        withdrawn: bool,
        calls: &std::sync::Arc<DiscoverCalls>,
    ) -> crate::sign_in::Opened {
        let plugin = hofund_plugin_api::PluginId::from_static(if withdrawn {
            WITHDRAWN_ID
        } else {
            OPEN_ID
        });
        let state = root.join(id);
        fs::create_dir_all(&state).expect("state");
        let host = hofund_plugin_host::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            plugin,
            &state,
        );
        let instance = DiscoverFactory {
            withdrawn,
            calls: std::sync::Arc::clone(calls),
        }
        .build(&DiscoverEmpty, host.context())
        .expect("build");
        crate::sign_in::Opened {
            id: id.to_owned(),
            name: id.to_owned(),
            instance,
            host: host.context().clone(),
            config_dir: state,
        }
    }

    #[test]
    fn d221_open_import_continues_when_a_discovered_row_is_refused() {
        let (_root, opened, registry, calls, _guard) = discover_opened_pair(true);
        let config = minimal_config(true);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let report = runtime
            .block_on(import_opened(&opened, &registry, &config))
            .expect("import_if_configured");
        assert_eq!(report.imported, vec!["a-00000000000000d1".to_owned()]);
        let open = report
            .lines
            .iter()
            .find(|line| line.subscription == OPEN_ID)
            .expect("open row");
        assert!(open.refused.is_none());
        assert!(!open.already_stored);
        let withdrawn = report
            .lines
            .iter()
            .find(|line| line.subscription == WITHDRAWN_ID)
            .expect("withdrawn row");
        assert_eq!(withdrawn.refused.as_deref(), Some(REFUSAL));
        assert_eq!(calls.imports.load(std::sync::atomic::Ordering::SeqCst), 1);
        let printed = format_report(&report, true);
        assert!(printed.contains("refused:"));
        assert!(printed.contains(REFUSAL), "{printed}");

        let (_root, only, registry, calls, _guard) = discover_opened_pair(false);
        let recorded = runtime
            .block_on(discover_opened(&only, &registry, &config, true))
            .expect("refused row is recorded");
        assert!(recorded.imported.is_empty());
        assert_eq!(recorded.lines[0].refused.as_deref(), Some(REFUSAL));
        let refused = reject_refused_only_add(recorded, true).expect_err("explicit add");
        assert!(refused.to_string().contains(REFUSAL), "{refused}");
        runtime
            .block_on(import_opened(&only, &registry, &config))
            .expect("configured import keeps going");
        assert_eq!(calls.imports.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}

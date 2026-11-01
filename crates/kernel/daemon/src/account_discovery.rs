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

fn discovery_enabled(config: &Config, cli_invoked: bool) -> Result<(), DaemonError> {
    discovery_enabled_inner(config, cli_invoked, discovery_blocked())
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
    let config =
        hofund_config::load_path(path).map_err(|error| DaemonError::new(error.to_string()))?;
    discovery_enabled(&config, true)?;
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
            discover_agent_accounts: discover,
            warnings: Vec::new(),
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
}

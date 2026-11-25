//! Antigravity used through the user's own `agy` CLI.
//!
//! Each account gets a home under the plugin state directory. Moving a saved
//! Antigravity account copies its refresh token into that home and checks it
//! with `agy models`. The prompt bridge is hofund's own contract: the live
//! `agy` argv is not in the reference commits this plugin ports, and hofund
//! does not download the CLI.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, AccountStatus, BinaryName, BuildError, ConfigError, HostContext, ModelList,
    ModelSpec, PluginError, PluginId, QuotaWindow, RawTable, RawValue, Schema, SignInChallenge,
    SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory,
    SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

mod cli;
mod gemini_key;
mod move_accounts;
mod notice;
mod upstream;

pub use gemini_key::{
    GeminiKeyOffer, GeminiKeySkip, GeminiKeyStatus, gemini_key_status, read_gemini_key,
};

/// The CLI command this subscription drives.
pub(crate) const COMMAND: BinaryName = BinaryName::new("agy");

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("agy-cli"),
    "agy CLI",
    SignInFlowKind::Bridge,
)
.with_stale_replica_quota(true);

/// Writes an account home secret for matrix tests. The refresh stays in the secret.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn matrix_sign_in(
    home: &std::path::Path,
    email: &str,
) -> Result<(AccountId, SecretObject), PluginError> {
    std::fs::create_dir_all(home)
        .map_err(|error| PluginError::io(format!("cannot create the agy home ({error})")))?;
    let email = email.trim().to_ascii_lowercase();
    Ok((
        account_id_from(&["agy-cli", &email]),
        SecretObject::new()
            .with_text("email", &email)
            .with_text("home", &home.to_string_lossy())
            .with_text("refresh_token", "matrix-refresh"),
    ))
}

/// Registers the agy CLI subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(AgyCliFactory)
}

/// Builds agy CLI subscription instances.
#[derive(Clone, Debug, Default)]
pub struct AgyCliFactory;

/// The validated config. `command` is the absolute path of `agy`.
#[derive(Clone, Debug)]
pub struct AgyCliConfig {
    command: Option<PathBuf>,
}

impl ValidConfig for AgyCliConfig {}

impl SubscriptionFactory for AgyCliFactory {
    type Config = AgyCliConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(vec![("command".to_owned(), Schema::string())], &[])
            .expect("the config schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "command" {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        let command = match raw.get("command") {
            None => None,
            Some(RawValue::String(value)) => {
                let path = PathBuf::from(value.trim());
                if !path.is_absolute() {
                    return Err(
                        ConfigError::new("command must be an absolute path").with_path("command")
                    );
                }
                Some(path)
            }
            Some(_) => {
                return Err(ConfigError::new("command must be a string").with_path("command"));
            }
        };
        Ok(AgyCliConfig { command })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(AgyCli {
            command: cfg.command.clone(),
            flow: SignInFlow::Bridge { command: COMMAND },
        }))
    }
}

struct AgyCli {
    command: Option<PathBuf>,
    flow: SignInFlow,
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for AgyCli {
    fn id(&self) -> &PluginId {
        SPEC.plugin()
    }

    fn flow(&self) -> &SignInFlow {
        &self.flow
    }

    async fn start_sign_in(
        &self,
        _ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        SignInChallenge::instructions(
            "Set command to the absolute path of your agy binary, then import a saved Antigravity account. hofund does not download agy. A new Antigravity sign-in stays closed.",
        )
    }

    async fn complete_sign_in(
        &self,
        _ctx: &SignInCtx,
        _proof: SignInProof,
        _host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        Err(PluginError::unsupported(
            "agy CLI accounts are imported from a saved Antigravity export",
        ))
    }

    async fn account_status(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        match host.accounts().secret(account).await? {
            Some(secret) => {
                let object = SecretObject::parse(&secret)?;
                if object.text("home").is_some() {
                    Ok(AccountStatus::Ready)
                } else {
                    Ok(AccountStatus::NeedsSignIn {
                        reason: "agy account has no home".to_owned(),
                    })
                }
            }
            None => Ok(AccountStatus::NeedsSignIn {
                reason: "agy account is missing".to_owned(),
            }),
        }
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        let _ = self.list_models(account, host).await?;
        Ok(())
    }

    async fn quota(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        // Usage windows from agy are display-only in the reference. An empty
        // list keeps them from narrowing a routing rest.
        Ok(Vec::new())
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let stored = load(host, account).await?;
        let program = cli::program(self.command.as_ref())?;
        let provider = cli::read_provider_proxy(host)?;
        let via = cli::via(&stored.proxy, &provider);
        let models = cli::models(host, &program, &stored.home, via).await?;
        let models = models
            .into_iter()
            .map(|(id, name)| ModelSpec::new(id, Some(name)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ModelList::new(models, None))
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::AgyUpstream::new(
            account.clone(),
            host.clone(),
            self.command.clone(),
        )))
    }

    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        let plan = move_accounts::parse_plan(text)?;
        let existing = host.accounts().list().await?;
        let mut have = HashSet::new();
        for account in &existing {
            if let Some(secret) = host.accounts().secret(account.id()).await? {
                let object = SecretObject::parse(&secret)?;
                if let Some(email) = object.text("email") {
                    have.insert(email.to_ascii_lowercase());
                }
            }
        }
        let rows = move_accounts::select_moves(plan.rows, &have);
        let agy_proxy = if plan.agy_proxy.is_empty() {
            cli::read_provider_proxy(host)?
        } else {
            plan.agy_proxy.clone()
        };
        let provider =
            move_accounts::provider_after_carry(existing.len(), &agy_proxy, &plan.source_proxy);
        if provider != agy_proxy
            || (!provider.is_empty() && cli::read_provider_proxy(host)?.is_empty())
        {
            cli::write_provider_proxy(host, &provider)?;
        }
        let program = cli::program(self.command.as_ref())?;
        let mut stored = Vec::new();
        let mut failed = Vec::new();
        for row in rows {
            let id = account_id_from(&["agy-cli", &row.email]);
            let home = host.dirs().state_dir()?.join("agy-homes").join(id.as_str());
            let account_proxy =
                move_accounts::account_proxy_to_store(&provider, &plan.source_proxy, &row.proxy);
            if let Err(error) = move_accounts::write_sign_in(&home, &row.email, &row.refresh) {
                failed.push(format!("{}: {error}", row.email));
                continue;
            }
            if let Err(error) =
                cli::models(host, &program, &home, cli::via(&account_proxy, &provider)).await
            {
                let _ = std::fs::remove_dir_all(&home);
                failed.push(format!("{}: {error}", row.email));
                continue;
            }
            let mut blob = SecretObject::new()
                .with_text("email", &row.email)
                .with_text("home", &home.to_string_lossy())
                .with_secret("refresh_token", &row.refresh);
            if !account_proxy.is_empty() {
                blob = blob.with_text("proxy", &account_proxy);
            }
            let account = host
                .accounts()
                .account(id.clone(), row.email.clone())
                .map_err(|error| PluginError::config(error.to_string()))?;
            host.accounts().upsert(account, blob.to_secret()).await?;
            stored.push(id);
        }
        if let Some(reason) = failed.first() {
            return Err(PluginError::config(format!("could not move {reason}")));
        }
        Ok(stored)
    }
}

struct Loaded {
    home: PathBuf,
    proxy: String,
}

async fn load(host: &HostContext, account: &AccountId) -> Result<Loaded, PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth("agy account is missing"));
    };
    let object = SecretObject::parse(&secret)?;
    let home = object
        .text("home")
        .ok_or_else(|| PluginError::auth("agy account has no home"))?;
    Ok(Loaded {
        home: PathBuf::from(home),
        proxy: object.text("proxy").unwrap_or("").to_owned(),
    })
}

#[cfg(test)]
mod d222_tests;

#[cfg(test)]
mod d256_tests {
    use super::SPEC;

    #[test]
    fn d256_stale_replica_quota_is_opted_in() {
        assert!(SPEC.stale_replica_quota());
    }
}

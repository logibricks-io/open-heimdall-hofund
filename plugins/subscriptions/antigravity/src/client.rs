//! Installed-app OAuth client for Antigravity, loaded from config or env.
//!
//! The 3005 reference embeds Antigravity's installed-app client. hofund does
//! not ship those literals. The operator must supply the client that Google
//! issued to Antigravity, either on the plugin config or in the environment.
//! 3005 does not read the desktop app's own config for this (Antigravity's
//! CLI keeps tokens in the system keyring, not a client file).

use hofund_plugin_api::BuildError;

/// Plugin config key for the OAuth client id.
pub(crate) const ID_KEY: &str = "client_id";
/// Plugin config key for the OAuth client secret.
pub(crate) const SECRET_KEY: &str = "client_secret";
/// Environment variable for the OAuth client id.
pub(crate) const ID_ENV: &str = "HOFUND_ANTIGRAVITY_CLIENT_ID";
/// Environment variable for the OAuth client secret.
pub(crate) const SECRET_ENV: &str = "HOFUND_ANTIGRAVITY_CLIENT_SECRET";

/// Returned when neither config nor the environment names a complete client.
pub(crate) const NOT_CONFIGURED: &str = "Antigravity OAuth client is not configured. Set HOFUND_ANTIGRAVITY_CLIENT_ID and HOFUND_ANTIGRAVITY_CLIENT_SECRET, or set client_id and client_secret on the antigravity plugin. hofund does not ship Antigravity's installed-app OAuth client (the 3005 reference embeds it). See docs/architecture.md and docs/coverage/subscription-parity.md.";

/// Returned when only one of the id or secret is present.
pub(crate) const INCOMPLETE: &str = "Antigravity client_id and client_secret must both be set (HOFUND_ANTIGRAVITY_CLIENT_ID / HOFUND_ANTIGRAVITY_CLIENT_SECRET, or the same keys on the antigravity plugin).";

/// An installed-app OAuth client used for authorize, exchange, and refresh.
#[derive(Clone)]
pub(crate) struct OauthClient {
    id: String,
    secret: String,
}

impl std::fmt::Debug for OauthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthClient")
            .field("id", &self.id)
            .field("secret", &"<set>")
            .finish()
    }
}

impl OauthClient {
    pub(crate) fn new(id: String, secret: String) -> Self {
        Self { id, secret }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }
}

/// Resolves the client from environment variables (non-empty values win) and
/// then from plugin config. Both the id and the secret must be present.
pub(crate) fn resolve(
    config_id: Option<&str>,
    config_secret: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<OauthClient, BuildError> {
    let id = first_nonempty(env(ID_ENV), config_id);
    let secret = first_nonempty(env(SECRET_ENV), config_secret);
    match (id, secret) {
        (Some(id), Some(secret)) => Ok(OauthClient::new(id, secret)),
        (None, None) => Err(BuildError::new(NOT_CONFIGURED)),
        _ => Err(BuildError::new(INCOMPLETE)),
    }
}

fn first_nonempty(env_value: Option<String>, config_value: Option<&str>) -> Option<String> {
    nonempty(env_value).or_else(|| nonempty(config_value.map(str::to_owned)))
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

pub(crate) fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::{ID_ENV, INCOMPLETE, NOT_CONFIGURED, SECRET_ENV, resolve};
    use std::collections::HashMap;

    fn env<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        move |name| map.get(name).map(|value| (*value).to_owned())
    }

    #[test]
    fn r49c_oauth_client_is_not_configured_without_config_or_env() {
        let error = resolve(None, None, env(&[])).expect_err("missing");
        assert_eq!(error.message(), NOT_CONFIGURED);
    }

    #[test]
    fn r49c_oauth_client_reads_plugin_config() {
        let client = resolve(Some("cfg-id"), Some("cfg-secret"), env(&[])).expect("config");
        assert_eq!(client.id(), "cfg-id");
        assert_eq!(client.secret(), "cfg-secret");
    }

    #[test]
    fn r49c_oauth_client_reads_env_and_overrides_config() {
        let client = resolve(
            Some("cfg-id"),
            Some("cfg-secret"),
            env(&[(ID_ENV, "env-id"), (SECRET_ENV, "env-secret")]),
        )
        .expect("env");
        assert_eq!(client.id(), "env-id");
        assert_eq!(client.secret(), "env-secret");
    }

    #[test]
    fn r49c_oauth_client_env_can_fill_a_missing_config_field() {
        let client =
            resolve(Some("cfg-id"), None, env(&[(SECRET_ENV, "env-secret")])).expect("mixed");
        assert_eq!(client.id(), "cfg-id");
        assert_eq!(client.secret(), "env-secret");
    }

    #[test]
    fn r49c_oauth_client_rejects_a_partial_pair() {
        let error = resolve(Some("cfg-id"), None, env(&[])).expect_err("partial");
        assert_eq!(error.message(), INCOMPLETE);
        let error = resolve(None, Some("cfg-secret"), env(&[])).expect_err("partial");
        assert_eq!(error.message(), INCOMPLETE);
    }

    #[test]
    fn r49c_oauth_client_treats_whitespace_as_absent() {
        let error = resolve(Some("  "), Some("\t"), env(&[])).expect_err("blank");
        assert_eq!(error.message(), NOT_CONFIGURED);
        let client = resolve(
            Some("  cfg-id  "),
            Some("cfg-secret"),
            env(&[(ID_ENV, "  ")]),
        )
        .expect("trimmed config after blank env");
        assert_eq!(client.id(), "cfg-id");
    }
}

//! Where a Copilot account lives: github.com or one `<name>.ghe.com`
//! enterprise, and the URLs derived from it.

use hofund_plugin_api::PluginError;

/// The public GitHub host.
pub(crate) const GITHUB: &str = "github.com";

/// The suffix every GitHub Enterprise Cloud with data residency host has.
const GHE_SUFFIX: &str = ".ghe.com";

/// The Copilot API base for github.com accounts.
const GITHUB_API_BASE: &str = "https://api.githubcopilot.com";

/// The domain every github.com Copilot API endpoint sits under.
const COPILOT_DOMAIN: &str = "githubcopilot.com";

/// A validated GitHub host: github.com or `<name>.ghe.com`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Site {
    host: String,
}

impl Site {
    /// github.com.
    pub(crate) fn github() -> Self {
        Self {
            host: GITHUB.to_owned(),
        }
    }

    /// Reads a host the user typed: empty, `github.com`, `www.github.com`,
    /// or `<name>.ghe.com`, optionally as an `http(s)://` origin.
    pub(crate) fn parse(text: &str) -> Result<Self, PluginError> {
        let lower = text.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return Ok(Self::github());
        }
        let mut host = lower.as_str();
        if let Some((scheme, rest)) = host.split_once("://") {
            if scheme != "https" && scheme != "http" {
                return Err(invalid());
            }
            host = rest.strip_suffix('/').unwrap_or(rest);
            if host.is_empty() || host.contains(['/', '?', '#', '@', ':']) {
                return Err(invalid());
            }
        }
        if host == GITHUB || host == "www.github.com" {
            return Ok(Self::github());
        }
        match host.strip_suffix(GHE_SUFFIX) {
            Some(name) if is_label(name) => Ok(Self {
                host: host.to_owned(),
            }),
            _ => Err(invalid()),
        }
    }

    /// The host name, such as `github.com` or `acme.ghe.com`.
    pub(crate) fn name(&self) -> &str {
        &self.host
    }

    /// Whether this is an enterprise host rather than github.com.
    pub(crate) fn is_enterprise(&self) -> bool {
        self.host != GITHUB
    }

    /// The device authorization endpoint.
    pub(crate) fn device_code_url(&self) -> String {
        format!("https://{}/login/device/code", self.host)
    }

    /// The OAuth token endpoint.
    pub(crate) fn token_url(&self) -> String {
        format!("https://{}/login/oauth/access_token", self.host)
    }

    /// The page where the user types the code.
    pub(crate) fn device_page(&self) -> String {
        format!("https://{}/login/device", self.host)
    }

    /// The REST API origin: `api.<host>`.
    fn api(&self) -> String {
        format!("https://api.{}", self.host)
    }

    /// Where the OAuth token is traded for a Copilot session token.
    pub(crate) fn session_url(&self) -> String {
        format!("{}/copilot_internal/v2/token", self.api())
    }

    /// The Copilot seat and quota of the signed-in user.
    pub(crate) fn copilot_user_url(&self) -> String {
        format!("{}/copilot_internal/user", self.api())
    }

    /// The signed-in GitHub user.
    pub(crate) fn user_url(&self) -> String {
        format!("{}/user", self.api())
    }

    /// The Copilot API base used when the session names none.
    pub(crate) fn api_base(&self) -> String {
        if self.is_enterprise() {
            format!("https://copilot-api.{}", self.host)
        } else {
            GITHUB_API_BASE.to_owned()
        }
    }

    /// `url` as an API base when it is an `https` origin this site may send
    /// the session token to: under `githubcopilot.com`, or
    /// `copilot-api.<host>` for an enterprise. No port, user, path, query,
    /// or fragment.
    pub(crate) fn trusted_api(&self, url: &str) -> Option<String> {
        let rest = url.strip_prefix("https://")?;
        let host = rest.strip_suffix('/').unwrap_or(rest).to_ascii_lowercase();
        if host.is_empty() || host.contains(['/', '?', '#', '@', ':', '\\']) {
            return None;
        }
        let trusted = host == COPILOT_DOMAIN
            || host
                .strip_suffix(COPILOT_DOMAIN)
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
            || (self.is_enterprise() && host == format!("copilot-api.{}", self.host));
        trusted.then(|| format!("https://{host}"))
    }
}

/// A DNS label: 1 to 63 of `a-z`, `0-9`, `-`, not starting or ending in `-`.
fn is_label(name: &str) -> bool {
    let bytes = name.as_bytes();
    let edge = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(edge)
        && bytes.last().is_some_and(edge)
        && bytes.iter().all(|byte| edge(byte) || *byte == b'-')
}

fn invalid() -> PluginError {
    PluginError::config("Copilot signs in on github.com or an enterprise's <name>.ghe.com host")
}

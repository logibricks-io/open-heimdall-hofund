//! Converse settings: the runtime root, the signing region, and the key pair.
//!
//! These checks run at build, after the host resolved `env:` references, so
//! a bad value fails `hofund check` instead of the first call.

use hofund_plugin_api::{BuildError, SecretString};
use hofund_plugin_sdk::sigv4::AwsSigner;

use crate::BedrockConfig;

/// The trimmed AWS key pair and optional session token.
pub(crate) struct Keys {
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
}

impl Keys {
    /// Reads and checks the key options. Each value must be non-empty header
    /// text; an empty session token means a long-lived key.
    pub(crate) fn read(cfg: &BedrockConfig) -> Result<Self, BuildError> {
        let keys = Self {
            access_key_id: cfg.access_key_id.trim().to_owned(),
            secret_access_key: cfg.secret_access_key.trim().to_owned(),
            session_token: cfg.session_token.trim().to_owned(),
        };
        header_safe(&keys.access_key_id, "access key id")?;
        header_safe(&keys.secret_access_key, "secret access key")?;
        if !keys.session_token.is_empty() {
            header_safe(&keys.session_token, "session token")?;
        }
        Ok(keys)
    }

    /// The signer for `region`.
    pub(crate) fn signer(self, region: String) -> AwsSigner {
        AwsSigner::new(
            region,
            self.access_key_id,
            SecretString::from(self.secret_access_key),
        )
        .session_token(SecretString::from(self.session_token))
    }
}

/// The runtime root under a configured base: the region's chat URL ends in
/// `/openai/v1` and its Messages URL in `/anthropic`; Converse sits above both.
pub(crate) fn runtime_root(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed
        .strip_suffix("/openai/v1")
        .or_else(|| trimmed.strip_suffix("/anthropic"))
        .unwrap_or(trimmed);
    trimmed.trim_end_matches('/').to_owned()
}

/// The signing region: the `region` option, else the region in a
/// `bedrock-runtime.<region>.amazonaws.com` host.
pub(crate) fn signing_region(explicit: &str, base_url: &str) -> Result<String, BuildError> {
    let explicit = explicit.trim().to_ascii_lowercase();
    if !explicit.is_empty() {
        check_region(&explicit)?;
        return Ok(explicit);
    }
    let host = hofund_plugin_sdk::url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    let Some(region) = host
        .strip_prefix("bedrock-runtime.")
        .and_then(|rest| rest.strip_suffix(".amazonaws.com"))
        .filter(|region| !region.is_empty())
    else {
        return Err(BuildError::new(
            "Converse needs a region, or a bedrock-runtime.<region>.amazonaws.com host",
        ));
    };
    check_region(region)?;
    Ok(region.to_owned())
}

/// A region id is at most 32 lowercase letters, digits, and hyphens. It goes
/// into the credential scope, so nothing else may reach the signer.
fn check_region(region: &str) -> Result<(), BuildError> {
    let ok = !region.is_empty()
        && region.len() <= 32
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if ok {
        Ok(())
    } else {
        Err(BuildError::new(format!(
            "Converse region `{region}` is not a region id"
        )))
    }
}

fn header_safe(value: &str, what: &str) -> Result<(), BuildError> {
    if value.is_empty() {
        return Err(BuildError::new(format!("Converse {what} is empty")));
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(BuildError::new(format!(
            "Converse {what} is not valid header text"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;

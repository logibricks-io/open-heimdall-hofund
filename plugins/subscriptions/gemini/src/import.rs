//! Google refresh-token exports and Gemini CLI's read-only sign-in files.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::{
    SecretObject, account_id_from, parse_google_import, parse_token_set, same_secret, token_error,
};

use crate::{CLIENT_ID, Endpoints, project, sign_in, tokens};

pub(crate) async fn store(
    host: &HostContext,
    endpoints: &Endpoints,
    text: &str,
    configured_project: Option<&str>,
) -> Result<Vec<AccountId>, PluginError> {
    let entries = parse_google_import("gemini", "Gemini CLI", text)?;
    for (index, entry) in entries.iter().enumerate() {
        if let Some(error) = entry.error() {
            return Err(PluginError::config(format!(
                "account {}: {error}",
                index + 1
            )));
        }
    }
    let mut imported = Vec::new();
    for entry in entries {
        let refresh = entry
            .refresh_token()
            .cloned()
            .ok_or_else(|| PluginError::config("no refresh_token in it"))?;
        if let Some(existing) = existing_account(host, &refresh).await? {
            imported.push(existing);
            continue;
        }
        let reply = Call::post(&endpoints.token)
            .header("content-type", "application/x-www-form-urlencoded")
            .bytes(tokens::form(&refresh))
            .fetch(host.http())
            .await?;
        if !(200..300).contains(&reply.status()) {
            return Err(token_error(reply.status(), reply.body()));
        }
        let set = parse_token_set(reply.body(), host.clock().now())
            .map_err(|error| PluginError::decode(error.message()))?;
        let email = match sign_in::who(host, &endpoints.userinfo, set.access()).await {
            Ok(email) => email,
            Err(_) if !entry.email().is_empty() => entry.email().to_owned(),
            Err(error) => return Err(error),
        };
        let id = account_id_from(&["google", &email.to_ascii_lowercase()]);
        let mut blob = SecretObject::new()
            .with_number("v", 1)
            .with_secret("refresh_token", &refresh)
            .with_text("email", &email);
        let imported_project = (!entry.project().is_empty()).then_some(entry.project());
        let wanted = configured_project.or(imported_project);
        if let Some(project_id) = imported_project {
            blob = blob.with_text(project::PROJECT, project_id);
        }
        let blob = project::settle(host, endpoints, set.access(), wanted, blob).await;
        let account = host.accounts().account(id.clone(), email)?;
        host.accounts().upsert(account, blob.to_secret()).await?;
        imported.push(id);
    }
    Ok(imported)
}

async fn existing_account(
    host: &HostContext,
    refresh: &SecretString,
) -> Result<Option<AccountId>, PluginError> {
    for account in host.accounts().list().await? {
        let Some(secret) = host.accounts().secret(account.id()).await? else {
            continue;
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            continue;
        };
        if blob
            .secret("refresh_token")
            .is_some_and(|current| same_secret(&current, refresh))
        {
            return Ok(Some(account.id().clone()));
        }
    }
    Ok(None)
}

pub(crate) fn discovery_client_matches(id_token: &SecretString) -> bool {
    let Ok(claims) = hofund_plugin_sdk::oauth::unverified_claims(id_token) else {
        return true;
    };
    let client = claims
        .get("azp")
        .and_then(hofund_plugin_sdk::json::Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            claims
                .get("aud")
                .and_then(hofund_plugin_sdk::json::Value::as_str)
                .filter(|value| !value.is_empty())
        });
    client.is_none_or(|client| client == CLIENT_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r49d_discovery_accepts_only_the_gemini_client() {
        fn jwt(claims: &str) -> SecretString {
            let bytes = claims.as_bytes();
            let mut out = String::from("e30.");
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            for chunk in bytes.chunks(3) {
                let n = chunk.iter().enumerate().fold(0_u32, |acc, (index, byte)| {
                    acc | (u32::from(*byte) << (16 - 8 * index))
                });
                for index in 0..chunk.len() + 1 {
                    out.push(char::from(
                        ALPHABET[((n >> (18 - 6 * index)) & 63) as usize],
                    ));
                }
            }
            SecretString::from(out + ".sig")
        }
        let own = jwt(&format!(r#"{{"azp":"{CLIENT_ID}"}}"#));
        assert!(discovery_client_matches(&own));
        let other = jwt(r#"{"aud":"another-client"}"#);
        assert!(!discovery_client_matches(&other));
        assert!(discovery_client_matches(&SecretString::from(
            "not-a-jwt".to_owned()
        )));
    }
}

//! Quota cards kept on remote gateways, merged into local quota views.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_config::{Config, Provider};
use hofund_control_proto::{ListEnvelope, Quota, QuotaKind};
use serde::Deserialize;
use url::form_urlencoded;

use crate::quota_scope::KEY_HELD_CODE;

/// How long a remote gateway's cards are kept before they are asked for again.
pub(crate) const REMOTE_CARDS_FOR: Duration = Duration::from_secs(30);

pub(crate) const ERR_REMOTE_NOTHING_READ: &str =
    "nothing read on that gateway yet; refresh to have it read";
const ERR_REMOTE_NO_SHARE: &str =
    "remote gateway doesn't share its quotas; update hofund on that computer";
const ERR_REMOTE_NOT_SHARED: &str =
    "remote gateway isn't shared; turn on Share on local network on that computer";
const ERR_REMOTE_HELD: &str = "remote gateway tells this key none of its quotas, as it holds the key to a budget, models or accounts; on that computer, turn on Quotas for the key under Gateway keys, and let it use every model of a provider whose usage it is to see";
const ERR_REMOTE_KEY: &str =
    "remote gateway didn't take this key; use one of its enabled gateway keys";
const ERR_REMOTE_UNREACHABLE: &str = "couldn't reach the remote gateway";

const CARDS_PATH: &str = "/v1/hofund/quotas/cards";
const REFRESH_PATH: &str = "/v1/hofund/quotas/refresh";

#[derive(Default)]
struct Cache {
    entries: HashMap<String, RemoteRead>,
}

#[derive(Clone)]
struct RemoteRead {
    at: SystemTime,
    cards: Vec<Quota>,
    err: String,
    held: bool,
}

/// In-memory cache of remote gateway quota cards.
pub(crate) struct RemoteQuotaCache {
    inner: Mutex<Cache>,
}

impl Default for RemoteQuotaCache {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Cache::default()),
        }
    }
}

impl RemoteQuotaCache {
    /// Drops cached remote reads (tests).
    #[cfg(test)]
    #[expect(dead_code)]
    pub(crate) fn forget_all(&self) {
        *self.inner.lock().expect("remote quota cache") = Cache::default();
    }

    /// Merges every configured remote-gateway provider's cards into `data`.
    pub(crate) async fn merge(
        &self,
        config: &Config,
        client: &reqwest::Client,
        data: &mut Vec<Quota>,
    ) {
        let remotes: Vec<&Provider> = config
            .providers
            .iter()
            .filter(|provider| {
                provider.enabled
                    && provider.preset.as_deref() == Some("remote-gateway")
                    && !provider.base_url.is_empty()
                    && !provider.api_key.is_empty()
            })
            .collect();
        if remotes.is_empty() {
            return;
        }
        let mut reads = Vec::with_capacity(remotes.len());
        for provider in &remotes {
            reads.push(self.cards_for(client, provider).await);
        }
        for (provider, read) in remotes.into_iter().zip(reads) {
            let name = remote_display_name(provider);
            if read.cards.is_empty() || read.held {
                let msg = if !read.err.is_empty() {
                    read.err.clone()
                } else if read.held {
                    ERR_REMOTE_HELD.to_owned()
                } else {
                    ERR_REMOTE_NOTHING_READ.to_owned()
                };
                data.push(placeholder_card(&provider.id, &name, &msg));
                continue;
            }
            for mut card in read.cards {
                if card.provider.contains('/') {
                    continue;
                }
                prefix_card(&mut card, provider, &name);
                data.push(card);
            }
        }
    }

    /// Asks a remote gateway to refresh a card, then re-reads its kept cards.
    pub(crate) async fn refresh(
        &self,
        client: &reqwest::Client,
        config: &Config,
        provider_id: &str,
        card_provider: &str,
        user: &str,
    ) -> Result<(), String> {
        let Some(remote) = find_remote(config, provider_id) else {
            return Err(format!("{provider_id} is no remote gateway"));
        };
        let path = if card_provider.is_empty() {
            REFRESH_PATH.to_owned()
        } else {
            let mut query = form_urlencoded::Serializer::new(String::new());
            query.append_pair("provider", card_provider);
            if !user.is_empty() {
                query.append_pair("user", user);
            }
            format!("{REFRESH_PATH}?{}", query.finish())
        };
        let _ = remote_json(client, remote, reqwest::Method::POST, &path).await?;
        let _ = self.cards_for(client, remote).await;
        Ok(())
    }

    async fn cards_for(&self, client: &reqwest::Client, provider: &Provider) -> RemoteRead {
        let id = cache_id(provider);
        let asked = SystemTime::now();
        {
            let cache = self.inner.lock().expect("remote quota cache");
            if let Some(entry) = cache.entries.get(&id)
                && asked.duration_since(entry.at).unwrap_or(Duration::ZERO) < REMOTE_CARDS_FOR
            {
                return entry.clone();
            }
        }
        let result = match remote_cards(client, provider).await {
            Ok(cards) => RemoteRead {
                at: asked,
                cards,
                err: String::new(),
                held: false,
            },
            Err(error) => {
                let held = error_is_held(&error);
                RemoteRead {
                    at: asked,
                    cards: Vec::new(),
                    err: error,
                    held,
                }
            }
        };
        let mut cache = self.inner.lock().expect("remote quota cache");
        let previous = cache.entries.get(&id);
        if previous.is_some_and(|entry| entry.at > asked) {
            return previous.cloned().unwrap_or(result);
        }
        cache.entries.insert(id, result.clone());
        result
    }
}

fn cache_id(provider: &Provider) -> String {
    format!("{}\0{}", provider.id, provider.api_key)
}

fn find_remote<'a>(config: &'a Config, id: &str) -> Option<&'a Provider> {
    config.providers.iter().find(|provider| {
        provider.enabled
            && provider.id.eq_ignore_ascii_case(id)
            && provider.preset.as_deref() == Some("remote-gateway")
    })
}

fn remote_display_name(provider: &Provider) -> String {
    if provider.name.is_empty() {
        provider.id.clone()
    } else {
        provider.name.clone()
    }
}

fn placeholder_card(remote_id: &str, name: &str, message: &str) -> Quota {
    Quota {
        provider: remote_id.to_owned(),
        name: name.to_owned(),
        kind: QuotaKind::Subscription,
        error: message.to_owned(),
        ..Quota::default()
    }
}

fn prefix_card(card: &mut Quota, remote: &Provider, name: &str) {
    if card.provider.contains('/') {
        return;
    }
    card.provider = format!("{}/{}", remote.id, card.provider);
    if !card.name.is_empty() {
        card.name = format!("{} · {}", card.name, name);
    } else {
        card.name = name.to_owned();
    }
}

fn error_is_held(message: &str) -> bool {
    message.contains(KEY_HELD_CODE) || message == ERR_REMOTE_HELD
}

/// Whether `provider` names a card on a remote gateway configured here.
#[must_use]
pub fn split_remote_card(config: &Config, provider: &str) -> Option<(String, String)> {
    if let Some(remote) = find_remote(config, provider) {
        return Some((remote.id.clone(), String::new()));
    }
    let (remote_id, card) = provider.split_once('/')?;
    let remote = find_remote(config, remote_id)?;
    Some((remote.id.clone(), card.to_owned()))
}

async fn remote_cards(client: &reqwest::Client, provider: &Provider) -> Result<Vec<Quota>, String> {
    remote_json(client, provider, reqwest::Method::GET, CARDS_PATH).await
}

async fn remote_json(
    client: &reqwest::Client,
    provider: &Provider,
    method: reqwest::Method,
    path: &str,
) -> Result<Vec<Quota>, String> {
    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{base}{path}");
    let response = client
        .request(method, url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .header("x-api-key", &provider.api_key)
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .map_err(|_| {
            format!(
                "{ERR_REMOTE_UNREACHABLE}: {}",
                remote_display_name(provider)
            )
        })?;
    let status = response.status();
    let body = response.bytes().await.map_err(|_| {
        format!(
            "{ERR_REMOTE_UNREACHABLE}: {}",
            remote_display_name(provider)
        )
    })?;
    if status.as_u16() == 404 || status.as_u16() == 405 {
        return Err(ERR_REMOTE_NO_SHARE.to_owned());
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        if let Ok(parsed) = serde_json::from_slice::<ErrorBody>(&body)
            && parsed.error.code == KEY_HELD_CODE
        {
            return Err(ERR_REMOTE_HELD.to_owned());
        }
        return Err(ERR_REMOTE_NOT_SHARED.to_owned());
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ERR_REMOTE_KEY.to_owned());
    }
    if !status.is_success() {
        if let Ok(parsed) = serde_json::from_slice::<ErrorBody>(&body)
            && !parsed.error.message.is_empty()
        {
            return Err(format!(
                "{}: {}",
                remote_display_name(provider),
                parsed.error.message
            ));
        }
        return Err(format!(
            "{} answered {}",
            remote_display_name(provider),
            status
        ));
    }
    let list: ListEnvelope<Quota> =
        serde_json::from_slice(&body).map_err(|_| ERR_REMOTE_UNREACHABLE.to_owned())?;
    Ok(list.data)
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorFields,
}

#[derive(Deserialize)]
struct ErrorFields {
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: String,
}

//! Copilot Auto model sessions and model-refusal memory.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{
    AccountId, ContentPart, HostContext, PluginError, ProtocolId, Request, Role, SecretString,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{same_secret, secret_text};

use crate::models::{Book, Listed};
use crate::session::Session;

pub(crate) const AUTO_ID: &str = "auto";
const AUTO_VERSION: &str = "2025-10-01";
const AUTO_V2_VERSION: &str = "2026-08-01";
const REFUSED_FOR: Duration = Duration::from_secs(6 * 60 * 60);
const REUSE: Duration = Duration::from_secs(60 * 60);
const FALLBACK_REUSE: Duration = Duration::from_secs(10 * 60);
const MARGIN: Duration = Duration::from_secs(120);

/// One resolved Auto pick. A fallback has no session token.
#[derive(Clone, Debug)]
pub(crate) struct AutoSession {
    pub(crate) model: String,
    pub(crate) token: Option<SecretString>,
    pub(crate) apis: Vec<ProtocolId>,
    expires_at: Option<SystemTime>,
}

impl AutoSession {
    pub(crate) fn protocol(&self, inbound: ProtocolId) -> ProtocolId {
        if self.apis.contains(&inbound) {
            inbound
        } else {
            self.apis
                .first()
                .copied()
                .unwrap_or(ProtocolId::OPENAI_CHAT)
        }
    }
}

enum V2 {
    Picked(AutoSession),
    Negotiate,
    Refused,
}

/// Resolves `auto` through `/auto`, then `/models/session`, then the first
/// manually served model when Auto is not offered.
pub(crate) async fn resolve(
    host: &HostContext,
    session: &Session,
    account: &AccountId,
    book: &Book,
    listed: &Listed,
    request: &Request,
) -> Result<AutoSession, PluginError> {
    let now = host.clock().now();
    if let Some(cached) = book
        .autos
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(account)
        .cloned()
        .filter(|cached| usable(cached, now))
        .filter(|cached| !refused(book, account, &cached.model, now))
    {
        return Ok(cached);
    }

    match auto_v2(host, session, book, account, listed, request, now).await? {
        V2::Picked(picked) => {
            book.autos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(account.clone(), picked.clone());
            return Ok(picked);
        }
        V2::Refused => return fallback(host, book, account, listed),
        V2::Negotiate => {}
    }

    match legacy(host, session, book, account, listed, now).await? {
        Some(picked) => {
            book.autos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(account.clone(), picked.clone());
            Ok(picked)
        }
        None => fallback(host, book, account, listed),
    }
}

fn usable(session: &AutoSession, now: SystemTime) -> bool {
    session
        .expires_at
        .is_none_or(|expires| now + MARGIN < expires)
}

/// Asks Copilot Chat's current Auto endpoint.
async fn auto_v2(
    host: &HostContext,
    session: &Session,
    book: &Book,
    account: &AccountId,
    _listed: &Listed,
    request: &Request,
    now: SystemTime,
) -> Result<V2, PluginError> {
    let body = json::to_vec(&json::json!({"prompt":prompt(request),"tier":"balance"}))
        .map_err(|_| PluginError::decode("Copilot Auto could not encode its prompt"))?;
    let reply = versioned_call(
        Call::post(format!("{}/auto", session.base)),
        session,
        AUTO_V2_VERSION,
    )
    .json(body)
    .fetch(host.http())
    .await?;
    if !(200..300).contains(&reply.status()) {
        return Ok(V2::Negotiate);
    }
    let Ok(Value::Object(object)) = json::from_slice::<Value>(reply.body()) else {
        return Ok(V2::Negotiate);
    };
    let selected = object.get("selected_model").and_then(Value::as_object);
    let model = selected
        .and_then(|model| model.get("id"))
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty());
    let token = object
        .get("session_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty());
    let (Some(model), Some(token)) = (model, token) else {
        return Ok(V2::Negotiate);
    };
    if refused(book, account, model, now) {
        return Ok(V2::Refused);
    }
    let endpoints = selected
        .and_then(|model| model.get("supported_endpoints"))
        .map(|endpoints| crate::models::strings(endpoints.into()))
        .unwrap_or_default();
    let apis = crate::models::apis(&endpoints);
    book.see(account, model, apis.clone());
    let expires_at = object.get("expires_at").and_then(Value::as_u64);
    let expiry = expires_at
        .filter(|secs| *secs > 0)
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
        .filter(|expires| *expires < now + REUSE)
        .unwrap_or(now + REUSE);
    Ok(V2::Picked(AutoSession {
        model: model.to_owned(),
        token: Some(SecretString::from(token.to_owned())),
        apis,
        expires_at: Some(expiry),
    }))
}

/// Asks the older session endpoint. `None` is its 404 fallback signal.
async fn legacy(
    host: &HostContext,
    session: &Session,
    book: &Book,
    account: &AccountId,
    listed: &Listed,
    now: SystemTime,
) -> Result<Option<AutoSession>, PluginError> {
    let reply = versioned_call(
        Call::post(format!("{}/models/session", session.base)),
        session,
        AUTO_VERSION,
    )
    .json(br#"{"auto_mode":{"model_hints":["auto"]}}"#)
    .fetch(host.http())
    .await?;
    if reply.status() == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Copilot did not offer an Auto session",
        ));
    }
    let Ok(Value::Object(object)) = json::from_slice::<Value>(reply.body()) else {
        return Err(PluginError::decode(
            "the Copilot Auto session reply is not a JSON object",
        ));
    };
    let token = object
        .get("session_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::decode("the Copilot Auto session has no token"))?;
    let selected = object.get("selected_model");
    let mut model = selected
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            selected
                .and_then(Value::as_object)
                .and_then(|selected| selected.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|model| !refused(book, account, model, now));
    let available: Vec<String> = object
        .get("available_models")
        .map(|models| crate::models::strings(models.into()))
        .unwrap_or_default()
        .into_iter()
        .filter(|model| !refused(book, account, model, now))
        .collect();
    if model.is_none() {
        model = available
            .iter()
            .find(|model| listed.knows(model))
            .cloned()
            .or_else(|| available.first().cloned());
    }
    let Some(model) = model else {
        if object
            .get("available_models")
            .and_then(Value::as_array)
            .is_some_and(|models| !models.is_empty())
        {
            return Ok(None);
        }
        return Err(PluginError::auth("Copilot Auto picked no model"));
    };
    let apis = listed.apis(&model);
    Ok(Some(AutoSession {
        model,
        token: Some(SecretString::from(token.to_owned())),
        apis,
        expires_at: object
            .get("expires_at")
            .and_then(Value::as_u64)
            .filter(|secs| *secs > 0)
            .map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
    }))
}

fn fallback(
    host: &HostContext,
    book: &Book,
    account: &AccountId,
    listed: &Listed,
) -> Result<AutoSession, PluginError> {
    let now = host.clock().now();
    let model = listed
        .picks()
        .into_iter()
        .find(|model| !refused(book, account, model, now))
        .ok_or_else(|| {
            PluginError::auth(
                "Copilot does not offer Auto to this account, and lists no served manual model",
            )
        })?;
    let apis = listed.apis(&model);
    let picked = AutoSession {
        model,
        token: None,
        apis,
        expires_at: Some(now + FALLBACK_REUSE),
    };
    book.autos
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(account.clone(), picked.clone());
    Ok(picked)
}

fn versioned_call(call: Call, session: &Session, version: &str) -> Call {
    session
        .headers()
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("X-GitHub-Api-Version"))
        .fold(
            call.sensitive_header(
                "authorization",
                format!("Bearer {}", secret_text(&session.token)),
            )
            .header("accept", "application/json"),
            |call, (name, value)| call.header(*name, *value),
        )
        .header("X-GitHub-Api-Version", version)
}

fn prompt(request: &Request) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|prompt| !prompt.trim().is_empty())
        .unwrap_or_else(|| "hi".to_owned())
}

/// Whether Copilot's response is a model refusal rather than a wrong endpoint.
pub(crate) fn model_refusal(error: &PluginError) -> bool {
    if error.status() != Some(400) {
        return false;
    }
    let message = error.message().to_ascii_lowercase();
    message.contains("\"model_not_supported\"")
        || message.contains("requested model is not supported")
}

pub(crate) fn session_gone(error: &PluginError) -> bool {
    error.status() == Some(400)
        && error
            .message()
            .to_ascii_lowercase()
            .contains("not available for session")
}

/// Remembers a model refusal and drops the Auto session that picked it.
pub(crate) fn refuse(book: &Book, account: &AccountId, model: &str, now: SystemTime) {
    book.refused
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(account.clone())
        .or_default()
        .insert(model.to_owned(), now);
    let mut sessions = book
        .autos
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if sessions
        .get(account)
        .is_some_and(|session| session.model == model)
    {
        sessions.remove(account);
    }
}

/// Drops an Auto session whose token Copilot no longer takes for its pick.
pub(crate) fn forget_session(book: &Book, account: &AccountId, token: &SecretString) {
    let mut sessions = book
        .autos
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if sessions.get(account).is_some_and(|session| {
        session
            .token
            .as_ref()
            .is_some_and(|current| same_secret(current, token))
    }) {
        sessions.remove(account);
    }
}

pub(crate) fn refused(book: &Book, account: &AccountId, model: &str, now: SystemTime) -> bool {
    book.refused
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(account)
        .and_then(|models| models.get(model))
        .is_some_and(|at| now.duration_since(*at).unwrap_or(Duration::ZERO) < REFUSED_FOR)
}

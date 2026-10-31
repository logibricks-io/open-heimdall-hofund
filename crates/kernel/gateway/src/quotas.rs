//! Quota routes on the model gateway: `GET /v1/hofund/quotas` and
//! `GET /v1/hofund/quotas/history`.
//!
//! Both answer `{"object":"list","data":[...]}` to any caller holding the
//! gateway token. What a caller is told goes through a [`QuotaScopeHook`]:
//! the default tells everything; a key store may narrow it to the providers
//! and accounts a key may use, or refuse it outright.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use hofund_control_proto::{
    ListEnvelope, Quota, QuotaHistory, QuotaHistoryQuery, QuotaQuery, QuotaResponse,
};

use super::{Inner, bearer, current_token, header_secret, json_response, query_key, reject};

/// Where the gateway reads quota reports and history from.
pub trait QuotaSource: Send + Sync {
    /// The quota report. A kept report never asks a vendor.
    fn report(&self, query: QuotaQuery) -> BoxFuture<'_, QuotaResponse>;

    /// Sampled history, filtered by the query.
    fn history(&self, query: QuotaHistoryQuery) -> Vec<QuotaHistory>;
}

/// Who is asking, as far as the gateway can tell.
#[derive(Clone, Copy)]
pub struct QuotaCaller<'a> {
    key: &'a str,
}

impl<'a> QuotaCaller<'a> {
    /// A caller that presented `key`.
    #[must_use]
    pub fn new(key: &'a str) -> Self {
        Self { key }
    }

    /// The secret the caller presented.
    #[must_use]
    pub fn key(&self) -> &'a str {
        self.key
    }
}

impl std::fmt::Debug for QuotaCaller<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("QuotaCaller").finish_non_exhaustive()
    }
}

/// Which cards a narrowed caller may be told.
pub trait QuotaScope: Send + Sync {
    /// Whether the caller may be told the card of `provider`'s account `user`.
    fn card(&self, provider: &str, user: &str) -> bool;
}

/// What a caller may be told of this gateway's quotas.
#[derive(Clone)]
pub enum QuotaAccess {
    /// Everything: an unrestricted key or the machine's owner.
    All,
    /// Only the cards the scope allows.
    Scoped(Arc<dyn QuotaScope>),
    /// Nothing; the request is answered 403.
    Refused,
}

impl std::fmt::Debug for QuotaAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::All => "All",
            Self::Scoped(_) => "Scoped",
            Self::Refused => "Refused",
        })
    }
}

impl QuotaAccess {
    /// `quotas` as this caller may be told them. Cards relayed from another
    /// machine are never passed on to a narrowed caller.
    #[must_use]
    pub fn quotas(&self, quotas: Vec<Quota>) -> Vec<Quota> {
        match self {
            Self::All => quotas,
            Self::Scoped(scope) => quotas
                .into_iter()
                .filter(|quota| scope.card(&quota.provider, &quota.user))
                .collect(),
            Self::Refused => Vec::new(),
        }
    }

    /// `histories` as this caller may be told them.
    #[must_use]
    pub fn histories(&self, histories: Vec<QuotaHistory>) -> Vec<QuotaHistory> {
        match self {
            Self::All => histories,
            Self::Scoped(scope) => histories
                .into_iter()
                .filter(|history| scope.card(&history.provider, &history.user))
                .collect(),
            Self::Refused => Vec::new(),
        }
    }
}

/// Decides what each caller may be told.
pub type QuotaScopeHook = Arc<dyn Fn(QuotaCaller<'_>) -> QuotaAccess + Send + Sync>;

/// The hook used when none is set: every token holder is told everything.
#[must_use]
pub fn tell_everything() -> QuotaScopeHook {
    Arc::new(|_| QuotaAccess::All)
}

pub(crate) fn routes(router: axum::Router<Arc<Inner>>) -> axum::Router<Arc<Inner>> {
    router
        .route("/v1/hofund/quotas", axum::routing::get(quotas))
        .route("/v1/hofund/quotas/history", axum::routing::get(history))
}

fn presented(headers: &HeaderMap, uri: &Uri) -> String {
    bearer(headers)
        .or_else(|| header_secret(headers, "x-api-key"))
        .or_else(|| header_secret(headers, "x-goog-api-key"))
        .map(str::to_owned)
        .or_else(|| uri.query().and_then(query_key))
        .unwrap_or_default()
}

/// Authenticates the request and resolves its access, or answers it.
fn admit<'a>(
    app: &'a Inner,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<(&'a dyn QuotaSource, QuotaAccess), Response> {
    if let Some(rejected) = reject(&current_token(app), headers, uri, None) {
        return Err(rejected);
    }
    let Some(source) = app.quota_source.as_deref() else {
        return Err(list_failure(StatusCode::NOT_FOUND, "not_found", "not found"));
    };
    let key = presented(headers, uri);
    match (app.quota_scope)(QuotaCaller::new(&key)) {
        QuotaAccess::Refused => Err(list_failure(
            StatusCode::FORBIDDEN,
            "permission_error",
            "this key may not read quotas",
        )),
        access => Ok((source, access)),
    }
}

async fn quotas(State(app): State<Arc<Inner>>, headers: HeaderMap, uri: Uri) -> Response {
    let (source, access) = match admit(&app, &headers, &uri) {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };
    let report = source.report(QuotaQuery::parse(uri.query())).await;
    list(serde_json::to_vec(&ListEnvelope::new(access.quotas(report.data))))
}

async fn history(State(app): State<Arc<Inner>>, headers: HeaderMap, uri: Uri) -> Response {
    let (source, access) = match admit(&app, &headers, &uri) {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };
    let lines = source.history(QuotaHistoryQuery::parse(uri.query()));
    list(serde_json::to_vec(&ListEnvelope::new(access.histories(lines))))
}

fn list(encoded: Result<Vec<u8>, serde_json::Error>) -> Response {
    match encoded {
        Ok(bytes) => json_response(StatusCode::OK, bytes),
        Err(_) => list_failure(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "encode"),
    }
}

fn list_failure(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "error": {"message": message, "type": kind, "param": null, "code": null}
    });
    (status, axum::Json(body)).into_response()
}

//! Quota routes on the model gateway: `GET /v1/hofund/quotas`, history, kept
//! cards, and peer refresh.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use hofund_control_proto::{
    ListEnvelope, Quota, QuotaHistory, QuotaHistoryQuery, QuotaQuery, QuotaResponse,
};
use hofund_secrets::Identity;

use super::{Inner, authenticate_request, bearer, header_secret, json_response, query_key};

/// Error code returned when a held key may read no account cards.
pub const KEY_HELD_CODE: &str = "gateway_key_held";

/// Where the gateway reads quota reports and history from.
pub trait QuotaSource: Send + Sync {
    /// The quota report. A kept report never asks a vendor.
    fn report(&self, query: QuotaQuery) -> BoxFuture<'_, QuotaResponse>;

    /// Sampled history, filtered by the query.
    fn history(&self, query: QuotaHistoryQuery) -> Vec<QuotaHistory>;

    /// Cards as last read on this gateway, without asking vendors.
    fn kept_cards(&self) -> BoxFuture<'_, Vec<Quota>> {
        Box::pin(async { Vec::new() })
    }

    /// Re-reads cards for a peer, then returns kept cards like [`Self::kept_cards`].
    fn refresh_kept_cards(
        &self,
        provider: Option<String>,
        user: String,
    ) -> BoxFuture<'_, Vec<Quota>> {
        let _ = (provider, user);
        Box::pin(async { Vec::new() })
    }
}

/// Who is asking, as far as the gateway can tell.
#[derive(Clone, Copy)]
pub struct QuotaCaller<'a> {
    key: &'a str,
    identity: Option<&'a Identity>,
}

impl<'a> QuotaCaller<'a> {
    /// A caller that presented `key`, optionally authenticated as a gateway key.
    #[must_use]
    pub fn new(key: &'a str, identity: Option<&'a Identity>) -> Self {
        Self { key, identity }
    }

    /// The secret the caller presented.
    #[must_use]
    pub fn key(&self) -> &'a str {
        self.key
    }

    /// The gateway-key identity when the key store authenticated the request.
    #[must_use]
    pub fn identity(&self) -> Option<&'a Identity> {
        self.identity
    }
}

impl std::fmt::Debug for QuotaCaller<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QuotaCaller")
            .finish_non_exhaustive()
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
                .filter(|quota| {
                    !quota.provider.contains('/') && scope.card(&quota.provider, &quota.user)
                })
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

    /// Whether `provider` may be refreshed for this caller.
    #[must_use]
    pub fn may_refresh(&self, provider: &str, user: &str) -> bool {
        match self {
            Self::All => true,
            Self::Scoped(scope) => !provider.is_empty() && scope.card(provider, user),
            Self::Refused => false,
        }
    }

    /// Cards in scope that exist in `all` (for a refresh-all ask).
    #[must_use]
    pub fn scoped_cards<'a>(&self, all: &'a [Quota]) -> Vec<&'a Quota> {
        match self {
            Self::All => all.iter().collect(),
            Self::Scoped(scope) => all
                .iter()
                .filter(|quota| {
                    !quota.provider.contains('/') && scope.card(&quota.provider, &quota.user)
                })
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

pub(crate) fn routes(router: super::readonly::RouteSet) -> super::readonly::RouteSet {
    router
        .route(&["GET"], "/v1/hofund/quotas", axum::routing::get(quotas))
        .route(
            &["GET"],
            "/v1/hofund/quotas/history",
            axum::routing::get(history),
        )
        .route(
            &["GET"],
            "/v1/hofund/quotas/cards",
            axum::routing::get(cards),
        )
        .route(
            &["POST"],
            "/v1/hofund/quotas/refresh",
            axum::routing::post(cards_refresh),
        )
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
    identity: Option<&Identity>,
) -> Result<(&'a dyn QuotaSource, QuotaAccess), Box<Response>> {
    authenticate_request(app, headers, uri, None, None)?;
    let Some(source) = app.quota_source.as_deref() else {
        return Err(Box::new(list_failure(
            StatusCode::NOT_FOUND,
            "not_found",
            "not found",
            None,
        )));
    };
    let key = presented(headers, uri);
    match (app.quota_scope)(QuotaCaller::new(&key, identity)) {
        QuotaAccess::Refused => Err(Box::new(held_failure(
            "this gateway key is held to a budget, models or accounts, so it is told nothing of this gateway's accounts until its owner turns on Quotas for it (Gateway keys, or `hofund gateway-key quotas <id> on`); it is then told the accounts it may use of each provider whose every model it may use. GET /v1/hofund/limit tells it its own limit and what holds it",
        ))),
        access => Ok((source, access)),
    }
}

async fn quotas(
    State(app): State<Arc<Inner>>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
) -> Response {
    let identity = request.extensions().get::<Identity>();
    let (source, access) = match admit(&app, &headers, &uri, identity) {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    let report = source.report(QuotaQuery::parse(uri.query())).await;
    list(serde_json::to_vec(&ListEnvelope::new(
        access.quotas(report.data),
    )))
}

async fn history(
    State(app): State<Arc<Inner>>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
) -> Response {
    let identity = request.extensions().get::<Identity>();
    let (source, access) = match admit(&app, &headers, &uri, identity) {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    let lines = source.history(QuotaHistoryQuery::parse(uri.query()));
    list(serde_json::to_vec(&ListEnvelope::new(
        access.histories(lines),
    )))
}

async fn cards(
    State(app): State<Arc<Inner>>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
) -> Response {
    let identity = request.extensions().get::<Identity>();
    let (source, access) = match admit(&app, &headers, &uri, identity) {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    let all = source.kept_cards().await;
    let had_cards = !all.is_empty();
    let filtered = access.quotas(all);
    if matches!(access, QuotaAccess::Scoped(_)) && filtered.is_empty() && had_cards {
        return held_failure(
            "this gateway key is held to a budget, models or accounts, so it is told nothing of this gateway's accounts: GET /v1/hofund/limit tells it its own limit",
        );
    }
    list(serde_json::to_vec(&ListEnvelope::new(filtered)))
}

async fn cards_refresh(
    State(app): State<Arc<Inner>>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
) -> Response {
    let identity = request.extensions().get::<Identity>();
    let (source, access) = match admit(&app, &headers, &uri, identity) {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    let (provider, user) = parse_refresh_query(uri.query());
    if let Some(provider) = provider.as_deref() {
        if provider.contains('/') {
            return bad_request("this gateway reads only its own cards again");
        }
        if !access.may_refresh(provider, &user) {
            return held_failure(
                "this gateway key is held to a budget, models or accounts, so it is told nothing of this gateway's accounts: GET /v1/hofund/limit tells it its own limit",
            );
        }
    }
    let refreshed = source
        .refresh_kept_cards(provider.clone(), user.clone())
        .await;
    let had_cards = !refreshed.is_empty();
    let filtered = access.quotas(refreshed);
    if matches!(access, QuotaAccess::Scoped(_)) && filtered.is_empty() && had_cards {
        return held_failure(
            "this gateway key is held to a budget, models or accounts, so it is told nothing of this gateway's accounts: GET /v1/hofund/limit tells it its own limit",
        );
    }
    list(serde_json::to_vec(&ListEnvelope::new(filtered)))
}

fn parse_refresh_query(query: Option<&str>) -> (Option<String>, String) {
    let mut provider = None;
    let mut user = String::new();
    let Some(query) = query else {
        return (provider, user);
    };
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match key {
            "provider" => provider = Some(value.to_owned()),
            "user" => user = value.to_owned(),
            _ => {}
        }
    }
    (provider, user)
}

fn list(encoded: Result<Vec<u8>, serde_json::Error>) -> Response {
    match encoded {
        Ok(bytes) => json_response(StatusCode::OK, bytes),
        Err(_) => list_failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "encode",
            None,
        ),
    }
}

fn list_failure(status: StatusCode, kind: &str, message: &str, code: Option<&str>) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": kind,
            "param": null,
            "code": code,
        }
    });
    (status, axum::Json(body)).into_response()
}

fn held_failure(message: &str) -> Response {
    list_failure(
        StatusCode::FORBIDDEN,
        "permission_error",
        message,
        Some(KEY_HELD_CODE),
    )
}

fn bad_request(message: &str) -> Response {
    list_failure(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        message,
        None,
    )
}

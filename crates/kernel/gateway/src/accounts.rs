//! `GET /v1/hofund/accounts`: every configured account and key, with no secrets.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use hofund_control_proto::ListEnvelope;
use hofund_router::account_pin::AccountFace;
use serde::Serialize;

use super::endpoints::{percent_decode, query_key};
use super::quotas::{KEY_HELD_CODE, QuotaAccess, QuotaCaller};
use super::{Inner, authenticate_request, bearer, header_secret, json_response};

/// One row of `GET /v1/hofund/accounts`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountRow {
    provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    provider_name: String,
    id: String,
    pin: String,
    kind: String,
    label: String,
    user: String,
    plan: String,
    on: bool,
}

impl AccountRow {
    fn from_face(face: &AccountFace) -> Self {
        Self {
            provider: face.provider.clone(),
            provider_name: face.provider_name.clone(),
            id: face.id.clone(),
            pin: face.pin(),
            kind: face.kind.as_str().to_owned(),
            label: face.label.clone(),
            user: face.user.clone(),
            plan: face.plan.clone(),
            on: face.on,
        }
    }
}

pub(crate) fn routes(router: super::readonly::RouteSet) -> super::readonly::RouteSet {
    router.route(
        &["GET"],
        "/v1/hofund/accounts",
        axum::routing::get(accounts),
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

fn wanted_provider(query: Option<&str>) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == "provider" {
            let value = percent_decode(value);
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

async fn accounts(
    State(app): State<Arc<Inner>>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
) -> Response {
    let inherited = request
        .extensions()
        .get::<hofund_secrets::Identity>()
        .cloned();
    let identity = match authenticate_request(&app, &headers, &uri, inherited, None) {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
    let access = (app.quota_scope)(QuotaCaller::new(
        &presented(&headers, &uri),
        Some(&identity),
    ));
    if matches!(access, QuotaAccess::Refused) {
        return held_failure(
            "this gateway key is held to a budget, models or accounts, so it is told nothing of this gateway's accounts until its owner turns on Quotas for it (Gateway keys, or `hofund gateway-key quotas <id> on`); it is then told the accounts it may use of each provider whose every model it may use. GET /v1/hofund/limit tells it its own limit and what holds it",
        );
    }
    let wanted = wanted_provider(uri.query());
    let router = app.snapshot.load();
    let mut rows: Vec<AccountRow> = router
        .accounts()
        .iter()
        .filter(|face| {
            wanted
                .as_ref()
                .is_none_or(|provider| face.provider.eq_ignore_ascii_case(provider))
        })
        .filter(|face| visible(&access, face))
        .map(AccountRow::from_face)
        .collect();
    rows.sort_by(|left, right| {
        (&left.provider, &left.kind, &left.id).cmp(&(&right.provider, &right.kind, &right.id))
    });
    match serde_json::to_vec(&ListEnvelope::new(rows)) {
        Ok(bytes) => json_response(StatusCode::OK, bytes),
        Err(_) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            br#"{"error":{"message":"encode","type":"api_error","param":null,"code":null}}"#
                .to_vec(),
        ),
    }
}

fn visible(access: &QuotaAccess, face: &AccountFace) -> bool {
    match access {
        QuotaAccess::All => true,
        QuotaAccess::Refused => false,
        QuotaAccess::Scoped(scope) => {
            scope.card(&face.provider, &face.id)
                || (!face.user.is_empty() && scope.card(&face.provider, &face.user))
        }
    }
}

fn held_failure(message: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "permission_error",
            "param": null,
            "code": KEY_HELD_CODE,
        }
    });
    (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
}

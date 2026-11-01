use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use hofund_secrets::Identity;
use hofund_usage::{CallerTotals, Window};
use serde_json::{Value, json};

use super::{Inner, authenticate_request, json_response, readonly, route_scope, text_response};

const PAGE: &str = include_str!("me.html");

pub(super) fn routes(router: readonly::RouteSet) -> readonly::RouteSet {
    router
        .route(&["GET"], "/me", axum::routing::get(page))
        .route(&["GET"], "/v1/hofund/me", axum::routing::get(info))
}

async fn page() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        )
        .header(header::X_FRAME_OPTIONS, "DENY")
        .header("x-content-type-options", "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(PAGE))
        .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "response"))
}

async fn info(State(app): State<Arc<Inner>>, request: Request) -> Response {
    if has_key_query(request.uri().query()) {
        return error(
            StatusCode::BAD_REQUEST,
            "send the gateway key as Authorization: Bearer <key> or x-api-key, never in the address",
        );
    }
    let inherited = request.extensions().get::<Identity>().cloned();
    let identity =
        match authenticate_request(&app, request.headers(), request.uri(), inherited, None) {
            Ok(identity) if !identity.key_id.is_empty() => identity,
            _ => {
                return error(
                    StatusCode::UNAUTHORIZED,
                    "send an enabled gateway key as Authorization: Bearer <key> or x-api-key",
                );
            }
        };
    match body(&app, &identity) {
        Ok(body) => {
            let mut response = json_response(StatusCode::OK, body);
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            response
        }
        Err(()) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read gateway-key usage",
        ),
    }
}

fn body(app: &Inner, identity: &Identity) -> Result<Vec<u8>, ()> {
    let mut usage = serde_json::Map::new();
    let mut top_models = Vec::new();
    for (name, window) in [
        ("today", Window::Today),
        ("7d", Window::Week),
        ("30d", Window::Month),
    ] {
        let (totals, models) = app
            .usage
            .caller_summary(&identity.key_id, window.since_now())
            .map_err(|_| ())?;
        usage.insert(name.to_owned(), totals_json(totals));
        if window == Window::Month {
            top_models = models
                .into_iter()
                .take(8)
                .map(|model| {
                    let mut value = totals_json(model.totals);
                    value["id"] = Value::String(model.id);
                    value
                })
                .collect();
        }
    }
    // The same status `/v1/hofund/limit` reports, reservations included.
    let limits = app
        .budget
        .status(identity, chrono::Local::now())
        .map_err(|_| ())?;
    let models = if identity.read_only {
        Vec::new()
    } else {
        app.snapshot
            .load()
            .models_scoped(Some(&route_scope(identity)))
            .into_iter()
            .map(|id| json!({"id": id, "label": id}))
            .collect()
    };
    serde_json::to_vec(&json!({
        "name": identity.key_name,
        "readOnly": identity.read_only,
        "limits": limits,
        "usage": usage,
        "topModels": top_models,
        "models": models,
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .map_err(|_| ())
}

fn totals_json(totals: CallerTotals) -> Value {
    json!({
        "calls": totals.calls,
        "errors": totals.errors,
        "input": totals.input,
        "output": totals.output,
        "cache_read": 0,
        "cache_write": 0,
        "cost": 0.0,
        "unpriced": 0,
    })
}

fn has_key_query(query: Option<&str>) -> bool {
    query.is_some_and(|query| {
        query
            .split('&')
            .any(|pair| pair.split_once('=').map_or(pair, |(name, _)| name) == "key")
    })
}

fn error(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        serde_json::to_vec(&json!({
            "error": {"message": message, "type": "invalid_request_error", "param": null, "code": null}
        }))
        .unwrap_or_else(|_| b"{\"error\":{\"message\":\"request failed\"}}".to_vec()),
    )
}

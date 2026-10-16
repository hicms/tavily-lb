use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, CONTENT_LENGTH, HOST};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use bytes::Bytes;
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::app::AppState;
use crate::auth;
use crate::egress::{EgressError, EgressResponse as Upstream, Route};
use crate::pool::{KeyEntry, now_ms};
use crate::usage_gate::{GateError, is_usage_url};

pub const X_PROXY_REQUEST_ID: &str = "x-proxy-request-id";
pub const X_TAVILY_REQUEST_ID: &str = "x-tavily-request-id";

const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

pub fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

pub async fn handle(State(app): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, app.cfg.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "Request body could not be read or exceeds max_body_bytes.",
            );
        }
    };

    let extracted = auth::extract(&parts.headers, parts.uri.query(), body);
    if !extracted.authorized(&app.master_key) {
        if app.cfg.admin_ui
            && !extracted.has_credential()
            && parts.method == Method::GET
            && parts.uri.path() == "/"
            && accepts_html(&parts.headers)
        {
            return Redirect::temporary("/admin/").into_response();
        }
        return json_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Missing or invalid master key.",
        );
    }

    forward(
        &app,
        Forward {
            method: parts.method,
            path: parts.uri.path().to_string(),
            query: extracted.query,
            headers: parts.headers,
            body: extracted.body,
        },
    )
    .await
}

fn accepts_html(headers: &HeaderMap) -> bool {
    match headers.get(ACCEPT).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(v) => v.contains("text/html"),
    }
}

pub struct Forward {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

enum Outcome {
    Done(Upstream, String),
    RateLimited(Upstream, String),
    Timeout,
    TransportError(String),
    NoKeys,
    UsageBlocked(GateError),
}

pub async fn forward(app: &AppState, req: Forward) -> Response {
    let request_id = Uuid::new_v4().to_string();
    let started = Instant::now();
    let (outcome, attempts) = try_keys(app, &req, &request_id, started).await;
    let latency_ms = started.elapsed().as_millis() as u64;

    let mut resp = match outcome {
        Outcome::UsageBlocked(e) => e.response(),
        Outcome::Done(up, alias) => {
            info!(%request_id, method = %req.method, path = %req.path, status = up.status.as_u16(), key = %alias, attempts, latency_ms, "proxied");
            upstream_response(up)
        }
        Outcome::RateLimited(up, alias) => {
            warn!(%request_id, method = %req.method, path = %req.path, status = 429, key = %alias, attempts, latency_ms, "all attempted keys rate limited");
            upstream_response(up)
        }
        Outcome::Timeout => {
            warn!(%request_id, method = %req.method, path = %req.path, attempts, latency_ms, "request deadline exceeded");
            json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "Upstream did not respond before the request deadline.",
            )
        }
        Outcome::TransportError(err) => {
            warn!(%request_id, method = %req.method, path = %req.path, attempts, latency_ms, error = %err, "upstream unreachable");
            json_error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "Upstream request failed for every attempted key.",
            )
        }
        Outcome::NoKeys => {
            warn!(%request_id, method = %req.method, path = %req.path, attempts, latency_ms, "no available keys");
            json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_available_keys",
                "No active Tavily API keys with remaining quota.",
            )
        }
    };
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert(X_PROXY_REQUEST_ID, v);
    }
    resp
}

async fn try_keys(
    app: &AppState,
    req: &Forward,
    request_id: &str,
    started: Instant,
) -> (Outcome, usize) {
    let candidates = app.pool.candidates(now_ms());
    if candidates.is_empty() {
        return (Outcome::NoKeys, 0);
    }
    let deadline = started + app.cfg.request_deadline;
    let max = match app.cfg.max_attempts {
        0 => usize::MAX,
        n => n,
    };
    let url = build_url(&app.cfg.upstream_base_url, &req.path, req.query.as_deref());
    let usage = is_usage_url(&url);
    let max = if usage { 1 } else { max };
    let headers = upstream_headers(&req.headers, request_id);

    let mut attempts = 0;
    let mut last_429: Option<(Upstream, String)> = None;
    let mut last_err: Option<String> = None;

    for key in &candidates {
        if attempts >= max {
            break;
        }
        let route = match app.egress.route(key) {
            Ok(route) => route,
            Err(e) => {
                last_err = Some(e.to_string());
                continue;
            }
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        attempts += 1;
        let timeout = remaining.min(app.cfg.upstream_timeout);
        match send(app, req, &url, &headers, key, timeout, route).await {
            Err(SendError::Gate(e)) => return (Outcome::UsageBlocked(e), attempts - 1),
            Err(e) => {
                warn!(%request_id, key = %key.alias(), error = %e, "upstream attempt failed");
                last_err = Some(e.to_string());
            }
            Ok(up) if usage => {
                match up.status.as_u16() {
                    401 => app.pool.mark_invalid(key),
                    432 | 433 => app.pool.mark_exhausted(key),
                    _ => {}
                }
                return (Outcome::Done(up, key.alias()), attempts);
            }
            Ok(up) => match up.status.as_u16() {
                401 => {
                    warn!(%request_id, key = %key.alias(), "key rejected with 401, marking invalid");
                    app.pool.mark_invalid(key);
                }
                429 => {
                    warn!(%request_id, key = %key.alias(), "key rate limited");
                    app.pool.mark_rate_limited(key, app.cfg.cooldown_on_429);
                    last_429 = Some((up, key.alias()));
                }
                432 | 433 => {
                    warn!(%request_id, key = %key.alias(), status = up.status.as_u16(), "key quota exhausted");
                    app.pool.mark_exhausted(key);
                }
                _ => {
                    if up.status == StatusCode::OK && req.method != Method::GET {
                        app.pool.record_success(key);
                    }
                    return (Outcome::Done(up, key.alias()), attempts);
                }
            },
        }
    }

    let outcome = if let Some((up, alias)) = last_429 {
        Outcome::RateLimited(up, alias)
    } else if Instant::now() >= deadline {
        Outcome::Timeout
    } else if let Some(err) = last_err {
        Outcome::TransportError(err)
    } else {
        Outcome::NoKeys
    };
    (outcome, attempts)
}

#[derive(Debug, thiserror::Error)]
enum SendError {
    #[error(transparent)]
    Egress(#[from] EgressError),
    #[error(transparent)]
    Gate(#[from] GateError),
}

async fn send(
    app: &AppState,
    req: &Forward,
    url: &str,
    headers: &HeaderMap,
    key: &KeyEntry,
    timeout: Duration,
    route: Route<'_>,
) -> Result<Upstream, SendError> {
    let _permit = if is_usage_url(url) {
        Some(app.usage_gate.reserve(&key.key, route.exit_ip)?)
    } else {
        None
    };
    let mut builder = route
        .client
        .request(req.method.clone(), url)
        .headers(headers.clone())
        .bearer_auth(&key.key)
        .timeout(timeout);
    if !req.body.is_empty() {
        builder = builder.body(req.body.clone());
    }
    Ok(route.send(builder).await?)
}

fn build_url(base: &str, path: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() => format!("{base}{path}?{q}"),
        _ => format!("{base}{path}"),
    }
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

fn upstream_headers(client: &HeaderMap, request_id: &str) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(client.len() + 1);
    for (name, value) in client {
        if name == HOST
            || name == CONTENT_LENGTH
            || name == axum::http::header::AUTHORIZATION
            || is_hop_by_hop(name)
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Ok(v) = HeaderValue::from_str(request_id) {
        out.insert(X_PROXY_REQUEST_ID, v);
    }
    out
}

fn upstream_response(up: Upstream) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = up.status;
    let headers = resp.headers_mut();
    for (name, value) in &up.headers {
        if name == CONTENT_LENGTH || is_hop_by_hop(name) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    if let Some(rid) = tavily_request_id(&up.body).and_then(|r| HeaderValue::from_str(&r).ok()) {
        headers.insert(X_TAVILY_REQUEST_ID, rid);
    }
    *resp.body_mut() = Body::from(up.body);
    resp
}

fn tavily_request_id(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Ids {
        request_id: Option<String>,
        #[serde(rename = "requestId")]
        request_id_camel: Option<String>,
    }
    let ids: Ids = serde_json::from_slice(body).ok()?;
    ids.request_id
        .filter(|s| !s.is_empty())
        .or(ids.request_id_camel.filter(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_extraction() {
        assert_eq!(
            tavily_request_id(br#"{"results":[],"request_id":"r1"}"#).as_deref(),
            Some("r1")
        );
        assert_eq!(
            tavily_request_id(br#"{"requestId":"r2"}"#).as_deref(),
            Some("r2")
        );
        assert_eq!(tavily_request_id(br#"{"request_id":""}"#), None);
        assert_eq!(tavily_request_id(b"[]"), None);
    }

    #[test]
    fn header_filtering() {
        let mut h = HeaderMap::new();
        h.insert(HOST, HeaderValue::from_static("proxy"));
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("3"));
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer master"),
        );
        h.insert("connection", HeaderValue::from_static("keep-alive"));
        h.insert("x-custom", HeaderValue::from_static("1"));
        let out = upstream_headers(&h, "rid");
        assert_eq!(out.len(), 2);
        assert_eq!(out.get("x-custom").unwrap(), "1");
        assert_eq!(out.get(X_PROXY_REQUEST_ID).unwrap(), "rid");
    }

    #[test]
    fn url_building() {
        assert_eq!(build_url("https://a", "/search", None), "https://a/search");
        assert_eq!(
            build_url("https://a", "/usage", Some("x=1")),
            "https://a/usage?x=1"
        );
    }
}

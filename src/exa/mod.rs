mod request;
mod response;

use std::collections::HashMap;
use std::time::Instant;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::app::AppState;
use crate::{auth, proxy};
use request::{ContentOptions, ContentsRequest, SearchRequest};
use response::{TavilyResponse, TavilyResult};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/search", any(search))
        .route("/contents", any(contents))
        .fallback(not_found)
}

struct ApiError {
    status: StatusCode,
    tag: &'static str,
    message: String,
    retry_after: Option<HeaderValue>,
}

impl ApiError {
    fn new(status: StatusCode, tag: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            tag,
            message: message.into(),
            retry_after: None,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "INVALID_REQUEST", message)
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "UNSUPPORTED_PARAMETER", message)
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "UPSTREAM_ERROR", message)
    }

    fn into_response(self, id: &str) -> Response {
        let mut response = (
            self.status,
            Json(json!({
                "requestId":id, "error":self.message, "tag":self.tag,
            })),
        )
            .into_response();
        if let Some(retry) = self.retry_after {
            response.headers_mut().insert(header::RETRY_AFTER, retry);
        }
        if self.status == StatusCode::METHOD_NOT_ALLOWED {
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST"));
        }
        response
    }
}

#[derive(Clone, Copy)]
enum Endpoint {
    Search,
    Contents,
}

async fn search(State(app): State<AppState>, req: Request) -> Response {
    handle(app, req, Endpoint::Search).await
}

async fn contents(State(app): State<AppState>, req: Request) -> Response {
    handle(app, req, Endpoint::Contents).await
}

async fn not_found() -> Response {
    let id = Uuid::new_v4().to_string();
    with_id(
        ApiError::new(StatusCode::NOT_FOUND, "NOT_FOUND", "Unknown Exa endpoint.")
            .into_response(&id),
        &id,
    )
}

fn with_id(mut response: Response, id: &str) -> Response {
    let value = HeaderValue::from_str(id).expect("UUID is a valid header value");
    response.headers_mut().insert("x-request-id", value.clone());
    response
        .headers_mut()
        .insert(proxy::X_PROXY_REQUEST_ID, value);
    response
}

async fn handle(app: AppState, req: Request, endpoint: Endpoint) -> Response {
    let id = Uuid::new_v4().to_string();
    let started = Instant::now();
    let result = tokio::time::timeout(app.cfg.request_deadline, execute(&app, req, endpoint)).await;
    let response = match result {
        Ok(Ok(mut body)) => {
            body["requestId"] = json!(id);
            body["searchTime"] = json!(started.elapsed().as_secs_f64() * 1000.0);
            Json(body).into_response()
        }
        Ok(Err(error)) => error.into_response(&id),
        Err(_) => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "UPSTREAM_TIMEOUT",
            "Request deadline exceeded.",
        )
        .into_response(&id),
    };
    with_id(response, &id)
}

async fn execute(app: &AppState, req: Request, endpoint: Endpoint) -> Result<Value, ApiError> {
    let (parts, body) = req.into_parts();
    let key = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok());
    let bearer = auth::bearer_token(&parts.headers);
    // A supplied credential must be valid even when a second credential is present.
    let valid = (key.is_some() || bearer.is_some())
        && (!parts.headers.contains_key("x-api-key") || key.is_some())
        && parts.headers.get_all("x-api-key").iter().count() <= 1
        && parts.headers.get_all(header::AUTHORIZATION).iter().count() <= 1
        && key.is_none_or(|k| auth::verify(&app.master_key, k))
        && bearer
            .as_deref()
            .is_none_or(|k| auth::verify(&app.master_key, k))
        && (!parts.headers.contains_key(header::AUTHORIZATION) || bearer.is_some());
    if !valid {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Missing or invalid master key.",
        ));
    }
    if parts.method != Method::POST {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "METHOD_NOT_ALLOWED",
            "Use POST for this endpoint.",
        ));
    }
    if parts.uri.query().is_some_and(|q| !q.is_empty()) {
        return Err(ApiError::invalid(
            "Pass parameters in the JSON body, not the query string.",
        ));
    }
    let json_content = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        });
    if !json_content || parts.headers.contains_key(header::CONTENT_ENCODING) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "INVALID_REQUEST",
            "An uncompressed application/json body is required.",
        ));
    }
    let bytes = to_bytes(body, app.cfg.max_body_bytes).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "INVALID_REQUEST",
            "Request body could not be read or exceeds max_body_bytes.",
        )
    })?;
    match endpoint {
        Endpoint::Search => {
            let request: SearchRequest = parse(&bytes)?;
            request.validate()?;
            run_search(app, request).await
        }
        Endpoint::Contents => {
            let request: ContentsRequest = parse(&bytes)?;
            let (urls, options) = request.into_parts()?;
            run_contents(app, &urls, &options).await
        }
    }
}

fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes)
        .map_err(|e| ApiError::invalid(format!("Invalid or unsupported parameters: {e}")))
}

async fn forward(app: &AppState, path: &str, body: Value) -> Result<TavilyResponse, ApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let response = proxy::forward(
        app,
        proxy::Forward {
            method: Method::POST,
            path: path.into(),
            query: None,
            headers,
            body: Bytes::from(body.to_string()),
        },
    )
    .await;
    let status = response.status();
    if status != StatusCode::OK {
        let (status, tag, message) = match status.as_u16() {
            400 | 422 => (
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "The upstream rejected the converted request.",
            ),
            429 => (
                status,
                "RATE_LIMIT_EXCEEDED",
                "The upstream is rate limited; retry later.",
            ),
            503 => (status, "SERVICE_UNAVAILABLE", "No available upstream keys."),
            504 => (
                status,
                "UPSTREAM_TIMEOUT",
                "The upstream request timed out.",
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_ERROR",
                "The upstream request failed.",
            ),
        };
        let mut error = ApiError::new(status, tag, message);
        error.retry_after = response.headers().get(header::RETRY_AFTER).cloned();
        return Err(error);
    }
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|_| ApiError::upstream("Could not read upstream response."))?;
    let response: TavilyResponse = serde_json::from_slice(&body)
        .map_err(|_| ApiError::upstream("Upstream returned an invalid response body."))?;
    response.validate()?;
    Ok(response)
}

async fn extract(
    app: &AppState,
    urls: &[String],
    query: Option<&str>,
) -> Result<TavilyResponse, ApiError> {
    let mut body = json!({"urls":urls, "format":"markdown"});
    if let Some(query) = query {
        body["query"] = json!(query);
        body["chunks_per_source"] = json!(3);
    }
    let response = forward(app, "/extract", body).await?;
    if response.results.iter().any(|r| !urls.contains(&r.url))
        || response
            .failed_results
            .iter()
            .any(|r| !urls.contains(&r.url))
    {
        return Err(ApiError::upstream(
            "Upstream returned an unexpected extraction URL.",
        ));
    }
    Ok(response)
}

async fn run_search(app: &AppState, request: SearchRequest) -> Result<Value, ApiError> {
    let response = forward(app, "/search", request.upstream_body()).await?;
    let options = request.contents.unwrap_or_default();
    let results: Vec<TavilyResult> = response
        .results
        .into_iter()
        .take(request.num_results.unwrap_or(10) as usize)
        .collect();
    let mut custom_highlights = HashMap::new();
    if let Some(query) = options.highlight_query() {
        let urls: Vec<String> = results.iter().map(|r| r.url.clone()).collect();
        if !urls.is_empty() {
            for result in extract(app, &urls, Some(query)).await?.results {
                if let Some(text) = result.raw_content {
                    custom_highlights.insert(result.url, text);
                }
            }
        }
    }
    let results: Vec<Value> = results
        .iter()
        .map(|r| {
            let highlight = if options.highlight_query().is_some() {
                custom_highlights.get(&r.url).map(String::as_str)
            } else {
                r.content.as_deref()
            };
            r.to_exa(&options, highlight)
        })
        .collect();
    Ok(json!({"results":results}))
}

async fn run_contents(
    app: &AppState,
    urls: &[String],
    options: &ContentOptions,
) -> Result<Value, ApiError> {
    let mut results = Vec::new();
    let mut statuses = Vec::new();
    for batch in urls.chunks(20) {
        let query = options.wants_highlights().then(|| {
            options
                .highlight_query()
                .unwrap_or("Key information and main points")
        });
        let primary_query = if options.wants_text() { None } else { query };
        let primary = extract(app, batch, primary_query).await?;
        let primary: HashMap<_, _> = primary
            .results
            .into_iter()
            .map(|r| (r.url.clone(), r))
            .collect();
        let mut highlights = HashMap::new();
        if options.wants_text() && options.wants_highlights() {
            let successful: Vec<String> = batch
                .iter()
                .filter(|u| primary.contains_key(*u))
                .cloned()
                .collect();
            if !successful.is_empty() {
                for r in extract(app, &successful, query).await?.results {
                    if let Some(text) = r.raw_content {
                        highlights.insert(r.url, text);
                    }
                }
            }
        }
        for url in batch {
            let result = primary.get(url).filter(|r| {
                !(options.wants_text() || options.wants_highlights()) || r.raw_content.is_some()
            });
            let highlight = if options.wants_text() {
                highlights.get(url).map(String::as_str)
            } else {
                result.and_then(|r| r.raw_content.as_deref())
            };
            if let Some(result) =
                result.filter(|_| !options.wants_highlights() || highlight.is_some())
            {
                results.push(result.to_exa(options, highlight));
                statuses.push(json!({"id":url,"status":"success"}));
            } else {
                statuses.push(json!({"id":url,"status":"error","error":{"tag":"CRAWL_ERROR"}}));
            }
        }
    }
    Ok(json!({"results":results,"statuses":statuses}))
}

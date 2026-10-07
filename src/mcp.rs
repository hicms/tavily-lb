//! Minimal stateless MCP server over the streamable HTTP transport.
//!
//! Every POST carries one JSON-RPC message (or a batch) and is answered with a plain
//! `application/json` body; no sessions or SSE streams are kept, which the transport
//! permits and which keeps the endpoint safe behind any load balancer.

use std::sync::LazyLock;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::{ALLOW, CONTENT_TYPE, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::app::AppState;
use crate::auth;
use crate::pool::now_ms;
use crate::proxy::{self, Forward, json_error};

const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const LATEST_VERSION: &str = SUPPORTED_VERSIONS[0];

static TOOLS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("mcp_tools.json")).expect("mcp_tools.json is valid JSON")
});

fn tool_path(name: &str) -> Option<&'static str> {
    match name {
        "tavily-search" => Some("/search"),
        "tavily-extract" => Some("/extract"),
        "tavily-crawl" => Some("/crawl"),
        "tavily-map" => Some("/map"),
        _ => None,
    }
}

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub async fn handle_get() -> Response {
    let mut resp = StatusCode::METHOD_NOT_ALLOWED.into_response();
    resp.headers_mut()
        .insert(ALLOW, HeaderValue::from_static("POST"));
    resp
}

pub async fn handle_post(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    match auth::bearer_token(&headers) {
        Some(t) if auth::verify(&app.master_key, &t) => {}
        _ => {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Missing or invalid master key.",
            );
        }
    }

    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(rpc_error(
                    Value::Null,
                    PARSE_ERROR,
                    &format!("Parse error: {e}"),
                )),
            )
                .into_response();
        }
    };

    match msg {
        Value::Array(batch) => {
            if batch.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(rpc_error(Value::Null, INVALID_REQUEST, "Empty batch")),
                )
                    .into_response();
            }
            let mut out = Vec::new();
            for m in batch {
                if let Some(r) = dispatch(&app, m).await {
                    out.push(r);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(out)).into_response()
            }
        }
        m => match dispatch(&app, m).await {
            Some(r) => Json(r).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// Returns `None` for notifications and client responses, which get no reply.
async fn dispatch(app: &AppState, msg: Value) -> Option<Value> {
    let Value::Object(obj) = msg else {
        return Some(rpc_error(Value::Null, INVALID_REQUEST, "Invalid Request"));
    };
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A JSON-RPC response sent by the client (we never send requests, so ignore it).
        return obj
            .get("id")
            .is_none()
            .then(|| rpc_error(Value::Null, INVALID_REQUEST, "Invalid Request"));
    };
    let id = obj.get("id").cloned()?;
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": &*TOOLS })),
        "tools/call" => call_tool(app, &params).await,
        _ => Err((METHOD_NOT_FOUND, format!("Method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => rpc_error(id, code, &message),
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(LATEST_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "tavily-lb-mcp", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Tavily search/extract/crawl/map tools backed by a pool of Tavily API keys.",
    })
}

async fn call_tool(app: &AppState, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((INVALID_PARAMS, "Missing tool name".to_string()))?;
    let args = match params.get("arguments") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err((INVALID_PARAMS, "Tool arguments must be an object".into())),
    };

    if name == "tavily-usage" {
        let o = app.pool.overview(now_ms());
        let payload = json!({
            "key": { "usage": o.used_quota, "limit": o.total_quota },
            "pool": o,
        });
        return Ok(tool_result(payload.to_string(), payload, false));
    }

    let path = tool_path(name).ok_or((INVALID_PARAMS, format!("Unknown tool: {name}")))?;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(USER_AGENT, HeaderValue::from_static("tavily-lb-mcp"));
    let resp = proxy::forward(
        app,
        Forward {
            method: Method::POST,
            path: path.to_string(),
            query: None,
            headers,
            body: Bytes::from(args.to_string()),
        },
    )
    .await;

    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&body).into_owned();
    let structured = match serde_json::from_slice::<Value>(&body) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({ "raw": text }),
    };
    if status.is_success() {
        Ok(tool_result(text, structured, false))
    } else {
        Ok(tool_result(
            format!("Upstream status {}: {text}", status.as_u16()),
            structured,
            true,
        ))
    }
}

fn tool_result(text: String, structured: Value, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_json_is_well_formed() {
        let tools = TOOLS.as_array().unwrap();
        assert_eq!(tools.len(), 5);
        for t in tools {
            let name = t["name"].as_str().unwrap();
            assert!(t["inputSchema"].is_object(), "{name} schema");
            assert!(
                name == "tavily-usage" || tool_path(name).is_some(),
                "{name} path"
            );
        }
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(
            initialize(&json!({"protocolVersion": "2025-03-26"}))["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(
            initialize(&json!({"protocolVersion": "1999-01-01"}))["protocolVersion"],
            LATEST_VERSION
        );
    }
}

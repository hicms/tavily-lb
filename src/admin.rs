use axum::Json;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::app::{AppState, MasterKeySource};
use crate::auth;
use crate::pool::{KeyEntry, KeyPatch, KeyStatus, PoolError, mask_key, now_ms};
use crate::proxy::json_error;

pub fn router(app: AppState) -> Router<AppState> {
    Router::new()
        .route("/overview", get(overview))
        .route("/settings", get(settings))
        .route("/keys", get(list_keys).post(import_keys))
        .route("/keys/export", get(export_keys))
        .route("/keys/invalid", axum::routing::delete(delete_invalid))
        .route(
            "/keys/{id}",
            axum::routing::patch(update_key).delete(delete_key),
        )
        .route("/keys/{id}/raw", get(raw_key))
        .route("/keys/{id}/reset", post(reset_key))
        .route("/keys/{id}/sync", post(sync_one))
        .route("/sync", get(sync_status).post(sync_start))
        .fallback(|| async {
            json_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "Unknown admin endpoint.",
            )
        })
        .route_layer(middleware::from_fn_with_state(app, require_master))
}

async fn require_master(State(app): State<AppState>, req: Request, next: Next) -> Response {
    match auth::bearer_token(req.headers()) {
        Some(t) if auth::verify(&app.master_key, &t) => next.run(req).await,
        _ => json_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Missing or invalid master key.",
        ),
    }
}

async fn persist(app: &AppState) {
    if let Err(e) = app.persister.save_now(&app.pool).await {
        warn!(error = %e, "failed to persist state; will retry on next flush");
    }
}

#[derive(Serialize)]
pub struct KeyDto {
    id: u64,
    key: String,
    alias: String,
    total_quota: u32,
    used_quota: u32,
    remaining: u32,
    active: bool,
    invalid: bool,
    status: KeyStatus,
    cooldown_seconds: u64,
    last_used_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl KeyDto {
    fn from_entry(k: &KeyEntry, now: u64) -> Self {
        Self {
            id: k.id,
            key: mask_key(&k.key),
            alias: k.alias(),
            total_quota: k.total_quota(),
            used_quota: k.used_quota(),
            remaining: k.remaining(),
            active: k.is_active(),
            invalid: k.is_invalid(),
            status: k.status(now),
            cooldown_seconds: k.cooldown_until_ms().saturating_sub(now).div_ceil(1000),
            last_used_at: k.last_used_at(),
            created_at: k.created_at,
        }
    }
}

fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "not_found", "Key not found.")
}

async fn overview(State(app): State<AppState>) -> Response {
    Json(app.pool.overview(now_ms())).into_response()
}

async fn settings(State(app): State<AppState>) -> Response {
    let source = match app.master_key_source {
        MasterKeySource::Config => "config",
        MasterKeySource::Stored => "state_file",
        MasterKeySource::Generated => "generated",
    };
    Json(serde_json::json!({
        "config": &app.cfg,
        "master_key_source": source,
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

async fn list_keys(State(app): State<AppState>) -> Response {
    let now = now_ms();
    let mut items: Vec<KeyDto> = app
        .pool
        .list()
        .iter()
        .map(|k| KeyDto::from_entry(k, now))
        .collect();
    items.sort_by_key(|k| std::cmp::Reverse(k.id));
    Json(serde_json::json!({ "items": items })).into_response()
}

#[derive(Deserialize)]
struct ImportBody {
    text: String,
    total_quota: Option<u32>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct ImportLineError {
    pub line: usize,
    pub reason: String,
}

#[derive(Serialize, Debug, Default)]
pub struct ImportResult {
    pub added: usize,
    pub duplicates: usize,
    pub errors: Vec<ImportLineError>,
}

pub struct ParsedKey {
    pub key: String,
    pub alias: Option<String>,
    pub total_quota: Option<u32>,
}

/// Parses one import line: `key`, `key,alias` or `key,alias,total_quota`.
/// Blank lines and `#` comments yield `Ok(None)`.
pub fn parse_key_line(line: &str) -> Result<Option<ParsedKey>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut parts = line.split([',', '\t']).map(str::trim);
    let key = parts.next().unwrap_or_default();
    if !key.starts_with("tvly-") || key.len() < 10 || key.chars().any(char::is_whitespace) {
        return Err("key must look like tvly-xxxx".into());
    }
    let alias = parts.next().filter(|s| !s.is_empty()).map(str::to_string);
    let total_quota = match parts.next().filter(|s| !s.is_empty()) {
        None => None,
        Some(q) => match q.parse::<u32>() {
            Ok(n) if n > 0 => Some(n),
            _ => return Err(format!("invalid total quota {q:?}")),
        },
    };
    Ok(Some(ParsedKey {
        key: key.to_string(),
        alias,
        total_quota,
    }))
}

pub fn import_text(app: &AppState, text: &str, default_quota: u32) -> ImportResult {
    let mut result = ImportResult::default();
    for (idx, line) in text.lines().enumerate() {
        match parse_key_line(line) {
            Ok(None) => {}
            Ok(Some(p)) => {
                let alias = p.alias.unwrap_or_else(|| mask_key(&p.key));
                match app
                    .pool
                    .add(&p.key, &alias, p.total_quota.unwrap_or(default_quota))
                {
                    Ok(_) => result.added += 1,
                    Err(PoolError::Duplicate) => result.duplicates += 1,
                    Err(e) => result.errors.push(ImportLineError {
                        line: idx + 1,
                        reason: e.to_string(),
                    }),
                }
            }
            Err(reason) => result.errors.push(ImportLineError {
                line: idx + 1,
                reason,
            }),
        }
    }
    result
}

async fn import_keys(State(app): State<AppState>, Json(body): Json<ImportBody>) -> Response {
    let quota = body
        .total_quota
        .filter(|q| *q > 0)
        .unwrap_or(app.cfg.default_total_quota);
    let result = import_text(&app, &body.text, quota);
    if result.added > 0 {
        persist(&app).await;
    }
    Json(result).into_response()
}

async fn export_keys(State(app): State<AppState>) -> Response {
    let mut out = String::new();
    for k in app.pool.list().iter().filter(|k| !k.is_invalid()) {
        out.push_str(&k.key);
        out.push('\n');
    }
    (
        [
            (CONTENT_TYPE, "text/plain; charset=utf-8"),
            (
                CONTENT_DISPOSITION,
                "attachment; filename=\"tavily-keys.txt\"",
            ),
        ],
        out,
    )
        .into_response()
}

async fn delete_invalid(State(app): State<AppState>) -> Response {
    let deleted = app.pool.remove_invalid();
    if deleted > 0 {
        persist(&app).await;
    }
    Json(serde_json::json!({ "deleted": deleted })).into_response()
}

#[derive(Deserialize)]
struct UpdateBody {
    alias: Option<String>,
    total_quota: Option<u32>,
    used_quota: Option<u32>,
    active: Option<bool>,
}

async fn update_key(
    State(app): State<AppState>,
    Path(id): Path<u64>,
    Json(body): Json<UpdateBody>,
) -> Response {
    if body.total_quota == Some(0) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_total_quota",
            "total_quota must be greater than 0.",
        );
    }
    let patch = KeyPatch {
        alias: body.alias.map(|a| a.trim().to_string()),
        total_quota: body.total_quota,
        used_quota: body.used_quota,
        active: body.active,
    };
    match app.pool.update(id, patch) {
        Ok(k) => {
            persist(&app).await;
            Json(KeyDto::from_entry(&k, now_ms())).into_response()
        }
        Err(_) => not_found(),
    }
}

async fn delete_key(State(app): State<AppState>, Path(id): Path<u64>) -> Response {
    match app.pool.remove(id) {
        Ok(()) => {
            persist(&app).await;
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(_) => not_found(),
    }
}

async fn raw_key(State(app): State<AppState>, Path(id): Path<u64>) -> Response {
    match app.pool.get(id) {
        Some(k) => Json(serde_json::json!({ "key": k.key })).into_response(),
        None => not_found(),
    }
}

async fn reset_key(State(app): State<AppState>, Path(id): Path<u64>) -> Response {
    let Some(k) = app.pool.get(id) else {
        return not_found();
    };
    app.pool.reset_usage(&k);
    persist(&app).await;
    Json(KeyDto::from_entry(&k, now_ms())).into_response()
}

async fn sync_one(State(app): State<AppState>, Path(id): Path<u64>) -> Response {
    let Some(k) = app.pool.get(id) else {
        return not_found();
    };
    let result = crate::usage::sync_key(&app, &k).await;
    persist(&app).await;
    match result {
        Ok(()) => Json(KeyDto::from_entry(&k, now_ms())).into_response(),
        Err(e) => json_error(StatusCode::BAD_GATEWAY, "sync_failed", &e.to_string()),
    }
}

async fn sync_status(State(app): State<AppState>) -> Response {
    Json(app.sync.status()).into_response()
}

async fn sync_start(State(app): State<AppState>) -> Response {
    match app.sync.start(app.clone()) {
        Ok(st) => (StatusCode::ACCEPTED, Json(st)).into_response(),
        Err(st) => (StatusCode::CONFLICT, Json(st)).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lines() {
        assert!(parse_key_line("  ").unwrap().is_none());
        assert!(parse_key_line("# comment").unwrap().is_none());
        let p = parse_key_line("tvly-abcdef123, main ,2000")
            .unwrap()
            .unwrap();
        assert_eq!(p.key, "tvly-abcdef123");
        assert_eq!(p.alias.as_deref(), Some("main"));
        assert_eq!(p.total_quota, Some(2000));
        let p = parse_key_line("tvly-abcdef123").unwrap().unwrap();
        assert!(p.alias.is_none() && p.total_quota.is_none());
        assert!(parse_key_line("sk-123456789").is_err());
        assert!(parse_key_line("tvly-abcdef123,a,zero").is_err());
    }
}

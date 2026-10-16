#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tavily_lb::{AppState, Config, router};
use tempfile::TempDir;
use tower::ServiceExt;

pub const MASTER: &str = "exa-test-master-0123456789";

#[derive(Clone, Debug)]
pub struct Seen {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

pub struct Upstream {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    pub task: tokio::task::JoinHandle<()>,
}

impl Upstream {
    pub fn calls(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn upstream(handler: impl Fn(&Seen) -> (u16, Value) + Send + Sync + 'static) -> Upstream {
    upstream_delayed(handler, Duration::ZERO).await
}

pub async fn upstream_delayed(
    handler: impl Fn(&Seen) -> (u16, Value) + Send + Sync + 'static,
    delay: Duration,
) -> Upstream {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let handler = Arc::new(handler);
    let routes = axum::Router::new().fallback(move |req: Request| {
        let seen = recorded.clone();
        let handler = handler.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = body.collect().await.unwrap().to_bytes();
            let call = Seen {
                path: parts.uri.path().into(),
                headers: parts.headers,
                body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            };
            seen.lock().unwrap().push(call.clone());
            tokio::time::sleep(delay).await;
            let (status, body) = handler(&call);
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .header("retry-after", "7")
                .header("content-encoding", "identity")
                .body(Body::from(body.to_string()))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    Upstream { url, seen, task }
}

pub fn app_with(
    base: &str,
    keys: &[(&str, u32)],
    tweak: impl FnOnce(&mut Config),
) -> (AppState, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config {
        upstream_base_url: base.into(),
        data_file: dir.path().join("state.json"),
        master_key: Some(MASTER.into()),
        upstream_timeout: Duration::from_secs(2),
        request_deadline: Duration::from_secs(5),
        ..Config::default()
    };
    tweak(&mut cfg);
    cfg.validate().unwrap();
    let app = AppState::build(cfg).unwrap();
    for (key, quota) in keys {
        app.pool.add(key, key, *quota).unwrap();
    }
    (app, dir)
}

pub fn post(path: &str, body: Value) -> Request {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-api-key", MASTER)
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub async fn call(app: &AppState, req: Request) -> (StatusCode, HeaderMap, Value) {
    let response = router(app.clone()).oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({"nonJson": String::from_utf8_lossy(&body)}));
    (status, headers, json)
}

pub fn search_response() -> Value {
    json!({"request_id":"tavily-request", "results":[{
        "url":"https://example.com/a", "title":"Example", "score":0.9,
        "content":"Relevant snippet", "raw_content":"完整😀正文 ABC",
        "published_date":"Tue, 11 Mar 2025 17:00:00 GMT"
    }], "response_time":0.25, "usage":{"credits":1}})
}

pub fn extract_response(call: &Seen) -> Value {
    json!({"request_id":"extract-request", "results":call.body["urls"].as_array().unwrap()
        .iter().map(|url| json!({"url":url,"raw_content":if call.body.get("query").is_some() {
            "Relevant chunk"
        } else { "Full page body" }})).collect::<Vec<_>>(), "failed_results":[]})
}

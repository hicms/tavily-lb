use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use tavily_lb::pool::{KeyStatus, now_ms};
use tavily_lb::{AppState, Config, router};
use tempfile::TempDir;
use tower::ServiceExt;

const MASTER: &str = "test-master-key-0123456789";

#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    path: String,
    query: Option<String>,
    auth: Option<String>,
    body: String,
}

impl Seen {
    fn bearer(&self) -> &str {
        self.auth
            .as_deref()
            .unwrap_or("")
            .trim_start_matches("Bearer ")
    }
}

type Handler = Arc<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

struct Upstream {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Upstream {
    fn calls(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn upstream(handler: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) -> Upstream {
    let handler: Handler = Arc::new(handler);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let handler = handler.clone();
        let seen = seen2.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = body.collect().await.unwrap().to_bytes();
            let s = Seen {
                method: parts.method,
                path: parts.uri.path().to_string(),
                query: parts.uri.query().map(str::to_string),
                auth: parts
                    .headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string()),
                body: String::from_utf8_lossy(&body).into_owned(),
            };
            seen.lock().unwrap().push(s.clone());
            let (status, body) = handler(&s);
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream {
        url: format!("http://{addr}"),
        seen,
    }
}

fn app_with(
    base: &str,
    keys: &[(&str, u32)],
    tweak: impl FnOnce(&mut Config),
) -> (AppState, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config {
        upstream_base_url: base.to_string(),
        master_key: Some(MASTER.into()),
        data_file: dir.path().join("state.json"),
        upstream_timeout: Duration::from_secs(5),
        request_deadline: Duration::from_secs(10),
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

async fn call(app: &AppState, req: Request) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = router(app.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn post_json(path: &str, body: &str, bearer: Option<&str>) -> Request {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn key_by(app: &AppState, key: &str) -> Arc<tavily_lb::pool::KeyEntry> {
    app.pool.list().into_iter().find(|k| k.key == key).unwrap()
}

// ---- auth (ported from router_proxy_legacy_test.go) ----

#[tokio::test]
async fn body_api_key_that_is_not_master_is_rejected_without_upstream_call() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[("tvly-pool-1234567890", 1000)], |_| {});
    let req = post_json(
        "/search",
        r#"{"query":"q","api_key":"tvly-direct-0000000000"}"#,
        None,
    );
    let (status, _, _) = call(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(up.calls().is_empty());
}

#[tokio::test]
async fn body_master_key_is_stripped_and_pool_key_used() {
    let up = upstream(|_| (200, r#"{"request_id":"tav-1","results":[]}"#.into())).await;
    let (app, _d) = app_with(&up.url, &[("tvly-pool-1234567890", 1000)], |_| {});
    let body = format!(r#"{{"query":"hello","api_key":"{MASTER}","max_results":5}}"#);
    let (status, headers, _) = call(&app, post_json("/search", &body, None)).await;
    assert_eq!(status, StatusCode::OK);
    let calls = up.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].bearer(), "tvly-pool-1234567890");
    assert_eq!(calls[0].body, r#"{"query":"hello","max_results":5}"#);
    assert_eq!(headers.get("x-tavily-request-id").unwrap(), "tav-1");
    assert!(headers.get("x-proxy-request-id").is_some());
}

#[tokio::test]
async fn query_master_key_is_stripped_and_other_params_kept() {
    let up = upstream(|_| (200, r#"{"key":{"usage":0,"limit":1000}}"#.into())).await;
    let (app, _d) = app_with(&up.url, &[("tvly-pool-1234567890", 1000)], |_| {});
    let req = Request::builder()
        .uri(format!("/usage?api_key={MASTER}&foo=bar"))
        .header("accept", "*/*")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = call(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    let calls = up.calls();
    assert_eq!(calls[0].method, Method::GET);
    assert_eq!(calls[0].path, "/usage");
    assert_eq!(calls[0].query.as_deref(), Some("foo=bar"));
    assert_eq!(calls[0].bearer(), "tvly-pool-1234567890");
}

#[tokio::test]
async fn missing_or_wrong_credentials_are_rejected() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[("tvly-pool-1234567890", 1000)], |_| {});
    let (s1, _, _) = call(&app, post_json("/search", r#"{"query":"q"}"#, None)).await;
    let (s2, _, _) = call(
        &app,
        post_json("/search", r#"{"query":"q"}"#, Some("wrong")),
    )
    .await;
    assert_eq!(s1, StatusCode::UNAUTHORIZED);
    assert_eq!(s2, StatusCode::UNAUTHORIZED);
    assert!(up.calls().is_empty());
}

#[tokio::test]
async fn browser_root_redirects_to_admin_ui() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[], |_| {});
    let req = Request::builder()
        .uri("/")
        .header("accept", "text/html")
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = call(&app, req).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(headers.get("location").unwrap(), "/admin/");
}

// ---- failover (ported from tavily_proxy_retry_test.go) ----

#[tokio::test]
async fn too_many_requests_falls_back_without_exhausting_key() {
    let up = upstream(|s| match s.bearer() {
        "tvly-first-000000" => (429, r#"{"error":"rate_limit"}"#.into()),
        "tvly-second-00000" => (200, r#"{"ok":true}"#.into()),
        _ => (401, "{}".into()),
    })
    .await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-first-000000", 2000), ("tvly-second-00000", 1000)],
        |_| {},
    );
    let (status, _, body) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"ok":true}"#);
    let first = key_by(&app, "tvly-first-000000");
    assert_eq!(first.used_quota(), 0);
    assert_eq!(first.status(now_ms()), KeyStatus::Cooling);
    assert_eq!(key_by(&app, "tvly-second-00000").used_quota(), 1);
}

#[tokio::test]
async fn all_too_many_requests_returns_429_and_keeps_keys_available() {
    let up = upstream(|_| {
        (
            429,
            r#"{"error":"rate_limit","request_id":"req-429"}"#.into(),
        )
    })
    .await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-first-000000", 2000), ("tvly-second-00000", 1000)],
        |_| {},
    );
    let (status, headers, body) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(body.contains("req-429"));
    assert_eq!(headers.get("x-tavily-request-id").unwrap(), "req-429");
    assert_eq!(up.calls().len(), 2);
    // Cooling keys remain candidates.
    assert_eq!(app.pool.candidates(now_ms()).len(), 2);
}

#[tokio::test]
async fn usage_429_is_returned_without_failover_or_cooldown() {
    let up = upstream(|_| (429, r#"{"detail":{"error":"excessive requests"}}"#.into())).await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-first-000000", 2000), ("tvly-second-00000", 1000)],
        |_| {},
    );
    let req = Request::builder()
        .uri("/usage")
        .header("authorization", format!("Bearer {MASTER}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(&app, req).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(body.contains("excessive requests"));
    assert_eq!(up.calls().len(), 1);
    for k in app.pool.list() {
        assert_eq!(k.status(now_ms()), KeyStatus::Available);
    }
}

#[tokio::test]
async fn mixed_failures_prefer_too_many_requests() {
    let up = upstream(|s| match s.bearer() {
        "tvly-429-00000000" => (
            429,
            r#"{"error":"rate_limit","request_id":"req-429"}"#.into(),
        ),
        "tvly-401-00000000" => (401, r#"{"error":"unauthorized"}"#.into()),
        _ => (500, "{}".into()),
    })
    .await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-429-00000000", 2000), ("tvly-401-00000000", 1000)],
        |_| {},
    );
    let req = Request::builder()
        .uri("/search")
        .header("authorization", format!("Bearer {MASTER}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(&app, req).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(body.contains("req-429"));
    let first = key_by(&app, "tvly-429-00000000");
    assert_ne!(first.used_quota(), first.total_quota());
    let second = key_by(&app, "tvly-401-00000000");
    assert!(second.is_invalid() && !second.is_active());
}

#[tokio::test]
async fn quota_exhausted_status_marks_key_and_falls_back() {
    for code in [432u16, 433] {
        let up = upstream(move |s| match s.bearer() {
            "tvly-first-000000" => (code, "{}".into()),
            _ => (200, "{}".into()),
        })
        .await;
        let (app, _d) = app_with(
            &up.url,
            &[("tvly-first-000000", 2000), ("tvly-second-00000", 1000)],
            |_| {},
        );
        let (status, _, _) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
        assert_eq!(status, StatusCode::OK);
        let first = key_by(&app, "tvly-first-000000");
        assert_eq!(first.status(now_ms()), KeyStatus::Exhausted);
    }
}

#[tokio::test]
async fn other_errors_are_returned_without_failover() {
    let up = upstream(|_| (500, r#"{"error":"boom"}"#.into())).await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-first-000000", 2000), ("tvly-second-00000", 1000)],
        |_| {},
    );
    let (status, _, body) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, r#"{"error":"boom"}"#);
    assert_eq!(up.calls().len(), 1);
    assert_eq!(key_by(&app, "tvly-first-000000").used_quota(), 0);
}

#[tokio::test]
async fn unreachable_upstream_returns_502() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let (app, _d) = app_with(
        &dead,
        &[("tvly-first-000000", 2000), ("tvly-second-00000", 1000)],
        |_| {},
    );
    let (status, _, body) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("upstream_error"));
    assert_eq!(app.pool.candidates(now_ms()).len(), 2);
}

#[tokio::test]
async fn no_keys_returns_503() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[], |_| {});
    let (status, _, body) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("no_available_keys"));
}

#[tokio::test]
async fn all_keys_invalid_returns_503() {
    let up = upstream(|_| (401, "{}".into())).await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-first-000000", 2000), ("tvly-second-00000", 1000)],
        |_| {},
    );
    let (status, _, _) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(app.pool.candidates(now_ms()).is_empty());
}

#[tokio::test]
async fn max_attempts_limits_retries() {
    let up = upstream(|_| (429, "{}".into())).await;
    let (app, _d) = app_with(
        &up.url,
        &[
            ("tvly-a-0000000000", 3000),
            ("tvly-b-0000000000", 2000),
            ("tvly-c-0000000000", 1000),
        ],
        |c| c.max_attempts = 2,
    );
    let (status, _, _) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(up.calls().len(), 2);
}

#[tokio::test]
async fn deadline_returns_504() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let slow = axum::Router::new().fallback(|| async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        "{}"
    });
    tokio::spawn(async move { axum::serve(listener, slow).await.unwrap() });
    let (app, _d) = app_with(
        &format!("http://{addr}"),
        &[("tvly-a-0000000000", 1000)],
        |c| {
            c.request_deadline = Duration::from_millis(300);
        },
    );
    let (status, _, body) = call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(body.contains("upstream_timeout"));
}

#[tokio::test]
async fn rate_limited_key_is_tried_last_on_next_request() {
    let up = upstream(|s| match s.bearer() {
        "tvly-first-000000" => (429, "{}".into()),
        _ => (200, "{}".into()),
    })
    .await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-first-000000", 2000), ("tvly-second-00000", 1000)],
        |_| {},
    );
    call(&app, post_json("/search", "{}", Some(MASTER))).await;
    call(&app, post_json("/search", "{}", Some(MASTER))).await;
    let order: Vec<String> = up.calls().iter().map(|s| s.bearer().to_string()).collect();
    assert_eq!(
        order,
        vec![
            "tvly-first-000000",
            "tvly-second-00000",
            "tvly-second-00000"
        ]
    );
}

#[tokio::test]
async fn get_requests_do_not_consume_quota() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[("tvly-a-0000000000", 1000)], |_| {});
    let req = Request::builder()
        .uri("/usage")
        .header("authorization", format!("Bearer {MASTER}"))
        .body(Body::empty())
        .unwrap();
    call(&app, req).await;
    assert_eq!(key_by(&app, "tvly-a-0000000000").used_quota(), 0);
    call(&app, post_json("/search", "{}", Some(MASTER))).await;
    assert_eq!(key_by(&app, "tvly-a-0000000000").used_quota(), 1);
}

#[tokio::test]
async fn healthz_needs_no_auth() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[], |_| {});
    let req = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"ok":true}"#);
}

// ---- admin API ----

fn admin(method: Method, path: &str, body: Option<&str>, bearer: Option<&str>) -> Request {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    match body {
        Some(s) => b
            .header("content-type", "application/json")
            .body(Body::from(s.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

#[tokio::test]
async fn admin_requires_master_key() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[], |_| {});
    let (s1, _, _) = call(&app, admin(Method::GET, "/api/keys", None, None)).await;
    let (s2, _, _) = call(&app, admin(Method::GET, "/api/keys", None, Some("nope"))).await;
    let (s3, _, _) = call(&app, admin(Method::GET, "/api/keys", None, Some(MASTER))).await;
    assert_eq!(
        (s1, s2, s3),
        (
            StatusCode::UNAUTHORIZED,
            StatusCode::UNAUTHORIZED,
            StatusCode::OK
        )
    );
}

#[tokio::test]
async fn admin_import_list_update_delete_and_persist() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, dir) = app_with(&up.url, &[], |_| {});

    let text =
        "tvly-aaaaaaaaaaaa,main,2000\n# comment\n\ntvly-bbbbbbbbbbbb\nsk-bad\ntvly-aaaaaaaaaaaa";
    let body = serde_json::json!({ "text": text }).to_string();
    let (status, _, res) = call(
        &app,
        admin(Method::POST, "/api/keys", Some(&body), Some(MASTER)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let res: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(res["added"], 2);
    assert_eq!(res["duplicates"], 1);
    assert_eq!(res["errors"][0]["line"], 5);

    let (_, _, list) = call(&app, admin(Method::GET, "/api/keys", None, Some(MASTER))).await;
    let list: serde_json::Value = serde_json::from_str(&list).unwrap();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(
        !list.to_string().contains("tvly-aaaaaaaaaaaa"),
        "keys must be masked"
    );
    let main = items.iter().find(|i| i["alias"] == "main").unwrap();
    assert_eq!(main["total_quota"], 2000);
    let id = main["id"].as_u64().unwrap();

    let patch = r#"{"active":false,"alias":"renamed"}"#;
    let (status, _, updated) = call(
        &app,
        admin(
            Method::PATCH,
            &format!("/api/keys/{id}"),
            Some(patch),
            Some(MASTER),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(updated.contains(r#""status":"disabled""#));

    let (_, _, raw) = call(
        &app,
        admin(
            Method::GET,
            &format!("/api/keys/{id}/raw"),
            None,
            Some(MASTER),
        ),
    )
    .await;
    assert!(raw.contains("tvly-aaaaaaaaaaaa"));

    let (_, _, export) = call(
        &app,
        admin(Method::GET, "/api/keys/export", None, Some(MASTER)),
    )
    .await;
    assert_eq!(export.lines().count(), 2);

    let (status, _, _) = call(
        &app,
        admin(
            Method::DELETE,
            &format!("/api/keys/{id}"),
            None,
            Some(MASTER),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(
        &app,
        admin(
            Method::DELETE,
            &format!("/api/keys/{id}"),
            None,
            Some(MASTER),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let saved = tavily_lb::store::load(&dir.path().join("state.json")).unwrap();
    assert_eq!(saved.keys.len(), 1);
    assert_eq!(saved.keys[0].key, "tvly-bbbbbbbbbbbb");
}

#[tokio::test]
async fn admin_sync_one_applies_upstream_usage() {
    let up = upstream(|s| match (s.path.as_str(), s.bearer()) {
        ("/usage", "tvly-good-0000000") => (
            200,
            r#"{"key":{"usage":42,"limit":null},"account":{"plan_limit":500}}"#.into(),
        ),
        ("/usage", _) => (401, r#"{"error":"bad"}"#.into()),
        _ => (404, "{}".into()),
    })
    .await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-good-0000000", 1000), ("tvly-bad-00000000", 1000)],
        |_| {},
    );
    let good = key_by(&app, "tvly-good-0000000");
    let bad = key_by(&app, "tvly-bad-00000000");
    // A successful sync proves the key works again, so it clears an earlier 401.
    app.pool.mark_invalid(&good);

    let (status, _, _) = call(
        &app,
        admin(
            Method::POST,
            &format!("/api/keys/{}/sync", good.id),
            None,
            Some(MASTER),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!((good.used_quota(), good.total_quota()), (42, 500));
    assert!(!good.is_invalid());

    let (status, _, _) = call(
        &app,
        admin(
            Method::POST,
            &format!("/api/keys/{}/sync", bad.id),
            None,
            Some(MASTER),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(bad.is_invalid());
}

#[tokio::test]
async fn admin_sync_all_runs_in_background() {
    let up = upstream(|_| (200, r#"{"key":{"usage":7,"limit":100}}"#.into())).await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-a-0000000000", 1000), ("tvly-b-0000000000", 1000)],
        |c| {
            c.usage_sync.request_interval = Duration::ZERO;
        },
    );
    let (status, _, _) = call(&app, admin(Method::POST, "/api/sync", None, Some(MASTER))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    for _ in 0..50 {
        if !app.sync.status().running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let st = app.sync.status();
    assert!(!st.running);
    assert_eq!((st.total, st.succeeded, st.failed), (2, 2, 0));
    assert!(
        app.pool
            .list()
            .iter()
            .all(|k| k.used_quota() == 7 && k.total_quota() == 100)
    );
}

#[tokio::test]
async fn sync_all_stops_at_the_first_429() {
    let up = upstream(|_| (429, r#"{"detail":{"error":"excessive requests"}}"#.into())).await;
    let (app, _d) = app_with(
        &up.url,
        &[
            ("tvly-a-0000000000", 1000),
            ("tvly-b-0000000000", 1000),
            ("tvly-c-0000000000", 1000),
        ],
        |c| {
            c.usage_sync.request_interval = Duration::ZERO;
        },
    );
    let (status, _, _) = call(&app, admin(Method::POST, "/api/sync", None, Some(MASTER))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    for _ in 0..50 {
        if !app.sync.status().running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let st = app.sync.status();
    assert!(!st.running);
    assert_eq!(
        (st.total, st.done, st.succeeded, st.failed, st.skipped),
        (3, 1, 0, 1, 2)
    );
    assert!(st.errors[0].error.contains("429"));
    assert_eq!(up.calls().len(), 1);
    assert!(app.pool.list().iter().all(|k| !k.is_invalid()));
}

#[tokio::test]
async fn state_survives_restart() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, dir) = app_with(&up.url, &[("tvly-a-0000000000", 1000)], |_| {});
    call(&app, post_json("/search", "{}", Some(MASTER))).await;
    app.persister.save_now(&app.pool).await.unwrap();

    let cfg = Config {
        upstream_base_url: up.url.clone(),
        master_key: Some(MASTER.into()),
        data_file: dir.path().join("state.json"),
        ..Config::default()
    };
    let again = AppState::build(cfg).unwrap();
    let k = key_by(&again, "tvly-a-0000000000");
    assert_eq!(k.used_quota(), 1);
    assert!(k.last_used_at().is_some());
}

#[tokio::test]
async fn generated_master_key_is_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_file: dir.path().join("state.json"),
        ..Config::default()
    };
    let first = AppState::build(cfg.clone()).unwrap();
    assert_eq!(
        first.master_key_source,
        tavily_lb::MasterKeySource::Generated
    );
    first.persister.save_now(&first.pool).await.unwrap();
    let second = AppState::build(cfg).unwrap();
    assert_eq!(second.master_key_source, tavily_lb::MasterKeySource::Stored);
    assert_eq!(first.master_key, second.master_key);
}

#[tokio::test]
async fn keys_file_is_imported_once() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys.txt");
    std::fs::write(&keys, "tvly-aaaaaaaaaaaa\ntvly-bbbbbbbbbbbb,b,50\n").unwrap();
    let cfg = Config {
        master_key: Some(MASTER.into()),
        data_file: dir.path().join("state.json"),
        keys_file: Some(keys),
        ..Config::default()
    };
    let first = AppState::build(cfg.clone()).unwrap();
    assert_eq!(first.pool.list().len(), 2);
    first.persister.save_now(&first.pool).await.unwrap();
    let second = AppState::build(cfg).unwrap();
    assert_eq!(second.pool.list().len(), 2);
    assert_eq!(key_by(&second, "tvly-bbbbbbbbbbbb").total_quota(), 50);
}

// ---- MCP ----

async fn mcp(app: &AppState, body: &str, bearer: Option<&str>) -> (StatusCode, serde_json::Value) {
    let (status, _, text) = call(app, post_json("/mcp", body, bearer)).await;
    let v = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).unwrap()
    };
    (status, v)
}

#[tokio::test]
async fn mcp_requires_master_key() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[("tvly-pool-1234567890", 10)], |_| {});
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    assert_eq!(mcp(&app, body, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        mcp(&app, body, Some("tvly-pool-1234567890")).await.0,
        StatusCode::UNAUTHORIZED
    );
    let req = Request::builder().uri("/mcp").body(Body::empty()).unwrap();
    assert_eq!(call(&app, req).await.0, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn mcp_handshake_and_tool_list() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(&up.url, &[], |_| {});
    let (status, v) = mcp(
        &app,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        Some(MASTER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    assert!(v["result"]["capabilities"]["tools"].is_object());

    let (status, _) = mcp(
        &app,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        Some(MASTER),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (_, v) = mcp(
        &app,
        r#"{"jsonrpc":"2.0","id":"x","method":"tools/list"}"#,
        Some(MASTER),
    )
    .await;
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "tavily-search",
            "tavily-extract",
            "tavily-crawl",
            "tavily-map",
            "tavily-usage"
        ]
    );

    let (_, v) = mcp(
        &app,
        r#"{"jsonrpc":"2.0","id":2,"method":"nope"}"#,
        Some(MASTER),
    )
    .await;
    assert_eq!(v["error"]["code"], -32601);
    let (status, v) = mcp(&app, "{not json", Some(MASTER)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], -32700);
}

#[tokio::test]
async fn mcp_search_goes_through_pool_with_failover() {
    let up = upstream(|s| match s.bearer() {
        "tvly-first-1234567890" => (429, r#"{"detail":"slow down"}"#.into()),
        _ => (200, r#"{"query":"rust","results":[{"title":"t"}]}"#.into()),
    })
    .await;
    let (app, _d) = app_with(
        &up.url,
        &[
            ("tvly-first-1234567890", 100),
            ("tvly-second-1234567890", 50),
        ],
        |_| {},
    );
    let (status, v) = mcp(
        &app,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"tavily-search","arguments":{"query":"rust","max_results":1}}}"#,
        Some(MASTER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let r = &v["result"];
    assert_eq!(r["isError"], false);
    assert_eq!(r["structuredContent"]["results"][0]["title"], "t");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("rust"));

    let calls = up.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|c| c.path == "/search" && c.method == Method::POST)
    );
    assert_eq!(calls[1].body, r#"{"query":"rust","max_results":1}"#);
    assert_eq!(key_by(&app, "tvly-second-1234567890").used_quota(), 1);
    assert_eq!(key_by(&app, "tvly-first-1234567890").used_quota(), 0);
}

#[tokio::test]
async fn mcp_upstream_errors_are_tool_errors() {
    let up = upstream(|_| (400, r#"{"detail":{"error":"bad url"}}"#.into())).await;
    let (app, _d) = app_with(&up.url, &[("tvly-pool-1234567890", 10)], |_| {});
    let (status, v) = mcp(
        &app,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tavily-extract","arguments":{"urls":["x"]}}}"#,
        Some(MASTER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["result"]["isError"], true);
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Upstream status 400")
    );
    assert_eq!(
        v["result"]["structuredContent"]["detail"]["error"],
        "bad url"
    );

    let (_, v) = mcp(
        &app,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tavily-unknown"}}"#,
        Some(MASTER),
    )
    .await;
    assert_eq!(v["error"]["code"], -32602);
}

#[tokio::test]
async fn mcp_usage_tool_reports_pool_totals() {
    let up = upstream(|_| (200, "{}".into())).await;
    let (app, _d) = app_with(
        &up.url,
        &[("tvly-a-123456789012", 100), ("tvly-b-123456789012", 50)],
        |_| {},
    );
    app.pool
        .set_usage(&key_by(&app, "tvly-a-123456789012"), 10, None);
    let (_, v) = mcp(
        &app,
        r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tavily-usage"}},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#,
        Some(MASTER),
    )
    .await;
    let batch = v.as_array().unwrap();
    assert_eq!(batch.len(), 1);
    let s = &batch[0]["result"]["structuredContent"];
    assert_eq!(s["key"]["limit"], 150);
    assert_eq!(s["key"]["usage"], 10);
    assert!(up.calls().is_empty());
}

#[tokio::test]
async fn mcp_can_be_disabled() {
    let up = upstream(|_| (404, r#"{"detail":"not found"}"#.into())).await;
    let (app, _d) = app_with(&up.url, &[("tvly-pool-1234567890", 10)], |c| c.mcp = false);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    // Without the MCP route the request falls through to the proxy.
    let (status, _) = mcp(&app, body, Some(MASTER)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(up.calls()[0].path, "/mcp");
}

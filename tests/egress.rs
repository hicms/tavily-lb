use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use http_body_util::BodyExt;
use tavily_lb::egress::EgressNode;
use tavily_lb::pool::KeyPatch;
use tavily_lb::usage_gate::{GateError, UsageGate};
use tavily_lb::{AppState, Config, router};
use tower::ServiceExt;

const MASTER: &str = "test-master-key-0123456789";

struct Proxy {
    url: String,
    calls: Arc<Mutex<Vec<(String, String)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn proxy(status: u16) -> Proxy {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push((
                req.uri().path().to_string(),
                req.headers()["authorization"].to_str().unwrap().to_string(),
            ));
            Response::builder()
                .status(status)
                .body(Body::from(r#"{"key":{"usage":7,"limit":100}}"#))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Proxy { url, calls, task }
}

fn config(dir: &std::path::Path, proxies: &[&Proxy]) -> Config {
    let mut cfg = Config {
        data_file: dir.join("state.json"),
        master_key: Some(MASTER.into()),
        upstream_base_url: "http://upstream.invalid".into(),
        upstream_timeout: Duration::from_secs(1),
        request_deadline: Duration::from_secs(3),
        max_attempts: 1,
        ..Default::default()
    };
    cfg.egress.nodes = proxies
        .iter()
        .enumerate()
        .map(|(i, p)| EgressNode {
            id: format!("exit-{i}"),
            exit_ip: format!("203.0.113.{}", i + 1).parse().unwrap(),
            proxy_url: p.url.clone(),
            hysteria_config: None,
        })
        .collect();
    cfg.validate().unwrap();
    cfg
}

async fn call(
    app: &AppState,
    path: &str,
    method: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {MASTER}"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = router(app.clone()).oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn assignments_are_balanced_persisted_and_stable_on_import_and_delete() {
    let a = proxy(200).await;
    let b = proxy(200).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &[&a, &b]);
    let app = AppState::build(cfg.clone()).unwrap();
    for i in 0..11 {
        app.pool
            .add(&format!("tvly-test-key-{i}"), "test", 100)
            .unwrap();
    }
    let before: Vec<_> = app
        .pool
        .list()
        .iter()
        .map(|k| (k.id, k.egress_id()))
        .collect();
    assert_eq!(
        before
            .iter()
            .filter(|(_, id)| id.as_deref() == Some("exit-0"))
            .count(),
        6
    );
    app.persister.save_now(&app.pool).await.unwrap();
    drop(app);
    let app = AppState::build(cfg).unwrap();
    assert_eq!(
        before,
        app.pool
            .list()
            .iter()
            .map(|k| (k.id, k.egress_id()))
            .collect::<Vec<_>>()
    );
    app.pool.remove(1).unwrap();
    let new = app.pool.add("tvly-new-key", "new", 100).unwrap();
    assert_eq!(new.egress_id().as_deref(), Some("exit-0"));
    for (id, exit) in before.into_iter().skip(1) {
        assert_eq!(app.pool.get(id).unwrap().egress_id(), exit);
    }
}

#[tokio::test]
async fn rest_search_extract_and_usage_sync_follow_the_same_key_exit() {
    let a = proxy(200).await;
    let b = proxy(200).await;
    let dir = tempfile::tempdir().unwrap();
    let app = AppState::build(config(dir.path(), &[&a, &b])).unwrap();
    let first = app.pool.add("tvly-first-key", "first", 100).unwrap();
    let second = app.pool.add("tvly-second-key", "second", 100).unwrap();
    app.pool
        .update(
            second.id,
            KeyPatch {
                active: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(call(&app, "/search", "POST").await.0, StatusCode::OK);
    assert_eq!(call(&app, "/extract", "POST").await.0, StatusCode::OK);
    tavily_lb::usage::sync_key(&app, &first).await.unwrap();
    tavily_lb::usage::sync_key(&app, &second).await.unwrap();
    assert_eq!(
        a.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>(),
        ["/search", "/extract", "/usage"]
    );
    assert!(
        a.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, auth)| auth == "Bearer tvly-first-key")
    );
    assert_eq!(
        b.calls.lock().unwrap().as_slice(),
        [("/usage".into(), "Bearer tvly-second-key".into())]
    );
}

#[tokio::test]
async fn failed_exit_never_falls_back_to_direct_connection() {
    let dead = proxy(200).await;
    let direct = proxy(200).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &[&dead]);
    cfg.upstream_base_url = direct.url.clone();
    let app = AppState::build(cfg).unwrap();
    app.pool.add("tvly-test-key", "test", 100).unwrap();
    dead.task.abort();
    tokio::task::yield_now().await;
    assert_eq!(
        call(&app, "/search", "POST").await.0,
        StatusCode::BAD_GATEWAY
    );
    assert!(direct.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn usage_manual_rest_and_normalized_paths_share_one_gate() {
    let p = proxy(200).await;
    let dir = tempfile::tempdir().unwrap();
    let app = AppState::build(config(dir.path(), &[&p])).unwrap();
    let key = app.pool.add("tvly-test-key", "test", 100).unwrap();
    assert_eq!(call(&app, "/x/../usage", "GET").await.0, StatusCode::OK);
    for path in ["/usage", "/usage/", "/%75sage", "/x%2f..%2fusage"] {
        let (status, headers, _) = call(&app, path, "GET").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{path}");
        assert!(
            headers["retry-after"]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0
        );
    }
    assert_eq!(
        call(&app, &format!("/api/keys/{}/sync", key.id), "POST")
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(p.calls.lock().unwrap().len(), 1);
    assert_eq!(call(&app, "/search", "POST").await.0, StatusCode::OK);
}

#[tokio::test]
async fn usage_401_does_not_try_another_key_and_invalidates_only_rejected_key() {
    let p = proxy(401).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &[&p]);
    cfg.max_attempts = 5;
    let app = AppState::build(cfg).unwrap();
    app.pool.add("tvly-bad-key", "bad", 200).unwrap();
    app.pool.add("tvly-other-key", "other", 100).unwrap();
    assert_eq!(
        call(&app, "/usage", "GET").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(p.calls.lock().unwrap().len(), 1);
    assert_eq!(app.pool.list().iter().filter(|k| k.is_invalid()).count(), 1);
}

#[test]
fn gate_limits_key_and_ip_independently_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let gate = UsageGate::open(&path).unwrap();
    assert!(UsageGate::open(&path).is_err());
    drop(gate.reserve("key-a", "ip-a").unwrap());
    assert!(matches!(
        gate.reserve("key-b", "ip-a"),
        Err(GateError::Limited(_))
    ));
    assert!(matches!(
        gate.reserve("key-a", "ip-b"),
        Err(GateError::Limited(_))
    ));
    drop(gate.reserve("key-b", "ip-b").unwrap());
    drop(gate);
    let gate = UsageGate::open(&path).unwrap();
    assert!(matches!(
        gate.reserve("key-a", "ip-a"),
        Err(GateError::Limited(_))
    ));
    let contents = std::fs::read_to_string(path.with_extension("usage.json")).unwrap();
    assert!(!contents.contains("key-a"));
    drop(gate);
    std::fs::write(path.with_extension("usage.json"), "corrupt").unwrap();
    assert!(UsageGate::open(&path).is_err());
}

#[test]
fn concurrent_reservations_admit_only_one_request() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(UsageGate::open(&dir.path().join("state.json")).unwrap());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let gate = gate.clone();
            std::thread::spawn(move || gate.reserve("key", "ip").is_ok())
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().ok())
            .filter(|ok| *ok)
            .count(),
        1
    );
}

#[test]
fn restart_keeps_a_conservative_minute_and_storage_failure_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    std::fs::write(path.with_extension("usage.json"), r#"{"ip:exit":1}"#).unwrap();
    let gate = UsageGate::open(&path).unwrap();
    assert!(matches!(
        gate.reserve("key", "exit"),
        Err(GateError::Limited(_))
    ));
    // Prevent atomic replacement for the next reservation.
    let temp = dir.path().join("state.usage.json.tmp");
    std::fs::create_dir(&temp).unwrap();
    assert!(matches!(
        gate.reserve("other-key", "other-exit"),
        Err(GateError::Storage(_))
    ));
}

#[tokio::test]
async fn duplicate_ips_missing_persisted_exit_and_short_sync_intervals_are_rejected() {
    let a = proxy(200).await;
    let b = proxy(200).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &[&a, &b]);
    let mut bad = cfg.clone();
    bad.egress.nodes[1].exit_ip = bad.egress.nodes[0].exit_ip;
    assert!(bad.validate().is_err());
    bad = cfg.clone();
    bad.usage_sync.request_interval = Duration::from_secs(59);
    assert!(bad.validate().is_err());
    let app = AppState::build(cfg.clone()).unwrap();
    app.pool.add("tvly-test-key", "test", 100).unwrap();
    app.persister.save_now(&app.pool).await.unwrap();
    drop(app);
    let mut missing = cfg;
    missing.egress.nodes.clear();
    assert!(AppState::build(missing).is_err());
}

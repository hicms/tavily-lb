#[path = "support/exa_socks.rs"]
mod socks;

use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use tavily_lb::egress::EgressNode;
use tavily_lb::{AppState, Config, router};
use tower::ServiceExt;

const MASTER: &str = "health-test-master";

struct Call {
    method: String,
    path: String,
    authorization: Option<String>,
}

struct Proxy {
    url: String,
    status: Arc<AtomicU16>,
    probe_status: Arc<AtomicU16>,
    delay_ms: Arc<AtomicU64>,
    probe_delay_ms: Arc<AtomicU64>,
    calls: Arc<Mutex<Vec<Call>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn proxy(status: u16) -> Proxy {
    let status = Arc::new(AtomicU16::new(status));
    let probe_status = Arc::new(AtomicU16::new(0));
    let delay_ms = Arc::new(AtomicU64::new(0));
    let probe_delay_ms = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let response_status = status.clone();
    let head_status = probe_status.clone();
    let response_delay = delay_ms.clone();
    let head_delay = probe_delay_ms.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let head = req.method() == "HEAD";
        seen.lock().unwrap().push(Call {
            method: req.method().to_string(),
            path: req.uri().path().to_string(),
            authorization: req
                .headers()
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_string()),
        });
        let status = if head && head_status.load(Ordering::SeqCst) != 0 {
            head_status.load(Ordering::SeqCst)
        } else {
            response_status.load(Ordering::SeqCst)
        };
        let delay = if head { &head_delay } else { &response_delay }.load(Ordering::SeqCst);
        async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Response::builder()
                .status(status)
                .body(Body::from(r#"{"key":{"usage":7,"limit":100}}"#))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Proxy {
        url,
        status,
        probe_status,
        delay_ms,
        probe_delay_ms,
        calls,
        task,
    }
}

fn config(dir: &std::path::Path, proxies: &[&Proxy]) -> Config {
    let mut cfg = Config {
        data_file: dir.join("state.json"),
        master_key: Some(MASTER.into()),
        upstream_base_url: "http://upstream.invalid".into(),
        upstream_timeout: Duration::from_millis(100),
        request_deadline: Duration::from_secs(2),
        max_attempts: 2,
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
    cfg
}

async fn call(app: &AppState) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri("/search")
        .header("authorization", format!("Bearer {MASTER}"))
        .body(Body::from("{}"))
        .unwrap();
    router(app.clone()).oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn proxy_failure_skips_other_keys_on_that_node_and_preserves_accounts() {
    let bad = proxy(502).await;
    let good = proxy(200).await;
    let dir = tempfile::tempdir().unwrap();
    let app = AppState::build(config(dir.path(), &[&bad, &good])).unwrap();
    let first = app.pool.add("tvly-first", "first", 1000).unwrap();
    let healthy = app.pool.add("tvly-healthy", "healthy", 100).unwrap();
    let same_exit = app.pool.add("tvly-same-exit", "same", 900).unwrap();
    assert_eq!(first.egress_id(), same_exit.egress_id());
    assert_ne!(first.egress_id(), healthy.egress_id());
    assert_eq!(call(&app).await, StatusCode::OK);
    assert_eq!(call(&app).await, StatusCode::OK);
    assert_eq!(bad.calls.lock().unwrap().len(), 1);
    assert_eq!(good.calls.lock().unwrap().len(), 2);
    for key in [&first, &same_exit] {
        assert!(key.is_active());
        assert!(!key.is_invalid());
        assert_eq!(key.used_quota(), 0);
    }
}

#[tokio::test]
async fn usage_failure_isolates_node_before_next_request_selection() {
    let bad = proxy(503).await;
    let good = proxy(200).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &[&bad, &good]);
    cfg.max_attempts = 1;
    let app = AppState::build(cfg).unwrap();
    let first = app.pool.add("tvly-first", "first", 1000).unwrap();
    app.pool.add("tvly-good", "good", 100).unwrap();
    assert!(tavily_lb::usage::sync_key(&app, &first).await.is_err());
    assert_eq!(call(&app).await, StatusCode::OK);
    assert!(first.is_active());
    assert!(!first.is_invalid());
    assert_eq!(first.used_quota(), 0);
    assert_eq!(bad.calls.lock().unwrap().len(), 1);
}

struct Jobs(Vec<tokio::task::JoinHandle<()>>);

impl Drop for Jobs {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

async fn wait_for(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition did not become true");
}

async fn wait_recovered(app: &AppState) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while call(app).await != StatusCode::OK {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("node did not recover");
}

#[tokio::test]
async fn background_probe_recovers_socks_exit_without_credentials_or_usage_reservation() {
    let p = proxy(502).await;
    let socks = socks::Socks::start(&p.url).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &[&p]);
    cfg.egress.nodes[0].proxy_url = socks.url.clone();
    cfg.egress.health_check_interval = Duration::from_millis(30);
    cfg.egress.health_check_timeout = Duration::from_millis(100);
    // The health check must strip path, query and URL credentials as well as key headers.
    cfg.upstream_base_url = "http://probe-user:secret@upstream.invalid/api?token=secret".into();
    let app = AppState::build(cfg).unwrap();
    let key = app.pool.add("tvly-first", "first", 100).unwrap();
    let binding = key.egress_id();
    assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY);
    assert!(matches!(
        tavily_lb::usage::sync_key(&app, &key).await,
        Err(tavily_lb::usage::UsageError::Egress(_))
    ));
    let jobs = Jobs(tavily_lb::jobs::spawn_all(&app));
    wait_for(|| p.calls.lock().unwrap().iter().any(|c| c.method == "HEAD")).await;
    assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY);
    assert_eq!(
        p.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.method == "POST")
            .count(),
        1
    );
    p.status.store(200, Ordering::SeqCst);
    p.probe_status.store(401, Ordering::SeqCst);
    wait_recovered(&app).await;
    assert_eq!(key.egress_id(), binding);
    assert!(key.is_active() && !key.is_invalid());
    for call in p
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.method == "HEAD")
    {
        assert_eq!(call.path, "/");
        assert!(call.authorization.is_none());
    }
    assert!(
        socks
            .destinations
            .lock()
            .unwrap()
            .iter()
            .all(|(host, port)| host == "upstream.invalid" && *port == 80)
    );
    // An offline usage attempt must not reserve the limit: sync works immediately after recovery.
    tavily_lb::usage::sync_key(&app, &key).await.unwrap();
    assert_eq!(key.used_quota(), 7);
    drop(jobs);
}

#[tokio::test]
async fn proxy_statuses_and_timeouts_quarantine_without_invalidating_keys() {
    for status in [407, 502, 503, 504, 200] {
        let p = proxy(status).await;
        if status == 200 {
            p.delay_ms.store(300, Ordering::SeqCst);
        }
        let dir = tempfile::tempdir().unwrap();
        let app = AppState::build(config(dir.path(), &[&p])).unwrap();
        let key = app.pool.add("tvly-test", "test", 100).unwrap();
        assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY, "{status}");
        assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY, "{status}");
        assert_eq!(p.calls.lock().unwrap().len(), 1, "{status}");
        assert!(key.is_active() && !key.is_invalid());
        assert_eq!(key.used_quota(), 0);
    }
}

#[tokio::test]
async fn account_errors_do_not_quarantine_the_exit() {
    for status in [401, 429, 432, 433, 400, 403, 500] {
        let p = proxy(status).await;
        let dir = tempfile::tempdir().unwrap();
        let app = AppState::build(config(dir.path(), &[&p])).unwrap();
        let key = app.pool.add("tvly-first", "first", 1000).unwrap();
        call(&app).await;
        assert_eq!(key.is_invalid(), status == 401);
        p.status.store(200, Ordering::SeqCst);
        app.pool.add("tvly-second", "second", 2000).unwrap();
        assert_eq!(call(&app).await, StatusCode::OK, "{status}");
        assert_eq!(p.calls.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn stale_probe_cannot_clear_a_new_failure() {
    let p = proxy(502).await;
    p.probe_status.store(200, Ordering::SeqCst);
    p.probe_delay_ms.store(150, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &[&p]);
    cfg.egress.health_check_interval = Duration::from_secs(60);
    let app = AppState::build(cfg).unwrap();
    app.pool.add("tvly-first", "first", 100).unwrap();
    let _jobs = Jobs(tavily_lb::jobs::spawn_all(&app));
    wait_for(|| !p.calls.lock().unwrap().is_empty()).await;
    assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY);
    tokio::time::sleep(Duration::from_millis(220)).await;
    p.status.store(200, Ordering::SeqCst);
    assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY);
    assert_eq!(
        p.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.method == "POST")
            .count(),
        1
    );
}

#[tokio::test]
async fn in_flight_success_does_not_restore_a_quarantined_node() {
    let p = proxy(200).await;
    p.delay_ms.store(150, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &[&p]);
    cfg.upstream_timeout = Duration::from_secs(1);
    let app = AppState::build(cfg).unwrap();
    app.pool.add("tvly-first", "first", 100).unwrap();
    let other = app.clone();
    let slow_success = tokio::spawn(async move { call(&other).await });
    wait_for(|| !p.calls.lock().unwrap().is_empty()).await;
    p.status.store(502, Ordering::SeqCst);
    p.delay_ms.store(0, Ordering::SeqCst);
    assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY);
    assert_eq!(slow_success.await.unwrap(), StatusCode::OK);
    p.status.store(200, Ordering::SeqCst);
    assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY);
    assert_eq!(p.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn old_request_failure_cannot_quarantine_a_recovered_epoch() {
    let p = proxy(502).await;
    p.delay_ms.store(250, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &[&p]);
    cfg.upstream_timeout = Duration::from_secs(1);
    cfg.egress.health_check_interval = Duration::from_secs(60);
    let app = AppState::build(cfg).unwrap();
    app.pool.add("tvly-first", "first", 100).unwrap();
    let other = app.clone();
    let old_failure = tokio::spawn(async move { call(&other).await });
    wait_for(|| !p.calls.lock().unwrap().is_empty()).await;
    p.delay_ms.store(0, Ordering::SeqCst);
    assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY);
    p.status.store(200, Ordering::SeqCst);
    let _jobs = Jobs(tavily_lb::jobs::spawn_all(&app));
    wait_recovered(&app).await;
    assert_eq!(old_failure.await.unwrap(), StatusCode::BAD_GATEWAY);
    assert_eq!(call(&app).await, StatusCode::OK);
}

#[tokio::test]
async fn truncated_response_body_is_a_proxy_failure_even_with_401_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4096];
        let count = stream.read(&mut buf).await.unwrap();
        assert!(count > 0);
        stream
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 100\r\n\r\nshort")
            .await
            .unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &[]);
    cfg.egress.nodes.push(EgressNode {
        id: "broken-body".into(),
        exit_ip: "203.0.113.1".parse().unwrap(),
        proxy_url: url,
        hysteria_config: None,
    });
    let app = AppState::build(cfg).unwrap();
    let key = app.pool.add("tvly-test", "test", 100).unwrap();
    assert_eq!(call(&app).await, StatusCode::BAD_GATEWAY);
    server.await.unwrap();
    assert!(matches!(
        tavily_lb::usage::sync_key(&app, &key).await,
        Err(tavily_lb::usage::UsageError::Egress(
            tavily_lb::egress::EgressError::Unavailable(_)
        ))
    ));
    assert!(key.is_active() && !key.is_invalid());
}

use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tavily_lb::egress::EgressNode;
use tavily_lb::{AppState, Config, jobs, router};
use tower::ServiceExt;

async fn call(app: &AppState, path: &str) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", "Bearer lifecycle-test-master")
        .body(Body::from("{}"))
        .unwrap();
    router(app.clone()).oneshot(request).await.unwrap().status()
}

async fn wait_for(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child lifecycle condition timed out");
}

fn count(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

struct Jobs(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Jobs {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}
impl Jobs {
    async fn stop(mut self) {
        for task in self.0.drain(..) {
            task.abort();
            let _ = task.await;
        }
    }
}

#[tokio::test]
async fn exited_child_restarts_and_cancelled_restart_eventually_reaps_child() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir
        .path()
        .join(format!("hysteria-fixture{}", std::env::consts::EXE_SUFFIX));
    let output =
        std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "tests/support/hysteria_fixture.rs", "-o"])
            .arg(&binary)
            .output()
            .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client_config = dir.path().join("client.json");
    std::fs::write(
        &client_config,
        serde_json::json!({"http":{"listen":addr.to_string()}}).to_string(),
    )
    .unwrap();
    let mut cfg = Config {
        data_file: dir.path().join("state.json"),
        master_key: Some("lifecycle-test-master".into()),
        upstream_base_url: "http://upstream.invalid".into(),
        upstream_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    cfg.egress.hysteria_binary = Some(binary);
    cfg.egress.health_check_interval = Duration::from_millis(40);
    cfg.egress.health_check_timeout = Duration::from_millis(100);
    cfg.egress.nodes.push(EgressNode {
        id: "managed".into(),
        exit_ip: "203.0.113.1".parse().unwrap(),
        proxy_url: format!("http://{addr}"),
        hysteria_config: Some(client_config.clone()),
    });
    let app = AppState::build(cfg).unwrap();
    let key = app.pool.add("tvly-test", "test", 100).unwrap();
    assert_eq!(call(&app, "/crash").await, StatusCode::OK);
    wait_for(|| std::net::TcpStream::connect(addr).is_err()).await;
    assert_eq!(call(&app, "/search").await, StatusCode::BAD_GATEWAY);
    let jobs = Jobs(jobs::spawn_all(&app));
    wait_for(|| count(&client_config.with_extension("ready")) == 2).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while call(&app, "/search").await != StatusCode::OK {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(key.is_active() && !key.is_invalid());

    // Cancel while spawn_blocking is still in the client's startup wait.
    std::fs::write(client_config.with_extension("delay"), "300").unwrap();
    assert_eq!(call(&app, "/crash").await, StatusCode::OK);
    wait_for(|| count(&client_config.with_extension("starts")) == 3).await;
    jobs.stop().await;
    drop(app);
    wait_for(|| count(&client_config.with_extension("ready")) == 3).await;
    wait_for(|| std::net::TcpListener::bind(addr).is_ok()).await;
}

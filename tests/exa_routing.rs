#[path = "support/exa_socks.rs"]
mod socks;
#[path = "support/exa.rs"]
mod support;

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::json;
use support::*;
use tavily_lb::egress::EgressNode;
use tavily_lb::pool::KeyPatch;

#[tokio::test]
async fn exa_failover_keeps_original_invalid_cooldown_and_exhaustion_rules() {
    for failure in [401, 429, 432, 433] {
        let up = upstream(move |call| {
            if call.headers["authorization"] == "Bearer tvly-first" {
                (failure, json!({"detail":"failure"}))
            } else {
                (200, search_response())
            }
        })
        .await;
        let (app, _dir) = app_with(
            &up.url,
            &[("tvly-first", 200), ("tvly-second", 100)],
            |_| {},
        );
        let (status, _, _) = call(&app, post("/exa/search", json!({"query":"q"}))).await;
        assert_eq!(status, StatusCode::OK);
        let calls = up.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].headers["authorization"], "Bearer tvly-first");
        assert_eq!(calls[1].headers["authorization"], "Bearer tvly-second");
        let first = app
            .pool
            .list()
            .into_iter()
            .find(|k| k.key == "tvly-first")
            .unwrap();
        match failure {
            401 => assert!(first.is_invalid()),
            429 => {
                assert!(first.cooldown_until_ms() > tavily_lb::pool::now_ms());
                assert_eq!(first.remaining(), 200);
            }
            _ => assert_eq!(first.remaining(), 0),
        }
        assert_eq!(
            call(&app, post("/exa/search", json!({"query":"again"})))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(up.calls()[2].headers["authorization"], "Bearer tvly-second");
    }
}

#[tokio::test]
async fn remaining_quota_and_disabled_keys_still_determine_selection() {
    let up = upstream(|c| (200, extract_response(c))).await;
    let (app, _dir) = app_with(
        &up.url,
        &[
            ("tvly-high", 200),
            ("tvly-low", 100),
            ("tvly-disabled", 300),
        ],
        |_| {},
    );
    let disabled = app
        .pool
        .list()
        .into_iter()
        .find(|k| k.key == "tvly-disabled")
        .unwrap();
    app.pool
        .update(
            disabled.id,
            KeyPatch {
                active: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            call(
                &app,
                post("/exa/contents", json!({"urls":["https://example.com"]}))
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert!(
        up.calls()
            .iter()
            .all(|c| c.headers["authorization"] == "Bearer tvly-high")
    );
}

#[tokio::test]
async fn exa_respects_max_attempts_and_reports_empty_pool() {
    let up = upstream(|_| (401, json!({"detail":"invalid"}))).await;
    let (app, _dir) = app_with(
        &up.url,
        &[("tvly-first", 200), ("tvly-second", 100)],
        |cfg| cfg.max_attempts = 1,
    );
    let (status, _, body) = call(&app, post("/exa/search", json!({"query":"q"}))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["tag"], "SERVICE_UNAVAILABLE");
    assert_eq!(up.calls().len(), 1);
    assert!(
        !app.pool
            .list()
            .into_iter()
            .find(|k| k.key == "tvly-second")
            .unwrap()
            .is_invalid()
    );
    let (empty, _dir) = app_with(&up.url, &[], |_| {});
    assert_eq!(
        call(
            &empty,
            post("/exa/contents", json!({"urls":["https://example.com"]}))
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(up.calls().len(), 1);
}

#[tokio::test]
async fn exa_search_contents_and_tavily_share_key_bound_socks5_exits() {
    let bad = upstream(|_| (401, json!({"detail":"invalid"}))).await;
    let good = upstream(|c| {
        (
            200,
            if c.path == "/search" {
                search_response()
            } else {
                extract_response(c)
            },
        )
    })
    .await;
    let first_exit = socks::Socks::start(&bad.url).await;
    let second_exit = socks::Socks::start(&good.url).await;
    let (app, _dir) = app_with(
        "http://upstream.invalid",
        &[("tvly-first", 200), ("tvly-second", 100)],
        |cfg| {
            cfg.egress.nodes = [&first_exit, &second_exit]
                .iter()
                .enumerate()
                .map(|(i, proxy)| EgressNode {
                    id: format!("exit-{i}"),
                    exit_ip: format!("203.0.113.{}", i + 1).parse().unwrap(),
                    proxy_url: proxy.url.clone(),
                    hysteria_config: None,
                })
                .collect();
        },
    );
    let assignments: Vec<_> = app
        .pool
        .list()
        .iter()
        .map(|k| (k.id, k.egress_id()))
        .collect();
    assert_eq!(
        call(&app, post("/exa/search", json!({"query":"q"})))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            post("/exa/contents", json!({"urls":["https://example.com"]}))
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut legacy = post("/search", json!({"query":"legacy","max_results":2}));
    legacy.headers_mut().remove("x-api-key");
    legacy
        .headers_mut()
        .insert("authorization", format!("Bearer {MASTER}").parse().unwrap());
    assert_eq!(call(&app, legacy).await.0, StatusCode::OK);
    assert_eq!(bad.calls().len(), 1);
    assert_eq!(
        good.calls()
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        ["/search", "/extract", "/search"]
    );
    assert!(
        good.calls()
            .iter()
            .all(|c| c.headers["authorization"] == "Bearer tvly-second")
    );
    for proxy in [&first_exit, &second_exit] {
        let destinations = proxy.destinations.lock().unwrap();
        assert!(!destinations.is_empty());
        assert!(
            destinations
                .iter()
                .all(|d| d == &("upstream.invalid".into(), 80))
        );
    }
    assert_eq!(
        app.pool
            .list()
            .iter()
            .map(|k| (k.id, k.egress_id()))
            .collect::<Vec<_>>(),
        assignments
    );
}

#[tokio::test]
async fn broken_socks5_exit_never_falls_back_to_direct() {
    let direct = upstream(|_| (200, search_response())).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("socks5h://{}", port.local_addr().unwrap());
    drop(port);
    let (app, _dir) = app_with(&direct.url, &[("tvly-key", 100)], |cfg| {
        cfg.egress.nodes = vec![EgressNode {
            id: "dead-exit".into(),
            exit_ip: "203.0.113.1".parse().unwrap(),
            proxy_url,
            hysteria_config: None,
        }];
        cfg.upstream_timeout = Duration::from_millis(100);
    });
    for (path, body) in [
        ("/exa/search", json!({"query":"q"})),
        ("/exa/contents", json!({"urls":["https://example.com"]})),
    ] {
        let (status, _, _) = call(&app, post(path, body)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
    assert!(direct.calls().is_empty());
    assert!(!app.pool.list()[0].is_invalid());
}

#[tokio::test]
async fn contents_batches_share_one_request_deadline() {
    let up = upstream_delayed(|c| (200, extract_response(c)), Duration::from_millis(90)).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |cfg| {
        cfg.request_deadline = Duration::from_millis(150)
    });
    let started = Instant::now();
    let (status, _, body) = call(
        &app,
        post(
            "/exa/contents",
            json!({
                "urls":(0..100).map(|n| format!("https://example.com/{n}")).collect::<Vec<_>>()
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["tag"], "UPSTREAM_TIMEOUT");
    assert!(started.elapsed() < Duration::from_millis(600));
    assert_eq!(up.calls().len(), 2);
    assert_eq!(app.pool.list()[0].used_quota(), 1);
}

#[tokio::test]
async fn batch_transport_failure_is_not_reported_as_complete_success() {
    let up = upstream(|c| {
        if c.body["urls"][0] == "https://example.com/20" {
            (500, json!({"detail":"failed"}))
        } else {
            (200, extract_response(c))
        }
    })
    .await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let (status, _, body) = call(
        &app,
        post(
            "/exa/contents",
            json!({
                "urls":(0..41).map(|n| format!("https://example.com/{n}")).collect::<Vec<_>>()
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.get("results").is_none());
    assert_eq!(up.calls().len(), 2);
    assert_eq!(app.pool.list()[0].used_quota(), 1);
}

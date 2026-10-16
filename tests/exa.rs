#[path = "support/exa.rs"]
mod support;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use serde_json::{Value, json};
use support::*;

#[tokio::test]
async fn search_converts_request_and_returns_exa_shape_without_leaking_credentials() {
    let up = upstream(|_| (200, search_response())).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-test-key", 100)], |_| {});
    let (status, headers, body) = call(
        &app,
        post(
            "/exa/search",
            json!({
                "query":"rust", "numResults":3, "type":"auto",
                "includeDomains":["example.com"], "excludeDomains":["other.com"],
                "contents":{"text":{"maxCharacters":4},"highlights":true}
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let calls = up.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/search");
    assert_eq!(calls[0].body["max_results"], 3);
    assert_eq!(calls[0].body["include_domains"], json!(["example.com"]));
    assert_eq!(calls[0].body["include_raw_content"], "markdown");
    assert_eq!(calls[0].headers["authorization"], "Bearer tvly-test-key");
    assert!(!calls[0].headers.contains_key("x-api-key"));
    assert!(!calls[0].body.to_string().contains(MASTER));
    assert!(calls[0].body.get("contents").is_none());
    assert_eq!(body["results"][0]["text"], "完整😀正");
    assert_eq!(body["results"][0]["id"], "https://example.com/a");
    assert_eq!(
        body["results"][0]["highlights"],
        json!(["Relevant snippet"])
    );
    assert!(body["results"][0].get("score").is_none());
    assert!(body.get("usage").is_none());
    assert!(body.get("costDollars").is_none());
    assert_eq!(body["requestId"], headers["x-request-id"].to_str().unwrap());
    assert!(!headers.contains_key("content-encoding"));
}

#[tokio::test]
async fn metadata_only_search_and_empty_results_are_valid() {
    let up = upstream(|c| {
        (
            200,
            if c.body["query"] == "empty" {
                json!({"results":[]})
            } else {
                search_response()
            },
        )
    })
    .await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let (status, _, body) = call(&app, post("/exa/search", json!({"query":"metadata"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["results"][0].get("text").is_none());
    assert!(body["results"][0].get("highlights").is_none());
    assert_eq!(
        body["results"][0]["publishedDate"],
        "2025-03-11T17:00:00+00:00"
    );
    let (status, _, body) = call(&app, post("/exa/search", json!({"query":"empty"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"], json!([]));
    assert_eq!(up.calls()[0].body["max_results"], 10);
    assert_eq!(up.calls()[0].body["include_raw_content"], false);
}

#[tokio::test]
async fn search_modes_category_and_moderation_are_mapped() {
    let up = upstream(|_| (200, json!({"results":[]}))).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    for (mode, depth) in [
        ("auto", "basic"),
        ("fast", "fast"),
        ("instant", "ultra-fast"),
    ] {
        let (status, _, _) = call(
            &app,
            post(
                "/exa/search",
                json!({
                    "query":"q","type":mode,"category":"news","moderation":true
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let calls = up.calls();
        let converted = &calls.last().unwrap().body;
        assert_eq!(converted["search_depth"], depth);
        assert_eq!(converted["topic"], "news");
        assert_eq!(converted["safe_search"], true);
    }
}

#[tokio::test]
async fn unsupported_and_invalid_parameters_never_reach_upstream() {
    let up = upstream(|_| (200, search_response())).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let searches = [
        json!({}),
        json!({"query":" "}),
        json!({"query":123}),
        json!({"query":"q","type":"deep"}),
        json!({"query":"q","numResults":21}),
        json!({"query":"q","numResults":0}),
        json!({"query":"q","category":"company"}),
        json!({"query":"q","includeDomains":["example.com/docs"]}),
        json!({"query":"q","includeDomains":["*.example.com"]}),
        json!({"query":"q","startPublishedDate":"2026-01-01T00:00:00Z"}),
        json!({"query":"q","outputSchema":{"type":"object"}}),
        json!({"query":"q","contents":{"summary":{}}}),
        json!({"query":"q","contents":{"text":{"includeHtmlTags":true}}}),
        json!({"query":"q","contents":{"text":{"maxCharacters":0}}}),
        json!({"query":"q","contents":{"highlights":{"maxCharacters":1000001}}}),
        json!({"query":"q","contents":{"highlights":{"query":" "}}}),
        json!({"query":"q","contents":{"maxAgeHours":0}}),
        json!({"query":"q","unknown":false}),
    ];
    for request in searches {
        let (status, headers, body) = call(&app, post("/exa/search", request.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{request}: {body}");
        assert_error(&headers, &body);
    }
    for request in [
        json!({}),
        json!({"urls":[]}),
        json!({"urls":["http://a"],"ids":["http://b"]}),
        json!({"ids":["opaque-exa-id"]}),
        json!({"urls":["file:///secret"]}),
        json!({"urls":["https://user:secret@example.com"]}),
        json!({"urls":["https://example.com"],"livecrawl":"always"}),
        json!({"urls":vec!["https://example.com";101]}),
    ] {
        let (status, _, body) = call(&app, post("/exa/contents", request.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{request}: {body}");
    }
    assert!(up.calls().is_empty());
}

fn assert_error(headers: &axum::http::HeaderMap, body: &Value) {
    let object = body.as_object().unwrap();
    assert_eq!(object.len(), 3, "{body}");
    for field in ["requestId", "error", "tag"] {
        assert!(
            body[field].as_str().is_some_and(|s| !s.is_empty()),
            "{body}"
        );
    }
    assert_eq!(body["requestId"], headers["x-request-id"].to_str().unwrap());
}

#[tokio::test]
async fn bearer_auth_is_supported_and_conflicting_keys_are_rejected() {
    let up = upstream(|_| (200, json!({"results":[]}))).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let mut request = post("/exa/search", json!({"query":"q"}));
    request.headers_mut().remove("x-api-key");
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {MASTER}")).unwrap(),
    );
    assert_eq!(call(&app, request).await.0, StatusCode::OK);
    for (key, auth) in [
        (None, None),
        (Some("wrong"), None),
        (Some(MASTER), Some("Bearer wrong")),
        (Some(MASTER), Some("Basic bad")),
    ] {
        let mut request = post("/exa/search", json!({"query":"q"}));
        request.headers_mut().remove("x-api-key");
        if let Some(key) = key {
            request
                .headers_mut()
                .insert("x-api-key", HeaderValue::from_str(key).unwrap());
        }
        if let Some(auth) = auth {
            request
                .headers_mut()
                .insert("authorization", HeaderValue::from_str(auth).unwrap());
        }
        let (status, headers, body) = call(&app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_error(&headers, &body);
    }
    assert_eq!(up.calls().len(), 1);
}

#[tokio::test]
async fn transport_validation_uses_exa_error_envelope() {
    let up = upstream(|_| (200, json!({"results":[]}))).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |cfg| {
        cfg.max_body_bytes = 100
    });
    let mut requests = Vec::new();
    let mut get = post("/exa/search", json!({}));
    *get.method_mut() = axum::http::Method::GET;
    requests.push((get, StatusCode::METHOD_NOT_ALLOWED));
    requests.push((post("/exa/other", json!({})), StatusCode::NOT_FOUND));
    requests.push((
        post("/exa/search?a=b", json!({"query":"q"})),
        StatusCode::BAD_REQUEST,
    ));
    let malformed = Request::builder()
        .method("POST")
        .uri("/exa/search")
        .header("x-api-key", MASTER)
        .header("content-type", "application/json")
        .body(Body::from("{broken"))
        .unwrap();
    requests.push((malformed, StatusCode::BAD_REQUEST));
    let mut encoded = post("/exa/search", json!({"query":"q"}));
    encoded
        .headers_mut()
        .insert("content-encoding", HeaderValue::from_static("gzip"));
    requests.push((encoded, StatusCode::UNSUPPORTED_MEDIA_TYPE));
    let mut plain = post("/exa/search", json!({"query":"q"}));
    plain
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("text/plain"));
    requests.push((plain, StatusCode::UNSUPPORTED_MEDIA_TYPE));
    requests.push((
        post("/exa/search", json!({"query":"x".repeat(101)})),
        StatusCode::PAYLOAD_TOO_LARGE,
    ));
    for (request, expected) in requests {
        let (status, headers, body) = call(&app, request).await;
        assert_eq!(status, expected, "{body}");
        assert_error(&headers, &body);
        if status == StatusCode::METHOD_NOT_ALLOWED {
            assert_eq!(headers["allow"], "POST");
        }
    }
    assert!(up.calls().is_empty());
}

#[tokio::test]
async fn ambiguous_or_non_text_auth_headers_are_rejected() {
    let up = upstream(|_| (200, json!({"results":[]}))).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    for invalid in [
        HeaderValue::from_static("wrong"),
        HeaderValue::from_bytes(&[0xff]).unwrap(),
    ] {
        let mut request = post("/exa/search", json!({"query":"q"}));
        request.headers_mut().append("x-api-key", invalid);
        assert_eq!(call(&app, request).await.0, StatusCode::UNAUTHORIZED);
    }
    let mut request = post("/exa/search", json!({"query":"q"}));
    request
        .headers_mut()
        .insert("x-api-key", HeaderValue::from_bytes(&[0xff]).unwrap());
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {MASTER}").parse().unwrap());
    assert_eq!(call(&app, request).await.0, StatusCode::UNAUTHORIZED);
    assert!(up.calls().is_empty());
}

#[tokio::test]
async fn contents_preserves_per_url_status_ids_order_and_text_default() {
    let up = upstream(|_| {
        (
            200,
            json!({"results":[
        {"url":"https://example.com/b","raw_content":"B"},
        {"url":"https://example.com/a","title":"A","raw_content":"A正文😀"}
    ],"failed_results":[{"url":"https://example.com/fail","error":"no page"}]}),
        )
    })
    .await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let urls = json!([
        "https://example.com/a",
        "https://example.com/fail",
        "https://example.com/b"
    ]);
    let (status, _, body) = call(&app, post("/exa/contents", json!({"ids":urls}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(up.calls()[0].path, "/extract");
    assert_eq!(up.calls()[0].body["urls"], urls);
    assert_eq!(body["results"][0]["title"], "A");
    assert_eq!(body["results"][0]["text"], "A正文😀");
    assert_eq!(body["results"][1]["title"], "https://example.com/b");
    assert_eq!(
        body["statuses"],
        json!([
            {"id":"https://example.com/a","status":"success"},
            {"id":"https://example.com/fail","status":"error","error":{"tag":"CRAWL_ERROR"}},
            {"id":"https://example.com/b","status":"success"}
        ])
    );
    assert!(body.get("failed_results").is_none());
}

#[tokio::test]
async fn contents_batches_100_urls_and_preserves_duplicates() {
    let up = upstream(|c| (200, extract_response(c))).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let urls: Vec<_> = (0..100)
        .map(|i| format!("https://example.com/{}", i % 25))
        .collect();
    let (status, _, body) = call(&app, post("/exa/contents", json!({"urls":urls}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(up.calls().len(), 5);
    assert!(
        up.calls()
            .iter()
            .all(|c| c.body["urls"].as_array().unwrap().len() == 20)
    );
    assert_eq!(body["results"].as_array().unwrap().len(), 100);
    assert_eq!(
        body["statuses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        urls
    );
    assert_eq!(app.pool.list()[0].used_quota(), 5);
}

#[tokio::test]
async fn contents_full_text_and_query_highlights_use_separate_extractions() {
    let up = upstream(|c| (200, extract_response(c))).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let (status, _, body) = call(
        &app,
        post(
            "/exa/contents",
            json!({
                "urls":["https://example.com"],"text":true,
                "highlights":{"query":"conclusions","maxCharacters":8}
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][0]["text"], "Full page body");
    assert_eq!(body["results"][0]["highlights"], json!(["Relevant"]));
    let calls = up.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].body.get("query").is_none());
    assert_eq!(calls[1].body["query"], "conclusions");
    assert_eq!(calls[1].body["chunks_per_source"], 3);
}

#[tokio::test]
async fn content_controls_distinguish_false_null_and_omitted() {
    let up = upstream(|c| (200, extract_response(c))).await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    for (options, text, highlights) in [
        (json!({}), true, false),
        (json!({"text":null,"highlights":null}), true, false),
        (json!({"text":false,"highlights":false}), false, false),
        (json!({"text":false}), false, false),
        (json!({"highlights":true}), false, true),
        (json!({"text":{}}), true, false),
    ] {
        let mut request = options.clone();
        request["urls"] = json!(["https://example.com"]);
        let (status, _, body) = call(&app, post("/exa/contents", request)).await;
        assert_eq!(status, StatusCode::OK, "{options}: {body}");
        assert_eq!(body["results"][0].get("text").is_some(), text, "{options}");
        assert_eq!(
            body["results"][0].get("highlights").is_some(),
            highlights,
            "{options}"
        );
    }
}

#[tokio::test]
async fn custom_search_highlights_do_not_replace_full_text() {
    let up = upstream(|c| {
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
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let (status, _, body) = call(
        &app,
        post(
            "/exa/search",
            json!({
                "query":"rust","contents":{"text":true,"highlights":{"query":"memory safety"}}
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][0]["text"], "完整😀正文 ABC");
    assert_eq!(body["results"][0]["highlights"], json!(["Relevant chunk"]));
    assert_eq!(up.calls()[1].body["query"], "memory safety");
}

#[tokio::test]
async fn missing_raw_content_is_never_replaced_with_snippets() {
    let up = upstream(|_| {
        (
            200,
            json!({"results":[{
                "url":"https://example.com", "content":"only a snippet", "raw_content":null
            }]}),
        )
    })
    .await;
    let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
    let (status, _, body) = call(
        &app,
        post("/exa/search", json!({"query":"q","contents":{"text":true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["results"][0].get("text").is_none());
    let (status, _, body) = call(
        &app,
        post("/exa/contents", json!({"urls":["https://example.com"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"], json!([]));
    assert_eq!(body["statuses"][0]["status"], "error");
}

#[tokio::test]
async fn malformed_upstream_and_unexpected_urls_are_explicit_errors() {
    for response in [
        json!("not an object"),
        json!({}),
        json!({"results":[{"title":"missing url"}]}),
        json!({"results":[{"url":"not a URL"}]}),
        json!({"results":[{"url":"https://unexpected.test","raw_content":"x"}]}),
    ] {
        let up = upstream(move |_| (200, response.clone())).await;
        let (app, _dir) = app_with(&up.url, &[("tvly-key", 100)], |_| {});
        let (status, headers, body) = call(
            &app,
            post("/exa/contents", json!({"urls":["https://example.com"]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_error(&headers, &body);
    }
}

#[tokio::test]
async fn upstream_errors_are_rebuilt_and_do_not_echo_secrets() {
    for (up_status, expected) in [(400, 400), (422, 400), (429, 429), (500, 502), (302, 502)] {
        let up =
            upstream(move |_| (up_status, json!({"secret":MASTER,"detail":"tvly-test-key"}))).await;
        let (app, _dir) = app_with(&up.url, &[("tvly-test-key", 100)], |_| {});
        let (status, headers, body) = call(&app, post("/exa/search", json!({"query":"q"}))).await;
        assert_eq!(status.as_u16(), expected);
        assert_error(&headers, &body);
        assert!(!body.to_string().contains(MASTER));
        assert!(!body.to_string().contains("tvly-test-key"));
        if up_status == 429 {
            assert_eq!(headers["retry-after"], "7");
        }
    }
}

use super::*;
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use futures_util::StreamExt;
use std::{collections::VecDeque, sync::Mutex, time::Duration};

async fn setup(
    replies: Vec<(u16, bool, String)>,
    attempts: usize,
) -> (Arc<Runtime>, Arc<Live>, Server, Server) {
    let r = root();
    let live = add(&r, &control("a")).await;
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let upstream = serve(
        Router::new()
            .route(
                "/v1/models",
                any(|| async { Json(json!({"data":[{"id":"model-test"}]})) }),
            )
            .route(
                "/v1/chat/completions",
                post(move || {
                    let reply = replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected upstream retry");
                    async move {
                        (
                            StatusCode::from_u16(reply.0).unwrap(),
                            [(
                                header::CONTENT_TYPE,
                                if reply.1 {
                                    "text/event-stream"
                                } else {
                                    "application/json"
                                },
                            )],
                            reply.2,
                        )
                            .into_response()
                    }
                }),
            ),
    )
    .await;
    let mut config = live.runtime.engine().config.clone();
    let pool = &mut config.models[0];
    pool.max_attempts = attempts;
    pool.retry_delay_ms = 0;
    let d = &mut pool.deployments[0];
    d.api_base = Some(format!("{}/v1", upstream.url));
    d.proxy = Some(upstream.url.clone());
    d.enabled = true;
    let mut second = d.clone();
    second.id = "second-account".into();
    if attempts > 1 {
        pool.deployments.push(second);
    }
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(config).unwrap()),
                ..Update::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    let router = serve(crate::server::app_runtime(r.clone())).await;
    (r, live, upstream, router)
}

fn body(stream: bool) -> Value {
    json!({"model":format!("{}/model-test",crypto::identity(&"a".repeat(64))),"messages":[{"role":"user","content":"Hello PRIVATE_INPUT"}],"stream":stream})
}

fn bill(r: &Runtime) -> Value {
    r.engine().vault.lock().unwrap().events.back().unwrap()["event"]["billing"].clone()
}

#[tokio::test]
async fn normal_json_and_sse_without_usage_remain_unpriced() {
    let mut json_reply = completion();
    json_reply.as_object_mut().unwrap().remove("usage");
    let sse = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"normal output"},"finish_reason":"stop"}]})
    );
    for (streaming, reply) in [(false, json_reply.to_string()), (true, sse)] {
        let (r, _, _upstream, router) = setup(vec![(200, streaming, reply)], 1).await;
        let response = reqwest::Client::builder()
            .user_agent("codex_cli_rs/1.0 (merchant fixture)")
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/v1/chat/completions", router.url))
            .bearer_auth("alice-key")
            .json(&body(streaming))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(!text.contains("upstream_error"));
        let b = bill(&r);
        assert_eq!(b["state"], "pending", "{b}");
        let attempt = &b["attempt_records"][0];
        assert_eq!(attempt["status"], 200);
        assert!(attempt["billing"].get("estimate").is_none());
        assert!(b.get("charge_microusd").is_none());
        assert_eq!(
            crate::balances::consumption(&r.engine().vault.lock().unwrap().events)
                .get("alice")
                .copied()
                .unwrap_or(0),
            0
        );
    }
}

#[tokio::test]
async fn model_listing_never_creates_model_charges() {
    let (r, _, _upstream, router) = setup(vec![], 1).await;
    let before = r.engine().vault.lock().unwrap().events.len();
    assert_eq!(
        call(&router, Method::GET, "/v1/models", "alice-key", Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(r.engine().vault.lock().unwrap().events.len(), before);
}

#[tokio::test]
async fn full_usage_in_long_http_error_is_charged_once_with_merchant_totals() {
    let error = json!({"error":{"message":"x".repeat(10000)},"usage":{"prompt_tokens":10,"completion_tokens":5}});
    let (r, live, _upstream, router) = setup(vec![(400, false, error.to_string())], 1).await;
    assert_eq!(
        call(
            &router,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            body(false)
        )
        .await
        .0,
        400
    );
    let b = bill(&r);
    assert_eq!(b["version"], 2);
    assert_eq!(b["charge_microusd"], 20, "{b}");
    assert_eq!(b["usage_source"], "official");
    assert_eq!(b["attempt_records"][0]["billing"]["upstream_status"], 400);
    let merchant = live.runtime.engine();
    let ledger = merchant.vault.lock().unwrap();
    assert!(ledger.events.is_empty(), "platform owns the receipt");
    assert_eq!(ledger.usage["alice"].requests, 1);
    assert_eq!(ledger.usage["alice"].errors, 1);
    assert_eq!(ledger.usage["alice"].input_tokens, 10);
    assert_eq!(ledger.usage["alice"].output_tokens, 5);
    let data = r.engine().vault.lock().unwrap().clone();
    assert_eq!(crate::balances::consumption(&data.events)["alice"], 20);
}

#[tokio::test]
async fn rate_limit_retry_is_zero_and_success_usage_is_not_lost() {
    let (r, _, _upstream, router) = setup(
        vec![
            (
                429,
                false,
                json!({"error":{"type":"rate_limit"}}).to_string(),
            ),
            (200, false, completion().to_string()),
        ],
        2,
    )
    .await;
    assert_eq!(
        call(
            &router,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            body(false)
        )
        .await
        .0,
        200
    );
    let b = bill(&r);
    assert_eq!(b["attempt_records"].as_array().unwrap().len(), 2);
    assert_eq!(b["attempt_records"][0]["billing"]["charge_microusd"], 0);
    assert_eq!(b["charge_microusd"], 20, "{b}");
    assert_eq!(b["unknown_attempts"], 0);
}

#[tokio::test]
async fn interrupted_sse_without_final_usage_is_estimated_and_charged_once() {
    let text = format!(
        "data: {}\n\n",
        json!({"id":"r","choices":[{"index":0,"delta":{"content":"PRIVATE_REPLY hello world"}}]})
    );
    let (r, _, _upstream, router) = setup(vec![(200, true, text)], 1).await;
    let response = reqwest::Client::builder()
        .user_agent("codex_cli_rs/1.0 (merchant fixture)")
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/chat/completions", router.url))
        .bearer_auth("alice-key")
        .json(&body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let output = response.text().await.unwrap();
    assert!(output.contains("PRIVATE_REPLY"));
    let b = bill(&r);
    assert_eq!(b["usage_source"], "estimated", "{b}");
    assert!(b["charge_microusd"].as_u64().unwrap() > 0);
    assert_eq!(b["attempt_records"][0]["status"], 502);
    assert!(!b.to_string().contains("PRIVATE_"));
    assert_eq!(
        r.engine()
            .vault
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|row| row["event"]["event"] == "request")
            .count(),
        1
    );
}

#[tokio::test]
async fn dropping_downstream_after_official_usage_still_charges() {
    let r = root();
    let live = add(&r, &control("a")).await;
    let sse = format!(
        "data: {}\n\n",
        json!({"id":"r","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}})
    );
    let upstream = serve(
        Router::new()
            .route(
                "/v1/models",
                any(|| async { Json(json!({"data":[{"id":"model-test"}]})) }),
            )
            .route(
                "/v1/chat/completions",
                post(move || {
                    let first = sse.clone();
                    async move {
                        let stream =
                            futures_util::stream::once(
                                async move { Ok::<_, std::io::Error>(first) },
                            )
                            .chain(futures_util::stream::pending());
                        (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            Body::from_stream(stream),
                        )
                    }
                }),
            ),
    )
    .await;
    let mut config = live.runtime.engine().config.clone();
    let d = &mut config.models[0].deployments[0];
    d.api_base = Some(format!("{}/v1", upstream.url));
    d.proxy = Some(upstream.url.clone());
    d.enabled = true;
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(config).unwrap()),
                ..Update::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    let router = serve(crate::server::app_runtime(r.clone())).await;
    let mut request = body(true);
    request["stream_options"] = json!({"include_usage":true});
    let response = reqwest::Client::builder()
        .user_agent("codex_cli_rs/1.0 (merchant fixture)")
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/chat/completions", router.url))
        .bearer_auth("alice-key")
        .json(&request)
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    assert!(stream.next().await.unwrap().is_ok());
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if r.engine()
                .vault
                .lock()
                .unwrap()
                .events
                .back()
                .is_some_and(|e| e["event"]["event"] == "request")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let b = bill(&r);
    assert_eq!(b["charge_microusd"], 20, "{b}");
    assert_eq!(b["usage_source"], "official");
    assert_eq!(b["attempt_records"][0]["status"], 499);
}

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn router_measures_stream_before_filtering_usage_for_the_client() {
    use axum::{body::Body, response::Response};
    let r = root();
    let token = "a".repeat(64);
    let live = add(&r, &token).await;
    let mock=serve(Router::new().route("/v1/models",any(||async {Json(json!({"data":[{"id":"model-test"}]}))}))
        .fallback(any(||async {
            let stream=futures_util::stream::unfold(0,|i|async move {
                let value=match i {
                    0=>json!({"choices":[{"index":0,"delta":{"role":"assistant"}}]}),
                    1=>json!({"choices":[{"index":0,"delta":{"content":"first"}}]}),
                    2=>{tokio::time::sleep(std::time::Duration::from_millis(300)).await;json!({"choices":[{"index":0,"delta":{"content":"last"},"finish_reason":"stop"}]})},
                    3=>json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":11}}),
                    4=>return Some((Ok::<_,std::io::Error>("data: [DONE]\n\n".to_string()),5)),
                    _=>return None,
                };Some((Ok(format!("data: {value}\n\n")),i+1))
            });
            Response::builder().header("content-type","text/event-stream").body(Body::from_stream(stream)).unwrap()
        }))).await;
    let mut config = live.runtime.engine().config.clone();
    let d = &mut config.models[0].deployments[0];
    d.api_base = Some(format!("{}/v1", mock.url));
    d.proxy = Some(mock.url.clone());
    d.enabled = true;
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(config).unwrap()),
                ..Default::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let response = request(
        &server,
        "alice-key",
        rule(None, 1000000),
        json!({"model":"model-test","messages":[{"role":"user","content":"hello"}],"stream":true}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let pool = response.headers()["x-router-pool"]
        .to_str()
        .unwrap()
        .to_owned();
    let text = response.text().await.unwrap();
    assert!(text.contains("[DONE]"));
    assert!(!text.contains("completion_tokens"));
    let (first, tps) = r.merchants.routing.sample(&pool, "model-test").unwrap();
    assert!(first >= 0.0);
    assert!(tps.unwrap() > 0.0 && tps.unwrap() < 100.0);
}

fn rule(pool: Option<&str>, cap: u64) -> Value {
    let mut v = json!({"version":1,"mode":if pool.is_some(){"fixed"}else{"auto"},"priority":"price","max_multiplier_millionths":cap});
    if let Some(pool) = pool {
        v["pool"] = pool.into();
    }
    v
}
async fn request(server: &Server, token: &str, policy: Value, body: Value) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/chat/completions", server.url))
        .bearer_auth(token)
        .header("x-router-routing", policy.to_string())
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn observed_fixture(live: &Live) -> (Server, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let mock=serve(Router::new().route("/v1/models",any(||async {Json(json!({"data":[{"id":"model-test"}]}))}))
        .fallback(any(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
            let count=calls.clone(); async move {
                assert!(!headers.contains_key("x-router-routing"));
                assert!(!headers.contains_key("x-router-pool"));
                assert_eq!(body["model"],"model-test");
                assert!(body.get("routing").is_none());
                count.fetch_add(1,Ordering::SeqCst);
                Json(json!({"model":"model-test","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}))
            }
        }))).await;
    let mut config = live.runtime.engine().config.clone();
    let d = &mut config.models[0].deployments[0];
    d.api_base = Some(format!("{}/v1", mock.url));
    d.proxy = Some(mock.url.clone());
    d.enabled = true;
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(config).unwrap()),
                ..Default::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    (mock, count)
}
fn multiplier(live: &Live, rate: u64) {
    let mut settings = live.settings.lock().unwrap();
    settings.pool_pricing[0].multiplier_millionths = rate;
    settings.revision += 1;
}
#[tokio::test]
async fn router_selects_across_merchants_and_enforces_live_fixed_caps_without_proxy() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let first = add(&r, &a).await;
    let second = add(&r, &b).await;
    let (_ma, ca) = observed_fixture(&first).await;
    let (_mb, cb) = observed_fixture(&second).await;
    multiplier(&first, 200000);
    multiplier(&second, 500000);
    let pa = format!("{}.official", crypto::identity(&a));
    let pb = format!("{}.official", crypto::identity(&b));
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let body = json!({"model":"model-test","messages":[{"role":"user","content":"hello"}]});
    let response = request(&server, "alice-key", rule(None, 500000), body.clone()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-pool"], pa);
    assert_eq!(response.headers()["x-router-routing-version"], "1");
    assert_eq!(
        response.headers()["x-router-multiplier-millionths"],
        "200000"
    );
    response.bytes().await.unwrap();
    let response = request(&server, "alice-key", rule(Some(&pb), 500000), body.clone()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-pool"], pb);
    response.bytes().await.unwrap();
    multiplier(&second, 900000);
    let response = request(&server, "alice-key", rule(Some(&pb), 500000), body.clone()).await;
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "routing_multiplier_exceeded"
    );
    assert_eq!(ca.load(Ordering::SeqCst), 1);
    assert_eq!(cb.load(Ordering::SeqCst), 1);
    let response = request(&server, "alice-key", rule(None, 199999), body.clone()).await;
    assert_eq!(response.status(), 503);
    let response = request(&server, "bob-key", rule(None, 1000000), body).await;
    assert_eq!(response.status(), 503);
    assert_eq!(ca.load(Ordering::SeqCst), 1);
    assert_eq!(cb.load(Ordering::SeqCst), 1);
    // The settled usage carries exactly the price that was admitted.
    let data = r.engine().vault.lock().unwrap().clone();
    let event = serde_json::to_value(data.events.back().unwrap()).unwrap();
    assert_eq!(
        event["event"]["merchant_pricing"]["multiplier_millionths"],
        500000
    );
}
#[tokio::test]
async fn automatic_rules_respect_platform_permissions_and_unavailable_accounts() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let first = add(&r, &a).await;
    let second = add(&r, &b).await;
    let (_ma, ca) = observed_fixture(&first).await;
    let (_mb, cb) = observed_fixture(&second).await;
    multiplier(&first, 200000);
    multiplier(&second, 500000);
    let pb = format!("{}.official", crypto::identity(&b));
    let mut config = r.engine().config.clone();
    config.keys[0].models = vec![format!("{pb}/*")];
    r.apply(
        Update {
            config: Some(serde_json::to_value(config).unwrap()),
            ..Default::default()
        },
        ADMIN,
    )
    .await
    .unwrap();
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let body = json!({"model":"model-test","messages":[{"role":"user","content":"hello"}]});
    let response = request(&server, "alice-key", rule(None, 1000000), body.clone()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-pool"], pb);
    response.bytes().await.unwrap();
    assert_eq!(ca.load(Ordering::SeqCst), 0);
    assert_eq!(cb.load(Ordering::SeqCst), 1);
    let mut config = second.runtime.engine().config.clone();
    config.models[0].deployments[0].enabled = false;
    second
        .runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(config).unwrap()),
                ..Default::default()
            },
            &second.admin,
        )
        .await
        .unwrap();
    assert_eq!(
        request(&server, "alice-key", rule(None, 1000000), body)
            .await
            .status(),
        503
    );
    assert_eq!(ca.load(Ordering::SeqCst), 0);
    assert_eq!(cb.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn bad_rules_and_ambiguous_continuations_never_reach_a_provider() {
    let r = root();
    let token = "a".repeat(64);
    let live = add(&r, &token).await;
    let (_mock, count) = observed_fixture(&live).await;
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let body = json!({"model":"model-test","messages":[{"role":"user","content":"hello"}]});
    let good = rule(None, 1000000);
    for (field, value) in [
        ("version", json!(2)),
        ("mode", json!("unknown")),
        ("max_multiplier_millionths", json!(-1)),
        ("max_multiplier_millionths", json!(0.5)),
        ("extra", json!(true)),
        ("pool", json!("m_a.pool")),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        assert_eq!(
            request(&server, "alice-key", bad, body.clone())
                .await
                .status(),
            400
        );
    }
    for name in [
        "x-router-pool",
        "x-router-account",
        "x-router-price-version",
    ] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", server.url))
            .bearer_auth("alice-key")
            .header("x-router-routing", good.to_string())
            .header(name, "1")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    let mut continued = body.clone();
    continued["previous_response_id"] = json!("unknown");
    let response = request(&server, "alice-key", good.clone(), continued).await;
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "routing_affinity_required"
    );
    let mut duplicate = reqwest::header::HeaderMap::new();
    duplicate.append("x-router-routing", good.to_string().parse().unwrap());
    duplicate.append("x-router-routing", good.to_string().parse().unwrap());
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.url))
        .bearer_auth("alice-key")
        .headers(duplicate)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(
        live.runtime
            .engine()
            .vault
            .lock()
            .unwrap()
            .upstream_users
            .is_empty()
    );
}
#[tokio::test]
async fn session_bound_to_a_merchant_never_moves_to_a_cheaper_merchant() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let first = add(&r, &a).await;
    let second = add(&r, &b).await;
    let (_ma, ca) = observed_fixture(&first).await;
    let (_mb, cb) = observed_fixture(&second).await;
    multiplier(&first, 200000);
    multiplier(&second, 500000);
    let pb = format!("{}.official", crypto::identity(&b));
    let server = serve(crate::server::app_runtime(r.clone())).await;
    for policy in [rule(Some(&pb), 1000000), rule(None, 1000000)] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", server.url))
            .bearer_auth("alice-key")
            .header("x-router-routing", policy.to_string())
            .header("x-router-session", "my-session")
            .json(&json!({"model":"model-test","messages":[{"role":"user","content":"hello"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-router-pool"], pb);
        response.bytes().await.unwrap();
    }
    assert_eq!(ca.load(Ordering::SeqCst), 0);
    assert_eq!(cb.load(Ordering::SeqCst), 2);
}

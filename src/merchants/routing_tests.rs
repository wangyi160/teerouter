use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn latency_routing_uses_each_accounts_proxy_preserves_sessions_and_enforces_caps() {
    use crate::network::latency::Target;
    let r = root();
    let a = control("a");
    let b = control("b");
    let first = add(&r, &a).await;
    let second = add(&r, &b).await;
    let (ma, ca) = observed_fixture(&first).await;
    let (mb, cb) = observed_fixture(&second).await;
    let cc = Arc::new(AtomicUsize::new(0));
    let count = cc.clone();
    let mc = serve(Router::new().fallback(any(move |Json(body): Json<Value>| {
        let count = count.clone(); async move {
            assert_eq!(body["model"], "model-test");
            count.fetch_add(1, Ordering::SeqCst);
            Json(json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}))
        }
    }))).await;
    let mut config = first.runtime.engine().config.clone();
    let mut extra = config.models[0].deployments[0].clone();
    extra.id = "second-account".into();
    extra.api_base = Some(format!("{}/v1", mc.url));
    extra.proxy = Some(mc.url.clone());
    config.models[0].deployments.push(extra);
    first
        .runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(config).unwrap()),
                ..Default::default()
            },
            &first.admin,
        )
        .await
        .unwrap();
    multiplier(&first, 100000);
    multiplier(&second, 500000);
    let target = |server: &Server| {
        Target::new(
            crate::Provider::OpenAI,
            &server.url,
            &format!("{}/v1", server.url).parse().unwrap(),
        )
    };
    for (server, ms) in [(&ma, 800), (&mb, 200), (&mc, 50)] {
        r.merchants
            .latency
            .record(&target(server), Some(std::time::Duration::from_millis(ms)));
    }
    let pa = format!("{}.official", crypto::identity(&a));
    let pb = format!("{}.official", crypto::identity(&b));
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let mut policy = rule(None, 1000000);
    policy["priority"] = "speed".into();
    let mut body =
        json!({"model":"model-test","messages":[{"role":"user","content":"session-one"}]});
    let response = request(&server, "alice-key", policy.clone(), body.clone()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-pool"], pa);
    response.bytes().await.unwrap();
    assert_eq!(ca.load(Ordering::SeqCst), 0);
    assert_eq!(cc.load(Ordering::SeqCst), 1); // chooses second account, not pool's first
    r.merchants.latency.record(&target(&mb), None);
    r.merchants
        .latency
        .record(&target(&mb), Some(std::time::Duration::from_millis(5)));
    let response = request(&server, "alice-key", policy.clone(), body.clone()).await;
    assert_eq!(response.headers()["x-router-pool"], pa); // same conversation stays
    response.bytes().await.unwrap();
    assert_eq!(cc.load(Ordering::SeqCst), 2);
    body["messages"][0]["content"] = "new-conversation".into();
    let response = request(&server, "alice-key", policy.clone(), body.clone()).await;
    assert_eq!(response.headers()["x-router-pool"], pb);
    response.bytes().await.unwrap();
    assert_eq!(cb.load(Ordering::SeqCst), 1);
    policy["max_multiplier_millionths"] = 100000.into();
    let response = request(&server, "alice-key", policy.clone(), body.clone()).await;
    assert_eq!(response.headers()["x-router-pool"], pa); // absolute cap beats latency/stickiness
    response.bytes().await.unwrap();
    r.merchants.latency.record(&target(&mc), None);
    let response = request(&server, "alice-key", policy, body).await;
    assert_eq!(response.headers()["x-router-pool"], pa);
    response.bytes().await.unwrap();
    assert_eq!(ca.load(Ordering::SeqCst), 1); // failed sticky proxy gives way to healthy account
}

#[tokio::test]
async fn price_priority_compares_cached_total_cost_and_rechecks_multiplier_caps() {
    let r = root();
    r.engine().vault.lock().unwrap().official_pricing.providers[0].models[0]
        .rates
        .cache_read = Some(100000);
    let a = control("a");
    let b = control("b");
    let first = add(&r, &a).await;
    let second = add(&r, &b).await;
    let (_ma, ca) = observed_fixture(&first).await;
    let reply = json!({"model":"model-test","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":6000,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":5000}}});
    let mock = serve(Router::new().route("/v1/models",any(|| async {Json(json!({"data":[{"id":"model-test"}]}))}))
        .fallback(any(move |Json(body): Json<Value>| { let reply = reply.clone(); async move {
            if body["stream"] == true {
                use axum::response::IntoResponse;
                let first = json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]});
                let usage = json!({"choices":[],"usage":reply["usage"]});
                ([("content-type","text/event-stream")],format!("data: {first}\n\ndata: {usage}\n\ndata: [DONE]\n\n")).into_response()
            } else { use axum::response::IntoResponse; Json(reply).into_response() }
        }}))).await;
    let mut config = second.runtime.engine().config.clone();
    let d = &mut config.models[0].deployments[0];
    d.enabled = true;
    d.api_base = Some(format!("{}/v1", mock.url));
    d.proxy = Some(mock.url.clone());
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
    multiplier(&first, 800000);
    multiplier(&second, 1000000);
    let pa = format!("{}.official", crypto::identity(&a));
    let pb = format!("{}.official", crypto::identity(&b));
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let mut body = json!({"model":"model-test","messages":[{"role":"user","content":"x".repeat(24000)}],"max_tokens":1,"stream":true});
    let response = request(&server, "alice-key", rule(Some(&pb), 1000000), body.clone()).await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    body["stream"] = false.into();
    // A stream's final usage warmed the expensive pool; even the conservative
    // read estimate beats the lower multiplier of a cold account.
    let response = request(&server, "alice-key", rule(None, 1000000), body.clone()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-pool"], pb);
    response.bytes().await.unwrap();
    assert_eq!(ca.load(Ordering::SeqCst), 0);
    // The cap is absolute, regardless of cache savings or stickiness.
    let response = request(&server, "alice-key", rule(Some(&pb), 900000), body.clone()).await;
    assert_eq!(response.status(), 403);
    multiplier(&second, 1500000);
    let response = request(&server, "alice-key", rule(None, 2000000), body.clone()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-pool"], pa);
    response.bytes().await.unwrap();
    assert_eq!(ca.load(Ordering::SeqCst), 1);
    // A new prompt cannot borrow an unrelated conversation's cache discount.
    body["messages"][0]["content"] = "y".repeat(24000).into();
    multiplier(&second, 1000000);
    let response = request(&server, "alice-key", rule(None, 2000000), body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-pool"], pa);
    response.bytes().await.unwrap();
}

#[tokio::test]
async fn provider_model_ids_disambiguate_catalog_and_enforce_fixed_and_auto_routes() {
    let r = root();
    let a = control("a");
    let b = control("b");
    let first = add(&r, &a).await;
    let second = add(&r, &b).await;
    let mut config = second.runtime.engine().config.clone();
    config.models[0].provider = Some(crate::provider::Provider::XAI);
    config.models[0].deployments[0].provider = crate::provider::Provider::XAI;
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
    {
        let engine = r.engine();
        let mut data = engine.vault.lock().unwrap();
        let mut book = data.official_pricing.providers[0].clone();
        book.provider = crate::provider::Provider::XAI;
        book.source = "https://docs.x.ai/developers/models".into();
        data.official_pricing.providers.push(book);
    }
    let (_ma, ca) = observed_fixture(&first).await;
    let (_mb, cb) = observed_fixture(&second).await;
    multiplier(&first, 100000);
    multiplier(&second, 500000);
    let pa = format!("{}.official", crypto::identity(&a));
    let pb = format!("{}.official", crypto::identity(&b));
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let (_, catalog) = call(&server, Method::GET, "/v1/models", "alice-key", Value::Null).await;
    assert_eq!(catalog["data"].as_array().unwrap().len(), 2);
    assert_eq!(catalog["data"][0]["id"], "chatgpt/model-test");
    assert_eq!(catalog["data"][1]["id"], "grok/model-test");
    assert_eq!(catalog["data"][1]["router_pools"], json!([pb]));
    assert_eq!(catalog["data"][1]["router_pricing"]["rates"][0]["pool"], pb);
    assert_eq!(
        catalog["data"][1]["router_pricing"]["rates"][0]["price_version"],
        3
    );
    let body = json!({"model":"grok/model-test","messages":[{"role":"user","content":"hello"}]});
    // A provider-qualified name, fixed pool, raw name, or legacy merchant
    // selector must never override the agent's provider constraint.
    for (ua, model, policy) in [
        ("codex_cli_rs/1.0", "grok/model-test", rule(None, 1000000)),
        (
            "claude-cli/2.1.0",
            "grok/model-test",
            rule(Some(&pb), 1000000),
        ),
        ("grok-shell/1.0", "model-test", rule(Some(&pa), 1000000)),
        ("GitHubCopilotChat/1.0", "model-test", rule(None, 1000000)),
    ] {
        let response = request_with_agent(
            &server,
            "alice-key",
            policy,
            json!({"model":model,"messages":[]}),
            ua,
        )
        .await;
        assert_eq!(response.status(), 403);
        let expected = if ua.starts_with("GitHub") {
            "unsupported_agent"
        } else {
            "agent_provider_mismatch"
        };
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            expected
        );
    }
    let legacy = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.url))
        .bearer_auth("alice-key")
        .header("user-agent", "grok-shell/1.0")
        .header("x-router-pool", &pa)
        .json(&json!({"model":"model-test","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.status(), 403);
    assert_eq!(
        legacy.json::<Value>().await.unwrap()["error"]["code"],
        "agent_provider_mismatch"
    );
    assert_eq!(ca.load(Ordering::SeqCst), 0);
    assert_eq!(cb.load(Ordering::SeqCst), 0);
    assert!(r.engine().vault.lock().unwrap().events.is_empty());
    for policy in [rule(None, 1000000), rule(Some(&pb), 1000000)] {
        let response = grok_request(&server, "alice-key", policy, body.clone()).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-router-pool"], pb);
        response.bytes().await.unwrap();
    }
    // A cheaper identically named model from another provider is not eligible.
    let response = grok_request(&server, "alice-key", rule(Some(&pa), 1000000), body.clone()).await;
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "agent_provider_mismatch"
    );
    // Provider-scoped platform grants also apply to catalogs and legacy raw IDs.
    let mut config = r.engine().config.clone();
    config.keys[0].models = vec!["grok/model-test".into()];
    r.apply(
        Update {
            config: Some(serde_json::to_value(config).unwrap()),
            ..Default::default()
        },
        ADMIN,
    )
    .await
    .unwrap();
    let (_, catalog) = call(&server, Method::GET, "/v1/models", "alice-key", Value::Null).await;
    assert_eq!(catalog["data"].as_array().unwrap().len(), 1);
    assert_eq!(catalog["data"][0]["id"], "grok/model-test");
    let (_, pools) = call(&server, Method::GET, "/v1/pools", "alice-key", Value::Null).await;
    assert_eq!(pools["data"].as_array().unwrap().len(), 1);
    assert_eq!(pools["data"][0]["id"], pb);
    let response = grok_request(
        &server,
        "alice-key",
        rule(None, 1000000),
        json!({"model":"model-test","messages":[{"role":"user","content":"legacy"}]}),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-pool"], pb);
    response.bytes().await.unwrap();
    assert_eq!(ca.load(Ordering::SeqCst), 0);
    assert_eq!(cb.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn router_measures_stream_before_filtering_usage_for_the_client() {
    use axum::{body::Body, response::Response};
    let r = root();
    let token = control("a");
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

#[tokio::test]
async fn soft_session_remembers_the_latest_pool_across_merchants_in_speed_mode() {
    let r = root();
    let a = control("a");
    let b = control("b");
    let first = add(&r, &a).await;
    let second = add(&r, &b).await;
    let (_ma, _) = observed_fixture(&first).await;
    let (_mb, _) = observed_fixture(&second).await;
    let pa = format!("{}.official", crypto::identity(&a));
    let pb = format!("{}.official", crypto::identity(&b));
    let server = serve(crate::server::app_runtime(r)).await;
    let body =
        json!({"model":"model-test","messages":[{"role":"user","content":"one conversation"}]});
    for pool in [&pb, &pa, &pb] {
        let response = request(
            &server,
            "alice-key",
            rule(Some(pool), 1000000),
            body.clone(),
        )
        .await;
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        let mut policy = rule(None, 1000000);
        policy["priority"] = "speed".into();
        let response = request(&server, "alice-key", policy, body.clone()).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-router-pool"], *pool);
        response.bytes().await.unwrap();
    }
}
async fn request(server: &Server, token: &str, policy: Value, body: Value) -> reqwest::Response {
    request_with_agent(server, token, policy, body, "codex_cli_rs/1.0").await
}
async fn grok_request(
    server: &Server,
    token: &str,
    policy: Value,
    body: Value,
) -> reqwest::Response {
    request_with_agent(server, token, policy, body, "grok-shell/1.0").await
}
async fn request_with_agent(
    server: &Server,
    token: &str,
    policy: Value,
    body: Value,
    ua: &str,
) -> reqwest::Response {
    reqwest::Client::builder()
        .user_agent(ua)
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
        .route("/v1/language-models",any(||async {Json(json!({"models":[{"id":"model-test"}]}))}))
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
    let a = control("a");
    let b = control("b");
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
    let a = control("a");
    let b = control("b");
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
    let token = control("a");
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
    let a = control("a");
    let b = control("b");
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
            .header("user-agent", "codex_cli_rs/1.0")
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

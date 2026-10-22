use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn pool_model_policy_survives_merchant_sync_and_recovery_and_rejects_invalid_lists() {
    let r = root();
    let token = control("a");
    add(&r, &token).await;
    let server = serve(crate::server::app_runtime(r.clone())).await;
    for invalid in [
        json!(["*"]),
        json!(["x", "x"]),
        json!(["a/../b"]),
        json!([""]),
        json!([42]),
        json!(vec!["x"; 4097]),
    ] {
        let mut body = update(2);
        body["pools"][0]["allowed_models"] = invalid;
        assert_eq!(
            call(&server, Method::PUT, "/merchant/config", &token, body)
                .await
                .0,
            400
        );
    }
    let mut body = update(2);
    body["pools"][0]["allowed_models"] = json!(["model-test", "model-other"]);
    let (status, saved) = call(
        &server,
        Method::PUT,
        "/merchant/config",
        &token,
        body.clone(),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(
        saved["pools"][0]["allowed_models"],
        json!(["model-test", "model-other"])
    );
    r.merchants.live.lock().unwrap().clear();
    let empty = unlock(&r, &token, 3).await.unwrap();
    assert_eq!(empty["pools"], json!([]));
    body["expected_revision"] = 3.into();
    let (status, restored) = call(&server, Method::PUT, "/merchant/config", &token, body).await;
    assert_eq!(status, 200);
    assert_eq!(
        restored["pools"][0]["allowed_models"],
        saved["pools"][0]["allowed_models"]
    );
    let mut body = update(4);
    body["pools"][0]["allowed_models"] = json!([]);
    assert_eq!(
        call(&server, Method::PUT, "/merchant/config", &token, body)
            .await
            .1["pools"][0]["allowed_models"],
        json!([])
    );
    let (_, saved) = call(&server, Method::PUT, "/merchant/config", &token, update(5)).await;
    assert!(saved["pools"][0].get("allowed_models").is_none());
}

#[tokio::test]
async fn twenty_models_two_accounts_publish_three_and_block_all_route_bypasses() {
    let r = root();
    let token = control("a");
    let live = add(&r, &token).await;
    let ids: Vec<_> = (0..20).map(|i| format!("model-{i:02}")).collect();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let catalog = json!({"data":ids.iter().map(|id| json!({"id":id})).collect::<Vec<_>>()});
    let upstream = serve(Router::new().route("/v1/models", any(move || {let value = catalog.clone(); async move {Json(value)}}))
        .fallback(any(move |Json(body): Json<Value>| {let counter = counter.clone(); async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Json(json!({"model":body["model"],"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}))
        }}))).await;
    {
        let engine = r.engine();
        let mut data = engine.vault.lock().unwrap();
        let book = &mut data.official_pricing.providers[0];
        let template = book.models[0].clone();
        book.models = ids
            .iter()
            .map(|id| {
                let mut price = template.clone();
                price.model = id.clone();
                price
            })
            .collect();
    }
    let mut config = live.runtime.engine().config.clone();
    let pool = &mut config.models[0];
    pool.allowed_models = Some(ids[..3].to_vec());
    let first = &mut pool.deployments[0];
    first.api_base = Some(format!("{}/v1", upstream.url));
    first.proxy = Some(upstream.url.clone());
    first.enabled = true;
    let mut second = first.clone();
    second.id = "account-2".into();
    pool.deployments.push(second);
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(&config).unwrap()),
                ..Default::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let qualified_pool = format!("{}.official", crypto::identity(&token));
    let (status, visible) =
        call(&server, Method::GET, "/v1/models", "alice-key", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(visible["data"].as_array().unwrap().len(), 3);
    for row in visible["data"].as_array().unwrap() {
        assert_eq!(row["router_capabilities"].as_array().unwrap().len(), 2);
    }
    let (_, inventory) = call(&server, Method::GET, "/v1/pools", "alice-key", Value::Null).await;
    for account in inventory["data"][0]["accounts"].as_array().unwrap() {
        assert_eq!(account["catalog"]["visible_models"], 3);
    }
    // Merchants retain the full discovery evidence for their selection UI.
    for account in live.runtime.engine().discovery_status()["data"]
        .as_array()
        .unwrap()
    {
        assert_eq!(account["official_models"].as_object().unwrap().len(), 20);
    }
    let client = reqwest::Client::builder()
        .user_agent("codex_cli_rs/1.0 (merchant fixture)")
        .no_proxy()
        .build()
        .unwrap();
    for (id, accepted) in [("model-00", true), ("model-03", false)] {
        for model in [id.to_owned(), format!("chatgpt/{id}")] {
            for mode in ["fixed", "auto", "legacy"] {
                let mut request = client
                    .post(format!("{}/v1/chat/completions", server.url))
                    .bearer_auth("alice-key")
                    .json(&json!({"model":model,"messages":[{"role":"user","content":"hello"}]}));
                request = if mode == "legacy" {
                    request.header("x-router-pool", &qualified_pool)
                } else {
                    let mut rule = json!({"version":1,"mode":mode,"priority":"price","max_multiplier_millionths":1000000});
                    if mode == "fixed" {
                        rule["pool"] = qualified_pool.clone().into();
                    }
                    request.header("x-router-routing", rule.to_string())
                };
                let response = request.send().await.unwrap();
                assert_eq!(response.status().is_success(), accepted, "{mode} {model}");
                response.bytes().await.unwrap();
            }
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    // An update applies to subsequent requests, even with a previously advertised ID.
    config.models[0].allowed_models = Some(vec![]);
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(&config).unwrap()),
                ..Default::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    assert!(
        call(&server, Method::GET, "/v1/models", "alice-key", Value::Null)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let response = client
        .post(format!("{}/v1/chat/completions", server.url))
        .bearer_auth("alice-key")
        .header("x-router-pool", &qualified_pool)
        .json(&json!({"model":"chatgpt/model-00","messages":[]}))
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    // Allowing an unknown ID never invents upstream capability. None restores legacy discovery.
    config.models[0].allowed_models = Some(vec!["not-in-catalog".into()]);
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(&config).unwrap()),
                ..Default::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    assert!(
        call(&server, Method::GET, "/v1/models", "alice-key", Value::Null)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    config.models[0].allowed_models = None;
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(&config).unwrap()),
                ..Default::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    assert_eq!(
        call(&server, Method::GET, "/v1/models", "alice-key", Value::Null)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .len(),
        20
    );
}

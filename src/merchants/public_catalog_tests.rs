use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn public_inventory_is_aggregate_read_only_and_respects_pool_policy_and_capacity() {
    let r = root();
    let token = control("a");
    let live = add(&r, &token).await;
    let counter = Arc::new(AtomicUsize::new(0));
    let calls = counter.clone();
    let upstream = serve(Router::new().fallback(any(move || {
        let calls = calls.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Json(json!({"data":[{"id":"model-test"},{"id":"private-model"}]}))
        }
    })))
    .await;
    let mut config = live.runtime.engine().config.clone();
    config.models[0].allowed_models = Some(vec!["model-test".into()]);
    let deployment = &mut config.models[0].deployments[0];
    deployment.enabled = true;
    deployment.max_concurrent = 8;
    deployment.proxy = Some(upstream.url.clone());
    deployment.api_base = Some(format!("{}/v1", upstream.url));
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
    live.runtime.engine().refresh_models(true).await;
    let target = live.runtime.engine().latency_targets()[0].clone();
    r.merchants
        .latency
        .record(&target, Some(std::time::Duration::from_millis(170)));
    {
        let mut settings = live.settings.lock().unwrap();
        settings.max_concurrent = 3;
        settings.pool_pricing[0].multiplier_millionths = 200000;
    }
    let before_calls = counter.load(Ordering::SeqCst);
    let before_data = r.engine().vault.lock().unwrap().sequence;
    let before_settings = serde_json::to_value(live.settings.lock().unwrap().clone()).unwrap();
    let server = serve(crate::server::app_runtime(r.clone())).await;
    for credential in ["", "alice-key", &token] {
        assert_eq!(
            call(
                &server,
                Method::GET,
                "/admin/public-catalog",
                credential,
                Value::Null
            )
            .await
            .0,
            401
        );
    }
    let permit = live.gate.acquire(3).unwrap();
    let (status, catalog) = call(
        &server,
        Method::GET,
        "/admin/public-catalog",
        ADMIN,
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{catalog}");
    let pool = &catalog["data"][0];
    assert_eq!(pool["models"].as_array().unwrap().len(), 1);
    assert_eq!(pool["models"][0]["id"], "chatgpt/model-test");
    assert_eq!(pool["models"][0]["pricing"]["input"], 1000000);
    assert_eq!(pool["multiplier_millionths"], 200000);
    assert_eq!(pool["concurrency"]["limit"], 3);
    assert_eq!(pool["concurrency"]["available"], 2);
    assert_eq!(pool["merchant_limits"]["in_flight"], 1);
    assert_eq!(pool["available_accounts"], 1);
    assert_eq!(pool["client_accounts"].as_array().unwrap().len(), 1);
    assert_eq!(pool["client_accounts"][0]["available"], true);
    assert_eq!(pool["client_accounts"][0]["catalog"]["visible_models"], 1);
    assert_eq!(pool["client_accounts"][0]["proxy_latency"]["latency_ms"], 170.0);
    assert_eq!(pool["available"], true);
    assert_eq!(pool["proxy_latency"]["status"], "measured");
    assert_eq!(pool["proxy_latency"]["latency_ms"], 170.0);
    let text = catalog.to_string();
    for hidden in [
        "credential_hash",
        "MERCHANT_PRIVATE_UPSTREAM_CREDENTIAL",
        "private-model",
        "alice",
        "customers",
        "\"proxy\":",
        &upstream.url,
        "deployment",
        "api_key_ref",
        "backup",
        "last_error",
    ] {
        assert!(!text.contains(hidden), "leaked {hidden}");
    }
    assert_eq!(counter.load(Ordering::SeqCst), before_calls);
    assert_eq!(r.engine().vault.lock().unwrap().sequence, before_data);
    assert_eq!(
        serde_json::to_value(live.settings.lock().unwrap().clone()).unwrap(),
        before_settings
    );
    let (_, inventory) = catalog_fixture(&r,"pools","alice-key").await;
    assert_eq!(inventory["data"][0]["proxy_latency"], pool["proxy_latency"]);
    assert_eq!(
        inventory["data"][0]["accounts"][0]["proxy_latency"],
        pool["proxy_latency"]
    );
    drop(permit);
    live.settings.lock().unwrap().rpm = 1;
    live.meter.lock().unwrap().requests = 1;
    let (_, throttled) = call(
        &server,
        Method::GET,
        "/admin/public-catalog",
        ADMIN,
        Value::Null,
    )
    .await;
    assert_eq!(throttled["data"][0]["concurrency"]["available"], 0);
    assert_eq!(throttled["data"][0]["available"], false);
    live.settings.lock().unwrap().rpm = 0;
    // A retired/missing quote is not treated as a free offering.
    r.engine()
        .vault
        .lock()
        .unwrap()
        .official_pricing
        .providers
        .clear();
    let (_, catalog) = call(
        &server,
        Method::GET,
        "/admin/public-catalog",
        ADMIN,
        Value::Null,
    )
    .await;
    assert_eq!(catalog["data"][0]["models"][0]["pricing"], Value::Null);
    assert_eq!(catalog["data"][0]["available"], false);
    r.engine()
        .vault
        .lock()
        .unwrap()
        .merchants
        .get_mut(&crypto::identity(&token))
        .unwrap()
        .enabled = false;
    assert!(
        call(
            &server,
            Method::GET,
            "/admin/public-catalog",
            ADMIN,
            Value::Null
        )
        .await
        .1["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

async fn enable(live: &Live, url: &str, second_pool: bool) {
    let mut config = live.runtime.engine().config.clone();
    let pool = &mut config.models[0];
    pool.allowed_models = Some(vec!["model-test".into()]);
    pool.deployments[0].enabled = true;
    pool.deployments[0].api_base = Some(format!("{url}/v1"));
    pool.deployments[0].proxy = Some(url.into());
    if second_pool {
        let mut other = pool.clone();
        other.name = "second".into();
        other.deployments[0].id = "second-account".into();
        other.allowed_models = Some(vec![]);
        config.models.push(other);
    }
    live.runtime
        .apply(
            Update {
                config: Some(json!(config)),
                ..Default::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn provider_catalog_is_shared_across_merchants_pools_and_revocable_sources() {
    let r = root();
    r.set_quiescing(true); // This test drives refreshes explicitly.
    let a = add(&r, &control("a")).await;
    let b = add(&r, &control("b")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicBool::new(false));
    let count = calls.clone();
    let failed = failures.clone();
    let upstream = serve(Router::new().fallback(any(move || {
        let count = count.clone();
        let failed = failed.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            // Make overlapping refresh calls coalesce deterministically.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            if failed.load(Ordering::SeqCst) {
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({})))
            } else {
                (
                    axum::http::StatusCode::OK,
                    Json(json!({"data":[{"id":"model-test"},{"id":"other"}]})),
                )
            }
        }
    })))
    .await;
    enable(&a, &upstream.url, true).await;
    enable(&b, &upstream.url, true).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "sync does not probe each account"
    );
    let ae = a.runtime.engine();
    let be = b.runtime.engine();
    tokio::join!(ae.refresh_models(true), be.refresh_models(true));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for engine in [&ae, &be] {
        for row in engine.discovery_status()["data"].as_array().unwrap() {
            assert_eq!(row["catalog"]["scope"], "provider");
            assert_eq!(row["catalog"]["provider"], "openai");
            assert!(row["catalog"].get("source_deployment").is_none());
            assert_eq!(row["official_models"].as_object().unwrap().len(), 2);
        }
    }
    // Same catalog, independent pool admission policies.
    assert_eq!(
        ae.config.models[0].allowed_models,
        Some(vec!["model-test".into()])
    );
    assert_eq!(ae.config.models[1].allowed_models, Some(vec![]));
    ae.refresh_models(false).await;
    be.refresh_models(false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Removing and disabling owners excludes their sources even while old Engines live.
    let mut disabled = registration(&control("a"));
    disabled.enabled = false;
    register(&r, disabled, ADMIN).await.unwrap();
    let mut config = be.config.clone();
    config.models.clear();
    b.runtime
        .apply(
            Update {
                config: Some(json!(config)),
                ..Default::default()
            },
            &b.admin,
        )
        .await
        .unwrap();
    ae.refresh_models(true).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        ae.discovery_status()["data"][0]["official_models"],
        json!({})
    );
    register(&r, registration(&control("a")), ADMIN)
        .await
        .unwrap();
    ae.refresh_models(true).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // Every eligible account failed: erase the shared evidence immediately.
    failures.store(true, Ordering::SeqCst);
    ae.refresh_models(true).await;
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    for engine in [&ae, &be] {
        for row in engine.discovery_status()["data"].as_array().unwrap() {
            assert_eq!(row["official_models"], json!({}));
            assert_eq!(row["catalog"]["complete_catalog"], false);
            assert_eq!(row["catalog"]["last_error"], "upstream_unavailable");
        }
    }
}

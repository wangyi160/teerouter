use super::*;
use teellm::vault::{Provision, Vault};

pub(super) fn bundle(provider: Provider, proxies: &[&Mock]) -> Provision {
    bundle_urls(
        provider,
        &proxies
            .iter()
            .map(|proxy| proxy.running.url.as_str())
            .collect::<Vec<_>>(),
    )
}

fn bundle_urls(provider: Provider, proxies: &[&str]) -> Provision {
    let mut accounts = serde_json::Map::new();
    let mut deployments = Vec::new();
    for (i, proxy) in proxies.iter().enumerate() {
        let name = format!("account-{i}");
        accounts.insert(name.clone(), json!({"provider":provider,"proxy":proxy,
            "credential":{"access_token":format!("sk-ant-oat-fixture-{i}"),"expires_at":4102444800u64}}));
        deployments.push(json!({"id":name,"provider":provider,"oauth_account":name,"api_base":"http://provider.fixture.invalid/v1"}));
    }
    serde_json::from_value(json!({"config":{
        "server":{"admin_key_ref":"admin"},
        "users":[{"id":"test-user"}],"keys":[{"name":"user","user_id":"test-user","key_ref":"client","models":["pool/*"]}],
        "models":[{"name":"pool","discover":true,"max_attempts":2,"retry_delay_ms":0,"deployments":deployments}]},
        "secrets":{"client":"client-secret","admin":"admin-secret"},"accounts":accounts})).unwrap()
}

fn catalog(provider: Provider, names: &[&str]) -> Reply {
    Reply::json(match provider {
        Provider::OpenAI => {
            json!({"models":names.iter().map(|id| json!({"slug":id,"visibility":"list"})).collect::<Vec<_>>()})
        }
        Provider::Anthropic => {
            json!({"data":names.iter().map(|id| json!({"id":id})).collect::<Vec<_>>(),"has_more":false})
        }
        Provider::XAI => {
            json!({"models":names.iter().map(|id| json!({"id":id})).collect::<Vec<_>>()})
        }
        #[cfg(feature = "workbuddy-lab")]
        Provider::Workbuddy => {
            json!({"data":names.iter().map(|id| json!({"id":id})).collect::<Vec<_>>()})
        }
    })
}

async fn listed(gateway: &Running) -> Vec<String> {
    let response = client()
        .get(format!("{}/v1/models", gateway.url))
        .bearer_auth("client-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json::<Value>().await.unwrap()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn generate(gateway: &Running, provider: Provider, id: &str) -> reqwest::Response {
    match provider {
        Provider::Anthropic => {
            post(
                gateway,
                "/v1/messages",
                json!({"model":id,"max_tokens":8,"messages":[{"role":"user","content":"hello"}]}),
            )
            .await
        }
        _ => {
            post(
                gateway,
                "/v1/responses",
                json!({"model":id,"input":"hello"}),
            )
            .await
        }
    }
}

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::Anthropic)]
#[case(Provider::XAI)]
#[tokio::test]
async fn discovers_through_account_proxy_and_routes_an_unconfigured_future_model(
    #[case] provider: Provider,
) {
    let replies = vec![
        catalog(provider, &["future-model-not-in-config"]),
        Reply::json(native(provider)),
    ];
    let proxy = catalog_mock(replies).await;
    let v = Vault::memory().unwrap();
    v.provision(&bundle(provider, &[&proxy])).unwrap();
    let e = Engine::from_vault(v).unwrap();
    let gateway = run(server::app_upstream_fixture(e)).await;
    let lists = futures_util::future::join_all((0..8).map(|_| listed(&gateway))).await;
    for list in lists {
        assert_eq!(list, ["future-model-not-in-config"]);
    }
    let response = generate(&gateway, provider, "future-model-not-in-config").await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let status: Value = client()
        .get(format!("{}/admin/models", gateway.url))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["data"][0]["catalog"]["complete_catalog"], true);
    let calls = proxy.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for call in calls.iter() {
        assert!(call.uri.starts_with("http://provider.fixture.invalid/"));
        assert_eq!(call.headers["authorization"], "Bearer sk-ant-oat-fixture-0");
    }
    assert_eq!(
        calls.last().unwrap().body["model"],
        "future-model-not-in-config"
    );
    match provider {
        Provider::OpenAI => assert!(calls[0].path.contains("/models?client_version=")),
        Provider::Anthropic => {
            assert!(calls[0].path.starts_with("/v1/models?limit="));
            assert!(calls[0].headers.contains_key("anthropic-beta"));
        }
        Provider::XAI => assert_eq!(calls[0].path, "/v1/language-models"),
        #[cfg(feature = "workbuddy-lab")]
        Provider::Workbuddy => assert_eq!(calls[0].path, "/v1/models"),
    }
}

#[tokio::test]
async fn pool_fetches_one_catalog_shares_it_and_fails_over_to_another_source() {
    let a = catalog_mock(vec![catalog(Provider::XAI, &["alpha"])]).await;
    // The second account serves generation without fetching its own catalog.
    let b = catalog_mock(vec![Reply::json(native(Provider::XAI))]).await;
    let v = Vault::memory().unwrap();
    v.provision(&bundle(Provider::XAI, &[&a, &b])).unwrap();
    let e = Engine::from_vault(v).unwrap();
    e.refresh_models(true).await;
    let gateway = run(server::app_upstream_fixture(e.clone())).await;
    for list in futures_util::future::join_all((0..8).map(|_| listed(&gateway))).await {
        assert_eq!(list, ["alpha"]);
    }
    assert_eq!(a.calls.lock().unwrap().len(), 1);
    assert!(b.calls.lock().unwrap().is_empty());
    let response = client()
        .post(format!("{}/v1/responses", gateway.url))
        .bearer_auth("client-secret")
        .header("x-router-account", "account-1")
        .json(&json!({"model":"alpha","input":"hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        b.calls.lock().unwrap()[0].headers["authorization"],
        "Bearer sk-ant-oat-fixture-1"
    );
    a.replies
        .lock()
        .unwrap()
        .push_back(catalog(Provider::XAI, &["updated"]));
    e.refresh_models(true).await;
    assert_eq!(a.calls.lock().unwrap().len(), 2); // One GET even though two backends share it.
    assert_eq!(b.calls.lock().unwrap().len(), 1);
    assert_eq!(listed(&gateway).await, ["updated"]);
    a.replies.lock().unwrap().push_back(Reply::error(503));
    b.replies
        .lock()
        .unwrap()
        .push_back(catalog(Provider::XAI, &["replacement"]));
    e.refresh_models(true).await;
    for row in e.discovery_status()["data"].as_array().unwrap() {
        assert_eq!(row["catalog"]["scope"], "pool");
        assert_eq!(row["catalog"]["source_deployment"], "account-1");
        assert_eq!(
            row["official_models"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["replacement"]
        );
    }
    // Last successful source is preferred on the next refresh, with no union of old results.
    b.replies
        .lock()
        .unwrap()
        .push_back(catalog(Provider::XAI, &["latest"]));
    e.refresh_models(true).await;
    assert_eq!(a.calls.lock().unwrap().len(), 3);
    assert_eq!(b.calls.lock().unwrap().len(), 3);
    assert_eq!(listed(&gateway).await, ["latest"]);
    assert_eq!(
        generate(&gateway, Provider::XAI, "alpha").await.status(),
        404
    );
}

#[tokio::test]
async fn pool_catalog_falls_back_after_timeout_or_auth_failure() {
    for auth_failure in [false, true] {
        let first = if auth_failure {
            Reply::error(401)
        } else {
            let mut reply = catalog(Provider::XAI, &["slow"]);
            reply.delay = Duration::from_secs(2);
            reply
        };
        let a = catalog_mock(vec![first]).await;
        let b = catalog_mock(vec![catalog(Provider::XAI, &["working"])]).await;
        let mut config = bundle(Provider::XAI, &[&a, &b]);
        config.config.discovery.timeout_secs = 1;
        let v = Vault::memory().unwrap();
        v.provision(&config).unwrap();
        let e = Engine::from_vault(v).unwrap();
        e.refresh_models(true).await;
        for row in e.discovery_status()["data"].as_array().unwrap() {
            assert_eq!(row["catalog"]["source_deployment"], "account-1");
            assert!(row["official_models"].get("working").is_some());
            assert!(row["official_models"].get("slow").is_none());
        }
        assert_eq!(a.calls.lock().unwrap().len(), 1);
        assert_eq!(b.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn anthropic_pagination_is_complete_and_failed_refresh_never_replaces_good_cache() {
    let proxy = catalog_mock(vec![
        Reply::json(json!({"data":[{"id":"first"}],"has_more":true,"last_id":"cursor"})),
        Reply::json(json!({"data":[{"id":"second"}],"has_more":false})),
    ])
    .await;
    let v = Vault::memory().unwrap();
    v.provision(&bundle(Provider::Anthropic, &[&proxy]))
        .unwrap();
    let e = Engine::from_vault(v).unwrap();
    let gateway = run(server::app_upstream_fixture(e.clone())).await;
    assert_eq!(listed(&gateway).await, ["first", "second"]);
    assert!(
        proxy.calls.lock().unwrap()[1]
            .path
            .contains("after_id=cursor")
    );
    proxy.replies.lock().unwrap().extend([
        Reply::json(json!({"data":[{"id":"incomplete"}],"has_more":true,"last_id":"cursor"})),
        Reply::error(500),
    ]);
    e.refresh_models(true).await;
    assert_eq!(listed(&gateway).await, ["first", "second"]);
    proxy.replies.lock().unwrap().push_back(Reply::json(
        json!({"data":[{"id":"invalid model\nsecret"}]}),
    ));
    e.refresh_models(true).await;
    assert_eq!(listed(&gateway).await, ["first", "second"]);
    proxy.replies.lock().unwrap().push_back(Reply::error(403));
    e.refresh_models(true).await;
    assert!(listed(&gateway).await.is_empty());
}

#[tokio::test]
async fn memory_cache_is_reused_but_new_credentials_invalidate_it() {
    let proxy = catalog_mock(vec![catalog(Provider::XAI, &["PRIVATE_MODEL_SENTINEL"])]).await;
    let v = Vault::memory().unwrap();
    v.provision(&bundle(Provider::XAI, &[&proxy])).unwrap();
    let e = Engine::from_vault(v.clone()).unwrap();
    e.refresh_models(true).await;
    drop(e);
    let reopened = v.clone();
    let e = Engine::from_vault(reopened.clone()).unwrap();
    let gateway = run(server::app_upstream_fixture(e)).await;
    assert_eq!(listed(&gateway).await, ["PRIVATE_MODEL_SENTINEL"]);
    assert_eq!(proxy.calls.lock().unwrap().len(), 1); // Loaded encrypted cache; no new request.
    proxy
        .replies
        .lock()
        .unwrap()
        .push_back(catalog(Provider::XAI, &["replacement"]));
    reopened
        .import_account(
            "account-0",
            Provider::XAI,
            &json!({"access_token":"replacement-token","expires_at":4102444800u64}),
        )
        .unwrap();
    assert_eq!(listed(&gateway).await, ["replacement"]);
    assert_eq!(
        proxy.calls.lock().unwrap().last().unwrap().headers["authorization"],
        "Bearer replacement-token"
    );
}

#[tokio::test]
async fn background_refresh_works_without_list_requests_and_shared_account_limits_survive_updates()
{
    // Periodic discovery must keep receiving a valid catalog after the second
    // refresh, and must never consume a queued generation response.
    let catalog_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = catalog_calls.clone();
    let proxy = run(Router::new().fallback(any(move |request: Request| {
        let calls = calls.clone();
        async move {
            match request.uri().path() {
                "/v1/language-models" => {
                    let count = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let model = if count == 0 { "old" } else { "new" };
                    axum::Json(json!({"models":[{"id":model}]})).into_response()
                }
                "/v1/responses" => axum::Json(native(Provider::XAI)).into_response(),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
    })))
    .await;
    let unused = catalog_mock(vec![]).await;
    let mut data = bundle_urls(Provider::XAI, &[&proxy.url, &unused.running.url]);
    data.config.models[0].deployments[1].rpm = 1;
    data.config.discovery.refresh_secs = 1;
    data.config.models[0].deployments[0].rpm = 1;
    let v = Vault::memory().unwrap();
    v.provision(&data).unwrap();
    let e = Engine::from_vault(v).unwrap();
    let gateway = run(server::app_upstream_fixture(e.clone())).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        // Wait for the response to be persisted, not merely for the mock to receive the GET.
        // This read does not trigger discovery, so the test still proves background refresh.
        while !e.discovery_status()["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["official_models"].get("new").is_some())
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(catalog_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    assert!(unused.calls.lock().unwrap().is_empty());
    assert!(
        e.discovery_status()["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["official_models"].get("new").is_some())
    );
    assert_eq!(listed(&gateway).await, ["new"]);
    for expected in [200, 503] {
        let response = client()
            .post(format!("{}/v1/responses", gateway.url))
            .bearer_auth("client-secret")
            .header("x-router-account", "account-0")
            .json(&json!({"model":"new","input":"hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[tokio::test]
async fn expired_catalog_blocks_generation_even_when_acl_allows_it() {
    let proxy = catalog_mock(vec![catalog(Provider::XAI, &["allowed", "denied"])]).await;
    let mut data = bundle(Provider::XAI, &[&proxy]);
    data.config.keys[0].models = vec!["pool/allowed".into()];
    data.config.discovery.refresh_secs = 1;
    data.config.discovery.max_stale_secs = 1;
    let v = Vault::memory().unwrap();
    v.provision(&data).unwrap();
    let gateway = run(server::app_upstream_fixture(Engine::from_vault(v).unwrap())).await;
    assert_eq!(listed(&gateway).await, ["allowed"]);
    assert_eq!(
        generate(&gateway, Provider::XAI, "denied").await.status(),
        403
    );
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(listed(&gateway).await.is_empty());
    proxy
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::json(native(Provider::XAI)));
    assert_eq!(
        generate(&gateway, Provider::XAI, "allowed").await.status(),
        503
    );
}

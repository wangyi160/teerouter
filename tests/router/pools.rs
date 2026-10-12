use super::*;
use teellm::vault::Vault;

async fn inventory(gateway: &Running, key: &str) -> Value {
    let response = client()
        .get(format!("{}/v1/pools", gateway.url))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

#[tokio::test]
async fn pools_exist_before_discovery_include_disabled_and_empty_and_never_leak_secrets() {
    let proxy = catalog_mock(vec![Reply::error(401), Reply::error(401)]).await;
    let mut bundle = discovery::bundle(Provider::OpenAI, &[&proxy]);
    bundle.config.keys[0].models.clear();
    let mut disabled = bundle.config.models[0].deployments[0].clone();
    disabled.id = "disabled-deployment".into();
    disabled.oauth_account = Some("disabled-account".into());
    let mut stored = bundle.accounts["account-0"].clone();
    stored.enabled = false;
    stored.credential =
        json!({"access_token":"NEVER_EXPOSE_ACCESS", "refresh_token":"NEVER_EXPOSE_REFRESH"});
    stored.proxy = Some("http://PRIVATE_USER:PRIVATE_PASSWORD@127.0.0.1:1".into());
    bundle.accounts.insert("disabled-account".into(), stored);
    bundle.config.models[0].deployments.push(disabled);
    let mut empty = bundle.config.models[0].clone();
    empty.name = "empty-pool".into();
    empty.provider = Some(Provider::XAI);
    empty.deployments.clear();
    bundle.config.models.push(empty);
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let engine = Engine::from_vault(vault).unwrap();
    // Inspect before starting the server's automatic discovery task.
    let before = engine.pools(0);
    assert_eq!(
        proxy.calls.lock().unwrap().len(),
        0,
        "inventory must never contact upstream"
    );
    assert_eq!(before["data"].as_array().unwrap().len(), 2);
    let accounts = &before["data"][0]["accounts"];
    assert_eq!(accounts[0]["catalog"]["state"], "not_fetched");
    assert_eq!(accounts[0]["available"], false);
    assert_eq!(accounts[1]["enabled"], false);
    assert_eq!(accounts[1]["catalog"]["state"], "disabled");
    assert_eq!(before["data"][1]["accounts"], json!([]));
    assert_eq!(before["data"][1]["provider"], "xai");
    engine.refresh_models(true).await;
    let after = engine.pools(0);
    assert_eq!(after["data"][0]["accounts"][0]["catalog"]["state"], "error");
    assert_eq!(
        after["data"][0]["accounts"][0]["catalog"]["last_error"],
        "authentication_failed"
    );
    assert_eq!(
        after["data"][0]["accounts"][0]["catalog"]["visible_models"],
        0
    );
    assert_eq!(after["data"].as_array().unwrap().len(), 2);
    let text = after.to_string();
    for secret in [
        "NEVER_EXPOSE_ACCESS",
        "NEVER_EXPOSE_REFRESH",
        "PRIVATE_USER",
        "PRIVATE_PASSWORD",
        "sk-ant-oat-fixture",
        "api_base",
        "credential",
        "proxy",
        "DO_NOT_LEAK",
    ] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
    let gateway = run(server::app_upstream_fixture(engine.clone())).await;
    assert_eq!(
        inventory(&gateway, "client-secret").await["data"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let unauthorized = client()
        .get(format!("{}/v1/pools", gateway.url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
}

#[tokio::test]
async fn inventory_respects_pool_acl_before_discovery_and_model_acl_after_discovery() {
    let proxy = mock(vec![]).await;
    let mut bundle = discovery::bundle(Provider::OpenAI, &[&proxy]);
    bundle.config.keys[0].models = vec!["pool/chat".into()];
    let mut hidden = bundle.config.models[0].clone();
    hidden.name = "hidden".into();
    hidden.deployments.clear();
    bundle.config.models.push(hidden);
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let engine = Engine::from_vault(vault).unwrap();
    let gateway = run(server::app_upstream_fixture(engine.clone())).await;
    let before = inventory(&gateway, "client-secret").await;
    assert_eq!(before["data"].as_array().unwrap().len(), 1);
    assert!(!before.to_string().contains("hidden"));
    engine.refresh_models(true).await;
    let after = inventory(&gateway, "client-secret").await;
    assert_eq!(
        after["data"][0]["accounts"][0]["catalog"]["visible_models"],
        1
    );
    assert_eq!(after["data"][0]["available_accounts"], 1);
}

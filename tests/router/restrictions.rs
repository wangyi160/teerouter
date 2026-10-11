use super::*;
use teellm::vault::Vault;

fn catalog() -> Reply {
    Reply::json(json!({"models":[
        {"id":"official","aliases":["official-alias"],"capabilities":{"reasoning_effort":["low","high"]}},
        {"id":"other","capabilities":{"reasoning_effort":["low","high"]}}
    ]}))
}
async fn setup(proxy: &Mock) -> (Arc<Vault>, Running) {
    let vault = Vault::memory().unwrap();
    vault
        .provision(&discovery::bundle(Provider::XAI, &[proxy]))
        .unwrap();
    let app = run(server::app(Engine::from_vault(vault.clone()).unwrap())).await;
    (vault, app)
}
async fn read(gateway: &Running, path: &str, admin: bool) -> Value {
    client()
        .get(format!("{}{path}", gateway.url))
        .bearer_auth(if admin {
            "admin-secret"
        } else {
            "client-secret"
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
async fn apply(gateway: &Running, value: Value) -> reqwest::Response {
    client()
        .post(format!("{}/admin/apply", gateway.url))
        .bearer_auth("admin-secret")
        .json(&value)
        .send()
        .await
        .unwrap()
}
async fn update(gateway: &Running, rules: Value) {
    let result = apply(gateway, json!({"config":{"restrictions":rules}})).await;
    assert_eq!(result.status(), 200, "{}", result.text().await.unwrap());
}
fn request(model: &str, effort: &str) -> Value {
    json!({"model":model,"input":"hello","stream":false,"reasoning":{"effort":effort}})
}

#[tokio::test]
async fn block_unblock_and_restart_leave_the_official_catalog_intact() {
    let proxy = catalog_mock(vec![catalog()]).await;
    let (vault, gateway) = setup(&proxy).await;
    let before = read(&gateway, "/v1/models", false).await;
    let original =
        read(&gateway, "/admin/models", true).await["data"][0]["official_models"].clone();
    update(
        &gateway,
        json!([{"provider":"xai","model":"official","block_model":true}]),
    )
    .await;
    let models = read(&gateway, "/v1/models", false).await;
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "other");
    for model in ["official", "official-alias"] {
        let response = post(&gateway, "/v1/responses", request(model, "low")).await;
        assert_eq!(response.status(), 403);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "model_blocked"
        );
    }
    assert_eq!(
        read(&gateway, "/admin/models", true).await["data"][0]["official_models"],
        original
    );
    let restarted = run(server::app(Engine::from_vault(vault).unwrap())).await;
    assert_eq!(
        post(&restarted, "/v1/responses", request("official", "low"))
            .await
            .status(),
        403
    );
    update(&restarted, json!([])).await;
    assert_eq!(read(&restarted, "/v1/models", false).await, before);
    assert_eq!(
        read(&restarted, "/admin/models", true).await["data"][0]["official_models"],
        original
    );
    assert_eq!(proxy.calls.lock().unwrap().len(), 1); // Policy edits did not refetch or replace evidence.
    proxy
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::json(native(Provider::XAI)));
    let body = request("official-alias", "low");
    assert_eq!(
        post(&restarted, "/v1/responses", body.clone())
            .await
            .status(),
        200
    );
    let mut expected = body;
    expected["store"] = false.into();
    assert_eq!(proxy.calls.lock().unwrap().last().unwrap().body, expected);
}

#[tokio::test]
async fn blocked_effort_is_separate_from_official_capabilities_and_never_downgraded() {
    let proxy = catalog_mock(vec![catalog()]).await;
    let (_, gateway) = setup(&proxy).await;
    read(&gateway, "/v1/models", false).await;
    update(
        &gateway,
        json!([{"model":"official","blocked_efforts":["high"]}]),
    )
    .await;
    let list = read(&gateway, "/v1/models", false).await;
    for id in ["official", "official-alias"] {
        let entry = list["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap();
        let c = &entry["router_capabilities"][0];
        assert_eq!(c["capabilities"]["effort"], json!(["high", "low"]));
        assert_eq!(c["available_capabilities"]["effort"], json!(["low"]));
        assert_eq!(
            post(&gateway, "/v1/responses", request(id, "high"))
                .await
                .status(),
            403
        );
    }
    assert_eq!(proxy.calls.lock().unwrap().len(), 1);
    proxy
        .replies
        .lock()
        .unwrap()
        .extend(vec![Reply::json(native(Provider::XAI)); 4]);
    for id in ["official", "official-alias"] {
        let body = json!({"model":id,"input":"hello"});
        assert_eq!(
            post(&gateway, "/v1/responses", body.clone()).await.status(),
            200
        );
        let mut expected = body;
        expected["store"] = false.into();
        assert_eq!(proxy.calls.lock().unwrap().last().unwrap().body, expected);
    }
    for (model, effort) in [("official-alias", "low"), ("other", "high")] {
        let body = request(model, effort);
        assert_eq!(
            post(&gateway, "/v1/responses", body.clone()).await.status(),
            200
        );
        let mut expected = body;
        expected["store"] = false.into();
        assert_eq!(proxy.calls.lock().unwrap().last().unwrap().body, expected);
    }
}

#[tokio::test]
async fn blocked_thinking_type_does_not_require_thinking_on_native_messages() {
    let proxy = catalog_mock(vec![Reply::json(json!({"data":[{"id":"official","capabilities":{"thinking":{"types":["adaptive","disabled"]}}}],"has_more":false})),
        Reply::json(native(Provider::Anthropic))]).await;
    let mut bundle = discovery::bundle(Provider::Anthropic, &[&proxy]);
    bundle.config.restrictions = serde_json::from_value(
        json!([{"blocked_efforts":["high"],"blocked_thinking_types":["adaptive"]}]),
    )
    .unwrap();
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let gateway = run(server::app(Engine::from_vault(vault).unwrap())).await;
    let body =
        json!({"model":"official","max_tokens":64,"messages":[{"role":"user","content":"hello"}]});
    assert_eq!(
        post(&gateway, "/v1/messages", body.clone()).await.status(),
        200
    );
    assert_eq!(proxy.calls.lock().unwrap().last().unwrap().body, body);
    let mut blocked = body;
    blocked["thinking"] = json!({"type":"adaptive"});
    assert_eq!(post(&gateway, "/v1/messages", blocked).await.status(), 403);
    assert_eq!(proxy.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn administrator_cannot_upload_or_edit_official_models_and_unblocking_cannot_invent_values() {
    let proxy = catalog_mock(vec![catalog()]).await;
    let (_, gateway) = setup(&proxy).await;
    read(&gateway, "/v1/models", false).await;
    let before = read(&gateway, "/admin/models", true).await["data"][0]["official_models"].clone();
    for update in [
        json!({"config":{"official_models":{"fake":{}}}}),
        json!({"config":{"restrictions":[{"model":"fake","allowed_efforts":["high"]}]}}),
        json!({"config":{"restrictions":[{"replace_model":"official"}]}}),
        json!({"config":{"restrictions":[{"blocked_budget_ranges":[{"min":0,"max":1024}]}]}}),
    ] {
        assert_eq!(apply(&gateway, update).await.status(), 400);
    }
    assert_eq!(
        client()
            .post(format!("{}/admin/models", gateway.url))
            .bearer_auth("admin-secret")
            .json(&json!({"fake":{}}))
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    update(&gateway, json!([{"model":"fake","block_model":false}])).await;
    assert_eq!(
        post(&gateway, "/v1/responses", request("fake", "high"))
            .await
            .status(),
        404
    );
    assert_eq!(
        post(&gateway, "/v1/responses", request("official", "invented"))
            .await
            .status(),
        400
    );
    assert_eq!(
        read(&gateway, "/admin/models", true).await["data"][0]["official_models"],
        before
    );
    assert_eq!(proxy.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn global_restrictions_cannot_be_bypassed_by_another_account_or_fallback_pool() {
    let a = catalog_mock(vec![catalog()]).await;
    let b = catalog_mock(vec![catalog()]).await;
    let mut bundle = discovery::bundle(Provider::XAI, &[&a, &b]);
    let mut backup = bundle.config.models[0].clone();
    backup.name = "backup".into();
    backup.deployments.retain(|d| d.id == "account-1");
    bundle.config.models[0]
        .deployments
        .retain(|d| d.id == "account-0");
    bundle.config.models[0].fallbacks.push("backup".into());
    bundle.config.models.push(backup);
    bundle.config.restrictions =
        serde_json::from_value(json!([{"blocked_efforts":["high"]}])).unwrap();
    let v = Vault::memory().unwrap();
    v.provision(&bundle).unwrap();
    let gateway = run(server::app(Engine::from_vault(v).unwrap())).await;
    for account in ["account-0", "account-1"] {
        let response = client()
            .post(format!("{}/v1/responses", gateway.url))
            .bearer_auth("client-secret")
            .header("x-router-pool", "pool")
            .header("x-router-account", account)
            .json(&request("official", "high"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
    assert_eq!(a.calls.lock().unwrap().len(), 1);
    assert_eq!(b.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn official_refresh_updates_evidence_without_erasing_the_independent_deny_policy() {
    let proxy = catalog_mock(vec![catalog()]).await;
    let (_, gateway) = setup(&proxy).await;
    read(&gateway, "/v1/models", false).await;
    update(
        &gateway,
        json!([{"model":"official","blocked_efforts":["high"]}]),
    )
    .await;
    proxy.replies.lock().unwrap().push_back(Reply::json(json!({"models":[
        {"id":"official","aliases":["new-official-alias"],"capabilities":{"reasoning_effort":["low","high","future"]}}
    ]})));
    assert_eq!(
        client()
            .post(format!("{}/admin/models/refresh", gateway.url))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let list = read(&gateway, "/v1/models", false).await;
    for entry in list["data"].as_array().unwrap() {
        let c = &entry["router_capabilities"][0];
        assert_eq!(
            c["capabilities"]["effort"],
            json!(["future", "high", "low"])
        );
        assert_eq!(
            c["available_capabilities"]["effort"],
            json!(["future", "low"])
        );
    }
    assert_eq!(
        post(
            &gateway,
            "/v1/responses",
            request("new-official-alias", "high")
        )
        .await
        .status(),
        403
    );
    proxy
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::json(native(Provider::XAI)));
    let body = request("new-official-alias", "future");
    assert_eq!(
        post(&gateway, "/v1/responses", body.clone()).await.status(),
        200
    );
    let mut expected = body;
    expected["store"] = false.into();
    assert_eq!(proxy.calls.lock().unwrap().last().unwrap().body, expected);
}

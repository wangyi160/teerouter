use super::*;
use teellm::vault::Vault;

fn xai(model: &str, levels: Option<&[&str]>) -> Reply {
    let mut entry = json!({"id":model});
    if let Some(levels) = levels {
        entry["capabilities"] = json!({"reasoning_effort":levels});
    }
    Reply::json(json!({"models":[entry]}))
}

async fn start(provider: Provider, proxies: &[&Mock]) -> (Arc<Engine>, Running) {
    let mut bundle = discovery::bundle(provider, proxies);
    // Legacy settings must not bypass validation or grant model access.
    bundle.config.models[0].discover = false;
    bundle.config.models[0].deployments[0].model = "static-hint".into();
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let engine = Engine::from_vault(vault).unwrap();
    let gateway = run(server::app(engine.clone())).await;
    (engine, gateway)
}

fn body(model: &str, effort: &str) -> Value {
    json!({"model":model,"input":"hello","stream":false,"reasoning":{"effort":effort}})
}

#[tokio::test]
async fn rejects_unlisted_models_and_efforts_without_sending_generation() {
    let proxy = catalog_mock(vec![xai("official", Some(&["high"]))]).await;
    let (_, gateway) = start(Provider::XAI, &[&proxy]).await;
    for (request, status, code) in [
        (body("static-hint", "high"), 404, "model_not_found"),
        (body("unknown", "high"), 404, "model_not_found"),
        (body("official", "HIGH"), 400, "unsupported_reasoning"),
        (body("official", "future"), 400, "unsupported_reasoning"),
        (
            json!({"model":"official","input":"hello","stream":false,"reasoning":{"effort":7}}),
            400,
            "invalid_reasoning",
        ),
    ] {
        let response = post(&gateway, "/v1/responses", request).await;
        assert_eq!(response.status(), status);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            code
        );
    }
    let list: Value = client()
        .get(format!("{}/v1/models", gateway.url))
        .bearer_auth("client-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(list["data"][0]["id"], "official");
    assert_eq!(
        list["data"][0]["router_capabilities"][0]["capabilities"]["effort"],
        json!(["high"])
    );
    assert_eq!(proxy.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn refresh_admits_new_model_and_effort_then_removes_old_capabilities() {
    let proxy = catalog_mock(vec![xai("old", Some(&["low"]))]).await;
    let (engine, gateway) = start(Provider::XAI, &[&proxy]).await;
    assert_eq!(
        post(
            &gateway,
            "/v1/responses",
            body("vendor/Future-v99:preview", "future-ULTRA")
        )
        .await
        .status(),
        404
    );
    proxy
        .replies
        .lock()
        .unwrap()
        .push_back(xai("vendor/Future-v99:preview", Some(&["future-ULTRA"])));
    engine.refresh_models(true).await;
    proxy
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::json(native(Provider::XAI)));
    let original = body("vendor/Future-v99:preview", "future-ULTRA");
    assert_eq!(
        post(&gateway, "/v1/responses", original.clone())
            .await
            .status(),
        200
    );
    let mut expected = original.clone();
    expected["store"] = false.into();
    assert_eq!(proxy.calls.lock().unwrap().last().unwrap().body, expected);
    assert_eq!(
        post(&gateway, "/v1/responses", body("old", "low"))
            .await
            .status(),
        404
    );
    proxy
        .replies
        .lock()
        .unwrap()
        .push_back(xai("vendor/Future-v99:preview", Some(&[])));
    engine.refresh_models(true).await;
    assert_eq!(
        post(&gateway, "/v1/responses", original).await.status(),
        400
    );
    assert_eq!(proxy.calls.lock().unwrap().len(), 4); // Three catalogs, one generation.
}

#[tokio::test]
async fn schedules_only_accounts_with_the_exact_capability_and_respects_pinning() {
    let low = catalog_mock(vec![xai("official", Some(&["low"]))]).await;
    let high = catalog_mock(vec![
        xai("official", Some(&["high"])),
        Reply::json(native(Provider::XAI)),
    ])
    .await;
    let (_, gateway) = start(Provider::XAI, &[&low, &high]).await;
    let request = body("official", "high");
    let response = post(&gateway, "/v1/responses", request.clone()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-deployment"], "account-1");
    let response = client()
        .post(format!("{}/v1/responses", gateway.url))
        .bearer_auth("client-secret")
        .header("x-router-account", "account-0")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(low.calls.lock().unwrap().len(), 1);
    assert_eq!(high.calls.lock().unwrap().len(), 2);
    let mut expected = request;
    expected["store"] = false.into();
    assert_eq!(high.calls.lock().unwrap()[1].body, expected);
}

#[tokio::test]
async fn missing_capabilities_reject_explicit_effort_but_do_not_invent_defaults() {
    let proxy = catalog_mock(vec![
        xai("official", None),
        Reply::json(native(Provider::XAI)),
    ])
    .await;
    let (_, gateway) = start(Provider::XAI, &[&proxy]).await;
    let response = post(&gateway, "/v1/responses", body("official", "high")).await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "reasoning_capabilities_unavailable"
    );
    let request = json!({"model":"official","input":"hello","stream":false});
    assert_eq!(
        post(&gateway, "/v1/responses", request.clone())
            .await
            .status(),
        200
    );
    let mut expected = request;
    expected["store"] = false.into();
    assert_eq!(proxy.calls.lock().unwrap()[1].body, expected);
}

#[tokio::test]
async fn unavailable_catalog_never_falls_back_to_a_static_hint() {
    let proxy = catalog_mock(vec![Reply::error(503)]).await;
    let (_, gateway) = start(Provider::XAI, &[&proxy]).await;
    let response = post(
        &gateway,
        "/v1/responses",
        json!({"model":"static-hint","input":"hello"}),
    )
    .await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "catalog_unavailable"
    );
    assert_eq!(proxy.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn official_aliases_are_accepted_without_resolving_them_to_other_ids() {
    let proxy = catalog_mock(vec![Reply::json(json!({"models":[{"id":"versioned-id","aliases":["official-alias"],"capabilities":{"reasoning_effort":["high"]}}]})), Reply::json(native(Provider::XAI))]).await;
    let (_, gateway) = start(Provider::XAI, &[&proxy]).await;
    let request = body("official-alias", "high");
    assert_eq!(
        post(&gateway, "/v1/responses", request.clone())
            .await
            .status(),
        200
    );
    let mut expected = request;
    expected["store"] = false.into();
    assert_eq!(proxy.calls.lock().unwrap()[1].body, expected);
}

#[tokio::test]
async fn conflicting_official_entries_do_not_union_their_permissions() {
    let proxy = catalog_mock(vec![Reply::json(json!({"models":[
        {"id":"official","capabilities":{"reasoning_effort":["low"]}},
        {"id":"official","capabilities":{"reasoning_effort":["high"]}}
    ]}))])
    .await;
    let (_, gateway) = start(Provider::XAI, &[&proxy]).await;
    assert_eq!(
        post(&gateway, "/v1/responses", body("official", "high"))
            .await
            .status(),
        503
    );
    assert_eq!(proxy.calls.lock().unwrap().len(), 1);
}

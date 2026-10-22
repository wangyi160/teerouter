use super::*;
use teellm::vault::Vault;

#[tokio::test]
async fn workbuddy_discovers_and_forwards_chat_through_bound_proxy() {
    let proxy = catalog_mock(vec![
        Reply::json(json!({"data":[{"id":"future-local-chat"}]})),
        Reply::json(json!({"id":"local-reply","object":"chat.completion","model":"future-local-chat",
            "choices":[{"index":0,"message":{"role":"assistant","content":"local answer"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}})),
    ]).await;
    let mut bundle = discovery::bundle(Provider::Workbuddy, &[&proxy]);
    bundle.config.models[0].provider = Some(Provider::Workbuddy);
    bundle.config.models[0].deployments[0].api_base = None;
    bundle.accounts.get_mut("account-0").unwrap().credential =
        json!({"api_key":"local-fixture-secret"});
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let engine = Engine::from_vault(vault).unwrap();
    engine.refresh_models(true).await;
    let list = engine.models(0).await;
    let model = &list["data"][0];
    assert_eq!(model["id"], "future-local-chat");
    assert_eq!(model["router_capabilities"][0]["provider"], "workbuddy");
    assert_eq!(model["router_capabilities"][0]["local_test"], true);
    assert_eq!(
        model["router_capabilities"][0]["protocols"],
        json!(["chat_completions"])
    );
    let gateway = run_fixture(engine).await;
    let body = json!({"model":"future-local-chat","messages":[{"role":"user","content":"preserve this exact question"}],"store":true});
    let response = post(&gateway, "/v1/chat/completions", body.clone()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "local answer"
    );
    {
        let calls = proxy.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[0]
                .uri
                .contains(&format!("{}/models", Provider::Workbuddy.base_url()))
        );
        assert!(calls[1].uri.contains(&format!(
            "{}/chat/completions",
            Provider::Workbuddy.base_url()
        )));
        assert_eq!(
            calls[1].headers["authorization"],
            "Bearer local-fixture-secret"
        );
        assert_eq!(calls[1].body["messages"], body["messages"]);
        assert_eq!(calls[1].body["model"], body["model"]);
        assert_eq!(calls[1].body["store"], false);
    }
    let invalid = post(
        &gateway,
        "/v1/responses",
        json!({"model":"future-local-chat","input":"hello"}),
    )
    .await;
    assert_ne!(invalid.status(), 200);
    assert_eq!(proxy.calls.lock().unwrap().len(), 2);
    assert_eq!(usage(&gateway).await["input_tokens"], 7);
}

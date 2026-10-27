use super::*;
use teellm::vault::Vault;

async fn apply(gateway: &Running, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}/admin/apply", gateway.url))
        .bearer_auth("admin-secret")
        .json(&body)
        .send()
        .await
        .unwrap()
}
fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAI => "openai",
        Provider::Anthropic => "anthropic",
        Provider::XAI => "xai",
        #[cfg(feature = "workbuddy-lab")]
        Provider::Workbuddy => "workbuddy",
    }
}
async fn generate(gateway: &Running, provider: Provider) -> reqwest::Response {
    let metadata = json!({"caller":"kept","obsolete":"remove-me"});
    match provider {
        Provider::Anthropic => post(gateway, "/v1/messages", json!({"model":"chat","max_tokens":8,"messages":[{"role":"user","content":"hello"}],"metadata":metadata})).await,
        _ => post(gateway, "/v1/responses", json!({"model":"chat","input":"hello","metadata":metadata,"store":true})).await,
    }
}
#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::Anthropic)]
#[case(Provider::XAI)]
#[tokio::test]
async fn generation_rewrites_cannot_be_activated_by_admin(#[case] provider: Provider) {
    let proxy = mock(vec![Reply::json(native(provider)); 2]).await;
    let mut bundle = discovery::bundle(provider, &[&proxy]);
    bundle.config.models[0].discover = false;
    bundle.config.keys[0].models.clear();
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let gateway = run_fixture(Engine::from_vault(vault.clone()).unwrap()).await;
    let unchanged = serde_json::to_value(vault.config().unwrap()).unwrap();
    let operation = if provider == Provider::Anthropic {
        "messages"
    } else {
        "responses"
    };
    for rule in [
        json!({"body_set":{"/version":"new"}}),
        json!({"body_defaults":{"/metadata/client":"router"}}),
        json!({"body_remove":["/metadata/obsolete"]}),
        json!({"body_null":["/include"]}),
        json!({"body_set":{"/store":false}}),
        json!({"body_set":{"/instructions":"admin instruction"}}),
        json!({"query":{"revision":"new"}}),
        json!({"headers":{"x-future-version":"new"}}),
    ] {
        let response = apply(&gateway,json!({"config":{"compatibility":{provider_name(provider):{"requests":{operation:rule}}}}})).await;
        assert_eq!(response.status(), 400);
        assert_eq!(
            serde_json::to_value(vault.config().unwrap()).unwrap(),
            unchanged
        );
    }
    assert!(proxy.calls.lock().unwrap().is_empty());
    let response = generate(&gateway, provider).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let calls = proxy.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].body["metadata"],
        json!({"caller":"kept","obsolete":"remove-me"})
    );
    assert!(calls[0].body.get("version").is_none());
    if provider != Provider::Anthropic {
        assert_eq!(calls[0].body["store"], true);
    }
}

#[tokio::test]
async fn codex_catalog_query_version_can_change_without_duplicate_parameters() {
    let proxy = catalog_mock(vec![Reply::json(json!({"models":[{"slug":"future"}]})); 2]).await;
    let mut bundle = discovery::bundle(Provider::OpenAI, &[&proxy]);
    bundle.config.compatibility.insert("openai".into(), serde_json::from_value(json!({"requests":{"models":{"query":{"client_version":"1.0.1"},"headers":{"x-new-catalog":"enabled"}}}})).unwrap());
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let gateway = run_fixture(Engine::from_vault(vault).unwrap()).await;
    for (i, version) in ["1.0.1", "1.1.1"].iter().enumerate() {
        if i == 1 {
            assert_eq!(apply(&gateway, json!({"config":{"compatibility":{"openai":{"requests":{"models":{"query":{"client_version":version}}}}}}})).await.status(), 200);
        }
        assert_eq!(fixture_models(&gateway,"client-secret").await["data"][0]["id"],"future");
        let calls = proxy.calls.lock().unwrap();
        let call = calls.last().unwrap();
        let url = reqwest::Url::parse(&call.uri).unwrap();
        assert_eq!(
            url.query_pairs()
                .filter(|(k, _)| k == "client_version")
                .count(),
            1
        );
        assert!(call.path.contains(version));
        assert_eq!(call.headers["x-new-catalog"], "enabled");
    }
}

#[rstest]
#[case(Provider::OpenAI, false, true)]
#[case(Provider::OpenAI, false, false)]
#[case(Provider::OpenAI, true, false)]
#[case(Provider::XAI, false, false)]
#[case(Provider::XAI, true, false)]
#[tokio::test]
async fn storage_is_preserved_for_native_and_chat_requests_with_both_auth_modes(
    #[case] provider: Provider,
    #[case] oauth: bool,
    #[case] chat: bool,
) {
    let upstream_chat = chat;
    let result = if upstream_chat {
        completion()
    } else {
        native(provider)
    };
    let stream = if upstream_chat {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"id":"reply","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
        )
    } else {
        format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","output_index":0,"delta":"ok"}),
            json!({"type":"response.completed","response":result})
        )
    };
    let proxy = mock(vec![
        Reply::json(result.clone()),
        Reply::json(result),
        Reply::sse(stream),
    ])
    .await;
    let gateway = if oauth {
        let mut bundle = discovery::bundle(provider, &[&proxy]);
        bundle.config.keys[0].models.clear();
        let vault = Vault::memory().unwrap();
        vault.provision(&bundle).unwrap();
        run_fixture(Engine::from_vault(vault).unwrap()).await
    } else {
        let mut config = configuration(&proxy);
        config.models[0].deployments[0].provider = provider;
        gateway(config).await
    };
    for (index, store) in [None, Some(json!(true)), Some(Value::Null)]
        .into_iter()
        .enumerate()
    {
        let mut body = if chat {
            request()
        } else {
            json!({"model":"chat","input":"hello"})
        };
        body["stream"] = json!(index == 2);
        if let Some(store) = store {
            body["store"] = store;
        }
        let response = post(
            &gateway,
            if chat {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            },
            body,
        )
        .await;
        assert_eq!(response.status(), 200);
        let output = response.text().await.unwrap();
        assert!(output.contains("ok"), "{output}");
    }
    let calls = proxy.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    for (call, store) in calls
        .iter()
        .zip([None, Some(json!(true)), Some(Value::Null)])
    {
        assert_eq!(call.body.get("store"), store.as_ref());
        assert_eq!(call.body["model"], "chat");
    }
}

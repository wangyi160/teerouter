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
        Provider::Gemini => "gemini",
        Provider::XAI => "xai",
        #[cfg(feature = "workbuddy-lab")]
        Provider::Workbuddy => "workbuddy",
    }
}
async fn generate(gateway: &Running, provider: Provider) -> reqwest::Response {
    let metadata = json!({"caller":"kept","obsolete":"remove-me"});
    match provider {
        Provider::Anthropic => post(gateway, "/v1/messages", json!({"model":"chat","max_tokens":8,"messages":[{"role":"user","content":"hello"}],"metadata":metadata})).await,
        Provider::Gemini => post(gateway, "/v1beta/models/chat:generateContent", json!({"contents":[{"parts":[{"text":"hello"}]}],"metadata":metadata})).await,
        _ => post(gateway, "/v1/responses", json!({"model":"chat","input":"hello","metadata":metadata,"store":true})).await,
    }
}
#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::Anthropic)]
#[case(Provider::Gemini)]
#[case(Provider::XAI)]
#[tokio::test]
async fn future_wire_fields_change_live_after_provider_conversion(#[case] provider: Provider) {
    let mut replies = Vec::new();
    for _ in 0..2 {
        if provider == Provider::Gemini {
            replies.push(Reply::json(json!({"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"fixture-project"})));
            replies.push(Reply::json(json!({"response":native(provider)})));
        } else {
            replies.push(Reply::json(native(provider)));
        }
    }
    let proxy = mock(replies).await;
    let mut bundle = discovery::bundle(provider, &[&proxy]);
    bundle.config.models[0].name = "chat".into();
    bundle.config.models[0].discover = false;
    bundle.config.models[0].deployments[0].model = "chosen-model".into();
    bundle.config.keys[0].models.clear();
    let operation = match provider {
        Provider::Anthropic => "messages",
        Provider::Gemini => "generate_content",
        _ => "responses",
    };
    let prefix = if provider == Provider::Gemini {
        "/request"
    } else {
        ""
    };
    let rule = json!({
        "headers":{"x-future-version":"1.0.1"}, "query":{"revision":"1.0.1"},
        "body_set":{format!("{prefix}/version"):"1.0.1",format!("{prefix}/metadata/new_feature"):true},
        "body_defaults":{format!("{prefix}/metadata/caller"):"default"},
        "body_remove":[format!("{prefix}/metadata/obsolete")]
    });
    let profile: teellm::compatibility::Profile = serde_json::from_value(
        json!({"chatgpt_extra_parameters":["metadata"],"requests":{operation:rule}}),
    )
    .unwrap();
    bundle
        .config
        .compatibility
        .insert(provider_name(provider).into(), profile);
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let gateway = run(server::app(Engine::from_vault(vault.clone()).unwrap())).await;
    let response = generate(&gateway, provider).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let update = apply(&gateway, json!({"config":{"compatibility":{provider_name(provider):{"requests":{operation:{
        "headers":{"x-future-version":"1.1.1"},"query":{"revision":"1.1.1"},
        "body_set":{format!("{prefix}/version"):"1.1.1",format!("{prefix}/metadata/later_field"):42}
    }}}}}})).await;
    assert_eq!(update.status(), 200, "{}", update.text().await.unwrap());
    let rejected = apply(&gateway, json!({"config":{"compatibility":{provider_name(provider):{"requests":{operation:{"body_set":{"/model":"other-account-model"}}}}}}})).await;
    assert_eq!(rejected.status(), 400);
    for header in ["content-type", "accept"] {
        let rejected = apply(&gateway, json!({"config":{"compatibility":{provider_name(provider):{"requests":{operation:{"headers":{header:"text/plain"}}}}}}})).await;
        assert_eq!(rejected.status(), 400);
    }
    let rejected = apply(&gateway, json!({"config":{"compatibility":{provider_name(provider):{"requests":{operation:{"body_set":{"/store":true}}}}}}})).await;
    assert_eq!(rejected.status(), 400);
    let unchanged = serde_json::to_value(vault.config().unwrap()).unwrap();
    for rule in [
        json!({"body_set":{format!("{prefix}/instructions"):"admin instruction"}}),
        json!({"body_defaults":{format!("{prefix}/system"):"admin system"}}),
        json!({"body_set":{format!("{prefix}/tools"):[]}}),
        json!({"body_remove":[format!("{prefix}/messages")]}),
        json!({"body_null":[format!("{prefix}/input")]}),
        json!({"body_set":{format!("{prefix}/metadata/wrapper"): {"contents":[{"parts":[{"text":"injected"}]}]}}}),
        json!({"query":{"instructions":"injected"}}),
    ] {
        let rejected = apply(&gateway, json!({"config":{"compatibility":{provider_name(provider):{"requests":{operation:rule}}}}})).await;
        assert_eq!(rejected.status(), 400);
        assert_eq!(
            serde_json::to_value(vault.config().unwrap()).unwrap(),
            unchanged
        );
    }
    let response = generate(&gateway, provider).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(
        serde_json::to_value(vault.config().unwrap()).unwrap()["compatibility"]
            [provider_name(provider)]["requests"][operation]["body_set"]
            [format!("{prefix}/version")],
        "1.1.1"
    );
    let calls = proxy.calls.lock().unwrap();
    let generated: Vec<_> = calls
        .iter()
        .filter(|c| c.body.get("model").is_some())
        .collect();
    assert_eq!(generated.len(), 2);
    for (index, call) in generated.iter().enumerate() {
        let version = if index == 0 { "1.0.1" } else { "1.1.1" };
        assert_eq!(call.headers["x-future-version"], version);
        assert_eq!(call.headers["content-type"], "application/json");
        assert!(call.path.ends_with(&format!("?revision={version}")));
        assert_eq!(call.headers["authorization"], "Bearer sk-ant-oat-fixture-0");
        assert_eq!(call.body["model"], "chat");
        let body = if provider == Provider::Gemini {
            &call.body["request"]
        } else {
            &call.body
        };
        assert_eq!(body["version"], version);
        match provider {
            Provider::Anthropic => {
                assert_eq!(body["messages"], json!([{"role":"user","content":"hello"}]))
            }
            Provider::Gemini => assert_eq!(body["contents"], json!([{"parts":[{"text":"hello"}]}])),
            _ => assert_eq!(body["input"], "hello"),
        }
        assert_eq!(body["metadata"]["caller"], "kept");
        assert_eq!(body["metadata"]["new_feature"], true);
        assert!(body["metadata"].get("obsolete").is_none());
        if index == 1 {
            assert_eq!(body["metadata"]["later_field"], 42);
        }
        if provider == Provider::OpenAI {
            assert_eq!(body["stream"], true);
        }
        if matches!(provider, Provider::OpenAI | Provider::XAI) {
            assert_eq!(body["store"], false);
        } else {
            assert!(body.get("store").is_none());
        }
        if provider == Provider::Gemini {
            assert_eq!(call.body["project"], "fixture-project");
        }
    }
}

#[tokio::test]
async fn codex_catalog_query_version_can_change_without_duplicate_parameters() {
    let proxy = catalog_mock(vec![Reply::json(json!({"models":[{"slug":"future"}]})); 2]).await;
    let mut bundle = discovery::bundle(Provider::OpenAI, &[&proxy]);
    bundle.config.compatibility.insert("openai".into(), serde_json::from_value(json!({"requests":{"models":{"query":{"client_version":"1.0.1"},"headers":{"x-new-catalog":"enabled"}}}})).unwrap());
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let gateway = run(server::app(Engine::from_vault(vault).unwrap())).await;
    for (i, version) in ["1.0.1", "1.1.1"].iter().enumerate() {
        if i == 1 {
            assert_eq!(apply(&gateway, json!({"config":{"compatibility":{"openai":{"requests":{"models":{"query":{"client_version":version}}}}}}})).await.status(), 200);
        }
        let response = client()
            .get(format!("{}/v1/models", gateway.url))
            .bearer_auth("client-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.json::<Value>().await.unwrap()["data"][0]["id"],
            "future"
        );
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
#[case(Provider::OpenAI, true, true)]
#[case(Provider::OpenAI, true, false)]
#[case(Provider::XAI, false, false)]
#[case(Provider::XAI, true, false)]
#[tokio::test]
async fn storage_is_locked_for_native_and_chat_requests_with_both_auth_modes(
    #[case] provider: Provider,
    #[case] oauth: bool,
    #[case] chat: bool,
) {
    let upstream_chat = chat && !oauth;
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
        run(server::app(Engine::from_vault(vault).unwrap())).await
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
    for call in calls.iter() {
        assert_eq!(call.body["store"], false);
        assert_eq!(call.body["model"], "chat");
    }
}

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::Gemini)]
#[tokio::test]
async fn unified_chat_streams_use_the_configured_native_wire_operation(#[case] provider: Provider) {
    let mut replies = Vec::new();
    if provider == Provider::Gemini {
        replies.push(Reply::json(
            json!({"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"fixture-project"}),
        ));
        replies.push(Reply::sse(format!(
            "data: {}\n\n",
            json!({"response":native(provider)})
        )));
    } else {
        replies.push(Reply::sse(format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","output_index":0,"delta":"ok"}),
            json!({"type":"response.completed","response":native(provider)})
        )));
    }
    let proxy = mock(replies).await;
    let mut bundle = discovery::bundle(provider, &[&proxy]);
    bundle.config.models[0].name = "chat".into();
    bundle.config.models[0].discover = false;
    bundle.config.models[0].deployments[0].model = "chosen-model".into();
    bundle.config.keys[0].models.clear();
    let operation = if provider == Provider::Gemini {
        "stream_generate_content"
    } else {
        "responses"
    };
    let path = if provider == Provider::Gemini {
        "/request/version"
    } else {
        "/version"
    };
    let mut profile: teellm::compatibility::Profile = serde_json::from_value(json!({"requests":{operation:{"body_set":{path:"1.1.1"},"headers":{"x-stream-version":"1.1.1"}}}})).unwrap();
    if provider == Provider::Gemini {
        profile.paths.insert(
            "stream_generate_content".into(),
            "futureStreamGenerate".into(),
        );
        profile.requests.insert(
            "gemini_load".into(),
            serde_json::from_value(json!({"body_set":{"/metadata/clientVersion":"1.1.1"}}))
                .unwrap(),
        );
    }
    bundle
        .config
        .compatibility
        .insert(provider_name(provider).into(), profile);
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let gateway = run(server::app(Engine::from_vault(vault).unwrap())).await;
    let response = post(
        &gateway,
        "/v1/chat/completions",
        json!({"model":"chat","stream":true,"messages":[{"role":"user","content":"hello"}]}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("ok") && text.contains("[DONE]"), "{text}");
    let calls = proxy.calls.lock().unwrap();
    let generation = calls.last().unwrap();
    assert_eq!(generation.body.pointer(path).unwrap(), "1.1.1");
    assert_eq!(generation.headers["x-stream-version"], "1.1.1");
    if provider == Provider::Gemini {
        assert_eq!(calls[0].body["metadata"]["clientVersion"], "1.1.1");
        assert!(generation.path.ends_with(":futureStreamGenerate?alt=sse"));
    } else {
        assert_eq!(generation.body["stream"], true);
        assert_eq!(generation.body["store"], false);
    }
}

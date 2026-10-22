use super::*;

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::Anthropic)]
#[case(Provider::XAI)]
#[tokio::test]
async fn inference_requires_matching_agent_before_forwarding_or_admission(
    #[case] provider: Provider,
) {
    let mock = mock(vec![Reply::json(if provider == Provider::Anthropic {
        native(provider)
    } else {
        completion()
    })])
    .await;
    let mut config = configuration(&mock);
    config.models[0].deployments[0].provider = provider;
    let operation = if provider == Provider::Anthropic {
        "messages"
    } else {
        "chat_completions"
    };
    config.compatibility.insert(
        crate::compatibility::name(provider).into(),
        serde_json::from_value(json!({"headers":{"user-agent":"stale-profile/1.0"},
            "requests":{(operation):{"headers":{"user-agent":"stale-operation/1.0"}}}}))
        .unwrap(),
    );
    let gateway = gateway(config).await;
    for other in [Provider::OpenAI, Provider::Anthropic, Provider::XAI] {
        if provider == other {
            continue;
        }
        for endpoint in ["/v1/chat/completions", "/v1/messages", "/v1/responses"] {
            let response = post_as(&gateway, endpoint, request(), other).await;
            assert_eq!(response.status(), 403);
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"]["code"],
                "agent_provider_mismatch"
            );
        }
    }
    for ua in [
        None,
        Some("GitHubCopilotChat/1.0"),
        Some("OpenAI/Python 1.0"),
        Some("Mozilla/5.0 (grok-shell/1.0)"),
    ] {
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", gateway.url))
            .bearer_auth("client-secret")
            .json(&request());
        if let Some(ua) = ua {
            request = request.header("user-agent", ua);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 403);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "unsupported_agent"
        );
    }
    assert!(mock.calls.lock().unwrap().is_empty());
    assert_eq!(usage(&gateway).await["requests"], 0);
    let response = post_as(&gateway, "/v1/chat/completions", request(), provider).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    response.bytes().await.unwrap();
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
    assert_eq!(
        mock.calls.lock().unwrap()[0].headers["user-agent"],
        agent_ua(provider)
    );
    // Metadata remains accessible to teeproxy and other management clients.
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{}/v1/models", gateway.url))
            .bearer_auth("client-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn fallback_cannot_switch_agent_provider() {
    let first = mock(vec![Reply::error(503)]).await;
    let other = mock(vec![Reply::json(completion())]).await;
    let mut config = configuration(&first);
    let mut fallback = config.models[0].clone();
    fallback.name = "other".into();
    fallback.deployments[0].id = "other-account".into();
    fallback.deployments[0].provider = Provider::XAI;
    fallback.deployments[0].api_base = Some(format!("{}/v1", other.running.url));
    config.models[0].fallbacks = vec![fallback.name.clone()];
    config.models.push(fallback);
    config.keys[0].models.clear();
    let gateway = gateway(config).await;
    let response = client()
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth("client-secret")
        .header("x-router-pool", "chat")
        .json(&request())
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert!(other.calls.lock().unwrap().is_empty());
}

#[cfg(feature = "workbuddy-lab")]
#[tokio::test]
async fn workbuddy_keeps_explicit_test_exception() {
    let mock = mock(vec![Reply::json(completion())]).await;
    let mut config = configuration(&mock);
    config.models[0].deployments[0].provider = Provider::Workbuddy;
    let gateway = gateway(config).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth("client-secret")
        .header("user-agent", "GitHubCopilotChat/1.0")
        .json(&request())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
}

use super::*;

// These tests compare complete inbound/outbound JSON, including absent fields.
// Unknown fields must survive without an allowlist update in Router.
#[rstest]
#[case(Provider::OpenAI, "/v1/chat/completions", false)]
#[case(Provider::OpenAI, "/v1/responses", false)]
#[case(Provider::OpenAI, "/v1/responses", true)]
#[case(Provider::Anthropic, "/v1/messages", false)]
#[case(Provider::Anthropic, "/v1/messages", true)]
#[case(Provider::XAI, "/v1/chat/completions", false)]
#[case(Provider::XAI, "/v1/chat/completions", true)]
#[case(Provider::XAI, "/v1/responses", false)]
#[case(Provider::XAI, "/v1/responses", true)]
#[cfg_attr(
    feature = "workbuddy-lab",
    case(Provider::Workbuddy, "/v1/chat/completions", false)
)]
#[tokio::test]
async fn native_requests_preserve_all_agent_parameters(
    #[case] provider: Provider,
    #[case] endpoint: &str,
    #[case] oauth: bool,
) {
    let chat = endpoint.ends_with("completions");
    let response = if chat { completion() } else { native(provider) };
    let stream = if chat {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
        )
    } else if provider == Provider::Anthropic {
        format!(
            "event: message_start\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
            json!({"type":"message_start","message":response}),
            json!({"type":"message_stop"})
        )
    } else {
        format!(
            "event: response.completed\ndata: {}\n\n",
            json!({"type":"response.completed","response":response})
        )
    };
    let mock = mock(vec![
        Reply::json(response.clone()),
        Reply::json(response),
        Reply::sse(stream),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = configuration(&mock);
    config.models[0].deployments[0].provider = provider;
    if oauth {
        let path = dir.path().join("fixture.json");
        let token = if provider == Provider::Anthropic {
            "sk-ant-oat-fixture"
        } else {
            "fixture-access"
        };
        std::fs::write(
            &path,
            json!({"access_token":token,"expires_at":4102444800u64,"account_id":"fixture-account"})
                .to_string(),
        )
        .unwrap();
        config.models[0].deployments[0].api_key_env.clear();
        config.models[0].deployments[0].oauth_file = Some(path);
    }
    let gateway = gateway(config).await;
    let minimal = if chat || provider == Provider::Anthropic {
        json!({"model":"chat","messages":[{"role":"user","content":"hello"}]})
    } else {
        json!({"model":"chat","input":"hello"})
    };
    let mut rich = minimal.clone();
    rich["metadata"] = json!({"empty":"","null":null,"vendor":[1,{"nested":false}]});
    rich["future_parameter"] = json!({"precision":0.12345678901234567,"enabled":false});
    rich["store"] = false.into();
    rich["stream"] = false.into();
    rich["prompt_cache_key"] = "caller-cache-identity".into();
    if provider == Provider::Anthropic {
        rich["max_tokens"] = 1024.into();
        rich["system"] = json!([{"type":"text","text":"original system","cache_control":{"type":"ephemeral","ttl":"1h"}}]);
        rich["messages"][0]["content"] =
            json!([{"type":"text","text":"hello","cache_control":{"type":"ephemeral","ttl":"5m"}}]);
    } else if !chat {
        rich["instructions"] = "Agent's exact instructions".into();
        rich["include"] = json!([]);
        rich["prompt_cache_options"] = json!({"mode":"implicit","ttl":"12h"});
        rich["input"] = json!([{"role":"user","content":[{"type":"input_text","text":"hello","prompt_cache_breakpoint":{"ttl":"30m"}}]}]);
    } else {
        rich["max_completion_tokens"] = 64.into();
        rich["stream_options"] = json!({"include_usage":false,"future":null});
    }
    let mut streaming = rich.clone();
    streaming["stream"] = true.into();
    let bodies = [minimal, rich, streaming];
    for (i, body) in bodies.iter().enumerate() {
        let mut request = client()
            .post(format!(
                "{}{endpoint}?beta=true&tag=a%2Fb&tag=c%20d",
                gateway.url
            ))
            .bearer_auth("client-secret")
            .header("user-agent", gateway.agent)
            .header("accept", "application/json")
            .header("x-native-feature", "caller-value")
            .header("x-router-internal-query", "injected=bad")
            .header("chatgpt-account-id", "forged-account")
            .header("x-api-key", "client-secret")
            .header("cookie", "local-secret=DO_NOT_FORWARD")
            .json(body);
        if i > 0 {
            request = request
                .header("session_id", "original-session")
                .header("x-grok-conv-id", "original-grok-id");
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        response.bytes().await.unwrap();
    }
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), bodies.len());
    for (i, (call, body)) in calls.iter().zip(&bodies).enumerate() {
        assert_eq!(&call.body, body, "entire request must be preserved");
        assert!(
            call.uri.ends_with("?beta=true&tag=a%2Fb&tag=c%20d"),
            "{}",
            call.uri
        );
        assert_eq!(call.headers["x-native-feature"], "caller-value");
        assert_eq!(call.headers["accept"], "application/json");
        assert_eq!(call.headers["user-agent"], gateway.agent);
        assert!(!call.headers.contains_key("cookie"));
        assert!(
            !call
                .headers
                .keys()
                .any(|k| k.as_str().starts_with("x-router-"))
        );
        assert_ne!(
            call.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer client-secret")
        );
        assert_ne!(
            call.headers
                .get("chatgpt-account-id")
                .and_then(|v| v.to_str().ok()),
            Some("forged-account")
        );
        if i == 0 {
            assert!(!call.headers.contains_key("session_id"));
            assert!(!call.headers.contains_key("x-grok-conv-id"));
        } else {
            assert_eq!(call.headers["session_id"], "original-session");
            assert_eq!(call.headers["x-grok-conv-id"], "original-grok-id");
        }
    }
}

#[tokio::test]
async fn claude_chat_conversion_is_rejected_before_forwarding() {
    let mock = mock(vec![]).await;
    let mut config = configuration(&mock);
    config.models[0].deployments[0].provider = Provider::Anthropic;
    let gateway = gateway(config).await;
    let response = post(&gateway, "/v1/chat/completions", request()).await;
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "unsupported_protocol"
    );
    assert!(mock.calls.lock().unwrap().is_empty());
    assert_eq!(usage(&gateway).await["input_tokens"], 0);
}

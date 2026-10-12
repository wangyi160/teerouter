use super::*;
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

fn account(path: &Path, _provider: Provider, token: &str) {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let value = json!({"access_token":token,"expires_at":expiry,"account_id":"fixture-account"});
    std::fs::write(path, value.to_string()).unwrap();
}
fn oauth_config(mock: &Mock, provider: Provider, path: &Path) -> Config {
    let mut config = configuration(mock);
    let deploy = &mut config.models[0].deployments[0];
    deploy.provider = provider;
    deploy.api_key_env.clear();
    deploy.oauth_file = Some(path.into());
    config
}
fn events(values: Vec<Value>) -> Reply {
    Reply::sse(
        values
            .into_iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect(),
    )
}
fn codex_events(output: Value) -> Reply {
    let mut response = native(Provider::OpenAI);
    response["output"] = output;
    events(vec![
        json!({"type":"response.created","response":{"id":"resp-test"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"ok"}),
        json!({"type":"response.completed","response":response}),
    ])
}

#[rstest]
#[case(Provider::OpenAI, false)]
#[case(Provider::OpenAI, true)]
#[tokio::test]
async fn native_oauth_uses_account_backend_and_preserves_usage(
    #[case] provider: Provider,
    #[case] streaming: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    account(&path, provider, "fixture-access");
    let replies = vec![codex_events(native(provider)["output"].clone())];
    let mock = mock(replies).await;
    let gateway = gateway(oauth_config(&mock, provider, &path)).await;
    let (route, body) = (
        "/v1/responses",
        json!({"model":"chat","input":"hello","stream":streaming,"store":true,"service_tier":"fast"}),
    );
    let response = post(&gateway, route, body).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("ok"));

    let used = usage(&gateway).await;
    assert_eq!(used["input_tokens"], 10);
    assert_eq!(used["output_tokens"], 5);
    let calls = mock.calls.lock().unwrap();
    let last = calls.last().unwrap();
    for call in calls.iter() {
        assert_eq!(call.headers["authorization"], "Bearer fixture-access");
        assert!(!call.headers.contains_key("x-goog-api-key"));
    }
    assert_eq!(last.path, "/v1/responses");
    assert_eq!(last.headers["chatgpt-account-id"], "fixture-account");
    assert_eq!(last.headers["originator"], "codex_cli_rs");
    assert!(last.headers.contains_key("session_id"));
    assert_eq!(last.body["stream"], true);
    assert_eq!(last.body["store"], false);
    assert_eq!(last.body["service_tier"], "priority");
    assert!(
        last.body["include"]
            .as_array()
            .unwrap()
            .contains(&json!("reasoning.encrypted_content"))
    );
}

#[rstest]
#[case(Provider::OpenAI, false)]
#[case(Provider::OpenAI, true)]
#[tokio::test]
async fn chat_bridge_handles_tools_and_visible_replay(
    #[case] provider: Provider,
    #[case] streaming: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    account(&path, provider, "fixture-access");
    let tool = json!({"type":"function_call","call_id":"call-return","name":"lookup","arguments":"{\"city\":\"Paris\"}"});
    let mut final_response = native(provider);
    final_response["output"] = json!([tool]);
    let replies = vec![events(vec![
        json!({"type":"response.created","response":{"id":"resp-tools"}}),
        json!({"type":"response.output_item.added","output_index":2,"item":tool}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"city\":"}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"\"Paris\"}"}),
        json!({"type":"response.function_call_arguments.done","output_index":2,"arguments":"{\"city\":\"Paris\"}"}),
        json!({"type":"response.completed","response":final_response}),
    ])];
    let mock = mock(replies).await;
    let gateway = gateway(oauth_config(&mock, provider, &path)).await;
    let body = json!({"model":"chat","stream":streaming,"stream_options":{"include_usage":true},
        "messages":[{"role":"system","content":"system instructions"},{"role":"user","content":"weather"},
        {"role":"assistant","content":"checking","tool_calls":[{"id":"call-before","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"Rome\"}"}}]},
        {"role":"tool","tool_call_id":"call-before","content":"{\"weather\":\"sunny\"}"}],
        "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],
        "tool_choice":{"type":"function","function":{"name":"lookup"}}});
    let response = post(&gateway, "/v1/chat/completions", body).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("tool_calls"));
    assert!(text.contains("lookup"));
    assert!(text.contains("Paris"));
    if streaming {
        assert!(text.contains("[DONE]"));
        let chunks: Vec<Value> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let args: String = chunks
            .iter()
            .filter_map(|c| {
                c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
            })
            .collect();
        assert_eq!(args, "{\"city\":\"Paris\"}");
    }
    let used = usage(&gateway).await;
    assert_eq!(used["input_tokens"], 10);
    assert_eq!(used["output_tokens"], 5);
    let calls = mock.calls.lock().unwrap();
    let sent = &calls.last().unwrap().body;
    assert_eq!(sent["input"][2]["type"], "function_call");
    assert_eq!(sent["input"][3]["type"], "function_call_output");
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tool_choice"]["name"], "lookup");
}

#[tokio::test]
async fn chatgpt_nonstream_recovers_output_omitted_from_terminal_event() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    account(&path, Provider::OpenAI, "fixture");
    let mut response = native(Provider::OpenAI);
    let item = response["output"][0].take();
    response["output"] = json!([]);
    let mock = mock(vec![events(vec![
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":response}),
    ])])
    .await;
    let gateway = gateway(oauth_config(&mock, Provider::OpenAI, &path)).await;
    let result: Value = post(
        &gateway,
        "/v1/responses",
        json!({"model":"chat","input":"hello"}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(result["output"][0]["content"][0]["text"], "ok");
}

#[tokio::test]
async fn grok_account_failover_keeps_tokens_separate() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.json");
    let b = dir.path().join("b.json");
    account(&a, Provider::XAI, "account-a");
    account(&b, Provider::XAI, "account-b");
    let mock = mock(vec![
        Reply::error(401),
        Reply::json(completion()),
        Reply::json(completion()),
    ])
    .await;
    let mut config = oauth_config(&mock, Provider::XAI, &a);
    let mut second = config.models[0].deployments[0].clone();
    second.id = "b".into();
    second.oauth_file = Some(b);
    config.models[0].deployments.push(second);
    let gateway = gateway(config).await;
    let response = post(&gateway, "/v1/chat/completions", request()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-deployment"], "b");
    assert_eq!(response.headers()["x-router-attempts"], "2");
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        200
    );
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls[0].headers["authorization"], "Bearer account-a");
    assert_eq!(calls[1].headers["authorization"], "Bearer account-b");
    assert_eq!(calls[2].headers["authorization"], "Bearer account-b");
}

#[tokio::test]
async fn claude_oauth_merges_beta_headers_without_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    account(&path, Provider::Anthropic, "sk-ant-oat-fixture");
    let mock = mock(vec![Reply::json(native(Provider::Anthropic))]).await;
    let gateway = gateway(oauth_config(&mock, Provider::Anthropic, &path)).await;
    let response = client()
        .post(format!("{}/v1/messages", gateway.url))
        .bearer_auth("client-secret")
        .header("anthropic-beta", "custom-beta,oauth-2025-04-20")
        .json(
            &json!({"model":"chat","max_tokens":20,"messages":[{"role":"user","content":"hello"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let calls = mock.calls.lock().unwrap();
    let headers = &calls[0].headers;
    assert_eq!(headers["authorization"], "Bearer sk-ant-oat-fixture");
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(headers["anthropic-beta"], "custom-beta,oauth-2025-04-20");
    assert_eq!(headers["anthropic-dangerous-direct-browser-access"], "true");
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn truncated_codex_stream_returns_error_instead_of_success(#[case] done_sentinel: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    account(&path, Provider::OpenAI, "fixture");
    let mut reply = events(vec![
        json!({"type":"response.output_text.delta","delta":"partial"}),
    ]);
    if done_sentinel {
        reply.body.push_str("data: [DONE]\n\n");
    }
    let mock = mock(vec![reply]).await;
    let mut config = oauth_config(&mock, Provider::OpenAI, &path);
    config.models[0].max_attempts = 1;
    let gateway = gateway(config).await;
    let mut body = request();
    body["stream"] = true.into();
    let response = post(&gateway, "/v1/chat/completions", body).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("stream_interrupted"));
    assert!(!text.contains("[DONE]"));
}

#[rstest]
#[case(Provider::OpenAI)]
#[tokio::test]
async fn oauth_chat_usage_is_accounted_but_hidden_unless_requested(#[case] provider: Provider) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    account(&path, provider, "fixture");
    let replies = vec![codex_events(native(provider)["output"].clone())];
    let mock = mock(replies).await;
    let gateway = gateway(oauth_config(&mock, provider, &path)).await;
    let mut body = request();
    body["stream"] = true.into();
    let text = post(&gateway, "/v1/chat/completions", body)
        .await
        .text()
        .await
        .unwrap();
    assert!(text.contains("ok"));
    assert!(text.contains("[DONE]"));
    assert!(!text.contains("prompt_tokens"));
    assert_eq!(usage(&gateway).await["input_tokens"], 10);
}

#[tokio::test]
async fn codex_output_recovery_merges_sparse_text_items_and_prefers_real_items() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    account(&path, Provider::OpenAI, "fixture");
    let mut response = native(Provider::OpenAI);
    let real = response["output"][0].take();
    response["output"] = json!([]);
    let mock = mock(vec![events(vec![json!({"type":"response.output_text.done","output_index":1,"content_index":0,"text":"ignored duplicate"}),
        json!({"type":"response.output_text.done","output_index":4,"content_index":0,"text":"extra"}),
        json!({"type":"response.output_item.done","output_index":1,"item":real}),
        json!({"type":"response.completed","response":response})])]).await;
    let gateway = gateway(oauth_config(&mock, Provider::OpenAI, &path)).await;
    let value: Value = post(
        &gateway,
        "/v1/responses",
        json!({"model":"chat","input":"hello"}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(value["output"].as_array().unwrap().len(), 2);
    assert_eq!(value["output"][0]["content"][0]["text"], "ok");
    assert_eq!(value["output"][1]["content"][0]["text"], "extra");
}

#[tokio::test]
async fn codex_nonstream_also_accepts_a_json_backend_response() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    account(&path, Provider::OpenAI, "fixture");
    let mock = mock(vec![Reply::json(native(Provider::OpenAI))]).await;
    let gateway = gateway(oauth_config(&mock, Provider::OpenAI, &path)).await;
    let value: Value = post(&gateway, "/v1/chat/completions", request())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(value["choices"][0]["message"]["content"], "ok");
}

#[rstest]
fn oauth_example_resolves_paths_and_rejects_ambiguous_credentials() {
    let mut config = Config::parse(include_str!("../../config.oauth.example.toml")).unwrap();
    assert_eq!(config.models.len(), 3);
    config.models[0].deployments[0].api_key_env = "AMBIGUOUS".into();
    assert!(config.validate().is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, include_str!("../../config.oauth.example.toml")).unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.models[0].deployments[0].oauth_file.as_ref().unwrap(),
        &dir.path().join("accounts/chatgpt-1.json")
    );
}

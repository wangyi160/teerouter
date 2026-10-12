use super::*;

fn safe() -> Reply {
    let mut reply = Reply::json(
        json!({"safe":true,"label":0,"confidence":98,"policy_confidence":97,"request_id":"scan-safe"}),
    );
    reply
        .headers
        .insert("x-lockllm-on-unavailable", "block".parse().unwrap());
    reply
}
fn blocked() -> Reply {
    Reply {
        status: 403,
        ..Reply::json(
            json!({"request_id":"scan-blocked","error":{"code":"policy_violation",
        "message":"PRIVATE_PROMPT","violated_policies":[{"policy_name":"PRIVATE_PROMPT","violation_details":"PRIVATE_PROMPT","violated_categories":["Sexual Content"]}]}}),
        )
    }
}
fn configured(upstream: &Mock, scanner: &Mock) -> Config {
    let mut config = configuration(upstream);
    config.moderation = Some(crate::moderation::Config {
        api_key_ref: "LOCKLLM_KEY".into(),
        test_endpoint: Some(format!("{}/v1/scan", scanner.running.url)),
        ..Default::default()
    });
    config
}
async fn admin(g: &Running, method: &str, path: &str, body: Value) -> reqwest::Response {
    client()
        .request(method.parse().unwrap(), format!("{}{path}", g.url))
        .bearer_auth("admin-secret")
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn user(g: &Running) -> Value {
    admin(g, "GET", "/admin/users/test-user", Value::Null)
        .await
        .json::<Value>()
        .await
        .unwrap()["user"]
        .clone()
}

#[tokio::test]
async fn block_prevents_all_upstream_attempts_and_is_visible_to_admin_only() {
    let upstream = mock(vec![]).await;
    let scanner = mock(vec![blocked()]).await;
    let g = gateway(configured(&upstream, &scanner)).await;
    let response = post(&g, "/v1/chat/completions", request()).await;
    assert_eq!(response.status(), 403);
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "content_policy_violation"
    );
    assert!(upstream.calls.lock().unwrap().is_empty());
    assert_eq!(scanner.calls.lock().unwrap().len(), 1);
    assert_eq!(
        post(&g, "/admin/users/test-user", Value::Null)
            .await
            .status(),
        401
    );
    let record = user(&g).await;
    assert_eq!(record["moderation"]["blocked_requests"], 1);
    assert_eq!(record["moderation"]["unavailable_requests"], 0);
    assert_eq!(record["moderation"]["last_violation"]["request_id"], id);
    assert_eq!(
        record["moderation"]["last_violation"]["result"]["categories"],
        json!(["sexual_content"])
    );
    let state = admin(&g, "GET", "/admin/checkpoint", Value::Null)
        .await
        .json::<Value>()
        .await
        .unwrap();
    let event = &state["state"]["events"][0]["event"];
    assert_eq!(event["user_id"], "test-user");
    assert_eq!(event["attempts"], 0);
    assert_eq!(event["usage"]["input_tokens"], 0);
    assert_eq!(event["usage"]["unknown_usage_requests"], 0);
    assert!(!state.to_string().contains("PRIVATE_PROMPT"));
    assert_eq!(
        admin(
            &g,
            "PUT",
            "/admin/users/test-user",
            json!({"moderation":{"blocked_requests":0}})
        )
        .await
        .status(),
        400
    );
}

#[tokio::test]
async fn full_input_is_scanned_once_before_retries_and_client_headers_cannot_weaken_it() {
    let upstream = mock(vec![Reply::error(500), Reply::json(completion())]).await;
    let scanner = mock(vec![safe()]).await;
    let g = gateway(configured(&upstream, &scanner)).await;
    let long = format!("START{}END", "agent code ".repeat(50_000));
    let body = json!({"model":"chat","messages":[{"role":"system","content":"rules"},{"role":"user","content":long}],
        "tools":[{"type":"function","function":{"name":"run","description":"tool-description","parameters":{"type":"object"}}}]});
    let result = client()
        .post(format!("{}/v1/chat/completions", g.url))
        .bearer_auth("client-secret")
        .header("x-lockllm-scan-action", "allow_with_warning")
        .header("x-lockllm-scan-mode", "normal")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    assert_eq!(upstream.calls.lock().unwrap().len(), 2);
    let calls = scanner.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/v1/scan");
    assert_eq!(calls[0].headers["x-lockllm-scan-mode"], "combined");
    assert_eq!(calls[0].headers["x-lockllm-scan-action"], "block");
    assert_eq!(calls[0].headers["x-lockllm-policy-action"], "block");
    assert_eq!(calls[0].headers["x-lockllm-on-unavailable"], "block");
    assert_ne!(calls[0].headers["authorization"], "Bearer client-secret");
    assert_eq!(
        calls[0].headers["authorization"],
        "Bearer moderation-secret"
    );
    assert!(!calls[0].headers.contains_key("x-router-session"));
    let input: Value = serde_json::from_str(calls[0].body["input"].as_str().unwrap()).unwrap();
    assert_eq!(input["messages"], body["messages"]);
    assert_eq!(input["tools"], body["tools"]);
    assert_eq!(
        upstream.calls.lock().unwrap()[1].body["messages"],
        body["messages"]
    );
    assert_eq!(
        upstream.calls.lock().unwrap()[1].body["tools"],
        body["tools"]
    );
}

#[rstest]
#[case(401)]
#[case(402)]
#[case(429)]
#[case(500)]
#[case(503)]
#[tokio::test]
async fn scanner_errors_are_not_user_violations(#[case] status: u16) {
    let upstream = mock(vec![]).await;
    let scanner = mock(vec![Reply::error(status)]).await;
    let g = gateway(configured(&upstream, &scanner)).await;
    let response = post(&g, "/v1/chat/completions", request()).await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "moderation_unavailable"
    );
    assert!(upstream.calls.lock().unwrap().is_empty());
    let record = user(&g).await;
    assert_eq!(record["moderation"]["blocked_requests"], 0);
    assert_eq!(record["moderation"]["unavailable_requests"], 1);
    assert!(record["moderation"]["last_violation"].is_null());
}

#[tokio::test]
async fn timeout_releases_capacity_and_does_not_forward() {
    let upstream = mock(vec![Reply::json(completion())]).await;
    let scanner = mock(vec![
        Reply {
            delay: Duration::from_secs(2),
            ..safe()
        },
        safe(),
    ])
    .await;
    let mut config = configured(&upstream, &scanner);
    config.moderation.as_mut().unwrap().timeout_secs = 1;
    config.moderation.as_mut().unwrap().max_concurrent = 1;
    let g = gateway(config).await;
    assert_eq!(
        post(&g, "/v1/chat/completions", request()).await.status(),
        503
    );
    assert!(upstream.calls.lock().unwrap().is_empty());
    assert_eq!(
        post(&g, "/v1/chat/completions", request()).await.status(),
        200
    );
    let record = user(&g).await;
    assert_eq!(record["moderation"]["unavailable_requests"], 1);
    assert_eq!(record["moderation"]["allowed_requests"], 1);
}

#[tokio::test]
async fn rate_limit_survives_config_reload_and_user_records_survive_key_rotation_and_ack() {
    let upstream = mock(vec![]).await;
    let scanner = mock(vec![blocked(), safe()]).await;
    let mut config = configured(&upstream, &scanner);
    config.moderation.as_mut().unwrap().rpm = 1;
    let g = gateway(config).await;
    assert_eq!(
        post(&g, "/v1/chat/completions", request()).await.status(),
        403
    );
    assert_eq!(
        admin(
            &g,
            "PUT",
            "/admin/users/test-user",
            json!({"keys":[{"name":"rotated","key":"rotated-secret"}]})
        )
        .await
        .status(),
        200
    );
    let result = client()
        .post(format!("{}/v1/chat/completions", g.url))
        .bearer_auth("rotated-secret")
        .json(&request())
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 503);
    assert_eq!(scanner.calls.lock().unwrap().len(), 1);
    let record = user(&g).await;
    assert_eq!(record["moderation"]["blocked_requests"], 1);
    assert_eq!(record["moderation"]["unavailable_requests"], 1);
    let checkpoint: Value = admin(&g, "GET", "/admin/checkpoint", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(admin(&g,"POST","/admin/checkpoint/ack",json!({"boot_id":checkpoint["boot_id"],"generation":checkpoint["state"]["generation"],"sequence":checkpoint["state"]["sequence"]})).await.status(),200);
    assert_eq!(user(&g).await["moderation"], record["moderation"]);
    let restored = crate::vault::Vault::memory().unwrap();
    restored.require_manager();
    restored
        .initialize(serde_json::from_value(checkpoint["state"].clone()).unwrap())
        .unwrap();
    assert_eq!(
        restored.lock().unwrap().moderation["test-user"].blocked_requests,
        1
    );
}

#[rstest]
#[case(Provider::OpenAI, "/v1/chat/completions", "messages")]
#[case(Provider::OpenAI, "/v1/responses", "input")]
#[case(Provider::Anthropic, "/v1/messages", "messages")]
#[case(Provider::Gemini, "/v1beta/models/chat:generateContent", "contents")]
#[tokio::test]
async fn every_generation_protocol_runs_the_guard(
    #[case] provider: Provider,
    #[case] path: &str,
    #[case] field: &str,
) {
    let output = if path.ends_with("chat/completions") {
        completion()
    } else {
        native(provider)
    };
    let upstream = mock(vec![Reply::json(output)]).await;
    let scanner = mock(vec![blocked(), safe()]).await;
    let mut config = configured(&upstream, &scanner);
    config.models[0].deployments[0].provider = provider;
    let g = gateway(config).await;
    let body = json!({"model":"chat","messages":[{"role":"user","content":"hello"}],"max_tokens":32,
        "input":"hello", "contents":[{"role":"user","parts":[{"text":"hello"}]}]});
    assert_eq!(post(&g, path, body.clone()).await.status(), 403);
    assert!(upstream.calls.lock().unwrap().is_empty());
    assert_eq!(post(&g, path, body.clone()).await.status(), 200);
    assert_eq!(upstream.calls.lock().unwrap().len(), 1);
    let calls = scanner.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let input: Value = serde_json::from_str(calls[1].body["input"].as_str().unwrap()).unwrap();
    assert_eq!(input[field], body[field]);
}

#[tokio::test]
async fn stream_starts_only_after_input_passes_and_rejections_are_json_errors() {
    let event =
        json!({"id":"reply","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]});
    let upstream = mock(vec![Reply::sse(format!(
        "data: {event}\n\ndata: [DONE]\n\n"
    ))])
    .await;
    let scanner = mock(vec![blocked(), safe()]).await;
    let g = gateway(configured(&upstream, &scanner)).await;
    let mut body = request();
    body["stream"] = json!(true);
    let denied = post(&g, "/v1/chat/completions", body.clone()).await;
    assert_eq!(denied.status(), 403);
    assert_eq!(denied.headers()["content-type"], "application/json");
    assert!(upstream.calls.lock().unwrap().is_empty());
    let allowed = post(&g, "/v1/chat/completions", body).await;
    assert_eq!(allowed.status(), 200);
    assert!(
        allowed.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert!(allowed.text().await.unwrap().contains("ok"));
    assert_eq!(upstream.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unsupported_and_oversized_inputs_fail_without_scanning_or_user_violation() {
    let upstream = mock(vec![]).await;
    let scanner = mock(vec![]).await;
    let mut config = configured(&upstream, &scanner);
    config.moderation.as_mut().unwrap().max_input_bytes = 1024;
    let g = gateway(config).await;
    let mut long = request();
    long["messages"][0]["content"] = json!("x".repeat(2000));
    assert_eq!(post(&g, "/v1/chat/completions", long).await.status(), 413);
    assert_eq!(post(&g,"/v1/responses",json!({"model":"chat","input":[{"type":"message","content":[{"type":"input_image","image_url":"https://example.invalid/image"}]}]})).await.status(),400);
    assert!(upstream.calls.lock().unwrap().is_empty());
    assert!(scanner.calls.lock().unwrap().is_empty());
    let record = user(&g).await;
    assert_eq!(record["moderation"]["rejected_requests"], 2);
    assert_eq!(record["moderation"]["blocked_requests"], 0);
}

#[tokio::test]
async fn authentication_precedes_scanning_and_other_users_have_no_violation() {
    let upstream = mock(vec![]).await;
    let scanner = mock(vec![blocked()]).await;
    let g = gateway(configured(&upstream, &scanner)).await;
    let response = client()
        .post(format!("{}/v1/chat/completions", g.url))
        .json(&request())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(scanner.calls.lock().unwrap().is_empty());
    assert_eq!(
        post(&g, "/v1/chat/completions", request()).await.status(),
        403
    );
    assert_eq!(
        admin(
            &g,
            "POST",
            "/admin/users",
            json!({"id":"another","keys":[{"name":"another-key","key":"another-secret"}]})
        )
        .await
        .status(),
        200
    );
    let another: Value = admin(&g, "GET", "/admin/users/another", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(another["user"]["moderation"]["blocked_requests"], 0);
    assert!(
        another["user"]["moderation"]["recent_events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let list: Value = admin(&g, "GET", "/admin/users", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["id"] == "test-user")
            .unwrap()["moderation"]["blocked_requests"],
        1
    );
}

#[tokio::test]
async fn invalid_configuration_is_atomic_and_disabling_guard_preserves_records() {
    let upstream = mock(vec![Reply::json(completion())]).await;
    let scanner = mock(vec![blocked()]).await;
    let g = gateway(configured(&upstream, &scanner)).await;
    assert_eq!(
        post(&g, "/v1/chat/completions", request()).await.status(),
        403
    );
    for patch in [
        json!({"api_key_ref":"missing"}),
        json!({"timeout_secs":0}),
        json!({"rpm":0}),
        json!({"endpoint":"https://attacker.invalid"}),
        json!({"api_key_ref":"CLIENT_KEY"}),
    ] {
        assert_eq!(
            admin(
                &g,
                "POST",
                "/admin/apply",
                json!({"config":{"moderation":patch}})
            )
            .await
            .status(),
            400
        );
    }
    assert_eq!(
        admin(
            &g,
            "POST",
            "/admin/apply",
            json!({"config":{"moderation":null}})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        post(&g, "/v1/chat/completions", request()).await.status(),
        200
    );
    assert_eq!(scanner.calls.lock().unwrap().len(), 1);
    assert_eq!(user(&g).await["moderation"]["blocked_requests"], 1);
}

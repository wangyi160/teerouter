use super::*;
use teellm::vault::Vault;

async fn admin(g: &Running, path: &str) -> Value {
    client()
        .get(format!("{}{path}", g.url))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
async fn record(g: &Running) -> Value {
    admin(g, "/admin/users/test-user").await["user"]["refusals"].clone()
}
fn refused(provider: Provider, native: bool) -> Value {
    if provider == Provider::Anthropic {
        json!({"id":"msg-declined","type":"message","role":"assistant","model":"chat","content":[{"type":"text","text":"Provider declined this request."}],"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"Provider explanation","fallback_credit":"DO_NOT_RETAIN"},"usage":{"input_tokens":10,"output_tokens":5}})
    } else if native {
        json!({"id":"resp-declined","object":"response","model":"chat","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"Provider declined this request."}]}],"usage":{"input_tokens":10,"output_tokens":5}})
    } else {
        json!({"id":"chat-declined","object":"chat.completion","model":"chat","choices":[{"index":0,"message":{"role":"assistant","content":null,"refusal":"Provider declined this request."},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
    }
}
fn route(provider: Provider, native: bool) -> &'static str {
    if !native {
        "/v1/chat/completions"
    } else if provider == Provider::Anthropic {
        "/v1/messages"
    } else {
        "/v1/responses"
    }
}
fn input(stream: bool) -> Value {
    json!({"model":"chat","stream":stream,"max_tokens":64,"messages":[{"role":"user","content":"REQUEST_BODY_MUST_NOT_BE_SAVED"}]})
}
fn native_input(provider: Provider, native: bool, stream: bool) -> Value {
    if native && provider != Provider::Anthropic {
        json!({"model":"chat","input":"REQUEST_BODY_MUST_NOT_BE_SAVED","stream":stream})
    } else {
        input(stream)
    }
}
fn events(values: Vec<Value>, done: bool) -> Reply {
    Reply::sse(
        values
            .into_iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>()
            + if done { "data: [DONE]\n\n" } else { "" },
    )
}
fn refusal_stream(provider: Provider, native: bool) -> Reply {
    if provider == Provider::Anthropic {
        events(
            vec![
                json!({"type":"message_start","message":{"id":"msg-declined","model":"chat","usage":{"input_tokens":10}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Provider declined this request."}}),
                json!({"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"Provider explanation"}},"usage":{"output_tokens":5}}),
                json!({"type":"message_stop"}),
            ],
            false,
        )
    } else if native {
        events(
            vec![
                json!({"type":"response.created","response":{"id":"resp-declined","model":"chat"}}),
                json!({"type":"response.refusal.delta","output_index":0,"content_index":0,"delta":"Provider declined "}),
                json!({"type":"response.refusal.delta","output_index":0,"content_index":0,"delta":"this request."}),
                json!({"type":"response.refusal.done","output_index":0,"content_index":0,"refusal":"Provider declined this request."}),
                json!({"type":"response.completed","response":refused(provider,true)}),
            ],
            false,
        )
    } else {
        events(
            vec![
                json!({"id":"chat-declined","model":"chat","choices":[{"index":0,"delta":{"refusal":"Provider declined "},"finish_reason":null}]}),
                json!({"id":"chat-declined","choices":[{"index":0,"delta":{"refusal":"this request."},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
            ],
            true,
        )
    }
}

#[rstest]
#[case(Provider::OpenAI, false, false)]
#[case(Provider::OpenAI, false, true)]
#[case(Provider::OpenAI, true, false)]
#[case(Provider::OpenAI, true, true)]
#[case(Provider::XAI, false, false)]
#[case(Provider::XAI, false, true)]
#[case(Provider::XAI, true, false)]
#[case(Provider::XAI, true, true)]
#[case(Provider::Anthropic, false, false)]
#[case(Provider::Anthropic, false, true)]
#[case(Provider::Anthropic, true, false)]
#[case(Provider::Anthropic, true, true)]
#[tokio::test]
async fn refusal_is_forwarded_and_audited_once_before_any_protocol_conversion(
    #[case] provider: Provider,
    #[case] native: bool,
    #[case] stream: bool,
) {
    let mut reply = if stream {
        refusal_stream(provider, native)
    } else {
        Reply::json(refused(provider, native))
    };
    reply
        .headers
        .insert("x-request-id", "upstream-id".parse().unwrap());
    let upstream = mock(vec![reply]).await;
    let mut config = configuration(&upstream);
    config.models[0].deployments[0].provider = provider;
    let g = gateway(config).await;
    let response = post(
        &g,
        route(provider, native),
        native_input(provider, native, stream),
    )
    .await;
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("Provider declined") || (stream && body.contains("refusal")));
    let r = record(&g).await;
    assert_eq!(r["refused_requests"], 1);
    assert_eq!(r["recent_events"].as_array().unwrap().len(), 1);
    let event = &r["recent_events"][0];
    assert_eq!(event["request_id"], request_id);
    assert_eq!(event["key"], "user");
    assert_eq!(event["deployment"], "a");
    assert_eq!(event["attempts"], 1);
    assert_eq!(event["result"]["provider"], json!(provider));
    assert_eq!(event["result"]["upstream_request_id"], "upstream-id");
    assert_eq!(event["result"]["complete"], true);
    if provider == Provider::Anthropic {
        assert_eq!(event["result"]["stop_details"]["category"], "cyber");
        assert_eq!(
            event["result"]["response_text"],
            "Provider declined this request."
        );
        assert_eq!(event["result"]["protocol"], "messages");
    } else {
        assert_eq!(
            event["result"]["parts"][0]["refusal"],
            "Provider declined this request."
        );
    }
    assert_eq!(usage(&g).await["output_tokens"], 5);
    assert_eq!(upstream.calls.lock().unwrap().len(), 1);
    let state = admin(&g, "/admin/checkpoint").await.to_string();
    assert!(!state.contains("REQUEST_BODY_MUST_NOT_BE_SAVED") && !state.contains("DO_NOT_RETAIN"));
    assert_eq!(
        client()
            .get(format!("{}/admin/users/test-user", g.url))
            .bearer_auth("client-secret")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        admin(&g, "/admin/users").await["data"][0]["refusals"]["refused_requests"],
        1
    );
}

#[tokio::test]
async fn scanner_is_disabled_even_when_restoring_legacy_configuration_and_normal_text_is_not_flagged()
 {
    let scanner = mock(vec![]).await;
    let mut normal = completion();
    normal["choices"][0]["message"]["content"] =
        json!("I cannot help with that. refusal content_policy_violation");
    let upstream = mock(vec![Reply::json(normal)]).await;
    let mut config = configuration(&upstream);
    config.moderation = Some(crate::moderation::Config {
        api_key_ref: "MISSING_RETIRED_KEY".into(),
        test_endpoint: Some(format!("{}/v1/scan", scanner.running.url)),
        ..Default::default()
    });
    let g = gateway(config).await;
    let mut body = input(false);
    body["messages"][0]["content"] = json!("long agent context ".repeat(30_000));
    assert_eq!(post(&g, "/v1/chat/completions", body).await.status(), 200);
    assert_eq!(record(&g).await["refused_requests"], 0);
    assert_eq!(upstream.calls.lock().unwrap().len(), 1);
    assert!(scanner.calls.lock().unwrap().is_empty());
    let status = admin(&g, "/admin/status").await;
    assert_eq!(status["lockllm_enabled"], false);
    assert_eq!(status["refusal_audit_enabled"], true);
    assert!(
        admin(&g, "/admin/config").await["config"]
            .get("moderation")
            .is_none()
    );
}

#[tokio::test]
async fn precise_policy_errors_are_recorded_without_retrying_another_account() {
    let upstream=mock(vec![Reply {status:403,..Reply::json(json!({"error":{"code":"content_policy_violation","type":"invalid_request_error","message":"Provider policy explanation","request_body":"PRIVATE"}}))},Reply::json(completion())]).await;
    let g = gateway(configuration(&upstream)).await;
    assert_eq!(
        post(&g, "/v1/chat/completions", input(false))
            .await
            .status(),
        403
    );
    let r = record(&g).await;
    assert_eq!(r["refused_requests"], 1);
    assert_eq!(r["recent_events"][0]["result"]["upstream_status"], 403);
    assert_eq!(
        r["recent_events"][0]["result"]["error"]["message"],
        "Provider policy explanation"
    );
    assert!(!r.to_string().contains("PRIVATE"));
    assert_eq!(upstream.calls.lock().unwrap().len(), 1);
}

#[rstest]
#[case(400)]
#[case(403)]
#[case(429)]
#[tokio::test]
async fn ordinary_http_errors_are_not_refusals(#[case] status: u16) {
    let upstream = mock(vec![Reply::error(status)]).await;
    let mut config = configuration(&upstream);
    config.models[0].max_attempts = 1;
    let g = gateway(config).await;
    assert_ne!(
        post(&g, "/v1/chat/completions", input(false))
            .await
            .status(),
        200
    );
    assert_eq!(record(&g).await["refused_requests"], 0);
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn codex_chat_bridge_audits_done_only_refusals(#[case] stream: bool) {
    let upstream=mock(vec![events(vec![
        json!({"type":"response.created","response":{"id":"resp-codex"}}),
        json!({"type":"response.refusal.done","output_index":0,"content_index":0,"refusal":"Provider declined this request."}),
        json!({"type":"response.completed","response":{"id":"resp-codex","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":5}}}),
    ],false)]).await;
    let bundle = discovery::bundle(Provider::OpenAI, &[&upstream]);
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let g = run(server::app(Engine::from_vault(vault).unwrap())).await;
    let response = post(&g, "/v1/chat/completions", input(stream)).await;
    assert_eq!(response.status(), 200);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("Provider declined this request.")
    );
    let r = record(&g).await;
    assert_eq!(r["refused_requests"], 1);
    assert_eq!(r["recent_events"][0]["result"]["protocol"], "responses");
    assert_eq!(
        r["recent_events"][0]["result"]["parts"][0]["refusal"],
        "Provider declined this request."
    );
}

async fn write_admin(g: &Running, method: &str, path: &str, body: Value) -> reqwest::Response {
    client()
        .request(method.parse().unwrap(), format!("{}{path}", g.url))
        .bearer_auth("admin-secret")
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn bounded_user_history_and_pending_archive_survive_checkpoint_ack_and_restart() {
    let upstream = mock(vec![Reply::json(refused(Provider::OpenAI, false)); 24]).await;
    let config = configuration(&upstream);
    let g = gateway(config.clone()).await;
    for _ in 0..23 {
        let response = post(&g, "/v1/chat/completions", input(false)).await;
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
    }
    let r = record(&g).await;
    assert_eq!(r["refused_requests"], 23);
    assert_eq!(r["evicted_events"], 3);
    assert_eq!(r["recent_events"].as_array().unwrap().len(), 20);
    let mut saved = admin(&g, "/admin/checkpoint").await;
    assert_eq!(saved["state"]["events"].as_array().unwrap().len(), 23);
    assert!(
        saved["state"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["event"]["refusal"].is_object())
    );
    assert_eq!(write_admin(&g,"POST","/admin/checkpoint/ack",json!({"boot_id":saved["boot_id"],"generation":saved["state"]["generation"],"sequence":saved["state"]["sequence"]})).await.status(),200);
    saved = admin(&g, "/admin/checkpoint").await;
    assert!(saved["state"]["events"].as_array().unwrap().is_empty());
    // Old LockLLM counters remain history and do not become refusal counts.
    saved["state"]["moderation"] = json!({"test-user":{"blocked_requests":7}});
    let next = engine(config);
    next.vault.require_manager();
    let rebooted = run(server::app(next)).await;
    let empty = admin(&rebooted, "/admin/checkpoint").await;
    assert_eq!(
        write_admin(
            &rebooted,
            "POST",
            "/admin/initialize",
            json!({"boot_id":empty["boot_id"],"state":saved["state"]})
        )
        .await
        .status(),
        200
    );
    let checkpoint = admin(&rebooted, "/admin/checkpoint").await;
    assert_eq!(write_admin(&rebooted,"POST","/admin/checkpoint/ack",json!({"boot_id":checkpoint["boot_id"],"generation":checkpoint["state"]["generation"],"sequence":checkpoint["state"]["sequence"]})).await.status(),200);
    assert_eq!(record(&rebooted).await, r);
    assert_eq!(
        admin(&rebooted, "/admin/users/test-user").await["user"]["moderation"]["blocked_requests"],
        7
    );
    assert_eq!(
        write_admin(
            &rebooted,
            "PUT",
            "/admin/users/test-user",
            json!({"id":"test-user","refusals":{"refused_requests":0}})
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        post(&rebooted, "/v1/chat/completions", input(false))
            .await
            .status(),
        200
    );
    assert_eq!(record(&rebooted).await["refused_requests"], 24);
}

#[tokio::test]
async fn refusal_followed_by_a_broken_stream_is_retained_once() {
    let upstream = mock(vec![events(
        vec![
            json!({"id":"partial","choices":[{"index":0,"delta":{"refusal":"Partial refusal"}}]}),
            json!({"error":{"message":"stream failed"}}),
        ],
        false,
    )])
    .await;
    let g = gateway(configuration(&upstream)).await;
    let response = post(&g, "/v1/chat/completions", input(true)).await;
    assert_eq!(response.status(), 200);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("stream_interrupted")
    );
    let r = record(&g).await;
    assert_eq!(r["refused_requests"], 1);
    assert_eq!(r["recent_events"][0]["status"], 502);
    assert_eq!(r["recent_events"][0]["result"]["complete"], false);
    assert_eq!(
        r["recent_events"][0]["result"]["parts"][0]["refusal"],
        "Partial refusal"
    );
    assert_eq!(upstream.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn client_disconnect_settles_the_observed_partial_refusal() {
    let mut reply = events(
        vec![
            json!({"id":"cancelled","choices":[{"index":0,"delta":{"refusal":"Observed refusal"}}]}),
        ],
        false,
    );
    reply.tail = Some((Duration::from_secs(30), "data: [DONE]\n\n".into()));
    let upstream = mock(vec![reply]).await;
    let g = gateway(configuration(&upstream)).await;
    let mut response = post(&g, "/v1/chat/completions", input(true)).await;
    assert_eq!(response.status(), 200);
    assert!(
        String::from_utf8_lossy(&response.chunk().await.unwrap().unwrap())
            .contains("Observed refusal")
    );
    drop(response);
    let r = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let r = record(&g).await;
            if r["refused_requests"] == 1 {
                break r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(r["recent_events"][0]["status"], 499);
    assert_eq!(r["recent_events"][0]["result"]["complete"], false);
    assert_eq!(
        r["recent_events"][0]["result"]["parts"][0]["refusal"],
        "Observed refusal"
    );
}

#[tokio::test]
async fn stream_refusal_before_first_client_frame_still_prevents_failover() {
    let upstream=mock(vec![events(vec![
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":"Declined"}}),
        json!({"type":"error","error":{"message":"unexpected stream end"}}),
    ],false),Reply::json(completion())]).await;
    let bundle = discovery::bundle(Provider::OpenAI, &[&upstream]);
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let g = run(server::app(Engine::from_vault(vault).unwrap())).await;
    let response = post(&g, "/v1/chat/completions", input(false)).await;
    assert_eq!(response.status(), 403);
    assert_eq!(record(&g).await["refused_requests"], 1);
    assert_eq!(upstream.calls.lock().unwrap().len(), 1);
}

#[cfg(feature = "workbuddy-lab")]
#[tokio::test]
async fn workbuddy_refusal_is_audited_with_the_same_user_permissions() {
    let upstream = mock(vec![Reply::json(refused(Provider::Workbuddy, false))]).await;
    let mut bundle = discovery::bundle(Provider::Workbuddy, &[&upstream]);
    bundle.accounts.get_mut("account-0").unwrap().credential =
        json!({"api_key":"workbuddy-test-secret"});
    bundle.config.models[0].deployments[0].api_base = None;
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let g = run(server::app(Engine::from_vault(vault).unwrap())).await;
    assert_eq!(
        post(&g, "/v1/chat/completions", input(false))
            .await
            .status(),
        200
    );
    assert_eq!(
        record(&g).await["recent_events"][0]["result"]["provider"],
        "workbuddy"
    );
}

#[tokio::test]
async fn user_audit_aggregates_keys_survives_rotation_and_does_not_mix_users() {
    let upstream = mock(vec![Reply::json(refused(Provider::OpenAI, false)); 4]).await;
    let g = gateway(configuration(&upstream)).await;
    assert_eq!(write_admin(&g, "POST", "/admin/users", json!({
        "id":"second", "max_concurrent":2,
        "keys":[{"name":"second-a","key":"second-secret-a"},{"name":"second-b","key":"second-secret-b"}]
    })).await.status(), 200);
    let generate = |key: &str| {
        client()
            .post(format!("{}/v1/chat/completions", g.url))
            .bearer_auth(key)
            .json(&input(false))
            .send()
    };
    assert_eq!(generate("second-secret-a").await.unwrap().status(), 200);
    assert_eq!(generate("second-secret-b").await.unwrap().status(), 200);
    assert_eq!(record(&g).await["refused_requests"], 0);
    let before = admin(&g, "/admin/users/second").await["user"]["refusals"].clone();
    assert_eq!(before["refused_requests"], 2);
    assert_eq!(before["recent_events"][0]["key"], "second-a");
    assert_eq!(before["recent_events"][1]["key"], "second-b");
    assert_eq!(
        write_admin(
            &g,
            "PUT",
            "/admin/users/second",
            json!({
                "keys":[{"name":"second-a","key":"rotated-secret"}]
            })
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        admin(&g, "/admin/users/second").await["user"]["refusals"],
        before
    );
    assert_eq!(generate("second-secret-a").await.unwrap().status(), 401);
    assert_eq!(generate("rotated-secret").await.unwrap().status(), 200);
    assert_eq!(
        post(&g, "/v1/chat/completions", input(false))
            .await
            .status(),
        200
    );
    assert_eq!(record(&g).await["refused_requests"], 1);
    assert_eq!(
        admin(&g, "/admin/users/second").await["user"]["refusals"]["refused_requests"],
        3
    );
    assert_eq!(upstream.calls.lock().unwrap().len(), 4);
}

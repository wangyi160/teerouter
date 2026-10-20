use super::*;

#[tokio::test]
async fn inferred_conversation_reuses_account_and_new_conversations_balance() {
    let a = mock(vec![Reply::json(completion()); 5]).await;
    let b = mock(vec![Reply::json(completion()); 5]).await;
    let mut config = configuration(&a);
    let mut second = config.models[0].deployments[0].clone();
    second.id = "b".into();
    second.api_base = Some(format!("{}/v1", b.running.url));
    second.proxy = Some(b.running.url.clone());
    config.models[0].deployments.push(second);
    let gateway = gateway(config).await;
    let mut body = request();
    for _ in 0..3 {
        let response = post(&gateway, "/v1/chat/completions", body.clone()).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-router-deployment"], "a");
        response.bytes().await.unwrap();
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":"answer"}),
            json!({"role":"user","content":"next"}),
        ]);
    }
    let mut other = request();
    other["messages"][0]["content"] = "different conversation".into();
    let response = post(&gateway, "/v1/chat/completions", other).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-deployment"], "b");
    response.bytes().await.unwrap();
    let calls = a.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[0].body["prompt_cache_key"],
        calls[2].body["prompt_cache_key"]
    );
    assert_eq!(
        calls[0].headers["session_id"],
        calls[2].headers["session_id"]
    );
    assert_eq!(b.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn soft_affinity_fails_over_and_keeps_the_replacement_account() {
    let a = mock(vec![Reply::json(completion()), Reply::error(429)]).await;
    let b = mock(vec![Reply::json(completion()); 3]).await;
    let mut config = configuration(&a);
    let mut second = config.models[0].deployments[0].clone();
    second.id = "b".into();
    second.api_base = Some(format!("{}/v1", b.running.url));
    second.proxy = Some(b.running.url.clone());
    config.models[0].deployments.push(second);
    let gateway = gateway(config).await;
    for expected in ["a", "b", "b"] {
        let response = post(&gateway, "/v1/chat/completions", request()).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-router-deployment"], expected);
        response.bytes().await.unwrap();
    }
    assert_eq!(a.calls.lock().unwrap().len(), 2);
    assert_eq!(b.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn agent_session_header_survives_compaction_without_being_forwarded_raw() {
    let a = mock(vec![Reply::json(completion()); 3]).await;
    let b = mock(vec![Reply::json(completion()); 3]).await;
    let mut config = configuration(&a);
    let mut second = config.models[0].deployments[0].clone();
    second.id = "b".into();
    second.api_base = Some(format!("{}/v1", b.running.url));
    second.proxy = Some(b.running.url.clone());
    config.models[0].deployments.push(second);
    let gateway = gateway(config).await;
    for content in ["original context", "compacted context", "new context"] {
        let response = client()
            .post(format!("{}/v1/chat/completions", gateway.url))
            .bearer_auth("client-secret")
            .header("session_id", "agent-conversation-1")
            .json(&json!({"model":"chat","messages":[{"role":"user","content":content}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
    }
    let calls = a.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_ne!(calls[0].headers["session_id"], "agent-conversation-1");
    assert_eq!(
        calls[0].headers["session_id"],
        calls[2].headers["session_id"]
    );
    assert!(b.calls.lock().unwrap().is_empty());
}

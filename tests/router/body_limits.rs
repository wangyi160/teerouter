use super::*;

fn account_config(mock: &Mock, oauth: bool, dir: &std::path::Path) -> Config {
    let mut config = configuration(mock);
    if oauth {
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            json!({"access_token":"fixture-access","expires_at":4102444800u64,"account_id":"fixture-account"})
                .to_string(),
        )
        .unwrap();
        config.models[0].deployments[0].api_key_env.clear();
        config.models[0].deployments[0].oauth_file = Some(path);
    }
    config
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn forwards_image_payload_above_old_body_limit(#[case] oauth: bool) {
    let mock = mock(vec![Reply::json(native(Provider::OpenAI))]).await;
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(account_config(&mock, oauth, dir.path())).await;
    // Synthetic image data exercises byte-for-byte forwarding, not image decoding.
    let image = format!("data:image/png;base64,{}", "A".repeat(16 * 1024 * 1024));
    let body = json!({
        "model":"chat",
        "input":[{"role":"user","content":[
            {"type":"input_text","text":"Describe this image"},
            {"type":"input_image","image_url":image,"detail":"original"}
        ]}],
        "store":false
    });
    let encoded = serde_json::to_vec(&body).unwrap();
    assert!(encoded.len() > 16 * 1024 * 1024);
    let response = client()
        .post(format!("{}/v1/responses", gateway.url))
        .bearer_auth("client-secret")
        .header("content-type", "application/json")
        .timeout(Duration::from_secs(60))
        .body(encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body, body);
    assert_eq!(calls[0].path, "/v1/responses");
    assert_eq!(
        calls[0].headers["authorization"],
        if oauth {
            "Bearer fixture-access"
        } else {
            "Bearer provider-secret"
        }
    );
    assert_eq!(calls[0].headers.contains_key("chatgpt-account-id"), oauth);
}

#[rstest]
#[case(1024, 200)]
#[case(1025, 413)]
#[tokio::test]
async fn body_limit_checks_received_bytes_at_boundary(
    #[case] size: usize,
    #[case] status: u16,
    #[values(false, true)] chunked: bool,
) {
    let mock = mock(vec![Reply::json(native(Provider::OpenAI))]).await;
    let mut config = configuration(&mock);
    config.server.max_body_bytes = 1024;
    let gateway = gateway(config).await;
    let body = json!({"model":"chat","input":"hello"});
    let mut encoded = serde_json::to_vec(&body).unwrap();
    encoded.resize(size, b' ');
    let body = if chunked {
        let chunks: Vec<_> = encoded
            .chunks(127)
            .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
            .collect();
        reqwest::Body::wrap_stream(futures_util::stream::iter(chunks))
    } else {
        reqwest::Body::from(encoded)
    };
    let response = client()
        .post(format!("{}/v1/responses", gateway.url))
        .bearer_auth("client-secret")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), status);
    if status == 413 {
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["code"], "body_too_large");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Router")
        );
        assert!(mock.calls.lock().unwrap().is_empty());
    } else {
        assert_eq!(mock.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn body_read_failure_is_not_reported_as_too_large() {
    let mock = mock(vec![]).await;
    let runtime = crate::runtime::Runtime::merchant(engine(configuration(&mock)));
    let mut app = server::app_runtime(runtime);
    let body = Body::from_stream(futures_util::stream::iter([
        Ok(bytes::Bytes::from_static(b"{\"model\":")),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "fixture upload failed",
        )),
    ]));
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", "Bearer client-secret")
        .header("user-agent", agent_ua(Provider::OpenAI))
        .body(body)
        .unwrap();
    let response = tower_service::Service::call(&mut app, request)
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["code"], "body_read_error");
    assert!(mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn body_timeout_remains_distinct_from_read_failure() {
    let mock = mock(vec![]).await;
    let mut config = configuration(&mock);
    config.server.body_timeout_secs = 1;
    let runtime = crate::runtime::Runtime::merchant(engine(config));
    let mut app = server::app_runtime(runtime);
    let body = Body::from_stream(futures_util::stream::pending::<
        Result<bytes::Bytes, std::io::Error>,
    >());
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", "Bearer client-secret")
        .header("user-agent", agent_ua(Provider::OpenAI))
        .body(body)
        .unwrap();
    let response = tower_service::Service::call(&mut app, request)
        .await
        .unwrap();
    assert_eq!(response.status(), 408);
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["code"], "body_timeout");
    assert!(mock.calls.lock().unwrap().is_empty());
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn upstream_body_limit_preserves_413_without_retry_or_disabling_account(#[case] oauth: bool) {
    let mock = mock(vec![
        Reply::error(413),
        Reply::json(native(Provider::OpenAI)),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(account_config(&mock, oauth, dir.path())).await;
    let body = json!({"model":"chat","input":"hello"});
    let response = post(&gateway, "/v1/responses", body.clone()).await;
    assert_eq!(response.status(), 413);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "upstream_body_too_large");
    assert!(!error.to_string().contains("DO_NOT_LEAK"));
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
    assert_eq!(post(&gateway, "/v1/responses", body).await.status(), 200);
    assert_eq!(mock.calls.lock().unwrap().len(), 2);
}

use super::*;
use teellm::vault::Vault;

async fn admin(
    gateway: &Running,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> reqwest::Response {
    client()
        .request(method, format!("{}{path}", gateway.url))
        .bearer_auth("admin-secret")
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn apply(gateway: &Running, body: Value) -> reqwest::Response {
    admin(gateway, reqwest::Method::POST, "/admin/apply", body).await
}
async fn oauth_gateway(proxy: &Mock) -> (Running, Arc<Vault>) {
    let mut bundle = discovery::bundle(Provider::Anthropic, &[proxy]);
    bundle.config.models[0].discover = false;
    bundle.config.models[0].name = "chat".into();
    bundle.config.keys[0].models.clear();
    bundle.config.models[0].deployments[0].model = "actual-model".into();
    let v = Vault::memory().unwrap();
    v.provision(&bundle).unwrap();
    (run_fixture(Engine::from_vault(v.clone()).unwrap()).await, v)
}
fn message() -> Value {
    json!({"model":"chat","max_tokens":10,"messages":[{"role":"user","content":"hello"}]})
}

#[tokio::test]
async fn credential_read_is_admin_only_uncached_and_returns_current_account_record() {
    let proxy = mock(vec![]).await;
    let (gateway, vault) = oauth_gateway(&proxy).await;
    let path = "/admin/accounts/account-0/credential";
    for token in ["", "client-secret"] {
        let response = client()
            .get(format!("{}{path}", gateway.url))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    let before = vault.account("account-0", Provider::Anthropic).unwrap();
    let response = admin(&gateway, reqwest::Method::GET, path, json!(null)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let record: Value = response.json().await.unwrap();
    assert_eq!(record["credential"], before.0);
    assert_eq!(record["revision"], before.1);
    assert!(record.get("proxy").is_none());
    let refreshed = json!({"access_token":"sk-ant-oat-DISPLAY_NEW", "expires_at":4102444800u64});
    vault
        .put_account("account-0", Provider::Anthropic, &refreshed)
        .unwrap();
    let record: Value = admin(&gateway, reqwest::Method::GET, path, json!(null))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(record["credential"], refreshed);
    assert_eq!(record["revision"], before.1 + 1);
    for path in ["/admin/accounts", "/admin/config", "/admin/logs?admin=true"] {
        let body = admin(&gateway, reqwest::Method::GET, path, json!(null))
            .await
            .text()
            .await
            .unwrap();
        assert!(!body.contains("DISPLAY_NEW") && !body.contains("REFRESH_JSON_SENTINEL"));
    }
    assert_eq!(
        admin(
            &gateway,
            reqwest::Method::GET,
            "/admin/accounts/missing/credential",
            json!(null)
        )
        .await
        .status(),
        404
    );
    assert!(proxy.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn proxy_secret_bindings_persist_rotate_and_survive_engine_restart() {
    let first = mock(vec![]).await;
    let second = mock(vec![
        Reply::json(native(Provider::Anthropic)),
        Reply::json(native(Provider::Anthropic)),
    ])
    .await;
    let (gateway, vault) = oauth_gateway(&first).await;
    let result=apply(&gateway,json!({"secrets":{"proxy-profile":second.running.url},"proxy_bindings":{"account-0":"proxy-profile"}})).await;
    assert_eq!(result.status(), 200, "{}", result.text().await.unwrap());
    assert_eq!(
        vault
            .account_proxy("account-0", Provider::Anthropic)
            .unwrap(),
        second.running.url
    );
    assert_eq!(
        post(&gateway, "/v1/messages", message()).await.status(),
        200
    );
    drop(gateway);
    let reopened = run_fixture(Engine::from_vault(vault).unwrap()).await;
    assert_eq!(
        post(&reopened, "/v1/messages", message()).await.status(),
        200
    );
    assert_eq!(second.calls.lock().unwrap().len(), 2);
    assert!(first.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn atomic_live_updates_validate_before_commit_and_rotate_admin_and_client_keys() {
    let proxy = mock(vec![Reply::json(completion()), Reply::json(completion())]).await;
    let gateway = gateway(configuration(&proxy)).await;
    assert_eq!(
        post(&gateway, "/admin/apply", json!({})).await.status(),
        401
    );
    let initial: Value = admin(&gateway, reqwest::Method::GET, "/admin/config", json!(null))
        .await
        .json()
        .await
        .unwrap();
    let revision = initial["revision"].as_i64().unwrap();
    assert_eq!(
        initial["config"]["models"][0]["deployments"][0]["proxy"],
        "[redacted]"
    );
    assert!(!initial.to_string().contains("client-secret"));
    let bad = apply(&gateway,json!({"expected_revision":revision,"config":{"server":{"max_ingress":0}},"secrets":{"CLIENT_KEY":"bad-change"}})).await;
    assert_eq!(bad.status(), 400);
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        200
    );
    assert_eq!(
        apply(
            &gateway,
            json!({"expected_revision":revision-1,"config":{"server":{"max_ingress":2}}})
        )
        .await
        .status(),
        409
    );
    let updated = apply(&gateway,json!({"expected_revision":revision,"config":{"server":{"max_ingress":2}},"secrets":{"CLIENT_KEY":"new-client","ADMIN_KEY":"new-admin"}})).await;
    assert_eq!(updated.status(), 200, "{}", updated.text().await.unwrap());
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        401
    );
    assert_eq!(apply(&gateway, json!({})).await.status(), 401);
    let response = client()
        .post(format!("{}/v1/chat/completions", gateway.url))
        .header("user-agent", gateway.agent)
        .bearer_auth("new-client")
        .json(&request())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let logs: Value = client()
        .get(format!("{}/admin/logs?admin=true", gateway.url))
        .bearer_auth("new-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(logs["data"].as_array().unwrap().len(), 1);
    assert!(!logs.to_string().contains("new-client") && !logs.to_string().contains("new-admin"));
}

#[tokio::test]
async fn proxy_and_credentials_change_live_and_accounts_can_be_disabled_and_deleted_atomically() {
    let first = mock(vec![Reply::json(native(Provider::Anthropic))]).await;
    let second = mock(vec![Reply::json(native(Provider::Anthropic))]).await;
    let (gateway, _) = oauth_gateway(&first).await;
    assert_eq!(
        post(&gateway, "/v1/messages", message()).await.status(),
        200
    );
    let updated = admin(&gateway,reqwest::Method::PUT,"/admin/accounts/account-0",json!({"proxy":second.running.url,"credential":{"access_token":"sk-ant-oat-REPLACEMENT_PRIVATE","expires_at":4102444800u64}})).await;
    assert_eq!(updated.status(), 200, "{}", updated.text().await.unwrap());
    assert_eq!(
        post(&gateway, "/v1/messages", message()).await.status(),
        200
    );
    assert_eq!(first.calls.lock().unwrap().len(), 1);
    assert_eq!(
        second.calls.lock().unwrap()[0].headers["authorization"],
        "Bearer sk-ant-oat-REPLACEMENT_PRIVATE"
    );
    assert_eq!(
        admin(
            &gateway,
            reqwest::Method::PUT,
            "/admin/accounts/account-0",
            json!({"enabled":false})
        )
        .await
        .status(),
        200
    );
    assert_ne!(
        post(&gateway, "/v1/messages", message()).await.status(),
        200
    );
    let accounts: Value = admin(
        &gateway,
        reqwest::Method::GET,
        "/admin/accounts",
        json!(null),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(accounts["data"][0]["enabled"], false);
    assert!(!accounts.to_string().contains("REPLACEMENT_PRIVATE"));
    assert_eq!(
        admin(
            &gateway,
            reqwest::Method::DELETE,
            "/admin/accounts/account-0",
            json!(null)
        )
        .await
        .status(),
        400
    );
    assert_eq!(apply(&gateway,json!({"config":{"server":{"allow_empty_routes":true},"models":[{"name":"chat","deployments":[]}]},"delete_accounts":["account-0"]})).await.status(),200);
    let accounts: Value = admin(
        &gateway,
        reqwest::Method::GET,
        "/admin/accounts",
        json!(null),
    )
    .await
    .json()
    .await
    .unwrap();
    assert!(accounts["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn live_changes_preserve_inflight_requests_and_existing_rate_limits() {
    let proxy = mock(vec![Reply {
        delay: Duration::from_millis(400),
        ..Reply::json(completion())
    }])
    .await;
    let mut config = configuration(&proxy);
    config.keys[0].rpm = 1;
    let gateway = gateway(config).await;
    let url = format!("{}/v1/chat/completions", gateway.url);
    let request_task = tokio::spawn(async move {
        client()
            .post(url)
            .bearer_auth("client-secret")
            .json(&request())
            .send()
            .await
            .unwrap()
            .status()
    });
    proxy.called.notified().await;
    assert_eq!(
        apply(&gateway, json!({"config":{"server":{"max_concurrent":1}}}))
            .await
            .status(),
        200
    );
    assert_eq!(request_task.await.unwrap(), 200);
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        429
    );
    assert_eq!(proxy.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn same_oauth_account_shares_rate_limits_across_aliases_and_generations() {
    let proxy = mock(vec![Reply::json(native(Provider::Anthropic))]).await;
    let mut bundle = discovery::bundle(Provider::Anthropic, &[&proxy]);
    bundle.config.keys[0].models.clear();
    let group = &mut bundle.config.models[0];
    group.discover = false;
    group.name = "chat".into();
    group.deployments[0].model = "actual-model".into();
    group.deployments[0].rpm = 1;
    let mut other = group.clone();
    other.name = "other".into();
    other.deployments[0].id = "other".into();
    bundle.config.models.push(other);
    let v = Vault::memory().unwrap();
    v.provision(&bundle).unwrap();
    let gateway = run_fixture(Engine::from_vault(v).unwrap()).await;
    assert_eq!(
        post(&gateway, "/v1/messages", message()).await.status(),
        200
    );
    assert_eq!(
        apply(
            &gateway,
            json!({"config":{"operations":{"log_retention_days":14}}})
        )
        .await
        .status(),
        200
    );
    let mut next = message();
    next["model"] = "other".into();
    assert_eq!(post(&gateway, "/v1/messages", next).await.status(), 503);
    assert_eq!(proxy.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_sessions_and_known_continuations_pin_the_original_account() {
    let a = mock(vec![Reply::json(native(Provider::OpenAI)); 4]).await;
    let b = mock(vec![Reply::json(native(Provider::OpenAI)); 4]).await;
    let mut config = configuration(&a);
    let mut second = config.models[0].deployments[0].clone();
    second.id = "b".into();
    second.api_base = Some(format!("{}/v1", b.running.url));
    second.proxy = Some(b.running.url.clone());
    config.models[0].deployments.push(second);
    let mut other = config.keys[0].clone();
    other.name = "other-client".into();
    other.key_env = "OTHER_KEY".into();
    config.keys.push(other);
    let gateway = gateway(config).await;
    for _ in 0..2 {
        let response = client()
            .post(format!("{}/v1/responses", gateway.url))
            .header("user-agent", gateway.agent)
            .bearer_auth("client-secret")
            .header("x-router-session", "session-A")
            .json(&json!({"model":"chat","input":"hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    assert_eq!(
        post(
            &gateway,
            "/v1/responses",
            json!({"model":"chat","input":"next","previous_response_id":"reply"})
        )
        .await
        .status(),
        200
    );
    assert_eq!(a.calls.lock().unwrap().len(), 3);
    assert_eq!(b.calls.lock().unwrap().len(), 0);
    assert!(
        !a.calls.lock().unwrap()[0]
            .headers
            .contains_key("x-router-session")
    );
    let foreign = client()
        .post(format!("{}/v1/responses", gateway.url))
        .header("user-agent", gateway.agent)
        .bearer_auth("provider-secret")
        .json(&json!({"model":"chat","input":"next","previous_response_id":"reply"}))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 409);
    let conflict = client()
        .post(format!("{}/v1/responses", gateway.url))
        .header("user-agent", gateway.agent)
        .bearer_auth("client-secret")
        .header("x-router-account", "b")
        .json(&json!({"model":"chat","input":"next","previous_response_id":"reply"}))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);
}

#[tokio::test]
async fn compatibility_updates_change_allowed_headers_and_paths_but_cannot_redirect_oauth_secrets()
{
    let proxy = mock(vec![Reply::json(native(Provider::Anthropic))]).await;
    let (gateway, _) = oauth_gateway(&proxy).await;
    let update=apply(&gateway,json!({"config":{"compatibility":{"anthropic":{"headers":{"anthropic-version":"2099-01-01"},"paths":{"messages":"future/messages"}}}}})).await;
    assert_eq!(update.status(), 200, "{}", update.text().await.unwrap());
    assert_eq!(
        post(&gateway, "/v1/messages", message()).await.status(),
        200
    );
    {
        let calls = proxy.calls.lock().unwrap();
        assert_eq!(calls[0].path, "/v1/future/messages");
        assert_eq!(calls[0].headers["anthropic-version"], "2099-01-01");
    }
    assert_eq!(
        apply(
            &gateway,
            json!({"config":{"compatibility":{"anthropic":{"headers":{"authorization":"bad"}}}}})
        )
        .await
        .status(),
        400
    );
    assert_eq!(apply(&gateway,json!({"config":{"compatibility":{"openai":{"token_url":"https://attacker.invalid/token"}}}})).await.status(),400);
    assert_eq!(
        apply(
            &gateway,
            json!({"config":{"compatibility":{"anthropic":{"paths":{"messages":"../secrets"}}}}})
        )
        .await
        .status(),
        400
    );
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn upstream_json_and_sse_sizes_are_bounded(#[case] streaming: bool) {
    let reply = if streaming {
        Reply::sse(format!("data: {}\n\n", "x".repeat(600)))
    } else {
        Reply::json(json!({"data":"x".repeat(2000)}))
    };
    let proxy = mock(vec![reply]).await;
    let mut config = configuration(&proxy);
    config.models[0].max_attempts = 1;
    config.server.max_response_bytes = 1024;
    config.server.max_event_bytes = 512;
    let gateway = gateway(config).await;
    let mut body = request();
    body["stream"] = streaming.into();
    assert_eq!(
        post(&gateway, "/v1/chat/completions", body).await.status(),
        502
    );
}

#[tokio::test]
async fn streaming_idle_timeout_terminates_and_releases_capacity() {
    let reply=Reply{tail:Some((Duration::from_secs(3),"event: response.completed\ndata: {}\n\n".into())),..Reply::sse("event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"one\"}}\n\n".into())};
    let proxy = mock(vec![reply, Reply::json(native(Provider::OpenAI))]).await;
    let mut config = configuration(&proxy);
    config.server.stream_idle_secs = 1;
    config.server.max_ingress = 1;
    let gateway = gateway(config).await;
    let response = post(
        &gateway,
        "/v1/responses",
        json!({"model":"chat","input":"hello","stream":true}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("error"));
    assert_eq!(
        post(
            &gateway,
            "/v1/responses",
            json!({"model":"chat","input":"hello"})
        )
        .await
        .status(),
        200
    );
}

#[tokio::test]
async fn request_admission_happens_before_reading_slow_bodies() {
    use tokio::io::AsyncWriteExt;
    let proxy = mock(vec![]).await;
    let mut config = configuration(&proxy);
    config.server.max_ingress = 1;
    let gateway = gateway(config).await;
    let addr = gateway.url.strip_prefix("http://").unwrap();
    let mut connection = tokio::net::TcpStream::connect(addr).await.unwrap();
    connection.write_all(format!("POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer client-secret\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{{").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        503
    );
    assert_eq!(
        client()
            .get(format!("{}/health/live", gateway.url))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(proxy.calls.lock().unwrap().is_empty());
}

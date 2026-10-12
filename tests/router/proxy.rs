use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use teellm::vault::{Provision, Vault};

fn bundle(provider: Provider, proxies: &[String], base: &str) -> Provision {
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let mut accounts = serde_json::Map::new();
    let mut deployments = Vec::new();
    for (i, proxy) in proxies.iter().enumerate() {
        let name = format!("account-{i}");
        accounts.insert(name.clone(), json!({"provider":provider,"proxy":proxy,
            "credential":{"access_token":format!("sk-ant-oat-token-{i}"),"expires_at":expiry,"expiry_date":expiry*1000}}));
        deployments.push(json!({"id":name,"provider":provider,"model":"actual-model","oauth_account":name,"api_base":base}));
    }
    serde_json::from_value(json!({"config":{
        "server":{"proxy":"http://127.0.0.1:9","no_proxy":"*"},
        "users":[{"id":"test-user"}],"keys":[{"name":"user","user_id":"test-user","key_ref":"client"}],
        "models":[{"name":"chat","max_attempts":2,"retry_delay_ms":0,"deployments":deployments}]},
        "secrets":{"client":"client-secret"},"accounts":accounts}))
    .unwrap()
}

fn replies(provider: Provider) -> Vec<Reply> {
    match provider {
        Provider::OpenAI => vec![Reply::sse(format!(
            "data: {}\n\n",
            json!({"type":"response.completed","response":native(provider)})
        ))],
        _ => vec![Reply::json(native(provider))],
    }
}

async fn generate(gateway: &Running, provider: Provider) -> reqwest::Response {
    let (path, body) = match provider {
        Provider::Anthropic => (
            "/v1/messages",
            json!({"model":"chat","max_tokens":8,"messages":[{"role":"user","content":"hello"}]}),
        ),
        _ => ("/v1/responses", json!({"model":"chat","input":"hello"})),
    };
    post(gateway, path, body).await
}

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::Anthropic)]
#[case(Provider::XAI)]
#[tokio::test]
async fn each_account_uses_its_own_authenticated_proxy_for_every_request(
    #[case] provider: Provider,
) {
    let proxies = [mock(replies(provider)).await, mock(replies(provider)).await];
    let urls: Vec<_> = proxies
        .iter()
        .enumerate()
        .map(|(i, p)| {
            p.running
                .url
                .replacen("http://", &format!("http://user-{i}:pass%3A{i}@"), 1)
        })
        .collect();
    let vault = Vault::memory().unwrap();
    vault
        .provision(&bundle(
            provider,
            &urls,
            "http://upstream.fixture.invalid/v1",
        ))
        .unwrap();
    let gateway = run(server::app(Engine::from_vault(vault).unwrap())).await;
    for _ in 0..2 {
        let response = generate(&gateway, provider).await;
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    for (i, proxy) in proxies.iter().enumerate() {
        let calls = proxy.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        for call in calls.iter() {
            assert!(call.uri.starts_with("http://upstream.fixture.invalid/"));
            assert_eq!(
                call.headers["authorization"],
                format!("Bearer sk-ant-oat-token-{i}")
            );
            assert_eq!(
                call.headers["proxy-authorization"],
                format!("Basic {}", STANDARD.encode(format!("user-{i}:pass:{i}")))
            );
            assert!(!call.headers.contains_key("x-api-key"));
        }
    }
}

#[tokio::test]
async fn dead_proxy_never_contacts_reachable_origin_and_failover_uses_next_binding() {
    let origin = mock(vec![Reply::json(native(Provider::XAI))]).await;
    let good = mock(vec![Reply::json(native(Provider::XAI))]).await;
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let base = format!("{}/v1", origin.running.url);
    let vault = Vault::memory().unwrap();
    vault
        .provision(&bundle(
            Provider::XAI,
            &[dead.clone(), good.running.url.clone()],
            &base,
        ))
        .unwrap();
    let gateway = run(server::app(Engine::from_vault(vault).unwrap())).await;
    let response = generate(&gateway, Provider::XAI).await;
    assert_eq!(response.status(), 200);
    // The dead account is excluded by catalog admission before generation.
    assert_eq!(response.headers()["x-router-attempts"], "1");
    assert!(origin.calls.lock().unwrap().is_empty());
    assert_eq!(
        good.calls.lock().unwrap()[0].headers["authorization"],
        "Bearer sk-ant-oat-token-1"
    );

    let vault = Vault::memory().unwrap();
    vault
        .provision(&bundle(Provider::XAI, &[dead], &base))
        .unwrap();
    let gateway = run(server::app(Engine::from_vault(vault).unwrap())).await;
    assert!(
        generate(&gateway, Provider::XAI)
            .await
            .status()
            .is_server_error()
    );
    assert!(origin.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn changing_binding_blocks_old_clients_until_restart_and_preserves_tokens() {
    let old = mock(vec![Reply::json(native(Provider::XAI))]).await;
    let new = mock(vec![Reply::json(native(Provider::XAI))]).await;
    let vault = Vault::memory().unwrap();
    vault
        .provision(&bundle(
            Provider::XAI,
            std::slice::from_ref(&old.running.url),
            "http://upstream.fixture.invalid/v1",
        ))
        .unwrap();
    let gateway = run(server::app(Engine::from_vault(vault.clone()).unwrap())).await;
    vault
        .set_account_proxy("account-0", Provider::XAI, &new.running.url)
        .unwrap();
    assert!(
        generate(&gateway, Provider::XAI)
            .await
            .status()
            .is_server_error()
    );
    assert!(old.calls.lock().unwrap().is_empty());
    assert!(new.calls.lock().unwrap().is_empty());
    let restarted = run(server::app(Engine::from_vault(vault).unwrap())).await;
    assert_eq!(generate(&restarted, Provider::XAI).await.status(), 200);
    assert_eq!(
        new.calls.lock().unwrap()[0].headers["authorization"],
        "Bearer sk-ant-oat-token-0"
    );
}

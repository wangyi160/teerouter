use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    http::HeaderMap,
    routing::post,
};
use rstest::rstest;
use std::sync::atomic::{AtomicUsize, Ordering};

fn jwt(exp: f64, account: &str) -> String {
    format!(
        "e30.{}.test",
        URL_SAFE_NO_PAD.encode(
            json!({"exp":exp,"https://api.openai.com/auth":{"chatgpt_account_id":account}})
                .to_string()
        )
    )
}

#[tokio::test]
async fn xai_refresh_discovery_needs_only_the_token_endpoint() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/metadata", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/metadata",
                axum::routing::get(|| async {
                    axum::Json(json!({"token_endpoint":"https://auth.x.ai/oauth/token"}))
                }),
            ),
        )
        .await
        .unwrap();
    });
    let result = discover_xai_at(&reqwest::Client::new(), &url, &Default::default()).await;
    task.abort();
    assert_eq!(result.unwrap(), "https://auth.x.ai/oauth/token");
}

#[rstest]
#[case(400, "reauthorization_required")]
#[case(503, "backoff")]
#[tokio::test]
async fn failed_refresh_is_persistently_quarantined_until_recovery(
    #[case] status: u16,
    #[case] state: &str,
) {
    let vault = crate::vault::Vault::memory().unwrap();
    vault
        .put_account(
            "test",
            Provider::XAI,
            &json!({"access_token":"old","refresh_token":"old-refresh"}),
        )
        .unwrap();
    let mock = endpoint(json!({"error":"invalid_grant"}), status).await;
    vault
        .set_account_proxy("test", Provider::XAI, mock.url.trim_end_matches("/token"))
        .unwrap();
    let mut store = TokenStore::open(Provider::XAI, vault.clone(), "test").unwrap();
    Arc::get_mut(&mut store).unwrap().test_token_endpoint =
        Some("http://token.fixture.invalid/token".into());
    assert!(store.credential().await.is_err());
    assert!(store.credential().await.is_err());
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let revision = store.revision().unwrap();
    assert_eq!(vault.health("test", revision).unwrap()["state"], state);
    let reopened = TokenStore::open(Provider::XAI, vault.clone(), "test").unwrap();
    assert!(reopened.credential().await.is_err());
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    vault
        .import_account(
            "test",
            Provider::XAI,
            &json!({"access_token":"new","refresh_token":"new-refresh","expires_at":4102444800u64}),
        )
        .unwrap();
    assert_eq!(reopened.credential().await.unwrap().access_token, "new");
}

#[tokio::test]
async fn proactive_refresh_and_configured_client_metadata_use_the_account_proxy() {
    let vault = crate::vault::Vault::memory().unwrap();
    vault
        .put_account(
            "test",
            Provider::XAI,
            &json!({"access_token":"old","refresh_token":"refresh","expires_at":now()+450.0}),
        )
        .unwrap();
    let mock = endpoint(
        json!({"access_token":"early-new","refresh_token":"rotated","expires_in":3600}),
        200,
    )
    .await;
    vault
        .set_account_proxy("test", Provider::XAI, mock.url.trim_end_matches("/token"))
        .unwrap();
    let mut store = TokenStore::open(Provider::XAI, vault.clone(), "test").unwrap();
    let mutable = Arc::get_mut(&mut store).unwrap();
    mutable.test_token_endpoint = Some("http://token.fixture.invalid/token".into());
    mutable.profile.client_id = Some("new-official-client".into());
    mutable.refresh_ahead = 600;
    mutable.profile.requests.insert(
        "oauth_refresh".into(),
        serde_json::from_value(json!({
            "headers":{"x-refresh-version":"1.1.1"},"query":{"revision":"next"},
            "body_set":{"/version":"1.1.1","/new_flag":true}
        }))
        .unwrap(),
    );
    mutable.profile.validate("xai").unwrap();
    assert_eq!(store.credential().await.unwrap().access_token, "old");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.maintain().await.unwrap().access_token, "early-new");
    let captured = mock.bodies.lock().await;
    assert_eq!(captured[0].0["x-refresh-version"], "1.1.1");
    assert!(captured[0].1.contains("version=1.1.1"));
    assert!(captured[0].1.contains("new_flag=true"));
    assert!(captured[0].1.contains("client_id=new-official-client"));
    assert!(!captured[0].1.contains("client_secret="));
    assert_eq!(
        vault.account("test", Provider::XAI).unwrap().0["refresh_token"],
        "rotated"
    );
}

#[tokio::test]
async fn refresh_urls_are_compiled_in_and_ignore_imported_endpoint_metadata() {
    for (provider, expected) in [(Provider::OpenAI, "https://auth.openai.com/oauth/token")] {
        let vault = crate::vault::Vault::memory().unwrap();
        vault.put_account("test", provider, &json!({"access_token":"fixture", "refresh_token":"fixture", "token_endpoint":"https://other.invalid/token"})).unwrap();
        vault
            .set_account_proxy("test", provider, "http://127.0.0.1:9")
            .unwrap();
        let store = TokenStore::open(provider, vault, "test").unwrap();
        assert_eq!(store.refresh_endpoint().await.unwrap(), expected);
    }
}

#[tokio::test]
async fn externally_renewed_claude_tokens_remain_usable_until_expiry() {
    let vault = crate::vault::Vault::memory().unwrap();
    vault
        .put_account(
            "claude",
            Provider::Anthropic,
            &json!({"access_token":"sk-ant-oat-fixture","expires_at":now()+30.0}),
        )
        .unwrap();
    vault
        .set_account_proxy("claude", Provider::Anthropic, "http://127.0.0.1:9")
        .unwrap();
    let store = TokenStore::open(Provider::Anthropic, vault.clone(), "claude").unwrap();
    assert!(store.maintain().await.is_ok());
    assert!(store.force_refresh().await.is_err());
    assert!(store.credential().await.is_ok());
    assert_eq!(
        vault.health("claude", store.revision().unwrap()).unwrap()["state"],
        "active"
    );
    vault
        .import_account(
            "claude",
            Provider::Anthropic,
            &json!({"access_token":"sk-ant-oat-fixture","expires_at":1}),
        )
        .unwrap();
    assert!(store.maintain().await.is_err());
    assert_eq!(
        vault.health("claude", store.revision().unwrap()).unwrap()["state"],
        "expired"
    );
}

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::XAI)]
#[tokio::test]
async fn access_only_tokens_do_not_refresh_and_expired_accounts_recover_on_import(
    #[case] provider: Provider,
) {
    let vault = crate::vault::Vault::memory().unwrap();
    // Inside both the proactive refresh window and the normal refresh margin.
    let access = jwt(now() + 30.0, "fixture-account");
    vault
        .put_account("test", provider, &json!({"access_token":access}))
        .unwrap();
    let mock = endpoint(json!({"error":"must_not_refresh"}), 400).await;
    vault
        .set_account_proxy("test", provider, mock.url.trim_end_matches("/token"))
        .unwrap();
    let store = TokenStore::open(provider, vault.clone(), "test").unwrap();
    assert_eq!(store.maintain().await.unwrap().access_token, access);
    assert_eq!(store.credential().await.unwrap().access_token, access);
    assert!(store.force_refresh().await.is_err());
    assert!(store.available());
    assert_eq!(
        vault.health("test", store.revision().unwrap()).unwrap()["state"],
        "active"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);

    vault
        .import_account(
            "test",
            provider,
            &json!({"access_token":jwt(1.0,"fixture-account")}),
        )
        .unwrap();
    assert!(store.maintain().await.is_err());
    assert!(!store.available());
    assert!(store.credential().await.is_err());
    assert!(store.force_refresh().await.is_err());
    let health = vault.health("test", store.revision().unwrap()).unwrap();
    assert_eq!(health["state"], "expired");
    assert_eq!(health["last_error"], "access_token_expired");
    assert_eq!(health["failures"], 0);
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);

    vault
        .import_account(
            "test",
            provider,
            &json!({"access_token":"replacement", "expires_at":now()+3600.0}),
        )
        .unwrap();
    assert!(store.available());
    assert_eq!(
        store.credential().await.unwrap().access_token,
        "replacement"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::XAI)]
#[case(Provider::Anthropic)]
#[tokio::test]
async fn rejected_access_only_tokens_require_replacement_without_refresh_requests(
    #[case] provider: Provider,
) {
    let vault = crate::vault::Vault::memory().unwrap();
    vault
        .put_account("test", provider, &json!({"access_token":"opaque-token"}))
        .unwrap();
    let mock = endpoint(json!({"error":"must_not_refresh"}), 400).await;
    vault
        .set_account_proxy("test", provider, mock.url.trim_end_matches("/token"))
        .unwrap();
    let store = TokenStore::open(provider, vault.clone(), "test").unwrap();
    assert!(store.maintain().await.is_ok());
    store.rejected();
    assert!(!store.available());
    assert!(store.credential().await.is_err());
    assert!(store.maintain().await.is_err());
    assert_eq!(
        vault.health("test", store.revision().unwrap()).unwrap()["last_error"],
        "access_rejected"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn chatgpt_compatibility_headers_override_defaults_and_keep_server_auth() {
    let vault = crate::vault::Vault::memory().unwrap();
    vault.put_account("chat", Provider::OpenAI, &json!({"access_token":"fixture-access","expires_at":now()+3600.0,"account_id":"fixture-account"})).unwrap();
    vault
        .set_account_proxy("chat", Provider::OpenAI, "http://127.0.0.1:9")
        .unwrap();
    let store = TokenStore::open(Provider::OpenAI, vault, "chat").unwrap();
    let http = crate::network::http_client(Provider::OpenAI, "http://127.0.0.1:9", 30).unwrap();
    let mut client = crate::Client::with_oauth(store, http).unwrap();
    client
        .profile
        .headers
        .insert("originator".into(), "future-cli".into());
    client
        .profile
        .headers
        .insert("user-agent".into(), "future-cli/1.0".into());
    let headers = client.auth_headers(HeaderMap::new()).await.unwrap();
    assert_eq!(headers["originator"], "future-cli");
    assert_eq!(headers["user-agent"], "future-cli/1.0");
    assert_eq!(headers["authorization"], "Bearer fixture-access");
    assert_eq!(headers["chatgpt-account-id"], "fixture-account");
}

struct Mock {
    url: String,
    task: tokio::task::JoinHandle<()>,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<(HeaderMap, String)>>>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn endpoint(payload: Value, status: u16) -> Mock {
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let (count, captured) = (calls.clone(), bodies.clone());
    let app = Router::new().route(
        "/token",
        post(move |request: Request| {
            let (count, captured, payload) = (count.clone(), captured.clone(), payload.clone());
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                let (parts, body) = request.into_parts();
                captured.lock().await.push((
                    parts.headers,
                    String::from_utf8(to_bytes(body, 10000).await.unwrap().to_vec()).unwrap(),
                ));
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                axum::http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Mock {
        url,
        task,
        calls,
        bodies,
    }
}

// Corresponds to upstream ChatGPT refresh, xAI token rotation and get_access_token
// double-check locking tests. HTTP is local; these fixtures are not credentials.
#[rstest]
#[case(Provider::OpenAI, false)]
#[case(Provider::OpenAI, true)]
#[case(Provider::XAI, false)]
#[case(Provider::XAI, true)]
#[tokio::test]
async fn refresh_is_single_flight_and_preserves_or_rotates_tokens(
    #[case] provider: Provider,
    #[case] rotate: bool,
) {
    let vault = crate::vault::Vault::memory().unwrap();
    vault.put_account("test", provider, &json!({"access_token":"old","refresh_token":"previous-refresh","expires_at":1,"token_endpoint":"https://auth.x.ai/token"})).unwrap();
    let access = if provider == Provider::OpenAI {
        jwt(now() + 3600.0, "acct-a")
    } else {
        "new-access".into()
    };
    let mut payload =
        json!({"access_token":access,"id_token":jwt(now()+3600.0,"acct-a"),"expires_in":3600});
    if rotate {
        payload["refresh_token"] = "rotated-refresh".into();
    }
    let mock = endpoint(payload, 200).await;
    vault
        .set_account_proxy("test", provider, mock.url.trim_end_matches("/token"))
        .unwrap();
    let mut store = TokenStore::open(provider, vault.clone(), "test").unwrap();
    Arc::get_mut(&mut store).unwrap().test_token_endpoint =
        Some("http://token.fixture.invalid/token".into());
    let results = futures_util::future::join_all((0..12).map(|_| store.credential())).await;
    for result in results {
        assert_eq!(result.unwrap().access_token, access);
    }
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let record = store.read().unwrap();
    assert_eq!(
        record["refresh_token"],
        if rotate {
            "rotated-refresh"
        } else {
            "previous-refresh"
        }
    );
    if provider == Provider::OpenAI {
        assert_eq!(
            store.credential().await.unwrap().account_id.as_deref(),
            Some("acct-a")
        );
    }

    let bodies = mock.bodies.lock().await;
    let (headers, body) = &bodies[0];
    if provider == Provider::OpenAI {
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let value: Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["grant_type"], "refresh_token");
        assert_eq!(value["refresh_token"], "previous-refresh");
        assert_eq!(value["scope"], "openid profile email");
    } else {
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/x-www-form-urlencoded")
        );
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=previous-refresh"));
        assert!(!body.contains("client_secret="));
    }
    drop(bodies);
    drop(store);
    let checkpoint = vault.checkpoint().unwrap();
    let data: crate::vault::Data = serde_json::from_value(checkpoint["state"].clone()).unwrap();
    let reopened = crate::vault::Vault::memory().unwrap();
    reopened.require_manager();
    reopened.initialize(data).unwrap();
    assert_eq!(reopened.account("test", provider).unwrap().0, record);
}

#[tokio::test]
async fn refresh_rejection_preserves_vault_record_and_does_not_expose_response_secrets() {
    let vault = crate::vault::Vault::memory().unwrap();
    let record = json!({"access_token":"old","refresh_token":"keep-me"});
    vault.put_account("test", Provider::XAI, &record).unwrap();
    let mock = endpoint(
        json!({"error":"invalid_grant","secret":"NEVER_EXPOSE"}),
        400,
    )
    .await;
    vault
        .set_account_proxy("test", Provider::XAI, mock.url.trim_end_matches("/token"))
        .unwrap();
    let mut store = TokenStore::open(Provider::XAI, vault.clone(), "test").unwrap();
    Arc::get_mut(&mut store).unwrap().test_token_endpoint =
        Some("http://token.fixture.invalid/token".into());
    let error = store.credential().await.err().unwrap().to_string();
    assert!(!error.contains("NEVER_EXPOSE"));
    assert_eq!(store.read().unwrap(), record);
}

#[tokio::test]
async fn claude_external_refresh_is_observed_and_expiration_is_rejected() {
    let vault = crate::vault::Vault::memory().unwrap();
    let record = |access: &str, expiry: f64| json!({"claudeAiOauth":{"accessToken":access,"expiresAt":expiry*1000.0}});
    vault
        .put_account(
            "test",
            Provider::Anthropic,
            &record("sk-ant-oat-first", now() + 3600.0),
        )
        .unwrap();
    vault
        .set_account_proxy("test", Provider::Anthropic, "http://127.0.0.1:9")
        .unwrap();
    let store = TokenStore::open(Provider::Anthropic, vault.clone(), "test").unwrap();
    assert_eq!(
        store.credential().await.unwrap().access_token,
        "sk-ant-oat-first"
    );
    vault
        .put_account(
            "test",
            Provider::Anthropic,
            &record("sk-ant-oat-second", now() + 3600.0),
        )
        .unwrap();
    assert_eq!(
        store.credential().await.unwrap().access_token,
        "sk-ant-oat-second"
    );
    vault
        .put_account(
            "test",
            Provider::Anthropic,
            &record("sk-ant-oat-expired", 1.0),
        )
        .unwrap();
    assert!(matches!(store.credential().await, Err(Error::OAuth(_))));
}

#[rstest]
#[case("https://auth.x.ai/oauth/token", true)]
#[case("https://x.ai/token", false)]
#[case("http://auth.x.ai/token", false)]
#[case("https://auth.x.ai.attacker.test/token", false)]
#[case("https://attacker.test/token", false)]
#[case("https://user@auth.x.ai/token", false)]
fn discovery_endpoint_validation_requires_official_auth_host(
    #[case] url: &str,
    #[case] allowed: bool,
) {
    assert_eq!(validate_xai_endpoint(url).is_ok(), allowed);
}

#[rstest]
fn invalid_payload_does_not_replace_a_valid_record() {
    assert!(
        build_record(
            Provider::OpenAI,
            &json!({"access_token":"a","refresh_token":"r"}),
            None
        )
        .is_err()
    );
    assert!(
        build_record(
            Provider::XAI,
            &json!({"access_token":"a","refresh_token":"r"}),
            None
        )
        .is_err()
    );
    assert!(
        build_record(
            Provider::XAI,
            &json!({"access_token":"a","refresh_token":"r","token_endpoint":"https://bad.test"}),
            None
        )
        .is_err()
    );
}
#[cfg(feature = "workbuddy-lab")]
#[tokio::test]
async fn workbuddy_static_key_is_replaceable_without_expiry_or_refresh_requests() {
    let vault = crate::vault::Vault::memory().unwrap();
    vault
        .put_account(
            "local",
            Provider::Workbuddy,
            &json!({"api_key":"first-key"}),
        )
        .unwrap();
    vault
        .set_account_proxy("local", Provider::Workbuddy, "http://127.0.0.1:9")
        .unwrap();
    let store = TokenStore::prepared(
        Provider::Workbuddy,
        vault.clone(),
        "local",
        "http://127.0.0.1:9".into(),
    )
    .unwrap();
    assert_eq!(store.maintain().await.unwrap().access_token, "first-key");
    assert!(store.force_refresh().await.is_err());
    vault
        .put_account(
            "local",
            Provider::Workbuddy,
            &json!({"api_key":"second-key"}),
        )
        .unwrap();
    assert_eq!(store.credential().await.unwrap().access_token, "second-key");
    assert!(
        vault
            .import_account(
                "local",
                Provider::Workbuddy,
                &json!({"refresh_token":"invalid"})
            )
            .is_err()
    );
}

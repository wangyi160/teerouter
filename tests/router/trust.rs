use super::*;
use teellm::vault::Vault;

const MODEL: &str = "vendor/Future-v99:preview";
const EFFORT: &str = "future-ULTRA-v99";

#[tokio::test]
async fn removed_provider_routes_and_configuration_cannot_reach_upstream() {
    let upstream = mock(vec![]).await;
    let config = configuration(&upstream);
    let g = gateway(config.clone()).await;
    for path in [
        "/v1beta/models/chat:generateContent",
        "/v1beta/models/chat:streamGenerateContent",
    ] {
        assert_eq!(
            post(&g, path, json!({"contents":[{"parts":[{"text":"hello"}]}]}))
                .await
                .status(),
            404
        );
    }
    for provider in ["gemini", "gemini_cli", "antigravity"] {
        assert!(serde_json::from_value::<Provider>(json!(provider)).is_err());
        assert!(
            teellm::compatibility::Profile::default()
                .validate(provider)
                .is_err()
        );
        let mut removed = serde_json::to_value(&config).unwrap();
        removed["models"][0]["deployments"][0]["provider"] = json!(provider);
        assert!(serde_json::from_value::<Config>(removed).is_err());
    }
    for host in [
        "generativelanguage.googleapis.com",
        "cloudcode-pa.googleapis.com",
        "oauth2.googleapis.com",
    ] {
        let mut foreign = config.clone();
        foreign.models[0].deployments[0].api_base = Some(format!("https://{host}/v1"));
        assert!(foreign.validate().is_err());
    }
    assert!(upstream.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn legacy_empty_provider_fields_restore_without_reenabling_removed_features() {
    let upstream = mock(vec![]).await;
    let mut legacy = serde_json::to_value(configuration(&upstream)).unwrap();
    legacy["models"][0]["deployments"][0]["gemini_project"] = Value::Null;
    legacy["compatibility"] = json!({"openai":{"client_secret_ref":null,"requests":{"responses":{"client_secret_ref":null}}}});
    let config: Config = serde_json::from_value(legacy.clone()).unwrap();
    config.validate().unwrap();
    let encoded = serde_json::to_value(config).unwrap();
    assert!(
        encoded["models"][0]["deployments"][0]
            .get("gemini_project")
            .is_none()
    );
    assert!(
        encoded["compatibility"]["openai"]
            .get("client_secret_ref")
            .is_none()
    );
    assert!(
        encoded["compatibility"]["openai"]["requests"]["responses"]
            .get("client_secret_ref")
            .is_none()
    );
    for path in [
        "/models/0/deployments/0/gemini_project",
        "/compatibility/openai/client_secret_ref",
        "/compatibility/openai/requests/responses/client_secret_ref",
    ] {
        let mut invalid = legacy.clone();
        *invalid.pointer_mut(path).unwrap() = json!("legacy-value");
        assert!(
            serde_json::from_value::<Config>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}

#[rstest]
#[case(Provider::OpenAI, false)]
#[case(Provider::Anthropic, false)]
#[case(Provider::XAI, false)]
#[case(Provider::XAI, true)]
#[tokio::test]
async fn caller_model_and_future_effort_reach_native_and_chat_upstreams_unchanged(
    #[case] provider: Provider,
    #[case] chat: bool,
) {
    let mut replies = Vec::new();

    replies.push(Reply::json(if provider == Provider::XAI && chat {
        completion()
    } else {
        native(provider)
    }));
    let proxy = mock(replies).await;
    let mut bundle = discovery::bundle(provider, &[&proxy]);
    bundle.config.models[0].discover = false;
    bundle.config.models[0].deployments[0].model = "must-not-substitute-this-model".into();
    // Future field locations are configurable, but accepted values still come
    // exclusively from the authenticated catalog fixture, not from this config.
    if provider == Provider::Anthropic {
        bundle.config.compatibility.insert(crate::compatibility::name(provider).into(), serde_json::from_value(json!({
            "catalog_capabilities":{"effort":"/fixture_levels", "budget_min":"/fixture_budget_min", "budget_max":"/fixture_budget_max"}
        })).unwrap());
    }
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let gateway = run_fixture(Engine::from_vault(vault).unwrap()).await;
    let (path, body) = if chat {
        (
            "/v1/chat/completions".to_owned(),
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"reasoning_effort":EFFORT}),
        )
    } else {
        match provider {
            Provider::Anthropic => (
                "/v1/messages".into(),
                json!({"model":MODEL,"max_tokens":8,"messages":[{"role":"user","content":"hello"}],"output_config":{"effort":EFFORT},"thinking":{"type":"future-mode","budget_tokens":12345}}),
            ),
            _ => (
                "/v1/responses".into(),
                json!({"model":MODEL,"input":"hello","reasoning":{"effort":EFFORT,"summary":"auto"}}),
            ),
        }
    };
    let response = client()
        .post(format!("{}{path}", gateway.url))
        .header("user-agent", gateway.agent)
        .bearer_auth("client-secret")
        .header("x-router-pool", "pool")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let calls = proxy.calls.lock().unwrap();
    let call = calls.last().unwrap();
    assert_eq!(call.body["model"], MODEL);
    assert!(!call.headers.contains_key("x-router-pool"));
    let effort = match provider {
        Provider::OpenAI => &call.body["reasoning"]["effort"],
        Provider::Anthropic => &call.body["output_config"]["effort"],
        Provider::XAI if chat => &call.body["reasoning_effort"],
        Provider::XAI => &call.body["reasoning"]["effort"],
        #[cfg(feature = "workbuddy-lab")]
        Provider::Workbuddy => &call.body["reasoning_effort"],
    };
    assert_eq!(effort, EFFORT);
    if !chat && provider == Provider::Anthropic {
        assert_eq!(call.body["thinking"], body["thinking"]);
    }
}

#[tokio::test]
async fn automatic_pool_selection_preserves_explicit_selection_and_acl() {
    let proxy = mock(vec![Reply::json(completion()); 3]).await;
    let mut config = configuration(&proxy);
    let mut second = config.models[0].clone();
    second.name = "second".into();
    second.deployments[0].id = "b".into();
    config.models.push(second);
    let gateway = gateway(config).await;
    let body = json!({"model":MODEL,"messages":[{"role":"user","content":"hi"}]});
    assert_eq!(
        post(&gateway, "/v1/chat/completions", body.clone())
            .await
            .status(),
        200
    );
    let response = client()
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth("client-secret")
        .header("x-router-pool", "second")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-deployment"], "b");
    let update = client().post(format!("{}/admin/apply", gateway.url)).bearer_auth("admin-secret")
        .json(&json!({"config":{"users":[{"id":"test-user"}],"keys":[{"name":"user","user_id":"test-user","key_ref":"CLIENT_KEY","models":["chat/*"]}]}})).send().await.unwrap();
    assert_eq!(update.status(), 200, "{}", update.text().await.unwrap());
    for (pool, status) in [("second", 403), ("chat", 200), ("missing", 404)] {
        assert_eq!(
            client()
                .post(format!("{}/v1/chat/completions", gateway.url))
                .bearer_auth("client-secret")
                .header("x-router-pool", pool)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            status
        );
    }
    assert_eq!(proxy.calls.lock().unwrap().len(), 3);
    // A model that resembles the ACL's pool syntax must not authorize another pool.
    for model in ["chat/forbidden", "chat/vendor/model"] {
        let mut spoof = body.clone();
        spoof["model"] = model.into();
        assert_eq!(
            client()
                .post(format!("{}/v1/chat/completions", gateway.url))
                .bearer_auth("client-secret")
                .header("x-router-pool", "second")
                .json(&spoof)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
}

#[tokio::test]
async fn pool_is_discovered_from_model_and_continuations_stay_on_the_original_account() {
    let a = mock(vec![]).await;
    let b = mock(vec![
        Reply::json(native(Provider::XAI)),
        Reply::json(native(Provider::XAI)),
    ])
    .await;
    let mut bundle = discovery::bundle(Provider::XAI, &[&a, &b]);
    bundle.config.keys[0].models.clear();
    let mut second = bundle.config.models[0].clone();
    second.name = "second".into();
    second.deployments.retain(|d| d.id == "account-1");
    second.allowed_models = Some(vec!["other".into(), "actual-model".into()]);
    bundle.config.models[0]
        .deployments
        .retain(|d| d.id == "account-0");
    bundle.config.models[0].allowed_models = Some(vec!["chat".into(), "actual-model".into()]);
    bundle.config.models.push(second);
    let vault = Vault::memory().unwrap();
    vault.provision(&bundle).unwrap();
    let gateway = run_fixture(Engine::from_vault(vault).unwrap()).await;
    let body = json!({"model":"other","input":"hello"});
    let response = post(&gateway, "/v1/responses", body.clone()).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let expected = body;
    assert_cached_forward(&b.calls.lock().unwrap().last().unwrap().body, &expected);
    let continuation = post(
        &gateway,
        "/v1/responses",
        json!({"model":"actual-model","previous_response_id":"reply","input":"continue"}),
    )
    .await;
    assert_eq!(continuation.status(), 200);
    assert_eq!(a.calls.lock().unwrap().len(), 0); // Never receives the user's prompt.
    assert_eq!(b.calls.lock().unwrap().len(), 2);
    // Automatic routing must not guess the account of a foreign response ID.
    assert_eq!(
        post(
            &gateway,
            "/v1/responses",
            json!({"model":"actual-model","previous_response_id":"unknown","input":"hello"})
        )
        .await
        .status(),
        409
    );
    assert_eq!(b.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn fallback_retries_the_same_model_and_effort() {
    let failed = mock(vec![Reply::error(503)]).await;
    let good = mock(vec![Reply::json(completion())]).await;
    let mut config = configuration(&failed);
    config.compatibility.insert(
        "openai".into(),
        serde_json::from_value(json!({
            "catalog_capabilities":{"effort":"/fixture_levels"}
        }))
        .unwrap(),
    );
    config.models[0].max_attempts = 2;
    config.models[0].fallbacks.push("backup".into());
    let mut backup = config.models[0].clone();
    backup.name = "backup".into();
    backup.fallbacks.clear();
    backup.deployments[0].id = "backup".into();
    backup.deployments[0].model = "different-model".into();
    backup.deployments[0].api_base = Some(format!("{}/v1", good.running.url));
    backup.deployments[0].proxy = Some(good.running.url.clone());
    config.models.push(backup);
    let gateway = gateway(config).await;
    let response = client().post(format!("{}/v1/chat/completions", gateway.url)).bearer_auth("client-secret")
        .header("x-router-pool", "chat").json(&json!({"model":MODEL,"reasoning_effort":EFFORT,"messages":[{"role":"user","content":"hi"}]})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    for proxy in [&failed, &good] {
        let calls = proxy.calls.lock().unwrap();
        assert_eq!(calls[0].body["model"], MODEL);
        assert_eq!(calls[0].body["reasoning_effort"], EFFORT);
    }
}

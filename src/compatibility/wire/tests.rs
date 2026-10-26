use super::*;
use serde_json::json;

fn profile(operation: &str, rule: Value) -> Profile {
    serde_json::from_value(json!({"requests":{operation:rule}})).unwrap()
}

#[test]
fn removed_login_operations_and_protected_refresh_fields_are_rejected() {
    for provider in ["openai", "anthropic", "xai"] {
        for operation in [
            "oauth_device_code",
            "oauth_device_poll",
            "oauth_authorize",
            "oauth_token",
            "oauth_refresh",
            "oauth_discovery",
        ] {
            assert!(profile(operation, json!({})).validate(provider).is_err());
        }
    }
    for rule in [
        json!({"body_set":{"/client_id":42}}),
        json!({"body_set":{"/client_id":""}}),
        json!({"body_set":{"/metadata/client_id":"nested"}}),
        json!({"body_remove":["/client_id"]}),
        json!({"body_set":{"/refresh_token":"changed"}}),
        json!({"body_set":{"/grant_type":"authorization_code"}}),
        json!({"body_set":{"/code_verifier":"changed"}}),
    ] {
        assert!(
            profile("oauth_refresh", rule.clone())
                .validate("openai")
                .is_err(),
            "{rule}"
        );
    }
    assert!(
        profile("responses", json!({"body_set":{"/client_id":"app"}}))
            .validate("openai")
            .is_err()
    );
}

#[test]
fn oauth_urls_and_transport_headers_are_not_configurable() {
    for value in [
        json!({"token_url":"https://auth.openai.com/oauth/token"}),
        json!({"auth_base":"https://auth.openai.com"}),
        json!({"discovery_url":"https://auth.x.ai/.well-known/openid-configuration"}),
        json!({"requests":{"oauth_refresh":{"url":"https://auth.openai.com/oauth/token"}}}),
        json!({"requests":{"oauth_discovery":{"url":"https://auth.x.ai/.well-known/openid-configuration"}}}),
    ] {
        assert!(serde_json::from_value::<Profile>(value).is_err());
    }
    for provider in ["openai", "anthropic", "xai"] {
        for header in [
            "content-type",
            "Content-Type",
            "accept",
            "Accept",
            "host",
            "content-length",
            "connection",
        ] {
            let global: Profile =
                serde_json::from_value(json!({"headers":{header:"custom"}})).unwrap();
            assert!(global.validate(provider).is_err());
            assert!(
                profile("models", json!({"headers":{header:"custom"}}))
                    .validate(provider)
                    .is_err()
            );
            assert!(
                profile("models", json!({"remove_headers":[header]}))
                    .validate(provider)
                    .is_err()
            );
        }
        assert!(
            profile("models", json!({"headers":{"x-client-version":"v2"}}))
                .validate(provider)
                .is_ok()
        );
    }
}

#[test]
fn catalog_metadata_remains_configurable_without_rewriting_generation() {
    let profile = profile(
        "models",
        json!({"headers":{"x-version":"v2"},"remove_headers":["x-old"],
        "query":{"version":"v2","label":"a & b"},"remove_query":["old"]}),
    );
    profile.validate("openai").unwrap();
    let mut url =
        Url::parse("https://api.openai.com/v1/models?version=v1&version=v0&old=1&keep=2").unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-old", HeaderValue::from_static("old"));
    profile
        .prepare("models", &mut url, &mut headers, None)
        .unwrap();
    assert_eq!(headers["x-version"], "v2");
    assert!(!headers.contains_key("x-old"));
    let pairs: Vec<_> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs.iter().filter(|(k, _)| k == "version").count(), 1);
    assert!(pairs.contains(&("keep".into(), "2".into())));
    assert!(pairs.contains(&("label".into(), "a & b".into())));
}

#[test]
fn all_parameter_edits_fail_at_validation_and_runtime_without_mutation() {
    for rule in [
        json!({"body_set":{"/unknown_parameter":42}}),
        json!({"body_defaults":{"/prompt_cache_options":{"ttl":"12h"}}}),
        json!({"body_remove":["/unknown_parameter"]}),
        json!({"body_null":["/metadata"]}),
        json!({"headers":{"x-future":"injected"}}),
        json!({"remove_headers":["x-future"]}),
        json!({"query":{"beta":"true"}}),
        json!({"remove_query":["beta"]}),
    ] {
        let profile = profile("responses", rule.clone());
        assert!(profile.validate("openai").is_err(), "{rule}");
        let mut url = Url::parse("https://api.openai.com/v1/responses?beta=false").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-future", HeaderValue::from_static("caller"));
        let body = json!({"unknown_parameter":1,"metadata":{"keep":true}});
        let before = (url.clone(), headers.clone(), body.clone());
        assert!(
            profile
                .prepare("responses", &mut url, &mut headers, Some(&body))
                .is_err()
        );
        assert_eq!((url, headers, body), before);
    }
}

#[test]
fn protected_fields_and_ambiguous_rules_are_rejected_before_activation() {
    for rule in [
        json!({"headers":{"authorization":"other"}}),
        json!({"headers":{"content-length":"1"}}),
        json!({"remove_headers":["chatgpt-account-id"]}),
        json!({"headers":{"X-Version":"bad-case"}}),
        json!({"headers":{"x-version":"ok\r\nAuthorization: bad"}}),
        json!({"headers":{"x-version":"v1"},"remove_headers":["x-version"]}),
        json!({"query":{"redirect_uri":"https://external.invalid"}}),
        json!({"query":{"key":"secret"}}),
        json!({"remove_query":["alt"]}),
        json!({"body_set":{"/model":"different"}}),
        json!({"body_set":{"/reasoning/effort":"low"}}),
        json!({"body_remove":["/reasoning"]}),
        json!({"body_defaults":{"/reasoning_effort":"low"}}),
        json!({"body_set":{"/service_tier":"priority"}}),
        json!({"body_remove":["/speed"]}),
        json!({"body_defaults":{"/inference_geo":"us"}}),
        json!({"body_set":{"/system/0/cache_control":{"type":"ephemeral","ttl":"1h"}}}),
        json!({"body_set":{"/generationConfig/thinkingConfig/thinkingBudget":1}}),
        json!({"body_set":{"/request/generationConfig":{}}}),
        json!({"body_set":{"/output_config/effort":"low"}}),
        json!({"body_null":["/thinking"]}),
        json!({"body_set":{"/metadata":{"previous_response_id":"foreign"}}}),
        json!({"body_set":{"/request":{"metadata":{}}}}),
        json!({"body_defaults":{"/request/model":"different"}}),
        json!({"body_set":{"/new/Access_Token":"secret"}}),
        json!({"body_set":{"/meta~2version":"invalid"}}),
        json!({"body_set":{"version":"not-pointer"}}),
        json!({"body_set":{"/metadata":{},"/metadata/version":"v1"}}),
        json!({"body_defaults":{"/version":"v1"},"body_remove":["/version"]}),
    ] {
        assert!(
            profile("responses", rule.clone())
                .validate("openai")
                .is_err(),
            "accepted {rule}"
        );
    }
    assert!(
        profile("models", json!({"body_set":{"/version":"v1"}}))
            .validate("openai")
            .is_err()
    );
    assert!(profile("respones", json!({})).validate("openai").is_err());
    assert!(
        profile("oauth_refresh", json!({"body_set":{"/meta/version":"v1"}}))
            .validate("xai")
            .is_err()
    );
    assert!(profile("responses", json!({})).validate("gemini").is_err());
}

#[test]
fn storage_controls_cannot_be_configured_or_reintroduced_in_nested_values() {
    for provider in ["openai", "xai"] {
        for operation in ["responses", "chat_completions"] {
            for rule in [
                json!({"body_set":{"/store":true}}),
                json!({"body_set":{"/store":false}}),
                json!({"body_defaults":{"/store":true}}),
                json!({"body_remove":["/store"]}),
                json!({"body_null":["/store"]}),
                json!({"body_set":{"/metadata":{"Store":true}}}),
                json!({"body_defaults":{"/metadata":[{"st_ore":true}]}}),
                json!({"body_set":{"/metadata/store":true}}),
                json!({"query":{"store":"true"}}),
                json!({"remove_query":["store"]}),
            ] {
                assert!(
                    profile(operation, rule.clone()).validate(provider).is_err(),
                    "{rule}"
                );
            }
        }
    }
    let extra: Profile =
        serde_json::from_value(json!({"chatgpt_extra_parameters":["store"]})).unwrap();
    assert!(extra.validate("openai").is_err());
}

#[test]
fn user_content_cannot_be_injected_replaced_or_deleted_by_protocol_rules() {
    for (provider, operation, prefix) in [
        ("openai", "responses", ""),
        ("openai", "chat_completions", ""),
        ("anthropic", "messages", ""),
        ("xai", "responses", ""),
        ("xai", "chat_completions", ""),
    ] {
        for field in [
            "instructions",
            "messages",
            "input",
            "tools",
            "tool_choice",
            "system",
            "systemInstruction",
            "contents",
            "parts",
            "text",
            "file",
            "attachments",
            "functionDeclarations",
            "provider_content",
            "response_format",
            "temperature",
            "max_output_tokens",
            "cachedContent",
            "IN_STRUCTIONS",
        ] {
            let path = format!("{prefix}/{field}");
            for rule in [
                json!({"body_set":{&path:"replacement"}}),
                json!({"body_set":{format!("{path}/nested"):"replacement"}}),
                json!({"body_defaults":{&path:[]}}),
                json!({"body_remove":[&path]}),
                json!({"body_null":[&path]}),
                json!({"body_set":{format!("{prefix}/metadata"): {"wrapper":[{field:"injected"}]}}}),
                json!({"query":{field:"injected"}}),
                json!({"remove_query":[field]}),
            ] {
                assert!(
                    profile(operation, rule).validate(provider).is_err(),
                    "{provider}/{operation}/{field}"
                );
            }
        }
        for rule in [
            json!({"headers":{"instructions":"override"}}),
            json!({"remove_headers":["tools"]}),
        ] {
            assert!(profile(operation, rule).validate(provider).is_err());
        }
    }
}

#[test]
fn responses_storage_preserves_presence_type_and_value() {
    for value in [
        None,
        Some(json!(true)),
        Some(json!(false)),
        Some(Value::Null),
        Some(json!("true")),
    ] {
        let mut body =
            json!({"model":"future-model","reasoning":{"effort":"future-level"},"input":"hello"});
        if let Some(value) = value {
            body["store"] = value;
        }
        let request = Profile::default()
            .json(
                "responses",
                &reqwest::Client::new(),
                "https://api.x.ai/v1/responses",
                HeaderMap::new(),
                body.clone(),
            )
            .unwrap()
            .build()
            .unwrap();
        let sent: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent, body);
        assert_eq!(sent["model"], "future-model");
        assert_eq!(sent["reasoning"]["effort"], "future-level");
    }
}

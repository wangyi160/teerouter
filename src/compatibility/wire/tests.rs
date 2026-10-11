use super::*;
use serde_json::json;

fn profile(operation: &str, rule: Value) -> Profile {
    serde_json::from_value(json!({"requests":{operation:rule}})).unwrap()
}

#[test]
fn refresh_metadata_is_isolated_from_model_interfaces() {
    let rules: Profile = serde_json::from_value(json!({"requests":{
        "responses":{"headers":{"x-version":"responses-v2"}},
        "models":{"headers":{"x-version":"models-v1"}},
        "oauth_refresh":{"body_set":{"/client_id":"refresh-app","/scope":"openid"}}
    }}))
    .unwrap();
    rules.validate("openai").unwrap();
    for (operation, expected) in [
        ("responses", "responses-v2"),
        ("models", "models-v1"),
        ("chat_completions", "original"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("x-version", HeaderValue::from_static("original"));
        let request = rules
            .get(
                operation,
                &reqwest::Client::new(),
                "https://chatgpt.com/backend-api/codex/models",
                headers,
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.headers()["x-version"], expected);
    }
    let request = rules.json("oauth_refresh", &reqwest::Client::new(), "https://auth.openai.com/oauth/token?keep=1", HeaderMap::new(),
        json!({"client_id":"original","scope":"old","grant_type":"refresh_token","refresh_token":"private"})).unwrap().build().unwrap();
    assert_eq!(request.url().path(), "/oauth/token");
    assert_eq!(request.headers()["content-type"], "application/json");
    assert_eq!(request.url().query(), Some("keep=1"));
    let body: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"client_id":"refresh-app","scope":"openid","grant_type":"refresh_token","refresh_token":"private"})
    );
    let gemini = profile(
        "oauth_refresh",
        json!({"client_secret_ref":"refresh-secret","body_set":{"/client_id":"gemini-app"}}),
    );
    gemini.validate("gemini").unwrap();
    assert_eq!(
        gemini
            .client_secret_for("oauth_refresh")
            .map(String::as_str),
        Some("refresh-secret")
    );
    let request = gemini
        .form(
            "oauth_refresh",
            &reqwest::Client::new(),
            "https://oauth2.googleapis.com/token",
            &[("client_id", "old"), ("refresh_token", "private")],
        )
        .unwrap()
        .build()
        .unwrap();
    let form = std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(
        request.headers()["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert!(form.contains("client_id=gemini-app"));
    assert!(form.contains("refresh_token=private"));
}

#[test]
fn removed_login_operations_and_protected_refresh_fields_are_rejected() {
    for provider in ["openai", "anthropic", "gemini", "xai"] {
        for operation in [
            "oauth_device_code",
            "oauth_device_poll",
            "oauth_authorize",
            "oauth_token",
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
    for provider in ["openai", "anthropic", "gemini", "xai"] {
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
fn metadata_edits_preserve_caller_defaults_and_encode_queries_without_duplicates() {
    let profile = profile(
        "responses",
        json!({
            "headers":{"x-future-version":"1.1.1","anthropic-beta":"future-beta"},
            "remove_headers":["x-obsolete"],
            "query":{"version":"1.1.1","label":"a & b"},"remove_query":["old"],
            "body_defaults":{"/metadata/existing":"default","/metadata/added":true},
            "body_set":{"/version":"1.1.1","/metadata/a~1b/~0key":[1,2]},
            "body_remove":["/metadata/obsolete","/missing/old"],"body_null":["/metadata/optional"]
        }),
    );
    profile.validate("openai").unwrap();
    let mut url =
        Url::parse("https://api.openai.com/v1/responses?version=old&version=older&old=1&keep=2")
            .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer private"));
    headers.insert("x-obsolete", HeaderValue::from_static("old"));
    let mut body = json!({"model":"chosen-model","stream":true,"metadata":{"existing":"caller","obsolete":false}});
    profile
        .prepare("responses", &mut url, &mut headers, Some(&mut body))
        .unwrap();
    assert_eq!(headers["authorization"], "Bearer private");
    assert_eq!(headers["x-future-version"], "1.1.1");
    assert!(!headers.contains_key("x-obsolete"));
    let pairs: Vec<_> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs.iter().filter(|(k, _)| k == "version").count(), 1);
    assert!(pairs.contains(&("label".into(), "a & b".into())));
    assert_eq!(
        body,
        json!({"model":"chosen-model","stream":true,"version":"1.1.1","metadata":{"existing":"caller","added":true,"optional":null,"a/b":{"~key":[1,2]}}})
    );
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
            .validate("gemini")
            .is_err()
    );
    assert!(profile("responses", json!({})).validate("gemini").is_err());
}

#[test]
fn conflicting_runtime_shape_stops_before_building_a_network_request() {
    let profile = profile("responses", json!({"body_set":{"/metadata/version":"v1"}}));
    profile.validate("openai").unwrap();
    assert!(
        profile
            .json(
                "responses",
                &reqwest::Client::new(),
                "https://api.openai.com/v1/responses",
                HeaderMap::new(),
                json!({"metadata":"caller-scalar"})
            )
            .is_err()
    );
}

#[test]
fn oauth_form_metadata_is_encoded_and_generated_security_fields_are_retained() {
    let profile = profile(
        "oauth_refresh",
        json!({"body_set":{"/version":"1.1.1","/new_flag":true},"query":{"revision":"next"},"headers":{"x-client-version":"1.1.1"}}),
    );
    profile.validate("gemini").unwrap();
    let request = profile
        .form(
            "oauth_refresh",
            &reqwest::Client::new(),
            "https://oauth2.googleapis.com/token",
            &[
                ("refresh_token", "private & token"),
                ("grant_type", "refresh_token"),
            ],
        )
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(request.headers()["x-client-version"], "1.1.1");
    let body = std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap();
    let parsed = Url::parse(&format!("http://fixture/?{body}")).unwrap();
    let pairs: BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(pairs["refresh_token"], "private & token");
    assert_eq!(pairs["version"], "1.1.1");
    assert_eq!(pairs["new_flag"], "true");
}

#[test]
fn editable_protocol_fields_do_not_allow_ancestor_edits_to_erase_reasoning() {
    let rules = profile(
        "responses",
        json!({"body_set":{
            "/include":["future.extension"],
            "/reasoning/summary":"detailed","/version":"1.1.1"
        }}),
    );
    rules.validate("openai").unwrap();
    let mut url = Url::parse("https://chatgpt.com/backend-api/codex/responses").unwrap();
    let mut body = json!({"model":"future-model","reasoning":{"effort":"FUTURE"}});
    rules
        .prepare(
            "responses",
            &mut url,
            &mut HeaderMap::new(),
            Some(&mut body),
        )
        .unwrap();
    assert_eq!(body["model"], "future-model");
    assert_eq!(body["reasoning"]["effort"], "FUTURE");
    assert_eq!(body["reasoning"]["summary"], "detailed");
    assert!(body.get("store").is_none());
    let rules = profile("messages", json!({"body_remove":["/messages"]}));
    assert!(rules.validate("anthropic").is_err());
    let mut body = json!({"model":"future","messages":[{"role":"user","output_config":{"effort":"high"},"content":"hello"}]});
    assert!(
        rules
            .prepare("messages", &mut url, &mut HeaderMap::new(), Some(&mut body))
            .is_err()
    );
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
        ("gemini", "generate_content", "/request"),
        ("gemini", "stream_generate_content", "/request"),
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
fn metadata_edits_preserve_caller_content_and_ancestor_edits_fail_closed() {
    let caller = json!({"model":"future", "input":[{"role":"user","content":[{"type":"input_text","text":"keep exactly"},{"type":"input_image","image_url":"data:image/png;base64,AA=="}]}],
        "instructions":"caller instructions", "tools":[{"type":"function","name":"keep_tool","parameters":{"type":"object","properties":{"model":{"type":"string"},"store":{"type":"boolean"}}}}],
        "tool_choice":"auto", "reasoning":{"effort":"high"}, "metadata":{"version":"old"}});
    let rules = profile("responses", json!({"body_set":{"/metadata/version":"new"}}));
    rules.validate("openai").unwrap();
    let request = rules
        .json(
            "responses",
            &reqwest::Client::new(),
            "https://api.openai.com/v1/responses",
            HeaderMap::new(),
            caller.clone(),
        )
        .unwrap()
        .build()
        .unwrap();
    let actual: Value =
        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    let mut expected = caller;
    expected["metadata"]["version"] = json!("new");
    expected["store"] = json!(false);
    assert_eq!(actual, expected);
    for rule in [
        json!({"body_set":{"/wrapper":{}}}),
        json!({"body_remove":["/wrapper"]}),
        json!({"body_null":["/wrapper"]}),
    ] {
        let rules = profile("responses", rule);
        rules.validate("openai").unwrap();
        assert!(
            rules
                .json(
                    "responses",
                    &reqwest::Client::new(),
                    "https://api.openai.com/v1/responses",
                    HeaderMap::new(),
                    json!({"wrapper":{"nested":{"messages":[{"role":"user","content":"keep"}]}}})
                )
                .is_err()
        );
    }
}

#[test]
fn responses_storage_is_false_in_the_serialized_request_even_without_a_profile() {
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
                body,
            )
            .unwrap()
            .build()
            .unwrap();
        let sent: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["store"], false);
        assert_eq!(sent["model"], "future-model");
        assert_eq!(sent["reasoning"]["effort"], "future-level");
    }
}

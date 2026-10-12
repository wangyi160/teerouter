use super::*;

fn policy(value: Value) -> Restrictions {
    let rules: Vec<Restriction> = serde_json::from_value(value).unwrap();
    validate(&rules).unwrap();
    Restrictions::for_model(&rules, Provider::Anthropic, "official-alias", "official")
}

#[test]
fn deny_rules_are_additive_and_scope_includes_official_canonical_id() {
    let rules = json!([
        {"provider":"anthropic","model":"official","block_model":true},
        {"provider":"anthropic","model":"official-alias","block_model":false},
        {"provider":"openai","blocked_efforts":["low"]}
    ]);
    let p = policy(rules);
    assert!(p.block_model);
    assert!(p.blocked_efforts.is_empty());
    assert_eq!(p.check(&json!({})).unwrap_err().code, "model_blocked");
    assert!(!policy(json!([{"model":"another","block_model":true}])).block_model);
}

#[test]
fn restrictions_never_rewrite_parameters_or_require_optional_fields() {
    let p = policy(json!([{"blocked_efforts":["high"]}]));
    for path in ["reasoning_effort", "reasoning", "output_config"] {
        let value = match path {
            "reasoning_effort" => json!("high"),
            _ => json!({"effort":"high"}),
        };
        let body = json!({path:value});
        let original = body.clone();
        assert_eq!(p.check(&body).unwrap_err().code, "reasoning_blocked");
        assert_eq!(body, original);
    }
    let omitted = json!({"model":"official", "messages":[{"role":"user","content":"hello"}]});
    let original = omitted.clone();
    p.check(&omitted).unwrap();
    assert_eq!(omitted, original);
    let allowed = json!({"reasoning_effort":"low"});
    p.check(&allowed).unwrap();
    assert_eq!(allowed["reasoning_effort"], "low");
    assert_eq!(p.check(&json!({"output_config":{"effort":"low"},"messages":[{"output_config":{"effort":"high"}}]})).unwrap_err().code, "reasoning_blocked");
}

#[test]
fn thinking_policy_leaves_budgets_and_official_defaults_untouched() {
    let p = policy(json!([{"blocked_thinking_types":["adaptive"]}]));
    let official = Capabilities {
        effort: None,
        thinking_types: Some(["adaptive".into(), "enabled".into()].into()),
        budget: Some((-1, 10)),
        per_message_effort: None,
    };
    let original = official.clone();
    let available = p.available(&official);
    assert_eq!(official, original);
    assert_eq!(available["thinking_types"], json!(["enabled"]));
    assert_eq!(available["budget"], json!([-1, 10]));
    assert_eq!(available["effort"], Value::Null); // Missing official metadata remains missing.
    for (kind, budget, status) in [
        ("adaptive", 1, false),
        ("enabled", 4, true),
        ("enabled", -1, true),
        ("enabled", 7, true),
    ] {
        assert_eq!(
            p.check(&json!({"thinking":{"type":kind,"budget_tokens":budget}}))
                .is_ok(),
            status
        );
    }
    for body in [
        json!({}),
        json!({"thinking":{"type":"enabled"}}),
        json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":-1}}}),
    ] {
        let before = body.clone();
        p.check(&body).unwrap();
        assert_eq!(body, before);
    }
    assert!(available.get("require_budget").is_none());
    assert!(available.get("require_thinking_type").is_none());
    assert!(available.get("require_effort").is_none());
}

#[test]
fn filtering_does_not_invent_capabilities_or_restrict_official_budgets() {
    let p = policy(json!([{"blocked_efforts":["high"]}]));
    let official = Capabilities {
        budget: Some((i64::MIN, i64::MAX)),
        ..Default::default()
    };
    assert_eq!(
        p.available(&official)["budget"],
        json!([i64::MIN, i64::MAX])
    );
    assert_eq!(p.available(&Capabilities::default())["budget"], Value::Null);
    let p = policy(json!([{"blocked_efforts":["high","invented"]}]));
    let official = Capabilities {
        effort: Some(["low".into(), "high".into()].into()),
        ..Default::default()
    };
    assert_eq!(p.available(&official)["effort"], json!(["low"]));
}

#[test]
fn configuration_accepts_only_bounded_deny_rules() {
    assert!(
        serde_json::from_value::<Restriction>(json!({"allowed_efforts":["invented"]})).is_err()
    );
    assert!(serde_json::from_value::<Restriction>(json!({"replace_model":"fake"})).is_err());
    assert!(serde_json::from_value::<Restriction>(json!({"blocked_budget_ranges":[]})).is_err());
    for value in [
        json!([{"blocked_efforts":[""]}]),
        json!([{"model":"bad model"}]),
    ] {
        assert!(validate(&serde_json::from_value::<Vec<Restriction>>(value).unwrap()).is_err());
    }
    assert!(validate(&vec![Restriction::default(); 1025]).is_err());
}

use super::*;
use serde_json::json;

#[test]
fn vendor_schema_choices_are_dynamic_exact_and_do_not_supply_defaults() {
    for (provider, codex, entry) in [
        (
            Provider::OpenAI,
            true,
            json!({"supported_reasoning_levels":[{"effort":"future-X"}],"default_reasoning_level":"not-allowed"}),
        ),
        (
            Provider::Anthropic,
            false,
            json!({"capabilities":{"effort":{"supported":true,"future-X":{"supported":true},"high":{"supported":false}}}}),
        ),
        (
            Provider::XAI,
            false,
            json!({"capabilities":{"reasoning_effort":["future-X"],"default_reasoning_effort":"not-allowed"}}),
        ),
    ] {
        let caps =
            Capabilities::read(provider, codex, &entry, &CapabilityPaths::default()).unwrap();
        let body = json!({"reasoning_effort":"future-X"});
        caps.check(provider, Protocol::Chat, &body).unwrap();
        assert_eq!(body, json!({"reasoning_effort":"future-X"}));
        for effort in ["future-x", "high", "not-allowed"] {
            assert_eq!(
                caps.check(
                    provider,
                    Protocol::Chat,
                    &json!({"reasoning_effort":effort})
                )
                .unwrap_err()
                .status,
                400
            );
        }
        let absent = json!({});
        caps.check(provider, Protocol::Chat, &absent).unwrap();
        assert_eq!(absent, json!({}));
    }
}

#[test]
fn thinking_types_and_numeric_bounds_require_official_evidence() {
    let paths: CapabilityPaths =
        serde_json::from_value(json!({"budget_min":"/range/min","budget_max":"/range/max"}))
            .unwrap();
    paths.validate().unwrap();
    let entry = json!({"capabilities":{"thinking":{"supported":true,"types":{"enabled":{"supported":true},"adaptive":{"supported":false},"disabled":{"supported":true}}}},"range":{"min":1024,"max":8192}});
    let caps = Capabilities::read(Provider::Anthropic, false, &entry, &paths).unwrap();
    let valid = json!({"thinking":{"type":"enabled","budget_tokens":8192}});
    caps.check(Provider::Anthropic, Protocol::Messages, &valid)
        .unwrap();
    assert_eq!(valid["thinking"]["budget_tokens"], 8192);
    for value in [
        json!(1023),
        json!(8193),
        json!(8192.5),
        json!("8192"),
        Value::Null,
    ] {
        assert_eq!(
            caps.check(
                Provider::Anthropic,
                Protocol::Messages,
                &json!({"thinking":{"type":"enabled","budget_tokens":value}})
            )
            .unwrap_err()
            .status,
            400
        );
    }
    assert_eq!(
        caps.check(
            Provider::Anthropic,
            Protocol::Messages,
            &json!({"thinking":{"type":"adaptive"}})
        )
        .unwrap_err()
        .status,
        400
    );
    let no_bounds = Capabilities::read(
        Provider::Anthropic,
        false,
        &entry,
        &CapabilityPaths::default(),
    )
    .unwrap();
    assert_eq!(
        no_bounds
            .check(Provider::Anthropic, Protocol::Messages, &valid)
            .unwrap_err()
            .status,
        503
    );
    // Disabling thinking is independently advertised even on non-thinking models.
    let disabled = Capabilities::read(Provider::Anthropic, false, &json!({"capabilities":{"thinking":{"supported":false,"types":{"disabled":{"supported":true}}}}}), &CapabilityPaths::default()).unwrap();
    disabled
        .check(
            Provider::Anthropic,
            Protocol::Messages,
            &json!({"thinking":{"type":"disabled"}}),
        )
        .unwrap();
}

#[test]
fn source_paths_can_follow_protocol_changes_without_local_allowlists() {
    let paths: CapabilityPaths = serde_json::from_value(json!({"effort":"/new/choices","effort_item":"/value","budget_min":"/new/min","budget_max":"/new/max"})).unwrap();
    paths.validate().unwrap();
    let caps = Capabilities::read(
        Provider::XAI,
        false,
        &json!({"new":{"choices":[{"value":"Next-Level"}],"min":-1,"max":2048}}),
        &paths,
    )
    .unwrap();
    let body = json!({"reasoning":{"effort":"Next-Level"}});
    caps.check(Provider::XAI, Protocol::Responses, &body)
        .unwrap();
    assert_eq!(body["reasoning"]["effort"], "Next-Level");
    assert_eq!(caps.budget, Some((-1, 2048)));
    assert!(serde_json::from_value::<CapabilityPaths>(json!({"effort":["high"]})).is_err());
    for invalid in [
        json!({"effort":"not/a/pointer"}),
        json!({"effort":"/bad~2escape"}),
        json!({"budget_min":"/min"}),
    ] {
        assert!(
            serde_json::from_value::<CapabilityPaths>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}

#[test]
fn malformed_conflicting_foreign_or_unpublished_controls_are_rejected() {
    let caps = Capabilities {
        effort: Some(["high".to_owned()].into()),
        ..Default::default()
    };
    for body in [
        json!({"reasoning_effort":"high","reasoning":{"effort":"low"}}),
        json!({"reasoning_effort":null}),
        json!({"reasoning":[]}),
        json!({"thinking":{"type":"adaptive"}}),
        json!({"output_config":{"effort":"high"}}),
    ] {
        assert_eq!(
            caps.check(Provider::OpenAI, Protocol::Chat, &body)
                .unwrap_err()
                .status,
            400
        );
    }
    assert_eq!(
        caps.check(
            Provider::Anthropic,
            Protocol::Messages,
            &json!({"messages":[{"output_config":{"effort":"high"}}]})
        )
        .unwrap_err()
        .status,
        503
    );
    let unknown = Capabilities::default();
    assert_eq!(
        unknown
            .check(
                Provider::OpenAI,
                Protocol::Chat,
                &json!({"reasoning_effort":"high"})
            )
            .unwrap_err()
            .status,
        503
    );
    for malformed in [
        json!("high"),
        json!([1]),
        json!({"high":{"supported":"yes"}}),
    ] {
        assert!(
            Capabilities::read(
                Provider::XAI,
                false,
                &json!({"capabilities":{"reasoning_effort":malformed}}),
                &CapabilityPaths::default()
            )
            .is_err()
        );
    }
}

#[test]
fn alternate_control_spellings_and_misplaced_values_cannot_bypass_admission() {
    let caps = Capabilities::default();
    for body in [
        json!({"generation_config":{"thinking_config":{"thinking_level":"high"}}}),
        json!({"generationConfig":{"thinking_config":{"thinkingLevel":"high"}}}),
        json!({"generationConfig":{"thinkingConfig":{"thinking_level":"high"}}}),
        json!({"thinkingBudget":1}),
        json!({"effort":"high"}),
    ] {
        assert_eq!(
            caps.check(Provider::XAI, Protocol::Responses, &body)
                .unwrap_err()
                .status,
            400
        );
    }
    assert_eq!(
        caps.check(
            Provider::OpenAI,
            Protocol::Responses,
            &json!({"reasoning":{"budget_tokens":1}})
        )
        .unwrap_err()
        .status,
        400
    );
    assert_eq!(
        caps.check(
            Provider::Anthropic,
            Protocol::Messages,
            &json!({"thinking":{"type":"enabled","budgetTokens":1}})
        )
        .unwrap_err()
        .status,
        400
    );
}

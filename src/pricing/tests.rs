use super::*;

fn fixture() -> Value {
    json!({
        "chatgpt/gpt-test":{"litellm_provider":"chatgpt","mode":"responses"},
        "gpt-test":{"litellm_provider":"openai","mode":"chat","input_cost_per_token":0.00001,"output_cost_per_token":0.00005,"cache_read_input_token_cost":0.000001,
            "input_cost_per_token_above_272k_tokens":0.00002,"output_cost_per_token_above_272k_tokens":0.000075,
            "input_cost_per_token_priority":0.00002,"output_cost_per_token_priority":0.0001},
        "claude-test":{"litellm_provider":"anthropic","mode":"chat","input_cost_per_token":0.000002,"output_cost_per_token":0.00001,
            "cache_creation_input_token_cost":0.0000025,"cache_creation_input_token_cost_above_1hr":0.000004,"provider_specific_entry":{"us":1.1,"fast":2}},
        "xai/grok-test":{"litellm_provider":"xai","mode":"responses","input_cost_per_token":0.000002,"output_cost_per_token":0.000006,
            "input_cost_per_token_above_200k_tokens":0.000004,"output_cost_per_token_above_200k_tokens":0.000012},
        "azure/gpt-test":{"litellm_provider":"azure","mode":"chat","input_cost_per_token":-1},
        "openai/container":{"litellm_provider":"openai","mode":"responses"},
        "ft:gpt-test":{"litellm_provider":"openai","mode":"chat"},
        "image-test":{"litellm_provider":"openai","mode":"image_generation"},
        "sample_spec":{}
    })
}
fn parse(v: Value) -> Result<Vec<ProviderBook>, crate::error::ApiError> {
    litellm::parse(&serde_json::to_vec(&v).unwrap(), source::DEFAULT_URL, 100).map(|i| i.providers)
}

#[test]
fn decimal_conversion_is_exact_and_rejects_negative_or_unrepresentable_costs() {
    for (text, expected) in [
        ("1e-5", 10000000),
        ("2.5e-7", 250000),
        ("0", 0),
        ("0.000000000001", 1),
    ] {
        assert_eq!(
            litellm::money(&serde_json::from_str(text).unwrap()).unwrap(),
            expected
        );
    }
    for text in ["-1", "0.0000000000001", "1.01", "null", "\"0.5\""] {
        assert!(litellm::money(&serde_json::from_str(text).unwrap()).is_err());
    }
}
#[test]
fn direct_text_provider_rates_cache_context_and_regional_multiplier() {
    let p = parse(fixture()).unwrap();
    assert!(p.iter().all(|p| p.models.len() == 1));
    let gpt = &p[0].models[0];
    assert_eq!(gpt.rates.input, 10_000_000);
    assert_eq!(
        gpt.long_context.as_ref().unwrap().above_input_tokens,
        272000
    );
    // Priority long-context prices were missing: do not bill those requests at short rates.
    assert!(gpt.billing_rules.variants.is_empty());
    let claude = &p[1].models[0];
    assert_eq!(claude.rates.cache_write_hour, Some(4_000_000));
    assert_eq!(claude.billing_rules.variants[0].rates.input, 2_200_000);
    assert_eq!(
        claude.billing_rules.variants[2].rates.cache_write_hour,
        Some(8_800_000)
    );
    assert_eq!(p[2].models[0].model, "grok-test");
    assert!(p[2].models[0].billing_rules.long_context_inclusive);
    assert_eq!(p[2].models[0].rates.cache_read, None);
}
#[test]
fn incomplete_or_conflicting_source_never_builds_a_partial_book() {
    let mut v = fixture();
    v.as_object_mut().unwrap().remove("claude-test");
    assert!(parse(v).is_err());
    let mut v = fixture();
    v["gpt-test"]["input_cost_per_token"] = json!(-1);
    assert!(parse(v).is_err());
    let mut v = fixture();
    v["openai/gpt-test"] = v["gpt-test"].clone();
    v["openai/gpt-test"]["input_cost_per_token"] = json!(0);
    assert!(parse(v).is_err());
}

#[test]
fn chatgpt_catalog_controls_scope_and_cannot_use_reseller_or_image_prices() {
    let mut v = fixture();
    v["gpt-api-only"] = v["gpt-test"].clone();
    v["chatgpt/image-only"] = json!({"litellm_provider":"chatgpt","mode":"image_generation"});
    v["image-only"] = json!({"litellm_provider":"openai","mode":"image_generation","input_cost_per_token":0.1,"output_cost_per_token":0.1});
    v["chatgpt/gpt-unpriced"] = json!({"litellm_provider":"chatgpt","mode":"responses"});
    v["gpt-unpriced"] = json!({"litellm_provider":"openrouter","mode":"chat","input_cost_per_token":0.1,"output_cost_per_token":0.1});
    v["bedrock/claude-test"] = v["claude-test"].clone();
    v["bedrock/claude-test"]["litellm_provider"] = json!("bedrock");
    v["xai/grok-image"] = json!({"litellm_provider":"xai","mode":"image_generation","input_cost_per_token":0.1,"output_cost_per_token":0.1});
    let imported =
        litellm::parse(&serde_json::to_vec(&v).unwrap(), source::DEFAULT_URL, 100).unwrap();
    assert!(imported.providers.iter().all(|p| p.models.len() == 1));
    assert_eq!(imported.unpriced_models, vec!["chatgpt/gpt-unpriced"]);
    v.as_object_mut()
        .unwrap()
        .retain(|id, _| !id.starts_with("chatgpt/"));
    assert!(
        parse(v).is_err(),
        "missing subscription catalog must not fall back to the whole OpenAI API catalog"
    );
}
#[test]
fn safe_github_resolution_and_daily_schedule() {
    assert_eq!(
        source::github(source::DEFAULT_URL).unwrap().as_str(),
        "https://api.github.com/repos/BerriAI/litellm/contents/model_prices_and_context_window.json?ref=main"
    );
    assert!(source::github("https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json").is_ok());
    for url in [
        "http://github.com/a/b/blob/main/p.json",
        "https://evil.test/p.json",
        "https://user:secret@github.com/a/b/blob/main/p.json",
        "https://github.com/a/b/blob/main/p.json?x=1",
        "https://github.com/a/b/blob/main/p.json#x",
        "https://github.com:444/a/b/blob/main/p.json",
    ] {
        assert!(source::github(url).is_err());
    }
    let mut s = source::State::default();
    assert!(!source::due(&s, 100));
    s.enabled = true;
    assert!(source::due(&s, 100));
    s.last_attempt = 100;
    assert!(!source::due(&s, 86499));
    assert!(source::due(&s, 86500));
    let restored: source::State =
        serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
    assert_eq!(restored.last_attempt, 100);
}
#[test]
fn unchanged_content_is_idempotent_and_new_quotes_do_not_change_old_ones() {
    let first = source::replace(&Book::default(), parse(fixture()).unwrap()).unwrap();
    let same = source::replace(&first, parse(fixture()).unwrap()).unwrap();
    assert_eq!(same.revision, first.revision);
    let mut v = fixture();
    v["gpt-test"]["input_cost_per_token"] = json!(0.00002);
    let next = source::replace(&same, parse(v).unwrap()).unwrap();
    assert_eq!(next.revision, first.revision + 1);
    assert_eq!(next.providers[0].version, 2);
    assert_eq!(next.providers[1].version, 1);
    assert_eq!(first.providers[0].models[0].rates.input, 10_000_000);
}
#[test]
fn downloaded_litellm_snapshot() {
    // Optional integration validation against an independently downloaded complete map.
    let Ok(path) = std::env::var("TEEROUTER_PRICE_FIXTURE") else {
        return;
    };
    let imported = litellm::parse(&std::fs::read(path).unwrap(), source::DEFAULT_URL, 100).unwrap();
    let p = imported.providers;
    for provider in &p {
        println!(
            "{}: {} models",
            provider.provider.api_key_env(),
            provider.models.len()
        );
    }
    let astra = p[0]
        .models
        .iter()
        .find(|m| m.model == "gpt-6-astra")
        .unwrap();
    assert_eq!(astra.rates.input, 10_000_000);
    assert_eq!(p[0].models.len(), 17);
    assert_eq!(
        imported.unpriced_models,
        vec!["chatgpt/gpt-5.3-codex-spark", "chatgpt/gpt-5.3-instant"]
    );
}

#[test]
fn failure_preserves_active_prices_and_reconfiguration_discards_inflight_download() {
    let mut data = crate::vault::Data::default();
    data.official_pricing = source::replace(&Book::default(), parse(fixture()).unwrap()).unwrap();
    let config = data.pricing_source.clone();
    let before = serde_json::to_value(&data.official_pricing).unwrap();
    assert!(
        source::finish(
            &mut data,
            &config,
            Err(crate::error::ApiError::new(
                502,
                "pricing_fetch_failed",
                "test"
            ))
        )
        .is_err()
    );
    assert_eq!(
        serde_json::to_value(&data.official_pricing).unwrap(),
        before
    );
    assert_eq!(
        data.pricing_source.last_error.as_deref(),
        Some("pricing_fetch_failed")
    );
    data.pricing_source.revision += 1;
    let downloaded = Ok((
        litellm::parse(
            &serde_json::to_vec(&fixture()).unwrap(),
            source::DEFAULT_URL,
            100,
        )
        .unwrap(),
        "a".repeat(40),
        "b".repeat(64),
    ));
    assert_eq!(
        source::finish(&mut data, &config, downloaded)
            .unwrap_err()
            .code,
        "pricing_source_conflict"
    );
    assert_eq!(
        serde_json::to_value(&data.official_pricing).unwrap(),
        before
    );
}

#[cfg(feature = "workbuddy-lab")]
#[test]
fn local_test_book_survives_import() {
    let old:Book=serde_json::from_value(json!({"revision":9,"providers":[{"provider":"workbuddy","version":3,"source":"local-test:workbuddy","checked_at":1,"models":[]}]})).unwrap();
    let next = source::replace(&old, parse(fixture()).unwrap()).unwrap();
    assert_eq!(next.revision, 10);
    assert_eq!(
        serde_json::to_value(&next.providers[3]).unwrap(),
        serde_json::to_value(&old.providers[0]).unwrap()
    );
}

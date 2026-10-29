use super::*;
use crate::upstream::Protocol;

pub(crate) fn quote() -> Quote {
    Quote {
        price: serde_json::from_value(
            json!({"pool":"p","model":"m","input":2_000_000,"output":10_000_000,
            "cache_read":100_000,"cache_write":2_500_000,"cache_write_hour":4_000_000}),
        )
        .unwrap(),
        basis: "official_pool_multiplier",
        multiplier_millionths: 800_000,
        official_price_revision: 3,
        official_version: 2,
        official_source: "https://developers.openai.com/api/docs/pricing".into(),
        official_checked_at: 1,
        billing_rules: Rules::default(),
    }
}

#[test]
fn grok_official_200k_inclusive_threshold_prices_all_tokens() {
    let mut q = quote();
    q.multiplier_millionths = 1_000_000;
    q.price.input = 2_000_000;
    q.price.output = 6_000_000;
    q.price.cache_read = Some(500_000);
    q.price.long_context = Some(
        serde_json::from_value(json!({"above_input_tokens":200000,
        "rates":{"input":4_000_000,"output":12_000_000,"cache_read":1_000_000}}))
        .unwrap(),
    );
    q.billing_rules.long_context_inclusive = true;
    for (input, expected) in [(199999, 399908), (200000, 799820)] {
        let o = Observer::new(Provider::XAI, &json!({}), q.clone()).unwrap();
        o.observe(
            Protocol::Chat,
            &json!({"usage":{"prompt_tokens":input,"completion_tokens":10,
            "prompt_tokens_details":{"cached_tokens":100}}}),
            false,
        );
        // (input-100)*input_rate + 100*cache_rate + 10*output_rate
        assert_eq!(o.finish(200, 1)["official_cost_microusd"], expected);
    }
}

#[test]
fn claude_fast_and_us_are_one_exact_price_variant_not_unverified_multipliers() {
    let mut q = quote();
    q.billing_rules.variants.push(Variant {
        service_tier: "default".into(),
        speed: "fast".into(),
        inference_geo: "us".into(),
        rates: TokenRates {
            input: 11_000_000,
            output: 55_000_000,
            cache_read: Some(1_100_000),
            cache_write: Some(13_750_000),
            cache_write_hour: Some(22_000_000),
        },
        long_context: None,
    });
    assert!(validate(Provider::Anthropic, &q.billing_rules));
    assert!(!validate(Provider::OpenAI, &q.billing_rules));
    let o = Observer::new(
        Provider::Anthropic,
        &json!({"speed":"fast","inference_geo":"us"}),
        q,
    )
    .unwrap();
    o.observe(Protocol::Messages,&json!({"usage":{"input_tokens":10,"output_tokens":2,
        "cache_read_input_tokens":20,"cache_creation_input_tokens":0,"service_tier":"standard","speed":"fast","inference_geo":"us"}}),false);
    assert_eq!(o.finish(200, 1)["official_cost_microusd"], 242);
}

#[test]
fn missing_hour_split_and_multi_iteration_context_are_explicitly_pending() {
    let o = Observer::new(
        Provider::Anthropic,
        &json!({"system":[{"type":"text","cache_control":{"type":"ephemeral","ttl":"1h"}}]}),
        quote(),
    )
    .unwrap();
    o.observe(
        Protocol::Messages,
        &json!({"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":100}}),
        false,
    );
    assert_eq!(o.finish(200, 1)["reason"], "cache_duration_usage_missing");
    let o = Observer::new(Provider::Anthropic, &json!({}), quote()).unwrap();
    o.observe(Protocol::Messages,&json!({"usage":{"input_tokens":1,"output_tokens":1,"iterations":[{"type":"message"},{"type":"compaction"}]}}),false);
    assert_eq!(o.finish(200, 1)["reason"], "iteration_pricing_unavailable");
}

#[test]
fn openai_cache_and_reasoning_are_not_double_counted() {
    let o = Observer::new(Provider::OpenAI, &json!({}), quote()).unwrap();
    o.observe(Protocol::Responses,&json!({"usage":{"input_tokens":100,"output_tokens":20,
        "input_tokens_details":{"cached_tokens":30,"cache_write_tokens":10},"output_tokens_details":{"reasoning_tokens":15}}}),false);
    let r = o.finish(200, 1);
    assert_eq!(r["state"], "calculated", "{r}");
    assert_eq!(r["official_cost_microusd"], 348); // 60*2 + 30*.1 + 10*2.5 + 20*10
    assert_eq!(r["charge_microusd"], 279);
    assert_eq!(r["official_cost_picousd"], "348000000");
}

#[test]
fn claude_stream_merges_native_cumulative_usage_and_cache_durations() {
    let o = Observer::new(
        Provider::Anthropic,
        &json!({"messages":[{"role":"user","content":"hello"}]}),
        quote(),
    )
    .unwrap();
    let start = json!({"type":"message_start","message":{"usage":{"input_tokens":60,"output_tokens":1,
        "cache_read_input_tokens":30,"cache_creation_input_tokens":10,
        "cache_creation":{"ephemeral_5m_input_tokens":6,"ephemeral_1h_input_tokens":4}}}});
    o.observe(Protocol::Messages, &start, true);
    assert_eq!(o.finish(499, 1)["coverage"], "partial");
    for _ in 0..2 {
        o.observe(
            Protocol::Messages,
            &json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}),
            true,
        );
    }
    let r = o.finish(200, 1);
    assert_eq!(r["official_cost_microusd"], 354, "{r}"); // 120 + 3 + 15 + 16 + 200
    assert_eq!(r["charge_microusd"], 284);
    assert_eq!(r["usage"]["input"], 60); // Native count, not converted prompt total.
}

#[test]
fn claude_context_threshold_includes_cached_input_and_switches_entire_request() {
    let mut q = quote();
    q.price.long_context=Some(serde_json::from_value(json!({"above_input_tokens":100,"rates":{"input":4_000_000,"output":20_000_000,"cache_read":200_000}})).unwrap());
    for (input, expected) in [(70, 343), (71, 690)] {
        let o = Observer::new(Provider::Anthropic, &json!({}), q.clone()).unwrap();
        o.observe(Protocol::Messages,&json!({"usage":{"input_tokens":input,"output_tokens":20,"cache_read_input_tokens":30}}),false);
        // At 100: 70*2 + 30*.1 + 20*10 = 343. Above: 71*4 + 30*.2 + 20*20 = 690.
        assert_eq!(o.finish(200, 1)["official_cost_microusd"], expected);
    }
}

#[test]
fn grok_chat_and_responses_reasoning_formats_have_the_same_charge() {
    let chat = Observer::new(Provider::XAI, &json!({}), quote()).unwrap();
    chat.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":32,"completion_tokens":9,"total_tokens":135,
        "completion_tokens_details":{"reasoning_tokens":94},"cost_in_usd_ticks":1}}),
        false,
    );
    let responses = Observer::new(Provider::XAI, &json!({}), quote()).unwrap();
    responses.observe(
        Protocol::Responses,
        &json!({"usage":{"input_tokens":32,"output_tokens":103,"total_tokens":135,
        "output_tokens_details":{"reasoning_tokens":94},"cost_in_usd_ticks":1}}),
        false,
    );
    let a = chat.finish(200, 1);
    let b = responses.finish(200, 1);
    assert_eq!(a["official_cost_microusd"], 1094, "{a}");
    assert_eq!(a["charge_microusd"], b["charge_microusd"]);
    assert_eq!(a["usage"]["reported_cost_in_usd_ticks"], 1); // Discounted actual cost never replaces list price.
}

#[test]
fn actual_tier_selects_frozen_variant_including_fallback_to_standard() {
    let mut q = quote();
    q.billing_rules.variants.push(Variant {
        service_tier: "priority".into(),
        speed: "standard".into(),
        inference_geo: "global".into(),
        rates: TokenRates {
            input: 4_000_000,
            output: 20_000_000,
            cache_read: Some(200_000),
            cache_write: None,
            cache_write_hour: None,
        },
        long_context: None,
    });
    for (tier, expected) in [("priority", 240), ("default", 120)] {
        let o = Observer::new(
            Provider::OpenAI,
            &json!({"service_tier":"priority"}),
            q.clone(),
        )
        .unwrap();
        o.observe(
            Protocol::Responses,
            &json!({"service_tier":tier,"usage":{"input_tokens":10,"output_tokens":10}}),
            false,
        );
        assert_eq!(o.finish(200, 1)["official_cost_microusd"], expected);
    }
    assert!(
        Observer::new(
            Provider::OpenAI,
            &json!({"service_tier":"priority"}),
            quote()
        )
        .is_err()
    );
    let auto = Observer::new(Provider::OpenAI, &json!({"service_tier":"auto"}), quote()).unwrap();
    auto.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":1,"completion_tokens":1}}),
        false,
    );
    assert_eq!(auto.finish(200, 1)["reason"], "actual_service_tier_missing");
    let premium = Observer::new(Provider::OpenAI, &json!({"service_tier":"priority"}), q).unwrap();
    premium.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":1,"completion_tokens":1}}),
        false,
    );
    assert_eq!(
        premium.finish(200, 1)["reason"],
        "actual_service_tier_missing"
    );
}

#[test]
fn responses_auto_events_wait_for_actual_tier_and_preserve_cache_pricing() {
    let mut q = quote();
    q.billing_rules.variants.push(Variant {
        service_tier: "priority".into(),
        speed: "standard".into(),
        inference_geo: "global".into(),
        rates: TokenRates {
            input: 4_000_000,
            output: 20_000_000,
            cache_read: Some(200_000),
            cache_write: None,
            cache_write_hour: None,
        },
        long_context: None,
    });
    for (actual, expected) in [("default", 2210), ("priority", 4420)] {
        for request in [json!({}), json!({"service_tier":"auto"})] {
            let o = Observer::new(Provider::OpenAI, &request, q.clone()).unwrap();
            // The Codex backend echoes the selector before it chooses a tier.
            for kind in ["response.created", "response.in_progress"] {
                o.observe(
                    Protocol::Responses,
                    &json!({"type":kind,
                    "response":{"status":"in_progress","service_tier":"auto","usage":null}}),
                    true,
                );
            }
            o.observe(
                Protocol::Responses,
                &json!({"type":"response.completed",
                "response":{"status":"completed","service_tier":actual,"usage":{
                    "input_tokens":1200,"input_tokens_details":{"cached_tokens":100},
                    "output_tokens":0,"total_tokens":1200}}}),
                true,
            );
            let b = o.finish(200, 1);
            assert_eq!(b["state"], "calculated", "{b}");
            assert_eq!(b["usage_source"], "official");
            assert_eq!(b["coverage"], "complete");
            assert_eq!(b["dimensions"]["service_tier"], actual);
            assert_eq!(b["official_cost_microusd"], expected);
            assert_eq!(b["charge_microusd"], expected * 8 / 10);
        }
    }
}

#[test]
fn responses_auto_without_actual_tier_never_assumes_standard_price() {
    for final_tier in [json!("auto"), Value::Null] {
        let o = Observer::new(Provider::OpenAI, &json!({}), quote()).unwrap();
        o.observe(
            Protocol::Responses,
            &json!({"type":"response.created",
            "response":{"service_tier":"auto","usage":null}}),
            true,
        );
        o.observe(
            Protocol::Responses,
            &json!({"type":"response.completed",
            "response":{"service_tier":final_tier,"usage":{"input_tokens":10,"output_tokens":2}}}),
            true,
        );
        let b = o.finish(200, 1);
        assert_eq!(b["state"], "pending", "{b}");
        assert_eq!(b["reason"], "actual_service_tier_missing");
        assert_eq!(b["usage"]["input"], 10);
        assert!(b.get("estimate").is_none());
    }
}

#[test]
fn responses_actual_tier_does_not_clear_other_billing_errors() {
    for (bad, reason) in [
        (json!("not-a-tier"), "unknown_service_tier"),
        (json!(42), "invalid_service_tier"),
    ] {
        let o = Observer::new(Provider::OpenAI, &json!({}), quote()).unwrap();
        o.observe(
            Protocol::Responses,
            &json!({"type":"response.created",
            "response":{"service_tier":bad}}),
            true,
        );
        o.observe(
            Protocol::Responses,
            &json!({"type":"response.completed",
            "response":{"service_tier":"default","usage":{"input_tokens":10,"output_tokens":2}}}),
            true,
        );
        let b = o.finish(200, 1);
        assert_eq!(b["state"], "pending");
        assert_eq!(b["reason"], reason);
    }
}

#[test]
fn malformed_details_and_conflicting_counts_never_silently_use_full_input_price() {
    for (provider, protocol, usage, reason) in [
        (
            Provider::OpenAI,
            Protocol::Responses,
            json!({"input_tokens":10,"output_tokens":1,"input_tokens_details":[]}),
            "invalid_usage_object",
        ),
        (
            Provider::OpenAI,
            Protocol::Responses,
            json!({"input_tokens":10,"output_tokens":1,"output_tokens_details":"bad"}),
            "invalid_usage_object",
        ),
        (
            Provider::OpenAI,
            Protocol::Responses,
            json!({"input_tokens":10,"output_tokens":1,"input_tokens_details":{"cache_creation_tokens":1,"cache_write_tokens":2}}),
            "inconsistent_cache_usage",
        ),
        (
            Provider::Anthropic,
            Protocol::Messages,
            json!({"input_tokens":10,"output_tokens":1,"cache_creation":true}),
            "invalid_usage_object",
        ),
        (
            Provider::Anthropic,
            Protocol::Messages,
            json!({"input_tokens":10,"output_tokens":1,"server_tool_use":[]}),
            "invalid_usage_object",
        ),
        (
            Provider::Anthropic,
            Protocol::Messages,
            json!({"input_tokens":10,"output_tokens":1,"iterations":{}}),
            "invalid_usage_object",
        ),
        (
            Provider::Anthropic,
            Protocol::Messages,
            json!({"input_tokens":10,"output_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":"10"}}),
            "invalid_usage_number",
        ),
        (
            Provider::XAI,
            Protocol::Chat,
            json!({"prompt_tokens":10,"completion_tokens":1,"total_tokens":20}),
            "inconsistent_total_usage",
        ),
        (
            Provider::OpenAI,
            Protocol::Chat,
            json!({"prompt_tokens":10,"completion_tokens":1,"total_tokens":20}),
            "inconsistent_total_usage",
        ),
    ] {
        let o = Observer::new(provider, &json!({}), quote()).unwrap();
        o.observe(protocol, &json!({"usage":usage}), false);
        let result = o.finish(200, 1);
        assert_eq!(result["reason"], reason, "{result}");
        assert!(result.get("charge_microusd").is_none());
    }
}

#[test]
fn claude_search_is_charged_per_official_count_and_missing_count_is_pending() {
    let mut q = quote();
    q.billing_rules.tools.insert("web_search".into(), 10_000);
    let body = json!({"tools":[{"type":"web_search_20250305","name":"web_search"}]});
    let o = Observer::new(Provider::Anthropic, &body, q).unwrap();
    o.observe(
        Protocol::Messages,
        &json!({"usage":{"input_tokens":10,"output_tokens":10}}),
        false,
    );
    assert_eq!(o.finish(200, 1)["reason"], "tool_usage_missing");
    o.observe(
        Protocol::Messages,
        &json!({"usage":{"server_tool_use":{"web_search_requests":2}}}),
        false,
    );
    assert_eq!(o.finish(200, 1)["official_cost_microusd"], 20120);
    assert!(
        Observer::new(
            Provider::OpenAI,
            &json!({"tools":[{"type":"web_search"}]}),
            quote()
        )
        .is_err()
    );
    assert!(
        Observer::new(
            Provider::XAI,
            &json!({"tools":[{"type":"x_search"}]}),
            quote()
        )
        .is_err()
    );
}

#[test]
fn unsupported_missing_interrupted_or_ambiguous_usage_never_becomes_zero_cost() {
    let cases = [
        (
            json!({"input_tokens":10,"output_tokens":1,"input_tokens_details":{"cached_tokens":11}}),
            "inconsistent_cache_usage",
        ),
        (
            json!({"input_tokens":10,"output_tokens":1,"output_tokens_details":{"audio_tokens":1}}),
            "unsupported_modality_usage",
        ),
        (
            json!({"input_tokens":-1,"output_tokens":1}),
            "invalid_usage_number",
        ),
        (
            json!({"input_tokens":10,"output_tokens":1,"num_server_side_tools_used":1}),
            "tool_usage_breakdown_missing",
        ),
    ];
    for (usage, reason) in cases {
        let o = Observer::new(Provider::OpenAI, &json!({}), quote()).unwrap();
        o.observe(Protocol::Responses, &json!({"usage":usage}), false);
        let r = o.finish(200, 1);
        assert_eq!(r["reason"], reason, "{r}");
        assert!(r.get("charge_microusd").is_none());
    }
    let o = Observer::new(Provider::OpenAI, &json!({}), quote()).unwrap();
    o.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":10,"completion_tokens":1}}),
        false,
    );
    assert_eq!(o.finish(499, 1)["state"], "calculated");
    assert_eq!(o.finish(200, 2)["reason"], "multiple_upstream_attempts");
}

#[test]
fn tiny_costs_round_once_after_multiplier_and_overflow_is_pending() {
    let mut q = quote();
    q.price.input = 1;
    q.price.output = 0;
    q.multiplier_millionths = 200_000;
    let o = Observer::new(Provider::OpenAI, &json!({}), q.clone()).unwrap();
    o.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":6_000_000,"completion_tokens":0}}),
        false,
    );
    assert_eq!(o.finish(200, 1)["charge_microusd"], 2);
    q.price.input = 1_000_000_000_000;
    q.multiplier_millionths = 1_000_000_000_000_000;
    let o = Observer::new(Provider::OpenAI, &json!({}), q).unwrap();
    o.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":u64::MAX,"completion_tokens":0}}),
        false,
    );
    assert_eq!(o.finish(200, 1)["reason"], "cost_overflow");
}

#[test]
fn records_only_allowlisted_metering_fields_not_conversation_or_unknown_data() {
    let o = Observer::new(
        Provider::OpenAI,
        &json!({"messages":[{"content":"PRIVATE_REQUEST"}]}),
        quote(),
    )
    .unwrap();
    o.observe(
        Protocol::Chat,
        &json!({"choices":[{"message":{"content":"PRIVATE_REPLY"}}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"untrusted":"PRIVATE_USAGE"}}),
        false,
    );
    let text = o.finish(200, 1).to_string();
    assert!(!text.contains("PRIVATE"));
}

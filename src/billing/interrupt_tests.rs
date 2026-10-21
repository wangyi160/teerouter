use super::*;
use crate::upstream::Protocol;

fn request() -> Value {
    json!({"messages":[{"role":"user","content":"PRIVATE_REQUEST Hello 世界"}]})
}
fn observer() -> Observer {
    Observer::new(Provider::OpenAI, &request(), tests::quote()).unwrap()
}
fn delta(text: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":text}}]})
}
fn record(number: usize, status: u16, o: &Observer) -> Value {
    json!({"number":number,"status":status,"billing":o.finish(status, 1)})
}

#[test]
fn final_official_usage_is_charged_despite_error_or_disconnect() {
    for status in [200, 403, 499, 500, 502, 504] {
        let o = observer();
        o.headers(status);
        o.observe(
            Protocol::Responses,
            &json!({"type":"response.failed","response":{"usage":{
            "input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":30}}}}),
            true,
        );
        let b = o.finish(status, 1);
        assert_eq!(b["state"], "calculated", "{b}");
        assert_eq!(b["usage_source"], "official");
        assert_eq!(b["official_cost_microusd"], 343);
        assert!(b.get("estimate").is_none());
    }
}

#[test]
fn interrupted_text_estimate_is_independent_of_stream_chunk_boundaries() {
    let a = observer();
    a.observe(Protocol::Chat, &delta("Hello 世界 PRIVATE_REPLY"), true);
    let b = observer();
    for chunk in ["He", "llo ", "世", "界 ", "PRIVATE_", "REPLY"] {
        b.observe(Protocol::Chat, &delta(chunk), true);
    }
    let one = a.finish(499, 1);
    assert_eq!(one, b.finish(502, 1));
    assert_eq!(one["state"], "calculated");
    assert_eq!(one["usage_source"], "estimated");
    assert_eq!(one["estimate"]["cache_policy"], "ordinary_input");
    assert_eq!(one["coverage"], "partial");
    assert!(!one.to_string().contains("PRIVATE_"));
}

#[test]
fn failed_request_with_final_usage_uses_only_official_counts_and_cache_discount() {
    let o = observer();
    o.observe(Protocol::Chat, &delta(&"long output ".repeat(100)), true);
    o.observe(
        Protocol::Chat,
        &json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,
        "prompt_tokens_details":{"cached_tokens":9}}}),
        true,
    );
    let b = o.finish(499, 1);
    assert_eq!(b["usage_source"], "official");
    assert_eq!(b["usage"]["output"], 2);
    assert_eq!(b["official_cost_picousd"], "22900000");
    assert!(b.get("estimate").is_none());
}

#[test]
fn pre_generation_rejections_are_zero_but_ambiguous_timeouts_are_unknown() {
    for status in [400, 401, 402, 403, 404, 413, 422, 429] {
        let o = observer();
        o.headers(status);
        let b = o.finish(status, 1);
        assert_eq!(b["charge_microusd"], 0, "{b}");
        assert_eq!(b["usage_source"], "none");
    }
    let o = observer();
    assert_eq!(o.finish(504, 1)["state"], "pending");
    o.no_generation();
    assert_eq!(o.finish(502, 1)["charge_microusd"], 0);
}

#[test]
fn partial_official_input_and_caches_survive_output_estimation() {
    let o = Observer::new(Provider::Anthropic, &request(), tests::quote()).unwrap();
    o.observe(Protocol::Messages, &json!({"type":"message_start","message":{"usage":{
        "input_tokens":10,"output_tokens":1,"cache_read_input_tokens":50,"cache_creation_input_tokens":0}}}), true);
    o.observe(
        Protocol::Messages,
        &json!({"type":"content_block_delta","index":0,"delta":{"text":"a b c d e f g h i j"}}),
        true,
    );
    let b = o.finish(499, 1);
    assert_eq!(b["usage_source"], "mixed");
    assert_eq!(b["usage"]["input"], 10);
    assert_eq!(b["usage"]["cache_read"], 50);
    assert_eq!(b["estimate"]["input_source"], "upstream");
    assert!(b["usage"]["output"].as_u64().unwrap() > 1);
}

#[test]
fn response_snapshots_do_not_duplicate_already_observed_text_or_tools() {
    let a = observer();
    let b = observer();
    let item = json!({"type":"message","content":[{"type":"output_text","text":"hello world"}]});
    for o in [&a, &b] {
        o.observe(Protocol::Responses, &json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hello world"}), true);
    }
    a.observe(Protocol::Responses, &json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":"hello world"}), true);
    a.observe(
        Protocol::Responses,
        &json!({"type":"response.output_item.done","output_index":0,"item":item}),
        true,
    );
    a.observe(
        Protocol::Responses,
        &json!({"type":"response.failed","response":{"output":[item]}}),
        true,
    );
    assert_eq!(a.finish(502, 1)["usage"], b.finish(502, 1)["usage"]);
    assert_eq!(a.finish(502, 1)["estimate"], b.finish(502, 1)["estimate"]);
}

#[test]
fn successful_responses_without_final_usage_never_use_estimation() {
    for (protocol, value, streaming) in [
        (
            Protocol::Chat,
            json!({"choices":[{"message":{"content":"normal reply"}}]}),
            false,
        ),
        (Protocol::Chat, delta("normal streamed reply"), true),
        (
            Protocol::Responses,
            json!({"type":"response.completed","response":{"status":"completed",
            "output":[{"type":"message","content":[{"text":"normal reply"}]}]}}),
            true,
        ),
        (
            Protocol::Messages,
            json!({"content":[{"type":"text","text":"normal reply"}],
            "usage":{"input_tokens":10}}),
            false,
        ),
    ] {
        let o = observer();
        o.headers(200);
        o.observe(protocol, &value, streaming);
        let b = o.finish(200, 1);
        assert_eq!(b["state"], "pending", "{b}");
        assert_eq!(b["reason"], "final_usage_missing");
        assert!(b.get("estimate").is_none());
        assert!(b.get("charge_microusd").is_none());
    }
}

#[test]
fn errors_require_model_input_and_processing_evidence_before_estimation() {
    for body in [
        json!({}),
        json!({"operation":"models"}),
        json!({"tools":[]}),
        json!({"input":42}),
    ] {
        let o = Observer::new(Provider::OpenAI, &body, tests::quote()).unwrap();
        o.observe(
            Protocol::Chat,
            &delta("not evidence of a model request"),
            true,
        );
        let b = o.finish(502, 1);
        assert_eq!(b["state"], "pending", "{b}");
        assert!(b.get("estimate").is_none());
    }
    for value in [
        json!({"error":{"message":"gateway unavailable"}}),
        json!({"message":"proxy error"}),
        json!({"usage":{"prompt_tokens":0}}),
    ] {
        let o = observer();
        o.headers(502);
        o.observe(Protocol::Chat, &value, false);
        let b = o.finish(502, 1);
        assert_eq!(b["state"], "pending", "{b}");
        assert!(b.get("estimate").is_none());
    }
}

#[test]
fn explicit_model_failure_in_http_200_can_use_observed_output() {
    let o = observer();
    o.headers(200);
    o.observe(
        Protocol::Responses,
        &json!({"status":"failed","error":{"code":"server_error"},
        "output":[{"type":"message","content":[{"text":"partial model output"}]}]}),
        false,
    );
    let b = o.finish(200, 1);
    assert_eq!(b["state"], "calculated", "{b}");
    assert_eq!(b["usage_source"], "estimated");
    assert_eq!(b["upstream_failed"], true);
}

#[test]
fn invalid_usage_never_gets_repaired_by_local_estimation() {
    let o = observer();
    o.observe(
        Protocol::Responses,
        &json!({"type":"response.in_progress","response":{"usage":{
        "input_tokens":10,"output_tokens":1,"total_tokens":99}}}),
        true,
    );
    o.observe(Protocol::Responses, &json!({"type":"response.output_text.delta","delta":"many more visible words than one token"}), true);
    let b = o.finish(502, 1);
    assert_eq!(b["state"], "pending");
    assert_eq!(b["reason"], "inconsistent_total_usage");
}

#[test]
fn retries_reset_usage_and_sum_exact_amounts_before_rounding() {
    let mut q = tests::quote();
    q.price.input = 1;
    q.price.output = 0;
    q.multiplier_millionths = 200_000;
    let first = Observer::new(Provider::OpenAI, &request(), q).unwrap();
    first.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":2_000_000,"completion_tokens":0}}),
        false,
    );
    let second = first.fresh();
    assert_eq!(second.finish(504, 1)["state"], "pending");
    second.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":2_000_000,"completion_tokens":0}}),
        false,
    );
    let b = aggregate(&[record(1, 500, &first), record(2, 200, &second)], 2);
    assert_eq!(b["charge_microusd"], 1);
    assert_eq!(b["official_cost_microusd"], 4);
    assert_eq!(b["coverage"], "complete");
    assert_eq!(aggregate(&[record(1, 200, &first)], 2)["state"], "pending");
}

#[test]
fn unknown_attempt_does_not_erase_other_billable_attempts() {
    let first = observer();
    let second = first.fresh();
    second.observe(
        Protocol::Chat,
        &json!({"usage":{"prompt_tokens":10,"completion_tokens":1}}),
        false,
    );
    let b = aggregate(&[record(1, 502, &first), record(2, 200, &second)], 2);
    assert_eq!(b["state"], "calculated");
    assert_eq!(b["charge_microusd"], 24);
    assert_eq!(b["coverage"], "partial");
    assert_eq!(b["unknown_attempts"], 1);
}

#[test]
fn huge_output_and_nontext_input_never_become_guessed_text_charges() {
    let o = observer();
    o.observe(
        Protocol::Chat,
        &delta(&"x".repeat(2 * 1024 * 1024 + 1)),
        true,
    );
    assert_eq!(o.finish(499, 1)["state"], "pending");
    let o = Observer::new(
        Provider::OpenAI,
        &json!({"input":[{"type":"input_image","image_url":"data:image/png;base64,AA"}]}),
        tests::quote(),
    )
    .unwrap();
    o.observe(Protocol::Chat, &delta("text"), true);
    assert_eq!(o.finish(499, 1)["state"], "pending");
}

use super::*;
use serde_json::json;

#[test]
fn only_documented_signal_locations_count_and_text_is_not_classified() {
    for protocol in [Protocol::Chat, Protocol::Messages, Protocol::Responses] {
        let observer = Observer::new(Provider::OpenAI);
        for value in [
            json!({"choices":[{"message":{"refusal":null,"content":"I cannot help with that"},"finish_reason":"stop","logprobs":{"refusal":[{"token":"no"}]}}]}),
            json!({"choices":[{"message":{"refusal":""},"finish_reason":"length"}]}),
            json!({"metadata":{"refusal":"untrusted"},"output":[{"type":"function_call","arguments":"refusal"}]}),
            json!({"error":{"type":"invalid_request_error","code":"rate_limit_exceeded","message":"content_policy_violation"}}),
            json!({"error":{"code":"content_filter_error","message":"filter service failed"}}),
            json!({"stop_reason":"end_turn","content":[{"type":"text","text":"I refuse"}]}),
        ] {
            observer.observe(protocol, &value, false);
        }
        assert!(observer.report().is_none());
    }
}

#[test]
fn response_deltas_done_and_terminal_snapshot_merge_without_duplicates() {
    let observer = Observer::new(Provider::OpenAI);
    for value in [
        json!({"type":"response.created","response":{"id":"resp-test","model":"m"}}),
        json!({"type":"response.refusal.delta","output_index":2,"content_index":0,"delta":"不能"}),
        json!({"type":"response.refusal.delta","output_index":2,"content_index":0,"delta":"回答"}),
        json!({"type":"response.refusal.done","output_index":2,"content_index":0,"refusal":"不能回答"}),
        json!({"type":"response.output_item.done","output_index":2,"item":{"type":"message","content":[{"type":"refusal","refusal":"不能回答"}]}}),
        json!({"type":"response.completed","response":{"id":"resp-test","output":[]}}),
    ] {
        observer.observe(Protocol::Responses, &value, true);
    }
    let report = observer.report().unwrap();
    assert_eq!(report.parts.len(), 1);
    assert_eq!(report.parts[0].refusal, "不能回答");
    assert_eq!(report.response_id.as_deref(), Some("resp-test"));
    assert!(report.complete && report.streamed);
}

#[test]
fn claude_details_and_visible_text_survive_but_thinking_and_credentials_do_not() {
    let observer = Observer::new(Provider::Anthropic);
    observer.observe(
        Protocol::Messages,
        &json!({"type":"message_start","message":{"id":"msg-1","model":"claude"}}),
        true,
    );
    observer.observe(Protocol::Messages,&json!({"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"PRIVATE_REASONING"}}),true);
    observer.observe(
        Protocol::Messages,
        &json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"不能回答。"}}),
        true,
    );
    observer.observe(Protocol::Messages,&json!({"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"provider explanation","fallback_credit":"SECRET"}}}),true);
    let report = observer.report().unwrap();
    assert_eq!(report.response_text.as_deref(), Some("不能回答。"));
    assert_eq!(
        report.stop_details.as_ref().unwrap().category.as_deref(),
        Some("cyber")
    );
    assert!(!report.complete);
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains("PRIVATE_REASONING") && !encoded.contains("SECRET"));
}

#[test]
fn terminal_only_refusals_and_content_filters_are_not_missed() {
    for (protocol, value) in [
        (
            Protocol::Chat,
            json!({"choices":[{"finish_reason":"content_filter","message":{"content":null}}]}),
        ),
        (
            Protocol::Messages,
            json!({"stop_reason":"refusal","stop_details":{"type":"refusal","category":null,"explanation":null},"content":[]}),
        ),
        (
            Protocol::Responses,
            json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"refusal","refusal":"no"}]}]}}),
        ),
        (
            Protocol::Responses,
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"output":[]}}),
        ),
        (
            Protocol::Responses,
            json!({"type":"response.failed","response":{"error":{"code":"content_policy_violation","message":"policy declined"}}}),
        ),
    ] {
        let observer = Observer::new(Provider::OpenAI);
        observer.observe(protocol, &value, false);
        assert!(observer.detected(), "{value}");
    }
}

#[test]
fn oversized_unicode_and_escaped_refusals_remain_bounded_and_valid_utf8() {
    let observer = Observer::new(Provider::XAI);
    for index in 0..50 {
        observer.observe(Protocol::Responses,&json!({"type":"response.refusal.done","output_index":index,"content_index":0,"refusal":"\u{0001}中文".repeat(9000)}),true);
    }
    let report = observer.report().unwrap();
    assert!(report.truncated);
    assert!(report.parts.len() <= PART_LIMIT);
    assert!(serde_json::to_vec(&report).unwrap().len() <= REPORT_BYTES);
}

#[test]
fn metadata_truncation_is_reported_even_when_refusal_text_is_short() {
    let observer = Observer::new(Provider::Anthropic);
    observer.observe(
        Protocol::Messages,
        &json!({
            "id":"中".repeat(256), "model":"m".repeat(400), "stop_reason":"refusal",
            "stop_details":{"type":"refusal","category":"c".repeat(300)},
            "content":[{"type":"text","text":"Declined"}]
        }),
        false,
    );
    let report = observer.report().unwrap();
    assert!(report.truncated);
    assert!(report.response_id.unwrap().len() <= 256);
    assert_eq!(report.response_model.unwrap().len(), 256);
    assert_eq!(report.stop_details.unwrap().category.unwrap().len(), 128);
    assert_eq!(report.response_text.unwrap(), "Declined");
}

#[test]
fn history_eviction_and_archive_backpressure_preserve_counts_and_inflight_events() {
    let vault = crate::vault::Vault::memory().unwrap();
    let ledger = crate::ledger::Ledger::new(vault.clone());
    let observer = Observer::new(Provider::OpenAI);
    observer.observe(
        Protocol::Chat,
        &json!({"choices":[{"message":{"refusal":"r".repeat(TEXT_BYTES)}}]}),
        false,
    );
    let report = observer.report().unwrap();
    let usage = crate::ledger::Usage {
        requests: 1,
        ..Default::default()
    };
    let mut count = 0;
    while ledger.healthy() {
        count += 1;
        ledger.record(
            "key",
            &usage,
            &json!({"user_id":format!("user-{count}"),"refusal":report}),
            Some(&report),
        );
        assert!(
            count < 4096,
            "byte budget must apply before the event count limit"
        );
    }
    {
        let data = vault.lock().unwrap();
        assert!(data.pending_event_bytes >= PENDING_BYTES);
        assert!(history_bytes(&data.refusals) <= HISTORY_BYTES);
        assert_eq!(
            data.refusals
                .values()
                .map(|r| r.refused_requests)
                .sum::<u64>(),
            count
        );
        assert!(data.refusals.values().all(UserRecord::valid));
        assert!(data.refusals.values().any(|r| r.evicted_events > 0));
    }
    let exported = vault.checkpoint().unwrap();
    // A request admitted before saturation may settle after the export.
    ledger.record(
        "key",
        &usage,
        &json!({"user_id":"inflight","refusal":report}),
        Some(&report),
    );
    vault
        .acknowledge(
            &vault.boot_id,
            exported["state"]["generation"].as_u64().unwrap(),
            exported["state"]["sequence"].as_u64().unwrap(),
        )
        .unwrap();
    assert!(ledger.healthy());
    {
        let data = vault.lock().unwrap();
        assert_eq!(data.events.len(), 1);
        assert_eq!(data.events[0]["event"]["user_id"], "inflight");
        assert_eq!(
            data.pending_event_bytes,
            serde_json::to_vec(&data.events[0]).unwrap().len()
        );
        assert_eq!(data.refusals.len() as u64, count + 1);
        assert_eq!(data.refusals["user-1"].refused_requests, 1);
        assert_eq!(data.refusals["user-1"].recent_events.len(), 0);
    }
    let empty_history = BTreeMap::from([(
        "old-user".into(),
        UserRecord {
            refused_requests: u64::MAX,
            ..Default::default()
        },
    )]);
    assert_eq!(history_bytes(&empty_history), 0);
}

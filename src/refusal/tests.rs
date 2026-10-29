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

fn test_report() -> Report {
    let observer = Observer::new(Provider::OpenAI);
    observer.observe(
        Protocol::Chat,
        &json!({"choices":[{"message":{"refusal":"r".repeat(TEXT_BYTES)}}]}),
        false,
    );
    observer.report().unwrap()
}

#[test]
fn queue_backpressure_never_evicts_and_checkpoint_does_not_consume_refusals() {
    let vault = crate::vault::Vault::memory().unwrap();
    let ledger = crate::ledger::Ledger::new(vault.clone());
    let report = test_report();
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
            &json!({"user_id":format!("user-{count}")}),
            Some(&report),
        );
        assert!(count < 4096);
    }
    {
        let data = vault.lock().unwrap();
        assert!(data.refusal_queue.bytes >= PENDING_BYTES);
        assert_eq!(data.refusal_queue.events.len(), count);
        assert_eq!(data.refusal_queue.events.front().unwrap().id, 1);
        assert!(data.refusals.is_empty());
        assert!(
            data.events
                .iter()
                .all(|e| e["event"].get("refusal").is_none())
        );
    }
    vault.checkpoint_export(None, None).unwrap();
    ledger.record("key", &usage, &json!({"user_id":"inflight"}), Some(&report));
    assert!(!ledger.healthy());
    {
        let mut data = vault.lock().unwrap();
        assert_eq!(data.refusal_queue.events.len(), count + 1);
        assert_eq!(data.events.len(), count + 1);
        assert_eq!(data.refusal_queue.delete_through(count as u64), Some(count));
        assert_eq!(data.refusal_queue.events[0].user_id, "inflight");
        assert_eq!(
            data.refusal_queue.bytes,
            serde_json::to_vec(&data.refusal_queue.events[0])
                .unwrap()
                .len()
        );
    }
    assert!(ledger.healthy());
    ledger.record(
        "key",
        &usage,
        &json!({"user_id":"after-delete"}),
        Some(&report),
    );
    let data = vault.lock().unwrap();
    assert_eq!(data.refusal_queue.last_id, count as u64 + 2);
    assert_eq!(data.refusal_queue.events[1].user_id, "after-delete");
}

#[test]
fn legacy_checkpoint_migration_deduplicates_and_removes_old_payload_copies() {
    let report = test_report();
    let audit = json!({"user_id":"old-user","key":"old-key","request_id":"old-request","model":"chat","created_at":1,"status":200,"attempts":1,"refusal":report});
    let event = Event::from_audit(5, &audit, report.clone());
    let mut data = crate::vault::Data {
        sequence: 10,
        ..Default::default()
    };
    data.refusals.insert(
        "old-user".into(),
        UserRecord {
            refused_requests: 99,
            recent_events: VecDeque::from([event]),
            ..Default::default()
        },
    );
    data.events.push_back(json!({"seq":5,"event":audit}));
    data.events.push_back(json!({"seq":8,"event":audit}));
    restore(&mut data).unwrap();
    assert_eq!(data.refusal_queue.events.len(), 2);
    assert_eq!(data.refusal_queue.events[0].id, 5);
    assert_eq!(data.refusal_queue.events[1].id, 8);
    assert!(data.refusals.is_empty());
    assert!(
        data.events
            .iter()
            .all(|e| e["event"].get("refusal").is_none())
    );
    assert_eq!(data.events[0]["event"]["refusal_id"], 5);
    let bytes = data.refusal_queue.bytes;
    let encoded = serde_json::to_value(&data).unwrap();
    assert!(encoded.get("refusals").is_none());
    assert!(encoded["refusal_queue"].get("bytes").is_none());
    let mut restored: crate::vault::Data = serde_json::from_value(encoded).unwrap();
    restore(&mut restored).unwrap();
    assert_eq!(restored.refusal_queue.bytes, bytes);
    assert_eq!(restored.refusal_queue.delete_through(10), Some(2));
    // An empty restored queue must retain both watermarks and never reuse IDs.
    let mut restored: crate::vault::Data =
        serde_json::from_value(serde_json::to_value(&restored).unwrap()).unwrap();
    restore(&mut restored).unwrap();
    assert_eq!(restored.refusal_queue.last_id, 10);
    assert_eq!(restored.refusal_queue.deleted_through, 10);
    assert_eq!(restored.refusal_queue.bytes, 0);
    assert_eq!(
        restored
            .refusal_queue
            .push(Event::from_audit(0, &audit, report)),
        11
    );
}

#[test]
fn malformed_checkpoint_ids_are_rejected_instead_of_resurrecting_or_overwriting_records() {
    let event = Event::from_audit(1, &json!({"user_id":"u"}), test_report());
    let queue = json!({"last_id":2,"deleted_through":0,"events":[event]});
    let mut cases = Vec::new();
    let mut duplicate = queue.clone();
    duplicate["events"] = json!([event, event]);
    cases.push(duplicate);
    let mut deleted = queue.clone();
    deleted["deleted_through"] = json!(1);
    cases.push(deleted);
    let mut future = queue.clone();
    future["events"][0]["id"] = json!(3);
    cases.push(future);
    let mut missing_user = queue.clone();
    missing_user["events"][0]["user_id"] = json!("");
    cases.push(missing_user);
    let mut invalid_watermark = queue.clone();
    invalid_watermark["deleted_through"] = json!(3);
    cases.push(invalid_watermark);
    for case in cases {
        let mut data = crate::vault::Data {
            refusal_queue: serde_json::from_value(case).unwrap(),
            ..Default::default()
        };
        assert!(restore(&mut data).is_err());
    }
    let mut data = crate::vault::Data {
        refusal_queue: serde_json::from_value(queue).unwrap(),
        ..Default::default()
    };
    data.refusals.insert("u".into(), UserRecord::default());
    assert!(restore(&mut data).is_err());
}

#[test]
fn old_event_without_user_field_gets_its_owner_from_the_user_map() {
    let mut event = serde_json::to_value(Event::from_audit(7, &json!({}), test_report())).unwrap();
    event.as_object_mut().unwrap().remove("user_id");
    let record: UserRecord = serde_json::from_value(json!({"recent_events":[event]})).unwrap();
    let mut data = crate::vault::Data {
        sequence: 9,
        ..Default::default()
    };
    data.refusals.insert("legacy-user".into(), record);
    restore(&mut data).unwrap();
    assert_eq!(data.refusal_queue.events[0].user_id, "legacy-user");
    assert_eq!(data.refusal_queue.events[0].id, 7);
}

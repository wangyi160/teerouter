use super::*;
use crate::upstream::Protocol;

fn safe() -> Value {
    json!({"safe":true,"label":0,"confidence":95,"policy_confidence":96,"usage":{"requests":2}})
}
fn headers() -> HeaderMap {
    HeaderMap::from_iter([(
        "x-lockllm-on-unavailable".parse().unwrap(),
        HeaderValue::from_static("block"),
    )])
}
#[test]
fn pass_requires_complete_verdict_and_confirmed_fail_closed_account() {
    let mut report = Report::unavailable(Reason::ProviderError, 7);
    report.classify(200, &headers(), &safe());
    assert_eq!(report.decision, Decision::Allowed);
    for body in [
        json!({}),
        json!({"safe":true}),
        json!({"safe":true,"label":0,"confidence":90}),
        json!({"safe":true,"label":0,"confidence":0,"policy_confidence":90}),
        json!({"safe":true,"label":0,"confidence":90,"policy_confidence":0}),
        json!({"safe":true,"label":0,"confidence":90,"policy_confidence":90,"detection_status":{"status":"degraded"}}),
        json!({"safe":true,"label":0,"confidence":90,"policy_confidence":90,"policy_warnings":[null]}),
        json!({"safe":true,"label":0,"confidence":90,"policy_confidence":90,"scan_warning":{}}),
    ] {
        let mut report = Report::unavailable(Reason::ProviderError, 7);
        report.classify(200, &headers(), &body);
        assert_eq!(report.decision, Decision::Unavailable);
    }
    for confirmation in [None, Some("allow")] {
        let mut h = HeaderMap::new();
        if let Some(value) = confirmation {
            h.insert("x-lockllm-on-unavailable", value.parse().unwrap());
        }
        let mut report = Report::unavailable(Reason::ProviderError, 7);
        report.classify(200, &h, &safe());
        assert_eq!(report.reason, Reason::FailClosedUnconfirmed);
    }
    // Cached results are legitimate; requests=0 is not an incomplete result.
    let mut cached = safe();
    cached["usage"]["requests"] = json!(0);
    let mut report = Report::unavailable(Reason::ProviderError, 7);
    report.classify(200, &headers(), &cached);
    assert_eq!(report.decision, Decision::Allowed);
}
#[test]
fn violations_are_distinct_from_service_errors_and_never_retain_free_text() {
    for (status, body) in [
        (
            400,
            json!({"error":{"code":"prompt_injection_detected","message":"PRIVATE_PROMPT"}}),
        ),
        (
            403,
            json!({"request_id":"PRIVATE PROMPT","error":{"code":"policy_violation","violated_policies":[{"policy_name":"PRIVATE_PROMPT","violation_details":"PRIVATE_PROMPT","violated_categories":["Sexual Content",{"name":"PRIVATE_PROMPT"}]}]}}),
        ),
        (200, json!({"safe":false,"label":1})),
        (
            200,
            json!({"safe":true,"policy_warnings":[{"policy_name":"Policy","violation_details":"PRIVATE_PROMPT"}]}),
        ),
    ] {
        let mut report = Report::unavailable(Reason::ProviderError, 7);
        report.classify(status, &HeaderMap::new(), &body);
        assert_eq!(report.decision, Decision::Blocked);
        assert_eq!(report.error().unwrap().status, 403);
        assert!(!serde_json::to_string(&report).unwrap().contains("PRIVATE"));
    }
    for status in [301, 400, 401, 402, 403, 429, 500, 502, 503, 504] {
        let mut report = Report::unavailable(Reason::ProviderError, 7);
        report.classify(status, &headers(), &json!({"error":{"message":"PRIVATE"}}));
        assert_eq!(report.decision, Decision::Unavailable);
        assert_eq!(report.error().unwrap().status, 503);
    }
}
#[test]
fn every_protocol_keeps_full_text_roles_tools_and_order() {
    let long = format!("START{}END", "中文 code ".repeat(40_000));
    for (protocol, body, expected) in [
        (
            Protocol::Chat,
            json!({"messages":[{"role":"system","content":"rules"},{"role":"tool","tool_call_id":"call-1","content":long}],"tools":[{"function":{"name":"run","description":"tool-description"}}],"metadata":{"ignored":"SECRET"}}),
            "/messages/1/content",
        ),
        (
            Protocol::Responses,
            json!({"instructions":"rules","input":[{"type":"function_call_output","call_id":"call-1","output":long}],"tools":[{"name":"run","description":"tool-description"}]}),
            "/input/0/output",
        ),
        (
            Protocol::Messages,
            json!({"system":"rules","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":long}]}],"tools":[{"name":"run","description":"tool-description"}]}),
            "/messages/0/content/0/content",
        ),
        (
            Protocol::Gemini,
            json!({"systemInstruction":{"parts":[{"text":"rules"}]},"contents":[{"role":"user","parts":[{"functionResponse":{"name":"run","response":{"output":long}}}]}],"tools":[{"functionDeclarations":[{"name":"run","description":"tool-description"}]}]}),
            "/contents/0/parts/0/functionResponse/response/output",
        ),
    ] {
        let extracted = input::extract(protocol, &body, 16 * 1024 * 1024).unwrap();
        let parsed: Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(parsed.pointer(expected), Some(&json!(long)));
        assert!(extracted.contains("rules") && extracted.contains("tool-description"));
        assert!(!extracted.contains("SECRET"));
        assert_eq!(
            input::extract(protocol, &body, 1024).unwrap_err(),
            Reason::InputTooLarge
        );
    }
}
#[test]
fn invisible_and_multimodal_inputs_are_not_silently_marked_scanned() {
    for body in [
        json!({"input":"hello","previous_response_id":"opaque"}),
        json!({"input":"hello","conversation":"opaque"}),
        json!({"input":"hello","prompt":{"id":"stored-prompt"}}),
        json!({"input":[{"type":"message","content":[{"type":"input_image","image_url":"data:secret"}]}]}),
        json!({"input":[{"type":"reasoning","encrypted_content":"opaque"}]}),
    ] {
        assert_eq!(
            input::extract(Protocol::Responses, &body, 10000).unwrap_err(),
            Reason::UnsupportedContent
        );
    }
}

#[test]
fn tool_schemas_with_media_field_names_are_still_scanned_as_text() {
    let body = json!({"messages":[{"role":"user","content":"hello"}],"tools":[{"type":"function","function":{"name":"inspect","parameters":{"type":"object","properties":{"file_id":{"type":"string"},"audio":{"type":"string"},"image_url":{"type":"string"}}}}}]});
    let input: Value =
        serde_json::from_str(&input::extract(Protocol::Chat, &body, 10_000).unwrap()).unwrap();
    assert_eq!(input, body);
}

#[test]
fn user_history_is_bounded_but_counters_and_last_violation_are_retained() {
    let mut record = UserRecord::default();
    for id in 0..30 {
        let mut result = Report::unavailable(Reason::Timeout, 1);
        if id == 0 {
            result.decision = Decision::Blocked;
            result.reason = Reason::ContentPolicy;
        }
        record.observe(Event {
            id,
            request_id: format!("req-{id}"),
            key: "key".into(),
            model: "model".into(),
            created_at: id,
            result,
        });
    }
    assert_eq!(record.recent_events.len(), 20);
    assert_eq!(record.blocked_requests, 1);
    assert_eq!(record.unavailable_requests, 29);
    assert_eq!(record.last_violation.as_ref().unwrap().id, 0);
}

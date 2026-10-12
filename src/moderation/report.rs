use crate::error::ApiError;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    Allowed,
    Blocked,
    Unavailable,
    Rejected,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    Passed,
    PromptInjection,
    ContentPolicy,
    UnsafeContent,
    TransportError,
    Timeout,
    RateLimit,
    ConcurrencyLimit,
    ProviderError,
    InvalidResponse,
    IncompleteResult,
    FailClosedUnconfirmed,
    UnsupportedContent,
    InputTooLarge,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub decision: Decision,
    pub reason: Reason,
    pub categories: BTreeSet<String>,
    pub scanner_request_id: Option<String>,
    pub scanner_status: Option<u16>,
    pub input_chars: u64,
    pub duration_ms: u64,
    pub config_revision: i64,
}
impl Report {
    pub(super) fn unavailable(reason: Reason, config_revision: i64) -> Self {
        Self {
            decision: Decision::Unavailable,
            reason,
            categories: BTreeSet::new(),
            scanner_request_id: None,
            scanner_status: None,
            input_chars: 0,
            duration_ms: 0,
            config_revision,
        }
    }
    pub(crate) fn error(&self) -> Option<ApiError> {
        match self.decision {
            Decision::Allowed => None,
            Decision::Blocked => Some(ApiError::new(
                403,
                "content_policy_violation",
                "Request blocked by content safety policy",
            )),
            Decision::Unavailable => Some(ApiError::new(
                503,
                "moderation_unavailable",
                "Content safety check is unavailable; request was not forwarded",
            )),
            Decision::Rejected if self.reason == Reason::InputTooLarge => Some(ApiError::new(
                413,
                "moderation_input_too_large",
                "Full input exceeds the moderation service limit",
            )),
            Decision::Rejected => Some(ApiError::new(
                400,
                "moderation_unsupported_content",
                "Input moderation requires visible text; opaque history and non-text content are unsupported",
            )),
        }
    }
    pub(super) fn classify(&mut self, status: u16, headers: &HeaderMap, body: &Value) {
        self.scanner_status = Some(status);
        self.scanner_request_id = body["request_id"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            })
            .map(str::to_owned);
        let code = body.pointer("/error/code").and_then(Value::as_str);
        let blocked = match (status, code) {
            (400, Some("prompt_injection_detected")) => Some(Reason::PromptInjection),
            (403, Some("policy_violation")) => Some(Reason::ContentPolicy),
            _ => None,
        };
        if let Some(reason) = blocked {
            self.block(reason, body);
            return;
        }
        if status == 413 {
            self.decision = Decision::Rejected;
            self.reason = Reason::InputTooLarge;
            return;
        }
        if status != 200 {
            self.reason = if status == 429 {
                Reason::RateLimit
            } else {
                Reason::ProviderError
            };
            return;
        }
        // An explicit violation still blocks even if another check was unavailable.
        if let Some(warnings) = body
            .get("policy_warnings")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
        {
            if !warnings.iter().all(|warning| {
                warning.is_object()
                    && (warning["policy_name"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                        || warning["violated_categories"]
                            .as_array()
                            .is_some_and(|a| !a.is_empty()))
            }) {
                self.reason = Reason::InvalidResponse;
                return;
            }
            self.block(Reason::ContentPolicy, body);
            return;
        }
        if body.get("scan_warning").is_some_and(|v| !v.is_null()) {
            if body["scan_warning"]["label"] == 1 {
                self.block(Reason::PromptInjection, body);
            } else {
                self.reason = Reason::InvalidResponse;
            }
            return;
        }
        if body["safe"] == false || body["label"] == 1 {
            self.block(Reason::UnsafeContent, body);
            return;
        }
        if body.get("detection_status").is_some_and(|v| !v.is_null())
            || headers.contains_key("x-lockllm-detection-status")
        {
            self.reason = Reason::IncompleteResult;
            return;
        }
        // The vendor silently ignores this request header on unsupported accounts.
        // Never infer support from the absence of a degraded-status header.
        if headers
            .get("x-lockllm-on-unavailable")
            .and_then(|v| v.to_str().ok())
            != Some("block")
        {
            self.reason = Reason::FailClosedUnconfirmed;
            return;
        }
        let score = |field: &str| {
            body[field]
                .as_f64()
                .is_some_and(|n| n.is_finite() && n > 0.0 && n <= 100.0)
        };
        if body["safe"] != true
            || body["label"] != 0
            || !score("confidence")
            || !score("policy_confidence")
            || body.get("error").is_some_and(|v| !v.is_null())
            || body
                .get("policy_warnings")
                .is_some_and(|v| !v.is_null() && !v.is_array())
            || headers.get("x-scan-mode").is_some_and(|v| v != "combined")
        {
            self.reason = Reason::InvalidResponse;
            return;
        }
        self.decision = Decision::Allowed;
        self.reason = Reason::Passed;
    }
    fn block(&mut self, reason: Reason, body: &Value) {
        self.decision = Decision::Blocked;
        self.reason = reason;
        if matches!(reason, Reason::PromptInjection | Reason::UnsafeContent) {
            self.categories.insert(
                if reason == Reason::PromptInjection {
                    "prompt_injection"
                } else {
                    "unsafe_content"
                }
                .into(),
            );
        }
        for path in ["/policy_warnings", "/error/violated_policies"] {
            if let Some(policies) = body.pointer(path).and_then(Value::as_array) {
                for policy in policies {
                    if let Some(categories) = policy["violated_categories"].as_array() {
                        for category in categories {
                            let name = category.as_str().or_else(|| category["name"].as_str());
                            self.categories.insert(category_id(name).into());
                        }
                    }
                }
            }
        }
        if self.categories.is_empty() {
            self.categories.insert("content_policy".into());
        }
    }
}

// Never retain arbitrary scanner-generated policy names, descriptions or violation_details:
// those may contain the user's prompt. Unknown labels get one fixed category.
fn category_id(name: Option<&str>) -> &'static str {
    match name.unwrap_or("").to_ascii_lowercase().as_str() {
        "violence" | "violent crimes" | "violent crimes and threats" => "violent_crimes",
        "non-violent crimes" | "non-violent criminal activity" => "non_violent_crimes",
        "sex-related crimes" => "sex_related_crimes",
        "child exploitation" => "child_exploitation",
        "defamation" => "defamation",
        "specialized advice" | "specialized professional advice" => "professional_advice",
        "privacy" | "privacy violations" => "privacy",
        "intellectual property" | "intellectual property concerns" => "intellectual_property",
        "weapons" | "weapons and dangerous materials" => "weapons",
        "hate speech" | "hate speech and discrimination" => "hate_speech",
        "suicide & self-harm" | "suicide and self-harm" => "self_harm",
        "sexual content" => "sexual_content",
        "elections" | "election interference" => "election_interference",
        "code execution abuse" => "code_execution_abuse",
        _ => "other_policy",
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub id: u64,
    pub request_id: String,
    pub key: String,
    pub model: String,
    pub created_at: u64,
    pub result: Report,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct UserRecord {
    pub allowed_requests: u64,
    pub blocked_requests: u64,
    pub unavailable_requests: u64,
    pub rejected_requests: u64,
    pub last_violation: Option<Event>,
    pub recent_events: VecDeque<Event>,
}
impl UserRecord {
    pub(crate) fn observe(&mut self, event: Event) {
        let count = match event.result.decision {
            Decision::Allowed => &mut self.allowed_requests,
            Decision::Blocked => &mut self.blocked_requests,
            Decision::Unavailable => &mut self.unavailable_requests,
            Decision::Rejected => &mut self.rejected_requests,
        };
        *count = count.saturating_add(1);
        if event.result.decision == Decision::Blocked {
            self.last_violation = Some(event.clone());
        }
        if event.result.decision != Decision::Allowed {
            self.recent_events.push_back(event);
            while self.recent_events.len() > 20 {
                self.recent_events.pop_front();
            }
        }
    }
}

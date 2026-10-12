//! Historic LockLLM records, retained for checkpoint compatibility only.
use serde::{Deserialize, Serialize};
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

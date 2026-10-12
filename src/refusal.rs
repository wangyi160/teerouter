//! Observe explicit upstream signals before protocol conversion. Never inspect prompts.
use crate::{Provider, upstream::Protocol};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

#[cfg(test)]
mod tests;

pub(crate) const RECENT_LIMIT: usize = 20;
pub(crate) const HISTORY_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const PENDING_BYTES: usize = 2 * 1024 * 1024;
const TEXT_BYTES: usize = 8192;
const PART_LIMIT: usize = 16;
const REPORT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Part {
    pub output_index: u64,
    pub content_index: u64,
    pub refusal: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StopDetails {
    pub r#type: Option<String>,
    pub category: Option<String>,
    pub explanation: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PolicyError {
    pub code: Option<String>,
    pub r#type: Option<String>,
    pub message: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub provider: Provider,
    pub protocol: String,
    pub streamed: bool,
    pub upstream_status: Option<u16>,
    pub upstream_request_id: Option<String>,
    pub response_id: Option<String>,
    pub response_model: Option<String>,
    pub signals: BTreeSet<String>,
    pub reasons: BTreeSet<String>,
    pub parts: Vec<Part>,
    /// Visible Claude text accompanying an explicit stop signal, possibly partial.
    pub response_text: Option<String>,
    pub stop_details: Option<StopDetails>,
    pub error: Option<PolicyError>,
    pub complete: bool,
    pub truncated: bool,
}

#[derive(Clone)]
pub(crate) struct Observer(Arc<Mutex<Tracker>>);
struct Tracker {
    report: Report,
    parts: BTreeMap<(u64, u64), String>,
    visible: String,
    visible_truncated: bool,
}
fn clipped(s: &str, limit: usize) -> String {
    let mut end = s.len().min(limit);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].into()
}
fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Chat => "chat_completions",
        Protocol::Messages => "messages",
        Protocol::Responses => "responses",
    }
}
impl Observer {
    pub(crate) fn new(provider: Provider) -> Self {
        Self(Arc::new(Mutex::new(Tracker {
            report: Report {
                provider,
                protocol: String::new(),
                streamed: false,
                upstream_status: None,
                upstream_request_id: None,
                response_id: None,
                response_model: None,
                signals: BTreeSet::new(),
                reasons: BTreeSet::new(),
                parts: Vec::new(),
                response_text: None,
                stop_details: None,
                error: None,
                complete: false,
                truncated: false,
            },
            parts: BTreeMap::new(),
            visible: String::new(),
            visible_truncated: false,
        })))
    }
    pub(crate) fn headers(&self, response: &reqwest::Response, protocol: Protocol) {
        let mut t = self.0.lock().unwrap();
        t.report.protocol = protocol_name(protocol).into();
        t.report.upstream_status = Some(response.status().as_u16());
        t.report.upstream_request_id = ["x-request-id", "request-id"]
            .into_iter()
            .find_map(|key| response.headers().get(key).and_then(|v| v.to_str().ok()))
            .map(|s| t.text(s, 256));
    }
    pub(crate) fn observe(&self, protocol: Protocol, value: &Value, streamed: bool) {
        let mut t = self.0.lock().unwrap();
        t.report.protocol = protocol_name(protocol).into();
        t.report.streamed |= streamed;
        t.identity(value);
        t.error(value);
        match protocol {
            Protocol::Chat => t.chat(value),
            Protocol::Messages => t.messages(value),
            Protocol::Responses => t.responses(value),
        }
        if !streamed {
            t.report.complete = true;
        }
    }
    pub(crate) fn complete(&self) {
        self.0.lock().unwrap().report.complete = true;
    }
    pub(crate) fn detected(&self) -> bool {
        !self.0.lock().unwrap().report.signals.is_empty()
    }
    pub(crate) fn report(&self) -> Option<Report> {
        let t = self.0.lock().unwrap();
        if t.report.signals.is_empty() {
            return None;
        }
        let mut report = t.report.clone();
        report.parts = t
            .parts
            .iter()
            .map(|(&(output_index, content_index), refusal)| Part {
                output_index,
                content_index,
                refusal: refusal.clone(),
            })
            .collect();
        if report.protocol == "messages" && !t.visible.is_empty() {
            report.response_text = Some(t.visible.clone());
            report.truncated |= t.visible_truncated;
        }
        // Bound escaped JSON too, including control characters and multiple parts.
        while serde_json::to_vec(&report).unwrap().len() > REPORT_BYTES {
            report.truncated = true;
            for part in &mut report.parts {
                part.refusal = clipped(&part.refusal, part.refusal.len() / 2);
            }
            let shrink = |value: &mut Option<String>| {
                if let Some(s) = value {
                    *s = clipped(s, s.len() / 2);
                }
            };
            shrink(&mut report.response_text);
            if let Some(details) = &mut report.stop_details {
                shrink(&mut details.explanation);
            }
            if let Some(error) = &mut report.error {
                shrink(&mut error.message);
            }
        }
        Some(report)
    }
}
impl Tracker {
    fn text(&mut self, value: &str, limit: usize) -> String {
        self.report.truncated |= value.len() > limit;
        clipped(value, limit)
    }
    fn string(&mut self, value: &Value, limit: usize) -> Option<String> {
        value.as_str().map(|s| self.text(s, limit))
    }
    fn signal(&mut self, field: &str) {
        self.report.signals.insert(field.into());
    }
    fn identity(&mut self, v: &Value) {
        if let Some(id) = self.string(&v["id"], 256) {
            self.report.response_id = Some(id);
        }
        if let Some(model) = self.string(&v["model"], 256) {
            self.report.response_model = Some(model);
        }
    }
    fn part(&mut self, key: (u64, u64), value: &Value, append: bool) {
        let Some(text) = value.as_str() else {
            return;
        };
        if !self.parts.contains_key(&key) && self.parts.len() >= PART_LIMIT {
            self.report.truncated = true;
            return;
        }
        if !append {
            self.parts.remove(&key);
        }
        let available =
            TEXT_BYTES.saturating_sub(self.parts.values().map(String::len).sum::<usize>());
        let text_part = clipped(text, available);
        self.report.truncated |= text_part.len() != text.len();
        self.parts.entry(key).or_default().push_str(&text_part);
    }
    fn visible(&mut self, v: &Value) {
        if let Some(text) = v.as_str() {
            let part = clipped(text, TEXT_BYTES.saturating_sub(self.visible.len()));
            self.visible_truncated |= part.len() != text.len();
            self.visible.push_str(&part);
        }
    }
    fn error(&mut self, v: &Value) {
        let error = if v["error"].is_object() {
            &v["error"]
        } else if v["type"] == "error" {
            v
        } else {
            return;
        };
        let policy = |s: &str| {
            matches!(
                s,
                "content_policy_violation" | "content_filter" | "safety_violation"
            )
        };
        for field in ["code", "type"] {
            if let Some(code) = error[field].as_str().filter(|s| policy(s)) {
                self.signal(if field == "code" {
                    "error.code"
                } else {
                    "error.type"
                });
                self.report.reasons.insert(code.into());
                self.report.error = Some(PolicyError {
                    code: self.string(&error["code"], 128),
                    r#type: self.string(&error["type"], 128),
                    message: self.string(&error["message"], TEXT_BYTES),
                });
            }
        }
    }
    fn chat(&mut self, v: &Value) {
        for (n, choice) in v["choices"].as_array().into_iter().flatten().enumerate() {
            let index = choice["index"].as_u64().unwrap_or(n as u64);
            for (field, append) in [("message", false), ("delta", true)] {
                let m = &choice[field];
                if m["refusal"].as_str().is_some_and(|s| !s.is_empty()) {
                    self.signal(if append {
                        "delta.refusal"
                    } else {
                        "message.refusal"
                    });
                    self.part((index, 0), &m["refusal"], append);
                }
                for (part_index, part) in m["content"].as_array().into_iter().flatten().enumerate()
                {
                    if part["type"] == "refusal" {
                        self.signal("content.refusal");
                        self.part((index, part_index as u64), &part["refusal"], append);
                    }
                }
            }
            if choice["finish_reason"] == "content_filter" {
                self.signal("finish_reason");
                self.report.reasons.insert("content_filter".into());
            }
        }
    }
    fn messages(&mut self, v: &Value) {
        match v["type"].as_str() {
            Some("message_start") => {
                self.identity(&v["message"]);
                self.message_body(&v["message"]);
            }
            Some("content_block_start") if v["content_block"]["type"] == "text" => {
                self.visible(&v["content_block"]["text"])
            }
            Some("content_block_delta") if v["delta"]["type"] == "text_delta" => {
                self.visible(&v["delta"]["text"])
            }
            Some("message_delta") => {
                self.message_stop(&v["delta"]);
                self.message_stop(v);
            }
            Some("message_stop") => self.report.complete = true,
            _ => self.message_body(v),
        }
    }
    fn message_body(&mut self, v: &Value) {
        for part in v["content"].as_array().into_iter().flatten() {
            if part["type"] == "text" {
                self.visible(&part["text"]);
            }
        }
        self.message_stop(v);
    }
    fn message_stop(&mut self, v: &Value) {
        if v["stop_reason"] == "refusal" {
            self.signal("stop_reason");
            self.report.reasons.insert("refusal".into());
        }
        if v["stop_details"]["type"] == "refusal" {
            self.signal("stop_details.type");
        }
        if (v["stop_reason"] == "refusal" || v["stop_details"]["type"] == "refusal")
            && v["stop_details"].is_object()
        {
            let details = &v["stop_details"];
            self.report.stop_details = Some(StopDetails {
                r#type: self.string(&details["type"], 128),
                category: self.string(&details["category"], 128),
                explanation: self.string(&details["explanation"], TEXT_BYTES),
            });
        }
    }
    fn response_part(&mut self, index: u64, content: u64, part: &Value) {
        if part["type"] == "refusal" {
            self.signal("content.refusal");
            self.part((index, content), &part["refusal"], false);
        }
    }
    fn response_item(&mut self, index: u64, item: &Value) {
        if item["type"] != "message" {
            return;
        }
        for (content, part) in item["content"].as_array().into_iter().flatten().enumerate() {
            self.response_part(index, content as u64, part);
        }
    }
    fn response_body(&mut self, v: &Value) {
        self.identity(v);
        self.error(v);
        if v["incomplete_details"]["reason"] == "content_filter" {
            self.signal("incomplete_details.reason");
            self.report.reasons.insert("content_filter".into());
        }
        for (index, item) in v["output"].as_array().into_iter().flatten().enumerate() {
            self.response_item(index as u64, item);
        }
    }
    fn responses(&mut self, v: &Value) {
        let key = (
            v["output_index"].as_u64().unwrap_or(0),
            v["content_index"].as_u64().unwrap_or(0),
        );
        match v["type"].as_str() {
            Some("response.refusal.delta") => {
                self.signal("response.refusal.delta");
                self.part(key, &v["delta"], true);
            }
            Some("response.refusal.done") => {
                self.signal("response.refusal.done");
                self.part(key, &v["refusal"], false);
            }
            Some("response.content_part.added" | "response.content_part.done") => {
                self.response_part(key.0, key.1, &v["part"])
            }
            Some("response.output_item.added" | "response.output_item.done") => {
                self.response_item(key.0, &v["item"])
            }
            Some("response.created" | "response.in_progress") => self.response_body(&v["response"]),
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                self.response_body(&v["response"]);
                self.report.complete = true;
            }
            _ => self.response_body(v),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub id: u64,
    pub request_id: String,
    pub key: String,
    pub model: String,
    pub pool: Option<String>,
    pub deployment: Option<String>,
    pub created_at: u64,
    pub status: u16,
    pub attempts: u64,
    pub result: Report,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct UserRecord {
    pub refused_requests: u64,
    pub last_refusal_at: Option<u64>,
    pub evicted_events: u64,
    pub recent_events: VecDeque<Event>,
}
/// Only detail consumes the eviction budget. Lifetime counters must survive even
/// when many users have no recent events left.
pub(crate) fn history_bytes(records: &BTreeMap<String, UserRecord>) -> usize {
    records
        .values()
        .flat_map(|record| &record.recent_events)
        .map(|event| serde_json::to_vec(event).unwrap().len())
        .sum()
}
impl UserRecord {
    pub(crate) fn observe(&mut self, event: Event) {
        self.refused_requests = self.refused_requests.saturating_add(1);
        self.last_refusal_at = Some(event.created_at);
        self.recent_events.push_back(event);
        while self.recent_events.len() > RECENT_LIMIT {
            self.evict();
        }
    }
    pub(crate) fn evict(&mut self) {
        if self.recent_events.pop_front().is_some() {
            self.evicted_events = self.evicted_events.saturating_add(1);
        }
    }
    pub(crate) fn valid(&self) -> bool {
        self.recent_events.len() <= RECENT_LIMIT
            && self.recent_events.iter().all(|e| {
                e.result.parts.len() <= PART_LIMIT
                    && !e.result.signals.is_empty()
                    && serde_json::to_vec(&e.result).is_ok_and(|v| v.len() <= REPORT_BYTES)
            })
    }
}

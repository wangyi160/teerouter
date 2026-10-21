//! Local estimates are explicitly NOT provider usage. Text is transient and bounded.
//! The same frozen o200k tokenizer is used as a surrogate for all three providers;
//! provider-specific framing, invisible reasoning and server-side context are unknown.
use super::*;
use crate::upstream::Protocol;

const MAX_TEXT: usize = 2 * 1024 * 1024;
const MAX_PARTS: usize = 1024;

#[derive(Clone, Default)]
pub(super) struct Estimate {
    input_text: Option<std::sync::Arc<str>>,
    pub model_request: bool,
    pub context_incomplete: bool,
    parts: BTreeMap<String, String>,
    bytes: usize,
    exhausted: bool,
}

fn tokens(text: &str) -> u64 {
    // Fixed UTF-8-safe windows bound BPE work on adversarial unbroken strings.
    // Window boundaries depend on the assembled content, never SSE packet splits.
    let mut remaining = text;
    let mut count = 0;
    while !remaining.is_empty() {
        let mut end = remaining.len().min(4096);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        count += tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(&remaining[..end])
            .len() as u64;
        remaining = &remaining[end..];
    }
    count
}

impl Estimate {
    pub fn new(body: &Value) -> Self {
        fn nontext(v: &Value, depth: usize) -> bool {
            if depth > 64 {
                return true;
            }
            match v {
                Value::Object(o) => {
                    o.get("type").and_then(Value::as_str).is_some_and(|t| {
                        matches!(
                            t,
                            "image"
                                | "image_url"
                                | "input_image"
                                | "input_audio"
                                | "audio"
                                | "video"
                                | "document"
                                | "file"
                                | "input_file"
                        )
                    }) || o.values().any(|v| nontext(v, depth + 1))
                }
                Value::Array(a) => a.iter().any(|v| nontext(v, depth + 1)),
                _ => false,
            }
        }
        let mut input = serde_json::Map::new();
        for key in ["messages", "input", "instructions", "system", "tools"] {
            if let Some(value) = body.get(key) {
                input.insert(key.into(), value.clone());
            }
        }
        let serialized = Value::Object(input).to_string();
        let model_request = body["messages"].as_array().is_some_and(|a| !a.is_empty())
            || body["input"].as_str().is_some_and(|s| !s.is_empty())
            || body["input"].as_array().is_some_and(|a| !a.is_empty());
        Self {
            // Capture bounded evidence, but do not tokenize ordinary requests.
            // Only settlement of a failed model attempt may invoke input/output().
            input_text: (model_request && serialized.len() <= MAX_TEXT && !nontext(body, 0))
                .then(|| serialized.into()),
            model_request,
            context_incomplete: ["previous_response_id", "conversation"]
                .iter()
                .any(|key| body.get(key).is_some_and(|v| !v.is_null())),
            ..Self::default()
        }
    }

    fn part(&mut self, key: String, text: &str, replace: bool) {
        if self.exhausted {
            return;
        }
        let old = self.parts.get(&key).map_or(0, String::len);
        let size = self.bytes - if replace { old } else { 0 } + text.len();
        if size > MAX_TEXT || (!self.parts.contains_key(&key) && self.parts.len() >= MAX_PARTS) {
            self.parts.clear();
            self.bytes = 0;
            self.exhausted = true;
            return;
        }
        let part = self.parts.entry(key).or_default();
        if replace {
            part.clear();
        }
        part.push_str(text);
        self.bytes = size;
    }

    pub fn observe(&mut self, protocol: Protocol, value: &Value, streaming: bool) {
        match protocol {
            Protocol::Chat => {
                if let Some(choices) = value["choices"].as_array() {
                    for (position, choice) in choices.iter().enumerate() {
                        let i = choice["index"].as_u64().unwrap_or(position as u64);
                        let content = if streaming {
                            &choice["delta"]
                        } else {
                            &choice["message"]
                        };
                        for field in ["content", "refusal", "reasoning_content"] {
                            if let Some(text) = content[field].as_str() {
                                self.part(format!("chat:{i}:{field}"), text, !streaming);
                            }
                        }
                        if let Some(tools) = content["tool_calls"].as_array() {
                            for (position, call) in tools.iter().enumerate() {
                                let n = call["index"].as_u64().unwrap_or(position as u64);
                                for field in ["name", "arguments"] {
                                    if let Some(text) = call["function"][field].as_str() {
                                        self.part(
                                            format!("chat:{i}:tool:{n}:{field}"),
                                            text,
                                            !streaming,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Protocol::Messages => {
                if !streaming {
                    if let Some(blocks) = value["content"].as_array() {
                        for (i, block) in blocks.iter().enumerate() {
                            self.block(i as u64, block);
                        }
                    }
                } else {
                    let i = value["index"].as_u64().unwrap_or(0);
                    match value["type"].as_str() {
                        Some("content_block_start") => self.block(i, &value["content_block"]),
                        Some("content_block_delta") => {
                            for field in ["text", "thinking", "partial_json"] {
                                if let Some(text) = value["delta"][field].as_str() {
                                    self.part(format!("msg:{i}:{field}"), text, false);
                                }
                            }
                        }
                        _ => (),
                    }
                }
            }
            Protocol::Responses => {
                let i = value["output_index"].as_u64().unwrap_or(0);
                let c = value["content_index"].as_u64().unwrap_or(0);
                let kind = value["type"].as_str().unwrap_or("");
                for (event, field, suffix) in [
                    ("response.output_text", "text", "text"),
                    ("response.refusal", "refusal", "refusal"),
                    ("response.function_call_arguments", "arguments", "args"),
                ] {
                    if kind == format!("{event}.delta") {
                        if let Some(text) = value["delta"].as_str() {
                            self.part(format!("resp:{i}:{c}:{suffix}"), text, false);
                        }
                    } else if kind == format!("{event}.done") {
                        if let Some(text) = value[field].as_str() {
                            self.part(format!("resp:{i}:{c}:{suffix}"), text, true);
                        }
                    }
                }
                if kind == "response.output_item.done" {
                    self.response_item(i, &value["item"]);
                }
                let envelope = value.get("response").unwrap_or(value);
                if let Some(output) = envelope["output"].as_array() {
                    for (i, item) in output.iter().enumerate() {
                        self.response_item(i as u64, item);
                    }
                }
            }
        }
    }

    fn block(&mut self, i: u64, block: &Value) {
        for field in ["text", "thinking", "name"] {
            if let Some(text) = block[field].as_str() {
                self.part(format!("msg:{i}:{field}"), text, true);
            }
        }
        if let Some(input) = block
            .get("input")
            .filter(|v| !v.is_null() && **v != json!({}))
        {
            self.part(format!("msg:{i}:partial_json"), &input.to_string(), true);
        }
    }

    fn response_item(&mut self, i: u64, item: &Value) {
        if item["type"] == "function_call" {
            for (field, suffix) in [("arguments", "args"), ("name", "name")] {
                if let Some(text) = item[field].as_str() {
                    self.part(format!("resp:{i}:0:{suffix}"), text, true);
                }
            }
        }
        if let Some(content) = item["content"].as_array() {
            for (c, part) in content.iter().enumerate() {
                for field in ["text", "refusal"] {
                    if let Some(text) = part[field].as_str() {
                        self.part(format!("resp:{i}:{c}:{field}"), text, true);
                    }
                }
            }
        }
    }

    pub fn generated(&self) -> bool {
        self.bytes > 0 || self.exhausted
    }
    pub fn input(&self) -> Option<u64> {
        self.input_text.as_deref().map(tokens)
    }
    pub fn output(&self) -> Option<u64> {
        (!self.exhausted).then(|| self.parts.values().map(|s| tokens(s)).sum())
    }
}

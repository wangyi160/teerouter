use std::{
    collections::{HashMap, VecDeque},
    pin::Pin,
};

use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value, json};
use tokio_util::{codec::FramedRead, io::StreamReader};

use crate::{
    Error, Provider, anthropic,
    sse::{SseCodec, SseEvent},
};

pub type CompletionStream = Pin<Box<dyn Stream<Item = Result<Value, Error>> + Send>>;

struct State {
    events: Pin<Box<dyn Stream<Item = Result<SseEvent, Error>> + Send>>,
    anthropic: Option<AnthropicStream>,
    pending: VecDeque<Value>,
    done: bool,
    refusal: Option<crate::refusal::Observer>,
    billing: Option<crate::billing::Observer>,
    protocol: crate::upstream::Protocol,
}

pub(crate) fn decode(
    response: reqwest::Response,
    provider: Provider,
    include_usage: bool,
    limits: crate::resources::Limits,
    refusal: Option<crate::refusal::Observer>,
    billing: Option<crate::billing::Observer>,
) -> CompletionStream {
    let input = crate::resources::bytes(response, limits);
    let events = FramedRead::new(StreamReader::new(input), SseCodec::bounded(limits.event)).fuse();
    let state = State {
        events: Box::pin(events),
        anthropic: (provider == Provider::Anthropic).then(|| AnthropicStream::new(include_usage)),
        pending: VecDeque::new(),
        done: false,
        refusal,
        billing,
        protocol: if provider == Provider::Anthropic {
            crate::upstream::Protocol::Messages
        } else {
            crate::upstream::Protocol::Chat
        },
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            loop {
                if let Some(chunk) = state.pending.pop_front() {
                    return Some((Ok(chunk), state));
                }
                if state.done {
                    return None;
                }
                let next = state.events.next().await;
                let result = match next {
                    Some(Ok(event)) => state.accept(event),
                    Some(Err(error)) => Err(error),
                    None => Err(Error::InvalidResponse(
                        "stream ended before its terminal event",
                    )),
                };
                if let Err(error) = result {
                    state.done = true;
                    return Some((Err(error), state));
                }
            }
        },
    ))
}

impl State {
    fn accept(&mut self, event: SseEvent) -> Result<(), Error> {
        if event.data.trim() == "[DONE]" && self.anthropic.is_none() {
            self.done = true;
            if let Some(observer) = &self.refusal {
                observer.complete();
            }
            return Ok(());
        }
        if event.data.is_empty() {
            return Ok(());
        }
        let value: Value = serde_json::from_str(&event.data)?;
        if let Some(observer) = &self.billing {
            observer.observe(self.protocol, &value, true);
        }
        if let Some(observer) = &self.refusal {
            observer.observe(self.protocol, &value, true);
        }
        if let Some(error) = value.get("error").filter(|v| !v.is_null()) {
            return Err(Error::Stream(error.to_string()));
        }
        if let Some(anthropic) = self.anthropic.as_mut() {
            if value["type"] == "message_stop" {
                if !anthropic.finished {
                    return Err(Error::InvalidResponse("message_stop before finish reason"));
                }
                self.done = true;
            } else {
                self.pending.extend(anthropic.accept(value)?);
            }
        } else {
            if !value.get("choices").is_some_and(Value::is_array) {
                return Err(Error::InvalidResponse("missing stream choices array"));
            }
            self.pending.push_back(value);
        }
        Ok(())
    }
}

struct ToolState {
    index: usize,
    has_arguments: bool,
}

struct AnthropicStream {
    id: Value,
    model: Value,
    created: u64,
    usage: Map<String, Value>,
    tools: HashMap<u64, ToolState>,
    include_usage: bool,
    finished: bool,
}

impl AnthropicStream {
    fn new(include_usage: bool) -> Self {
        Self {
            id: Value::Null,
            model: Value::Null,
            created: anthropic::unix_now(),
            usage: Map::new(),
            tools: HashMap::new(),
            include_usage,
            finished: false,
        }
    }

    fn chunk(&self, delta: Value, finish: Value) -> Value {
        json!({"id":self.id, "object":"chat.completion.chunk", "created":self.created, "model":self.model,
            "choices":[{"index":0, "delta":delta, "finish_reason":finish}]})
    }

    fn accept(&mut self, event: Value) -> Result<Vec<Value>, Error> {
        if self.id.is_null()
            && !matches!(
                event["type"].as_str(),
                Some("message_start" | "ping" | "error")
            )
        {
            return Err(Error::InvalidResponse("content before message_start"));
        }
        match event["type"].as_str() {
            Some("message_start") => {
                let message = &event["message"];
                if !message["id"].is_string() || !message["model"].is_string() {
                    return Err(Error::InvalidResponse("message_start needs id and model"));
                }
                self.id = message["id"].clone();
                self.model = message["model"].clone();
                if let Some(usage) = message["usage"].as_object() {
                    self.usage.extend(usage.clone());
                }
                Ok(vec![self.chunk(
                    json!({"role":"assistant", "content":""}),
                    Value::Null,
                )])
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                if block["type"] == "tool_use" {
                    let index = self.tools.len();
                    let initial = block
                        .get("input")
                        .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()));
                    self.tools.insert(
                        block_index(&event)?,
                        ToolState {
                            index,
                            has_arguments: initial.is_some(),
                        },
                    );
                    return Ok(vec![self.chunk(json!({"tool_calls":[{"index":index, "id":block["id"],
                        "type":"function", "function":{"name":block["name"], "arguments":initial.map(Value::to_string).unwrap_or_default()}}]}), Value::Null)]);
                }
                if block["type"] == "text" {
                    return Ok(vec![
                        self.chunk(json!({"content":block["text"]}), Value::Null),
                    ]);
                }
                Ok(vec![
                    self.chunk(json!({"provider_event":event}), Value::Null),
                ])
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                let normalized = match delta["type"].as_str() {
                    Some("text_delta") => json!({"content":delta["text"]}),
                    Some("input_json_delta") => {
                        let tool = self
                            .tools
                            .get_mut(&block_index(&event)?)
                            .ok_or(Error::InvalidResponse("tool delta without tool start"))?;
                        let arguments =
                            delta["partial_json"]
                                .as_str()
                                .ok_or(Error::InvalidResponse(
                                    "tool argument delta must be a string",
                                ))?;
                        tool.has_arguments |= !arguments.is_empty();
                        json!({"tool_calls":[{"index":tool.index, "function":{"arguments":delta["partial_json"]}}]})
                    }
                    Some("thinking_delta") => {
                        json!({"reasoning_content":delta["thinking"], "provider_event":event})
                    }
                    _ => json!({"provider_event":event}),
                };
                Ok(vec![self.chunk(normalized, Value::Null)])
            }
            Some("content_block_stop") => {
                if let Some(tool) = self
                    .tools
                    .get(&block_index(&event)?)
                    .filter(|t| !t.has_arguments)
                {
                    return Ok(vec![self.chunk(
                        json!({"tool_calls":[{"index":tool.index,
                        "function":{"arguments":"{}"}}]}),
                        Value::Null,
                    )]);
                }
                Ok(vec![])
            }
            Some("message_delta") => {
                if let Some(usage) = event["usage"].as_object() {
                    self.usage.extend(usage.clone());
                }
                let reason = event["delta"]["stop_reason"].as_str();
                let Some(reason) = reason else {
                    return Ok(vec![]);
                };
                self.finished = true;
                let finish = self.chunk(json!({}), anthropic::finish_reason(reason).into());
                if !self.include_usage {
                    return Ok(vec![finish]);
                }
                let mut usage = self.chunk(json!({}), Value::Null);
                usage["choices"] = json!([]);
                usage["usage"] = anthropic::usage(&Value::Object(self.usage.clone()));
                Ok(vec![finish, usage])
            }
            Some("ping") => Ok(vec![]),
            Some("error") => Err(Error::Stream(event.to_string())),
            Some(_) => Ok(vec![
                self.chunk(json!({"provider_event":event}), Value::Null),
            ]),
            None => Err(Error::InvalidResponse("missing Anthropic event type")),
        }
    }
}

fn block_index(event: &Value) -> Result<u64, Error> {
    event["index"]
        .as_u64()
        .ok_or(Error::InvalidResponse("missing content block index"))
}

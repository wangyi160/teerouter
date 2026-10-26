use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use serde_json::Value;
use tokio_util::{codec::FramedRead, io::StreamReader};

use crate::{
    Client, Error, Provider,
    sse::{SseCodec, SseEvent},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Chat,
    Messages,
    Responses,
}

impl Protocol {
    pub(crate) fn supports(self, provider: Provider) -> bool {
        match self {
            Self::Chat => provider != Provider::Anthropic,
            Self::Messages => provider == Provider::Anthropic,
            Self::Responses => matches!(provider, Provider::OpenAI | Provider::XAI),
        }
    }
}

pub(crate) struct Frame {
    pub event: Option<String>,
    pub data: String,
    pub value: Option<Value>,
}

impl Frame {
    pub fn json(value: Value) -> Self {
        Self {
            event: None,
            data: value.to_string(),
            value: Some(value),
        }
    }
    pub fn done() -> Self {
        Self {
            event: None,
            data: "[DONE]".into(),
            value: None,
        }
    }
}

pub(crate) type EventStream = Pin<Box<dyn Stream<Item = Result<Frame, Error>> + Send>>;
pub(crate) enum Output {
    Json(Value),
    Stream(EventStream),
}

pub(crate) async fn call(
    client: &Client,
    protocol: Protocol,
    model: &str,
    body: Value,
    streaming: bool,
    headers: HeaderMap,
) -> Result<Output, Error> {
    let response = client.native(protocol, model, body, headers).await?;
    let codex = client.oauth.is_some() && client.provider == Provider::OpenAI;
    if codex {
        return crate::oauth::chatgpt::decode(
            response,
            streaming,
            client.limits,
            client.refusal.clone(),
            client.billing.clone(),
        )
        .await;
    }
    decode_native(
        response,
        protocol,
        streaming,
        client.limits,
        client.refusal.clone(),
        client.billing.clone(),
    )
    .await
}

pub(crate) async fn decode_native(
    response: reqwest::Response,
    protocol: Protocol,
    streaming: bool,
    limits: crate::resources::Limits,
    refusal: Option<crate::refusal::Observer>,
    billing: Option<crate::billing::Observer>,
) -> Result<Output, Error> {
    if !streaming {
        let value = crate::resources::json(response, limits.bytes).await?;
        if let Some(observer) = &billing {
            observer.observe(protocol, &value, false);
        }
        if let Some(observer) = &refusal {
            observer.observe(protocol, &value, false);
        }
        if value.get("error").is_some_and(|v| !v.is_null()) || value["status"] == "failed" {
            return Err(Error::InvalidResponse("provider returned an error body"));
        }
        let valid = match protocol {
            Protocol::Messages => value["content"].is_array(),
            Protocol::Responses => value["output"].is_array(),
            Protocol::Chat => value["choices"].is_array(),
        };
        if !valid {
            return Err(Error::InvalidResponse("missing native response content"));
        }
        return Ok(Output::Json(value));
    }
    if !response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        })
    {
        return Err(Error::InvalidResponse("expected text/event-stream"));
    }
    let events = FramedRead::new(
        StreamReader::new(crate::resources::bytes(response, limits)),
        SseCodec::bounded(limits.event),
    );
    struct State {
        events: Pin<Box<dyn Stream<Item = Result<SseEvent, Error>> + Send>>,
        terminal: bool,
        refusal: Option<crate::refusal::Observer>,
        billing: Option<crate::billing::Observer>,
        done: bool,
    }
    let state = State {
        events: Box::pin(events),
        terminal: false,
        refusal,
        billing,
        done: false,
    };
    Ok(Output::Stream(Box::pin(futures_util::stream::unfold(
        state,
        move |mut state| async move {
            if state.done {
                return None;
            }
            loop {
                let item = match state.events.next().await {
                    None if state.terminal => return None,
                    None => Err(Error::InvalidResponse(
                        "native stream ended before completion",
                    )),
                    Some(Err(error)) => Err(error),
                    Some(Ok(event)) => {
                        if event.data.is_empty() {
                            continue;
                        }
                        if event.data == "[DONE]" {
                            if protocol == Protocol::Responses && !state.terminal {
                                Err(Error::InvalidResponse(
                                    "Responses stream ended before completion",
                                ))
                            } else {
                                state.terminal = true;
                                state.done = true;
                                if let Some(observer) = &state.refusal {
                                    observer.complete();
                                }
                                Ok(Frame::done())
                            }
                        } else {
                            serde_json::from_str::<Value>(&event.data)
                                .map_err(Error::from)
                                .and_then(|value| {
                                    if let Some(observer) = &state.billing {
                                        observer.observe(protocol, &value, true);
                                    }
                                    if let Some(observer) = &state.refusal {
                                        observer.observe(protocol, &value, true);
                                    }
                                    if value.get("error").is_some_and(|v| !v.is_null())
                                        || matches!(
                                            value["type"].as_str(),
                                            Some(
                                                "error" | "response.failed" | "response.cancelled"
                                            )
                                        )
                                    {
                                        return Err(Error::Stream(
                                            "upstream reported an error".into(),
                                        ));
                                    }
                                    if protocol == Protocol::Chat && !value["choices"].is_array() {
                                        return Err(Error::InvalidResponse(
                                            "missing stream choices array",
                                        ));
                                    }
                                    state.terminal |= match protocol {
                                        Protocol::Messages => value["type"] == "message_stop",
                                        Protocol::Responses => matches!(
                                            value["type"].as_str(),
                                            Some("response.completed" | "response.incomplete")
                                        ),
                                        Protocol::Chat => false,
                                    };
                                    if state.terminal {
                                        state.done = true;
                                    }
                                    Ok(Frame {
                                        event: event.event,
                                        data: event.data,
                                        value: Some(value),
                                    })
                                })
                        }
                    }
                };
                if item.is_err() {
                    state.done = true;
                }
                return Some((item, state));
            }
        },
    ))))
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_hour: u64,
    pub known: bool,
}

impl Tokens {
    pub fn observe(&mut self, protocol: Protocol, value: &Value) {
        let usage = match protocol {
            Protocol::Chat => value.get("usage"),
            Protocol::Messages => value.get("usage").or_else(|| value["message"].get("usage")),
            Protocol::Responses => value
                .get("usage")
                .or_else(|| value["response"].get("usage")),
        };
        let Some(usage) = usage.filter(|v| v.is_object()) else {
            return;
        };
        let details = match protocol {
            Protocol::Chat => &usage["prompt_tokens_details"],
            Protocol::Responses => &usage["input_tokens_details"],
            _ => &Value::Null,
        };
        let (read, write, hour) = match protocol {
            Protocol::Messages => (
                usage["cache_read_input_tokens"].as_u64(),
                usage["cache_creation_input_tokens"].as_u64(),
                usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64(),
            ),
            _ => (
                details["cached_tokens"].as_u64(),
                details["cache_creation_tokens"]
                    .as_u64()
                    .or_else(|| details["cache_write_tokens"].as_u64()),
                details["cache_creation_1h_tokens"].as_u64(),
            ),
        };
        if let Some(n) = read {
            self.cache_read = n;
        }
        if let Some(n) = hour {
            self.cache_write_hour = n;
        }
        if let Some(n) = write {
            self.cache_write = n.saturating_sub(self.cache_write_hour);
        }
        let (input, output) = match protocol {
            Protocol::Chat => (
                usage["prompt_tokens"].as_u64(),
                usage["completion_tokens"].as_u64(),
            ),
            Protocol::Responses => (
                usage["input_tokens"].as_u64(),
                usage["output_tokens"].as_u64(),
            ),
            Protocol::Messages => (
                usage["input_tokens"].as_u64().map(|n| {
                    n.saturating_add(usage["cache_read_input_tokens"].as_u64().unwrap_or(0))
                        .saturating_add(usage["cache_creation_input_tokens"].as_u64().unwrap_or(0))
                }),
                usage["output_tokens"].as_u64(),
            ),
        };
        if let Some(n) = input {
            self.input = n;
        }
        if let Some(n) = output {
            self.output = n;
        }
        self.known |= input.is_some() && output.is_some();
    }
}

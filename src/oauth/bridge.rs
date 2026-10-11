//! Router glue around the upstream message transformations. Native endpoints keep
//! provider payloads; this adapter exposes the common Chat Completions endpoint.
use crate::{
    Client, Error, Provider,
    upstream::{self, Frame, Output, Protocol, Tokens},
};
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

pub(crate) async fn call(
    client: &Client,
    model: &str,
    body: Value,
    streaming: bool,
) -> Result<Output, Error> {
    let responses = client.provider == Provider::OpenAI;
    let protocol = if responses {
        Protocol::Responses
    } else {
        Protocol::Gemini
    };
    let body = if responses {
        super::responses_bridge::request(&body)?
    } else {
        super::gemini_bridge::request(&body)?
    };
    let response = client
        .native(protocol, model, body, streaming, HeaderMap::new())
        .await?;
    let output = if responses {
        super::chatgpt::decode(response, streaming, client.limits).await?
    } else {
        upstream::decode_native(response, protocol, streaming, true, client.limits).await?
    };
    if !streaming {
        let value = match output {
            Output::Json(value) => value,
            Output::Stream(events) => super::chatgpt::collect(events).await?,
        };
        return Ok(Output::Json(if responses {
            super::responses_bridge::response(value, model)?
        } else {
            super::gemini_bridge::response(value, model)?
        }));
    }
    let Output::Stream(events) = output else {
        unreachable!()
    };
    let state = (
        events,
        super::responses_bridge::StreamState::default(),
        super::gemini_bridge::StreamState::default(),
        model.to_owned(),
        false,
        None::<Value>,
    );
    Ok(Output::Stream(Box::pin(futures_util::stream::unfold(
        state,
        move |(mut events, mut rs, mut gs, model, ended, pending)| async move {
            if ended {
                return None;
            }
            if let Some(chunk) = pending {
                return Some((Ok(Frame::json(chunk)), (events, rs, gs, model, false, None)));
            }
            loop {
                let result = match events.next().await? {
                    Err(error) => Err(error),
                    Ok(frame) => {
                        let Some(value) = frame.value else { continue };
                        if responses {
                            rs.event(&value, &model)
                        } else {
                            gs.event(&value, &model)
                        }
                    }
                };
                match result {
                    Ok(None) => continue,
                    Ok(Some(mut chunk)) => {
                        let pending =
                            if let Some(usage) = chunk.as_object_mut().unwrap().remove("usage") {
                                let mut usage_chunk = chunk.clone();
                                usage_chunk["choices"] = json!([]);
                                usage_chunk["usage"] = usage;
                                Some(usage_chunk)
                            } else {
                                None
                            };
                        return Some((
                            Ok(Frame::json(chunk)),
                            (events, rs, gs, model, false, pending),
                        ));
                    }
                    Err(error) => return Some((Err(error), (events, rs, gs, model, true, None))),
                }
            }
        },
    ))))
}

pub(super) fn usage(protocol: Protocol, value: &Value) -> Option<Value> {
    let mut tokens = Tokens::default();
    tokens.observe(protocol, value);
    tokens.known.then(|| {
        let mut value = json!({"prompt_tokens":tokens.input,"completion_tokens":tokens.output,"total_tokens":tokens.input.saturating_add(tokens.output)});
        if tokens.cache_read > 0 || tokens.cache_write > 0 || tokens.cache_write_hour > 0 {
            value["prompt_tokens_details"] = json!({"cached_tokens":tokens.cache_read,"cache_creation_tokens":tokens.cache_write.saturating_add(tokens.cache_write_hour),"cache_creation_1h_tokens":tokens.cache_write_hour});
        }
        value
    })
}

pub(super) fn completion(
    id: &str,
    model: &str,
    message: Value,
    finish: &str,
    usage: Option<Value>,
) -> Value {
    let mut value = json!({"id":id,"object":"chat.completion","created":super::tokens::now() as u64,"model":model,
        "choices":[{"index":0,"message":message,"finish_reason":finish}]});
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    value
}

pub(super) fn chunk(
    id: &str,
    model: &str,
    delta: Value,
    finish: Option<&str>,
    usage: Option<Value>,
) -> Value {
    let mut value = json!({"id":id,"object":"chat.completion.chunk","created":super::tokens::now() as u64,"model":model,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    value
}

//! Selected LiteLLMResponsesTransformationHandler conversions (MIT, 98465fe2).
//! Streaming envelopes and single-choice aggregation are router integration code.
use super::bridge;
use crate::{Error, upstream::Protocol};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) fn request(body: &Value) -> Result<Value, Error> {
    let messages = body["messages"]
        .as_array()
        .ok_or(Error::InvalidRequest("messages must be an array"))?;
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    let mut leading = true;
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or(Error::InvalidRequest("message role missing"))?;
        if role == "system" && leading && message["content"].is_string() {
            instructions.push(message["content"].as_str().unwrap());
            continue;
        }
        leading = false;
        if let Some(items) = message["reasoning_items"].as_array() {
            input.extend(items.iter().cloned());
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":content(&message["content"],"user")?}));
            continue;
        }
        if !matches!(role, "system" | "developer" | "user" | "assistant") {
            return Err(Error::InvalidRequest("unsupported message role"));
        }
        if !message["content"].is_null() {
            input.push(
                json!({"type":"message","role":role,"content":content(&message["content"],role)?}),
            );
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                if call["type"] != "function" {
                    return Err(Error::Unsupported(
                        "use /v1/responses for custom tools".into(),
                    ));
                }
                input.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
            }
        }
    }
    let mut request = json!({"input":input});
    if !instructions.is_empty() {
        request["instructions"] = instructions.join(" ").into();
    }
    for key in [
        "reasoning",
        "previous_response_id",
        "truncation",
        "service_tier",
        "include",
        "prompt_cache_key",
        "prompt_cache_retention",
    ] {
        if let Some(value) = body.get(key) {
            request[key] = value.clone();
        }
    }
    crate::reasoning::effort(
        body.get("reasoning_effort"),
        &mut request,
        &["reasoning", "effort"],
    )?;
    if let Some(tools) = body["tools"].as_array() {
        let tools = tools
            .iter()
            .map(|tool| {
                if tool["type"] != "function" {
                    return Err(Error::Unsupported(
                        "use /v1/responses for native tools".into(),
                    ));
                }
                let mut tool = tool["function"].clone();
                let object = tool
                    .as_object_mut()
                    .ok_or(Error::InvalidRequest("function tool must be an object"))?;
                object.insert("type".into(), "function".into());
                Ok(tool)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        request["tools"] = tools.into();
    }
    if let Some(choice) = body.get("tool_choice") {
        request["tool_choice"] = if choice["type"] == "function" {
            json!({"type":"function","name":choice["function"]["name"]})
        } else {
            choice.clone()
        };
    }
    Ok(request)
}

fn content(value: &Value, role: &str) -> Result<Vec<Value>, Error> {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    if value.is_null() {
        return Ok(vec![]);
    }
    if let Some(text) = value.as_str() {
        return Ok(vec![json!({"type":text_type,"text":text})]);
    }
    value
        .as_array()
        .ok_or(Error::InvalidRequest("invalid message content"))?
        .iter()
        .map(|part| match part["type"].as_str() {
            Some("text") => Ok(json!({"type":text_type,"text":part["text"]})),
            Some("image_url") if role != "assistant" => {
                let image = &part["image_url"];
                let mut value =
                    json!({"type":"input_image","image_url":image.get("url").unwrap_or(image)});
                if let Some(detail) = image.get("detail") {
                    value["detail"] = detail.clone();
                }
                Ok(value)
            }
            Some("refusal") if role == "assistant" => Ok(part.clone()),
            _ => Err(Error::Unsupported(
                "use /v1/responses for this content type".into(),
            )),
        })
        .collect()
}

fn finish(value: &Value, tools: bool) -> &'static str {
    if value["status"] == "incomplete" {
        if value["incomplete_details"]["reason"] == "content_filter" {
            "content_filter"
        } else {
            "length"
        }
    } else if tools {
        "tool_calls"
    } else {
        "stop"
    }
}

pub(super) fn response(value: Value, model: &str) -> Result<Value, Error> {
    let mut text = String::new();
    let mut refusal = String::new();
    let mut tools = Vec::new();
    let mut reasoning = Vec::new();
    let mut summary = String::new();
    for item in value["output"]
        .as_array()
        .ok_or(Error::InvalidResponse("missing Responses output"))?
    {
        match item["type"].as_str() {
            Some("message") => for part in item["content"].as_array().into_iter().flatten() {
                if let Some(s) = part["text"].as_str() {text.push_str(s);}
                if let Some(s) = part["refusal"].as_str() {refusal.push_str(s);}
            },
            Some("function_call") => tools.push(json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}})),
            Some("reasoning") => {
                reasoning.push(item.clone());
                for part in item["summary"].as_array().into_iter().flatten() {if let Some(s) = part["text"].as_str() {summary.push_str(s);}}
            }
            _ => {}
        }
    }
    let mut message =
        json!({"role":"assistant","content":if text.is_empty() {Value::Null} else {text.into()}});
    let reason = finish(&value, !tools.is_empty());
    if !tools.is_empty() {
        message["tool_calls"] = tools.into();
    }
    if !reasoning.is_empty() {
        message["reasoning_items"] = reasoning.into();
        message["reasoning_content"] = summary.into();
    }
    if !refusal.is_empty() {
        message["refusal"] = refusal.into();
    }
    Ok(bridge::completion(
        value["id"].as_str().unwrap_or("chatcmpl-codex"),
        model,
        message,
        reason,
        bridge::usage(Protocol::Responses, &value),
    ))
}

#[derive(Default)]
pub(super) struct StreamState {
    id: String,
    tools: HashMap<u64, usize>,
    arguments: std::collections::HashSet<u64>,
    text: std::collections::HashSet<(u64, u64)>,
    refusals: std::collections::HashSet<(u64, u64)>,
}
impl StreamState {
    pub fn event(&mut self, value: &Value, model: &str) -> Result<Option<Value>, Error> {
        let index = value["output_index"].as_u64().unwrap_or(0);
        let text_index = (index, value["content_index"].as_u64().unwrap_or(0));
        if self.id.is_empty() {
            self.id = value["response"]["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or(super::random_string()?);
        }
        let delta = match value["type"].as_str() {
            Some("response.created") => {
                self.id = value["response"]["id"]
                    .as_str()
                    .unwrap_or("chatcmpl-codex")
                    .into();
                json!({"role":"assistant","content":""})
            }
            Some("response.output_text.delta") => {
                self.text.insert(text_index);
                json!({"content":value["delta"]})
            }
            Some("response.refusal.delta") => {
                self.refusals.insert(text_index);
                json!({"refusal":value["delta"]})
            }
            Some("response.refusal.done") if self.refusals.insert(text_index) => {
                json!({"refusal":value["refusal"]})
            }
            Some("response.content_part.done")
                if value["part"]["type"] == "refusal" && self.refusals.insert(text_index) =>
            {
                json!({"refusal":value["part"]["refusal"]})
            }
            Some("response.output_item.done") if value["item"]["type"] == "message" => {
                let refusal = self.item_refusals(index, &value["item"]);
                if refusal.is_empty() {
                    return Ok(None);
                }
                json!({"refusal":refusal})
            }
            Some("response.reasoning_summary_text.delta") => {
                json!({"reasoning_content":value["delta"]})
            }
            Some("response.output_item.added") if value["item"]["type"] == "function_call" => {
                let next = self.tools.len();
                self.tools.insert(index, next);
                json!({"tool_calls":[{"index":next,"id":value["item"]["call_id"],"type":"function","function":{"name":value["item"]["name"],"arguments":""}}]})
            }
            Some("response.function_call_arguments.delta") => {
                let tool = self
                    .tools
                    .get(&index)
                    .ok_or(Error::InvalidResponse("tool delta without tool start"))?;
                self.arguments.insert(index);
                json!({"tool_calls":[{"index":tool,"function":{"arguments":value["delta"]}}]})
            }
            Some("response.function_call_arguments.done") if !self.arguments.contains(&index) => {
                let tool = self
                    .tools
                    .get(&index)
                    .ok_or(Error::InvalidResponse("tool arguments without tool start"))?;
                self.arguments.insert(index);
                json!({"tool_calls":[{"index":tool,"function":{"arguments":value["arguments"]}}]})
            }
            Some("response.output_item.done") if value["item"]["type"] == "reasoning" => {
                json!({"reasoning_items":[value["item"]]})
            }
            Some("response.output_text.done") if !self.text.contains(&text_index) => {
                self.text.insert(text_index);
                json!({"content":value["text"]})
            }
            Some("response.completed" | "response.incomplete") => {
                let mut refusal = String::new();
                for (index, item) in value["response"]["output"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    refusal.push_str(&self.item_refusals(index as u64, item));
                }
                return Ok(Some(bridge::chunk(
                    &self.id,
                    model,
                    if refusal.is_empty() {
                        json!({})
                    } else {
                        json!({"refusal":refusal})
                    },
                    Some(finish(&value["response"], !self.tools.is_empty())),
                    bridge::usage(Protocol::Responses, value),
                )));
            }
            _ => return Ok(None),
        };
        Ok(Some(bridge::chunk(&self.id, model, delta, None, None)))
    }
    fn item_refusals(&mut self, index: u64, item: &Value) -> String {
        if item["type"] != "message" {
            return String::new();
        }
        let mut text = String::new();
        for (content, part) in item["content"].as_array().into_iter().flatten().enumerate() {
            if part["type"] == "refusal"
                && self.refusals.insert((index, content as u64))
                && let Some(refusal) = part["refusal"].as_str()
            {
                text.push_str(refusal);
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[rstest::rstest]
    fn output_text_done_recovers_each_index_without_repeating_deltas() {
        let mut state = StreamState::default();
        let chunk = state.event(&json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"first"}),"m").unwrap().unwrap();
        assert!(!chunk["id"].as_str().unwrap().is_empty());
        assert!(state.event(&json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":"first"}),"m").unwrap().is_none());
        let chunk = state.event(&json!({"type":"response.output_text.done","output_index":2,"content_index":0,"text":"second"}),"m").unwrap().unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["content"], "second");
    }
}

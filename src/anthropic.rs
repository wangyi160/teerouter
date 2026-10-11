use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::Error;

pub(crate) fn request(input: Value, streaming: bool) -> Result<Value, Error> {
    let input = input
        .as_object()
        .ok_or(Error::InvalidRequest("request must be an object"))?;
    let mut body = Map::new();
    for (key, value) in input {
        match key.as_str() {
            "model" | "temperature" | "top_p" | "top_k" | "metadata" | "thinking"
            | "output_config" | "service_tier" => {
                body.insert(key.clone(), value.clone());
            }
            "messages"
            | "stream"
            | "max_tokens"
            | "max_completion_tokens"
            | "stop"
            | "tools"
            | "tool_choice"
            | "parallel_tool_calls"
            | "reasoning_effort"
            | "stream_options" => {}
            _ => return Err(Error::Unsupported(key.clone())),
        }
    }
    if input.contains_key("max_tokens") && input.contains_key("max_completion_tokens") {
        return Err(Error::InvalidRequest(
            "use only one of max_tokens and max_completion_tokens",
        ));
    }
    let limit = input
        .get("max_completion_tokens")
        .or_else(|| input.get("max_tokens"))
        .cloned()
        .unwrap_or(json!(4096));
    if !limit.as_u64().is_some_and(|n| n > 0) {
        return Err(Error::InvalidRequest(
            "max_tokens must be a positive integer",
        ));
    }
    body.insert("max_tokens".into(), limit);
    body.insert("stream".into(), streaming.into());
    if let Some(stop) = input.get("stop").filter(|v| !v.is_null()) {
        let stops = match stop {
            Value::String(s) => vec![json!(s)],
            Value::Array(items) if items.iter().all(Value::is_string) => items.clone(),
            _ => {
                return Err(Error::InvalidRequest(
                    "stop must be a string or array of strings",
                ));
            }
        };
        body.insert("stop_sequences".into(), stops.into());
    }
    if let Some(options) = input.get("stream_options") {
        let options = options
            .as_object()
            .ok_or(Error::InvalidRequest("stream_options must be an object"))?;
        if options
            .iter()
            .any(|(key, value)| key != "include_usage" || !value.is_boolean())
        {
            return Err(Error::Unsupported("stream_options".into()));
        }
    }
    let messages = input
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(Error::InvalidRequest("messages must be an array"))?;
    let mut system = Vec::new();
    let mut turns: Vec<Value> = Vec::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or(Error::InvalidRequest("missing message role"))?;
        if matches!(role, "system" | "developer") {
            let parts = content(&message["content"])?;
            if parts.iter().any(|p| p["type"] != "text") {
                return Err(Error::InvalidRequest("system content must be text"));
            }
            system.extend(parts);
            continue;
        }
        let (role, parts) = if role == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .ok_or(Error::InvalidRequest("tool message needs tool_call_id"))?;
            (
                "user",
                vec![
                    json!({"type":"tool_result", "tool_use_id":id, "content":content(&message["content"])?}),
                ],
            )
        } else if role == "assistant" && message.get("provider_content").is_some() {
            let parts = message["provider_content"]
                .as_array()
                .ok_or(Error::InvalidRequest("provider_content must be an array"))?;
            (role, parts.clone())
        } else {
            let mut parts = content(&message["content"])?;
            if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
                if role != "assistant" {
                    return Err(Error::InvalidRequest("tool_calls require assistant role"));
                }
                let calls = calls
                    .as_array()
                    .ok_or(Error::InvalidRequest("tool_calls must be an array"))?;
                for call in calls {
                    let args =
                        call["function"]["arguments"]
                            .as_str()
                            .ok_or(Error::InvalidRequest(
                                "tool arguments must be a JSON string",
                            ))?;
                    let args: Value = serde_json::from_str(args).map_err(|_| {
                        Error::InvalidRequest("tool arguments must contain valid JSON")
                    })?;
                    if !args.is_object() {
                        return Err(Error::InvalidRequest(
                            "tool arguments must encode an object",
                        ));
                    }
                    parts.push(json!({
                        "type":"tool_use", "id":required_str(call, "id")?,
                        "name":required_str(&call["function"], "name")?, "input":args,
                    }));
                }
            }
            (role, parts)
        };
        if !matches!(role, "user" | "assistant") {
            return Err(Error::InvalidRequest("unsupported message role"));
        }
        if let Some(last) = turns.last_mut().filter(|last| last["role"] == role) {
            last["content"]
                .as_array_mut()
                .ok_or(Error::InvalidRequest("invalid content"))?
                .extend(parts);
        } else {
            turns.push(json!({"role":role, "content":parts}));
        }
    }
    if turns.is_empty() {
        return Err(Error::InvalidRequest(
            "at least one user or assistant message is required",
        ));
    }
    body.insert("messages".into(), turns.into());
    if !system.is_empty() {
        body.insert("system".into(), system.into());
    }
    if let Some(tools) = input.get("tools") {
        let tools = tools
            .as_array()
            .ok_or(Error::InvalidRequest("tools must be an array"))?;
        let native = tools.iter().map(tool).collect::<Result<Vec<_>, _>>()?;
        body.insert("tools".into(), native.into());
    }
    let choice = input.get("tool_choice").map(tool_choice).transpose()?;
    let parallel = input
        .get("parallel_tool_calls")
        .map(|v| {
            v.as_bool().ok_or(Error::InvalidRequest(
                "parallel_tool_calls must be a boolean",
            ))
        })
        .transpose()?;
    if choice.is_some() || parallel.is_some() {
        let mut choice = choice.unwrap_or(json!({"type":"auto"}));
        if let Some(parallel) = parallel {
            choice["disable_parallel_tool_use"] = (!parallel).into();
        }
        body.insert("tool_choice".into(), choice);
    }
    let mut body = Value::Object(body);
    crate::reasoning::effort(
        input.get("reasoning_effort"),
        &mut body,
        &["output_config", "effort"],
    )?;
    Ok(body)
}

fn required_str<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(Error::InvalidRequest(field))
}

fn content(value: &Value) -> Result<Vec<Value>, Error> {
    match value {
        Value::Null => Ok(vec![]),
        Value::String(text) => Ok(vec![json!({"type":"text", "text":text})]),
        Value::Array(parts) => parts.iter().map(content_part).collect(),
        _ => Err(Error::InvalidRequest(
            "content must be text, an array or null",
        )),
    }
}

fn content_part(part: &Value) -> Result<Value, Error> {
    match part["type"].as_str() {
        Some("text") => {
            required_str(part, "text")?;
            Ok(part.clone())
        }
        Some("image_url") => {
            let url = part["image_url"]["url"]
                .as_str()
                .ok_or(Error::InvalidRequest("image_url.url is required"))?;
            let source = if let Some(data) = url.strip_prefix("data:") {
                let (media_type, data) = data.split_once(";base64,").ok_or(
                    Error::InvalidRequest("image data URL must be base64 encoded"),
                )?;
                json!({"type":"base64", "media_type":media_type, "data":data})
            } else if url.starts_with("https://") || url.starts_with("http://") {
                json!({"type":"url", "url":url})
            } else {
                return Err(Error::InvalidRequest(
                    "image URL must be HTTP(S) or base64 data",
                ));
            };
            Ok(json!({"type":"image", "source":source}))
        }
        Some(kind) => Err(Error::Unsupported(kind.into())),
        None => Err(Error::InvalidRequest("missing content type")),
    }
}

fn tool(value: &Value) -> Result<Value, Error> {
    if value["type"] != "function" {
        return Err(Error::Unsupported(
            "only function tools are supported".into(),
        ));
    }
    let function = &value["function"];
    let schema = function
        .get("parameters")
        .cloned()
        .unwrap_or(json!({"type":"object", "properties":{}}));
    if !schema.is_object() {
        return Err(Error::InvalidRequest("tool parameters must be an object"));
    }
    let mut tool = json!({"name":required_str(function,"name")?, "input_schema":schema});
    for field in ["description", "strict"] {
        if let Some(value) = function.get(field) {
            tool[field] = value.clone();
        }
    }
    Ok(tool)
}

fn tool_choice(value: &Value) -> Result<Value, Error> {
    match value.as_str() {
        Some("auto") => Ok(json!({"type":"auto"})),
        Some("required") => Ok(json!({"type":"any"})),
        Some("none") => Ok(json!({"type":"none"})),
        None if value["type"] == "function" => {
            Ok(json!({"type":"tool", "name":required_str(&value["function"],"name")?}))
        }
        _ => Err(Error::InvalidRequest("unsupported tool_choice")),
    }
}

pub(crate) fn response(body: Value) -> Result<Value, Error> {
    let blocks = body["content"]
        .as_array()
        .ok_or(Error::InvalidResponse("missing content array"))?;
    let text: String = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    let calls = blocks.iter().filter(|b| b["type"] == "tool_use").map(|b| {
        json!({"id":b["id"], "type":"function", "function":{"name":b["name"], "arguments":b["input"].to_string()}})
    }).collect::<Vec<_>>();
    let mut message = json!({"role":"assistant", "content":if text.is_empty() { Value::Null } else { text.into() }, "provider_content":blocks});
    if !calls.is_empty() {
        message["tool_calls"] = calls.into();
    }
    let reasoning: String = blocks
        .iter()
        .filter(|b| b["type"] == "thinking")
        .filter_map(|b| b["thinking"].as_str())
        .collect();
    if !reasoning.is_empty() {
        message["reasoning_content"] = reasoning.into();
    }
    Ok(json!({
        "id":body["id"], "object":"chat.completion", "created":unix_now(), "model":body["model"],
        "choices":[{"index":0, "message":message, "finish_reason":finish_reason(body["stop_reason"].as_str().unwrap_or(""))}],
        "usage":usage(&body["usage"]),
    }))
}

pub(crate) fn finish_reason(reason: &str) -> &str {
    match reason {
        "max_tokens" | "compaction" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        "end_turn" | "stop_sequence" | "" => "stop",
        other => other,
    }
}

pub(crate) fn usage(value: &Value) -> Value {
    let count = |key: &str| value[key].as_u64().unwrap_or(0);
    let cached = count("cache_read_input_tokens");
    let written = count("cache_creation_input_tokens");
    let input = count("input_tokens") + cached + written;
    let output = count("output_tokens");
    let mut result = json!({"prompt_tokens":input, "completion_tokens":output, "total_tokens":input + output,
        "prompt_tokens_details":{"cached_tokens":cached, "cache_creation_tokens":written}});
    if let Some(n) = value["cache_creation"]["ephemeral_1h_input_tokens"].as_u64() {
        result["prompt_tokens_details"]["cache_creation_1h_tokens"] = n.into();
    }
    result
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

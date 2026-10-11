//! Common subset of LiteLLM gemini_convert_messages_with_history and response mapping.
//! Advanced provider fields remain available through the native Gemini endpoint.
use super::bridge;
use crate::{Error, upstream::Protocol};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) fn request(body: &Value) -> Result<Value, Error> {
    for key in body
        .as_object()
        .ok_or(Error::InvalidRequest("request must be an object"))?
        .keys()
    {
        if !matches!(
            key.as_str(),
            "model"
                | "messages"
                | "stream"
                | "stream_options"
                | "tools"
                | "tool_choice"
                | "temperature"
                | "top_p"
                | "max_tokens"
                | "max_completion_tokens"
                | "stop"
                | "response_format"
                | "seed"
                | "n"
                | "user"
                | "reasoning_effort"
                | "generationConfig"
        ) {
            return Err(Error::Unsupported(format!(
                "Gemini CLI Chat parameter: {key}; use the native Gemini endpoint"
            )));
        }
    }
    if body.get("n").is_some_and(|v| v != 1) {
        return Err(Error::Unsupported("Gemini CLI Chat supports n=1".into()));
    }
    let messages = body["messages"]
        .as_array()
        .ok_or(Error::InvalidRequest("messages must be an array"))?;
    let mut contents: Vec<Value> = Vec::new();
    let mut system = Vec::new();
    let mut calls = HashMap::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or(Error::InvalidRequest("message role missing"))?;
        let mut parts = parts(&message["content"])?;
        if matches!(role, "system" | "developer") {
            system.extend(parts);
            continue;
        }
        if role == "assistant" {
            let mut call_parts = Vec::new();
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                if call["type"] != "function" {
                    return Err(Error::Unsupported(
                        "Gemini Chat supports function tools".into(),
                    ));
                }
                let id = call["id"]
                    .as_str()
                    .ok_or(Error::InvalidRequest("tool call ID missing"))?;
                let name = call["function"]["name"]
                    .as_str()
                    .ok_or(Error::InvalidRequest("tool name missing"))?;
                calls.insert(id.to_owned(), name.to_owned());
                let args: Value =
                    serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}"))
                        .map_err(|_| Error::InvalidRequest("tool arguments must be JSON"))?;
                let mut part = json!({"functionCall":{"name":name,"args":args}});
                if let Some(signature) = call["extra_content"]["google"]
                    .get("thought_signature")
                    .or_else(|| call["provider_specific_fields"].get("thought_signature"))
                {
                    part["thoughtSignature"] = signature.clone();
                }
                call_parts.push(part);
            }
            // Do not replay a signature twice on text and function-call parts (upstream fix).
            if let Some(signature) = message["provider_specific_fields"]["thought_signatures"]
                .as_array()
                .and_then(|a| a.first())
                && !call_parts
                    .iter()
                    .any(|part| part.get("thoughtSignature") == Some(signature))
                && let Some(part) = parts.first_mut()
            {
                part["thoughtSignature"] = signature.clone();
            }
            parts.extend(call_parts);
        } else if role == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .ok_or(Error::InvalidRequest("tool result ID missing"))?;
            let name = calls
                .get(id)
                .ok_or(Error::InvalidRequest("tool result has no matching call"))?;
            let result = message["content"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| message["content"].clone());
            let result = if result.is_object() {
                result
            } else {
                json!({"result":result})
            };
            parts = vec![json!({"functionResponse":{"name":name,"response":result}})];
        } else if role != "user" {
            return Err(Error::InvalidRequest("unsupported message role"));
        }
        let role = if role == "assistant" { "model" } else { "user" };
        if parts.is_empty() {
            continue;
        }
        if let Some(last) = contents.last_mut().filter(|last| last["role"] == role) {
            last["parts"].as_array_mut().unwrap().extend(parts);
        } else {
            contents.push(json!({"role":role,"parts":parts}));
        }
    }
    if contents.is_empty() {
        contents.push(json!({"role":"user","parts":[{"text":" "}]}));
    }
    let mut request = json!({"contents":contents});
    if !system.is_empty() {
        request["systemInstruction"] = json!({"parts":system});
    }
    let mut config = body
        .get("generationConfig")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !config.is_object() {
        return Err(Error::InvalidRequest("generationConfig must be an object"));
    }
    crate::reasoning::effort(
        body.get("reasoning_effort"),
        &mut config,
        &["thinkingConfig", "thinkingLevel"],
    )?;
    for (source, target) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("max_tokens", "maxOutputTokens"),
        ("max_completion_tokens", "maxOutputTokens"),
        ("seed", "seed"),
    ] {
        if let Some(value) = body.get(source) {
            config[target] = value.clone();
        }
    }
    if let Some(stop) = body.get("stop") {
        config["stopSequences"] = if stop.is_string() {
            json!([stop])
        } else {
            stop.clone()
        };
    }
    match body["response_format"]["type"].as_str() {
        Some("json_object") => {
            config["responseMimeType"] = "application/json".into();
        }
        Some("json_schema") => {
            config["responseMimeType"] = "application/json".into();
            config["responseJsonSchema"] = body["response_format"]["json_schema"]["schema"].clone();
        }
        Some("text") | None => {}
        _ => return Err(Error::Unsupported("response_format".into())),
    }
    if !config.as_object().unwrap().is_empty() {
        request["generationConfig"] = config;
    }
    if let Some(tools) = body["tools"].as_array() {
        let mut declarations = Vec::new();
        for tool in tools {
            if tool["type"] != "function" {
                return Err(Error::Unsupported(
                    "Gemini Chat supports function tools".into(),
                ));
            }
            let mut function = tool["function"].clone();
            function
                .as_object_mut()
                .ok_or(Error::InvalidRequest("invalid function definition"))?
                .remove("strict");
            declarations.push(function);
        }
        request["tools"] = json!([{"functionDeclarations":declarations}]);
    }
    if let Some(choice) = body.get("tool_choice") {
        let config = match choice.as_str() {
            Some("auto") => json!({"mode":"AUTO"}),
            Some("none") => json!({"mode":"NONE"}),
            Some("required") => json!({"mode":"ANY"}),
            None if choice["type"] == "function" => {
                json!({"mode":"ANY","allowedFunctionNames":[choice["function"]["name"]]})
            }
            _ => return Err(Error::InvalidRequest("unsupported tool_choice")),
        };
        request["toolConfig"] = json!({"functionCallingConfig":config});
    }
    Ok(request)
}

fn parts(content: &Value) -> Result<Vec<Value>, Error> {
    if content.is_null() {
        return Ok(vec![]);
    }
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"text":text})]);
    }
    content
        .as_array()
        .ok_or(Error::InvalidRequest("invalid message content"))?
        .iter()
        .map(|part| match part["type"].as_str() {
            Some("text") => Ok(json!({"text":part["text"]})),
            Some("image_url") => {
                let image = &part["image_url"];
                let url = image["url"]
                    .as_str()
                    .or_else(|| image.as_str())
                    .ok_or(Error::InvalidRequest("missing image URL"))?;
                if let Some(data) = url.strip_prefix("data:") {
                    let (mime, bytes) = data
                        .split_once(";base64,")
                        .ok_or(Error::InvalidRequest("image must be base64 data URL"))?;
                    Ok(json!({"inlineData":{"mimeType":mime,"data":bytes}}))
                } else if url.starts_with("gs://") {
                    let mime = image["mime_type"]
                        .as_str()
                        .ok_or(Error::InvalidRequest("gs image requires mime_type"))?;
                    Ok(json!({"fileData":{"mimeType":mime,"fileUri":url}}))
                } else {
                    Err(Error::Unsupported(
                        "Gemini CLI Chat images require data URLs or gs:// with mime_type".into(),
                    ))
                }
            }
            _ => Err(Error::Unsupported(
                "use native Gemini for this content type".into(),
            )),
        })
        .collect()
}

fn finish(value: &Value, tools: bool) -> Option<&'static str> {
    match value.as_str() {
        Some("MAX_TOKENS") => Some("length"),
        Some(
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY",
        ) => Some("content_filter"),
        Some(_) if tools => Some("tool_calls"),
        Some(_) => Some("stop"),
        None => None,
    }
}

#[derive(Default)]
pub(super) struct StreamState {
    id: String,
    tools: usize,
}
impl StreamState {
    fn message(&mut self, value: &Value, streaming: bool) -> Result<Value, Error> {
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tools = Vec::new();
        let mut signatures = Vec::new();
        for part in value["candidates"][0]["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(text_part) = part["text"].as_str() {
                if part["thought"] == true {
                    reasoning.push_str(text_part);
                } else {
                    text.push_str(text_part);
                }
            }
            if part["functionCall"].is_object() {
                let call = &part["functionCall"];
                let id = call["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{}_{}", self.id, self.tools));
                let mut tool = json!({"id":id,"type":"function","function":{"name":call["name"],"arguments":call.get("args").unwrap_or(&json!({})).to_string()}});
                if streaming {
                    tool["index"] = self.tools.into();
                }
                if let Some(signature) = part.get("thoughtSignature") {
                    tool["extra_content"] = json!({"google":{"thought_signature":signature}});
                }
                self.tools += 1;
                tools.push(tool);
            } else if let Some(signature) = part.get("thoughtSignature") {
                signatures.push(signature.clone());
            }
        }
        let mut message = json!({"role":"assistant"});
        if !text.is_empty() {
            message["content"] = text.into();
        } else if !streaming {
            message["content"] = Value::Null;
        }
        if !reasoning.is_empty() {
            message["reasoning_content"] = reasoning.into();
        }
        if !tools.is_empty() {
            message["tool_calls"] = tools.into();
        }
        if !signatures.is_empty() {
            message["provider_specific_fields"] = json!({"thought_signatures":signatures});
        }
        Ok(message)
    }
    pub fn event(&mut self, value: &Value, model: &str) -> Result<Option<Value>, Error> {
        if self.id.is_empty() {
            self.id = value["responseId"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or(super::random_string()?);
        }
        let message = self.message(value, true)?;
        let reason = if value["promptFeedback"]["blockReason"].is_string() {
            Some("content_filter")
        } else {
            finish(&value["candidates"][0]["finishReason"], self.tools > 0)
        };
        Ok(Some(bridge::chunk(
            &self.id,
            model,
            message,
            reason,
            bridge::usage(Protocol::Gemini, value),
        )))
    }
}

pub(super) fn response(value: Value, model: &str) -> Result<Value, Error> {
    let mut state = StreamState {
        id: value["responseId"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or(super::random_string()?),
        tools: 0,
    };
    let message = state.message(&value, false)?;
    let reason = if value["promptFeedback"]["blockReason"].is_string() {
        "content_filter"
    } else {
        finish(&value["candidates"][0]["finishReason"], state.tools > 0).unwrap_or("stop")
    };
    Ok(bridge::completion(
        &state.id,
        model,
        message,
        reason,
        bridge::usage(Protocol::Gemini, &value),
    ))
}

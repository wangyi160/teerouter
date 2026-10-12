use super::Reason;
use crate::upstream::Protocol;
use serde_json::{Map, Value};

/// Preserve role/order/tool relationships as JSON text. Do not truncate, summarize,
/// interpret instructions, or reuse an earlier verdict for a changed request.
pub(super) fn extract(protocol: Protocol, body: &Value, max: usize) -> Result<String, Reason> {
    for key in [
        "previous_response_id",
        "conversation",
        "prompt",
        "cachedContent",
        "cached_content",
    ] {
        if body.get(key).is_some_and(|v| !v.is_null()) {
            return Err(Reason::UnsupportedContent);
        }
    }
    let fields: &[&str] = match protocol {
        Protocol::Chat => &[
            "messages",
            "tools",
            "functions",
            "tool_choice",
            "function_call",
            "response_format",
            "prediction",
        ],
        Protocol::Responses => &["input", "instructions", "tools", "tool_choice", "text"],
        Protocol::Messages => &[
            "system",
            "messages",
            "tools",
            "tool_choice",
            "output_config",
        ],
    };
    let mut content = Map::new();
    for key in fields {
        if let Some(value) = body.get(*key).filter(|v| !v.is_null()) {
            // Schemas and tool definitions are text, even when they describe fields
            // named image_url/file_id. Only actual conversation content carries media.
            if matches!(
                *key,
                "messages"
                    | "input"
                    | "system"
                    | "contents"
                    | "systemInstruction"
                    | "system_instruction"
            ) && unsupported(value)
            {
                return Err(Reason::UnsupportedContent);
            }
            content.insert((*key).into(), value.clone());
        }
    }
    if content.is_empty() {
        return Err(Reason::UnsupportedContent);
    }
    // Serialization is bounded by the already limited incoming HTTP body. A second
    // explicit scan limit rejects the entire request rather than silently clipping it.
    let text = serde_json::to_string(&content).map_err(|_| Reason::UnsupportedContent)?;
    if text.len() > max {
        return Err(Reason::InputTooLarge);
    }
    Ok(text)
}

fn unsupported(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.iter().any(|(key, value)| {
                !value.is_null()
                    && matches!(
                        key.as_str(),
                        "file_data"
                            | "file_url"
                            | "image_url"
                            | "input_audio"
                            | "inlineData"
                            | "inline_data"
                            | "fileData"
                            | "audio"
                            | "video"
                            | "encrypted_content"
                            | "thoughtSignature"
                            | "thought_signature"
                    )
            }) || map.get("type").and_then(Value::as_str).is_some_and(|t| {
                matches!(
                    t,
                    "image"
                        | "image_url"
                        | "input_image"
                        | "input_audio"
                        | "audio"
                        | "input_file"
                        | "file"
                        | "document"
                        | "video"
                        | "item_reference"
                        | "redacted_thinking"
                )
            }) || map.values().any(unsupported)
        }
        Value::Array(values) => values.iter().any(unsupported),
        _ => false,
    }
}

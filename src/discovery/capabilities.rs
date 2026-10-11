//! Read capabilities from authenticated catalog responses, never from model-name heuristics.
use crate::{Error, Provider, error::ApiError, upstream::Protocol};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Only response-field locations are configurable. No locally supplied allowlists.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityPaths {
    pub effort: Option<String>,
    pub effort_item: Option<String>,
    pub thinking_types: Option<String>,
    pub budget_min: Option<String>,
    pub budget_max: Option<String>,
    pub per_message_effort: Option<String>,
}
impl CapabilityPaths {
    pub(crate) fn validate(&self) -> Result<(), String> {
        for path in [
            &self.effort,
            &self.effort_item,
            &self.thinking_types,
            &self.budget_min,
            &self.budget_max,
            &self.per_message_effort,
        ]
        .into_iter()
        .flatten()
        {
            if !path.starts_with('/')
                || path.len() > 512
                || path.chars().any(char::is_control)
                || path.split('/').count() > 17
                || path.split('/').skip(1).any(str::is_empty)
            {
                return Err("invalid catalog capability JSON pointer".into());
            }
            let mut chars = path.chars();
            while let Some(c) = chars.next() {
                if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
                    return Err("invalid catalog capability JSON pointer escape".into());
                }
            }
        }
        if self.budget_min.is_some() != self.budget_max.is_some() {
            return Err("catalog budget bounds require both source paths".into());
        }
        Ok(())
    }
}

#[derive(Clone, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub effort: Option<BTreeSet<String>>,
    pub thinking_types: Option<BTreeSet<String>>,
    pub budget: Option<(i64, i64)>,
    pub per_message_effort: Option<BTreeSet<String>>,
}

fn choices(value: Option<&Value>, item_path: &str) -> Result<Option<BTreeSet<String>>, Error> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let invalid = || Error::InvalidResponse("invalid catalog capability choices");
    let mut result = BTreeSet::new();
    let mut insert = |name: &str| -> Result<(), Error> {
        if name.is_empty()
            || name.len() > 128
            || name.chars().any(char::is_control)
            || result.len() >= 64
        {
            return Err(invalid());
        }
        result.insert(name.to_owned());
        Ok(())
    };
    match value {
        Value::Array(values) => {
            for entry in values {
                let name = entry
                    .as_str()
                    .or_else(|| entry.pointer(item_path).and_then(Value::as_str))
                    .ok_or_else(invalid)?;
                insert(name)?;
            }
        }
        Value::Object(flags) => {
            if flags.get("supported") == Some(&Value::Bool(false)) {
                return Ok(Some(result));
            }
            for (name, flag) in flags {
                if name == "supported" {
                    continue;
                }
                let supported = flag
                    .as_bool()
                    .or_else(|| flag.get("supported").and_then(Value::as_bool))
                    .ok_or_else(invalid)?;
                if supported {
                    insert(name)?;
                }
            }
        }
        _ => return Err(invalid()),
    }
    Ok(Some(result))
}

impl Capabilities {
    pub(super) fn read(
        provider: Provider,
        codex: bool,
        entry: &Value,
        paths: &CapabilityPaths,
    ) -> Result<Self, Error> {
        let default = match provider {
            Provider::OpenAI if codex => Some("/supported_reasoning_levels"),
            Provider::Anthropic => Some("/capabilities/effort"),
            Provider::XAI => Some("/capabilities/reasoning_effort"),
            _ => None,
        };
        let source = |configured: Option<&str>, default: Option<&str>| {
            configured.or(default).and_then(|p| entry.pointer(p))
        };
        let item = paths.effort_item.as_deref().unwrap_or("/effort");
        let effort = choices(source(paths.effort.as_deref(), default), item)?;
        let thinking_types = choices(
            source(
                paths.thinking_types.as_deref(),
                (provider == Provider::Anthropic).then_some("/capabilities/thinking/types"),
            ),
            item,
        )?;
        let budget = match (
            paths.budget_min.as_deref().and_then(|p| entry.pointer(p)),
            paths.budget_max.as_deref().and_then(|p| entry.pointer(p)),
        ) {
            (None, None) | (Some(Value::Null), Some(Value::Null)) => None,
            (Some(a), Some(b)) => {
                let (a, b) = a.as_i64().zip(b.as_i64()).filter(|(a, b)| a <= b).ok_or(
                    Error::InvalidResponse("invalid catalog thinking budget bounds"),
                )?;
                Some((a, b))
            }
            _ => {
                return Err(Error::InvalidResponse(
                    "incomplete catalog thinking budget bounds",
                ));
            }
        };
        Ok(Self {
            effort,
            thinking_types,
            budget,
            per_message_effort: choices(
                paths
                    .per_message_effort
                    .as_deref()
                    .and_then(|p| entry.pointer(p)),
                item,
            )?,
        })
    }

    pub(crate) fn check(
        &self,
        provider: Provider,
        protocol: Protocol,
        body: &Value,
    ) -> Result<(), ApiError> {
        let bad = || {
            ApiError::new(
                400,
                "invalid_reasoning",
                "Invalid or conflicting reasoning parameters; values are never rewritten",
            )
        };
        let check = |value: &Value, allowed: &Option<BTreeSet<String>>| -> Result<(), ApiError> {
            let value = value.as_str().ok_or_else(bad)?;
            let allowed = allowed.as_ref().ok_or_else(missing)?;
            if !allowed.contains(value) {
                return Err(unsupported());
            }
            Ok(())
        };
        // Do not let alternate casing/snake_case or a misplaced native control
        // bypass validation (protobuf JSON accepts multiple spellings).
        for (node, allowed) in [
            (
                Some(body),
                &[
                    "reasoning_effort",
                    "reasoning",
                    "thinking",
                    "output_config",
                    "generationConfig",
                ][..],
            ),
            (body.get("reasoning"), &["effort"][..]),
            (body.get("output_config"), &["effort"][..]),
            (body.get("thinking"), &["budget_tokens"][..]),
            (body.get("generationConfig"), &["thinkingConfig"][..]),
            (
                body.pointer("/generationConfig/thinkingConfig"),
                &["thinkingLevel", "thinkingBudget"][..],
            ),
        ] {
            for key in node
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|o| o.keys())
            {
                let normalized: String = key
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .flat_map(char::to_lowercase)
                    .collect();
                if matches!(
                    normalized.as_str(),
                    "effort"
                        | "reasoningeffort"
                        | "reasoning"
                        | "thinking"
                        | "thinkingconfig"
                        | "thinkinglevel"
                        | "thinkingbudget"
                        | "budgettokens"
                        | "generationconfig"
                        | "outputconfig"
                ) && !allowed.contains(&key.as_str())
                {
                    return Err(bad());
                }
            }
        }
        // Reject foreign spellings/containers that a bridge would otherwise drop.
        for (key, accepted) in [
            ("reasoning_effort", protocol == Protocol::Chat),
            (
                "reasoning",
                matches!(provider, Provider::OpenAI | Provider::XAI),
            ),
            ("output_config", provider == Provider::Anthropic),
            ("thinking", provider == Provider::Anthropic),
            ("generationConfig", provider == Provider::Gemini),
        ] {
            if body.get(key).is_some() && !accepted {
                return Err(bad());
            }
        }
        let mut effort: Option<&Value> = None;
        for value in [
            body.get("reasoning_effort"),
            body.pointer("/reasoning/effort"),
            body.pointer("/output_config/effort"),
            body.pointer("/generationConfig/thinkingConfig/thinkingLevel"),
        ]
        .into_iter()
        .flatten()
        {
            if effort.is_some_and(|old| old != value) {
                return Err(bad());
            }
            check(value, &self.effort)?;
            effort = Some(value);
        }
        for key in ["reasoning", "output_config", "generationConfig"] {
            if body.get(key).is_some_and(|v| !v.is_object()) {
                return Err(bad());
            }
        }
        if let Some(thinking) = body.get("thinking") {
            let object = thinking.as_object().ok_or_else(bad)?;
            check(object.get("type").ok_or_else(bad)?, &self.thinking_types)?;
        }
        if body
            .pointer("/generationConfig/thinkingConfig")
            .is_some_and(|v| !v.is_object())
        {
            return Err(bad());
        }
        for budget in [
            body.pointer("/thinking/budget_tokens"),
            body.pointer("/generationConfig/thinkingConfig/thinkingBudget"),
        ]
        .into_iter()
        .flatten()
        {
            let value = budget.as_i64().ok_or_else(bad)?;
            let (min, max) = self.budget.ok_or_else(missing)?;
            if !(min..=max).contains(&value) {
                return Err(unsupported());
            }
        }
        for message in body["messages"].as_array().into_iter().flatten() {
            if let Some(value) = message.pointer("/output_config/effort") {
                if provider != Provider::Anthropic || protocol != Protocol::Messages {
                    return Err(bad());
                }
                check(value, &self.per_message_effort)?;
            }
        }
        Ok(())
    }
}
pub(crate) fn missing() -> ApiError {
    ApiError::new(
        503,
        "reasoning_capabilities_unavailable",
        "Official catalog does not publish the requested reasoning capability; request was not sent",
    )
}
fn unsupported() -> ApiError {
    ApiError::new(
        400,
        "unsupported_reasoning",
        "Requested reasoning setting is not supported by the official model catalog",
    )
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;

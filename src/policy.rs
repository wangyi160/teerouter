//! Administrator restrictions only subtract from authenticated official evidence.
use crate::{Provider, discovery::capabilities::Capabilities, error::ApiError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Restriction {
    /// None applies to all providers/models. Model names are exact, never globs.
    pub provider: Option<Provider>,
    pub model: Option<String>,
    pub block_model: bool,
    pub blocked_efforts: BTreeSet<String>,
    pub blocked_thinking_types: BTreeSet<String>,
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;

pub(crate) fn validate(rules: &[Restriction]) -> Result<(), String> {
    if rules.len() > 1024 {
        return Err("too many administrator restrictions".into());
    }
    for rule in rules {
        if rule
            .model
            .as_ref()
            .is_some_and(|m| !crate::config::valid_model_id(m))
            || [&rule.blocked_efforts, &rule.blocked_thinking_types]
                .into_iter()
                .any(|values| {
                    values.len() > 64
                        || values.iter().any(|v| {
                            v.is_empty() || v.len() > 128 || v.chars().any(char::is_control)
                        })
                })
        {
            return Err("invalid administrator restriction".into());
        }
    }
    Ok(())
}

#[derive(Default, Serialize)]
pub(crate) struct Restrictions {
    pub block_model: bool,
    blocked_efforts: BTreeSet<String>,
    blocked_thinking_types: BTreeSet<String>,
}
impl Restrictions {
    pub(crate) fn for_model(
        rules: &[Restriction],
        provider: Provider,
        model: &str,
        canonical: &str,
    ) -> Self {
        let mut result = Self::default();
        for rule in rules.iter().filter(|r| {
            r.provider.is_none_or(|p| p == provider)
                && r.model
                    .as_ref()
                    .is_none_or(|m| m == model || m == canonical)
        }) {
            result.block_model |= rule.block_model;
            result
                .blocked_efforts
                .extend(rule.blocked_efforts.iter().cloned());
            result
                .blocked_thinking_types
                .extend(rule.blocked_thinking_types.iter().cloned());
        }
        result
    }

    pub(crate) fn check(&self, body: &Value) -> Result<(), ApiError> {
        if self.block_model {
            return Err(ApiError::new(
                403,
                "model_blocked",
                "Model is blocked by administrator policy",
            ));
        }
        let blocked = || {
            ApiError::new(
                403,
                "reasoning_blocked",
                "Reasoning setting is blocked by administrator policy",
            )
        };
        let efforts: Vec<_> = [
            body.get("reasoning_effort"),
            body.pointer("/reasoning/effort"),
            body.pointer("/output_config/effort"),
            body.pointer("/generationConfig/thinkingConfig/thinkingLevel"),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
        let choice = |values: &[&str], denied: &BTreeSet<String>| -> Result<(), ApiError> {
            // Policies match explicit choices. Omission preserves the provider's
            // default behavior and must never become a new required parameter.
            if values.iter().any(|v| denied.contains(*v)) {
                return Err(blocked());
            }
            Ok(())
        };
        choice(&efforts, &self.blocked_efforts)?;
        // Per-message overrides must also obey the same effort restriction.
        for value in body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m.pointer("/output_config/effort"))
            .filter_map(Value::as_str)
        {
            choice(&[value], &self.blocked_efforts)?;
        }
        choice(
            &body
                .pointer("/thinking/type")
                .and_then(Value::as_str)
                .into_iter()
                .collect::<Vec<_>>(),
            &self.blocked_thinking_types,
        )?;
        Ok(())
    }

    /// Derived client view only. The source object and cached catalog are never mutated.
    pub(crate) fn available(&self, official: &Capabilities) -> Value {
        let subtract = |values: &Option<BTreeSet<String>>, denied: &BTreeSet<String>| {
            values
                .as_ref()
                .map(|v| v.difference(denied).cloned().collect::<BTreeSet<_>>())
        };
        json!({"effort":subtract(&official.effort, &self.blocked_efforts),
            "thinking_types":subtract(&official.thinking_types, &self.blocked_thinking_types),
            "budget":official.budget,
            "per_message_effort":subtract(&official.per_message_effort, &self.blocked_efforts)})
    }
}

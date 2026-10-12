//! Usage metadata only. Prices, balances and settlement live in management.
use crate::vault::Vault;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Usage {
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub unknown_usage_requests: u64,
}
pub(crate) struct Ledger {
    vault: Arc<Vault>,
}
impl Ledger {
    pub fn new(vault: Arc<Vault>) -> Self {
        Self { vault }
    }
    pub fn healthy(&self) -> bool {
        self.vault.serving() && self.vault.lock().is_ok_and(|d| d.events.len() < 4096)
    }
    pub(crate) fn recover(&self) -> Result<(), crate::vault::VaultError> {
        self.vault.verify()?;
        self.vault.audit("runtime_checked")
    }
    pub fn snapshot(&self, key: &str) -> Result<Usage, String> {
        Ok(self
            .vault
            .lock()
            .map_err(|e| e.to_string())?
            .usage
            .get(key)
            .cloned()
            .unwrap_or_default())
    }
    pub fn record(
        &self,
        key: &str,
        usage: &Usage,
        audit: &serde_json::Value,
        moderation: Option<&crate::moderation::Report>,
    ) {
        let Ok(mut data) = self.vault.lock() else {
            return;
        };
        let total = data.usage.entry(key.into()).or_default();
        total.requests = total.requests.saturating_add(usage.requests);
        total.errors = total.errors.saturating_add(usage.errors);
        total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
        total.unknown_usage_requests = total
            .unknown_usage_requests
            .saturating_add(usage.unknown_usage_requests);
        data.sequence += 1;
        let seq = data.sequence;
        if let Some(result) = moderation
            && let Some(user) = audit["user_id"].as_str()
        {
            data.moderation
                .entry(user.into())
                .or_default()
                .observe(crate::moderation::Event {
                    id: seq,
                    request_id: audit["request_id"].as_str().unwrap_or_default().into(),
                    key: key.into(),
                    model: audit["model"].as_str().unwrap_or_default().into(),
                    created_at: audit["created_at"].as_u64().unwrap_or_default(),
                    result: result.clone(),
                });
        }
        data.events
            .push_back(serde_json::json!({"seq":seq,"event":audit}));
        data.changed();
    }
}

//! Usage metadata and explicit upstream refusal evidence, never request bodies. Prices, balances and settlement live in management.
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
        self.vault.serving()
            && self.vault.lock().is_ok_and(|d| {
                d.events.len() < 4096 && d.pending_event_bytes < crate::refusal::PENDING_BYTES
            })
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
        refusal: Option<&crate::refusal::Report>,
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
        if let Some(result) = refusal
            && let Some(user) = audit["user_id"].as_str()
        {
            data.refusals
                .entry(user.into())
                .or_default()
                .observe(crate::refusal::Event {
                    id: seq,
                    request_id: audit["request_id"].as_str().unwrap_or_default().into(),
                    key: key.into(),
                    model: audit["model"].as_str().unwrap_or_default().into(),
                    created_at: audit["created_at"].as_u64().unwrap_or_default(),
                    pool: audit["pool"].as_str().map(str::to_owned),
                    deployment: audit["deployment"].as_str().map(str::to_owned),
                    status: audit["status"].as_u64().unwrap_or_default() as u16,
                    attempts: audit["attempts"].as_u64().unwrap_or_default(),
                    result: result.clone(),
                });
            // Recent detail is bounded globally; pending checkpoint events retain the archive copy.
            while crate::refusal::history_bytes(&data.refusals) > crate::refusal::HISTORY_BYTES {
                let oldest = data
                    .refusals
                    .iter()
                    .filter_map(|(user, record)| {
                        record.recent_events.front().map(|e| (e.id, user.clone()))
                    })
                    .min();
                let Some((_, user)) = oldest else {
                    break;
                };
                data.refusals.get_mut(&user).unwrap().evict();
            }
        }
        let event = serde_json::json!({"seq":seq,"event":audit});
        data.pending_event_bytes += serde_json::to_vec(&event).unwrap().len();
        data.events.push_back(event);
        data.changed();
    }
}

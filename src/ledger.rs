//! Usage, provider-calculated charges and refusal evidence, never request bodies.
//! Management persists balances; the router tracks consumption between syncs.
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
#[derive(Clone)]
pub(crate) struct Activity {
    ledger: Arc<Ledger>,
    user: String,
}
impl Activity {
    pub fn observed(&self) {
        if let Ok(mut data) = self.ledger.vault.lock()
            && data.upstream_users.insert(self.user.clone())
        {
            data.changed();
        }
    }
}
impl Ledger {
    pub fn activity(self: &Arc<Self>, user: &str) -> Activity {
        Activity {
            ledger: self.clone(),
            user: user.into(),
        }
    }
    pub fn has_upstream_user(&self, user: &str) -> bool {
        self.vault
            .lock()
            .is_ok_and(|data| data.upstream_users.contains(user))
    }
    pub fn new(vault: Arc<Vault>) -> Self {
        Self { vault }
    }
    pub fn healthy(&self) -> bool {
        self.vault.serving()
            && self.vault.lock().is_ok_and(|d| {
                d.events.len() < 4096
                    && d.pending_event_bytes < crate::refusal::PENDING_BYTES
                    && d.refusal_queue.healthy()
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
        let mut audit = audit.clone();
        // Usage records only link to the queue, so deleting a refusal removes its
        // body from the entire current checkpoint, independently of usage ACKs.
        if let Some(object) = audit.as_object_mut() {
            object.remove("refusal");
        }
        if let Some(result) = refusal {
            let mut event = crate::refusal::Event::from_audit(0, &audit, result.clone());
            event.key = key.into();
            audit["refusal_id"] = data.refusal_queue.push(event).into();
        }
        let event = serde_json::json!({"seq":seq,"event":audit});
        // Only the platform ledger enforces user balances. Merchant ledgers
        // have their own receipt acknowledgements and no platform balance map.
        if self.vault.managed() {
            crate::balances::add(&mut data.consumption, &event["event"]);
        }
        data.pending_event_bytes += serde_json::to_vec(&event).unwrap().len();
        data.events.push_back(event);
        data.changed();
    }
}

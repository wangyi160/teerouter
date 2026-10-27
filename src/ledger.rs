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
        let user=self.vault.lock().ok().and_then(|d|d.bundle.as_ref().map(|b|b.config.customer_id(user).to_owned())).unwrap_or_else(||user.to_owned());
        Activity {
            ledger: self.clone(),
            user: user.into(),
        }
    }
    pub fn has_upstream_user(&self, user: &str) -> bool {
        self.vault
            .lock()
            .is_ok_and(|data| data.upstream_users.contains(data.bundle.as_ref().map(|b|b.config.customer_id(user)).unwrap_or(user)))
    }
    pub fn new(vault: Arc<Vault>) -> Self {
        Self { vault }
    }
    pub fn healthy(&self) -> bool {
        self.vault.serving()
            && self.vault.lock().is_ok_and(|d| {
                d.events.len() < 4096
                    && d.receipt_relay.as_ref().is_none_or(|r| r.healthy())
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
            if let (Some(merchant), Some(user)) =
                (audit["merchant_id"].as_str(), audit["user_id"].as_str())
            {
                let user = data.bundle.as_ref().map(|b|b.config.customer_id(user)).unwrap_or(user).to_owned();
                let count = data
                    .merchant_refusal_counts
                    .entry(merchant.into())
                    .or_default()
                    .entry(user.into())
                    .or_default();
                *count = count.saturating_add(1);
            }
            audit["refusal_type"] = if result.reasons.contains("content_filter") {
                "content_filter"
            } else if result.error.is_some() {
                "policy_violation"
            } else {
                "refusal"
            }
            .into();
            if data.receipt_relay.is_none() {
                let mut event = crate::refusal::Event::from_audit(0, &audit, result.clone());
                event.key = key.into();
                audit["refusal_id"] = data.refusal_queue.push(event).into();
            }
        }
        let event = serde_json::json!({"seq":seq,"event":audit});
        // Only the platform ledger enforces user balances. Merchant runtimes
        // retain usage totals through record_usage and have no receipt outbox.
        if self.vault.managed() {
            crate::balances::add(&mut data.consumption, &event["event"]);
        }
        data.pending_event_bytes += serde_json::to_vec(&event).unwrap().len();
        data.events.push_back(event);
        data.changed();
        // A failed durable write latches admission closed, including cancellation.
        // Never silently fall back to RAM after enabling the accounting journal.
        let _ = self.vault.persist_accounting(&data);
    }

    /// Merchant runtimes retain rate-limit totals, while the parent platform
    /// ledger owns the single durable receipt and refusal category.
    pub fn record_usage(&self, key: &str, usage: &Usage) {
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
        data.changed();
    }
}

#[cfg(test)]
mod merchant_statistics_tests {
    use super::*;

    #[test]
    fn merchant_totals_never_need_receipt_acknowledgements() {
        let vault = Vault::memory().unwrap();
        let ledger = Ledger::new(vault.clone());
        for _ in 0..5000 {
            ledger.record_usage(
                "customer",
                &Usage {
                    requests: 1,
                    input_tokens: 2,
                    ..Default::default()
                },
            );
        }
        assert!(ledger.healthy());
        let data = vault.lock().unwrap();
        assert_eq!(data.usage["customer"].requests, 5000);
        assert_eq!(data.usage["customer"].input_tokens, 10000);
        assert!(data.events.is_empty());
        assert!(data.refusal_queue.events.is_empty());
        assert_eq!(data.pending_event_bytes, 0);
    }
}

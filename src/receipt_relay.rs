//! Public, pseudonymous receipts. Storage ACK and financial settlement are separate.
mod protocol;
mod publisher;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod review_tests;
#[cfg(test)]
pub(crate) mod test_support;

use crate::{
    receipts::Head,
    vault::{Data, Vault, VaultError},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};

type Result<T> = std::result::Result<T, VaultError>;
pub(crate) use publisher::start;
const MAX_RETAINED_BATCHES: usize = 4096;
const MAX_RETAINED_USERS: usize = 100_000;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub url: String,
    pub upload_token: String,
    #[serde(default = "interval")]
    pub interval_seconds: u64,
    /// Secret random 32-byte identities, NEVER present in public records.
    pub user_ids: BTreeMap<String, String>,
}
fn interval() -> u64 {
    300
}
impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.url).map_err(|_| VaultError("invalid relay URL"))?;
        let secure = url.scheme() == "https"
            || cfg!(test) && url.scheme() == "http" && url.host_str() == Some("127.0.0.1");
        if !secure
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
            || !(30..=3600).contains(&self.interval_seconds)
            || !(32..=256).contains(&self.upload_token.len())
            || !self
                .upload_token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            || self.user_ids.len() > 100_000
            || self.user_ids.iter().any(|(id, secret)| {
                !crate::config::valid_name(id) || crate::receipts::unhex(secret, 32).is_none()
            })
        {
            return Err(VaultError("invalid receipt relay configuration"));
        }
        let unique: std::collections::BTreeSet<_> = self.user_ids.values().collect();
        if unique.len() != self.user_ids.len() {
            return Err(VaultError("duplicate private user identity"));
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Retained {
    pub head: Head,
    pub consumption: BTreeMap<String, u128>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub config: Config,
    pub start_sequence: u64,
    pub uploaded: Option<Head>,
    pub settled: Option<Head>,
    #[serde(default)]
    pub retained: VecDeque<Retained>,
    #[serde(default)]
    pub balance_revision: u64,
    #[serde(default)]
    pub last_settlement_hash: Option<String>,
}
impl State {
    pub(crate) fn healthy(&self) -> bool {
        self.retained.len() < MAX_RETAINED_BATCHES
            && self
                .retained
                .iter()
                .map(|b| b.consumption.len())
                .sum::<usize>()
                < MAX_RETAINED_USERS
    }
    pub(crate) fn validate_recovery(&self, sequence: u64) -> Result<()> {
        self.config.validate()?;
        // Management must first ingest everything durably uploaded before restart.
        if !self.retained.is_empty()
            || self.uploaded.as_ref().is_some_and(|h| !h.valid(sequence))
            || serde_json::to_value(&self.uploaded).ok() != serde_json::to_value(&self.settled).ok()
            || self.uploaded.is_none() && self.start_sequence != sequence
        {
            return Err(VaultError("unsettled receipt relay recovery state"));
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Publishing {
    pub(crate) pending: Option<Export>,
    pub(crate) last_attempt: Option<Instant>,
    pub(crate) last_attempt_ms: Option<u64>,
    pub last_success_ms: Option<u64>,
    pub last_error: Option<&'static str>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Export {
    pub(crate) body: Value,
    pub(crate) head: Head,
    pub(crate) consumption: BTreeMap<String, u128>,
    pub(crate) refusal_through: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settlement {
    pub boot_id: String,
    pub revision: u64,
    pub through: Option<Head>,
    pub balances: Option<BTreeMap<String, i64>>,
    pub balances_patch: Option<BTreeMap<String, Option<i64>>>,
}

impl Vault {
    pub(crate) fn relay_status(&self) -> Result<Value> {
        let data = self.lock()?;
        let publishing = self.relay_publishing.lock().unwrap();
        let mut result = json!({"version":1,"enabled":data.receipt_relay.is_some(),
            "instance_id":data.instance,"boot_id":self.boot_id,
            "public_key":self.receipt_signer.public_key(),"pending_records":data.events.len(),
            "last_success_ms":publishing.last_success_ms,"last_error":publishing.last_error});
        if let Some(state) = &data.receipt_relay {
            result["url"] = state.config.url.clone().into();
            result["interval_seconds"] = state.config.interval_seconds.into();
            result["uploaded"] = json!(state.uploaded);
            result["settled"] = json!(state.settled);
            result["balance_revision"] = state.balance_revision.into();
            result["unsettled_batches"] = state.retained.len().into();
        }
        Ok(result)
    }
    pub(crate) fn relay_settle(&self, ack: Settlement) -> Result<()> {
        if ack.boot_id != self.boot_id
            || ack.revision == 0
            || ack.balances.is_some() == ack.balances_patch.is_some()
        {
            return Err(VaultError("invalid relay settlement"));
        }
        let fingerprint = crate::receipts::hex(&crate::receipts::digest(
            &serde_json::to_vec(&ack).map_err(|_| VaultError("invalid settlement"))?,
        ));
        let mut data = self.lock()?;
        if !self.serving() {
            return Err(VaultError("not initialized"));
        }
        let state = data
            .receipt_relay
            .as_ref()
            .ok_or(VaultError("relay not configured"))?;
        if ack.revision == state.balance_revision
            && state.last_settlement_hash.as_deref() == Some(&fingerprint)
        {
            return Ok(());
        }
        if ack.revision <= state.balance_revision {
            return Err(VaultError("stale balance revision"));
        }
        let through = ack
            .through
            .as_ref()
            .map_or(state.start_sequence, |h| h.through_seq);
        let old = state
            .settled
            .as_ref()
            .map_or(state.start_sequence, |h| h.through_seq);
        let same = |a: &Option<Head>, b: &Option<Head>| {
            serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
        };
        if through < old
            || !(same(&state.settled, &ack.through)
                || state
                    .retained
                    .iter()
                    .any(|b| same(&Some(b.head.clone()), &ack.through)))
        {
            return Err(VaultError("unknown relay settlement boundary"));
        }
        let mut balances = ack.balances.unwrap_or_else(|| data.balances.clone());
        let patch = ack.balances_patch.unwrap_or_default();
        if patch.len() > 100_000 {
            return Err(VaultError("too many balance changes"));
        }
        for (id, balance) in patch {
            if !crate::config::valid_name(&id) {
                return Err(VaultError("invalid balance identity"));
            }
            if let Some(balance) = balance {
                balances.insert(id, balance);
            } else {
                balances.remove(&id);
            }
        }
        if balances.len() > 100_000 || balances.keys().any(|k| !crate::config::valid_name(k)) {
            return Err(VaultError("invalid balances"));
        }
        let state = data.receipt_relay.as_mut().unwrap();
        state.retained.retain(|b| b.head.through_seq > through);
        state.settled = ack.through;
        state.balance_revision = ack.revision;
        state.last_settlement_hash = Some(fingerprint);
        let mut consumption = crate::balances::consumption(&data.events);
        for batch in &data.receipt_relay.as_ref().unwrap().retained {
            for (user, amount) in &batch.consumption {
                *consumption.entry(user.clone()).or_default() += amount;
            }
        }
        data.balances = balances;
        data.consumption = consumption;
        data.changed();
        self.persist_accounting(&data)?;
        Ok(())
    }
    fn prepare_public(&self, force: bool) -> Result<Option<(Config, Value)>> {
        let data = self.lock()?;
        if !self.serving() {
            return Ok(None);
        }
        let Some(state) = &data.receipt_relay else {
            return Ok(None);
        };
        let mut p = self.relay_publishing.lock().unwrap();
        if !force
            && p.last_attempt
                .is_some_and(|t| t.elapsed().as_secs() < state.config.interval_seconds)
        {
            return Ok(None);
        }
        if p.pending.is_none() {
            if data.events.is_empty() || !state.healthy() {
                p.last_attempt = Some(Instant::now());
                p.last_attempt_ms = Some(protocol::now_ms());
                self.persist_accounting_with(&data, &p)?;
                return Ok(None);
            }
            p.pending = Some(protocol::seal(&self.receipt_signer, &data, &self.boot_id)?);
        }
        p.last_attempt = Some(Instant::now());
        p.last_attempt_ms = Some(protocol::now_ms());
        self.persist_accounting_with(&data, &p)?;
        Ok(Some((
            state.config.clone(),
            p.pending.as_ref().unwrap().body.clone(),
        )))
    }
    fn uploaded_public(&self, hash: &str) -> Result<()> {
        let mut data = self.lock()?;
        let mut p = self.relay_publishing.lock().unwrap();
        let export = p
            .pending
            .as_ref()
            .ok_or(VaultError("no pending public batch"))?;
        if export.head.batch_hash != hash {
            return Err(VaultError("public batch acknowledgement mismatch"));
        }
        let through = export.head.through_seq;
        let refusal_through = export.refusal_through;
        let state = data
            .receipt_relay
            .as_mut()
            .ok_or(VaultError("relay not configured"))?;
        state.uploaded = Some(export.head.clone());
        state.retained.push_back(Retained {
            head: export.head.clone(),
            consumption: export.consumption.clone(),
        });
        data.events
            .retain(|e| e["seq"].as_u64().unwrap_or(u64::MAX) > through);
        data.pending_event_bytes = data
            .events
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap().len())
            .sum();
        if refusal_through > 0 {
            data.refusal_queue.delete_through(refusal_through);
        }
        // Intentionally DO NOT change balances or consumption here.
        data.changed();
        p.pending = None;
        p.last_error = None;
        p.last_success_ms = Some(protocol::now_ms());
        self.persist_accounting_with(&data, &p)?;
        Ok(())
    }
}

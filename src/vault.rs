//! Business configuration stays in memory; an optional sealed accounting journal
//! preserves receipts independently of management checkpoints.
use crate::{Provider, config::Config};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
use zeroize::Zeroizing;

mod checkpoint_diff;
mod journal;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct VaultError(pub(crate) &'static str);
type Result<T> = std::result::Result<T, VaultError>;
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provision {
    pub config: Config,
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    #[serde(default)]
    pub accounts: BTreeMap<String, StoredAccount>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAccount {
    pub provider: Provider,
    pub credential: Value,
    /// Omission preserves an existing binding; new accounts require a proxy.
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default = "account_enabled")]
    pub enabled: bool,
}
fn account_enabled() -> bool {
    true
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Data {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_relay: Option<crate::receipt_relay::State>,
    /// Last balances committed by management. Requests never modify this map.
    #[serde(default)]
    pub balances: BTreeMap<String, i64>,
    /// Cost of receipts not yet financially settled through relay.
    #[serde(default)]
    pub consumption: BTreeMap<String, u128>,
    #[serde(default)]
    pub official_pricing: crate::pricing::Book,
    #[serde(default)]
    pub pricing_source: crate::pricing::source::State,
    /// Merchant customers with evidence of a model request reaching an upstream.
    /// Independent of usage-detail retention and refusal queues.
    #[serde(default)]
    pub upstream_users: BTreeSet<String>,
    /// Public admission and control-key digests only. No merchant secret backups.
    #[serde(default)]
    pub merchants: BTreeMap<String, crate::merchants::Registration>,
    pub bundle: Option<Provision>,
    pub revision: i64,
    pub account_revisions: BTreeMap<String, i64>,
    pub usage: BTreeMap<String, crate::ledger::Usage>,
    #[serde(default)]
    pub merchant_refusal_counts: crate::merchants::refusals::Counts,
    /// Empty marker emitted by previous releases; populated history requires archival first.
    #[serde(default, rename = "moderation", skip_serializing, deserialize_with = "empty_retired_moderation")]
    pub(crate) _retired_moderation: (),
    #[serde(default, skip_serializing)]
    pub refusals: BTreeMap<String, crate::refusal::UserRecord>,
    #[serde(default)]
    pub refusal_queue: crate::refusal::Queue,
    #[serde(skip)]
    pub pending_event_bytes: usize,
    pub instance: String,
    pub sequence: u64,
    pub events: VecDeque<Value>,
    pub generation: u64,
    #[serde(skip)]
    pub catalog: BTreeMap<String, Value>,
    #[serde(skip)]
    pub health: BTreeMap<String, (i64, Value)>,
    #[serde(skip)]
    pub sessions: BTreeMap<(String, String, String), (String, u64)>,
    #[serde(skip)]
    pub admin_log: VecDeque<Value>,
    #[serde(skip)]
    pub log_sequence: u64,
}
fn empty_retired_moderation<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<(), D::Error> {
    let map = BTreeMap::<String, serde::de::IgnoredAny>::deserialize(deserializer)?;
    if map.is_empty() {
        Ok(())
    } else {
        Err(serde::de::Error::custom("archive retired moderation history before restoring"))
    }
}

#[cfg(test)]
#[test]
fn retired_scanner_history_is_not_silently_dropped_on_restore() {
    let mut snapshot = serde_json::to_value(Data::default()).unwrap();
    assert!(snapshot.get("moderation").is_none());
    snapshot["moderation"] = json!({});
    let restored: Data = serde_json::from_value(snapshot.clone()).unwrap();
    assert!(serde_json::to_value(restored).unwrap().get("moderation").is_none());
    snapshot["moderation"] = json!({"user":{"recent_events":[{"id":1}]}});
    assert!(serde_json::from_value::<Data>(snapshot).err().unwrap().to_string()
        .contains("archive retired moderation history"));
}

impl Data {
    pub(crate) fn changed(&mut self) {
        self.generation += 1;
    }
    fn retire(&mut self, binding: &str) {
        for (value, _) in self.sessions.values_mut() {
            if value == binding {
                *value = "!retired".into();
            }
        }
    }
    pub(crate) fn log(&mut self, event: Value) {
        self.log_sequence += 1;
        self.admin_log.push_back(json!({"id": self.log_sequence, "created_at":crate::time::unix_now(), "event": event}));
        while self.admin_log.len() > 1000 {
            self.admin_log.pop_front();
        }
    }
}
struct SyncState {
    managed: bool,
    initialized: bool,
    last_contact: Option<Instant>,
    diff_base: Option<(String, Value)>,
}
pub struct Vault {
    journal: Option<Mutex<journal::Journal>>,
    journal_failed: std::sync::atomic::AtomicBool,
    pub(crate) relay_publishing: Mutex<crate::receipt_relay::Publishing>,
    pub(crate) relay_upload: tokio::sync::Mutex<()>,
    pub(crate) receipt_signer: crate::receipts::Signer,
    pub(crate) data: Mutex<Data>,
    pub(crate) boot_id: String,
    sync: Mutex<SyncState>,
    user_gates: Mutex<BTreeMap<String, std::sync::Weak<crate::resources::Gate>>>,
}
pub(crate) fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| VaultError("random source unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
impl Vault {
    pub fn memory() -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            journal: None,
            journal_failed: std::sync::atomic::AtomicBool::new(false),
            relay_publishing: Mutex::default(),
            relay_upload: tokio::sync::Mutex::new(()),
            receipt_signer: crate::receipts::Signer::new()?,
            data: Mutex::new(Data {
                instance: random_id()?,
                ..Data::default()
            }),
            boot_id: random_id()?,
            sync: Mutex::new(SyncState {
                managed: false,
                initialized: true,
                last_contact: None,
                diff_base: None,
            }),
            user_gates: Mutex::new(BTreeMap::new()),
        }))
    }
    pub(crate) fn user_gate(&self, id: &str) -> Arc<crate::resources::Gate> {
        let mut gates = self.user_gates.lock().unwrap();
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(id).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::default();
        gates.insert(id.to_owned(), Arc::downgrade(&gate));
        gate
    }
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Data>> {
        self.data
            .lock()
            .map_err(|_| VaultError("runtime state unavailable"))
    }
    pub fn require_manager(&self) {
        *self.sync.lock().unwrap() = SyncState {
            managed: true,
            initialized: false,
            last_contact: None,
            diff_base: None,
        };
    }
    pub(crate) fn initialized(&self) -> bool {
        self.sync.lock().unwrap().initialized
    }
    pub(crate) fn managed(&self) -> bool {
        self.sync.lock().unwrap().managed
    }
    pub(crate) fn serving(&self) -> bool {
        let sync = self.sync.lock().unwrap();
        (!sync.managed || sync.initialized)
            && !self
                .journal_failed
                .load(std::sync::atomic::Ordering::SeqCst)
    }
    // Synchronization freshness is diagnostic; it is never a serving lease.
    pub(crate) fn manager_connected(&self) -> bool {
        self.sync
            .lock()
            .unwrap()
            .last_contact
            .is_some_and(|t| t.elapsed().as_secs() < 630)
    }
    pub(crate) fn initialize(&self, mut data: Data) -> Result<()> {
        if let Some(bundle) = &data.bundle {
            for account in bundle.accounts.values() {
                if !account.credential.as_object().is_some_and(|m| m.is_empty()) {
                    validate_credential(account.provider, &account.credential)?;
                }
            }
        }
        let mut current = self.lock()?;
        let mut sync = self.sync.lock().unwrap();
        if sync.initialized {
            return Err(VaultError("router is already initialized"));
        }
        // Restored balances/configuration are management input. Historical events
        // must never be re-signed as if this process had observed them.
        if !data.events.is_empty() {
            return Err(VaultError("invalid receipt recovery state"));
        }
        if let Some(relay) = &data.receipt_relay {
            relay.validate_recovery(data.sequence)?;
        }
        // Only locally authenticated accounting may restore unexported events.
        // Management input above must still contain no unsigned historical rows.
        self.restore_accounting(&mut data)?;
        data.pending_event_bytes = data
            .events
            .iter()
            .map(|v| serde_json::to_vec(v).map_or(0, |b| b.len()))
            .sum();
        data.consumption = crate::balances::consumption(&data.events);
        if let Some(relay) = &data.receipt_relay {
            for batch in &relay.retained {
                for (user, amount) in &batch.consumption {
                    *data.consumption.entry(user.clone()).or_default() += amount;
                }
            }
        }
        self.persist_accounting(&data)?;
        *current = data;
        sync.initialized = true;
        Ok(())
    }
    pub(crate) fn configure_relay(&self, config: crate::receipt_relay::Config) -> Result<()> {
        config.validate()?;
        let mut data = self.lock()?;
        let mut sync = self.sync.lock().unwrap();
        if !sync.managed || !sync.initialized {
            return Err(VaultError(
                "initialize managed router before enabling relay",
            ));
        }
        if data.bundle.as_ref().is_some_and(|b| {
            b.config
                .keys
                .iter()
                .any(|k| !config.user_ids.contains_key(&k.user_id))
        }) {
            return Err(VaultError("private receipt identity missing"));
        }
        if let Some(state) = &mut data.receipt_relay {
            if state.config.url.trim_end_matches('/') != config.url.trim_end_matches('/')
                || state
                    .config
                    .user_ids
                    .iter()
                    .any(|(id, value)| config.user_ids.get(id) != Some(value))
            {
                return Err(VaultError(
                    "relay destination and existing identities are immutable",
                ));
            }
            state.config = config;
        } else {
            let before = data
                .events
                .front()
                .and_then(|e| e["seq"].as_u64())
                .map_or(data.sequence, |s| s - 1);
            data.receipt_relay = Some(crate::receipt_relay::State {
                config,
                start_sequence: before,
                uploaded: None,
                settled: None,
                retained: Default::default(),
                balance_revision: 0,
                last_settlement_hash: None,
            });
            // Public relay records carry only a refusal type; retire old text copies.
            let last = data.refusal_queue.last_id;
            data.refusal_queue.delete_through(last);
        }
        sync.diff_base = None;
        data.changed();
        let mut publishing = self.relay_publishing.lock().unwrap();
        if publishing.last_attempt.is_none() {
            publishing.last_attempt = Some(Instant::now());
            publishing.last_attempt_ms = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| VaultError("clock unavailable"))?
                    .as_millis() as u64,
            );
        }
        self.persist_accounting_with(&data, &publishing)?;
        Ok(())
    }
    pub fn verify(&self) -> Result<()> {
        drop(self.lock()?);
        Ok(())
    }
    pub fn config(&self) -> Result<Config> {
        let data = self.lock()?;
        if data.revision == 0 {
            return Err(VaultError("not initialized"));
        }
        data.bundle
            .as_ref()
            .map(|b| b.config.clone())
            .ok_or(VaultError("not initialized"))
    }
    pub fn secret(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .bundle
            .as_ref()
            .and_then(|b| b.secrets.get(name).cloned()))
    }
    pub(crate) fn catalog(&self, source: &str) -> Result<Option<Value>> {
        Ok(self.lock()?.catalog.get(source).cloned())
    }
    pub(crate) fn store_catalog(&self, source: &str, value: Option<&Value>) -> Result<()> {
        let mut data = self.lock()?;
        if let Some(value) = value {
            data.catalog.insert(source.into(), value.clone());
        } else {
            data.catalog.remove(source);
        }
        Ok(())
    }
    pub fn account(&self, name: &str, provider: Provider) -> Result<(Value, i64)> {
        let data = self.lock()?;
        let account = data
            .bundle
            .as_ref()
            .and_then(|b| b.accounts.get(name))
            .ok_or(VaultError("account missing"))?;
        if account.provider != provider {
            return Err(VaultError("account provider mismatch"));
        }
        Ok((account.credential.clone(), data.account_revisions[name]))
    }
    pub fn account_proxy(&self, name: &str, provider: Provider) -> Result<String> {
        let data = self.lock()?;
        let account = data
            .bundle
            .as_ref()
            .and_then(|b| b.accounts.get(name))
            .ok_or(VaultError("account missing"))?;
        if account.provider != provider {
            return Err(VaultError("account provider mismatch"));
        }
        let proxy = account
            .proxy
            .clone()
            .ok_or(VaultError("account proxy required"))?;
        validate_proxy(provider, &proxy)?;
        Ok(proxy)
    }
    pub fn set_account_proxy(&self, name: &str, provider: Provider, proxy: &str) -> Result<()> {
        validate_account(name, &json!({}))?;
        validate_proxy(provider, proxy)?;
        let mut data = self.lock()?;
        let bundle = data.bundle.get_or_insert_with(|| Provision {
            config: serde_json::from_value(
                json!({"server":{"allow_empty_routes":true},"keys":[],"models":[]}),
            )
            .unwrap(),
            secrets: BTreeMap::new(),
            accounts: BTreeMap::new(),
        });
        let account = bundle.accounts.entry(name.into()).or_insert(StoredAccount {
            provider,
            credential: json!({}),
            proxy: None,
            enabled: true,
        });
        if account.provider != provider {
            return Err(VaultError("account provider cannot change"));
        }
        account.proxy = Some(proxy.into());
        *data.account_revisions.entry(name.into()).or_default() += 1;
        data.changed();
        Ok(())
    }
    pub fn accounts(&self) -> Result<Vec<Value>> {
        let data = self.lock()?;
        Ok(data.bundle.as_ref().into_iter().flat_map(|b| b.accounts.iter()).map(|(name,a)| json!({"name":name,"provider":a.provider,"revision":data.account_revisions[name],"proxy_configured":a.proxy.is_some()})).collect())
    }
    pub fn put_account(&self, name: &str, provider: Provider, record: &Value) -> Result<()> {
        validate_account(name, record)?;
        validate_credential(provider, record)?;
        let mut data = self.lock()?;
        let bundle = data.bundle.get_or_insert_with(|| Provision {
            config: serde_json::from_value(
                json!({"server":{"allow_empty_routes":true},"keys":[],"models":[]}),
            )
            .unwrap(),
            secrets: BTreeMap::new(),
            accounts: BTreeMap::new(),
        });
        let account = bundle.accounts.entry(name.into()).or_insert(StoredAccount {
            provider,
            credential: json!({}),
            proxy: None,
            enabled: true,
        });
        if account.provider != provider {
            return Err(VaultError("account provider cannot change"));
        }
        account.credential = record.clone();
        *data.account_revisions.entry(name.into()).or_default() += 1;
        data.retire(&format!("oauth:{}:{name}", provider_id(provider)));
        data.changed();
        Ok(())
    }
    pub fn import_account(&self, name: &str, provider: Provider, record: &Value) -> Result<()> {
        validate_credential(provider, record)?;
        self.put_account(name, provider, record)
    }
    pub fn provision(&self, bundle: &Provision) -> Result<()> {
        if serde_json::to_vec(bundle)
            .map_err(|_| VaultError("invalid configuration"))?
            .len()
            > 8 * 1024 * 1024
        {
            return Err(VaultError("configuration exceeds 8 MiB"));
        }
        bundle
            .config
            .validate()
            .map_err(|_| VaultError("invalid configuration"))?;
        let config = &bundle.config;
        let required = |name: &str| -> Result<()> {
            if bundle.secrets.get(name).is_none_or(|v| v.trim().is_empty()) {
                return Err(VaultError("configuration references a missing secret"));
            }
            Ok(())
        };
        for key in &config.keys {
            required(&key.key_env)?;
        }
        let mut client_keys = std::collections::HashSet::new();
        for key in &config.keys {
            if !client_keys.insert(&bundle.secrets[&key.key_env]) {
                return Err(VaultError("client keys must be unique"));
            }
        }
        if let Some(name) = &config.server.admin_key_env {
            required(name)?;
            if client_keys.contains(&bundle.secrets[name]) {
                return Err(VaultError("admin key must differ from client keys"));
            }
        }
        if let Some(name) = &config.server.router_access_key_ref {
            required(name)?;
            let value = &bundle.secrets[name];
            if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit())
                || client_keys.contains(value)
                || config.server.admin_key_env.as_ref().is_some_and(|k| bundle.secrets[k] == *value) {
                return Err(VaultError("Router access signing key must be independent and 32-byte hex"));
            }
        }
        for deployment in config.models.iter().flat_map(|m| &m.deployments) {
            if deployment.oauth_file.is_some()
                || (deployment.enabled && deployment.model.starts_with("env:"))
            {
                return Err(VaultError(
                    "external file/environment references require explicit legacy import",
                ));
            }
            if !deployment.enabled {
                continue;
            }
            if let Some(base) = &deployment.api_base {
                crate::client::parse_base_url(base)
                    .map_err(|_| VaultError("invalid provider base URL"))?;
            }
            if !deployment.api_key_env.is_empty() {
                validate_proxy(
                    deployment.provider,
                    deployment
                        .proxy
                        .as_deref()
                        .ok_or(VaultError("API-key deployment proxy is required"))?,
                )?;
                required(&deployment.api_key_env)?;
                reqwest::header::HeaderValue::from_str(&bundle.secrets[&deployment.api_key_env])
                    .map_err(|_| VaultError("invalid provider key"))?;
            }
            if let Some(name) = &deployment.oauth_account {
                if deployment.proxy.is_some() {
                    return Err(VaultError("OAuth proxy must be configured on the account"));
                }
                self.provisioned_proxy(bundle, name, deployment.provider)?;
                let record = if let Some(account) = bundle.accounts.get(name) {
                    if account.provider != deployment.provider {
                        return Err(VaultError("account provider mismatch"));
                    }
                    account.credential.clone()
                } else {
                    self.account(name, deployment.provider)?.0
                };
                validate_account(name, &record)?;
                validate_credential(deployment.provider, &record)?;
            }
        }
        for (name, account) in &bundle.accounts {
            validate_account(name, &account.credential)?;
            if !account.credential.as_object().is_some_and(|m| m.is_empty()) {
                validate_credential(account.provider, &account.credential)?;
            }
            self.provisioned_proxy(bundle, name, account.provider)?;
        }
        let mut data = self.lock()?;
        for (name, account) in &bundle.accounts {
            if data
                .bundle
                .as_ref()
                .and_then(|b| b.accounts.get(name))
                .is_some_and(|a| a.provider != account.provider)
            {
                return Err(VaultError("account provider cannot change"));
            }
        }
        let mut next = bundle.clone();
        if let Some(previous) = &data.bundle {
            for (name, account) in &previous.accounts {
                next.accounts
                    .entry(name.clone())
                    .or_insert_with(|| account.clone());
            }
        }
        for (name, account) in &mut next.accounts {
            if account.proxy.is_none() {
                account.proxy = data
                    .bundle
                    .as_ref()
                    .and_then(|b| b.accounts.get(name))
                    .and_then(|a| a.proxy.clone());
            }
            *data.account_revisions.entry(name.clone()).or_default() += 1;
        }
        data.bundle = Some(next);
        data.revision += 1;
        for (binding, _) in data.sessions.values_mut() {
            *binding = "!retired".into();
        }
        data.changed();
        Ok(())
    }
    fn provisioned_proxy(&self, bundle: &Provision, name: &str, provider: Provider) -> Result<()> {
        if let Some(proxy) = bundle.accounts.get(name).and_then(|a| a.proxy.as_deref()) {
            validate_proxy(provider, proxy)
        } else {
            self.account_proxy(name, provider).map(|_| ())
        }
    }
}

impl Provision {
}
fn validate_proxy(provider: Provider, proxy: &str) -> Result<()> {
    crate::network::validate_proxy(provider, proxy)
        .map(|_| ())
        .map_err(|_| {
            VaultError("invalid proxy: expected http, https, socks5h or supported vless URL")
        })
}
fn provider_id(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAI => "openai",
        Provider::Anthropic => "anthropic",
        Provider::XAI => "xai",
    }
}
fn validate_account(name: &str, record: &Value) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        || !record.is_object()
    {
        return Err(VaultError("invalid account name or credential object"));
    }
    Ok(())
}

fn validate_credential(provider: Provider, record: &Value) -> Result<()> {
    let fields = record
        .as_object()
        .ok_or(VaultError("credential must be an object"))?;
    if fields.keys().any(|k| {
        !matches!(
            k.as_str(),
            "access_token"
                | "expires_at"
                | "account_id"
                | "id_token"
                | "token_type"
                | "scope"
                | "claudeAiOauth"
        )
    }) {
        return Err(VaultError(
            "access-token-only protocol: refresh tokens and unknown credential fields are forbidden",
        ));
    }
    let value = if let Some(legacy) = record.get("claudeAiOauth") {
        if provider != Provider::Anthropic
            || fields.len() != 1
            || legacy.as_object().is_none_or(|o| {
                o.keys().any(|k| {
                    !matches!(
                        k.as_str(),
                        "accessToken"
                            | "expiresAt"
                            | "scopes"
                            | "subscriptionType"
                            | "rateLimitTier"
                    )
                })
            })
        {
            return Err(VaultError(
                "access-token-only protocol: invalid Claude credential fields",
            ));
        }
        for key in ["subscriptionType", "rateLimitTier"] {
            if legacy
                .get(key)
                .is_some_and(|v| !v.is_string() && !v.is_null())
            {
                return Err(VaultError("invalid Claude metadata"));
            }
        }
        if legacy.get("scopes").is_some_and(|v| {
            v.as_array()
                .is_none_or(|a| a.iter().any(|v| !v.is_string()))
        }) {
            return Err(VaultError("invalid Claude scopes"));
        }
        if legacy
            .get("expiresAt")
            .is_some_and(|v| v.as_f64().is_none_or(|n| !n.is_finite() || n <= 0.0))
        {
            return Err(VaultError("invalid expiry"));
        }
        &legacy["accessToken"]
    } else {
        &record["access_token"]
    };
    for key in ["account_id", "id_token", "scope", "token_type"] {
        if record.get(key).is_some_and(|v| !v.is_string()) {
            return Err(VaultError("credential metadata must be text"));
        }
    }
    if record
        .get("expires_at")
        .is_some_and(|v| v.as_f64().is_none_or(|n| !n.is_finite() || n <= 0.0))
    {
        return Err(VaultError("expiry must be a positive Unix timestamp"));
    }
    let token = value
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(VaultError("account requires access_token"))?;
    if token.len() > 65536 || reqwest::header::HeaderValue::from_str(token).is_err() {
        return Err(VaultError("invalid access token"));
    }
    if record.to_string().len() > 1024 * 1024 {
        return Err(VaultError("credential record exceeds limit"));
    }
    Ok(())
}

#[path = "vault/operations.rs"]
pub(crate) mod operations;
#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;

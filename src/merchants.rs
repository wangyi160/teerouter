//! Isolated merchant runtimes. Platform keys identify users; merchant keys never leave their owners.
use crate::{
    config::{ApiKey, Config, Model, User},
    error::ApiError,
    runtime::{Runtime, storage},
    vault::{Data, Provision, Vault, operations::Update},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Instant,
};
use subtle::ConstantTimeEq;
mod crypto;
mod customers;
pub(crate) mod http;
mod model_catalog;
mod probe;
pub(crate) mod public_catalog;
pub(crate) mod route_metrics;
pub(crate) mod route_rule;
pub(crate) mod routing;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Capsule {
    version: u32,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Registration {
    pub id: String,
    pub credential_hash: String,
    pub name: String,
    pub enabled: bool,
    pub max_concurrent: usize,
    pub rpm: u64,
    pub tpm: u64,
    #[serde(default)]
    pub allow_test: bool,
    #[serde(default)]
    pub users: BTreeSet<String>,
    #[serde(default)]
    pub backup: Option<Capsule>,
}
impl Registration {
    pub fn validate(&self) -> bool {
        self.credential_hash.len() == 64
            && self
                .credential_hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && self.id == format!("m_{}", &self.credential_hash[..32])
            && !self.name.is_empty()
            && self.name.len() <= 128
            && (1..=4096).contains(&self.max_concurrent)
            && self.rpm <= 10_000_000
            && self.tpm <= 10_000_000_000
            && self.users.len() <= 100_000
            && self.users.iter().all(|s| crate::config::valid_name(s))
    }
    pub fn public(&self, unlocked: bool) -> Value {
        json!({"id":self.id,"name":self.name,"credential_hash":self.credential_hash,"enabled":self.enabled,
            "max_concurrent":self.max_concurrent,"rpm":self.rpm,"tpm":self.tpm,"allow_test":self.allow_test,
            "users":self.users,"unlocked":unlocked,"has_backup":self.backup.is_some()})
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Price {
    pub pool: String,
    pub model: String,
    /// Integer micro-USD per million tokens, including cache categories.
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_read: Option<u64>,
    #[serde(default)]
    pub cache_write: Option<u64>,
    #[serde(default)]
    pub cache_write_hour: Option<u64>,
    #[serde(default)]
    pub long_context: Option<LongContextPrice>,
    #[serde(default)]
    pub valid_until: Option<u64>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LongContextPrice {
    pub above_input_tokens: u64,
    pub rates: TokenRates,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenRates {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_read: Option<u64>,
    #[serde(default)]
    pub cache_write: Option<u64>,
    #[serde(default)]
    pub cache_write_hour: Option<u64>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Customer {
    pub user_id: String,
    pub enabled: bool,
    pub models: Vec<String>,
    pub max_concurrent: usize,
    #[serde(default)]
    pub rpm: u64,
    #[serde(default)]
    pub tpm: u64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub revision: u64,
    pub max_concurrent: usize,
    pub rpm: u64,
    pub tpm: u64,
    pub customers: Vec<Customer>,
    #[serde(default)]
    pub customer_defaults: customers::Defaults,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prices: Vec<Price>,
    #[serde(default)]
    pub pool_pricing: Vec<crate::pricing::PoolPrice>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: 1,
            max_concurrent: 16,
            rpm: 0,
            tpm: 0,
            customers: vec![],
            customer_defaults: customers::Defaults::default(),
            prices: vec![],
            pool_pricing: vec![],
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    id: String,
    state: Data,
    settings: Settings,
}

pub(crate) struct Live {
    probe_gate: Arc<crate::resources::Gate>,
    pub runtime: Arc<Runtime>,
    pub settings: Mutex<Settings>,
    admin: zeroize::Zeroizing<String>,
    recovery_key: zeroize::Zeroizing<[u8; 32]>,
    checkpoint_revision: Mutex<Option<(u64, u64)>>,
    pub gate: Arc<crate::resources::Gate>,
    pub meter: Mutex<RateMeter>,
}
pub(crate) struct RateMeter {
    pub start: Instant,
    pub requests: u64,
    pub initial_tokens: u64,
}
#[derive(Default)]
pub(crate) struct Registry {
    probe_gate: Arc<crate::resources::Gate>,
    pub routing: Arc<route_metrics::State>,
    pub latency: Arc<crate::network::latency::State>,
    pub live: Mutex<BTreeMap<String, Arc<Live>>>,
    pub write: tokio::sync::Mutex<()>,
}
pub(super) fn invalid() -> ApiError {
    ApiError::new(
        400,
        "invalid_merchant_update",
        "Invalid merchant configuration",
    )
}
pub(super) fn unavailable() -> ApiError {
    ApiError::new(
        503,
        "merchant_storage_unavailable",
        "Merchant state is unavailable",
    )
}
pub(super) fn forbidden() -> ApiError {
    ApiError::new(
        403,
        "merchant_forbidden",
        "Merchant resource is not authorized",
    )
}
pub(crate) fn authenticate(runtime: &Runtime, token: &str) -> Result<Registration, ApiError> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(forbidden());
    }
    let digest = crypto::digest(token);
    let id = crypto::identity(token);
    let engine = runtime.engine();
    let data = engine.vault.lock().map_err(storage)?;
    let item = data
        .merchants
        .get(&id)
        .filter(|item| {
            item.enabled && bool::from(item.credential_hash.as_bytes().ct_eq(digest.as_bytes()))
        })
        .ok_or_else(forbidden)?;
    Ok(item.clone())
}
pub(crate) fn live(runtime: &Runtime, id: &str) -> Result<Arc<Live>, ApiError> {
    runtime
        .merchants
        .live
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .ok_or_else(|| {
            ApiError::new(
                423,
                "merchant_locked",
                "Merchant must verify TEE and unlock its recovery capsule",
            )
        })
}
pub(crate) fn records(runtime: &Runtime) -> Result<Vec<Registration>, ApiError> {
    Ok(runtime
        .engine()
        .vault
        .lock()
        .map_err(storage)?
        .merchants
        .values()
        .cloned()
        .collect())
}
pub(crate) async fn register(
    runtime: &Runtime,
    mut item: Registration,
    secret: &str,
) -> Result<Value, ApiError> {
    let _write = runtime.merchants.write.lock().await;
    let engine = runtime.engine();
    if !engine.authenticate_admin(secret) {
        return Err(forbidden());
    }
    if !item.validate() || item.backup.is_some() {
        return Err(invalid());
    }
    let mut data = engine.vault.lock().map_err(storage)?;
    if item
        .users
        .iter()
        .any(|id| !engine.config.users.iter().any(|u| &u.id == id))
    {
        return Err(invalid());
    }
    if let Some(old) = data.merchants.get(&item.id) {
        if old.credential_hash != item.credential_hash {
            return Err(forbidden());
        }
        item.backup = old.backup.clone();
    }
    data.merchants.insert(item.id.clone(), item.clone());
    data.changed();
    Ok(item.public(
        runtime
            .merchants
            .live
            .lock()
            .unwrap()
            .contains_key(&item.id),
    ))
}

pub(crate) async fn unlock(
    runtime: &Runtime,
    token: &str,
    minimum_revision: u64,
) -> Result<Value, ApiError> {
    let _write = runtime.merchants.write.lock().await;
    let item = authenticate(runtime, token)?;
    if let Ok(live) = live(runtime, &item.id) {
        if live.settings.lock().unwrap().revision < minimum_revision {
            return Err(ApiError::new(
                409,
                "merchant_rollback",
                "Merchant revision is older than locally accepted state",
            ));
        }
        return state(runtime, &item);
    }
    let recovery_key = crypto::key(token);
    let vault = Vault::memory().map_err(storage)?;
    let (settings, admin) = if let Some(capsule) = &item.backup {
        let mut recovery = crypto::open(&item.id, &recovery_key, capsule)?;
        if recovery.settings.revision < minimum_revision {
            return Err(ApiError::new(
                409,
                "merchant_rollback",
                "Merchant revision is older than locally accepted state",
            ));
        }
        crate::refusal::restore(&mut recovery.state).map_err(|_| invalid())?;
        customers::restore(&mut recovery.state);
        let bundle = recovery.state.bundle.as_ref().ok_or_else(invalid)?;
        crate::pricing::migrate(
            &mut recovery.settings,
            &bundle.config,
            &crate::pricing::book(runtime)?,
        )?;
        validate_bundle(bundle, &item, &recovery.settings)?;
        let admin = bundle
            .secrets
            .get("merchant_internal_admin")
            .cloned()
            .ok_or_else(invalid)?;
        vault.provision(bundle).map_err(|_| invalid())?;
        recovery.state.pending_event_bytes = recovery
            .state
            .events
            .iter()
            .map(|e| serde_json::to_vec(e).map_or(0, |v| v.len()))
            .sum();
        *vault.lock().map_err(storage)? = recovery.state;
        (recovery.settings, admin)
    } else {
        if minimum_revision > 1 {
            return Err(ApiError::new(
                409,
                "merchant_backup_missing",
                "Merchant recovery capsule is missing",
            ));
        }
        let admin = format!(
            "{}{}",
            crate::vault::random_id().map_err(storage)?,
            crate::vault::random_id().map_err(storage)?
        );
        let config: Config = serde_json::from_value(json!({"server":{"admin_key_ref":"merchant_internal_admin","allow_empty_routes":true},"keys":[],"models":[]})).map_err(|_| invalid())?;
        vault
            .provision(&Provision {
                config,
                secrets: BTreeMap::from([("merchant_internal_admin".into(), admin.clone())]),
                accounts: BTreeMap::new(),
            })
            .map_err(|_| invalid())?;
        let settings = Settings {
            max_concurrent: item.max_concurrent.min(16),
            ..Settings::default()
        };
        (settings, admin)
    };
    let engine = crate::router::Engine::from_vault(vault).map_err(|_| invalid())?;
    let initial_tokens =
        engine
            .vault
            .lock()
            .map_err(storage)?
            .usage
            .values()
            .fold(0u64, |sum, u| {
                sum.saturating_add(u.input_tokens)
                    .saturating_add(u.output_tokens)
            });
    let child = runtime.merchant_child(engine);
    runtime.merchants.live.lock().unwrap().insert(
        item.id.clone(),
        Arc::new(Live {
            probe_gate: Arc::default(),
            runtime: child,
            settings: Mutex::new(settings),
            admin: zeroize::Zeroizing::new(admin),
            recovery_key: zeroize::Zeroizing::new(recovery_key),
            checkpoint_revision: Mutex::new(None),
            gate: Arc::default(),
            meter: Mutex::new(RateMeter {
                start: Instant::now(),
                requests: 0,
                initial_tokens,
            }),
        }),
    );
    checkpoint_one(runtime, &item.id)?;
    state(runtime, &item)
}

pub(crate) fn checkpoint_one(runtime: &Runtime, id: &str) -> Result<(), ApiError> {
    let live = live(runtime, id)?;
    let settings = live.settings.lock().unwrap().clone();
    let data = live.runtime.engine().vault.lock().map_err(storage)?.clone();
    let revision = (data.generation, settings.revision);
    let mut saved_revision = live.checkpoint_revision.lock().unwrap();
    if *saved_revision == Some(revision) {
        return Ok(());
    }
    let capsule = crypto::seal(
        id,
        &live.recovery_key,
        &Recovery {
            id: id.into(),
            state: data,
            settings,
        },
    )?;
    let engine = runtime.engine();
    let mut data = engine.vault.lock().map_err(storage)?;
    data.merchants.get_mut(id).ok_or_else(forbidden)?.backup = Some(capsule);
    data.changed();
    *saved_revision = Some(revision);
    Ok(())
}
pub(crate) async fn checkpoint_all(runtime: &Runtime) -> Result<(), ApiError> {
    let _write = runtime.merchants.write.lock().await;
    let ids: Vec<_> = runtime
        .merchants
        .live
        .lock()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    for id in ids {
        checkpoint_one(runtime, &id)?;
    }
    Ok(())
}

fn validate_bundle(
    bundle: &Provision,
    item: &Registration,
    settings: &Settings,
) -> Result<(), ApiError> {
    crate::pricing::validate_pools(settings, &bundle.config)?;
    if settings.revision == 0
        || !(1..=4096).contains(&settings.max_concurrent)
        || settings.rpm > 10_000_000
        || settings.tpm > 10_000_000_000
        || settings.customers.len() > 100_000
        || settings.prices.len() > 8192
        || bundle.config.models.len() > 1024
        || bundle.accounts.len() > 4096
    {
        return Err(invalid());
    }
    let mut names = BTreeSet::new();
    if settings
        .customers
        .iter()
        .any(|c| !crate::config::valid_name(&c.user_id) || !names.insert(&c.user_id))
    {
        return Err(invalid());
    }
    let defaults = &settings.customer_defaults;
    let default_customer = Customer {
        user_id: "merchant-default-validation".into(),
        enabled: defaults.enabled,
        models: defaults.models.clone(),
        max_concurrent: defaults.max_concurrent,
        rpm: defaults.rpm,
        tpm: defaults.tpm,
    };
    for c in settings
        .customers
        .iter()
        .chain(std::iter::once(&default_customer))
    {
        if !(1..=4096).contains(&c.max_concurrent)
            || c.rpm > 10_000_000
            || c.tpm > 10_000_000_000
            || c.models.len() > 8192
            || c.models.iter().any(|rule| {
                !crate::config::valid_model_id(rule)
                    && !bundle
                        .config
                        .models
                        .iter()
                        .any(|p| p.name == *rule || rule == &format!("{}/*", p.name))
            })
        {
            return Err(invalid());
        }
    }
    let mut prices = BTreeSet::new();
    for p in &settings.prices {
        if p.long_context.as_ref().is_some_and(|tier| {
            tier.above_input_tokens == 0
                || tier.above_input_tokens > 1_000_000_000
                || [
                    Some(tier.rates.input),
                    Some(tier.rates.output),
                    tier.rates.cache_read,
                    tier.rates.cache_write,
                    tier.rates.cache_write_hour,
                ]
                .into_iter()
                .flatten()
                .any(|v| v > 1_000_000_000_000)
        }) {
            return Err(invalid());
        }
        if !bundle.config.models.iter().any(|g| g.name == p.pool)
            || !crate::config::valid_model_id(&p.model)
            || !prices.insert((&p.pool, &p.model))
            || [
                Some(p.input),
                Some(p.output),
                p.cache_read,
                p.cache_write,
                p.cache_write_hour,
            ]
            .into_iter()
            .flatten()
            .any(|r| r > 1_000_000_000_000)
        {
            return Err(invalid());
        }
    }
    for pool in &bundle.config.models {
        if pool.name.len() > 80 || pool.provider.is_none() || !pool.fallbacks.is_empty() {
            return Err(invalid());
        }
        for d in &pool.deployments {
            if d.oauth_file.is_some()
                || d.api_key_env.starts_with("merchant_internal_")
                || d.api_base.is_some()
                || d.provider.is_local_test() && !item.allow_test
            {
                return Err(forbidden());
            }
        }
    }
    bundle.config.validate().map_err(|_| invalid())
}

#[cfg(test)]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MerchantUpdate {
    pub expected_revision: u64,
    pub pools: Vec<Model>,
    pub customers: Vec<Customer>,
    pub customer_defaults: Option<customers::Defaults>,
    #[serde(default)]
    pub prices: Vec<Price>,
    pub pool_pricing: Vec<crate::pricing::PoolPrice>,
    pub max_concurrent: usize,
    #[serde(default)]
    pub rpm: u64,
    #[serde(default)]
    pub tpm: u64,
    #[serde(default)]
    pub accounts: BTreeMap<String, Value>,
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    #[serde(default)]
    pub delete_accounts: Vec<String>,
    #[serde(default)]
    pub delete_secrets: Vec<String>,
    pub compatibility: Option<BTreeMap<String, crate::compatibility::Profile>>,
    pub restrictions: Option<Vec<crate::policy::Restriction>>,
}
pub(crate) async fn apply(
    runtime: &Runtime,
    token: &str,
    mut input: MerchantUpdate,
) -> Result<Value, ApiError> {
    let _write = runtime.merchants.write.lock().await;
    let item = authenticate(runtime, token)?;
    let live = live(runtime, &item.id)?;
    if !input.prices.is_empty() {
        return Err(ApiError::new(
            400,
            "pool_multiplier_required",
            "Set a multiplier per pool; official model prices are platform managed",
        ));
    }
    let old = live.settings.lock().unwrap().clone();
    if old.revision != input.expected_revision {
        return Err(ApiError::new(
            409,
            "revision_conflict",
            "Reload merchant settings before editing",
        ));
    }
    let known = customers::known(&live)?;
    if input.customers.iter().any(|c| !known.contains(&c.user_id)) {
        return Err(forbidden());
    }
    if input
        .secrets
        .keys()
        .chain(input.delete_secrets.iter())
        .any(|k| k.starts_with("merchant_internal_"))
    {
        return Err(forbidden());
    }
    let mut ids = BTreeSet::new();
    if input
        .customers
        .iter()
        .any(|c| !ids.insert(c.user_id.clone()))
    {
        return Err(invalid());
    }
    // Omission is not revocation: filtered or stale lists must not erase a
    // hidden legacy override or a newly discovered customer's restrictions.
    let mut overrides = old.customers.clone();
    overrides.retain(|c| !ids.contains(&c.user_id));
    overrides.extend(input.customers);
    let settings = Settings {
        revision: old.revision.checked_add(1).ok_or_else(invalid)?,
        max_concurrent: input.max_concurrent,
        rpm: input.rpm,
        tpm: input.tpm,
        customers: overrides,
        customer_defaults: input.customer_defaults.unwrap_or(old.customer_defaults),
        prices: vec![],
        pool_pricing: input.pool_pricing,
    };
    let snapshot = live.runtime.engine().vault.snapshot().map_err(storage)?;
    for deployment in input.pools.iter_mut().flat_map(|p| &mut p.deployments) {
        if deployment.proxy.as_deref() == Some("[redacted]") {
            deployment.proxy = snapshot
                .bundle
                .config
                .models
                .iter()
                .flat_map(|p| &p.deployments)
                .find(|d| d.id == deployment.id)
                .and_then(|d| d.proxy.clone());
        }
    }
    let mut config = snapshot.bundle.config.clone();
    config.models = input.pools;
    if let Some(profiles) = input.compatibility {
        config.compatibility = profiles;
    }
    if let Some(restrictions) = input.restrictions {
        config.restrictions = restrictions;
    }
    config.server.max_concurrent = settings.max_concurrent.min(item.max_concurrent);
    let mut secrets = input.secrets;
    let ids = config
        .keys
        .iter()
        .map(|k| k.user_id.clone())
        .chain(settings.customers.iter().map(|c| c.user_id.clone()))
        .chain(known)
        .collect();
    customers::configure(
        &mut config,
        &settings,
        ids,
        &snapshot.bundle.secrets,
        &mut secrets,
    )?;
    let update = Update {
        expected_revision: Some(snapshot.revision),
        config: Some(serde_json::to_value(config).map_err(|_| invalid())?),
        secrets,
        accounts: input.accounts,
        delete_accounts: input.delete_accounts,
        delete_secrets: input.delete_secrets,
        ..Update::default()
    };
    let candidate = snapshot.updated(&update).map_err(|_| invalid())?;
    validate_bundle(&candidate, &item, &settings)?;
    live.runtime.apply(update, &live.admin).await?;
    *live.settings.lock().unwrap() = settings;
    checkpoint_one(runtime, &item.id)?;
    state(runtime, &item)
}

pub(crate) fn state(runtime: &Runtime, item: &Registration) -> Result<Value, ApiError> {
    let live = live(runtime, &item.id)?;
    let engine = live.runtime.engine();
    let mut pools = serde_json::to_value(&engine.config.models).map_err(|_| invalid())?;
    fn redact(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    if k == "proxy" && !v.is_null() {
                        *v = json!("[redacted]");
                    } else {
                        redact(v);
                    }
                }
            }
            Value::Array(a) => {
                for v in a {
                    redact(v)
                }
            }
            _ => {}
        }
    }
    redact(&mut pools);
    let mut settings = live.settings.lock().unwrap().clone();
    settings.customers = customers::visible(&live, &settings)?;
    let mut merchant = item.public(true);
    merchant.as_object_mut().unwrap().remove("users");
    let mut book = crate::pricing::book(runtime)?;
    book.providers.retain(|p| {
        engine
            .config
            .models
            .iter()
            .any(|m| m.provider == Some(p.provider))
    });
    Ok(
        json!({"merchant":merchant,"settings":settings,"pools":pools,
        "official_pricing":book,"accounts":engine.account_status()?,"status":engine.status(),"catalog":engine.discovery_status()}),
    )
}

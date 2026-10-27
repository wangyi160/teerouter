//! Merchant secrets exist only in merchant-owned storage and the live TEE runtime.
#[cfg(test)]
use crate::vault::Data;
use crate::{
    config::{ApiKey, Config, Model, User},
    error::ApiError,
    runtime::{Runtime, storage},
    vault::{Provision, Vault, operations::Update},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Instant,
};
use subtle::ConstantTimeEq;
mod access;
mod account_sync;
pub(crate) mod control;
mod crypto;
mod customers;
pub(crate) mod http;
#[cfg(test)]
mod model_catalog;
mod probe;
pub(crate) mod public_catalog;
pub(crate) mod refusals;
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
    /// Independent merchant-local control secret. Only its digest may reach management.
    #[serde(default)]
    pub owner_hash: Option<String>,
    pub name: String,
    pub enabled: bool,
    pub max_concurrent: usize,
    pub rpm: u64,
    pub tpm: u64,
    #[serde(default)]
    pub allow_test: bool,
    #[serde(default)]
    pub users: BTreeSet<String>,
    // Read legacy checkpoints for migration, but never export or restore their capsules.
    #[serde(default, skip_serializing)]
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
            && self.owner_hash.as_deref().is_none_or(crypto::valid_secret)
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
            "users":self.users,"unlocked":unlocked,"has_backup":false,
            "owner_hash":self.owner_hash,"merchant_control_version":1})
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
    #[serde(default)]
    pub refusal_limit: Option<u64>,
    #[serde(default)]
    pub refusal_baseline: u64,
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
pub(crate) struct Live {
    control: Mutex<control::State>,
    probe_gate: Arc<crate::resources::Gate>,
    pub runtime: Arc<Runtime>,
    pub settings: Mutex<Settings>,
    admin: zeroize::Zeroizing<String>,
    configured: std::sync::atomic::AtomicBool,
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
    let (enrollment, owner) = crypto::control_token(token)?;
    let item = authenticate_enrollment(runtime, enrollment)?;
    let digest = crypto::digest(owner);
    if !item
        .owner_hash
        .as_ref()
        .is_some_and(|expected| bool::from(expected.as_bytes().ct_eq(digest.as_bytes())))
    {
        return Err(forbidden());
    }
    Ok(item)
}
fn authenticate_enrollment(runtime: &Runtime, token: &str) -> Result<Registration, ApiError> {
    if !crypto::valid_secret(token) {
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
/// Bind a local control key before any account material is submitted. Enrollment
/// alone can never access or replace an already-bound merchant's private state.
pub(crate) async fn bind(runtime: &Runtime, token: &str) -> Result<Value, ApiError> {
    let _write = runtime.merchants.write.lock().await;
    let (_, owner) = crypto::control_token(token)?;
    let item = authorize_binding(runtime, token)?;
    let digest = crypto::digest(owner);
    if item
        .owner_hash
        .as_ref()
        .is_some_and(|old| !bool::from(old.as_bytes().ct_eq(digest.as_bytes())))
    {
        return Err(forbidden());
    }
    let engine = runtime.engine();
    let mut data = engine.vault.lock().map_err(storage)?;
    if item.owner_hash.is_none() {
        data.merchants
            .get_mut(&item.id)
            .ok_or_else(forbidden)?
            .owner_hash = Some(digest.clone());
        data.changed();
    }
    Ok(json!({"merchant_id":item.id,"owner_hash":digest,"merchant_control_version":1}))
}
pub(crate) fn authorize_binding(runtime: &Runtime, token: &str) -> Result<Registration, ApiError> {
    let (enrollment, owner) = crypto::control_token(token)?;
    let item = authenticate_enrollment(runtime, enrollment)?;
    let digest = crypto::digest(owner);
    if item
        .owner_hash
        .as_ref()
        .is_some_and(|old| !bool::from(old.as_bytes().ct_eq(digest.as_bytes())))
    {
        return Err(forbidden());
    }
    Ok(item)
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
                "Merchant must verify TEE and synchronize its local account configuration",
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
    if !item.validate() || item.backup.is_some() || item.owner_hash.is_some() {
        return Err(invalid());
    }
    let mut data = engine.vault.lock().map_err(storage)?;
    if item
        .users
        .iter()
        .any(|id| !engine.config.users.iter().any(|u| &u.id == id || u.customer_id.as_ref() == Some(id)))
    {
        return Err(invalid());
    }
    if let Some(old) = data.merchants.get(&item.id) {
        if old.credential_hash != item.credential_hash {
            return Err(forbidden());
        }
        item.owner_hash = old.owner_hash.clone();
    }
    data.merchants.insert(item.id.clone(), item.clone());
    data.changed();
    drop(data);
    if let Some(live) = runtime.merchants.live.lock().unwrap().get(&item.id) {
        live.runtime.set_catalogs_enabled(item.enabled);
    }
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
    let vault = Vault::memory().map_err(storage)?;
    let (settings, admin) = {
        if minimum_revision > u64::MAX / 2 {
            return Err(invalid());
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
            revision: minimum_revision.max(1),
            max_concurrent: item.max_concurrent.min(16),
            ..Settings::default()
        };
        (settings, admin)
    };
    // Historical refusal totals prove prior upstream use. Keep blocked customers
    // visible after restart so their merchant can review and reset them.
    let prior_users = runtime
        .engine()
        .vault
        .lock()
        .map_err(storage)?
        .merchant_refusal_counts
        .get(&item.id)
        .cloned()
        .unwrap_or_default();
    vault
        .lock()
        .map_err(storage)?
        .upstream_users
        .extend(prior_users.into_keys());
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
            control: Mutex::default(),
            probe_gate: Arc::default(),
            runtime: child,
            settings: Mutex::new(settings),
            admin: zeroize::Zeroizing::new(admin),
            configured: std::sync::atomic::AtomicBool::new(false),
            gate: Arc::default(),
            meter: Mutex::new(RateMeter {
                start: Instant::now(),
                requests: 0,
                initial_tokens,
            }),
        }),
    );
    state(runtime, &item)
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
        refusal_limit: Some(defaults.refusal_limit),
        refusal_baseline: 0,
    };
    for c in settings
        .customers
        .iter()
        .chain(std::iter::once(&default_customer))
    {
        if !(1..=4096).contains(&c.max_concurrent)
            || c.rpm > 10_000_000
            || c.tpm > 10_000_000_000
            || c.refusal_limit.is_some_and(|limit| limit > 1_000_000_000)
            || c.models.len() > 8192
            || c.models.iter().any(|rule| {
                !crate::config::valid_model_id(rule)
                    && !rule
                        .strip_suffix("/*")
                        .is_some_and(crate::config::valid_name)
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
#[cfg(test)]
pub(crate) async fn apply(
    runtime: &Runtime,
    token: &str,
    input: MerchantUpdate,
) -> Result<Value, ApiError> {
    let _write = runtime.merchants.write.lock().await;
    let item = authenticate(runtime, token)?;
    apply_for(runtime, &item, input).await
}

// The caller holds Registry::write, across validation and activation.
async fn apply_for(
    runtime: &Runtime,
    item: &Registration,
    mut input: MerchantUpdate,
) -> Result<Value, ApiError> {
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
    // Pools are published only while they contain accounts. Full replacement
    // also withdraws the pool and its multiplier after the last account leaves.
    let empty_names: BTreeSet<_> = input
        .pools
        .iter()
        .filter(|pool| pool.deployments.is_empty())
        .map(|pool| pool.name.clone())
        .collect();
    input.pools.retain(|pool| !pool.deployments.is_empty());
    input
        .pool_pricing
        .retain(|price| !empty_names.contains(&price.pool));
    let known = customers::known(&live)?;
    if live.configured.load(std::sync::atomic::Ordering::Acquire)
        && input.customers.iter().any(|c| !known.contains(&c.user_id))
    {
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
    overrides.extend(input.customers.into_iter().map(|mut customer| {
        // A stale editor must not undo a completed manual reset.
        customer.refusal_baseline = customer
            .refusal_baseline
            .max(old.customer(&customer.user_id).refusal_baseline);
        customer
    }));
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
    live.configured
        .store(true, std::sync::atomic::Ordering::Release);
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
    let customer_refusals: BTreeMap<_, _> = settings
        .customers
        .iter()
        .map(|customer| {
            refusals::status(runtime, &item.id, customer, &settings)
                .map(|status| (customer.user_id.clone(), status))
        })
        .collect::<Result<_, _>>()?;
    let mut merchant = item.public(true);
    merchant.as_object_mut().unwrap().remove("users");
    merchant["configuration_required"] =
        (!live.configured.load(std::sync::atomic::Ordering::Acquire)).into();
    merchant["credential_protocol"] = "access-token-only-v1".into();
    merchant["control_protocol"] = "platform-config-v1".into();
    merchant["runtime_id"] = engine
        .vault
        .lock()
        .map_err(storage)?
        .instance
        .clone()
        .into();
    let mut book = crate::pricing::book(runtime)?;
    book.providers.retain(|p| {
        engine
            .config
            .models
            .iter()
            .any(|m| m.provider == Some(p.provider))
    });
    Ok(
        json!({"merchant":merchant,"settings":settings,"pools":pools,"customer_refusals":customer_refusals,
        "compatibility":engine.config.compatibility,"restrictions":engine.config.restrictions,
        "official_pricing":book,"accounts":engine.account_status()?,"status":engine.status(),"catalog":engine.discovery_status()}),
    )
}

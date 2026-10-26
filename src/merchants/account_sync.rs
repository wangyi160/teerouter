//! Owner-only account activation. Platform availability is not a prerequisite.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    operation_id: String,
    runtime_id: String,
    expected_revision: u64,
    pools: Vec<Model>,
    #[serde(default)]
    new_pool_pricing: Vec<crate::pricing::PoolPrice>,
    #[serde(default)]
    accounts: BTreeMap<String, Value>,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    #[serde(default)]
    delete_accounts: Vec<String>,
    #[serde(default)]
    delete_secrets: Vec<String>,
}

pub(super) async fn sync(runtime: &Runtime, token: &str, body: Value) -> Result<Value, ApiError> {
    let digest = crypto::digest(&serde_json::to_string(&body).map_err(|_| invalid())?);
    let input: Input = serde_json::from_value(body).map_err(|_| invalid())?;
    if input.operation_id.is_empty() || input.operation_id.len() > 100
        || !input.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        || input.pools.len() > 256 || input.accounts.len() > 4096 || input.secrets.len() > 4096 {
        return Err(invalid());
    }
    let _write = runtime.merchants.write.lock().await;
    let item = authenticate(runtime, token)?;
    let live = live(runtime, &item.id)?;
    let snapshot = live.runtime.engine().vault.snapshot().map_err(storage)?;
    if live.runtime.engine().vault.lock().map_err(storage)?.instance != input.runtime_id {
        return Err(ApiError::new(409, "merchant_runtime_changed", "Reconnect to the current Router runtime"));
    }
    if let Some((_, old, _)) = live.control.lock().unwrap().owner_completed.iter().find(|(id,_,_)| id == &input.operation_id) {
        if old != &digest { return Err(ApiError::new(409, "operation_conflict", "Operation content changed")); }
        return control::view(runtime, &item);
    }
    let old = live.settings.lock().unwrap().clone();
    let existing: BTreeMap<_,_> = snapshot.bundle.config.models.iter().map(|p| (p.name.clone(), p)).collect();
    // Account edits cannot change an existing pool's price, model policy or
    // merchant/customer limits. A pool's first account publishes its initial
    // local pool definition; later policy edits use the platform channel.
    if input.new_pool_pricing.iter().any(|p| existing.contains_key(&p.pool)) { return Err(invalid()); }
    let mut pools = Vec::new();
    for incoming in input.pools {
        if let Some(previous) = existing.get(&incoming.name) {
            if incoming.provider != previous.provider { return Err(invalid()); }
            let mut pool = (**previous).clone();
            pool.deployments = incoming.deployments;
            pools.push(pool);
        } else { pools.push(incoming); }
    }
    let names: BTreeSet<_> = pools.iter().filter(|p| !p.deployments.is_empty()).map(|p| p.name.clone()).collect();
    let mut prices = old.pool_pricing.clone();
    prices.retain(|p| names.contains(&p.pool));
    prices.extend(input.new_pool_pricing);
    let configured = live.configured.load(std::sync::atomic::Ordering::Acquire);
    apply_for(runtime, &item, MerchantUpdate {
        expected_revision: input.expected_revision, pools,
        customers: vec![], customer_defaults: None, prices: vec![], pool_pricing: prices,
        max_concurrent: old.max_concurrent, rpm: old.rpm, tpm: old.tpm,
        accounts: input.accounts, secrets: input.secrets,
        delete_accounts: input.delete_accounts, delete_secrets: input.delete_secrets,
        compatibility: None, restrictions: None,
    }).await?;
    if !configured { live.configured.store(false, std::sync::atomic::Ordering::Release); }
    let revision = live.settings.lock().unwrap().revision;
    let mut completed = live.control.lock().unwrap();
    completed.owner_completed.push_back((input.operation_id, digest, revision));
    while completed.owner_completed.len() > 256 { completed.owner_completed.pop_front(); }
    drop(completed);
    control::view(runtime, &item)
}

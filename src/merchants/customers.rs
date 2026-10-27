//! Customer visibility is earned by upstream use, never by platform enrollment.
use super::*;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Defaults {
    pub enabled: bool,
    pub models: Vec<String>,
    pub max_concurrent: usize,
    pub rpm: u64,
    pub tpm: u64,
    pub refusal_limit: u64,
}
impl Default for Defaults {
    fn default() -> Self {
        Self {
            enabled: true,
            models: vec![],
            max_concurrent: 16,
            rpm: 0,
            tpm: 0,
            refusal_limit: 0,
        }
    }
}
impl Settings {
    pub(super) fn customer(&self, user: &str) -> Customer {
        self.customers
            .iter()
            .find(|c| c.user_id == user)
            .cloned()
            .unwrap_or_else(|| {
                let d = &self.customer_defaults;
                Customer {
                    user_id: user.into(),
                    enabled: d.enabled,
                    models: d.models.clone(),
                    max_concurrent: d.max_concurrent,
                    rpm: d.rpm,
                    tpm: d.tpm,
                    refusal_limit: None,
                    refusal_baseline: 0,
                }
            })
    }
}

pub(super) fn known(live: &Live) -> Result<BTreeSet<String>, ApiError> {
    Ok(live
        .runtime
        .engine()
        .vault
        .lock()
        .map_err(storage)?
        .upstream_users
        .clone())
}

pub(super) fn visible(live: &Live, settings: &Settings) -> Result<Vec<Customer>, ApiError> {
    Ok(known(live)?
        .iter()
        .map(|id| settings.customer(id))
        .collect())
}

// Old capsules have already ACKed their usage rows. Successful aggregate usage
// is still positive evidence; registration and zero-use customer overrides are not.
#[cfg(test)]
pub(super) fn restore(data: &mut Data) {
    data.upstream_users
        .extend(data.refusal_queue.events.iter().map(|e| e.user_id.clone()));
    if let Some(bundle) = &data.bundle {
        for key in &bundle.config.keys {
            if data
                .usage
                .get(&key.name)
                .is_some_and(|u| u.requests > u.errors || u.input_tokens > 0 || u.output_tokens > 0)
            {
                data.upstream_users.insert(key.user_id.clone());
            }
        }
    }
}

pub(super) fn configure(
    config: &mut Config,
    settings: &Settings,
    ids: BTreeSet<String>,
    existing_secrets: &BTreeMap<String, String>,
    secrets: &mut BTreeMap<String, String>,
) -> Result<(), ApiError> {
    let aliases:BTreeMap<_,_>=config.users.iter().filter_map(|u|u.customer_id.clone().map(|c|(u.id.clone(),c))).collect();
    config.users.clear();
    config.keys.clear();
    for id in ids {
        let c = settings.customer(aliases.get(&id).map_or(id.as_str(),String::as_str));
        config.users.push(User {
            id: id.clone(),
            customer_id: aliases.get(&id).cloned(),
            name: String::new(),
            note: String::new(),
            enabled: c.enabled,
            max_concurrent: c.max_concurrent,
        });
        let key_env = format!("merchant_internal_customer_{id}");
        if !existing_secrets.contains_key(&key_env) {
            secrets.insert(
                key_env.clone(),
                format!(
                    "{}{}",
                    crate::vault::random_id().map_err(storage)?,
                    crate::vault::random_id().map_err(storage)?
                ),
            );
        }
        config.keys.push(ApiKey {
            name: id.clone(),
            user_id: id,
            enabled: c.enabled,
            expires_at: 0,
            key_env,
            models: c.models,
            rpm: c.rpm,
            tpm: c.tpm,
            max_concurrent: c.max_concurrent,
        });
    }
    Ok(())
}

// Called only after platform authorization, under the registry write lock.
// An internal key supports normal per-user admission without exposing the user
// in merchant state. Listing models alone never makes a customer visible.
#[cfg(test)]
pub(super) async fn ensure(live: &Live, user: &str) -> Result<(), ApiError> {ensure_identity(live,user,user).await}
pub(super) async fn ensure_identity(live: &Live, user: &str, customer: &str) -> Result<(), ApiError> {
    let settings = live.settings.lock().unwrap().clone();
    let c = settings.customer(customer);
    let engine = live.runtime.engine();
    if engine.config.customer_id(user)==customer && engine.config.keys.iter().any(|k| {
        k.user_id == user
            && k.enabled == c.enabled
            && k.models == c.models
            && k.max_concurrent == c.max_concurrent
            && k.rpm == c.rpm
            && k.tpm == c.tpm
    }) {
        return Ok(());
    }
    let snapshot = engine.vault.snapshot().map_err(storage)?;
    let mut config = snapshot.bundle.config.clone();
    if user!=customer {
        if let Some(entry)=config.users.iter_mut().find(|u|u.id==user) {entry.customer_id=Some(customer.into());}
        else {config.users.push(User {id:user.into(),customer_id:Some(customer.into()),name:String::new(),note:String::new(),enabled:c.enabled,max_concurrent:c.max_concurrent});}
    }
    let mut ids: BTreeSet<_> = config.keys.iter().map(|k| k.user_id.clone()).collect();
    ids.insert(user.into());
    if ids.len() > 100_000 {
        return Err(unavailable());
    }
    let mut secrets = BTreeMap::new();
    configure(
        &mut config,
        &settings,
        ids,
        &snapshot.bundle.secrets,
        &mut secrets,
    )?;
    live.runtime
        .apply(
            Update {
                expected_revision: Some(snapshot.revision),
                config: Some(serde_json::to_value(config).map_err(|_| invalid())?),
                secrets,
                ..Update::default()
            },
            &live.admin,
        )
        .await?;
    Ok(())
}

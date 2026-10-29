use super::*;
use crate::network::latency::{Snapshot, State};

impl Engine {
    /// Management inventory: aggregates plus usable account aliases. Never
    /// refresh upstreams, create customers, expose credentials or raw errors.
    pub(crate) fn public_pool_inventory(&self, latency: &State) -> Vec<Value> {
        self.config.models.iter().map(|pool| {
            let group = &self.groups[&pool.name];
            let mut models = std::collections::BTreeMap::new();
            let mut bindings = HashSet::new();
            let mut probes = Vec::new();
            let mut client_accounts = Vec::new();
            let (mut used, mut limit, mut available, mut ready) = (0u64, 0u64, 0u64, 0usize);
            for backend in &group.backends {
                let mut visible = false;
                let mut visible_models = 0usize;
                if let Some(catalog) = &backend.catalog {
                    for model in catalog.models() {
                        if pool.allowed_models.as_ref().is_some_and(|ids| !ids.contains(&model)) { continue; }
                        let Ok(info) = catalog.evidence(&model) else { continue };
                        if crate::policy::Restrictions::for_model(&self.config.restrictions,
                            backend.config.provider, &model, &info.canonical_id).block_model { continue; }
                        visible = true;
                        visible_models += 1;
                        models.entry(model.clone()).or_insert_with(|| json!({
                            "id":format!("{}/{}", backend.config.provider.model_namespace(),model),"model":model}));
                    }
                }
                if !bindings.insert(Backend::binding(&backend.config)) { continue; }
                let current = backend.status()["in_flight"].as_u64().unwrap_or(0);
                used = used.saturating_add(current);
                limit = limit.saturating_add(backend.config.max_concurrent as u64);
                if visible && backend.ready() {
                    ready += 1;
                    let probe = latency.snapshot(backend.client.latency.as_deref());
                    probes.push(probe.clone());
                    let protocols: Vec<_> = [(Protocol::Responses, "responses"),
                        (Protocol::Messages, "messages"), (Protocol::Chat, "chat_completions")]
                        .into_iter().filter_map(|(p, name)| backend.client.supports_native(p).then_some(name)).collect();
                    client_accounts.push(json!({"id":backend.config.oauth_account.as_deref().unwrap_or(&backend.config.id),
                        "provider":backend.config.provider,"enabled":true,"available":true,
                        "proxy_latency":probe,"protocols":protocols,
                        "catalog":{"state":"fetched","visible_models":visible_models}}));
                }
                if visible && backend.has_capacity() {
                    available = available.saturating_add((backend.config.max_concurrent as u64).saturating_sub(current));
                }
            }
            let child_limit = self.config.server.max_concurrent as u64;
            let provider = pool.provider.or_else(|| pool.deployments.first().map(|d| d.provider));
            json!({"name":pool.name,"provider":provider,"account_count":pool.deployments.len(),
                "enabled_accounts":bindings.len(),"available_accounts":ready,
                "client_accounts":client_accounts,
                "proxy_latency":Snapshot::fastest(probes.into_iter()),
                "concurrency":{"limit":limit.min(child_limit),"in_flight":used,
                    "available":available.min(child_limit.saturating_sub(self.global.used() as u64))},
                "models":models.into_values().collect::<Vec<_>>()})
        }).collect()
    }

    // Pool-scoped ACLs apply even before an upstream catalog can be fetched.
    // An unscoped model permission can use that model in any pool; this does
    // not grant additional model permissions or expose another key's scopes.
    #[cfg(test)]
    fn pool_visible(&self, account: usize, pool: &str) -> bool {
        let rules = &self.accounts[account].config.models;
        rules.is_empty()
            || rules.iter().any(|rule| match rule.split_once('/') {
                Some((scope, _)) if self.groups.contains_key(scope) => scope == pool,
                _ => true,
            })
    }

    /// Client inventory, deliberately independent of model discovery success.
    /// Read local state only: no refresh, secrets, proxy, or raw errors.
    #[cfg(test)]
    pub(crate) fn pools(&self, account: usize, latency: &State) -> Value {
        let data: Vec<_> = self
            .config
            .models
            .iter()
            .filter(|pool| self.pool_visible(account, &pool.name))
            .map(|pool| {
                let group = &self.groups[&pool.name];
                let mut probes = Vec::new();
                let accounts: Vec<_> = pool
                    .deployments
                    .iter()
                    .map(|deployment| {
                        let backend = group.backends.iter().find(|b| b.config.id == deployment.id);
                        let catalog = backend.and_then(|b| b.catalog.as_ref());
                        let status = catalog.map(|c| c.status());
                        let models = catalog
                            .map(|c| {
                                c.models()
                                    .into_iter()
                                    .filter(|model| {
                                        self.model_allowed(account, &pool.name, model)
                                            && c.evidence(model).is_ok_and(|info| {
                                                !crate::policy::Restrictions::for_model(
                                                    &self.config.restrictions,
                                                    deployment.provider,
                                                    model,
                                                    &info.canonical_id,
                                                )
                                                .block_model
                                            })
                                    })
                                    .count()
                            })
                            .unwrap_or(0);
                        let fetched_at = status.as_ref().and_then(|s| s["fetched_at"].as_u64());
                        let last_error = status.as_ref().and_then(|s| s["last_error"].as_str());
                        let catalog_state = if backend.is_none() {
                            "disabled"
                        } else if status.as_ref().is_some_and(|s| s["refreshing"] == true) {
                            "refreshing"
                        } else if last_error.is_some() {
                            "error"
                        } else if fetched_at.is_none() {
                            "not_fetched"
                        } else {
                            "fetched"
                        };
                        let probe = if backend.is_some_and(|b| b.ready()) {
                            latency.snapshot(backend.and_then(|b| b.client.latency.as_deref()))
                        } else {
                            Snapshot::unavailable()
                        };
                        if models > 0 && backend.is_some_and(|b| b.ready()) {
                            probes.push(probe.clone());
                        }
                        json!({
                            "id": deployment.oauth_account.as_deref().unwrap_or(&deployment.id),
                            "deployment": deployment.id,
                            "provider": deployment.provider,
                            "enabled": backend.is_some(),
                            "available": models > 0 && backend.is_some_and(|b| b.ready()),
                            "proxy_latency": probe,
                            "catalog": {"state": catalog_state, "last_error": last_error,
                                "fetched_at": fetched_at, "visible_models": models}
                        })
                    })
                    .collect();
                let provider = pool.provider.or_else(|| {
                    let first = pool.deployments.first()?.provider;
                    pool.deployments
                        .iter()
                        .all(|d| d.provider == first)
                        .then_some(first)
                });
                json!({"id":pool.name, "object":"pool", "provider":provider,
                    "account_count":accounts.len(),
                    "available_accounts":accounts.iter().filter(|a| a["available"] == true).count(),
                    "proxy_latency":Snapshot::fastest(probes.into_iter()),
                    "accounts":accounts})
            })
            .collect();
        json!({"object":"list", "data":data})
    }
}

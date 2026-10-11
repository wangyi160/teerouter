use super::*;

impl Engine {
    // Pool-scoped ACLs apply even before an upstream catalog can be fetched.
    // An unscoped model permission can use that model in any pool; this does
    // not grant additional model permissions or expose another key's scopes.
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
    pub(crate) fn pools(&self, account: usize) -> Value {
        let data: Vec<_> = self
            .config
            .models
            .iter()
            .filter(|pool| self.pool_visible(account, &pool.name))
            .map(|pool| {
                let group = &self.groups[&pool.name];
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
                        json!({
                            "id": deployment.oauth_account.as_deref().unwrap_or(&deployment.id),
                            "deployment": deployment.id,
                            "provider": deployment.provider,
                            "enabled": backend.is_some(),
                            "available": models > 0 && backend.is_some_and(|b| b.ready()),
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
                    "accounts":accounts})
            })
            .collect();
        json!({"object":"list", "data":data})
    }
}

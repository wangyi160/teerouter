use super::*;
impl Engine {
    pub(crate) fn recover_changed_accounts(
        &self,
        old: &Engine,
        update: &crate::vault::operations::Update,
    ) {
        let current: HashSet<_> = self
            .groups
            .values()
            .flat_map(|g| &g.backends)
            .map(|b| Backend::binding(&b.config))
            .collect();
        for backend in old.groups.values().flat_map(|g| &g.backends) {
            let binding = Backend::binding(&backend.config);
            if !current.contains(&binding) {
                self.cache.invalidate(&binding);
            }
        }
        for backend in self.groups.values().flat_map(|g| &g.backends) {
            let changed = backend.config.oauth_account.as_ref().is_some_and(|n| {
                update.accounts.contains_key(n) || update.proxy_bindings.contains_key(n)
            }) || update.secrets.contains_key(&backend.config.api_key_env)
                || old
                    .groups
                    .values()
                    .flat_map(|g| &g.backends)
                    .find(|b| b.config.id == backend.config.id)
                    .is_some_and(|b| {
                        b.config.proxy != backend.config.proxy
                            || b.config.api_key_env != backend.config.api_key_env
                            || b.config.api_base != backend.config.api_base
                            || serde_json::to_value(&b.client.profile).ok()
                                != serde_json::to_value(&backend.client.profile).ok()
                    });
            if changed {
                backend.recover();
                self.cache.invalidate(&Backend::binding(&backend.config));
            }
        }
    }
    pub(crate) fn recover_storage(&self) -> Result<(), ApiError> {
        if !self.storage_ready() {
            return Err(ApiError::new(
                503,
                "storage_capacity",
                "Runtime state is unavailable",
            ));
        }
        self.vault
            .maintain(&self.config.operations)
            .map_err(crate::runtime::storage)?;
        self.ledger.recover().map_err(crate::runtime::storage)
    }
    pub(crate) fn account_status(&self) -> Result<Value, ApiError> {
        let snapshot = self.vault.snapshot().map_err(crate::runtime::storage)?;
        let mut data = Vec::new();
        for (name, account) in snapshot.bundle.accounts {
            let revision = snapshot.account_revisions[&name];
            let mut health = self
                .vault
                .health(&name, revision)
                .map_err(crate::runtime::storage)?;
            let expiry = crate::oauth::tokens::access_expiry(&account.credential);
            if !account.provider.is_local_test()
                && expiry.is_some_and(|t| t <= crate::oauth::tokens::now())
            {
                health["state"] = "expired".into();
                health["last_error"] = "access_token_expired".into();
                health["next_retry_at"] = 0.into();
            }
            data.push(json!({"name":name,"provider":account.provider,"enabled":account.enabled,"revision":revision,
                "proxy_configured":account.proxy.is_some(),"expires_at":expiry,"health":health,
                "access_sha256":crate::oauth::tokens::access_fingerprint(&account.credential),
                "refresh_mode":if account.provider.is_local_test() {"none"} else {"external"}}));
        }
        Ok(json!({"revision":snapshot.revision,"data":data}))
    }
    pub(crate) fn storage_ready(&self) -> bool {
        self.vault
            .storage_status()
            .is_ok_and(|s| s["low_disk"] == false)
    }
}

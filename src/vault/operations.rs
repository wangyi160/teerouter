use super::*;
pub(crate) struct Snapshot {
    pub bundle: Provision,
    pub revision: i64,
    pub account_revisions: BTreeMap<String, i64>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Update {
    pub expected_revision: Option<i64>,
    pub config: Option<Value>,
    pub secrets: BTreeMap<String, String>,
    pub accounts: BTreeMap<String, Value>,
    /// Resolve proxy secrets inside the vault; administrators never need to read them back.
    pub proxy_bindings: BTreeMap<String, String>,
    pub delete_accounts: Vec<String>,
    pub delete_secrets: Vec<String>,
}
pub(crate) fn merge(target: &mut Value, patch: &Value) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                merge(target.entry(key).or_insert(Value::Null), value);
            }
        }
    } else {
        *target = patch.clone();
    }
}
impl Snapshot {
    pub fn updated(&self, update: &Update) -> Result<Provision> {
        let mut bundle = self.bundle.clone();
        if let Some(patch) = &update.config {
            let mut config =
                serde_json::to_value(&bundle.config).map_err(|_| VaultError("invalid config"))?;
            merge(&mut config, patch);
            bundle.config =
                serde_json::from_value(config).map_err(|_| VaultError("invalid config update"))?;
            if bundle.config.server.proxy.as_deref() == Some("[redacted]") {
                bundle.config.server.proxy = self.bundle.config.server.proxy.clone();
            }
            if bundle.config.server.no_proxy.as_deref() == Some("[redacted]") {
                bundle.config.server.no_proxy = self.bundle.config.server.no_proxy.clone();
            }
            for deployment in bundle
                .config
                .models
                .iter_mut()
                .flat_map(|g| &mut g.deployments)
            {
                if deployment.proxy.as_deref() == Some("[redacted]") {
                    deployment.proxy = self
                        .bundle
                        .config
                        .models
                        .iter()
                        .flat_map(|g| &g.deployments)
                        .find(|old| old.id == deployment.id && old.provider == deployment.provider)
                        .and_then(|old| old.proxy.clone());
                    if deployment.proxy.is_none() {
                        return Err(VaultError("no existing proxy for redacted reference"));
                    }
                }
            }
        }
        for (key, value) in &update.secrets {
            bundle.secrets.insert(key.clone(), value.clone());
        }
        for key in &update.delete_secrets {
            bundle.secrets.remove(key);
        }
        for (name, patch) in &update.accounts {
            let mut account = bundle
                .accounts
                .get(name)
                .map(serde_json::to_value)
                .transpose()
                .map_err(|_| VaultError("invalid account"))?
                .unwrap_or(json!({}));
            // Credential objects are replacements, not recursive merges of different identities.
            let replacement = patch.get("credential").cloned();
            merge(&mut account, patch);
            if let Some(credential) = replacement {
                account["credential"] = credential;
            }
            bundle.accounts.insert(
                name.clone(),
                serde_json::from_value(account)
                    .map_err(|_| VaultError("invalid account update"))?,
            );
        }
        for name in &update.delete_accounts {
            bundle.accounts.remove(name);
        }
        for (account, reference) in &update.proxy_bindings {
            let proxy = bundle
                .secrets
                .get(reference)
                .ok_or(VaultError("proxy secret missing"))?
                .clone();
            let account = bundle
                .accounts
                .get_mut(account)
                .ok_or(VaultError("proxy account missing"))?;
            validate_proxy(account.provider, &proxy)?;
            account.proxy = Some(proxy);
        }
        for deployment in bundle.config.models.iter().flat_map(|m| &m.deployments) {
            if deployment
                .oauth_account
                .as_ref()
                .is_some_and(|name| !bundle.accounts.contains_key(name))
            {
                return Err(VaultError("account still referenced by a deployment"));
            }
        }
        // Removing keys through bulk user synchronization also removes their
        // unused secrets, while preserving references still used elsewhere.
        let config =
            serde_json::to_value(&bundle.config).map_err(|_| VaultError("invalid config"))?;
        fn mentions(value: &Value, name: &str) -> bool {
            match value {
                Value::String(s) => s == name,
                Value::Array(a) => a.iter().any(|v| mentions(v, name)),
                Value::Object(o) => o.values().any(|v| mentions(v, name)),
                _ => false,
            }
        }
        for key in &self.bundle.config.keys {
            if !mentions(&config, &key.key_env) {
                bundle.secrets.remove(&key.key_env);
            }
        }
        Ok(bundle)
    }
}

impl Vault {
    pub(crate) fn audit(&self, operation: &str) -> Result<()> {
        self.lock()?.log(json!({"operation":operation}));
        Ok(())
    }
    pub(crate) fn snapshot(&self) -> Result<Snapshot> {
        let data = self.lock()?;
        Ok(Snapshot {
            bundle: data.bundle.clone().ok_or(VaultError("not initialized"))?,
            revision: data.revision,
            account_revisions: data.account_revisions.clone(),
        })
    }
    pub(crate) fn commit_update(
        &self,
        old: &Snapshot,
        candidate: &Provision,
        update: &Update,
    ) -> Result<bool> {
        let mut data = self.lock()?;
        if data.revision != old.revision || data.account_revisions != old.account_revisions {
            return Ok(false);
        }
        for (name, account) in &candidate.accounts {
            if old
                .bundle
                .accounts
                .get(name)
                .is_some_and(|a| a.provider != account.provider)
            {
                return Err(VaultError("account provider cannot change"));
            }
        }
        let changed: std::collections::BTreeSet<_> = update
            .accounts
            .keys()
            .chain(update.proxy_bindings.keys())
            .collect();
        for (name, account) in &candidate.accounts {
            let provider = provider_id(account.provider);
            let profile_changed =
                serde_json::to_value(old.bundle.config.compatibility.get(provider)).ok()
                    != serde_json::to_value(candidate.config.compatibility.get(provider)).ok();
            if changed.contains(name) || profile_changed {
                *data.account_revisions.entry(name.clone()).or_default() += 1;
                data.health.remove(name);
            }
            if update
                .accounts
                .get(name)
                .is_some_and(|v| v.get("credential").is_some())
            {
                data.retire(&format!("oauth:{provider}:{name}"));
            }
        }
        for name in &update.delete_accounts {
            if let Some(account) = old.bundle.accounts.get(name) {
                data.retire(&format!("oauth:{}:{name}", provider_id(account.provider)));
            }
            data.account_revisions.remove(name);
            data.health.remove(name);
        }
        for before in old
            .bundle
            .config
            .models
            .iter()
            .flat_map(|p| &p.deployments)
            .filter(|d| d.oauth_account.is_none())
        {
            let after = candidate
                .config
                .models
                .iter()
                .flat_map(|p| &p.deployments)
                .find(|d| d.id == before.id);
            if !after.is_some_and(|d| {
                d.provider == before.provider
                    && d.oauth_account.is_none()
                    && d.api_base == before.api_base
                    && candidate.secrets.get(&d.api_key_env)
                        == old.bundle.secrets.get(&before.api_key_env)
            }) {
                data.retire(&format!("deployment:{}", before.id));
            }
        }
        data.bundle = Some(candidate.clone());
        data.revision += 1;
        data.changed();
        let revision = data.revision;
        data.log(json!({"operation":"apply","revision":revision}));
        Ok(true)
    }
    pub(crate) fn health(&self, name: &str, revision: i64) -> Result<Value> {
        Ok(self
            .lock()?
            .health
            .get(name)
            .filter(|(r, _)| *r == revision)
            .map(|(_, v)| v.clone())
            .unwrap_or(json!({"state":"active","failures":0,"next_retry_at":0,"last_error":null})))
    }
    pub(crate) fn record_health(
        &self,
        name: &str,
        revision: i64,
        state: &str,
        failures: u64,
        retry: u64,
        error: Option<&str>,
    ) -> Result<()> {
        let mut data = self.lock()?;
        if data.account_revisions.get(name) == Some(&revision) {
            data.health.insert(name.into(),(revision,json!({"state":state,"failures":failures,"next_retry_at":retry,"last_error":error,"updated_at":crate::time::unix_now()})));
        }
        Ok(())
    }
    pub(crate) fn enabled_account(&self, name: &str) -> Result<bool> {
        Ok(self
            .lock()?
            .bundle
            .as_ref()
            .and_then(|b| b.accounts.get(name))
            .is_some_and(|a| a.enabled))
    }
    pub(crate) fn storage_status(&self) -> Result<Value> {
        let data = self.lock()?;
        Ok(
            json!({"mode":if self.journal_enabled() {"persistent_accounting"} else {"memory"},"journal":self.journal_status(),"low_disk":false,"pending_usage":data.events.len(),
                "pending_refusals":data.refusal_queue.events.len(),"pending_refusal_bytes":data.refusal_queue.bytes,
                "last_refusal_id":data.refusal_queue.last_id,"refusals_deleted_through":data.refusal_queue.deleted_through,
                "manager_connected":self.manager_connected(),"initialized":self.initialized(),"boot_id":self.boot_id}),
        )
    }
    pub(crate) fn maintain(&self, ops: &crate::config::Operations) -> Result<()> {
        let mut data = self.lock()?;
        let cutoff = crate::time::unix_now().saturating_sub(ops.log_retention_days * 86400);
        data.admin_log
            .retain(|v| v["created_at"].as_u64().is_some_and(|n| n >= cutoff));
        while data.admin_log.len() as u64 > ops.max_log_rows {
            data.admin_log.pop_front();
        }
        data.sessions
            .retain(|_, (_, expires)| *expires >= crate::time::unix_now());
        data.catalog.retain(|_, v| {
            v["fetched_at"]
                .as_u64()
                .is_some_and(|t| t.saturating_add(604800) >= crate::time::unix_now())
        });
        Ok(())
    }
    pub(crate) fn logs(&self, admin: bool, after: i64, limit: usize) -> Result<Value> {
        let data = self.lock()?;
        let rows: Vec<Value> = if admin {
            data.admin_log
                .iter()
                .filter(|v| v["id"].as_i64().is_some_and(|n| n > after))
                .take(limit.min(500))
                .cloned()
                .collect()
        } else {
            data.events.iter().filter(|v|v["seq"].as_i64().is_some_and(|n|n>after)).take(limit.min(500)).map(|v|json!({"id":v["seq"],"created_at":v["event"]["created_at"],"event":v["event"]})).collect()
        };
        Ok(json!({"data":rows}))
    }
    pub(crate) fn binding(&self, owner: &str, kind: &str, token: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .sessions
            .get(&(owner.into(), kind.into(), hash(token)))
            .filter(|(_, expires)| *expires >= crate::time::unix_now())
            .map(|(b, _)| b.clone()))
    }
    #[cfg(test)]
    pub(crate) fn bind_session(
        &self,
        owner: &str,
        kind: &str,
        token: &str,
        binding: &str,
        ttl: u64,
        max: u64,
    ) -> Result<bool> {
        self.bind_session_guarded((owner, kind, token), binding, ttl, max, None)
    }
    pub(crate) fn bind_session_guarded(
        &self,
        identity: (&str, &str, &str),
        binding: &str,
        ttl: u64,
        max: u64,
        revision: Option<i64>,
    ) -> Result<bool> {
        let (owner, kind, token) = identity;
        let mut data = self.lock()?;
        let stale = revision.is_some_and(|r| r != data.revision);
        if stale && kind != "response" {
            return Ok(false);
        }
        let binding = if stale { "!ambiguous" } else { binding };
        let now = crate::time::unix_now();
        data.sessions.retain(|_, (_, expires)| *expires >= now);
        let key = (owner.into(), kind.into(), hash(token));
        if data.sessions.len() as u64 >= max && !data.sessions.contains_key(&key) {
            return Err(VaultError("session storage is full"));
        }
        let mut next = binding.to_owned();
        if let Some((old, _)) = data.sessions.get(&key) {
            if kind == "response" {
                if old == "!retired" {
                    next = old.clone();
                } else if old != binding {
                    next = "!ambiguous".into();
                }
            } else if old != binding {
                return Ok(false);
            }
        }
        data.sessions.insert(key, (next, now.saturating_add(ttl)));
        Ok(true)
    }
}
fn hash(value: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

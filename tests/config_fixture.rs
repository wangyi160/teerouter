//! Test-only translation of local fixture files and secret lookups.
//! Production startup accepts only management provisioning and merchant sync.
use crate::{
    config::Config,
    router::Engine,
    vault::{Provision, StoredAccount, VaultError},
};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use zeroize::Zeroizing;
type Result<T> = std::result::Result<T, VaultError>;

impl Config {
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let config: Self =
            toml::from_str(text).map_err(|e| format!("invalid TOML configuration: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> std::result::Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read configuration: {e}"))?;
        let mut config = Self::parse(&text)?;
        for group in &mut config.models {
            for deployment in &mut group.deployments {
                if let Some(account) = deployment.oauth_file.as_mut().filter(|p| p.is_relative()) {
                    *account = path.parent().unwrap_or(Path::new(".")).join(&*account);
                }
            }
        }
        Ok(config)
    }
}

impl Engine {
    pub(crate) fn from_fixture(
        config: Config,
        env: impl Fn(&str) -> Option<String>,
    ) -> std::result::Result<Arc<Self>, String> {
        let bundle =
            crate::vault::Provision::from_fixture(config, env).map_err(|e| e.to_string())?;
        let vault = crate::vault::Vault::memory().map_err(|e| e.to_string())?;
        vault.provision(&bundle).map_err(|e| e.to_string())?;
        Self::from_vault(vault)
    }
}

impl Provision {
    fn from_fixture(mut config: Config, lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut secrets = BTreeMap::new();
        let mut accounts = BTreeMap::new();
        let mut files = BTreeMap::new();
        let mut import = |name: &str| -> Result<()> {
            let value = lookup(name)
                .filter(|s| !s.trim().is_empty())
                .ok_or(VaultError("missing legacy environment secret"))?;
            secrets.insert(name.to_owned(), value);
            Ok(())
        };
        for key in &config.keys {
            import(&key.key_env)?;
        }
        if let Some(name) = &config.server.admin_key_env {
            import(name)?;
        }
        if let Some(name) = &config.server.router_access_key_ref {
            import(name)?;
        }
        for d in config.models.iter_mut().flat_map(|m| &mut m.deployments) {
            if !d.enabled {
                if d.oauth_file.take().is_some() {
                    d.oauth_account = Some(d.id.clone());
                }
                continue;
            }
            if let Some(name) = d.model.strip_prefix("env:") {
                d.model = lookup(name)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or(VaultError("missing legacy model value"))?;
            }
            if !d.api_key_env.is_empty() {
                import(&d.api_key_env)?;
            }
            // Explicit legacy conversion only. Runtime never falls back to server.proxy.
            if d.proxy.is_none() {
                d.proxy = config.server.proxy.clone();
            }
            if let Some(path) = d.oauth_file.take() {
                let path = std::fs::canonicalize(path)
                    .map_err(|_| VaultError("cannot read legacy account"))?;
                let name = files
                    .entry(path.clone())
                    .or_insert_with(|| d.id.clone())
                    .clone();
                let text = Zeroizing::new(
                    std::fs::read_to_string(path)
                        .map_err(|_| VaultError("cannot read legacy account"))?,
                );
                let credential = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                    .map_err(|_| VaultError("invalid legacy account JSON"))?;
                if let Some(existing) = accounts.get(&name) {
                    let existing: &StoredAccount = existing;
                    if existing.provider != d.provider || existing.proxy != d.proxy {
                        return Err(VaultError(
                            "shared legacy account requires the same provider and proxy",
                        ));
                    }
                }
                accounts.insert(
                    name.clone(),
                    StoredAccount {
                        provider: d.provider,
                        credential,
                        proxy: d.proxy.take(),
                        enabled: true,
                    },
                );
                d.oauth_account = Some(name);
            }
        }
        Ok(Self {
            config,
            secrets,
            accounts,
        })
    }
}

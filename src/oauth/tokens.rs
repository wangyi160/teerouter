//! Access-token-only authentication. Renewal belongs exclusively to the merchant service.
use crate::{Error, Provider};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) struct Credential {
    pub access_token: String,
    pub account_id: Option<String>,
}

pub(crate) struct TokenStore {
    pub provider: Provider,
    vault: Arc<crate::vault::Vault>,
    pub(crate) name: String,
    proxy: String,
    pub(crate) profile: crate::compatibility::Profile,
}

impl TokenStore {
    #[cfg(test)]
    pub fn open(
        provider: Provider,
        vault: Arc<crate::vault::Vault>,
        name: &str,
    ) -> Result<Arc<Self>, Error> {
        let proxy = vault
            .account_proxy(name, provider)
            .map_err(|_| Error::OAuth("account proxy is required; use set-proxy first"))?;
        let mut store = Self::prepared(provider, vault.clone(), name, proxy)?;
        if let Ok(config) = vault.config() {
            let store = Arc::get_mut(&mut store).unwrap();
            store.profile = config
                .compatibility
                .get(crate::compatibility::name(provider))
                .cloned()
                .unwrap_or_default();
        }
        let data = store.read()?;
        if token(&data).is_none() {
            return Err(Error::OAuth("stored account contains no access token"));
        }
        Ok(store)
    }

    pub(crate) fn prepared(
        provider: Provider,
        vault: Arc<crate::vault::Vault>,
        name: &str,
        proxy: String,
    ) -> Result<Arc<Self>, Error> {
        Ok(Arc::new(Self {
            provider,
            vault,
            name: name.to_owned(),
            proxy,
            profile: Default::default(),
        }))
    }

    fn read(&self) -> Result<Value, Error> {
        let (data, _) = self
            .vault
            .account(&self.name, self.provider)
            .map_err(|_| Error::OAuth("cannot read account"))?;
        self.check_proxy()?;
        if self.provider == Provider::Anthropic
            && let Some(oauth) = data.get("claudeAiOauth")
        {
            return Ok(
                json!({"access_token":oauth["accessToken"], "expires_at":oauth["expiresAt"].as_f64().map(|n|n / 1000.0)}),
            );
        }
        Ok(data)
    }

    pub(crate) fn revision(&self) -> Result<i64, Error> {
        let revision = self
            .vault
            .account(&self.name, self.provider)
            .map_err(|_| Error::OAuth("account unavailable"))?
            .1;
        self.check_proxy()?;
        Ok(revision)
    }

    pub(crate) fn request_revision(&self) -> Result<(i64, i64), Error> {
        let data = self
            .vault
            .lock()
            .map_err(|_| Error::OAuth("account unavailable"))?;
        let account = data
            .account_revisions
            .get(&self.name)
            .copied()
            .ok_or(Error::OAuth("account unavailable"))?;
        // The configuration revision prevents delete/recreate from reusing an
        // account-local revision while an old request is still in flight.
        Ok((data.revision, account))
    }

    fn check_proxy(&self) -> Result<(), Error> {
        if let Ok(config) = self.vault.config() {
            let current = config
                .compatibility
                .get(crate::compatibility::name(self.provider))
                .cloned()
                .unwrap_or_default();
            if serde_json::to_value(current).ok() != serde_json::to_value(&self.profile).ok() {
                return Err(Error::OAuth("provider profile changed; old client retired"));
            }
        }
        if !self
            .vault
            .enabled_account(&self.name)
            .map_err(|_| Error::OAuth("cannot read account state"))?
        {
            return Err(Error::OAuth("account is disabled or deleted"));
        }
        let current = self
            .vault
            .account_proxy(&self.name, self.provider)
            .map_err(|_| Error::OAuth("account proxy unavailable"))?;
        if current != self.proxy {
            return Err(Error::OAuth("account proxy changed; old client retired"));
        }
        Ok(())
    }

    pub async fn credential(&self) -> Result<Credential, Error> {
        let revision = self.revision()?;
        let data = self.read()?;
        let health = self
            .vault
            .health(&self.name, revision)
            .map_err(|_| Error::OAuth("cannot read account health"))?;
        if health["state"] == "reauthorization_required" {
            return Err(Error::OAuth("account requires renewed authorization"));
        }
        if access_expiry(&data).is_some_and(|t| t <= now()) {
            let _ = self.vault.record_health(
                &self.name,
                revision,
                "expired",
                0,
                0,
                Some("access_token_expired"),
            );
            return Err(Error::OAuth(
                "access token expired; merchant must upload renewed credentials",
            ));
        }
        credential(&data)
    }
    pub(crate) fn rejected(&self, (config, account): (i64, i64)) {
        // Compare and mark under the same lock as account replacement/deletion.
        if let Ok(mut data) = self.vault.lock()
            && data.revision == config
            && data.account_revisions.get(&self.name) == Some(&account)
        {
            data.health.insert(
                self.name.clone(),
                (
                    account,
                    json!({"state":"reauthorization_required","failures":0,"next_retry_at":0,
                    "last_error":"access_rejected","updated_at":crate::time::unix_now()}),
                ),
            );
        }
    }
    pub(crate) fn available(&self) -> bool {
        let Ok(revision) = self.revision() else {
            return false;
        };
        self.vault.health(&self.name, revision).is_ok_and(|h| {
            h["state"] != "reauthorization_required"
                && h["state"] != "expired"
                && h["next_retry_at"].as_u64().unwrap_or(0) <= now() as u64
        })
    }
}
fn token(data: &Value) -> Option<&str> {
    data["access_token"].as_str().filter(|s| !s.is_empty())
}
pub(crate) fn access_expiry(data: &Value) -> Option<f64> {
    data["expires_at"]
        .as_f64()
        .or_else(|| {
            data["claudeAiOauth"]["expiresAt"]
                .as_f64()
                .map(|t| t / 1000.0)
        })
        .or_else(|| {
            token(data)
                .or_else(|| data["claudeAiOauth"]["accessToken"].as_str())
                .and_then(|t| claims(t)["exp"].as_f64())
        })
}
/// Reconciliation marker for an owner's idempotent access-token uploads.
pub(crate) fn access_fingerprint(data: &Value) -> String {
    use sha2::{Digest, Sha256};
    let access = token(data)
        .or_else(|| data["claudeAiOauth"]["accessToken"].as_str())
        .unwrap_or("");
    format!("{:x}", Sha256::digest(access.as_bytes()))
}
fn credential(data: &Value) -> Result<Credential, Error> {
    let access_token = token(data)
        .ok_or(Error::OAuth("access token missing"))?
        .to_owned();
    let account_id = data["account_id"].as_str().map(str::to_owned).or_else(|| {
        let jwt = data["id_token"].as_str().unwrap_or(&access_token);
        claims(jwt)["https://api.openai.com/auth"]["chatgpt_account_id"]
            .as_str()
            .map(str::to_owned)
    });
    Ok(Credential {
        access_token,
        account_id,
    })
}

pub(crate) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn claims(jwt: &str) -> Value {
    jwt.split('.')
        .nth(1)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
#[path = "token_tests.rs"]
mod tests;

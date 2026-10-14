//! Ports of LiteLLM ChatGPT Authenticator and XAIOAuthAuthenticator token routines.
use super::{CHATGPT_CLIENT_ID, CHATGPT_TOKEN_URL, XAI_CLIENT_ID, XAI_DISCOVERY_URL};
use crate::{Error, Provider};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

pub(crate) struct Credential {
    pub access_token: String,
    pub account_id: Option<String>,
}

pub(crate) struct TokenStore {
    pub provider: Provider,
    vault: Arc<crate::vault::Vault>,
    pub(crate) name: String,
    proxy: String,
    http: reqwest::Client,
    refresh_lock: Arc<Mutex<()>>,
    pub(crate) profile: crate::compatibility::Profile,
    pub(crate) refresh_ahead: u64,
    #[cfg(test)]
    pub test_token_endpoint: Option<String>,
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
            store.refresh_ahead = config.operations.refresh_ahead_secs;
        }
        let data = store.read()?;
        if token(&data).is_none() && data["refresh_token"].as_str().is_none() {
            return Err(Error::OAuth(
                "stored account contains no access or refresh token",
            ));
        }
        Ok(store)
    }

    pub(crate) fn prepared(
        provider: Provider,
        vault: Arc<crate::vault::Vault>,
        name: &str,
        proxy: String,
    ) -> Result<Arc<Self>, Error> {
        let http = crate::network::http_client(provider, &proxy, 30)?;
        Ok(Arc::new(Self {
            provider,
            refresh_lock: vault.account_lock(name),
            vault,
            name: name.to_owned(),
            http,
            proxy,
            profile: Default::default(),
            refresh_ahead: 300,
            #[cfg(test)]
            test_token_endpoint: None,
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

    fn fresh(&self, data: &Value) -> bool {
        if token(data).is_none() {
            return false;
        }
        let expiry = access_expiry(data);
        let skew = match self.provider {
            Provider::XAI => 120.0,
            _ => 60.0,
        };
        // Upstream Claude accepts externally managed opaque OAuth tokens without an expiry claim.
        expiry.is_some_and(|expiry| now() < expiry - skew)
            || (self.provider == Provider::Anthropic && expiry.is_none())
    }

    pub async fn credential(&self) -> Result<Credential, Error> {
        self.resolve(false, false).await
    }
    pub(crate) async fn maintain(&self) -> Result<Credential, Error> {
        self.resolve(false, true).await
    }
    pub(crate) async fn force_refresh(&self) -> Result<Credential, Error> {
        if self.provider == Provider::Anthropic {
            return Err(Error::OAuth("Claude uses external credential renewal"));
        }
        self.resolve(true, false).await
    }
    pub(crate) fn rejected(&self) {
        if let Ok(revision) = self.revision() {
            let renewable = self
                .read()
                .is_ok_and(|data| can_refresh(self.provider, &data));
            let _ = self.vault.record_health(
                &self.name,
                revision,
                if renewable {
                    "refresh_required"
                } else {
                    "reauthorization_required"
                },
                0,
                0,
                Some("access_rejected"),
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
    async fn resolve(&self, force: bool, early: bool) -> Result<Credential, Error> {
        let _guard = self.refresh_lock.lock().await;
        let revision = self.revision()?;
        let health = self
            .vault
            .health(&self.name, revision)
            .map_err(|_| Error::OAuth("cannot read account health"))?;
        if !force && health["state"] == "reauthorization_required" {
            return Err(Error::OAuth("account requires renewed authorization"));
        }
        if health["state"] == "expired" {
            return Err(Error::OAuth(
                "access token expired; import renewed credentials",
            ));
        }
        if !force && health["next_retry_at"].as_u64().unwrap_or(0) > now() as u64 {
            return Err(Error::Api {
                status: 503,
                body: String::new(),
                retry_after_secs: Some(
                    health["next_retry_at"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(now() as u64),
                ),
            });
        }
        let data = self.read()?;
        // Static local API keys share account storage, revocation and proxy checks,
        // but never enter an OAuth refresh flow or acquire a fabricated expiry.
        if self.provider.is_local_test() {
            if force {
                return Err(Error::OAuth("API keys use external replacement"));
            }
            return Ok(Credential {
                access_token: data["api_key"]
                    .as_str()
                    .filter(|key| !key.is_empty())
                    .ok_or(Error::OAuth("local API key missing"))?
                    .to_owned(),
                account_id: None,
            });
        }
        let expiry = access_expiry(&data);
        if !can_refresh(self.provider, &data) {
            // Externally renewed credentials remain usable until their actual
            // expiry. Neither proactive maintenance nor a manual refresh may
            // make an OAuth request without a refresh token.
            if expiry.is_some_and(|t| t <= now()) {
                let _ = self.vault.record_health(
                    &self.name,
                    revision,
                    "expired",
                    0,
                    0,
                    Some("access_token_expired"),
                );
                return Err(Error::OAuth(
                    "access token expired; import renewed credentials",
                ));
            }
            if force {
                return Err(Error::OAuth("account uses external credential renewal"));
            }
            if health["state"] == "refresh_required" {
                let _ = self.vault.record_health(
                    &self.name,
                    revision,
                    "reauthorization_required",
                    0,
                    0,
                    Some("access_rejected"),
                );
                return Err(Error::OAuth(
                    "access token rejected; import renewed credentials",
                ));
            }
            return credential(&data);
        }
        let early = early
            && self.provider != Provider::Anthropic
            && expiry.is_some_and(|t| t <= now() + self.refresh_ahead as f64);
        if !force && !early && health["state"] != "refresh_required" && self.fresh(&data) {
            return credential(&data);
        }
        let result = self.refresh_credential().await;
        match &result {
            Ok(_) => {
                let rev = self.revision()?;
                self.vault
                    .record_health(&self.name, rev, "active", 0, 0, None)
                    .map_err(|_| Error::OAuth("cannot save account health"))?;
            }
            Err(error) => {
                let permanent = matches!(
                    error,
                    Error::Api {
                        status: 400 | 401 | 403,
                        ..
                    }
                ) || matches!(
                    error,
                    Error::OAuth(
                        "Claude token expired; renew it with Claude Code"
                            | "refresh token missing; log in again"
                    )
                );
                let failures = health["failures"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(1)
                    .min(20);
                let retry = if permanent {
                    0
                } else {
                    now() as u64 + (5u64 << failures.min(8)).min(900)
                };
                let _ = self.vault.record_health(
                    &self.name,
                    revision,
                    if permanent {
                        "reauthorization_required"
                    } else {
                        "backoff"
                    },
                    failures,
                    retry,
                    Some(if permanent {
                        "reauthorization_required"
                    } else {
                        "refresh_unavailable"
                    }),
                );
            }
        }
        result
    }
    async fn refresh_credential(&self) -> Result<Credential, Error> {
        if self.provider == Provider::Anthropic {
            return Err(Error::OAuth(
                "Claude token expired; renew it with Claude Code",
            ));
        }
        // Same double-check as XAIOAuthAuthenticator.get_access_token; shared per vault account.
        let (data, revision) = self
            .vault
            .account(&self.name, self.provider)
            .map_err(|_| Error::OAuth("cannot read account"))?;
        // Check after reading the revision, so a concurrent rebind either fails here
        // or invalidates the refresh CAS. Never pair a new revision with an old proxy.
        self.check_proxy()?;
        let refresh = data["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(Error::OAuth(
                "refresh token missing; import renewed credentials",
            ))?;
        let endpoint = self.refresh_endpoint().await?;
        let original_endpoint = endpoint.clone();
        let configured_id = self
            .profile
            .requests
            .get("oauth_refresh")
            .and_then(|p| {
                p.body_set
                    .get("/client_id")
                    .or_else(|| p.body_defaults.get("/client_id"))
            })
            .and_then(Value::as_str)
            .or(self.profile.client_id.as_deref())
            .filter(|value| !value.is_empty());
        let client_id = configured_id
            .or(match self.provider {
                Provider::OpenAI => Some(CHATGPT_CLIENT_ID),
                Provider::XAI => Some(XAI_CLIENT_ID),
                _ => None,
            })
            .ok_or(Error::OAuth(
                "OAuth client_id missing; configure oauth_refresh client_id",
            ))?;
        let response = match self.provider {
            Provider::OpenAI => self.profile.json("oauth_refresh", &self.http, endpoint, reqwest::header::HeaderMap::new(), json!({"client_id":client_id,
                "grant_type":"refresh_token","refresh_token":refresh,"scope":self.profile.scope.as_deref().unwrap_or("openid profile email")}))?.send().await?,
            Provider::XAI => self.profile.form("oauth_refresh", &self.http, endpoint, &[("grant_type","refresh_token"),("refresh_token",refresh),("client_id",client_id)])?.send().await?,
            Provider::Anthropic => unreachable!(),
            #[cfg(feature = "workbuddy-lab")]
            Provider::Workbuddy => return Err(Error::OAuth("API keys use external replacement")),
        };
        let mut payload = token_response(response).await?;
        if self.provider == Provider::XAI {
            payload["token_endpoint"] = original_endpoint.into();
        }
        let record = build_record(self.provider, &payload, Some(&data))?;
        if !self
            .vault
            .refresh_account(&self.name, revision, &record)
            .map_err(|_| Error::OAuth("cannot persist refreshed credential"))?
        {
            // An explicit import won the race; never overwrite it with an old refresh.
            let current = self.read()?;
            if self.fresh(&current) {
                return credential(&current);
            }
            return Err(Error::OAuth(
                "account changed during refresh; retry request",
            ));
        }
        self.check_proxy()?;
        credential(&record)
    }

    async fn refresh_endpoint(&self) -> Result<String, Error> {
        // The test transport override is absent from both production binaries.
        #[cfg(test)]
        if let Some(url) = &self.test_token_endpoint {
            return Ok(url.clone());
        }
        match self.provider {
            Provider::OpenAI => Ok(CHATGPT_TOKEN_URL.into()),
            // Read the official discovery document; imported credential metadata
            // is not trusted to select the refresh endpoint.
            Provider::XAI => discover_xai_at(&self.http, XAI_DISCOVERY_URL, &self.profile).await,
            Provider::Anthropic => Err(Error::OAuth("Claude uses external credential renewal")),
            #[cfg(feature = "workbuddy-lab")]
            Provider::Workbuddy => Err(Error::OAuth("API keys use external replacement")),
        }
    }
}

pub(crate) fn token(data: &Value) -> Option<&str> {
    data["access_token"].as_str().filter(|s| !s.is_empty())
}
pub(crate) fn can_refresh(provider: Provider, data: &Value) -> bool {
    !provider.is_local_test()
        && provider != Provider::Anthropic
        && data["refresh_token"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
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

pub(crate) async fn token_response(response: reqwest::Response) -> Result<Value, Error> {
    if !response.status().is_success() {
        return Err(Error::Api {
            status: response.status().as_u16(),
            body: String::new(),
            retry_after_secs: None,
        });
    }
    let value = crate::resources::json(response, 1024 * 1024)
        .await
        .map_err(|_| Error::OAuth("token endpoint returned invalid JSON"))?;
    if !value.is_object() || value.get("error").is_some_and(|v| !v.is_null()) {
        return Err(Error::OAuth("invalid token response"));
    }
    Ok(value)
}

pub(crate) fn build_record(
    provider: Provider,
    payload: &Value,
    previous: Option<&Value>,
) -> Result<Value, Error> {
    let access = token(payload).ok_or(Error::OAuth("token response missing access_token"))?;
    let refresh = payload["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| previous.and_then(|p| p["refresh_token"].as_str()))
        .ok_or(Error::OAuth("token response missing refresh_token"))?;
    let mut record = json!({"access_token":access,"refresh_token":refresh});
    match provider {
        Provider::OpenAI => {
            let id = payload["id_token"]
                .as_str()
                .ok_or(Error::OAuth("ChatGPT token response missing id_token"))?;
            record["id_token"] = id.into();
            record["expires_at"] = claims(access)["exp"].clone();
            record["account_id"] =
                claims(id)["https://api.openai.com/auth"]["chatgpt_account_id"].clone();
        }
        Provider::XAI => {
            let endpoint = payload["token_endpoint"]
                .as_str()
                .or_else(|| previous.and_then(|p| p["token_endpoint"].as_str()))
                .ok_or(Error::OAuth("xAI token endpoint missing"))?;
            validate_xai_endpoint(endpoint)?;
            record["token_endpoint"] = endpoint.into();
            record["id_token"] = payload["id_token"].clone();
            record["token_type"] = payload["token_type"].as_str().unwrap_or("Bearer").into();
            record["expires_at"] = json!(
                now()
                    + payload["expires_in"]
                        .as_f64()
                        .filter(|n| *n > 0.0)
                        .unwrap_or(3600.0)
            );
        }
        Provider::Anthropic => {
            return Err(Error::OAuth(
                "Claude login and refresh are managed by Claude Code",
            ));
        }
        #[cfg(feature = "workbuddy-lab")]
        Provider::Workbuddy => return Err(Error::OAuth("API keys use external replacement")),
    }
    Ok(record)
}

pub(crate) fn validate_xai_endpoint(url: &str) -> Result<&str, Error> {
    crate::official::validate(url, Some(Provider::XAI))?;
    Ok(url)
}

pub(crate) async fn discover_xai_at(
    http: &reqwest::Client,
    url: &str,
    profile: &crate::compatibility::Profile,
) -> Result<String, Error> {
    let response = profile
        .get(
            "oauth_discovery",
            http,
            url,
            reqwest::header::HeaderMap::new(),
        )?
        .send()
        .await?;
    let data = token_response(response).await?;
    let token = data["token_endpoint"]
        .as_str()
        .ok_or(Error::OAuth("xAI discovery missing token endpoint"))?;
    Ok(validate_xai_endpoint(token)?.into())
}

#[cfg(test)]
#[path = "token_tests.rs"]
mod tests;

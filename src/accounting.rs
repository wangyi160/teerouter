use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::resources::{Gate, Permit};

use crate::{
    config::ApiKey,
    error::ApiError,
    ledger::{Ledger, Usage},
    scheduler::Backend,
    upstream::Tokens,
};

pub(crate) struct Account {
    pub config: ApiKey,
    pub secret: String,
    slots: Arc<Gate>,
    meter: Arc<Mutex<Meter>>,
    vault: Arc<crate::vault::Vault>,
    user_slots: Arc<Gate>,
}

struct Meter {
    window: Instant,
    requests: u64,
    tokens: u64,
}

impl Meter {
    fn refresh(&mut self) {
        if self.window.elapsed() >= Duration::from_secs(60) {
            self.window = Instant::now();
            self.requests = 0;
            self.tokens = 0;
        }
    }
}

impl Account {
    pub(crate) fn inherit(
        config: ApiKey,
        secret: String,
        vault: Arc<crate::vault::Vault>,
        old: Option<&Arc<Self>>,
    ) -> Arc<Self> {
        let user_slots = vault.user_gate(&config.user_id);
        Arc::new(Self {
            slots: old.map(|a| a.slots.clone()).unwrap_or_default(),
            meter: old.map(|a| a.meter.clone()).unwrap_or_else(|| {
                Arc::new(Mutex::new(Meter {
                    window: Instant::now(),
                    requests: 0,
                    tokens: 0,
                }))
            }),
            config,
            secret,
            vault,
            user_slots,
        })
    }
    // Consult the current committed state, including requests whose body was
    // still arriving during a permission change. Admission and commit serialize
    // on this mutex; the shared gate survives key rotation and user recreation.
    fn with_user<T>(
        &self,
        action: impl FnOnce(&crate::config::User) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        use subtle::ConstantTimeEq;
        let invalid = || {
            ApiError::new(
                401,
                "authentication_error",
                "User or API key is disabled, expired or revoked",
            )
        };
        let data = self.vault.lock().map_err(crate::runtime::storage)?;
        let bundle = data.bundle.as_ref().ok_or_else(invalid)?;
        let key = bundle
            .config
            .keys
            .iter()
            .find(|k| k.name == self.config.name)
            .ok_or_else(invalid)?;
        if !key.enabled
            || key.user_id != self.config.user_id
            || (key.expires_at != 0 && key.expires_at <= crate::anthropic::unix_now())
            || !bundle
                .secrets
                .get(&key.key_env)
                .is_some_and(|s| bool::from(s.as_bytes().ct_eq(self.secret.as_bytes())))
        {
            return Err(invalid());
        }
        let user = bundle
            .config
            .users
            .iter()
            .find(|u| u.id == key.user_id && u.enabled)
            .ok_or_else(invalid)?;
        action(user)
    }
    pub(crate) fn authorize(&self) -> Result<(), ApiError> {
        self.with_user(|_| Ok(()))
    }
    pub fn admit(
        self: &Arc<Self>,
        ledger: Arc<Ledger>,
        global: Arc<Gate>,
        global_limit: usize,
        request_id: String,
        model: String,
    ) -> Result<Ticket, ApiError> {
        if !ledger.healthy() {
            return Err(ApiError::new(
                503,
                "accounting_unavailable",
                "Usage synchronization is unavailable",
            ));
        }
        let _global = global.acquire(global_limit.min(4096)).ok_or_else(|| {
            ApiError::new(503, "router_busy", "Router concurrency limit exceeded")
        })?;
        let _user = self.with_user(|u| {
            self.user_slots.acquire(u.max_concurrent).ok_or_else(|| {
                ApiError::new(
                    429,
                    "user_concurrency_limit",
                    "User concurrency limit exceeded",
                )
            })
        })?;
        let _key = self
            .slots
            .clone()
            .acquire(self.config.max_concurrent)
            .ok_or_else(|| ApiError::limited("API key concurrency limit exceeded"))?;
        let mut meter = self.meter.lock().unwrap();
        meter.refresh();
        if self.config.rpm > 0 && meter.requests >= self.config.rpm {
            return Err(ApiError::limited("API key request limit exceeded"));
        }
        if self.config.tpm > 0 && meter.tokens >= self.config.tpm {
            return Err(ApiError::limited("API key observed token limit exceeded"));
        }
        meter.requests += 1;
        Ok(Ticket {
            account: self.clone(),
            ledger,
            _global,
            _key,
            _user,
            start: Instant::now(),
            request_id,
            model,
            pool: None,
            provider: None,
            created_at: crate::anthropic::unix_now(),
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            tokens: Tokens::default(),
            backend: None,
            attempts: 0,
            moderation: None,
            settled: false,
        })
    }
}

pub(crate) struct Ticket {
    account: Arc<Account>,
    ledger: Arc<Ledger>,
    _global: Permit,
    _key: Permit,
    _user: Permit,
    start: Instant,
    pub request_id: String,
    model: String,
    pool: Option<String>,
    provider: Option<crate::Provider>,
    created_at: u64,
    started_at_ms: u128,
    pub tokens: Tokens,
    pub backend: Option<Arc<Backend>>,
    pub attempts: usize,
    pub moderation: Option<crate::moderation::Report>,
    settled: bool,
}

impl Ticket {
    pub(crate) fn begin_request(
        &mut self,
        pool: &str,
        provider: crate::Provider,
    ) -> Result<(), ApiError> {
        self.pool = Some(pool.into());
        self.provider = Some(provider);
        Ok(())
    }
    pub fn finish(&mut self, status: u16) {
        if self.settled {
            return;
        }
        self.settled = true;
        let usage = Usage {
            requests: 1,
            errors: u64::from(status >= 400),
            input_tokens: self.tokens.input,
            output_tokens: self.tokens.output,
            unknown_usage_requests: u64::from(
                !(self.attempts == 0 && self.moderation.is_some())
                    && (!self.tokens.known || status >= 400 || self.attempts > 1),
            ),
        };
        let mut meter = self.account.meter.lock().unwrap();
        meter.refresh();
        meter.tokens = meter
            .tokens
            .saturating_add(self.tokens.input)
            .saturating_add(self.tokens.output);
        self.ledger.record(&self.account.config.name, &usage,
            &serde_json::json!({"event":"request", "request_id":self.request_id,
            "key":self.account.config.name, "user_id":self.account.config.user_id, "model":self.model, "pool":self.pool,"provider":self.provider,"created_at":self.created_at,"started_at_ms": self.started_at_ms,
            "deployment":self.backend.as_ref().map(|b| &b.config.id), "status":status,
            "attempts":self.attempts, "duration_ms":self.start.elapsed().as_millis(), "usage":usage,
            "cache_usage":{"read":self.tokens.cache_read,"write":self.tokens.cache_write,"write_hour":self.tokens.cache_write_hour},
            "moderation":self.moderation}), self.moderation.as_ref()
        );
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.finish(499);
    }
}

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
        check_balance: bool,
        action: impl FnOnce(&crate::config::User, usize) -> Result<T, ApiError>,
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
        let limit = if check_balance && self.vault.managed() {
            crate::balances::concurrency_limit(&data, user, &bundle.config.balance_concurrency)?
        } else {
            user.max_concurrent
        };
        action(user, limit)
    }
    pub(crate) fn authorize(&self) -> Result<(), ApiError> {
        self.with_user(false, |_, _| Ok(()))
    }
    pub(crate) fn has_capacity(&self) -> bool {
        if !self
            .with_user(true, |_, limit| Ok(self.user_slots.used() < limit))
            .unwrap_or(false)
            || self.slots.used() >= self.config.max_concurrent
        {
            return false;
        }
        let mut meter = self.meter.lock().unwrap();
        meter.refresh();
        (self.config.rpm == 0 || meter.requests < self.config.rpm)
            && (self.config.tpm == 0 || meter.tokens < self.config.tpm)
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
                "Usage or refusal synchronization is unavailable",
            ));
        }
        let _global = global.acquire(global_limit.min(4096)).ok_or_else(|| {
            ApiError::new(503, "router_busy", "Router concurrency limit exceeded")
        })?;
        let _user = self.with_user(true, |u, limit| {
            self.user_slots.acquire(limit).ok_or_else(|| {
                if limit < u.max_concurrent {
                    ApiError::new(
                        429,
                        "balance_concurrency_limit",
                        "Concurrency is limited by remaining user balance",
                    )
                } else {
                    ApiError::new(
                        429,
                        "user_concurrency_limit",
                        "User concurrency limit exceeded",
                    )
                }
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
            billing: None,
            billing_records: Vec::new(),
            billing_active: false,
            backend: None,
            attempts: 0,
            refusal: None,
            settled: false,
            parent: None,
            context: serde_json::Value::Null,
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
    pub(crate) billing: Option<crate::billing::Observer>,
    billing_records: Vec<serde_json::Value>,
    billing_active: bool,
    pub backend: Option<Arc<Backend>>,
    pub attempts: usize,
    pub refusal: Option<crate::refusal::Observer>,
    settled: bool,
    pub(crate) parent: Option<Box<Ticket>>,
    pub(crate) context: serde_json::Value,
}

impl Ticket {
    pub(crate) fn begin_billing_attempt(&mut self) {
        if let Some(billing) = &self.billing {
            self.billing = Some(billing.fresh());
            self.billing_active = true;
        }
    }
    pub(crate) fn finish_billing_attempt(&mut self, status: u16) {
        if self.billing_active {
            if let Some(billing) = &self.billing {
                self.billing_records
                    .push(serde_json::json!({"number":self.attempts,
                    "status":status,"billing":billing.finish(status, 1)}));
            }
            self.billing_active = false;
        }
    }
    pub(crate) fn activity(&self) -> Option<crate::ledger::Activity> {
        self.parent
            .as_ref()
            .map(|_| self.ledger.activity(&self.account.config.user_id))
    }
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
        self.finish_billing_attempt(status);
        let billing = self
            .billing
            .as_ref()
            .map(|_| crate::billing::aggregate(&self.billing_records, self.attempts));
        if let Some(record) = &billing {
            if self.attempts > 0 {
                self.tokens = crate::billing::tokens(record);
            }
        }
        let usage = Usage {
            requests: 1,
            errors: u64::from(status >= 400),
            input_tokens: self.tokens.input,
            output_tokens: self.tokens.output,
            unknown_usage_requests: u64::from(
                !self.tokens.known || (billing.is_none() && (status >= 400 || self.attempts > 1)),
            ),
        };
        let mut meter = self.account.meter.lock().unwrap();
        meter.refresh();
        meter.tokens = meter
            .tokens
            .saturating_add(self.tokens.input)
            .saturating_add(self.tokens.output);
        let refusal = self
            .refusal
            .as_ref()
            .and_then(crate::refusal::Observer::report);
        let mut audit = serde_json::json!({"event":"request", "request_id":self.request_id,
        "key":self.account.config.name, "user_id":self.account.config.user_id, "model":self.model, "pool":self.pool,"provider":self.provider,"created_at":self.created_at,"started_at_ms": self.started_at_ms,
        "deployment":self.backend.as_ref().map(|b| &b.config.id), "status":status,
        "attempts":self.attempts, "duration_ms":self.start.elapsed().as_millis(), "usage":usage,
        "cache_usage":{"read":self.tokens.cache_read,"write":self.tokens.cache_write,"write_hour":self.tokens.cache_write_hour},
        });
        if let Some(extra) = self.context.as_object() {
            audit.as_object_mut().unwrap().extend(extra.clone());
        }
        if let Some(billing) = billing {
            audit["billing"] = billing;
        }
        // Platform keeps all admission outcomes. A merchant must not learn an
        // unused user's identity from an error before any upstream contact.
        if self.parent.is_none() || self.ledger.has_upstream_user(&self.account.config.user_id) {
            self.ledger
                .record(&self.account.config.name, &usage, &audit, refusal.as_ref());
        }
        if let Some(parent) = self.parent.as_mut() {
            parent.settled = true;
            let mut meter = parent.account.meter.lock().unwrap();
            meter.refresh();
            meter.tokens = meter
                .tokens
                .saturating_add(self.tokens.input)
                .saturating_add(self.tokens.output);
            audit["key"] = parent.account.config.name.clone().into();
            audit["user_id"] = parent.account.config.user_id.clone().into();
            if let Some(extra) = parent.context.as_object() {
                audit.as_object_mut().unwrap().extend(extra.clone());
            }
            parent.ledger.record(
                &parent.account.config.name,
                &usage,
                &audit,
                refusal.as_ref(),
            );
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.finish(499);
    }
}

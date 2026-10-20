//! Validated, atomic configuration generations. Existing requests retain their engine.
use crate::{error::ApiError, resources::Gate, router::Engine, vault::operations::Update};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

// Shared by the platform and every merchant, including newly unlocked ones.
// This is an explicit, process-local shutdown barrier, never a manager lease.
#[derive(Default)]
struct Lifecycle {
    quiescing: AtomicBool,
    background: Arc<Gate>,
}

pub(crate) struct Runtime {
    merchant: bool,
    pub merchants: crate::merchants::Registry,
    current: RwLock<Arc<Engine>>,
    pub ingress: Arc<Gate>,
    pub pricing_sync: Arc<tokio::sync::Mutex<()>>,
    write: tokio::sync::Mutex<()>,
    maintenance: RwLock<serde_json::Value>,
    lifecycle: Arc<Lifecycle>,
}
impl Runtime {
    pub fn new(engine: Arc<Engine>) -> Arc<Self> {
        Self::scoped(engine, false, Arc::default())
    }
    #[cfg(test)]
    pub(super) fn merchant(engine: Arc<Engine>) -> Arc<Self> {
        Self::scoped(engine, true, Arc::default())
    }
    pub(super) fn merchant_child(&self, engine: Arc<Engine>) -> Arc<Self> {
        Self::scoped(engine, true, self.lifecycle.clone())
    }
    pub fn is_platform(&self) -> bool {
        !self.merchant
    }
    fn scoped(engine: Arc<Engine>, merchant: bool, lifecycle: Arc<Lifecycle>) -> Arc<Self> {
        let runtime = Arc::new(Self {
            merchant,
            merchants: Default::default(),
            current: RwLock::new(engine),
            ingress: Arc::default(),
            pricing_sync: Arc::new(tokio::sync::Mutex::new(())),
            write: tokio::sync::Mutex::new(()),
            maintenance: RwLock::new(serde_json::json!({"last_run":null,"last_error":null})),
            lifecycle,
        });
        let weak = Arc::downgrade(&runtime);
        tokio::spawn(async move {
            loop {
                let Some(runtime) = weak.upgrade() else {
                    break;
                };
                let engine = runtime.engine();
                let interval = engine
                    .config
                    .operations
                    .maintenance_secs
                    .min(engine.config.discovery.refresh_secs);
                // Refresh jobs hold only this generation for a bounded iteration.
                if let Some(_job) = runtime.background_job() {
                    if runtime.merchant {
                        engine.refresh_models(false).await;
                        if engine.vault.serving() {
                            engine.maintain_accounts().await;
                        }
                    }
                    let vault = engine.vault.clone();
                    let ops = engine.config.operations.clone();
                    let result = tokio::task::spawn_blocking(move || vault.maintain(&ops)).await;
                    *runtime.maintenance.write().unwrap() = serde_json::json!({"last_run":crate::oauth::tokens::now() as u64,
                    "last_error":if matches!(result,Ok(Ok(()))) {None} else {Some("maintenance_failed")}});
                }
                drop(engine);
                drop(runtime);
                tokio::time::sleep(std::time::Duration::from_secs(interval.min(30))).await;
            }
        });
        crate::pricing::source::start(&runtime);
        crate::network::latency::start(&runtime);
        runtime
    }
    pub fn engine(&self) -> Arc<Engine> {
        self.current.read().unwrap().clone()
    }
    pub fn quiescing(&self) -> bool {
        self.lifecycle.quiescing.load(Ordering::SeqCst)
    }
    pub fn set_quiescing(&self, value: bool) {
        self.lifecycle.quiescing.store(value, Ordering::SeqCst);
    }
    pub fn background_job(&self) -> Option<crate::resources::Permit> {
        let permit = self.lifecycle.background.acquire(usize::MAX)?;
        // Acquire before checking the flag so a concurrent drain sees this job.
        (!self.quiescing()).then_some(permit)
    }
    pub fn lifecycle_status(&self) -> serde_json::Value {
        let quiescing = self.quiescing();
        let background = self.lifecycle.background.used();
        serde_json::json!({"quiesce_version":1,"quiescing":quiescing,
            "background_jobs":background,
            "drained":quiescing && background == 0 && self.ingress.used() <= 1})
    }
    pub async fn initialize(
        &self,
        body: serde_json::Value,
        secret: &str,
    ) -> Result<serde_json::Value, ApiError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Initialization {
            boot_id: String,
            state: Option<crate::vault::Data>,
        }
        let input: Initialization = serde_json::from_value(body).map_err(|_| invalid())?;
        let _lock = self.write.lock().await;
        let old = self.engine();
        if !old.authenticate_admin(secret) {
            return Err(ApiError::new(
                401,
                "authentication_error",
                "Invalid admin credential",
            ));
        }
        if old.vault.initialized() || input.boot_id != old.vault.boot_id {
            return Err(ApiError::new(
                409,
                "initialization_conflict",
                "Reload router boot identity",
            ));
        }
        let mut data = match input.state {
            Some(data) => data,
            None => old.vault.lock().map_err(storage)?.clone(),
        };
        crate::refusal::restore(&mut data).map_err(|_| invalid())?;
        crate::pricing::validate(&data.official_pricing)?;
        crate::pricing::source::validate(&data.pricing_source)?;
        if data
            .merchants
            .iter()
            .any(|(id, item)| id != &item.id || !item.validate())
        {
            return Err(invalid());
        }
        let bundle = data.bundle.as_mut().ok_or_else(invalid)?;
        bundle.config.server.bind = old.config.server.bind;
        self.validate_scope(bundle)?;
        let admin = bundle
            .config
            .server
            .admin_key_env
            .as_ref()
            .and_then(|r| bundle.secrets.get(r));
        if admin.is_none_or(|s| s != secret)
            || data.instance.len() != 32
            || !data.instance.bytes().all(|c| c.is_ascii_hexdigit())
            || data.revision < 1
            || data.generation > u64::MAX / 2
            || data.sequence > u64::MAX / 2
            || data.events.len() > 8192
            || data.moderation.values().any(|m| m.recent_events.len() > 20)
            || data.account_revisions.len() != bundle.accounts.len()
            || bundle
                .accounts
                .keys()
                .any(|k| data.account_revisions.get(k).is_none_or(|r| *r < 1))
        {
            return Err(invalid());
        }
        // Only validated business configuration is restored. Catalog evidence,
        // session bindings and process health are never accepted from management.
        let preview = crate::vault::Vault::memory().map_err(storage)?;
        preview.provision(bundle).map_err(|_| invalid())?;
        let next = Engine::from_snapshot(old.vault.clone(), bundle, None, data.revision)
            .map_err(|_| invalid())?;
        old.vault.initialize(data).map_err(storage)?;
        *self.current.write().unwrap() = next;
        Ok(serde_json::json!({"initialized":true,"boot_id":old.vault.boot_id}))
    }
    pub fn maintenance_status(&self) -> serde_json::Value {
        self.maintenance.read().unwrap().clone()
    }
    pub async fn delete_refusals(
        &self,
        through_id: u64,
        secret: &str,
    ) -> Result<serde_json::Value, ApiError> {
        let _lock = self.write.lock().await;
        let engine = self.engine();
        if !engine.authenticate_admin(secret) {
            return Err(ApiError::new(
                401,
                "authentication_error",
                "Admin credential was revoked",
            ));
        }
        if !engine.vault.initialized() {
            return Err(ApiError::new(
                409,
                "not_initialized",
                "Initialize router first",
            ));
        }
        let mut data = engine.vault.lock().map_err(storage)?;
        let before = data.refusal_queue.deleted_through;
        let deleted = data
            .refusal_queue
            .delete_through(through_id)
            .ok_or_else(|| {
                ApiError::new(
                    409,
                    "invalid_refusal_cursor",
                    "Deletion ID exceeds the last issued refusal ID",
                )
            })?;
        if before != data.refusal_queue.deleted_through {
            data.changed();
        }
        Ok(
            serde_json::json!({"deleted":deleted,"deleted_through":data.refusal_queue.deleted_through,
            "last_id":data.refusal_queue.last_id,"pending":data.refusal_queue.events.len(),"instance_id":data.instance}),
        )
    }
    pub async fn apply(&self, update: Update, secret: &str) -> Result<serde_json::Value, ApiError> {
        let _lock = self.write.lock().await;
        let old = self.engine();
        if !old.vault.initialized() {
            return Err(ApiError::new(
                409,
                "not_initialized",
                "Initialize router first",
            ));
        }
        if !old.authenticate_admin(secret) {
            return Err(ApiError::new(
                401,
                "authentication_error",
                "Admin credential was revoked",
            ));
        }
        for _ in 0..3 {
            let snapshot = old.vault.snapshot().map_err(storage)?;
            if update
                .expected_revision
                .is_some_and(|r| r != snapshot.revision)
            {
                return Err(ApiError::new(
                    409,
                    "revision_conflict",
                    "Configuration changed; reload its revision",
                ));
            }
            let candidate = snapshot.updated(&update).map_err(|_| invalid())?;
            self.validate_scope(&candidate)?;
            if candidate.config.server.bind != old.config.server.bind {
                return Err(ApiError::new(
                    409,
                    "listener_change_requires_restart",
                    "Listener address requires restart",
                ));
            }
            if candidate.config.server.admin_key_env.is_none() {
                return Err(ApiError::new(
                    400,
                    "admin_key_required",
                    "Online changes must retain an admin key",
                ));
            }
            let preview = crate::vault::Vault::memory().map_err(storage)?;
            preview.provision(&candidate).map_err(|_| invalid())?;
            let next = Engine::from_snapshot(
                old.vault.clone(),
                &candidate,
                Some(&old),
                snapshot.revision + 1,
            )
            .map_err(|_| invalid())?;
            if old
                .vault
                .commit_update(&snapshot, &candidate, &update)
                .map_err(storage)?
            {
                next.recover_changed_accounts(&old, &update);
                *self.current.write().unwrap() = next;
                return Ok(serde_json::json!({"revision":snapshot.revision+1,"applied":true}));
            }
        }
        Err(ApiError::new(
            409,
            "account_changed",
            "Account refresh raced with update; retry with current revision",
        ))
    }

    fn validate_scope(&self, bundle: &crate::vault::Provision) -> Result<(), ApiError> {
        if self.merchant {
            return Ok(());
        }
        let allowed: std::collections::BTreeSet<_> = bundle
            .config
            .keys
            .iter()
            .map(|key| &key.key_env)
            .chain(bundle.config.server.admin_key_env.iter())
            .collect();
        if !bundle.config.models.is_empty()
            || !bundle.accounts.is_empty()
            || bundle.config.server.proxy.is_some()
            || bundle.config.server.no_proxy.is_some()
            || bundle.secrets.keys().any(|name| !allowed.contains(name))
        {
            return Err(ApiError::new(
                400,
                "merchant_ownership_required",
                "Upstream pools and credentials must belong to a merchant",
            ));
        }
        Ok(())
    }
}
fn invalid() -> ApiError {
    ApiError::new(
        400,
        "invalid_update",
        "Update failed validation; active configuration was preserved",
    )
}
pub(crate) fn storage(_: crate::vault::VaultError) -> ApiError {
    ApiError::new(503, "storage_unavailable", "Runtime state operation failed")
}

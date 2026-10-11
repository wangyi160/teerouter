//! Account-scoped official model/capability evidence; only fresh caches admit requests.
pub(crate) mod capabilities;
mod fetch;

use crate::{
    Client, Error,
    config::{Deployment, Discovery, valid_model_id},
    vault::Vault,
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::Mutex, time::Instant};

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ModelInfo {
    pub canonical_id: String,
    pub capabilities: capabilities::Capabilities,
}

#[derive(Clone, Deserialize, Serialize)]
struct Snapshot {
    models: BTreeMap<String, ModelInfo>,
    fetched_at: u64,
    revision: Option<i64>,
}

struct Refresh {
    attempted: Option<Instant>,
    revision: Option<Option<i64>>,
    error: Option<&'static str>,
}

pub(crate) struct Catalog {
    client: Client,
    options: Discovery,
    vault: Arc<Vault>,
    source: String,
    snapshot: RwLock<Option<Snapshot>>,
    refresh: Mutex<Refresh>,
}

impl Catalog {
    pub fn new(
        client: Client,
        deployment: &Deployment,
        options: Discovery,
        vault: Arc<Vault>,
        binding: &str,
    ) -> Result<Arc<Self>, String> {
        // Bind restart caches to the provider, credential reference, proxy and endpoint.
        // Hash input can contain secrets, but only the digest is persisted as a cache key.
        let encoded = zeroize::Zeroizing::new(
            serde_json::to_vec(&(
                "official-capabilities-v2",
                deployment,
                &options,
                binding,
                &client.profile,
            ))
            .map_err(|_| "invalid discovery configuration")?,
        );
        let source = format!("{:x}", Sha256::digest(&*encoded));
        let snapshot: Option<Snapshot> = vault
            .catalog(&source)
            .map_err(|e| e.to_string())?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| "invalid model cache")?;
        if snapshot.as_ref().is_some_and(|s| {
            s.models.len() > 10000
                || s.models.iter().any(|(id, info)| {
                    !valid_model_id(id)
                        || !valid_model_id(&info.canonical_id)
                        || !s.models.contains_key(&info.canonical_id)
                })
        }) {
            return Err("invalid model cache".into());
        }
        Ok(Arc::new(Self {
            client,
            options,
            vault,
            source,
            snapshot: RwLock::new(snapshot),
            refresh: Mutex::new(Refresh {
                attempted: None,
                revision: None,
                error: None,
            }),
        }))
    }

    fn current_revision(&self) -> Result<Option<i64>, Error> {
        self.client.oauth.as_ref().map(|a| a.revision()).transpose()
    }

    pub fn models(&self) -> BTreeSet<String> {
        let Ok(revision) = self.current_revision() else {
            return BTreeSet::new();
        };
        let time = now();
        self.snapshot
            .read()
            .unwrap()
            .as_ref()
            .filter(|s| {
                s.revision == revision
                    && s.fetched_at <= time
                    && time - s.fetched_at
                        < self.options.refresh_secs.min(self.options.max_stale_secs)
            })
            .map(|s| s.models.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn evidence(&self, model: &str) -> Result<ModelInfo, crate::error::ApiError> {
        let unavailable = || {
            crate::error::ApiError::new(
                503,
                "catalog_unavailable",
                "A current authenticated official model catalog is required",
            )
        };
        let revision = self.current_revision().map_err(|_| unavailable())?;
        let time = now();
        let guard = self.snapshot.read().unwrap();
        let snapshot = guard
            .as_ref()
            .filter(|s| {
                s.revision == revision
                    && s.fetched_at <= time
                    && time - s.fetched_at
                        < self.options.refresh_secs.min(self.options.max_stale_secs)
            })
            .ok_or_else(unavailable)?;
        snapshot.models.get(model).cloned().ok_or_else(|| {
            crate::error::ApiError::new(
                404,
                "model_not_found",
                "Model is not in the selected account's official catalog",
            )
        })
    }

    pub async fn refresh(&self, force: bool) {
        // Concurrent startup, listing and background refresh requests share one fetch.
        let requested = Instant::now();
        if !force && self.refresh.try_lock().is_err() && !self.models().is_empty() {
            return;
        }
        let mut state = self.refresh.lock().await;
        let current = self.current_revision();
        if state.attempted.is_some_and(|t| {
            state.revision == current.as_ref().ok().copied()
                && (t >= requested
                    || (!force
                        && state.error.is_some()
                        && t.elapsed() < Duration::from_secs(self.options.refresh_secs.min(30))))
        }) {
            return;
        }
        let time = now();
        let fresh = self.snapshot.read().unwrap().as_ref().is_some_and(|s| {
            current.as_ref().is_ok_and(|rev| *rev == s.revision)
                && s.fetched_at <= time
                && time - s.fetched_at < self.options.refresh_secs
        });
        if !force && fresh {
            return;
        }
        let result = tokio::time::timeout(
            Duration::from_secs(self.options.timeout_secs),
            fetch::models(&self.client, &self.options.codex_client_version),
        )
        .await;
        match result {
            Ok(Ok((models, revision))) => {
                let snapshot = Snapshot {
                    models,
                    fetched_at: now(),
                    revision,
                };
                if self.current_revision().ok() != Some(revision) {
                    state.error = Some("account_changed");
                } else if self
                    .vault
                    .store_catalog(&self.source, Some(&json!(snapshot)))
                    .is_err()
                {
                    state.error = Some("cache_write_failed");
                } else {
                    *self.snapshot.write().unwrap() = Some(snapshot);
                    state.error = None;
                }
            }
            Ok(Err(error)) => {
                if matches!(
                    error,
                    Error::Api {
                        status: 401 | 403,
                        ..
                    } | Error::OAuth(_)
                ) {
                    // Authentication/permission failures must not keep advertising old access.
                    *self.snapshot.write().unwrap() = None;
                    let _ = self.vault.store_catalog(&self.source, None);
                }
                state.error = Some(match error {
                    Error::Api {
                        status: 401 | 403, ..
                    }
                    | Error::OAuth(_) => "authentication_failed",
                    Error::Api {
                        status: 404 | 405, ..
                    } => "discovery_unavailable",
                    Error::InvalidResponse(_) | Error::Json(_) => "invalid_catalog",
                    _ => "upstream_unavailable",
                });
            }
            Err(_) => state.error = Some("timeout"),
        }
        state.attempted = Some(Instant::now());
        state.revision = self.current_revision().ok();
    }

    pub fn status(&self) -> Value {
        let fetched_at = self.snapshot.read().unwrap().as_ref().map(|s| s.fetched_at);
        let state = self.refresh.try_lock();
        json!({"source": if self.client.gemini.is_some() { "gemini_cli_quota" } else { "provider_catalog" },
            "complete_catalog": self.client.gemini.is_none(),
            "fetched_at":fetched_at,
            "models":self.models().len(),
            "refreshing":state.is_err(),
            "last_error":state.ok().and_then(|s| s.error)})
    }

    // Read-only normalized official evidence, with no administrator policy applied.
    pub fn official_models(&self) -> Value {
        let models: BTreeMap<_, _> = self
            .models()
            .into_iter()
            .filter_map(|model| self.evidence(&model).ok().map(|info| (model, info)))
            .collect();
        json!(models)
    }
}

pub(crate) async fn refresh_all(catalogs: Vec<Arc<Catalog>>, force: bool) {
    stream::iter(catalogs)
        .for_each_concurrent(4, |c| async move { c.refresh(force).await })
        .await;
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

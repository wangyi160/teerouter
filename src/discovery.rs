//! One official model/capability catalog per pool, fetched through one usable account.
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
    collections::{BTreeMap, BTreeSet, HashSet},
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
    account: usize,
}

struct Refresh {
    attempted: Option<Instant>,
    revisions: Vec<Option<Option<i64>>>,
    next_account: usize,
    error: Option<&'static str>,
}

pub(crate) struct Catalog {
    clients: Vec<(String, Client)>,
    options: Discovery,
    vault: Arc<Vault>,
    source: String,
    snapshot: RwLock<Option<Snapshot>>,
    refresh: Mutex<Refresh>,
}

impl Catalog {
    pub fn new(
        pool: &str,
        accounts: &[(Deployment, Client, String)],
        options: Discovery,
        vault: Arc<Vault>,
    ) -> Result<Arc<Self>, String> {
        if accounts.is_empty()
            || accounts
                .iter()
                .any(|(d, _, _)| d.provider != accounts[0].0.provider)
        {
            return Err("model discovery requires a nonempty pool with one provider".into());
        }
        // Pool identity and source bindings isolate merchants, pools, endpoints and credentials.
        // Hash input can contain secrets, but only the digest is persisted as a cache key.
        let encoded = zeroize::Zeroizing::new(
            serde_json::to_vec(&(
                "official-pool-capabilities-v1",
                pool,
                &options,
                accounts
                    .iter()
                    .map(|(d, c, binding)| (d, binding, &c.profile))
                    .collect::<Vec<_>>(),
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
            s.account >= accounts.len()
                || s.models.len() > 10000
                || s.models.iter().any(|(id, info)| {
                    !valid_model_id(id)
                        || !valid_model_id(&info.canonical_id)
                        || !s.models.contains_key(&info.canonical_id)
                })
        }) {
            return Err("invalid model cache".into());
        }
        let next_account = snapshot.as_ref().map(|s| s.account).unwrap_or(0);
        Ok(Arc::new(Self {
            clients: accounts
                .iter()
                .map(|(d, c, _)| (d.id.clone(), c.clone()))
                .collect(),
            options,
            vault,
            source,
            snapshot: RwLock::new(snapshot),
            refresh: Mutex::new(Refresh {
                attempted: None,
                revisions: Vec::new(),
                next_account,
                error: None,
            }),
        }))
    }

    fn current_revision(&self, account: usize) -> Result<Option<i64>, Error> {
        self.clients[account]
            .1
            .oauth
            .as_ref()
            .map(|a| a.revision())
            .transpose()
    }

    fn revisions(&self) -> Vec<Option<Option<i64>>> {
        (0..self.clients.len())
            .map(|i| self.current_revision(i).ok())
            .collect()
    }

    fn fresh(&self, snapshot: &Snapshot) -> bool {
        let time = now();
        self.current_revision(snapshot.account).ok() == Some(snapshot.revision)
            && snapshot.fetched_at <= time
            && time - snapshot.fetched_at
                < self.options.refresh_secs.min(self.options.max_stale_secs)
    }

    pub fn models(&self) -> BTreeSet<String> {
        self.snapshot
            .read()
            .unwrap()
            .as_ref()
            .filter(|s| self.fresh(s))
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
        let guard = self.snapshot.read().unwrap();
        let snapshot = guard
            .as_ref()
            .filter(|s| self.fresh(s))
            .ok_or_else(unavailable)?;
        snapshot.models.get(model).cloned().ok_or_else(|| {
            crate::error::ApiError::new(
                404,
                "model_not_found",
                "Model is not in the selected pool's official catalog",
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
        let current = self.revisions();
        if state.attempted.is_some_and(|t| {
            state.revisions == current
                && (t >= requested
                    || (!force
                        && state.error.is_some()
                        && t.elapsed() < Duration::from_secs(self.options.refresh_secs.min(30))))
        }) {
            return;
        }
        let fresh = self
            .snapshot
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|s| self.fresh(s));
        if !force && fresh {
            return;
        }
        // Prefer the last successful source. A failed source is replaced, never merged.
        // Bound the whole pool refresh; reserve time for a fallback after a slow source.
        let budget = Duration::from_secs(self.options.timeout_secs);
        let deadline = Instant::now() + budget;
        let start = state.next_account;
        let candidates: Vec<_> = (0..self.clients.len())
            .map(|n| (start + n) % self.clients.len())
            .filter(|i| {
                self.clients[*i]
                    .1
                    .oauth
                    .as_ref()
                    .is_none_or(|a| a.available())
            })
            .collect();
        let attempt_budget = if candidates.len() > 1 {
            budget / 2
        } else {
            budget
        };
        state.error = Some("authentication_failed");
        for account in candidates {
            if Instant::now() >= deadline {
                break;
            }
            state.next_account = (account + 1) % self.clients.len();
            let client = &self.clients[account].1;
            let result = tokio::time::timeout_at(
                deadline.min(Instant::now() + attempt_budget),
                fetch::models(client, &self.options.codex_client_version),
            )
            .await;
            match result {
                Ok(Ok((models, revision))) => {
                    let snapshot = Snapshot {
                        models,
                        fetched_at: now(),
                        revision,
                        account,
                    };
                    if self.current_revision(account).ok() != Some(revision) {
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
                        state.next_account = account;
                        break;
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
                        // Only the source of the cached evidence can invalidate it.
                        let mut snapshot = self.snapshot.write().unwrap();
                        if snapshot.as_ref().is_some_and(|s| s.account == account) {
                            *snapshot = None;
                            let _ = self.vault.store_catalog(&self.source, None);
                        }
                        if matches!(error, Error::Api { status: 401, .. })
                            && let Some(oauth) = &client.oauth
                        {
                            oauth.rejected();
                        }
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
        }
        state.attempted = Some(Instant::now());
        state.revisions = self.revisions();
    }

    pub fn status(&self) -> Value {
        let (fetched_at, source_account) = self
            .snapshot
            .read()
            .unwrap()
            .as_ref()
            .map(|s| (Some(s.fetched_at), Some(self.clients[s.account].0.clone())))
            .unwrap_or_default();
        let state = self.refresh.try_lock();
        json!({"source": "provider_catalog",
            "scope": "pool", "source_deployment":source_account, "refresh_secs":self.options.refresh_secs,
            "complete_catalog": true,
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
    let mut seen = HashSet::new();
    stream::iter(
        catalogs
            .into_iter()
            .filter(|c| seen.insert(Arc::as_ptr(c) as usize))
            .collect::<Vec<_>>(),
    )
    .for_each_concurrent(4, |c| async move { c.refresh(force).await })
    .await;
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

//! Process-local provider evidence. Sources are weak references to committed
//! merchant generations: metadata is shared, credentials and health are not.
use super::{Catalog, ModelInfo, fetch, now};
use crate::{Error, Provider, config::Discovery, error::ApiError};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

pub(crate) struct Registry {
    providers: HashMap<Provider, Arc<Shared>>,
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

struct Sources {
    enabled: Arc<AtomicBool>,
    catalogs: Vec<Weak<Catalog>>,
}
struct Evidence {
    models: BTreeMap<String, ModelInfo>,
    fetched_at: u64,
    checked_at: Instant,
}
#[derive(Default)]
struct Refresh {
    started: bool,
    completed: Option<Instant>,
    preferred: Option<(String, String, usize)>,
    error: Option<&'static str>,
    next: usize,
}
struct Settings {
    options: Discovery,
    profile: crate::compatibility::Profile,
    generation: u64,
}
pub(crate) struct Shared {
    provider: Provider,
    settings: RwLock<Settings>,
    sources: RwLock<BTreeMap<String, Sources>>,
    snapshot: RwLock<Option<Evidence>>,
    refresh: Mutex<Refresh>,
}

impl Registry {
    pub fn new(
        options: &Discovery,
        profiles: &BTreeMap<String, crate::compatibility::Profile>,
    ) -> Self {
        Self {
            providers: [Provider::OpenAI, Provider::Anthropic, Provider::XAI]
                .into_iter()
                .map(|provider| {
                    (
                        provider,
                        Arc::new(Shared {
                            provider,
                            settings: RwLock::new(Settings {
                                options: options.clone(),
                                generation: 0,
                                profile: profiles
                                    .get(crate::compatibility::name(provider))
                                    .cloned()
                                    .unwrap_or_default(),
                            }),
                            sources: RwLock::default(),
                            snapshot: RwLock::default(),
                            refresh: Mutex::default(),
                        }),
                    )
                })
                .collect(),
        }
    }

    pub fn configure(
        &self,
        options: &Discovery,
        profiles: &BTreeMap<String, crate::compatibility::Profile>,
    ) {
        for shared in self.providers.values() {
            let profile = profiles
                .get(crate::compatibility::name(shared.provider))
                .cloned()
                .unwrap_or_default();
            let mut settings = shared.settings.write().unwrap();
            if serde_json::to_value((&settings.options, &settings.profile)).ok()
                != serde_json::to_value((options, &profile)).ok()
            {
                settings.options = options.clone();
                settings.profile = profile;
                settings.generation += 1;
                drop(settings);
                *shared.snapshot.write().unwrap() = None;
                if let Ok(mut state) = shared.refresh.try_lock() {
                    state.completed = None;
                }
            }
        }
    }

    pub fn replace(&self, owner: &str, enabled: &Arc<AtomicBool>, catalogs: Vec<Arc<Catalog>>) {
        for (provider, shared) in &self.providers {
            let mut seen = std::collections::HashSet::new();
            let selected: Vec<_> = catalogs
                .iter()
                .filter(|c| c.clients[0].1.provider == *provider)
                .filter(|c| seen.insert(Arc::as_ptr(c) as usize))
                .collect();
            // A directly constructed engine may already have fetched evidence
            // before joining this runtime. Reuse only fresh, process-owned data
            // parsed with the same provider profile; never restore it from management.
            if let Ok(mut state) = shared.refresh.try_lock()
                && !state.started
                && let Some((catalog, snapshot)) = selected.iter().find_map(|catalog| {
                    let snapshot = catalog.snapshot.read().unwrap().clone()?;
                    (catalog.fresh(&snapshot)
                        && serde_json::to_value(&catalog.clients[snapshot.account].1.profile).ok()
                            == serde_json::to_value(&shared.settings.read().unwrap().profile).ok())
                    .then_some((*catalog, snapshot))
                })
            {
                let checked_at =
                    Instant::now() - Duration::from_secs(now().saturating_sub(snapshot.fetched_at));
                *shared.snapshot.write().unwrap() = Some(Evidence {
                    models: snapshot.models,
                    fetched_at: snapshot.fetched_at,
                    checked_at,
                });
                state.completed = Some(checked_at);
                state.started = true;
                state.preferred = Some((owner.into(), catalog.source.clone(), snapshot.account));
            }
            let catalogs: Vec<_> = selected
                .into_iter()
                .map(|c| {
                    *c.shared.write().unwrap() = Some(shared.clone());
                    Arc::downgrade(c)
                })
                .collect();
            let mut sources = shared.sources.write().unwrap();
            if catalogs.is_empty() {
                sources.remove(owner);
            } else {
                sources.insert(
                    owner.to_owned(),
                    Sources {
                        enabled: enabled.clone(),
                        catalogs,
                    },
                );
            }
        }
    }

    pub fn remove(&self, owner: &str) {
        for shared in self.providers.values() {
            shared.sources.write().unwrap().remove(owner);
        }
    }
}

impl Shared {
    fn fresh(&self, evidence: &Evidence) -> bool {
        let settings = self.settings.read().unwrap();
        let options = &settings.options;
        evidence.checked_at.elapsed()
            < Duration::from_secs(options.refresh_secs.min(options.max_stale_secs))
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

    pub fn evidence(&self, model: &str) -> Result<ModelInfo, ApiError> {
        let guard = self.snapshot.read().unwrap();
        let snapshot = guard.as_ref().filter(|s| self.fresh(s)).ok_or_else(|| {
            ApiError::new(
                503,
                "catalog_unavailable",
                "A current authenticated official model catalog is required",
            )
        })?;
        snapshot.models.get(model).cloned().ok_or_else(|| {
            ApiError::new(
                404,
                "model_not_found",
                "Model is not in the official provider catalog",
            )
        })
    }

    pub fn status(&self) -> Value {
        let snapshot = self.snapshot.read().unwrap();
        let snapshot = snapshot.as_ref().filter(|s| self.fresh(s));
        let state = self.refresh.try_lock();
        json!({"source":"provider_catalog", "scope":"provider", "provider":self.provider,
            "refresh_secs":self.settings.read().unwrap().options.refresh_secs,
            "complete_catalog":snapshot.is_some(), "fetched_at":snapshot.map(|s| s.fetched_at),
            "models":snapshot.map(|s| s.models.len()).unwrap_or(0), "refreshing":state.is_err(),
            "last_error":state.ok().and_then(|s| s.error)})
    }

    pub async fn refresh(&self, force: bool) {
        let requested = Instant::now();
        let mut state = self.refresh.lock().await;
        let (options, profile, generation) = {
            let settings = self.settings.read().unwrap();
            (
                settings.options.clone(),
                settings.profile.clone(),
                settings.generation,
            )
        };
        // Coalesce overlapping manual refreshes and perform failed checks only
        // on the next regular interval. Account synchronization itself is not a probe.
        if state.completed.is_some_and(|t| {
            t >= requested || (!force && t.elapsed() < Duration::from_secs(options.refresh_secs))
        }) {
            return;
        }
        state.started = true;
        let mut candidates = Vec::new();
        {
            let sources = self.sources.read().unwrap();
            for (owner, source) in sources
                .iter()
                .filter(|(_, s)| s.enabled.load(Ordering::SeqCst))
            {
                for catalog in source.catalogs.iter().filter_map(Weak::upgrade) {
                    for account in 0..catalog.clients.len() {
                        let client = &catalog.clients[account].1;
                        if client.oauth.as_ref().is_none_or(|a| a.available()) {
                            candidates.push((owner.clone(), catalog.clone(), account));
                        }
                    }
                }
            }
        }
        if !candidates.is_empty() {
            let offset = state.next % candidates.len();
            candidates.rotate_left(offset);
        }
        if let Some(preferred) = &state.preferred {
            candidates.sort_by_key(|(owner, c, a)| {
                (owner, &c.source, a) != (&preferred.0, &preferred.1, &preferred.2)
            });
        }
        let budget = Duration::from_secs(options.timeout_secs);
        let deadline = Instant::now() + budget;
        let attempt_budget = if candidates.len() > 1 {
            budget / 2
        } else {
            budget
        };
        state.error = Some("authentication_failed");
        for (owner, catalog, account) in candidates {
            if Instant::now() >= deadline {
                state.error = Some("timeout");
                break;
            }
            if !self.active(&owner, &catalog) {
                state.error = Some("account_changed");
                continue;
            }
            state.next = state.next.wrapping_add(1);
            let mut client = catalog.clients[account].1.clone();
            // Shared evidence uses the platform's official parsing/wire profile,
            // not a different merchant's capability overrides.
            client.profile = profile.clone();
            let revision_before = catalog.current_revision(account).ok();
            let credential_revision = client.oauth.as_ref().and_then(|a| a.request_revision().ok());
            let result = tokio::time::timeout_at(
                deadline.min(Instant::now() + attempt_budget),
                fetch::models(&client, &options.codex_client_version),
            )
            .await;
            if !self.active(&owner, &catalog)
                || catalog.current_revision(account).ok() != revision_before
            {
                state.error = Some("account_changed");
                continue;
            }
            match result {
                Ok(Ok((models, revision))) => {
                    // Hold membership lock through publication: a removed generation
                    // cannot publish late evidence or stay a credential source.
                    let sources = self.sources.read().unwrap();
                    let mut snapshot = self.snapshot.write().unwrap();
                    let settings = self.settings.read().unwrap();
                    if !Self::contains(&sources, &owner, &catalog)
                        || catalog.current_revision(account).ok() != Some(revision)
                        || settings.generation != generation
                    {
                        state.error = Some("account_changed");
                        continue;
                    }
                    *snapshot = Some(Evidence {
                        models,
                        fetched_at: now(),
                        checked_at: Instant::now(),
                    });
                    state.preferred = Some((owner, catalog.source.clone(), account));
                    state.error = None;
                    break;
                }
                Ok(Err(error)) => {
                    // A superseded request must not reject newly synchronized credentials.
                    if matches!(error, Error::Api { status: 401, .. })
                        && self.active(&owner, &catalog)
                        && catalog.current_revision(account).ok() == revision_before
                        && let Some(oauth) = &client.oauth
                        && let Some(revision) = credential_revision
                    {
                        oauth.rejected(revision);
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
        if state.error.is_some() {
            *self.snapshot.write().unwrap() = None;
            state.preferred = None;
        }
        state.completed = (state.error != Some("account_changed")).then(Instant::now);
    }

    fn contains(sources: &BTreeMap<String, Sources>, owner: &str, catalog: &Arc<Catalog>) -> bool {
        sources.get(owner).is_some_and(|s| {
            s.enabled.load(Ordering::SeqCst)
                && s.catalogs
                    .iter()
                    .filter_map(Weak::upgrade)
                    .any(|c| c.source == catalog.source)
        })
    }
    fn active(&self, owner: &str, catalog: &Arc<Catalog>) -> bool {
        Self::contains(&self.sources.read().unwrap(), owner, catalog)
    }
}

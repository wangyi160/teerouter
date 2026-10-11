use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::{
    Client,
    config::{Deployment, Model, Strategy},
    upstream::Protocol,
};

pub(crate) struct Backend {
    pub config: Deployment,
    pub client: Client,
    pub catalog: Option<Arc<crate::discovery::Catalog>>,
    pub(crate) state: Arc<Mutex<State>>,
}

pub(crate) struct State {
    in_flight: usize,
    failures: u64,
    open_until: Option<Instant>,
    window: Instant,
    requests: u64,
    successes: u64,
    errors: u64,
    last_error: Option<&'static str>,
}

impl Backend {
    pub(crate) fn validate_model(
        &self,
        protocol: Protocol,
        model: &str,
        body: &Value,
        rules: &[crate::policy::Restriction],
    ) -> Result<(), crate::error::ApiError> {
        let catalog = self.catalog.as_ref().ok_or_else(|| {
            crate::error::ApiError::new(
                503,
                "catalog_unavailable",
                "Official model catalog is required",
            )
        })?;
        let info = catalog.evidence(model)?;
        info.capabilities
            .check(self.config.provider, protocol, body)?;
        crate::policy::Restrictions::for_model(
            rules,
            self.config.provider,
            model,
            &info.canonical_id,
        )
        .check(body)
    }

    pub fn new(
        config: Deployment,
        client: Client,
        catalog: Option<Arc<crate::discovery::Catalog>>,
        shared: Option<Arc<Mutex<State>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            client,
            catalog,
            state: shared.unwrap_or_else(|| {
                Arc::new(Mutex::new(State {
                    in_flight: 0,
                    failures: 0,
                    open_until: None,
                    window: Instant::now(),
                    requests: 0,
                    successes: 0,
                    errors: 0,
                    last_error: None,
                }))
            }),
        })
    }

    fn available(&self, state: &mut State) -> bool {
        if state.window.elapsed() >= Duration::from_secs(60) {
            state.window = Instant::now();
            state.requests = 0;
        }
        state.in_flight < self.config.max_concurrent
            && (self.config.rpm == 0 || state.requests < self.config.rpm)
            && state
                .open_until
                .is_none_or(|until| Instant::now() >= until && state.in_flight == 0)
    }

    fn load(&self) -> Option<usize> {
        if self.client.oauth.as_ref().is_some_and(|a| !a.available()) {
            return None;
        }
        let mut state = self.state.lock().unwrap();
        self.available(&mut state).then_some(state.in_flight)
    }

    fn acquire(self: &Arc<Self>) -> Option<Lease> {
        let mut state = self.state.lock().unwrap();
        if !self.available(&mut state) {
            return None;
        }
        state.in_flight += 1;
        state.requests += 1;
        Some(Lease {
            backend: self.clone(),
        })
    }

    pub fn status(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({"id":self.config.id, "provider":format!("{:?}", self.config.provider).to_lowercase(),"last_error":state.last_error,
            "in_flight":state.in_flight, "successes":state.successes, "errors":state.errors,
            "consecutive_failures":state.failures,
            "cooldown_remaining_secs":state.open_until.map(|t| t.saturating_duration_since(Instant::now()).as_secs()).unwrap_or(0),
            "circuit":if state.open_until.is_none() {"closed"} else if state.open_until.is_some_and(|t| t > Instant::now()) {"open"} else {"half_open"}})
    }

    pub fn ready(&self) -> bool {
        if self.client.oauth.as_ref().is_some_and(|a| !a.available()) {
            return false;
        }
        let state = self.state.lock().unwrap();
        state.open_until.is_none_or(|until| until <= Instant::now())
    }
    pub(crate) fn binding(config: &Deployment) -> String {
        match &config.oauth_account {
            Some(name) => format!(
                "oauth:{}:{name}",
                crate::compatibility::name(config.provider)
            ),
            None => format!("deployment:{}", config.id),
        }
    }
    pub(crate) fn diagnostic(&self, code: &'static str) {
        self.state.lock().unwrap().last_error = Some(code);
    }
    pub(crate) fn recover(&self) {
        let mut state = self.state.lock().unwrap();
        state.open_until = None;
        state.failures = 0;
        state.last_error = None;
    }
}

pub(crate) struct Lease {
    pub backend: Arc<Backend>,
}

impl Lease {
    pub fn success(&self) {
        let mut state = self.backend.state.lock().unwrap();
        state.failures = 0;
        state.open_until = None;
        state.successes += 1;
        state.last_error = None;
    }

    pub fn failure(&self, retry_after: Option<u64>, immediate: bool) {
        let mut state = self.backend.state.lock().unwrap();
        state.failures += 1;
        state.errors += 1;
        if immediate
            || retry_after.is_some()
            || state.failures >= self.backend.config.failure_threshold
            || state.open_until.is_some()
        {
            let seconds = retry_after
                .unwrap_or(self.backend.config.cooldown_secs)
                .clamp(1, 3600);
            state.open_until = Some(Instant::now() + Duration::from_secs(seconds));
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.backend.state.lock().unwrap().in_flight -= 1;
    }
}

pub(crate) struct Group {
    pub config: Model,
    pub backends: Vec<Arc<Backend>>,
    cursor: AtomicU64,
}

impl Group {
    pub fn new(config: Model, backends: Vec<Arc<Backend>>) -> Self {
        Self {
            config,
            backends,
            cursor: AtomicU64::new(0),
        }
    }

    pub fn select(
        &self,
        protocol: Protocol,
        excluded: &HashSet<String>,
        binding: Option<&str>,
        eligible: &HashSet<String>,
    ) -> Option<Lease> {
        let mut candidates: Vec<_> = self
            .backends
            .iter()
            .filter(|b| protocol.supports(b.config.provider) && !excluded.contains(&b.config.id))
            .filter(|b| eligible.contains(&b.config.id))
            .filter(|b| binding.is_none_or(|s| Self::binding_matches(b, s)))
            .filter_map(|b| b.load().map(|load| (b, load)))
            .collect();
        while !candidates.is_empty() {
            let counter = self.cursor.fetch_add(1, Ordering::Relaxed);
            let index = match self.config.strategy {
                Strategy::RoundRobin => {
                    let total: u64 = candidates.iter().map(|(b, _)| b.config.weight).sum();
                    let mut slot = counter % total;
                    candidates
                        .iter()
                        .position(|(b, _)| {
                            if slot < b.config.weight {
                                true
                            } else {
                                slot -= b.config.weight;
                                false
                            }
                        })
                        .unwrap_or(0)
                }
                Strategy::LeastBusy => {
                    // Rotate tie-breaking, then compare load normalized by deployment weight.
                    let count = candidates.len();
                    candidates.rotate_left(counter as usize % count);
                    (0..count)
                        .min_by(|&a, &b| {
                            (candidates[a].1 as u128 * candidates[b].0.config.weight as u128).cmp(
                                &(candidates[b].1 as u128 * candidates[a].0.config.weight as u128),
                            )
                        })
                        .unwrap_or(0)
                }
            };
            let (backend, _) = candidates.remove(index);
            if let Some(lease) = backend.acquire() {
                return Some(lease);
            }
        }
        None
    }
    fn binding_matches(backend: &Backend, binding: &str) -> bool {
        Backend::binding(&backend.config) == binding
    }
}

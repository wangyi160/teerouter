//! Pool candidates use the same official evidence and account affinity as dispatch.
use super::*;
use crate::network::latency::{Reading, State, Target};

pub(crate) struct PoolChoice {
    pub cost: u128,
    pub sticky: bool,
    pub binding: Option<String>,
    pub latency: Reading,
}
impl Engine {
    pub(crate) fn latency_targets(&self) -> Vec<Arc<Target>> {
        self.groups
            .values()
            .flat_map(|g| &g.backends)
            .filter(|b| b.client.oauth.as_ref().is_none_or(|a| a.available()))
            .filter_map(|b| b.client.latency.clone())
            .collect()
    }
    pub(crate) fn routing_context_owner(
        &self,
        account: usize,
        body: &Value,
        headers: &HeaderMap,
    ) -> Result<bool, ApiError> {
        let owner = &self.accounts[account].config.name;
        for (kind, token) in [
            ("response", body["previous_response_id"].as_str()),
            (
                "session",
                headers
                    .get("x-router-session")
                    .and_then(|v| v.to_str().ok()),
            ),
        ] {
            if let Some(token) = token
                && self
                    .vault
                    .binding(owner, kind, token)
                    .map_err(crate::runtime::storage)?
                    .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn routing_candidates(
        &self,
        account: usize,
        protocol: Protocol,
        model: &str,
        body: &Value,
        headers: &HeaderMap,
        pool: Option<&str>,
        agent: crate::agent::Agent,
    ) -> Result<Vec<(String, Vec<String>)>, ApiError> {
        if !self.accounts[account].has_capacity()
            || !self.storage_ready()
            || !self.ledger.healthy()
            || self.global.used() >= self.config.server.max_concurrent.min(4096)
        {
            return Ok(vec![]);
        }
        let groups: Vec<_> = self
            .config
            .models
            .iter()
            .filter_map(|p| self.groups.get(&p.name))
            .filter(|g| pool.is_none_or(|p| g.config.name == p))
            .filter(|g| self.model_allowed(account, &g.config.name, model))
            .filter(|g| g.backends.iter().any(|b| agent.allows(b.config.provider)))
            .collect();
        let catalogs = groups
            .iter()
            .flat_map(|g| &g.backends)
            .filter(|b| protocol.supports(b.config.provider))
            .filter_map(|b| b.catalog.clone())
            .collect();
        timeout_at(
            Instant::now() + Duration::from_secs(self.config.server.request_timeout_secs),
            crate::discovery::refresh_all(catalogs, false),
        )
        .await
        .map_err(|_| ApiError::new(504, "request_timeout", "Official model discovery timed out"))?;
        let affinity = affinity::Affinity::new(self, account, &groups, body, &mut headers.clone())?;
        Ok(groups
            .into_iter()
            .filter_map(|g| {
                let bindings: Vec<_> = g
                    .backends
                    .iter()
                    .filter(|b| {
                        protocol.supports(b.config.provider)
                            && b.has_capacity()
                            && affinity
                                .pinned
                                .as_ref()
                                .is_none_or(|p| Backend::binding(&b.config) == *p)
                            && b.validate_model(protocol, model, body, &self.config.restrictions)
                                .is_ok()
                    })
                    .map(|b| Backend::binding(&b.config))
                    .collect();
                (!bindings.is_empty()).then(|| (g.config.name.clone(), bindings))
            })
            .collect())
    }
    pub(crate) fn routing_cache_choice(
        &self,
        request: &cache::Request,
        body: &Value,
        bindings: &[String],
        quote: &crate::pricing::Quote,
        latency: Option<&State>,
    ) -> Option<PoolChoice> {
        let group = self.groups.get(&quote.price.pool)?;
        let provider = group.config.provider?;
        let readings: HashMap<_, _> = latency
            .map(|state| {
                group
                    .backends
                    .iter()
                    .filter_map(|b| {
                        b.client
                            .latency
                            .as_ref()
                            .map(|t| (Backend::binding(&b.config), state.reading(t)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut choices: Vec<_> = bindings
            .iter()
            .filter_map(|binding| {
                Some((
                    self.cache
                        .estimate(request, binding, provider, body, quote)?,
                    self.cache.sticky(request, binding),
                    binding,
                    readings.get(binding).copied().unwrap_or_default(),
                ))
            })
            .collect();
        choices.sort_by(|a, b| {
            let price = a.0.cmp(&b.0).then(b.1.cmp(&a.1));
            if latency.is_some() {
                a.3.failed()
                    .cmp(&b.3.failed())
                    .then(b.1.cmp(&a.1))
                    .then(a.3.cmp(&b.3))
                    .then(price)
            } else {
                price
            }
        });
        let (cost, sticky, binding, reading) = choices.first()?;
        // Equal cost/latency leaves the account scheduler free to balance load.
        let prefer = *sticky
            || choices
                .iter()
                .any(|v| v.0 > *cost || (latency.is_some() && v.3 != *reading));
        Some(PoolChoice {
            cost: *cost,
            sticky: *sticky,
            binding: prefer.then(|| (*binding).clone()),
            latency: *reading,
        })
    }
}

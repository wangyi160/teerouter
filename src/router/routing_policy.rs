//! Pool candidates use the same official evidence and account affinity as dispatch.
use super::*;
impl Engine {
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
    ) -> Result<Vec<String>, ApiError> {
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
            .filter(|g| {
                g.backends.iter().any(|b| {
                    protocol.supports(b.config.provider)
                        && b.has_capacity()
                        && affinity
                            .pinned
                            .as_ref()
                            .is_none_or(|p| Backend::binding(&b.config) == *p)
                        && b.validate_model(protocol, model, body, &self.config.restrictions)
                            .is_ok()
                })
            })
            .map(|g| g.config.name.clone())
            .collect())
    }
}

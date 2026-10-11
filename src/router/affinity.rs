use super::*;
pub(super) struct Affinity {
    owner: String,
    pub session: Option<String>,
    pub pinned: Option<String>,
    vault: Arc<crate::vault::Vault>,
    ttl: u64,
    max: u64,
    revision: i64,
}
impl Affinity {
    pub fn new(
        engine: &Engine,
        account: usize,
        groups: &[&Group],
        body: &Value,
        headers: &mut HeaderMap,
    ) -> Result<Self, ApiError> {
        let header = |name: &str| -> Result<Option<String>, ApiError> {
            headers
                .get(name)
                .map(|v| {
                    v.to_str()
                        .ok()
                        .filter(|s| {
                            !s.is_empty()
                                && s.len() <= 256
                                && !s.bytes().any(|c| c.is_ascii_control())
                        })
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            ApiError::new(
                                400,
                                "invalid_affinity",
                                "Invalid account/session selector",
                            )
                        })
                })
                .transpose()
        };
        let session = header("x-router-session")?;
        let explicit = header("x-router-account")?;
        let owner = engine.accounts[account].config.name.clone();
        let bindings: HashSet<_> = groups
            .iter()
            .flat_map(|g| &g.backends)
            .map(|b| Backend::binding(&b.config))
            .collect();
        let mut pinned = if let Some(name) = explicit {
            let selected: HashSet<_> = groups
                .iter()
                .flat_map(|g| &g.backends)
                .filter(|b| b.config.oauth_account.as_deref().unwrap_or(&b.config.id) == name)
                .map(|b| Backend::binding(&b.config))
                .collect();
            if selected.len() != 1 {
                return Err(ApiError::new(
                    403,
                    "account_not_in_route",
                    "Requested account is not in this route",
                ));
            }
            selected.into_iter().next()
        } else {
            None
        };
        if let Some(session) = &session
            && let Some(bound) = engine
                .vault
                .binding(&owner, "session", session)
                .map_err(crate::runtime::storage)?
        {
            reconcile(&mut pinned, bound)?;
        }
        if let Some(id) = body.get("previous_response_id").filter(|v| !v.is_null()) {
            let id = id
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 512)
                .ok_or_else(|| {
                    ApiError::new(400, "invalid_response_id", "Invalid previous response ID")
                })?;
            if let Some(bound) = engine
                .vault
                .binding(&owner, "response", id)
                .map_err(crate::runtime::storage)?
            {
                if bound == "!retired" {
                    return Err(ApiError::new(
                        409,
                        "response_account_replaced",
                        "The original response account was replaced",
                    ));
                } else if bound == "!ambiguous" {
                    if pinned.is_none() {
                        return Err(ApiError::new(
                            409,
                            "ambiguous_response_account",
                            "Response ID exists on multiple accounts; select its original account",
                        ));
                    }
                } else {
                    reconcile(&mut pinned, bound)?;
                }
            } else if pinned.is_none() {
                if bindings.len() == 1 {
                    pinned = bindings.iter().next().cloned();
                } else {
                    return Err(ApiError::new(
                        409,
                        "unknown_response_account",
                        "Response account is unknown; select its original account explicitly",
                    ));
                }
            }
        }
        fn has_file_id(value: &Value) -> bool {
            match value {
                Value::Object(map) => map.contains_key("file_id") || map.values().any(has_file_id),
                Value::Array(a) => a.iter().any(has_file_id),
                _ => false,
            }
        }
        if has_file_id(body) && pinned.is_none() && bindings.len() > 1 {
            return Err(ApiError::new(
                400,
                "file_account_required",
                "Select the account that owns the provider file",
            ));
        }
        if pinned.as_ref().is_some_and(|p| !bindings.contains(p)) {
            return Err(ApiError::new(
                409,
                "session_account_unavailable",
                "Original session account is no longer in this route",
            ));
        }
        headers.remove("x-router-session");
        headers.remove("x-router-account");
        Ok(Self {
            owner,
            session,
            pinned,
            vault: engine.vault.clone(),
            ttl: engine.config.operations.session_ttl_secs,
            max: engine.config.operations.max_sessions,
            revision: engine.config_revision,
        })
    }
    pub fn select(&mut self, backend: &Backend) -> Result<(), ApiError> {
        if let Some(session) = &self.session {
            let binding = Backend::binding(&backend.config);
            self.save("session", session, &binding)?;
            self.pinned = Some(binding);
        }
        Ok(())
    }
    fn save(&self, kind: &str, token: &str, binding: &str) -> Result<(), ApiError> {
        if token.len() > 512 {
            return Err(ApiError::new(
                502,
                "invalid_response_id",
                "Upstream response ID exceeds limit",
            ));
        }
        if !self
            .vault
            .bind_session_guarded(
                (&self.owner, kind, token),
                binding,
                self.ttl,
                self.max,
                Some(self.revision),
            )
            .map_err(crate::runtime::storage)?
        {
            return Err(ApiError::new(
                409,
                "session_conflict",
                "Session or response ID is already bound to another account",
            ));
        }
        Ok(())
    }
    pub fn response(
        &self,
        protocol: Protocol,
        value: &Value,
        backend: &Backend,
    ) -> Result<(), ApiError> {
        if protocol == Protocol::Responses {
            let response = value.get("response").unwrap_or(value);
            if let Some(id) = response["id"].as_str().filter(|s| !s.is_empty()) {
                self.save("response", id, &Backend::binding(&backend.config))?;
            }
        }
        Ok(())
    }
}
fn reconcile(pinned: &mut Option<String>, binding: String) -> Result<(), ApiError> {
    if pinned.as_ref().is_some_and(|s| s != &binding) {
        return Err(ApiError::new(
            409,
            "session_conflict",
            "Account selector conflicts with stored session",
        ));
    }
    *pinned = Some(binding);
    Ok(())
}

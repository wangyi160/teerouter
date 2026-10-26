use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::time::{Instant, timeout_at};

use crate::{
    Client, Error,
    accounting::{Account, Ticket},
    config::{self, Config},
    error::ApiError,
    ledger::Ledger,
    scheduler::{Backend, Group, Lease},
    upstream::{self, EventStream, Frame, Output},
};

pub use crate::upstream::Protocol;
#[path = "router/affinity.rs"]
mod affinity;
#[path = "router/cache.rs"]
pub(crate) mod cache;
#[path = "router/operations.rs"]
mod operations;
#[path = "router/pools.rs"]
mod pools;
#[path = "router/routing_policy.rs"]
mod routing_policy;

pub struct Engine {
    pub(crate) config: Config,
    pub(crate) vault: Arc<crate::vault::Vault>,
    config_revision: i64,
    accounts: Vec<Arc<Account>>,
    admin_secret: Option<String>,
    groups: HashMap<String, Group>,
    ledger: Arc<Ledger>,
    global: Arc<crate::resources::Gate>,
    sequence: Arc<AtomicU64>,
    request_prefix: Arc<str>,
    cache: Arc<cache::State>,
}

pub(crate) struct Routed {
    pub deployment: String,
    pub attempts: usize,
    pub output: Output,
}

impl Engine {
    pub fn new(config: Config, env: impl Fn(&str) -> Option<String>) -> Result<Arc<Self>, String> {
        let bundle = crate::vault::Provision::legacy(config, env).map_err(|e| e.to_string())?;
        let vault = crate::vault::Vault::memory().map_err(|e| e.to_string())?;
        vault.provision(&bundle).map_err(|e| e.to_string())?;
        Self::from_vault(vault)
    }

    pub fn from_vault(vault: Arc<crate::vault::Vault>) -> Result<Arc<Self>, String> {
        let snapshot = vault.snapshot().map_err(|e| e.to_string())?;
        Self::from_snapshot(vault, &snapshot.bundle, None, snapshot.revision)
    }
    pub(crate) fn from_snapshot(
        vault: Arc<crate::vault::Vault>,
        bundle: &crate::vault::Provision,
        previous: Option<&Arc<Self>>,
        config_revision: i64,
    ) -> Result<Arc<Self>, String> {
        let config = bundle.config.clone();
        let env = |name: &str| bundle.secrets.get(name).cloned();
        config.validate()?;
        let accounts: Vec<_> = config
            .keys
            .iter()
            .map(|key| {
                Ok(Account::inherit(
                    key.clone(),
                    config::secret(&key.key_env, &env)?,
                    vault.clone(),
                    previous.and_then(|p| p.accounts.iter().find(|a| a.config.name == key.name)),
                ))
            })
            .collect::<Result<_, String>>()?;
        for (index, account) in accounts.iter().enumerate() {
            if accounts[..index].iter().any(|a| a.secret == account.secret) {
                return Err("API key values must be unique".into());
            }
        }
        let admin_secret = config
            .server
            .admin_key_env
            .as_ref()
            .map(|name| config::secret(name, &env))
            .transpose()?;
        if admin_secret
            .as_ref()
            .is_some_and(|s| accounts.iter().any(|a| &a.secret == s))
        {
            return Err("admin key must differ from client API keys".into());
        }
        let mut groups = HashMap::new();
        let mut oauth_accounts = HashMap::new();
        let mut shared = HashMap::new();
        if let Some(old) = previous {
            for backend in old.groups.values().flat_map(|g| &g.backends) {
                shared.insert(Backend::binding(&backend.config), backend.state.clone());
            }
        }
        for model in &config.models {
            let mut prepared = Vec::new();
            for deployment in model.deployments.iter().filter(|d| d.enabled) {
                let deployment = deployment.clone();
                if deployment
                    .oauth_account
                    .as_ref()
                    .is_some_and(|n| bundle.accounts.get(n).is_some_and(|a| !a.enabled))
                {
                    continue;
                }
                let profile = config
                    .compatibility
                    .get(crate::compatibility::name(deployment.provider))
                    .cloned()
                    .unwrap_or_default();
                let mut client = if let Some(name) = &deployment.oauth_account {
                    if deployment.proxy.is_some() {
                        return Err("OAuth proxy must be configured on the account".into());
                    }
                    let index = (deployment.provider, name.clone());
                    let account = if let Some(account) = oauth_accounts.get(&index) {
                        Arc::clone(account)
                    } else {
                        let proxy = bundle
                            .accounts
                            .get(name)
                            .and_then(|a| a.proxy.clone())
                            .ok_or("account proxy required")?;
                        let mut account = crate::oauth::TokenStore::prepared(
                            deployment.provider,
                            vault.clone(),
                            name,
                            proxy,
                        )
                        .map_err(|e| format!("deployment {}: {e}", deployment.id))?;
                        let store = Arc::get_mut(&mut account).unwrap();
                        store.profile = profile.clone();
                        oauth_accounts.insert(index, account.clone());
                        account
                    };
                    let http = crate::network::http_client(
                        deployment.provider,
                        bundle.accounts[name]
                            .proxy
                            .as_deref()
                            .ok_or("account proxy required")?,
                        deployment.timeout_secs,
                    )
                    .map_err(|e| e.to_string())?;
                    Client::with_oauth(account, http)
                } else {
                    let proxy = deployment
                        .proxy
                        .as_deref()
                        .ok_or("API-key deployment proxy is required")?;
                    let http = crate::network::http_client(
                        deployment.provider,
                        proxy,
                        deployment.timeout_secs,
                    )
                    .map_err(|e| e.to_string())?;
                    Client::with_http_client(
                        deployment.provider,
                        config::secret(&deployment.api_key_env, &env)?,
                        http,
                    )
                }
                .map_err(|_| format!("invalid credential for deployment {}", deployment.id))?;
                client.profile = profile;
                client.limits = crate::resources::Limits {
                    bytes: config.server.max_response_bytes,
                    event: config.server.max_event_bytes,
                    idle: config.server.stream_idle_secs,
                };
                if let Some(base) = &deployment.api_base {
                    client = client.with_base_url(base).map_err(|_| {
                        format!("invalid api_base for deployment {}", deployment.id)
                    })?;
                }
                let proxy = deployment
                    .oauth_account
                    .as_ref()
                    .and_then(|name| bundle.accounts.get(name))
                    .and_then(|account| account.proxy.as_deref())
                    .or(deployment.proxy.as_deref())
                    .ok_or("missing proxy")?;
                client.latency = Some(crate::network::latency::Target::new(
                    deployment.provider,
                    proxy,
                    &client.base_url,
                ));
                client.transport = Some(
                    crate::network::metered::Transport::new(
                        deployment.provider,
                        proxy,
                        deployment.timeout_secs,
                    )
                    .map_err(|_| "cannot create metered upstream transport")?,
                );
                let binding = if let Some(name) = &deployment.oauth_account {
                    bundle.accounts[name]
                        .proxy
                        .as_deref()
                        .ok_or("missing proxy")?
                } else {
                    bundle
                        .secrets
                        .get(&deployment.api_key_env)
                        .map(String::as_str)
                        .ok_or("missing API key")?
                }
                .to_owned();
                prepared.push((deployment, client, binding));
            }
            let catalog = if prepared.is_empty() {
                None
            } else {
                Some(crate::discovery::Catalog::new(
                    &model.name,
                    &prepared,
                    config.discovery.clone(),
                    vault.clone(),
                )?)
            };
            let mut backends = Vec::new();
            for (deployment, client, _) in prepared {
                let identity = Backend::binding(&deployment);
                let backend = Backend::new(
                    deployment,
                    client,
                    catalog.clone(),
                    shared.get(&identity).cloned(),
                );
                shared.insert(identity, backend.state.clone());
                backends.push(backend);
            }
            groups.insert(model.name.clone(), Group::new(model.clone(), backends));
        }
        let ledger = previous
            .map(|p| p.ledger.clone())
            .unwrap_or_else(|| Arc::new(Ledger::new(vault.clone())));
        let global = previous.map(|p| p.global.clone()).unwrap_or_default();
        Ok(Arc::new(Self {
            vault,
            config_revision,
            config,
            accounts,
            admin_secret,
            groups,
            cache: previous.map(|p| p.cache.clone()).unwrap_or_default(),
            ledger,
            global,
            sequence: previous
                .map(|p| p.sequence.clone())
                .unwrap_or_else(|| Arc::new(AtomicU64::new(1))),
            request_prefix: match previous {
                Some(p) => p.request_prefix.clone(),
                None => {
                    let mut bytes = [0_u8; 16];
                    getrandom::getrandom(&mut bytes)
                        .map_err(|_| "Request ID entropy unavailable")?;
                    bytes
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                        .into()
                }
            },
        }))
    }

    pub(crate) fn request_id(&self) -> String {
        format!(
            "req-{}-{:x}",
            self.request_prefix,
            self.sequence.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub(crate) fn authenticate(&self, secret: &str) -> Option<usize> {
        self.accounts
            .iter()
            .position(|a| bool::from(a.secret.as_bytes().ct_eq(secret.as_bytes())))
    }

    pub(crate) fn authorize(&self, index: usize) -> Result<(), ApiError> {
        self.accounts[index].authorize()
    }

    pub(crate) fn authenticate_admin(&self, secret: &str) -> bool {
        self.admin_secret
            .as_ref()
            .is_some_and(|s| bool::from(s.as_bytes().ct_eq(secret.as_bytes())))
    }

    pub(crate) fn admin_enabled(&self) -> bool {
        self.admin_secret.is_some()
    }
    pub(crate) fn ready(&self) -> bool {
        self.ledger.healthy()
            && self.storage_ready()
            && self
                .groups
                .values()
                .any(|g| g.backends.iter().any(|b| b.ready()))
    }

    pub(crate) async fn models(&self, account: usize) -> Value {
        let catalogs = self
            .groups
            .values()
            .flat_map(|g| g.backends.iter().filter_map(|b| b.catalog.clone()))
            .collect();
        crate::discovery::refresh_all(catalogs, false).await;
        let mut visible: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
        for group in self.groups.values() {
            for backend in &group.backends {
                let Some(catalog) = &backend.catalog else {
                    continue;
                };
                for model in catalog.models() {
                    if !self.model_allowed(account, &group.config.name, &model) {
                        continue;
                    }
                    let Ok(info) = catalog.evidence(&model) else {
                        continue;
                    };
                    let policy = crate::policy::Restrictions::for_model(
                        &self.config.restrictions,
                        backend.config.provider,
                        &model,
                        &info.canonical_id,
                    );
                    if policy.block_model {
                        continue;
                    }
                    let protocols: Vec<_> = [
                        (Protocol::Chat, "chat_completions"),
                        (Protocol::Responses, "responses"),
                        (Protocol::Messages, "messages"),
                    ]
                    .into_iter()
                    .filter_map(|(p, name)| backend.client.supports_native(p).then_some(name))
                    .collect();
                    visible.entry(model).or_default().push(json!({"pool":group.config.name,
                        "deployment":backend.config.id,"canonical_id":info.canonical_id,
                        "provider":backend.config.provider,"local_test":backend.config.provider.is_local_test(),
                        "protocols":protocols,
                        "capabilities":info.capabilities,"available_capabilities":policy.available(&info.capabilities)}));
                }
            }
        }
        let data: Vec<_> = visible.into_iter().map(|(model, capabilities)| {
            let pools: std::collections::BTreeSet<_> = capabilities.iter().filter_map(|c| c["pool"].as_str()).collect();
            json!({"id":model,"object":"model","created":0,"owned_by":"teellm","router_pools":pools,"router_capabilities":capabilities})
        }).collect();
        json!({"object":"list", "data":data})
    }

    pub async fn refresh_models(&self, force: bool) {
        crate::discovery::refresh_all(self.catalogs(), force).await;
    }

    pub(crate) fn catalogs(&self) -> Vec<Arc<crate::discovery::Catalog>> {
        self.groups
            .values()
            .flat_map(|g| &g.backends)
            .filter_map(|b| b.catalog.clone())
            .collect()
    }

    pub(crate) fn discovery_status(&self) -> Value {
        json!({"data": self.groups.values().flat_map(|g| g.backends.iter().map(|b|
            json!({"pool":g.config.name,"deployment":b.config.id,"catalog":b.catalog.as_ref().map(|c| c.status()),"official_models":b.catalog.as_ref().map(|c| c.official_models())}))).collect::<Vec<_>>()})
    }

    pub(crate) fn usage(&self, account: Option<usize>) -> Result<Value, ApiError> {
        let read = |a: &Arc<Account>| {
            self.ledger
                .snapshot(&a.config.name)
                .map(|usage| json!({"key":a.config.name,"usage":usage}))
                .map_err(|_| {
                    ApiError::new(
                        503,
                        "accounting_unavailable",
                        "Usage synchronization is unavailable",
                    )
                })
        };
        if let Some(index) = account {
            let account = &self.accounts[index];
            return read(account);
        }
        Ok(json!({"data":self.accounts.iter().map(read).collect::<Result<Vec<_>,_>>()?}))
    }

    pub(crate) fn status(&self) -> Value {
        let mut groups: Vec<_> = self.groups.values().collect();
        groups.sort_by_key(|g| &g.config.name);
        json!({"ready":self.ready(),"moderation_mode":"upstream_refusal","lockllm_enabled":false,"refusal_audit_enabled":true,"usage_healthy":self.ledger.healthy(),"storage":if self.vault.journal_enabled() {"persistent_accounting"} else {"memory"},"accounting_journal":self.vault.journal_status(),"initialized":self.vault.initialized(),"boot_id":self.vault.boot_id,
            "in_flight":self.global.used(),
            "models":groups.iter().map(|g| json!({"model":g.config.name,"deployments":g.backends.iter().map(|b| b.status()).collect::<Vec<_>>()})).collect::<Vec<_>>()})
    }

    fn route_groups(&self, alias: &str) -> Vec<&Group> {
        fn walk<'a>(
            engine: &'a Engine,
            name: &str,
            groups: &mut Vec<&'a Group>,
            seen: &mut HashSet<String>,
        ) {
            if !seen.insert(name.into()) {
                return;
            }
            if let Some(group) = engine.groups.get(name) {
                groups.push(group);
                for fallback in &group.config.fallbacks {
                    walk(engine, fallback, groups, seen);
                }
            }
        }
        let mut groups = Vec::new();
        walk(self, alias, &mut groups, &mut HashSet::new());
        groups
    }

    pub(crate) fn model_allowed(&self, account: usize, pool: &str, model: &str) -> bool {
        // A merchant's pool policy applies to catalogs, automatic candidates,
        // explicit selection and every account in the pool, before dispatch.
        if !self.groups.get(pool).is_some_and(|group| {
            group
                .config
                .allowed_models
                .as_ref()
                .is_none_or(|ids| ids.iter().any(|id| id == model))
        }) {
            return false;
        }
        let rules = &self.accounts[account].config.models;
        rules.is_empty()
            || rules.iter().any(|rule| {
                // ACL pool prefixes belong to the permission syntax, never to the
                // caller's model. Otherwise model="allowed-pool/foo" could bypass
                // a pool restriction while selecting a different pool by header.
                if let Some((scope, id)) = rule.split_once('/')
                    && self.groups.contains_key(scope)
                {
                    scope == pool && (id == "*" || id == model)
                } else {
                    rule == model
                }
            })
    }

    async fn select_pool(
        &self,
        account: usize,
        protocol: Protocol,
        model: &str,
        body: &Value,
        headers: &mut HeaderMap,
        deadline: Instant,
        agent: crate::agent::Agent,
    ) -> Result<&str, ApiError> {
        if let Some(pool) = headers.remove("x-router-pool") {
            let pool = pool
                .to_str()
                .ok()
                .filter(|s| config::valid_name(s))
                .ok_or_else(|| ApiError::new(400, "invalid_pool", "Invalid x-router-pool"))?;
            let group = self
                .groups
                .get(pool)
                .ok_or_else(|| ApiError::new(404, "pool_not_found", "Unknown router pool"))?;
            if !group
                .backends
                .iter()
                .any(|b| agent.allows(b.config.provider))
            {
                return Err(agent.rejection());
            }
            return Ok(group.config.name.as_str());
        }
        // Automatic routing follows configured pool order, using authenticated
        // evidence rather than model-name guesses or requiring a router header.
        let eligible: Vec<_> = self
            .config
            .models
            .iter()
            .filter_map(|g| self.groups.get(&g.name))
            .filter(|g| g.backends.iter().any(|b| agent.allows(b.config.provider)))
            .filter(|g| {
                self.model_allowed(account, &g.config.name, model)
                    && g.backends
                        .iter()
                        .any(|b| protocol.supports(b.config.provider))
            })
            .collect();
        if eligible.len() == 1 {
            return Ok(&eligible[0].config.name);
        }
        if eligible.is_empty() {
            if self
                .groups
                .values()
                .any(|g| self.model_allowed(account, &g.config.name, model))
                && !self.groups.values().any(|g| {
                    self.model_allowed(account, &g.config.name, model)
                        && g.backends.iter().any(|b| agent.allows(b.config.provider))
                })
            {
                return Err(agent.rejection());
            }
            if self
                .groups
                .values()
                .any(|g| self.model_allowed(account, &g.config.name, model))
            {
                return Err(ApiError::new(
                    400,
                    "unsupported_protocol",
                    "No authorized deployment supports this API protocol",
                ));
            }
            return Err(ApiError::new(
                403,
                "model_forbidden",
                "No authorized pool supports this protocol",
            ));
        }
        let catalogs = eligible
            .iter()
            .flat_map(|g| self.route_groups(&g.config.name))
            .filter(|g| self.model_allowed(account, &g.config.name, model))
            .flat_map(|g| &g.backends)
            .filter(|b| agent.allows(b.config.provider))
            .filter(|b| protocol.supports(b.config.provider))
            .filter_map(|b| b.catalog.clone())
            .collect();
        timeout_at(deadline, crate::discovery::refresh_all(catalogs, false))
            .await
            .map_err(|_| {
                ApiError::new(
                    504,
                    "request_timeout",
                    "Official catalog refresh exceeded request deadline",
                )
            })?;
        let mut matching = Vec::new();
        let mut last_error = ApiError::new(
            404,
            "model_not_found",
            "Model is absent from authorized official catalogs",
        );
        let rank = |status| match status {
            503 => 4,
            403 => 3,
            400 => 2,
            _ => 1,
        };
        for group in eligible {
            let mut accepts = false;
            for backend in self
                .route_groups(&group.config.name)
                .iter()
                .filter(|g| self.model_allowed(account, &g.config.name, model))
                .flat_map(|g| &g.backends)
                .filter(|b| agent.allows(b.config.provider))
                .filter(|b| protocol.supports(b.config.provider))
            {
                match backend.validate_model(protocol, model, body, &self.config.restrictions) {
                    Ok(()) => accepts = true,
                    Err(error) => {
                        if rank(error.status) >= rank(last_error.status) {
                            last_error = error;
                        }
                    }
                }
            }
            if accepts {
                matching.push(group);
            }
        }
        if matching.is_empty() {
            return Err(last_error);
        }
        let groups: Vec<_> = matching
            .iter()
            .flat_map(|g| self.route_groups(&g.config.name))
            .filter(|g| self.model_allowed(account, &g.config.name, model))
            .collect();
        // Resolve account/session ownership across all matching pools before
        // selecting one, so automatic routing cannot guess another account.
        let affinity = affinity::Affinity::new(self, account, &groups, body, &mut headers.clone())?;
        for group in matching {
            if self
                .route_groups(&group.config.name)
                .iter()
                .flat_map(|g| &g.backends)
                .any(|b| {
                    protocol.supports(b.config.provider)
                        && agent.allows(b.config.provider)
                        && affinity
                            .pinned
                            .as_ref()
                            .is_none_or(|p| Backend::binding(&b.config) == *p)
                        && b.validate_model(protocol, model, body, &self.config.restrictions)
                            .is_ok()
                })
            {
                return Ok(&group.config.name);
            }
        }
        Err(ApiError::new(
            409,
            "session_account_unavailable",
            "Original session account cannot serve this model and reasoning choice",
        ))
    }

    pub(crate) async fn execute(
        &self,
        account: usize,
        protocol: Protocol,
        alias: &str,
        body: Value,
        headers: HeaderMap,
        request_id: String,
        agent: crate::agent::Agent,
    ) -> Result<Routed, ApiError> {
        self.execute_admitted(
            account, protocol, alias, body, headers, request_id, None, None, agent,
        )
        .await
    }

    pub(crate) fn admit_external(
        &self,
        account: usize,
        request_id: String,
        model: String,
    ) -> Result<Ticket, ApiError> {
        self.accounts[account].admit(
            self.ledger.clone(),
            self.global.clone(),
            self.config.server.max_concurrent,
            request_id,
            model,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_admitted(
        &self,
        account: usize,
        protocol: Protocol,
        alias: &str,
        body: Value,
        mut headers: HeaderMap,
        request_id: String,
        mut parent: Option<Ticket>,
        mut route_observation: Option<crate::merchants::route_metrics::Observation>,
        agent: crate::agent::Agent,
    ) -> Result<Routed, ApiError> {
        let reject = |parent: &mut Option<Ticket>, error: ApiError| {
            if let Some(parent) = parent.as_mut() {
                parent.finish(error.status);
            }
            error
        };
        if !config::valid_model_id(alias) {
            return Err(reject(
                &mut parent,
                ApiError::new(400, "invalid_model", "Invalid model ID"),
            ));
        }
        let deadline =
            Instant::now() + Duration::from_secs(self.config.server.request_timeout_secs);
        let pool = self
            .select_pool(
                account,
                protocol,
                alias,
                &body,
                &mut headers,
                deadline,
                agent,
            )
            .await
            .map_err(|e| reject(&mut parent, e))?;
        if !self.model_allowed(account, pool, alias) {
            return Err(reject(
                &mut parent,
                ApiError::new(
                    403,
                    "model_forbidden",
                    "Model is not permitted by the API key or pool model list",
                ),
            ));
        }
        if !self.storage_ready() {
            return Err(reject(
                &mut parent,
                ApiError::new(503, "storage_capacity", "Runtime state is unavailable"),
            ));
        }
        // A priced merchant request is confined to its selected pool. Fallbacks
        // must not silently change the pool after pricing or affinity checks.
        let groups: Vec<_> = if parent.is_some() {
            self.groups.get(pool).into_iter().collect()
        } else {
            self.route_groups(pool)
                .into_iter()
                .filter(|g| self.model_allowed(account, &g.config.name, alias))
                .filter(|g| g.backends.iter().any(|b| agent.allows(b.config.provider)))
                .collect()
        };
        let Some(primary) = groups.first() else {
            return Err(reject(
                &mut parent,
                ApiError::new(404, "model_not_found", "Unknown model alias"),
            ));
        };
        if !groups.iter().any(|g| {
            g.backends
                .iter()
                .any(|b| protocol.supports(b.config.provider))
        }) {
            return Err(reject(
                &mut parent,
                ApiError::new(
                    400,
                    "unsupported_protocol",
                    "No enabled deployment supports this API protocol",
                ),
            ));
        }
        let cache_request = cache::Request::new(
            &self.accounts[account].config.user_id,
            protocol,
            alias,
            &body,
            &headers,
        );
        let mut affinity = affinity::Affinity::new(self, account, &groups, &body, &mut headers)
            .map_err(|e| reject(&mut parent, e))?;
        let mut ticket = self.accounts[account]
            .admit(
                self.ledger.clone(),
                self.global.clone(),
                self.config.server.max_concurrent,
                request_id,
                alias.into(),
            )
            .map_err(|e| reject(&mut parent, e))?;
        if let Some(parent) = parent {
            ticket.context = parent.context.clone();
            ticket.billing = parent.billing.clone();
            ticket.parent = Some(Box::new(parent));
        }
        let catalogs = groups
            .iter()
            .flat_map(|g| &g.backends)
            .filter(|b| {
                protocol.supports(b.config.provider)
                    && agent.allows(b.config.provider)
                    && affinity
                        .pinned
                        .as_ref()
                        .is_none_or(|s| Backend::binding(&b.config) == *s)
            })
            .filter_map(|b| b.catalog.clone())
            .collect();
        if timeout_at(deadline, crate::discovery::refresh_all(catalogs, false))
            .await
            .is_err()
        {
            ticket.finish(504);
            return Err(ApiError::new(
                504,
                "request_timeout",
                "Official catalog refresh exceeded request deadline",
            ));
        }
        let streaming = body["stream"] == true;
        let mut tried = HashSet::new();
        let mut last_error = ApiError::new(
            503,
            "no_available_deployment",
            "All deployments are busy, rate limited or cooling down",
        );
        for attempt in 0..primary.config.max_attempts {
            let mut eligible = HashSet::new();
            let mut catalog_error: Option<ApiError> = None;
            for backend in groups.iter().flat_map(|g| &g.backends).filter(|b| {
                protocol.supports(b.config.provider)
                    && agent.allows(b.config.provider)
                    && affinity
                        .pinned
                        .as_ref()
                        .is_none_or(|s| Backend::binding(&b.config) == *s)
            }) {
                match backend.validate_model(protocol, alias, &body, &self.config.restrictions) {
                    Ok(()) => {
                        eligible.insert(backend.config.id.clone());
                    }
                    Err(error) => {
                        // Missing evidence takes precedence over an account-local absence.
                        let rank = |status| match status {
                            503 => 4,
                            403 => 3,
                            400 => 2,
                            _ => 1,
                        };
                        if catalog_error
                            .as_ref()
                            .is_none_or(|old| rank(error.status) > rank(old.status))
                        {
                            catalog_error = Some(error);
                        }
                    }
                }
            }
            if eligible.is_empty() {
                if let Some(error) = catalog_error {
                    last_error = error;
                }
                break;
            }
            let select = |tried: &HashSet<String>| {
                self.cache.select(&cache_request, |sticky| {
                    let preferred = route_observation
                        .as_ref()
                        .and_then(|o| o.preferred_account())
                        .or(sticky);
                    let acquire = |binding| {
                        groups
                            .iter()
                            .find_map(|g| g.select(protocol, tried, binding, &eligible))
                    };
                    if let Some(hard) = affinity.pinned.as_deref() {
                        acquire(Some(hard))
                    } else {
                        preferred
                            .and_then(|p| acquire(Some(p)))
                            .or_else(|| acquire(None))
                    }
                })
            };
            let mut selected = select(&tried);
            if selected.is_none() && !tried.is_empty() {
                tried.clear();
                selected = select(&tried);
            }
            let Some(lease) = selected else {
                break;
            };
            if attempt > 0 {
                let delay = Duration::from_millis(
                    primary
                        .config
                        .retry_delay_ms
                        .saturating_mul(1 << (attempt - 1).min(5)),
                );
                if timeout_at(deadline, tokio::time::sleep(delay))
                    .await
                    .is_err()
                {
                    last_error =
                        ApiError::new(504, "request_timeout", "Router request deadline exceeded");
                    break;
                }
            }
            let backend = &lease.backend;
            if let Err(error) =
                backend.validate_model(protocol, alias, &body, &self.config.restrictions)
            {
                last_error = error;
                continue;
            }
            if let Err(error) = affinity.select(backend) {
                ticket.finish(error.status);
                return Err(error);
            }
            tried.insert(backend.config.id.clone());
            if ticket.attempts == 0 {
                ticket.begin_request(pool, backend.config.provider)?;
            }
            ticket.backend = Some(backend.clone());
            ticket.attempts += 1;
            ticket.begin_billing_attempt();
            let observer = crate::refusal::Observer::new(backend.config.provider);
            ticket.refusal = Some(observer.clone());
            let mut request_client = backend.client.clone();
            request_client.preserve_agent_headers(&headers);
            request_client.refusal = Some(observer.clone());
            request_client.billing = ticket.billing.clone();
            request_client.activity = ticket.activity();
            let result = timeout_at(deadline, async {
                let output = upstream::call(
                    &request_client,
                    protocol,
                    alias,
                    body.clone(),
                    streaming,
                    headers.clone(),
                )
                .await?;
                match output {
                    Output::Stream(mut stream) => {
                        // Until the first frame is available, no response has been exposed to the client.
                        let first = stream
                            .next()
                            .await
                            .ok_or(Error::InvalidResponse("empty stream"))??;
                        Ok((Output::Stream(stream), Some(first)))
                    }
                    output => Ok((output, None)),
                }
            })
            .await;
            match result {
                Ok(Ok((Output::Json(value), _))) => {
                    ticket.tokens.observe(protocol, &value);
                    if value["status"] != "incomplete" && value["status"] != "failed" {
                        self.cache.complete(&cache_request, backend, ticket.tokens);
                    }
                    if let Err(error) = affinity.response(protocol, &value, backend) {
                        ticket.finish(error.status);
                        return Err(error);
                    }
                    ticket.finish(200);
                    lease.success();
                    if let Some(observation) = &mut route_observation {
                        observation.nonstream();
                    }
                    return Ok(Routed {
                        deployment: backend.config.id.clone(),
                        attempts: ticket.attempts,
                        output: Output::Json(value),
                    });
                }
                Ok(Ok((Output::Stream(stream), first))) => {
                    if let Some(value) = first.as_ref().and_then(|f| f.value.as_ref())
                        && let Err(error) = affinity.response(protocol, value, backend)
                    {
                        ticket.finish(error.status);
                        return Err(error);
                    }
                    let deployment = backend.config.id.clone();
                    let attempts = ticket.attempts;
                    let output = Output::Stream(forward_stream(StreamState {
                        stream,
                        first,
                        ticket,
                        lease,
                        deadline,
                        protocol,
                        ended: false,
                        affinity,
                        cache: self.cache.clone(),
                        cache_request,
                        cache_ok: true,
                        route_observation,
                    }));
                    return Ok(Routed {
                        deployment,
                        attempts,
                        output,
                    });
                }
                Ok(Err(error)) => {
                    if matches!(
                        &error,
                        Error::OAuth(_) | Error::InvalidRequest(_) | Error::Unsupported(_)
                    ) || matches!(&error, Error::Http(e) if e.is_connect())
                    {
                        if let Some(billing) = &ticket.billing {
                            billing.no_generation();
                        }
                    }
                    self.cache.failed(&cache_request, backend);
                    if matches!(error, Error::Api { status: 401, .. })
                        && let Some(account) = &backend.client.oauth
                    {
                        account.rejected();
                    }
                    last_error = if observer.detected() {
                        ApiError::new(
                            403,
                            "upstream_refusal",
                            "Upstream declined the request under its content policy",
                        )
                    } else {
                        ApiError::upstream(&error)
                    };
                    backend.diagnostic(last_error.code);
                    ticket.finish_billing_attempt(last_error.status);
                    let retry = !observer.detected() && retryable(&error);
                    if retry {
                        lease.failure(
                            last_error.retry_after,
                            matches!(
                                error,
                                Error::Api {
                                    status: 401 | 402 | 403 | 404 | 429,
                                    ..
                                }
                            ),
                        );
                    }
                    if !retry {
                        break;
                    }
                }
                Err(_) => {
                    self.cache.failed(&cache_request, backend);
                    lease.failure(None, false);
                    last_error =
                        ApiError::new(504, "request_timeout", "Router request deadline exceeded");
                    break;
                }
            }
        }
        ticket.finish(last_error.status);
        if let Some(observation) = &mut route_observation {
            observation.fail();
        }
        Err(last_error)
    }
}

fn retryable(error: &Error) -> bool {
    match error {
        Error::Api { status, .. } => {
            matches!(status, 401 | 402 | 403 | 404 | 408 | 409 | 429 | 500..=599)
        }
        Error::InvalidRequest(_) | Error::Unsupported(_) => false,
        _ => true,
    }
}

struct StreamState {
    cache_ok: bool,
    cache: Arc<cache::State>,
    cache_request: cache::Request,
    route_observation: Option<crate::merchants::route_metrics::Observation>,
    affinity: affinity::Affinity,
    stream: EventStream,
    first: Option<Frame>,
    ticket: Ticket,
    lease: Lease,
    deadline: Instant,
    protocol: Protocol,
    ended: bool,
}

impl StreamState {
    fn finish_success(&mut self) {
        if self.cache_ok {
            self.cache
                .complete(&self.cache_request, &self.lease.backend, self.ticket.tokens);
        }
        if let Some(observation) = &mut self.route_observation {
            observation.finish();
        }
        self.ticket.finish(200);
        self.lease.success();
        self.ended = true;
    }
}

fn forward_stream(state: StreamState) -> EventStream {
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            if state.ended {
                return None;
            }
            let next = if let Some(frame) = state.first.take() {
                Ok(Some(Ok(frame)))
            } else {
                timeout_at(state.deadline, state.stream.next()).await
            };
            match next {
                Ok(Some(Ok(frame))) => {
                    if let Some(value) = &frame.value {
                        state.cache_ok &= !matches!(
                            value["type"].as_str(),
                            Some("error" | "response.failed" | "response.incomplete")
                        ) && !value.get("error").is_some_and(|v| !v.is_null());
                        if let Some(observation) = &mut state.route_observation {
                            observation.value(value);
                        }
                        if state
                            .affinity
                            .response(state.protocol, value, &state.lease.backend)
                            .is_err()
                        {
                            state.ticket.finish(503);
                            if let Some(observation) = &mut state.route_observation {
                                observation.fail();
                            }
                            state.ended = true;
                            return Some((
                                Err(Error::InvalidResponse("cannot persist response affinity")),
                                state,
                            ));
                        }
                        state.ticket.tokens.observe(state.protocol, value);
                    }
                    // Settle before exposing the terminal frame: a client may close
                    // immediately after receiving it without polling the body again.
                    let terminal = match state.protocol {
                        Protocol::Chat => frame.data == "[DONE]",
                        Protocol::Messages => frame
                            .value
                            .as_ref()
                            .is_some_and(|v| v["type"] == "message_stop"),
                        Protocol::Responses => frame.value.as_ref().is_some_and(|v| {
                            matches!(
                                v["type"].as_str(),
                                Some("response.completed" | "response.incomplete")
                            )
                        }),
                    };
                    if terminal {
                        state.finish_success();
                    }
                    Some((Ok(frame), state))
                }
                Ok(None) => {
                    state.finish_success();
                    None
                }
                error => {
                    state
                        .cache
                        .failed(&state.cache_request, &state.lease.backend);
                    let timeout = error.is_err();
                    state.ticket.finish(if timeout { 504 } else { 502 });
                    if let Some(observation) = &mut state.route_observation {
                        observation.fail();
                    }
                    state.lease.failure(None, false);
                    state.ended = true;
                    let mut frame = Frame::json(
                        json!({"error":{"type":"upstream_error","code":if timeout {"request_timeout"} else {"stream_interrupted"},"message":"Upstream stream interrupted","request_id":state.ticket.request_id}}),
                    );
                    if state.protocol != Protocol::Chat {
                        frame.event = Some("error".into());
                    }
                    Some((Ok(frame), state))
                }
            }
        },
    ))
}

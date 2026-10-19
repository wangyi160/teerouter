use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::Provider;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Retired configuration accepted on import, never executed or exported.
    #[serde(default, skip_serializing)]
    pub moderation: Option<crate::moderation::Config>,
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub discovery: Discovery,
    #[serde(default)]
    pub operations: Operations,
    #[serde(default)]
    pub balance_concurrency: BalanceConcurrency,
    #[serde(default)]
    pub compatibility: std::collections::BTreeMap<String, crate::compatibility::Profile>,
    #[serde(default)]
    pub restrictions: Vec<crate::policy::Restriction>,
    #[serde(default)]
    pub users: Vec<User>,
    pub keys: Vec<ApiKey>,
    pub models: Vec<Model>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub bind: SocketAddr,
    #[serde(rename = "admin_key_ref", alias = "admin_key_env")]
    pub admin_key_env: Option<String>,
    pub proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub max_concurrent: usize,
    pub max_body_bytes: usize,
    pub request_timeout_secs: u64,
    pub body_timeout_secs: u64,
    pub max_ingress: usize,
    pub max_response_bytes: usize,
    pub max_event_bytes: usize,
    pub stream_idle_secs: u64,
    pub allow_empty_routes: bool,
}

/// Per-user concurrency caps selected from the remaining synchronized balance.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BalanceConcurrency {
    pub tiers: Vec<BalanceTier>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BalanceTier {
    pub up_to_microusd: u64,
    pub max_concurrent: usize,
}

impl Default for BalanceConcurrency {
    fn default() -> Self {
        Self {
            tiers: [(1_000_000, 1), (5_000_000, 2), (10_000_000, 4)]
                .into_iter()
                .map(|(up_to_microusd, max_concurrent)| BalanceTier {
                    up_to_microusd,
                    max_concurrent,
                })
                .collect(),
        }
    }
}

impl BalanceConcurrency {
    pub fn validate(&self) -> Result<(), String> {
        if self.tiers.is_empty()
            || self.tiers.len() > 5
            || self.tiers[0].max_concurrent != 1
            || self.tiers.iter().any(|t| {
                t.up_to_microusd == 0
                    || t.up_to_microusd > i64::MAX as u64
                    || !(1..=100_000).contains(&t.max_concurrent)
            })
            || self.tiers.windows(2).any(|pair| {
                pair[0].up_to_microusd >= pair[1].up_to_microusd
                    || pair[0].max_concurrent > pair[1].max_concurrent
            })
        {
            return Err("balance concurrency requires 1..5 ascending positive balance thresholds, nondecreasing concurrency caps, and a lowest cap of 1".into());
        }
        Ok(())
    }

    pub(crate) fn limit(&self, remaining: u64, configured: usize) -> usize {
        self.tiers
            .iter()
            .find(|t| remaining <= t.up_to_microusd)
            .map_or(configured, |t| configured.min(t.max_concurrent))
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:4000".parse().expect("constant address"),
            admin_key_env: None,
            proxy: None,
            no_proxy: None,
            max_concurrent: 256,
            max_body_bytes: 16 * 1024 * 1024,
            request_timeout_secs: 120,
            body_timeout_secs: 15,
            max_ingress: 256,
            max_response_bytes: 32 * 1024 * 1024,
            max_event_bytes: 2 * 1024 * 1024,
            stream_idle_secs: 60,
            allow_empty_routes: false,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Operations {
    pub maintenance_secs: u64,
    pub log_retention_days: u64,
    pub max_log_rows: u64,
    pub session_ttl_secs: u64,
    pub max_sessions: u64,
    pub refresh_ahead_secs: u64,
}
impl Default for Operations {
    fn default() -> Self {
        Self {
            maintenance_secs: 30,
            log_retention_days: 30,
            max_log_rows: 100_000,
            session_ttl_secs: 86400,
            max_sessions: 100_000,
            refresh_ahead_secs: 300,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub note: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_concurrent")]
    pub max_concurrent: usize,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKey {
    pub name: String,
    pub user_id: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(rename = "key_ref", alias = "key_env")]
    pub key_env: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub rpm: u64,
    #[serde(default)]
    pub tpm: u64,
    #[serde(default = "default_concurrent")]
    pub max_concurrent: usize,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    RoundRobin,
    LeastBusy,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub name: String,
    /// Pool type; absent only in legacy configurations awaiting classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    /// Native upstream IDs shared by every account in this pool. None keeps
    /// official discovery unrestricted; an empty list publishes no models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,
    #[serde(default)]
    // Legacy serialized field. Official discovery and admission are mandatory.
    pub discover: bool,
    #[serde(default = "default_strategy")]
    pub strategy: Strategy,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default = "default_attempts")]
    pub max_attempts: usize,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_ms: u64,
    pub deployments: Vec<Deployment>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub id: String,
    pub provider: Provider,
    #[serde(default)]
    pub model: String,
    #[serde(default, rename = "api_key_ref", alias = "api_key_env")]
    pub api_key_env: String,
    #[serde(default)]
    pub oauth_file: Option<PathBuf>,
    #[serde(default)]
    pub oauth_account: Option<String>,
    /// API-key deployments bind here; OAuth deployments bind on the stored account.
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub api_base: Option<String>,
    /// Accept legacy empty checkpoints; a configured project is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini_project: Option<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default = "one")]
    pub weight: u64,
    #[serde(default = "default_concurrent")]
    pub max_concurrent: usize,
    #[serde(default)]
    pub rpm: u64,
    #[serde(default = "default_failures")]
    pub failure_threshold: u64,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Discovery {
    pub refresh_secs: u64,
    pub max_stale_secs: u64,
    pub timeout_secs: u64,
    pub codex_client_version: String,
}
impl Default for Discovery {
    fn default() -> Self {
        Self {
            refresh_secs: 300,
            max_stale_secs: 86400,
            timeout_secs: 15,
            // Catalog protocol version, configurable without recompiling the router.
            // Official Codex release rust-v0.162.0, verified 2026-10-09.
            codex_client_version: "0.162.0".into(),
        }
    }
}

fn default_concurrent() -> usize {
    32
}
fn default_strategy() -> Strategy {
    Strategy::RoundRobin
}
fn default_attempts() -> usize {
    3
}
fn default_retry_delay() -> u64 {
    100
}
fn enabled() -> bool {
    true
}
fn one() -> u64 {
    1
}
fn default_failures() -> u64 {
    2
}
fn default_cooldown() -> u64 {
    30
}
fn default_timeout() -> u64 {
    90
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self =
            toml::from_str(text).map_err(|e| format!("invalid TOML configuration: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read configuration: {e}"))?;
        let mut config = Self::parse(&text)?;
        for group in &mut config.models {
            for deployment in &mut group.deployments {
                if let Some(account) = deployment.oauth_file.as_mut().filter(|p| p.is_relative()) {
                    *account = path.parent().unwrap_or(Path::new(".")).join(&*account);
                }
            }
        }
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        crate::policy::validate(&self.restrictions)?;
        let ops = &self.operations;
        if !(1..=3600).contains(&ops.maintenance_secs)
            || !(1..=3650).contains(&ops.log_retention_days)
            || !(100..=10_000_000).contains(&ops.max_log_rows)
            || !(60..=604800).contains(&ops.session_ttl_secs)
            || !(100..=1_000_000).contains(&ops.max_sessions)
            || !(30..=3600).contains(&ops.refresh_ahead_secs)
            || !(1..=100_000).contains(&self.server.max_ingress)
            || !(1024..=256 * 1024 * 1024).contains(&self.server.max_response_bytes)
            || !(128..=self.server.max_response_bytes).contains(&self.server.max_event_bytes)
            || !(1..=3600).contains(&self.server.stream_idle_secs)
        {
            return Err("invalid operational limits".into());
        }
        for (provider, profile) in &self.compatibility {
            profile.validate(provider)?;
        }
        if !(1..=86400).contains(&self.discovery.refresh_secs)
            || !(self.discovery.refresh_secs..=604800).contains(&self.discovery.max_stale_secs)
            || !(1..=120).contains(&self.discovery.timeout_secs)
            || self.discovery.codex_client_version.is_empty()
            || self.discovery.codex_client_version.len() > 32
            || !self
                .discovery
                .codex_client_version
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.')
        {
            return Err("invalid discovery settings".into());
        }
        if self.server.allow_empty_routes && self.server.admin_key_env.is_none() {
            return Err("empty routes require an admin key".into());
        }
        self.balance_concurrency.validate()?;
        if !self.server.allow_empty_routes && (self.keys.is_empty() || self.models.is_empty()) {
            return Err("at least one API key and model group is required".into());
        }
        if self.server.max_concurrent == 0
            || self.server.max_concurrent > 1_000_000
            || self.server.max_body_bytes == 0
            || self.server.request_timeout_secs == 0
            || self.server.request_timeout_secs > 3600
            || self.server.body_timeout_secs == 0
            || self.server.body_timeout_secs > 300
        {
            return Err("invalid server concurrency, body limit or timeout".into());
        }
        let mut names = HashSet::new();
        let mut ids = HashSet::new();
        for group in &self.models {
            if let Some(first) = group.deployments.first()
                && group
                    .deployments
                    .iter()
                    .any(|d| d.provider != first.provider)
            {
                return Err("all accounts in a pool must use the same provider".into());
            }
            if let Some(models) = &group.allowed_models {
                let mut unique = HashSet::new();
                if models.len() > 4096
                    || models
                        .iter()
                        .any(|id| !valid_model_id(id) || !unique.insert(id))
                {
                    return Err("pool allowed_models must contain at most 4096 unique, valid native model IDs".into());
                }
            }
            let has_test = group.deployments.iter().any(|d| d.provider.is_local_test());
            if has_test
                && group
                    .deployments
                    .iter()
                    .any(|d| !d.provider.is_local_test())
            {
                return Err("test and official deployments cannot share a pool".into());
            }
            for fallback in &group.fallbacks {
                if let Some(target) = self.models.iter().find(|g| &g.name == fallback) {
                    let target_test = target
                        .deployments
                        .iter()
                        .any(|d| d.provider.is_local_test())
                        || target.provider.is_some_and(|p| p.is_local_test());
                    let source_test = has_test || group.provider.is_some_and(|p| p.is_local_test());
                    if source_test != target_test {
                        return Err("test services cannot be an official pool fallback".into());
                    }
                }
            }
            if !valid_name(&group.name) || !names.insert(group.name.as_str()) {
                return Err("model aliases must be unique and contain only letters, digits, dot, underscore or hyphen".into());
            }
            if group.max_attempts == 0 || group.max_attempts > 16 || group.retry_delay_ms > 5000 {
                return Err("max_attempts must be 1..16 and retry_delay_ms <= 5000".into());
            }
            for deployment in &group.deployments {
                if group
                    .provider
                    .is_some_and(|provider| provider != deployment.provider)
                {
                    return Err("deployment provider must match its pool provider".into());
                }
                if deployment.gemini_project.is_some() {
                    return Err("Gemini projects are no longer supported".into());
                }
                if let Some(base) = &deployment.api_base {
                    crate::official::validate(base, Some(deployment.provider)).map_err(|_| {
                        format!(
                            "deployment {} requires an official HTTPS host",
                            deployment.id
                        )
                    })?;
                }
                if !valid_name(&deployment.id) || !ids.insert(deployment.id.as_str()) {
                    return Err("deployment IDs must be valid and globally unique".into());
                }
                if (!deployment.model.is_empty() && !valid_model_id(&deployment.model))
                    || (usize::from(!deployment.api_key_env.is_empty())
                        + usize::from(deployment.oauth_file.is_some())
                        + usize::from(deployment.oauth_account.is_some())
                        != 1)
                    || deployment
                        .oauth_account
                        .as_ref()
                        .is_some_and(|s| !valid_name(s))
                    || deployment
                        .oauth_file
                        .as_ref()
                        .is_some_and(|p| p.as_os_str().is_empty())
                    || deployment.weight == 0
                    || deployment.weight > 1000
                    || deployment.max_concurrent == 0
                    || deployment.max_concurrent > 1_000_000
                    || deployment.failure_threshold == 0
                    || deployment.cooldown_secs == 0
                    || deployment.timeout_secs == 0
                    || deployment.timeout_secs > 3600
                {
                    return Err(format!("invalid deployment settings: {}", deployment.id));
                }
            }
        }
        let mut limits = std::collections::HashMap::new();
        for d in self
            .models
            .iter()
            .flat_map(|g| &g.deployments)
            .filter(|d| d.enabled)
        {
            if let Some(account) = &d.oauth_account {
                let value = (
                    d.max_concurrent,
                    d.rpm,
                    d.failure_threshold,
                    d.cooldown_secs,
                );
                if limits
                    .insert((d.provider, account), value)
                    .is_some_and(|old| old != value)
                {
                    return Err("deployments of the same OAuth account must share concurrency, RPM and cooldown settings".into());
                }
            }
        }
        let mut users = HashSet::new();
        for user in &self.users {
            if !valid_name(&user.id)
                || !users.insert(user.id.as_str())
                || user.name.len() > 512
                || user.note.len() > 4096
                || user.max_concurrent == 0
                || user.max_concurrent > 100_000
            {
                return Err("invalid or duplicate user, or user concurrency".into());
            }
        }
        let mut key_names = HashSet::new();
        for key in &self.keys {
            if !valid_name(&key.name)
                || !users.contains(key.user_id.as_str())
                || !key_names.insert(&key.name)
                || key.max_concurrent == 0
                || key.max_concurrent > 1_000_000
                || key.key_env.is_empty()
            {
                return Err(
                    "invalid or duplicate API key name, environment reference or concurrency"
                        .into(),
                );
            }
            if key.models.iter().any(|name| {
                !valid_model_id(name)
                    && !names.contains(name.as_str())
                    && !name.split_once('/').is_some_and(|(pool, id)| {
                        (self.models.iter().any(|g| g.name == pool)
                            || (pool.starts_with("m_") && pool.contains('.') && valid_name(pool)))
                            && (id == "*" || valid_model_id(id))
                    })
            }) {
                return Err(format!("unknown model in allowlist for key {}", key.name));
            }
        }
        for group in &self.models {
            visit(self, &group.name, &mut HashSet::new())?;
        }
        if !self.server.allow_empty_routes
            && !self
                .models
                .iter()
                .any(|g| g.deployments.iter().any(|d| d.enabled))
        {
            return Err("at least one enabled deployment is required".into());
        }
        Ok(())
    }
}

pub(crate) fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
        && id
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn visit<'a>(config: &'a Config, name: &'a str, path: &mut HashSet<&'a str>) -> Result<(), String> {
    if !path.insert(name) {
        return Err(format!("fallback cycle at model {name}"));
    }
    let group = config
        .models
        .iter()
        .find(|g| g.name == name)
        .ok_or_else(|| format!("unknown fallback model {name}"))?;
    for fallback in &group.fallbacks {
        visit(config, fallback, path)?;
    }
    path.remove(name);
    Ok(())
}

pub(crate) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}

pub(crate) fn secret(
    name: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    lookup(name)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("missing stored secret {name}"))
}

fn default_enabled() -> bool {
    true
}

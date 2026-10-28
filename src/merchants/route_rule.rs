//! Versioned client routing constraints, evaluated inside the attested router.
use super::{routing::Selection, *};
use crate::router::{Engine, Protocol};
use reqwest::header::HeaderMap;

pub(crate) const HEADER: &str = "x-router-routing";
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Priority {
    #[default]
    Price,
    Speed,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Fixed,
    Auto,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rule {
    pub version: u32,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    pub max_multiplier_millionths: u64,
    #[serde(default)]
    pub priority: Priority,
}
fn invalid_rule() -> ApiError {
    ApiError::new(
        400,
        "invalid_routing_rule",
        "Invalid or unsupported X-Router-Routing rule",
    )
}
impl Rule {
    pub fn parse(headers: &HeaderMap) -> Result<Option<Self>, ApiError> {
        let values: Vec<_> = headers.get_all(HEADER).iter().collect();
        if values.is_empty() {
            return Ok(None);
        }
        if values.len() != 1 || values[0].as_bytes().len() > 2048 {
            return Err(invalid_rule());
        }
        if [
            "x-router-pool",
            "x-router-account",
            "x-router-price-version",
            "x-router-official-price-revision",
        ]
        .iter()
        .any(|name| headers.contains_key(*name))
        {
            return Err(ApiError::new(
                400,
                "routing_rule_conflict",
                "Routing rules cannot be combined with legacy pool, account or price selectors",
            ));
        }
        let rule: Self =
            serde_json::from_slice(values[0].as_bytes()).map_err(|_| invalid_rule())?;
        if rule.version != 1 || rule.max_multiplier_millionths > 1_000_000_000_000_000 {
            return Err(invalid_rule());
        }
        match (rule.mode, rule.pool.as_deref()) {
            (Mode::Auto, None) => (),
            (Mode::Fixed, Some(pool))
                if pool.len() <= 128
                    && pool.split_once('.').is_some_and(|(m, p)| {
                        m.starts_with("m_")
                            && crate::config::valid_name(m)
                            && crate::config::valid_name(p)
                    }) => {}
            _ => return Err(invalid_rule()),
        }
        Ok(Some(rule))
    }
}
pub(super) struct Choice {
    pub selection: Selection,
    pub pool: String,
    pub model: String,
    pub observation: super::route_metrics::Observation,
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn choose(
    runtime: &Runtime,
    root: &Engine,
    account: usize,
    protocol: Protocol,
    alias: &str,
    body: &Value,
    headers: &HeaderMap,
    rule: &Rule,
    agent: crate::agent::Agent,
) -> Result<Choice, ApiError> {
    agent.check_model(alias)?;
    let (merchant, model) = alias
        .split_once('/')
        .filter(|(m, _)| m.starts_with("m_"))
        .map_or((None, alias), |(m, id)| (Some(m), id));
    let (provider, model) = crate::provider::Provider::split_model(model);
    if !crate::config::valid_model_id(model) {
        return Err(invalid_rule());
    }
    let mut selections =
        super::routing::selections(runtime, &root.config.keys[account].user_id).await?;
    selections.retain(|s| {
        merchant.is_none_or(|m| s.item.id == m)
            && rule
                .pool
                .as_ref()
                .is_none_or(|p| p.starts_with(&format!("{}.", s.item.id)))
    });
    if let Some(fixed) = &rule.pool {
        for s in &selections {
            for p in &s.engine.config.models {
                if *fixed == format!("{}.{}", s.item.id, p.name) {
                    super::refusals::check(
                        runtime,
                        &s.item.id,
                        &root.config.keys[account].user_id,
                        &s.settings,
                    )?;
                    agent.check(p.provider.ok_or_else(invalid)?)?;
                }
            }
        }
    }
    // Stateful continuations must stay with their existing merchant/account. An
    // unknown response ID cannot be guessed by comparing prices across merchants.
    if rule.mode == Mode::Auto {
        let owners = selections
            .iter()
            .map(|s| s.engine.routing_context_owner(s.account, body, headers))
            .collect::<Result<Vec<_>, _>>()?;
        if owners.iter().filter(|owned| **owned).count() > 1 {
            return Err(ApiError::new(
                409,
                "routing_affinity_required",
                "Multiple merchants own this context; select its original pool",
            ));
        }
        if owners.iter().any(|owned| *owned) {
            let mut owners = owners.into_iter();
            selections.retain(|_| owners.next().unwrap());
        } else if body
            .get("previous_response_id")
            .is_some_and(|v| !v.is_null())
            || has_file(body)
        {
            return Err(ApiError::new(
                409,
                "routing_affinity_required",
                "Select the original pool for an unknown response or provider file",
            ));
        }
    }
    let mut rows = Vec::new();
    let mut over_limit = false;
    let mut refusal_blocked = false;
    let mut incompatible_model = false;
    let mut compatible_model = false;
    let cache_request = crate::router::cache::Request::new(
        &root.config.keys[account].user_id,
        protocol,
        model,
        body,
        headers,
    );
    for (index, selection) in selections.iter().enumerate() {
        let blocked = super::refusals::blocked(
            runtime,
            &selection.item.id,
            &root.config.keys[account].user_id,
            &selection.settings,
        )?;
        let candidates = selection
            .engine
            .routing_candidates(
                selection.account,
                protocol,
                model,
                body,
                headers,
                rule.pool
                    .as_deref()
                    .and_then(|p| p.split_once('.'))
                    .map(|(_, p)| p),
                agent,
            )
            .await?;
        for quote in crate::pricing::quotes(
            &selection.settings,
            &selection.engine.config,
            &selection.price_book,
        ) {
            let pool = &quote.price.pool;
            let qualified = format!("{}.{}", selection.item.id, pool);
            let pool_provider = selection
                .engine
                .config
                .models
                .iter()
                .find(|p| p.name == *pool)
                .and_then(|p| p.provider);
            if quote.price.model == model
                && super::routing::platform_allows(root, account, &qualified, model, pool_provider)
                && rule.pool.as_ref().is_none_or(|p| *p == qualified)
                && provider.is_none_or(|wanted| pool_provider == Some(wanted))
            {
                if pool_provider.is_some_and(|p| agent.allows(p)) {
                    compatible_model = true;
                } else {
                    incompatible_model = true;
                }
            }
            if provider.is_some_and(|wanted| pool_provider != Some(wanted))
                || !pool_provider.is_some_and(|p| agent.allows(p))
                || quote.price.model != model
                || !super::routing::platform_allows(root, account, &qualified, model, pool_provider)
                || rule.pool.as_ref().is_some_and(|p| *p != qualified)
                || quote
                    .price
                    .valid_until
                    .is_some_and(|t| t <= crate::oauth::tokens::now() as u64)
                || !candidates.iter().any(|c| c.0 == *pool)

            {
                continue;
            }
            if blocked {
                refusal_blocked = true;
                continue;
            }
            if quote.multiplier_millionths > rule.max_multiplier_millionths {
                over_limit = true;
                continue;
            }
            if !super::routing::merchant_ready(selection)? {
                continue;
            }
            let bindings = &candidates
                .iter()
                .find(|c| c.0 == *pool)
                .expect("eligible pool")
                .1;
            let Some(choice) = selection.engine.routing_cache_choice(
                &cache_request,
                body,
                bindings,
                &quote,
                (rule.priority == Priority::Speed).then_some(runtime.merchants.latency.as_ref()),
            ) else {
                continue;
            };
            let mut candidate = super::route_metrics::Candidate::new(
                qualified,
                model,
                protocol,
                quote.multiplier_millionths,
            );
            candidate.estimated_cost = Some(choice.cost);
            candidate.sticky = choice.sticky;
            candidate.binding = choice.binding;
            candidate.latency = choice.latency;
            rows.push((index, pool.clone(), candidate));
        }
    }
    let metrics = &runtime.merchants.routing;
    let session = cache_request.identity();
    let mut candidates: Vec<_> = rows.iter().map(|r| r.2.clone()).collect();
    metrics.prefer_session(&mut candidates, &session);
    let Some(selected) = metrics.select(&candidates, rule.priority, rule.mode == Mode::Auto) else {
        return Err(if incompatible_model && !compatible_model {
            agent.rejection()
        } else if refusal_blocked {
            super::refusals::rejection()
        } else if rule.mode == Mode::Fixed && over_limit {
            ApiError::new(
                403,
                "routing_multiplier_exceeded",
                "Selected pool exceeds the maximum multiplier",
            )
        } else {
            ApiError::new(
                503,
                "routing_no_matching_pool",
                "No authorized available pool satisfies the routing rule",
            )
        });
    };
    let (index, pool, candidate) = rows.swap_remove(selected);
    metrics.remember_session(session, candidate.pool.clone());
    Ok(Choice {
        selection: selections.swap_remove(index),
        pool,
        model: model.into(),
        observation: metrics.observe(candidate),
    })
}
fn has_file(value: &Value) -> bool {
    match value {
        Value::Object(o) => o.contains_key("file_id") || o.values().any(has_file),
        Value::Array(a) => a.iter().any(has_file),
        _ => false,
    }
}

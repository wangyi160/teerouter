//! One platform price book, merchant-owned pool multipliers, immutable request quotes.
use crate::{
    Provider,
    config::Config,
    error::ApiError,
    merchants::{LongContextPrice, Price, Settings, TokenRates},
    runtime::{Runtime, storage},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
mod litellm;
pub(crate) mod source;
#[cfg(test)]
mod tests;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Book {
    pub revision: u64,
    pub providers: Vec<ProviderBook>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderBook {
    pub provider: Provider,
    pub version: u64,
    pub source: String,
    pub checked_at: u64,
    pub models: Vec<ModelPrice>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelPrice {
    pub model: String,
    pub rates: TokenRates,
    #[serde(default)]
    pub long_context: Option<LongContextPrice>,
    #[serde(default)]
    pub valid_until: Option<u64>,
    #[serde(default, skip_serializing_if = "crate::billing::Rules::is_empty")]
    pub billing_rules: crate::billing::Rules,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PoolPrice {
    pub pool: String,
    pub multiplier_millionths: u64,
}
#[derive(Clone, Serialize)]
pub(crate) struct Quote {
    #[serde(flatten)]
    pub price: Price,
    pub basis: &'static str,
    pub multiplier_millionths: u64,
    pub official_price_revision: u64,
    pub official_version: u64,
    pub official_source: String,
    pub official_checked_at: u64,
    pub billing_rules: crate::billing::Rules,
}
fn invalid() -> ApiError {
    ApiError::new(
        400,
        "invalid_pricing",
        "Invalid official price book or pool multiplier",
    )
}
pub(crate) fn book(runtime: &Runtime) -> Result<Book, ApiError> {
    Ok(runtime
        .engine()
        .vault
        .lock()
        .map_err(storage)?
        .official_pricing
        .clone())
}
pub(crate) fn validate(book: &Book) -> Result<(), ApiError> {
    let mut providers = BTreeSet::new();
    if book.providers.len() > 4 {
        return Err(invalid());
    }
    for p in &book.providers {
        let source = reqwest::Url::parse(&p.source).ok();
        let source_ok = if p.provider.is_local_test() {
            p.source == "local-test:workbuddy"
        } else {
            self::source::github(&p.source).is_ok()
                || source.is_some_and(|u| {
                    u.scheme() == "https"
                        && u.username().is_empty()
                        && u.password().is_none()
                        && match p.provider {
                            Provider::OpenAI => matches!(
                                u.host_str(),
                                Some(
                                    "openai.com" | "platform.openai.com" | "developers.openai.com"
                                )
                            ),
                            Provider::Anthropic => matches!(
                                u.host_str(),
                                Some(
                                    "anthropic.com"
                                        | "www.anthropic.com"
                                        | "docs.anthropic.com"
                                        | "platform.claude.com"
                                        | "claude.com"
                                )
                            ),
                            Provider::XAI => matches!(u.host_str(), Some("x.ai" | "docs.x.ai")),
                            #[allow(unreachable_patterns)]
                            _ => false,
                        }
                })
        };
        if !source_ok
            || p.version == 0
            || p.checked_at == 0
            || p.models.len() > 8192
            || !providers.insert(p.provider.api_key_env())
        {
            return Err(invalid());
        }
        let mut models = BTreeSet::new();
        for m in &p.models {
            let rates_valid = |r: &TokenRates| {
                [
                    Some(r.input),
                    Some(r.output),
                    r.cache_read,
                    r.cache_write,
                    r.cache_write_hour,
                ]
                .into_iter()
                .flatten()
                .all(|n| n <= 1_000_000_000_000)
            };
            if !crate::config::valid_model_id(&m.model)
                || !crate::billing::validate(p.provider, &m.billing_rules)
                || !models.insert(&m.model)
                || !rates_valid(&m.rates)
                || m.long_context.as_ref().is_some_and(|t| {
                    t.above_input_tokens == 0
                        || t.above_input_tokens > 1_000_000_000
                        || !rates_valid(&t.rates)
                })
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}
pub(crate) fn validate_pools(settings: &Settings, config: &Config) -> Result<(), ApiError> {
    let mut pools = BTreeSet::new();
    if settings.pool_pricing.len() > 1024 {
        return Err(invalid());
    }
    for p in &settings.pool_pricing {
        if p.multiplier_millionths > 1_000_000_000_000_000
            || !pools.insert(&p.pool)
            || !config.models.iter().any(|m| m.name == p.pool)
        {
            return Err(invalid());
        }
    }
    Ok(())
}
pub(crate) fn quotes(settings: &Settings, config: &Config, book: &Book) -> Vec<Quote> {
    let mut result = vec![];
    for pool in &config.models {
        let Some(multiplier) = settings.pool_pricing.iter().find(|p| p.pool == pool.name) else {
            continue;
        };
        let Some(provider) = book
            .providers
            .iter()
            .find(|p| Some(p.provider) == pool.provider)
        else {
            continue;
        };
        for model in &provider.models {
            let r = &model.rates;
            result.push(Quote {
                price: Price {
                    pool: pool.name.clone(),
                    model: model.model.clone(),
                    input: r.input,
                    output: r.output,
                    cache_read: r.cache_read,
                    cache_write: r.cache_write,
                    cache_write_hour: r.cache_write_hour,
                    long_context: model.long_context.clone(),
                    valid_until: model.valid_until,
                },
                basis: "official_pool_multiplier",
                multiplier_millionths: multiplier.multiplier_millionths,
                official_price_revision: book.revision,
                official_version: provider.version,
                official_source: provider.source.clone(),
                official_checked_at: provider.checked_at,
                billing_rules: model.billing_rules.clone(),
            });
        }
    }
    result
}

// Convert an old per-model quote only when every rate (including context/cache)
// proves the same pool multiplier. Ambiguous free test books use neutral 1x.
#[cfg(test)]
pub(crate) fn migrate(
    settings: &mut Settings,
    config: &Config,
    book: &Book,
) -> Result<(), ApiError> {
    if settings.prices.is_empty() {
        return Ok(());
    }
    if !settings.pool_pricing.is_empty() {
        return Err(invalid());
    }
    let mut migrated = vec![];
    for pool in &config.models {
        let old: Vec<_> = settings
            .prices
            .iter()
            .filter(|p| p.pool == pool.name)
            .collect();
        if old.is_empty() {
            continue;
        }
        let provider = book
            .providers
            .iter()
            .find(|p| Some(p.provider) == pool.provider)
            .ok_or_else(|| {
                ApiError::new(
                    409,
                    "official_prices_required",
                    "Load the official price book before unlocking legacy prices",
                )
            })?;
        let mut multiplier = None;
        for price in old {
            let base = provider
                .models
                .iter()
                .find(|m| m.model == price.model)
                .ok_or_else(invalid)?;
            if price.valid_until != base.valid_until
                || price.long_context.is_some() != base.long_context.is_some()
            {
                return Err(invalid());
            }
            let retail = TokenRates {
                input: price.input,
                output: price.output,
                cache_read: price.cache_read,
                cache_write: price.cache_write,
                cache_write_hour: price.cache_write_hour,
            };
            let mut pairs = vec![(&retail, &base.rates)];
            if let (Some(a), Some(b)) = (&price.long_context, &base.long_context) {
                if a.above_input_tokens != b.above_input_tokens {
                    return Err(invalid());
                }
                pairs.push((&a.rates, &b.rates));
            }
            for (a, b) in pairs {
                for (a, b) in [
                    Some(a.input),
                    Some(a.output),
                    a.cache_read,
                    a.cache_write,
                    a.cache_write_hour,
                ]
                .into_iter()
                .zip([
                    Some(b.input),
                    Some(b.output),
                    b.cache_read,
                    b.cache_write,
                    b.cache_write_hour,
                ]) {
                    match (a, b) {
                        (None, None) | (Some(0), Some(0)) => (),
                        (Some(a), Some(b)) if b > 0 => {
                            let scaled = u128::from(a) * 1_000_000;
                            if scaled % u128::from(b) != 0 {
                                return Err(invalid());
                            }
                            let value =
                                u64::try_from(scaled / u128::from(b)).map_err(|_| invalid())?;
                            if multiplier.is_some_and(|m| m != value) {
                                return Err(invalid());
                            }
                            multiplier = Some(value);
                        }
                        _ => return Err(invalid()),
                    }
                }
            }
        }
        migrated.push(PoolPrice {
            pool: pool.name.clone(),
            multiplier_millionths: multiplier.unwrap_or(1_000_000),
        });
    }
    settings.pool_pricing = migrated;
    settings.prices.clear();
    validate_pools(settings, config)
}

pub(crate) async fn get(
    axum::extract::State(runtime): axum::extract::State<std::sync::Arc<Runtime>>,
) -> Result<axum::Json<Book>, ApiError> {
    book(&runtime).map(axum::Json)
}

/// Public list prices are independent of merchant inventory and model ACLs.
/// Authentication still requires an enabled platform user/key. Never expose
/// merchant multipliers, accounts or credentials through this endpoint.
pub(crate) async fn public_get(
    axum::extract::State(runtime): axum::extract::State<std::sync::Arc<Runtime>>,
) -> Result<axum::Json<Value>, ApiError> {
    let mut book = book(&runtime)?;
    book.providers.retain(|p| !p.provider.is_local_test());
    Ok(axum::Json(
        json!({"object":"price_book", "revision":book.revision,
        "currency":"USD", "unit":"micro_usd_per_million_tokens", "providers":book.providers,
        "billing_policy":{"version":3,"error_usage_chargeable":true,"attempts_billed_separately":true,
            "missing_usage":"estimate_failed_model_attempt_after_request_flush_or_processing_evidence",
            "incomplete_transmission_without_usage":"zero_charge",
            "upstream_rejection_without_usage":"zero_charge",
            "client_disconnect":"cancel_upstream_and_settle_observed_progress",
            "successful_missing_usage":"pending_no_estimate","estimator":"o200k_json_text_v1",
            "unknown_cache":"ordinary_input","hidden_output_estimated":false,
            "rounding":"ceil_once_per_request_to_micro_usd"}}),
    ))
}
pub(crate) async fn put(
    axum::extract::State(runtime): axum::extract::State<std::sync::Arc<Runtime>>,
    request: axum::extract::Request,
) -> Result<axum::Json<Value>, ApiError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Update {
        expected_revision: u64,
        providers: Vec<ProviderBook>,
    }
    let (_, body) = crate::server::admin::input(&runtime, request).await?;
    let input: Update = serde_json::from_value(body).map_err(|_| invalid())?;
    let _write = runtime.merchants.write.lock().await;
    let engine = runtime.engine();
    let mut data = engine.vault.lock().map_err(storage)?;
    if data.official_pricing.revision != input.expected_revision {
        return Err(ApiError::new(
            409,
            "price_revision_conflict",
            "Reload the official price book",
        ));
    }
    let next = Book {
        revision: input.expected_revision.checked_add(1).ok_or_else(invalid)?,
        providers: input.providers,
    };
    validate(&next)?;
    if data.pricing_source.enabled {
        let managed = |b: &Book| {
            let mut providers: Vec<_> = b
                .providers
                .iter()
                .filter(|p| !p.provider.is_local_test())
                .collect();
            providers.sort_by_key(|p| p.provider.api_key_env());
            serde_json::to_value(providers).unwrap()
        };
        if managed(&next) != managed(&data.official_pricing) {
            return Err(ApiError::new(
                409,
                "pricing_source_managed",
                "Prices are managed by the configured source; use pricing refresh",
            ));
        }
    }
    if data
        .official_pricing
        .providers
        .iter()
        .any(|old| !next.providers.iter().any(|p| p.provider == old.provider))
    {
        return Err(invalid());
    }
    for p in &next.providers {
        if let Some(old) = data
            .official_pricing
            .providers
            .iter()
            .find(|old| old.provider == p.provider)
            && (p.version < old.version
                || p.checked_at < old.checked_at
                || (p.version == old.version
                    && serde_json::to_value(p).unwrap() != serde_json::to_value(old).unwrap()))
        {
            return Err(invalid());
        }
    }
    data.official_pricing = next.clone();
    data.changed();
    Ok(axum::Json(json!(next)))
}

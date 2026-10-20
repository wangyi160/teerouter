//! Unified platform authentication followed by independent merchant admission.
use super::*;
use crate::{
    accounting::Ticket,
    resources::Permit,
    router::{Engine, Protocol, Routed},
};
use reqwest::header::HeaderMap;

pub(crate) struct Selection {
    pub item: Registration,
    pub live: Arc<Live>,
    pub engine: Arc<Engine>,
    pub account: usize,
    pub settings: Settings,
    pub price_book: Arc<crate::pricing::Book>,
}

pub(crate) async fn selections(runtime: &Runtime, user: &str) -> Result<Vec<Selection>, ApiError> {
    let _read = runtime.merchants.write.lock().await;
    let mut selected = Vec::new();
    let price_book = Arc::new(crate::pricing::book(runtime)?);
    for item in records(runtime)? {
        if !item.enabled || !item.users.contains(user) {
            continue;
        }
        let Ok(live) = live(runtime, &item.id) else {
            continue;
        };
        customers::ensure(&live, user).await?;
        let engine = live.runtime.engine();
        let Some(account) = engine
            .config
            .keys
            .iter()
            .position(|key| key.user_id == user && key.enabled)
        else {
            continue;
        };
        if engine.authorize(account).is_err() {
            continue;
        }
        let settings = live.settings.lock().unwrap().clone();
        selected.push(Selection {
            item,
            live,
            engine,
            account,
            settings,
            price_book: price_book.clone(),
        });
    }
    Ok(selected)
}
pub(crate) fn platform_allows(
    root: &Engine,
    account: usize,
    pool: &str,
    model: &str,
    provider: Option<crate::provider::Provider>,
) -> bool {
    let rules = &root.config.keys[account].models;
    let qualified_model = pool
        .split_once('.')
        .map(|(merchant, _)| format!("{merchant}/{model}"));
    rules.is_empty()
        || rules.iter().any(|r| {
            r == model
                || provider.is_some_and(|p| r == &format!("{}/{model}", p.model_namespace()))
                || r == pool
                || qualified_model.as_ref() == Some(r)
                || r == &format!("{pool}/{model}")
                || r == &format!("{pool}/*")
        })
}
pub(crate) use super::model_catalog::models;
pub(crate) async fn pools(
    runtime: &Runtime,
    root: &Engine,
    account: usize,
) -> Result<Vec<Value>, ApiError> {
    let mut rows = vec![];
    for s in selections(runtime, &root.config.keys[account].user_id).await? {
        let mut pools = s.engine.pools(s.account, &runtime.merchants.latency);
        for pool in pools["data"].as_array_mut().into_iter().flatten() {
            let name = pool["id"]
                .as_str()
                .or_else(|| pool["name"].as_str())
                .unwrap_or("")
                .to_owned();
            let qualified = format!("{}.{}", s.item.id, name);
            let rules = &root.config.keys[account].models;
            if !rules.is_empty()
                && !rules.iter().any(|r| {
                    r == &qualified
                        || r.starts_with(&format!("{qualified}/"))
                        || r.starts_with(&format!("{}/", s.item.id))
                        || crate::provider::Provider::split_model(r)
                            .0
                            .is_some_and(|provider| {
                                pool["provider"] == serde_json::to_value(provider).unwrap()
                            })
                        || (!r.contains('/')
                            && !r.starts_with("m_")
                            && !root.config.models.iter().any(|p| p.name == *r))
                })
            {
                continue;
            }
            pool["id"] = qualified.into();
            pool["merchant_id"] = s.item.id.clone().into();
            pool["merchant_name"] = s.item.name.clone().into();
            pool["pool_name"] = name.clone().into();
            pool["multiplier_millionths"] = json!(
                s.settings
                    .pool_pricing
                    .iter()
                    .find(|p| p.pool == name)
                    .map(|p| p.multiplier_millionths)
            );
            pool["price_version"] = s.settings.revision.into();
            pool["official_price_revision"] = s.price_book.revision.into();
            rows.push(pool.clone());
        }
    }
    Ok(rows)
}
fn tighter(a: u64, b: u64) -> u64 {
    if a == 0 {
        b
    } else if b == 0 {
        a
    } else {
        a.min(b)
    }
}
pub(super) fn merchant_ready(s: &Selection) -> Result<bool, ApiError> {
    if s.live.gate.used() >= s.settings.max_concurrent.min(s.item.max_concurrent) {
        return Ok(false);
    }
    let meter = s.live.meter.lock().unwrap();
    if meter.start.elapsed().as_secs() >= 60 {
        return Ok(true);
    }
    let rpm = tighter(s.settings.rpm, s.item.rpm);
    let tpm = tighter(s.settings.tpm, s.item.tpm);
    let total = s
        .engine
        .vault
        .lock()
        .map_err(storage)?
        .usage
        .values()
        .fold(0u64, |a, u| {
            a.saturating_add(u.input_tokens)
                .saturating_add(u.output_tokens)
        });
    Ok((rpm == 0 || meter.requests < rpm)
        && (tpm == 0 || total.saturating_sub(meter.initial_tokens) < tpm))
}

pub(crate) struct Dispatch {
    pub routed: Routed,
    pub permit: Permit,
    pub merchant: String,
    pub price_version: u64,
    pub pool: String,
    pub multiplier: u64,
    pub rule_applied: bool,
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute(
    runtime: &Runtime,
    root: Arc<Engine>,
    account: usize,
    protocol: Protocol,
    alias: &str,
    mut body: Value,
    mut headers: HeaderMap,
    request_id: String,
) -> Result<Dispatch, ApiError> {
    let legacy_model = alias
        .split_once('/')
        .filter(|(m, _)| m.starts_with("m_"))
        .map_or(alias, |(_, model)| model);
    let (provider, raw_model) = crate::provider::Provider::split_model(legacy_model);
    let rule = super::route_rule::Rule::parse(&headers)?;
    let chosen = if let Some(rule) = &rule {
        Some(
            super::route_rule::choose(
                runtime, &root, account, protocol, alias, &body, &headers, rule,
            )
            .await?,
        )
    } else {
        None
    };
    headers.remove(super::route_rule::HEADER);
    let mut observation = None;
    let selected_pool;
    let (selection, model) = if let Some(choice) = chosen {
        selected_pool = Some(choice.pool);
        observation = Some(choice.observation);
        (choice.selection, choice.model)
    } else {
        let qualified_pool = headers
            .get("x-router-pool")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let (merchant, model) = alias
            .split_once('/')
            .filter(|(id, _)| id.starts_with("m_"))
            .map(|(id, m)| {
                (
                    id.to_owned(),
                    crate::provider::Provider::split_model(m).1.to_owned(),
                )
            })
            .or_else(|| {
                qualified_pool
                    .as_ref()
                    .and_then(|p| p.split_once('.'))
                    .filter(|(id, _)| id.starts_with("m_"))
                    .map(|(id, _)| (id.to_owned(), raw_model.to_owned()))
            })
            .ok_or_else(|| {
                ApiError::new(
                    400,
                    "merchant_required",
                    "Select a merchant-qualified model or pool",
                )
            })?;
        let selection = selections(runtime, &root.config.keys[account].user_id)
            .await?
            .into_iter()
            .find(|s| s.item.id == merchant)
            .ok_or_else(forbidden)?;
        selected_pool = qualified_pool;
        (selection, model)
    };
    let merchant = selection.item.id.clone();
    let settings = selection.settings;
    let quotes = crate::pricing::quotes(&settings, &selection.engine.config, &selection.price_book);
    let pool = if rule.is_some() {
        selected_pool.expect("rule selected pool")
    } else if let Some(qualified) = selected_pool {
        let (id, pool) = qualified.split_once('.').ok_or_else(forbidden)?;
        if id != merchant {
            return Err(forbidden());
        }
        pool.to_owned()
    } else {
        let candidates: Vec<_> = selection
            .engine
            .config
            .models
            .iter()
            .filter(|p| provider.is_none_or(|wanted| p.provider == Some(wanted)))
            .filter(|p| {
                selection
                    .engine
                    .model_allowed(selection.account, &p.name, &model)
            })
            .filter(|p| {
                p.deployments
                    .iter()
                    .any(|d| d.enabled && protocol.supports(d.provider))
            })
            .filter(|p| {
                quotes
                    .iter()
                    .any(|q| q.price.pool == p.name && q.price.model == model)
            })
            .collect();
        if candidates.len() != 1 {
            return Err(ApiError::new(
                409,
                "merchant_pool_required",
                "Select a priced merchant pool explicitly",
            ));
        }
        candidates[0].name.clone()
    };
    let group = selection
        .engine
        .config
        .models
        .iter()
        .find(|p| p.name == pool)
        .ok_or_else(forbidden)?;
    if provider.is_some_and(|wanted| group.provider != Some(wanted))
        || !platform_allows(
            &root,
            account,
            &format!("{merchant}.{pool}"),
            &model,
            group.provider,
        )
    {
        return Err(forbidden());
    }
    if group.deployments.iter().any(|d| d.provider.is_local_test()) && !selection.item.allow_test {
        return Err(forbidden());
    }
    let price = quotes
        .iter()
        .find(|q| q.price.pool == pool && q.price.model == model)
        .ok_or_else(|| {
            ApiError::new(
                409,
                "merchant_price_missing",
                "Official model price and merchant pool multiplier are required",
            )
        })?;
    if rule
        .as_ref()
        .is_some_and(|r| price.multiplier_millionths > r.max_multiplier_millionths)
    {
        return Err(ApiError::new(
            403,
            "routing_multiplier_exceeded",
            "Selected pool exceeds the maximum multiplier",
        ));
    }
    if let Some(version) = headers.remove("x-router-price-version")
        && version.to_str().ok().and_then(|s| s.parse::<u64>().ok()) != Some(settings.revision)
    {
        return Err(ApiError::new(
            409,
            "price_changed",
            "Reload the merchant price before requesting",
        ));
    }
    if let Some(version) = headers.remove("x-router-official-price-revision")
        && version.to_str().ok().and_then(|s| s.parse::<u64>().ok())
            != Some(selection.price_book.revision)
    {
        return Err(ApiError::new(
            409,
            "price_changed",
            "Reload the official price book before requesting",
        ));
    }
    if price
        .price
        .valid_until
        .is_some_and(|t| t <= crate::oauth::tokens::now() as u64)
    {
        return Err(ApiError::new(
            409,
            "merchant_price_expired",
            "Merchant must renew the price for this model",
        ));
    }
    let permit = selection
        .live
        .gate
        .acquire(settings.max_concurrent.min(selection.item.max_concurrent))
        .ok_or_else(|| ApiError::limited("Merchant concurrency limit exceeded"))?;
    {
        let mut meter = selection.live.meter.lock().unwrap();
        let total_tokens = selection
            .engine
            .vault
            .lock()
            .map_err(storage)?
            .usage
            .values()
            .fold(0u64, |a, u| {
                a.saturating_add(u.input_tokens)
                    .saturating_add(u.output_tokens)
            });
        if meter.start.elapsed().as_secs() >= 60 {
            meter.start = Instant::now();
            meter.requests = 0;
            meter.initial_tokens = total_tokens;
        }
        let rpm = tighter(settings.rpm, selection.item.rpm);
        let tpm = tighter(settings.tpm, selection.item.tpm);
        if (rpm > 0 && meter.requests >= rpm)
            || (tpm > 0 && total_tokens.saturating_sub(meter.initial_tokens) >= tpm)
        {
            return Err(ApiError::limited("Merchant rate limit exceeded"));
        }
        meter.requests += 1;
    }
    let mut parent: Ticket = root.admit_external(account, request_id.clone(), alias.into())?;
    parent.billing = Some(crate::billing::Observer::new(
        group.provider.ok_or_else(invalid)?,
        &body,
        price.clone(),
    )?);
    parent.context = json!({"merchant_id":merchant,"merchant_price_version":settings.revision,"merchant_pricing":price});
    body["model"] = model.clone().into();
    headers.insert("x-router-pool", pool.parse().map_err(|_| invalid())?);
    let routed = selection
        .engine
        .execute_admitted(
            selection.account,
            protocol,
            &model,
            body,
            headers,
            request_id,
            Some(parent),
            observation,
        )
        .await?;
    Ok(Dispatch {
        routed,
        permit,
        merchant,
        price_version: settings.revision,
        pool: format!("{}.{}", selection.item.id, pool),
        multiplier: price.multiplier_millionths,
        rule_applied: rule.is_some(),
    })
}

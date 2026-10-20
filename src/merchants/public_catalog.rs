//! Read-only, anonymous-display inventory for the platform's public website.
//! The transport is admin authenticated; the response is an explicit allowlist.
use super::*;
use axum::{Json, extract::State};

pub(crate) async fn get(State(runtime): State<Arc<Runtime>>) -> Result<Json<Value>, ApiError> {
    let _read = runtime.merchants.write.lock().await;
    let book = crate::pricing::book(&runtime)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut pools = Vec::new();
    for merchant in records(&runtime)?.into_iter().filter(|m| m.enabled) {
        let Ok(live) = live(&runtime, &merchant.id) else {
            continue;
        };
        let engine = live.runtime.engine();
        let settings = live.settings.lock().unwrap().clone();
        let quotes = crate::pricing::quotes(&settings, &engine.config, &book);
        let merchant_limit = settings.max_concurrent.min(merchant.max_concurrent);
        let merchant_used = live.gate.used();
        let tighter = |a: u64, b: u64| {
            if a == 0 {
                b
            } else if b == 0 {
                a
            } else {
                a.min(b)
            }
        };
        let rpm = tighter(settings.rpm, merchant.rpm);
        let tpm = tighter(settings.tpm, merchant.tpm);
        let rate_limited = {
            let meter = live.meter.lock().unwrap();
            if meter.start.elapsed().as_secs() >= 60 {
                false
            } else {
                let total =
                    engine
                        .vault
                        .lock()
                        .map_err(storage)?
                        .usage
                        .values()
                        .fold(0u64, |a, u| {
                            a.saturating_add(u.input_tokens)
                                .saturating_add(u.output_tokens)
                        });
                (rpm > 0 && meter.requests >= rpm)
                    || (tpm > 0 && total.saturating_sub(meter.initial_tokens) >= tpm)
            }
        };
        for mut pool in engine.public_pool_inventory(&runtime.merchants.latency) {
            let name = pool["name"].as_str().unwrap().to_owned();
            pool["id"] = json!(format!("{}.{}", merchant.id, name));
            pool["merchant"] = json!({"id":merchant.id,"name":merchant.name});
            pool["price_version"] = json!(settings.revision);
            pool["multiplier_millionths"] = json!(
                settings
                    .pool_pricing
                    .iter()
                    .find(|p| p.pool == name)
                    .map(|p| p.multiplier_millionths)
            );
            pool["concurrency"]["limit"] = json!(
                pool["concurrency"]["limit"]
                    .as_u64()
                    .unwrap()
                    .min(merchant_limit as u64)
            );
            pool["concurrency"]["available"] = json!(
                pool["concurrency"]["available"]
                    .as_u64()
                    .unwrap()
                    .min(merchant_limit.saturating_sub(merchant_used) as u64)
            );
            pool["merchant_limits"] = json!({"max_concurrent":merchant_limit,"in_flight":merchant_used,
                "rpm":tighter(settings.rpm,merchant.rpm),"tpm":tighter(settings.tpm,merchant.tpm)});
            let mut priced = false;
            for model in pool["models"].as_array_mut().unwrap() {
                let quote = quotes
                    .iter()
                    .find(|q| q.price.pool == name && model["model"] == q.price.model);
                model["pricing"] = quote.map_or(Value::Null, |q| json!({
                    "input":q.price.input,"output":q.price.output,"cache_read":q.price.cache_read,
                    "cache_write":q.price.cache_write,"cache_write_hour":q.price.cache_write_hour,
                    "long_context":q.price.long_context,"valid_until":q.price.valid_until,
                    "billing_rules":q.billing_rules,"official_version":q.official_version,
                    "official_checked_at":q.official_checked_at,"official_source":q.official_source}));
                let valid = quote.is_some_and(|q| q.price.valid_until.is_none_or(|t| t > now));
                model["priced"] = json!(valid);
                priced |= valid;
            }
            if !priced || rate_limited {
                pool["concurrency"]["available"] = json!(0);
            }
            pool["available"] = json!(
                priced
                    && pool["available_accounts"].as_u64().unwrap() > 0
                    && pool["concurrency"]["available"].as_u64().unwrap() > 0
            );
            pools.push(pool);
        }
    }
    Ok(Json(
        json!({"version":1,"observed_at":now,"official_price_revision":book.revision,
        "currency":"USD","price_unit":"micro_usd_per_million_tokens","data":pools}),
    ))
}

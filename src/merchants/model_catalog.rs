//! One public model per official provider, with independently priced merchant pools.
use super::*;
use crate::{provider::Provider, router::Engine};

pub(crate) async fn models(
    runtime: &Runtime,
    root: &Engine,
    account: usize,
) -> Result<Vec<Value>, ApiError> {
    let mut result = BTreeMap::<String, Value>::new();
    for selection in routing::selections(runtime, &root.config.keys[account].user_id).await? {
        if refusals::blocked(
            runtime,
            &selection.item.id,
            &root.config.keys[account].user_id,
            &selection.settings,
        )? {
            continue;
        }
        let list = selection.engine.models(selection.account).await;
        let quotes = crate::pricing::quotes(
            &selection.settings,
            &selection.engine.config,
            &selection.price_book,
        );
        for row in list["data"].as_array().into_iter().flatten() {
            let model = row["id"].as_str().unwrap_or("");
            for cap in row["router_capabilities"].as_array().into_iter().flatten() {
                let (Some(pool), Ok(provider)) = (
                    cap["pool"].as_str(),
                    serde_json::from_value::<Provider>(cap["provider"].clone()),
                ) else {
                    continue;
                };
                let qualified = format!("{}.{}", selection.item.id, pool);
                if !routing::platform_allows(root, account, &qualified, model, Some(provider)) {
                    continue;
                }
                let id = format!("{}/{model}", provider.model_namespace());
                let out = result.entry(id.clone()).or_insert_with(|| {
                    json!({
                        "id":id,"object":"model","created":0,"owned_by":provider.model_namespace(),
                        "router_provider":provider,"router_model":model,
                        "router_pools":[],"router_capabilities":[],"router_merchants":[],
                        "router_pricing":{"official_price_revision":selection.price_book.revision,
                            "currency":"USD","unit":"micro_usd_per_million_tokens","rates":[]}
                    })
                });
                let mut capability = cap.clone();
                capability["pool"] = qualified.clone().into();
                out["router_capabilities"]
                    .as_array_mut()
                    .unwrap()
                    .push(capability);
                if out["router_pools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p == &qualified)
                {
                    continue;
                }
                out["router_pools"]
                    .as_array_mut()
                    .unwrap()
                    .push(qualified.clone().into());
                let merchants = out["router_merchants"].as_array_mut().unwrap();
                let index = merchants
                    .iter()
                    .position(|m| m["id"] == selection.item.id)
                    .unwrap_or_else(|| {
                        merchants.push(json!({"id":selection.item.id,"name":selection.item.name,
                            "price_version":selection.settings.revision,"pools":[]}));
                        merchants.len() - 1
                    });
                merchants[index]["pools"]
                    .as_array_mut()
                    .unwrap()
                    .push(qualified.clone().into());
                for quote in quotes
                    .iter()
                    .filter(|q| q.price.pool == pool && q.price.model == model)
                {
                    let mut rate = serde_json::to_value(quote).map_err(|_| invalid())?;
                    rate["pool"] = qualified.clone().into();
                    rate["merchant_id"] = selection.item.id.clone().into();
                    rate["price_version"] = selection.settings.revision.into();
                    out["router_pricing"]["rates"]
                        .as_array_mut()
                        .unwrap()
                        .push(rate);
                }
            }
        }
    }
    Ok(result.into_values().collect())
}

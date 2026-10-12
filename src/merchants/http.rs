use super::*;
use axum::{
    Json,
    extract::{Request, State},
    response::{IntoResponse, Response},
};

pub(crate) async fn platform_list(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    let items = records(&runtime)?;
    let live = runtime.merchants.live.lock().unwrap();
    Ok(Json(
        json!({"data":items.iter().map(|r|r.public(live.contains_key(&r.id))).collect::<Vec<_>>()}),
    ))
}
pub(crate) async fn platform_put(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (token, body) = crate::server::admin::input(&runtime, request).await?;
    let item: Registration = serde_json::from_value(body).map_err(|_| invalid())?;
    register(&runtime, item, &token).await.map(Json)
}
pub(crate) async fn dispatch(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Response, ApiError> {
    let token = zeroize::Zeroizing::new(
        crate::server::credential(request.headers())
            .unwrap_or("")
            .to_owned(),
    );
    let item = authenticate(&runtime, &token)?;
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query: BTreeMap<String, String> =
        reqwest::Url::parse(&format!("http://local{}", request.uri()))
            .map_err(|_| invalid())?
            .query_pairs()
            .into_owned()
            .collect();
    let number = |name: &str, default: u64| -> Result<u64, ApiError> {
        query
            .get(name)
            .map(|s| s.parse().map_err(|_| invalid()))
            .unwrap_or(Ok(default))
    };
    let result = match (method.as_str(), path.as_str()) {
        ("POST", "/merchant/unlock") => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Input {
                #[serde(default)]
                minimum_revision: u64,
            }
            let (_, body) = crate::server::admin::input(&runtime, request).await?;
            let input: Input = serde_json::from_value(body).map_err(|_| invalid())?;
            unlock(&runtime, &token, input.minimum_revision).await?
        }
        ("GET", "/merchant/state") => {
            let _read = runtime.merchants.write.lock().await;
            state(&runtime, &item)?
        }
        ("PUT", "/merchant/config") => {
            let (_, body) = crate::server::admin::input(&runtime, request).await?;
            apply(
                &runtime,
                &token,
                serde_json::from_value(body).map_err(|_| invalid())?,
            )
            .await?
        }
        ("POST", "/merchant/models/refresh") => {
            let live = live(&runtime, &item.id)?;
            live.runtime.engine().refresh_models(true).await;
            state(&runtime, &item)?
        }
        ("GET", "/merchant/usage") => {
            let live = live(&runtime, &item.id)?;
            let engine = live.runtime.engine();
            let data = engine.vault.lock().map_err(storage)?;
            let after = number("after_seq", 0)?;
            let limit = number("limit", 100)?.clamp(1, 1000) as usize;
            let rows: Vec<_> = data
                .events
                .iter()
                .filter(|v| v["seq"].as_u64().is_some_and(|seq| seq > after))
                .take(limit)
                .collect();
            json!({"data":rows,"last_seq":data.sequence,"instance_id":data.instance,"merchant_id":item.id,"usage":data.usage})
        }
        ("POST", "/merchant/usage/ack") => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Ack {
                instance_id: String,
                through_seq: u64,
            }
            let (_, body) = crate::server::admin::input(&runtime, request).await?;
            let input: Ack = serde_json::from_value(body).map_err(|_| invalid())?;
            let _write = runtime.merchants.write.lock().await;
            let live = live(&runtime, &item.id)?;
            let engine = live.runtime.engine();
            {
                let mut data = engine.vault.lock().map_err(storage)?;
                if data.instance != input.instance_id || input.through_seq > data.sequence {
                    return Err(invalid());
                }
                data.events
                    .retain(|v| v["seq"].as_u64().is_some_and(|seq| seq > input.through_seq));
                data.pending_event_bytes = data
                    .events
                    .iter()
                    .map(|e| serde_json::to_vec(e).map_or(0, |v| v.len()))
                    .sum();
                data.changed();
            }
            checkpoint_one(&runtime, &item.id)?;
            json!({"acknowledged":input.through_seq})
        }
        ("GET", "/merchant/refusals") => {
            let live = live(&runtime, &item.id)?;
            let engine = live.runtime.engine();
            let data = engine.vault.lock().map_err(storage)?;
            let mut page = data.refusal_queue.page(
                number("after_id", 0)?,
                number("limit", 100)?.clamp(1, 1000) as usize,
            );
            page["instance_id"] = data.instance.clone().into();
            page["merchant_id"] = item.id.clone().into();
            page
        }
        ("DELETE", "/merchant/refusals") => {
            let _write = runtime.merchants.write.lock().await;
            let live = live(&runtime, &item.id)?;
            let result = live
                .runtime
                .delete_refusals(number("through_id", 0)?, &live.admin)
                .await?;
            checkpoint_one(&runtime, &item.id)?;
            result
        }
        _ => {
            return Err(ApiError::new(
                404,
                "not_found",
                "Merchant endpoint not found",
            ));
        }
    };
    Ok(([("cache-control", "no-store")], Json(result)).into_response())
}

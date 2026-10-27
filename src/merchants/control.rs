//! Public configuration is platform mediated. Credentials travel only through
//! the owner-authenticated direct account sync channel.
use super::*;
use axum::{
    Json,
    extract::{Path, Request, State as AxumState},
};

#[derive(Default)]
pub(super) struct State {
    pub(super) owner_completed: std::collections::VecDeque<(String, String, u64)>,
    completed: std::collections::VecDeque<(String, String, u64)>,
}
fn conflict(code: &'static str) -> ApiError {
    ApiError::new(
        409,
        code,
        "Merchant configuration must be refreshed before retrying",
    )
}

// Validate the public boundary before deserialization; empty secret maps are
// forbidden too. Proxy values are always resolved inside the TEE.
fn public_update(value: &Value) -> Result<MerchantUpdate, ApiError> {
    let map = value.as_object().ok_or_else(invalid)?;
    if map.contains_key("accounts") || map.contains_key("secrets") {
        return Err(invalid());
    }
    let mut body = value.clone();
    let controls = body.as_object_mut().unwrap().remove("account_controls");
    let mut input: MerchantUpdate = serde_json::from_value(body).map_err(|_| invalid())?;
    if let Some(controls) = controls {
        for (name, control) in controls.as_object().ok_or_else(invalid)? {
            let control = control.as_object().ok_or_else(invalid)?;
            if control.len() != 1 || !control.get("enabled").is_some_and(Value::is_boolean) {
                return Err(invalid());
            }
            input.accounts.insert(name.clone(), json!(control));
        }
    }
    if input.compatibility.as_ref().is_some_and(|p| !p.is_empty()) {
        return Err(invalid());
    }
    for d in input.pools.iter().flat_map(|p| &p.deployments) {
        if d.proxy.as_deref().is_some_and(|p| p != "[redacted]")
            || d.oauth_file.is_some()
            || d.api_base.is_some()
            || d.gemini_project.is_some()
        {
            return Err(invalid());
        }
    }
    Ok(input)
}

/// The management platform receives only the public model/configuration view.
pub(super) fn view(runtime: &Runtime, item: &Registration) -> Result<Value, ApiError> {
    let mut value = state(runtime, item)?;
    value["compatibility"] = json!({});
    value["merchant"]["control_protocol"] = json!("platform-config-v1");
    value["merchant"]["account_sync_protocol"] = json!("direct-accounts-v1");
    // account_status contains only aliases, expiry, health and fingerprints.
    // status/catalog are Router-generated summaries, not raw provider replies.
    Ok(value)
}

pub(crate) async fn snapshots(
    AxumState(runtime): AxumState<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    let _read = runtime.merchants.write.lock().await;
    let mut data = Vec::new();
    let mut locked = Vec::new();
    for item in records(&runtime)?.into_iter().filter(|m| m.enabled) {
        if live(&runtime, &item.id).is_ok() {
            data.push(view(&runtime, &item)?);
        } else {
            locked.push(item.id);
        }
    }
    Ok(Json(json!({"data":data,"locked":locked})))
}

pub(crate) async fn snapshot(
    AxumState(runtime): AxumState<Arc<Runtime>>, Path(mid): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let _read = runtime.merchants.write.lock().await;
    let item = records(&runtime)?.into_iter().find(|m| m.id == mid && m.enabled).ok_or_else(forbidden)?;
    Ok(Json(view(&runtime, &item)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    operation_id: String,
    runtime_id: String,
    config: Value,
}

pub(crate) async fn put(
    AxumState(runtime): AxumState<Arc<Runtime>>,
    Path(mid): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (_, body) = crate::server::admin::input(&runtime, request).await?;
    execute(&runtime, &mid, body).await.map(Json)
}

pub(super) async fn execute(runtime: &Runtime, mid: &str, body: Value) -> Result<Value, ApiError> {
    let digest = crypto::digest(&serde_json::to_string(&body).map_err(|_| invalid())?);
    let command: Command = serde_json::from_value(body).map_err(|_| invalid())?;
    if command.operation_id.len() > 100
        || command.operation_id.is_empty()
        || !command
            .operation_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(invalid());
    }
    let input = public_update(&command.config)?;
    let _write = runtime.merchants.write.lock().await;
    let item = records(runtime)?
        .into_iter()
        .find(|m| m.id == mid && m.enabled)
        .ok_or_else(forbidden)?;
    let live = live(runtime, mid)?;
    if live
        .runtime
        .engine()
        .vault
        .lock()
        .map_err(storage)?
        .instance
        != command.runtime_id
    {
        return Err(conflict("merchant_runtime_changed"));
    }
    if let Some((_, old, revision)) = live
        .control
        .lock()
        .unwrap()
        .completed
        .iter()
        .find(|(id, _, _)| id == &command.operation_id)
    {
        if old != &digest {
            return Err(conflict("operation_conflict"));
        }
        return Ok(
            json!({"operation_id":command.operation_id,"applied_revision":revision,"state":view(runtime,&item)?}),
        );
    }
    apply_for(runtime, &item, input).await?;
    let revision = live.settings.lock().unwrap().revision;
    {
        let mut state = live.control.lock().unwrap();
        state
            .completed
            .push_back((command.operation_id.clone(), digest, revision));
        while state.completed.len() > 256 {
            state.completed.pop_front();
        }
    }
    Ok(
        json!({"operation_id":command.operation_id,"applied_revision":revision,"state":view(runtime,&item)?}),
    )
}

pub(crate) async fn receipt(
    AxumState(runtime): AxumState<Arc<Runtime>>,
    Path((mid, oid)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let _read = runtime.merchants.write.lock().await;
    let item = records(&runtime)?
        .into_iter()
        .find(|m| m.id == mid && m.enabled)
        .ok_or_else(forbidden)?;
    let live = live(&runtime, &mid)?;
    let revision = live
        .control
        .lock()
        .unwrap()
        .completed
        .iter()
        .find(|(id, _, _)| id == &oid)
        .map(|(_, _, r)| *r)
        .ok_or_else(|| ApiError::new(404, "operation_not_found", "No confirmed operation"))?;
    Ok(Json(
        json!({"operation_id":oid,"applied_revision":revision,"state":view(&runtime,&item)?}),
    ))
}

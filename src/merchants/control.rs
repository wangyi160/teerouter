//! Public configuration is platform mediated. Credential material is staged by
//! the TEE-authenticated owner and is never readable by the platform.
use super::*;
use axum::{
    Json,
    extract::{Path, Request, State as AxumState},
};

#[derive(Default)]
pub(super) struct State {
    prepared: BTreeMap<String, Prepared>,
    completed: std::collections::VecDeque<(String, String, u64)>,
}
struct Prepared {
    at: Instant,
    digest: String,
    material: zeroize::Zeroizing<String>,
    accounts: BTreeMap<String, Option<i64>>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Material {
    #[serde(default)]
    accounts: BTreeMap<String, Value>,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    #[serde(default)]
    proxies: BTreeMap<String, Option<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    runtime_id: String,
    expected_revision: u64,
    public_digest: String,
    material: Material,
}

fn conflict(code: &'static str) -> ApiError {
    ApiError::new(
        409,
        code,
        "Merchant configuration must be refreshed before retrying",
    )
}

pub(super) async fn prepare(
    runtime: &Runtime,
    token: &str,
    body: Value,
) -> Result<Value, ApiError> {
    let input: Prepare = serde_json::from_value(body).map_err(|_| invalid())?;
    if !crypto::valid_secret(&input.public_digest)
        || input.material.accounts.len() > 4096
        || input.material.secrets.len() > 4096
        || input.material.proxies.len() > 4096
    {
        return Err(invalid());
    }
    let _write = runtime.merchants.write.lock().await;
    let item = authenticate(runtime, token)?;
    let live = live(runtime, &item.id)?;
    let engine = live.runtime.engine();
    if engine.vault.lock().map_err(storage)?.instance != input.runtime_id {
        return Err(conflict("merchant_runtime_changed"));
    }
    if live.settings.lock().unwrap().revision != input.expected_revision {
        return Err(conflict("revision_conflict"));
    }
    // Validate imports in a throwaway vault, including rejection of refresh
    // fields. Staging has no effect on the currently serving credentials.
    let snapshot = engine.vault.snapshot().map_err(storage)?;
    let candidate = snapshot
        .updated(&Update {
            accounts: input.material.accounts.clone(),
            secrets: input.material.secrets.clone(),
            ..Update::default()
        })
        .map_err(|_| invalid())?;
    for (name, patch) in &input.material.accounts {
        if patch
            .as_object()
            .ok_or_else(invalid)?
            .keys()
            .any(|k| !matches!(k.as_str(), "provider" | "credential" | "proxy"))
        {
            return Err(invalid());
        }
        let account = candidate.accounts.get(name).ok_or_else(invalid)?;
        Vault::memory()
            .map_err(storage)?
            .import_account(name, account.provider, &account.credential)
            .map_err(|_| {
                ApiError::new(
                    400,
                    "access_token_only",
                    "Only access-token credentials are accepted",
                )
            })?;
    }
    let raw = serde_json::to_string(&input.material).map_err(|_| invalid())?;
    if raw.len() > 12 * 1024 * 1024 {
        return Err(invalid());
    }
    let runtimes: Vec<_> = runtime
        .merchants
        .live
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    let mut bytes = 0usize;
    for runtime in runtimes {
        let mut pending = runtime.control.lock().unwrap();
        pending
            .prepared
            .retain(|_, p| p.at.elapsed().as_secs() < 600);
        bytes += pending
            .prepared
            .values()
            .map(|p| p.material.len())
            .sum::<usize>();
    }
    let mut state = live.control.lock().unwrap();
    if state.prepared.len() >= 4 {
        // A fresh owner edit supersedes the oldest uncommitted draft. Bound
        // memory without locking the owner out after several rejected edits.
        let oldest = state
            .prepared
            .iter()
            .min_by_key(|(_, p)| p.at)
            .map(|(k, _)| k.clone())
            .unwrap();
        bytes -= state.prepared.remove(&oldest).unwrap().material.len();
    }
    if bytes.saturating_add(raw.len()) > 128 * 1024 * 1024 {
        return Err(ApiError::new(
            429,
            "credential_staging_full",
            "Credential staging capacity is temporarily full",
        ));
    }
    let ticket = crypto::digest(&format!(
        "{}{}",
        crate::vault::random_id().map_err(storage)?,
        crate::vault::random_id().map_err(storage)?
    ));
    state.prepared.insert(
        ticket.clone(),
        Prepared {
            at: Instant::now(),
            digest: input.public_digest,
            material: zeroize::Zeroizing::new(raw),
            accounts: input
                .material
                .accounts
                .keys()
                .map(|name| (name.clone(), snapshot.account_revisions.get(name).copied()))
                .collect(),
        },
    );
    Ok(json!({"ticket":ticket,"runtime_id":input.runtime_id,"expires_in":600}))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    operation_id: String,
    runtime_id: String,
    #[serde(default)]
    ticket: Option<String>,
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
    let mut input = public_update(&command.config)?;
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
    if let Some(ticket) = &command.ticket {
        let (material, bases): (Material, BTreeMap<String, Option<i64>>) = {
            let state = live.control.lock().unwrap();
            let prepared = state
                .prepared
                .get(ticket)
                .filter(|p| p.at.elapsed().as_secs() < 600)
                .ok_or_else(|| conflict("credential_ticket_expired"))?;
            if prepared.digest
                != crypto::digest(&serde_json::to_string(&command.config).map_err(|_| invalid())?)
            {
                return Err(conflict("credential_ticket_mismatch"));
            }
            (
                serde_json::from_str(&prepared.material).map_err(|_| invalid())?,
                prepared.accounts.clone(),
            )
        };
        let snapshot = live.runtime.engine().vault.snapshot().map_err(storage)?;
        if bases
            .iter()
            .any(|(name, revision)| snapshot.account_revisions.get(name).copied() != *revision)
        {
            return Err(conflict("account_credential_changed"));
        }
        for (name, mut account) in material.accounts {
            if let Some(control) = input.accounts.remove(&name) {
                account
                    .as_object_mut()
                    .ok_or_else(invalid)?
                    .insert("enabled".into(), control["enabled"].clone());
            }
            input.accounts.insert(name, account);
        }
        input.secrets = material.secrets;
        for d in input.pools.iter_mut().flat_map(|p| &mut p.deployments) {
            if let Some(proxy) = material.proxies.get(&d.id) {
                d.proxy = proxy.clone();
            }
        }
    }
    apply_for(runtime, &item, input).await?;
    let revision = live.settings.lock().unwrap().revision;
    {
        let mut state = live.control.lock().unwrap();
        if let Some(ticket) = command.ticket {
            state.prepared.remove(&ticket);
        }
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

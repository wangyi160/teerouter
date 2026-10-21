use super::*;
use crate::{
    runtime::{Runtime, storage},
    vault::operations::Update,
};
use axum::extract::{Path, Query};
use std::collections::BTreeMap;

pub(crate) async fn input(
    runtime: &Runtime,
    request: Request,
) -> Result<(String, Value), ApiError> {
    let engine = runtime.engine();
    let secret = credential(request.headers()).unwrap_or("").to_owned();
    let body = tokio::time::timeout(
        Duration::from_secs(engine.config.server.body_timeout_secs),
        to_bytes(
            request.into_body(),
            engine.config.server.max_body_bytes.min(16 * 1024 * 1024),
        ),
    )
    .await
    .map_err(|_| ApiError::new(408, "body_timeout", "Management body timed out"))?
    .map_err(|_| ApiError::new(413, "body_too_large", "Management body too large"))?;
    let value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::new(400, "invalid_json", "Invalid management JSON"))?;
    Ok((secret, value))
}
fn invalid() -> ApiError {
    ApiError::new(400, "invalid_update", "Invalid management update")
}
pub(super) async fn admin_relay_status(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    runtime
        .engine()
        .vault
        .relay_status()
        .map(Json)
        .map_err(storage)
}
pub(super) async fn admin_relay_configure(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, body) = input(&runtime, request).await?;
    let engine = runtime.engine();
    if !runtime.is_platform() || !engine.authenticate_admin(&secret) {
        return Err(invalid());
    }
    let config = serde_json::from_value(body).map_err(|_| invalid())?;
    engine.vault.configure_relay(config).map_err(|_| {
        ApiError::new(
            409,
            "relay_configuration_conflict",
            "Relay configuration conflicts with current state",
        )
    })?;
    engine.vault.relay_status().map(Json).map_err(storage)
}
pub(super) async fn admin_relay_flush(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    runtime
        .engine()
        .vault
        .publish_public(true)
        .await
        .map(Json)
        .map_err(|e| ApiError::new(503, e.0, "Public receipt upload has not been acknowledged"))
}
pub(super) async fn admin_relay_settle(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, body) = input(&runtime, request).await?;
    let engine = runtime.engine();
    if !engine.authenticate_admin(&secret) {
        return Err(invalid());
    }
    let ack = serde_json::from_value(body).map_err(|_| invalid())?;
    engine.vault.relay_settle(ack).map_err(|_| {
        ApiError::new(
            409,
            "relay_settlement_conflict",
            "Relay settlement does not match an uploaded boundary",
        )
    })?;
    engine.vault.relay_status().map(Json).map_err(storage)
}

pub(super) async fn admin_apply(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, body) = input(&runtime, request).await?;
    let update = serde_json::from_value(body).map_err(|_| invalid())?;
    runtime.apply(update, &secret).await.map(Json)
}
pub(super) async fn admin_config(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    let snapshot = runtime.engine().vault.snapshot().map_err(storage)?;
    let mut config = serde_json::to_value(snapshot.bundle.config).map_err(|_| invalid())?;
    // Proxy credentials can occur in legacy server fields and API-key deployments.
    fn redact(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if matches!(key.as_str(), "proxy" | "no_proxy") && !value.is_null() {
                        *value = json!("[redacted]");
                    } else {
                        redact(value);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    redact(value);
                }
            }
            _ => {}
        }
    }
    redact(&mut config);
    Ok(Json(
        json!({"revision":snapshot.revision,"config":config,"redacted":true}),
    ))
}
pub(super) async fn admin_config_update(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, body) = input(&runtime, request).await?;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Patch {
        expected_revision: Option<i64>,
        patch: Value,
    }
    let patch: Patch = serde_json::from_value(body).map_err(|_| invalid())?;
    runtime
        .apply(
            Update {
                expected_revision: patch.expected_revision,
                config: Some(patch.patch),
                ..Update::default()
            },
            &secret,
        )
        .await
        .map(Json)
}
pub(super) async fn admin_accounts(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    runtime.engine().account_status().map(Json)
}
pub(super) async fn admin_account_credential(
    State(runtime): State<Arc<Runtime>>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    if !crate::config::valid_name(&name) {
        return Err(invalid());
    }
    let record = runtime
        .engine()
        .vault
        .account_credential(&name)
        .map_err(storage)?
        .ok_or_else(|| ApiError::new(404, "account_not_found", "Account not found"))?;
    Ok(([("cache-control", "no-store")], Json(record)).into_response())
}
pub(super) async fn admin_account_update(
    State(runtime): State<Arc<Runtime>>,
    Path(name): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, mut body) = input(&runtime, request).await?;
    let expected_revision = body
        .as_object_mut()
        .ok_or_else(invalid)?
        .remove("expected_revision")
        .map(|v| v.as_i64().ok_or_else(invalid))
        .transpose()?;
    runtime
        .apply(
            Update {
                expected_revision,
                accounts: BTreeMap::from([(name, body)]),
                ..Update::default()
            },
            &secret,
        )
        .await
        .map(Json)
}
pub(super) async fn admin_account_delete(
    State(runtime): State<Arc<Runtime>>,
    Path(name): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let secret = credential(request.headers()).unwrap_or("");
    runtime
        .apply(
            Update {
                delete_accounts: vec![name],
                ..Update::default()
            },
            secret,
        )
        .await
        .map(Json)
}
pub(super) async fn admin_account_refresh(
    State(runtime): State<Arc<Runtime>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    runtime.engine().refresh_account(&name).await?;
    runtime.engine().account_status().map(Json)
}
#[derive(serde::Deserialize, Default)]
pub(super) struct LogQuery {
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
    #[serde(default)]
    admin: bool,
}
pub(super) async fn admin_logs(
    State(runtime): State<Arc<Runtime>>,
    Query(query): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    runtime
        .engine()
        .vault
        .logs(query.admin, query.after.max(0), query.limit.unwrap_or(100))
        .map(Json)
        .map_err(storage)
}
pub(super) async fn admin_metrics(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    let engine = runtime.engine();
    Ok(Json(
        json!({"runtime":engine.status(),"lifecycle":runtime.lifecycle_status(),"ingress":runtime.ingress.used(),"maintenance":runtime.maintenance_status(),"storage":engine.vault.storage_status().map_err(storage)?,"accounts":engine.account_status()?,"discovery":engine.discovery_status(),"proxy_probes":runtime.merchants.latency.status()}),
    ))
}
pub(super) async fn admin_maintenance(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    let engine = runtime.engine();
    tokio::task::spawn_blocking(move || engine.recover_storage())
        .await
        .map_err(|_| ApiError::new(503, "maintenance_failed", "Maintenance worker failed"))??;
    Ok(Json(json!({"healthy":true})))
}

pub(super) async fn admin_checkpoint(
    State(runtime): State<Arc<Runtime>>,
) -> Result<impl IntoResponse, ApiError> {
    let checkpoint = runtime.engine().vault.checkpoint().map_err(storage)?;
    Ok(([("cache-control", "no-store")], Json(checkpoint)))
}
pub(super) async fn admin_acknowledge(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, body) = input(&runtime, request).await?;
    if !runtime.engine().authenticate_admin(&secret) {
        return Err(ApiError::new(
            401,
            "authentication_error",
            "Admin credential was revoked",
        ));
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Ack {
        boot_id: String,
        generation: u64,
        sequence: u64,
        balances: Option<BTreeMap<String, i64>>,
        balances_patch: Option<BTreeMap<String, Option<i64>>>,
        receipt_batch_hash: Option<String>,
    }
    let ack: Ack = serde_json::from_value(body).map_err(|_| invalid())?;
    if ack.balances.is_some() == ack.balances_patch.is_some() {
        return Err(invalid());
    }
    runtime
        .engine()
        .vault
        .acknowledge_receipts(
            &ack.boot_id,
            ack.generation,
            ack.sequence,
            ack.balances,
            ack.balances_patch.unwrap_or_default(),
            ack.receipt_batch_hash.as_deref(),
        )
        .map_err(|_| {
            ApiError::new(
                409,
                "checkpoint_conflict",
                "Checkpoint acknowledgement mismatch",
            )
        })?;
    Ok(Json(json!({"acknowledged":true})))
}

pub(super) async fn admin_checkpoint_diff(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    let (secret, body) = input(&runtime, request).await?;
    if !runtime.engine().authenticate_admin(&secret) {
        return Err(ApiError::new(
            401,
            "authentication_error",
            "Admin credential was revoked",
        ));
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Base {
        boot_id: Option<String>,
        base_id: Option<String>,
        #[serde(default)]
        sign_receipts: bool,
    }
    let base: Base = serde_json::from_value(body).map_err(|_| invalid())?;
    if base.boot_id.as_ref().is_some_and(|id| id.len() > 64)
        || base.base_id.as_ref().is_some_and(|id| id.len() > 64)
    {
        return Err(invalid());
    }
    let result = runtime
        .engine()
        .vault
        .checkpoint_export(
            base.boot_id.as_deref(),
            base.base_id.as_deref(),
            base.sign_receipts,
        )
        .map_err(storage)?;
    Ok(([("cache-control", "no-store")], Json(result)))
}
pub(super) async fn admin_receipt_evidence(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    let (_, body) = input(&runtime, request).await?;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Challenge {
        nonce: String,
    }
    let challenge: Challenge = serde_json::from_value(body).map_err(|_| invalid())?;
    let vault = runtime.engine().vault.clone();
    let binding = vault
        .receipt_signer
        .identity_binding(&vault.boot_id, &challenge.nonce)
        .ok_or_else(invalid)?;
    let evidence = crate::attestation::receipt_evidence(binding)
        .await
        .map_err(|_| {
            ApiError::new(
                503,
                "attestation_unavailable",
                "Receipt key hardware attestation unavailable",
            )
        })?;
    Ok((
        [("cache-control", "no-store")],
        Json(json!({"schema":"teerouter.receipt-key.v1",
        "public_key":vault.receipt_signer.public_key(),"boot_id":vault.boot_id,"nonce":challenge.nonce,
        "evidence_type":if cfg!(feature="dstack-tdx") {"dstack_tdx"} else {"snp"},"evidence":evidence})),
    ))
}
pub(super) async fn admin_initialize(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, body) = input(&runtime, request).await?;
    runtime.initialize(body, &secret).await.map(Json)
}

pub(super) async fn admin_quiesce(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, body) = input(&runtime, request).await?;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Quiesce {
        boot_id: String,
        quiescing: bool,
    }
    let input: Quiesce = serde_json::from_value(body).map_err(|_| invalid())?;
    let engine = runtime.engine();
    if !engine.authenticate_admin(&secret) {
        return Err(ApiError::new(
            401,
            "authentication_error",
            "Admin credential was revoked",
        ));
    }
    if input.boot_id != engine.vault.boot_id {
        return Err(ApiError::new(
            409,
            "boot_conflict",
            "Reload router boot identity",
        ));
    }
    runtime.set_quiescing(input.quiescing);
    Ok(Json(runtime.lifecycle_status()))
}

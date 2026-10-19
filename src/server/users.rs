//! User CRUD and effective admission limits. Balances arrive through checkpoint ACKs.
use super::*;
use crate::{
    config::User,
    runtime::{Runtime, storage},
    vault::operations::Update,
};
use axum::extract::Path;
use std::collections::BTreeMap;

fn invalid() -> ApiError {
    ApiError::new(400, "invalid_user", "Invalid user or API key")
}
fn missing() -> ApiError {
    ApiError::new(404, "user_not_found", "User not found")
}
fn conflict() -> ApiError {
    ApiError::new(409, "user_conflict", "User or API key already exists")
}

fn view(
    bundle: &crate::vault::Provision,
    user: &User,
    vault: &crate::vault::Vault,
) -> Result<Value, ApiError> {
    let mut value = serde_json::to_value(user).expect("serializable user");
    value["in_flight"] = json!(vault.user_gate(&user.id).used());
    let effective = if vault.managed() {
        let data = vault.lock().map_err(storage)?;
        crate::balances::concurrency_limit(&data, user, &bundle.config.balance_concurrency)
            .unwrap_or(0)
    } else {
        user.max_concurrent
    };
    value["effective_max_concurrent"] = json!(effective);
    value["keys"] = json!(
        bundle
            .config
            .keys
            .iter()
            .filter(|k| k.user_id == user.id)
            .map(|k| {
                let mut key = serde_json::to_value(k).expect("serializable key");
                key["key"] = json!(bundle.secrets.get(&k.key_env));
                key
            })
            .collect::<Vec<_>>()
    );
    value["moderation"] = json!(
        vault
            .lock()
            .map_err(storage)?
            .moderation
            .get(&user.id)
            .cloned()
            .unwrap_or_default()
    );
    Ok(value)
}
pub(super) async fn list(State(runtime): State<Arc<Runtime>>) -> Result<Response, ApiError> {
    let engine = runtime.engine();
    let snapshot = engine.vault.snapshot().map_err(storage)?;
    let users = snapshot
        .bundle
        .config
        .users
        .iter()
        .map(|u| view(&snapshot.bundle, u, &engine.vault))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        [("cache-control", "no-store")],
        Json(json!({"revision":snapshot.revision,"data":users})),
    )
        .into_response())
}
pub(super) async fn get_one(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let engine = runtime.engine();
    let snapshot = engine.vault.snapshot().map_err(storage)?;
    let user = snapshot
        .bundle
        .config
        .users
        .iter()
        .find(|u| u.id == id)
        .ok_or_else(missing)?;
    Ok((
        [("cache-control", "no-store")],
        Json(
            json!({"revision":snapshot.revision,"user":view(&snapshot.bundle,user,&engine.vault)?}),
        ),
    )
        .into_response())
}
pub(super) async fn create(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    write(runtime, None, request).await
}
pub(super) async fn replace(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    write(runtime, Some(id), request).await
}
async fn write(
    runtime: Arc<Runtime>,
    path: Option<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (secret, mut body) = admin::input(&runtime, request).await?;
    let map = body.as_object_mut().ok_or_else(invalid)?;
    let revision = map
        .remove("expected_revision")
        .map(|v| v.as_i64().ok_or_else(invalid))
        .transpose()?;
    let keys = map.remove("keys");
    if let Some(id) = &path {
        if map
            .get("id")
            .is_some_and(|v| v.as_str() != Some(id.as_str()))
        {
            return Err(invalid());
        }
        map.insert("id".into(), json!(id));
    }
    let user: User = serde_json::from_value(body).map_err(|_| invalid())?;
    let snapshot = runtime.engine().vault.snapshot().map_err(storage)?;
    if revision.is_some_and(|r| r != snapshot.revision) {
        return Err(conflict());
    }
    let exists = snapshot.bundle.config.users.iter().any(|u| u.id == user.id);
    if path.is_some() && !exists {
        return Err(missing());
    }
    if path.is_none() && exists {
        return Err(conflict());
    }
    let mut users = snapshot.bundle.config.users.clone();
    users.retain(|u| u.id != user.id);
    users.push(user.clone());
    let mut next_keys = snapshot.bundle.config.keys.clone();
    let mut secrets = BTreeMap::new();
    if let Some(keys) = keys {
        let keys = keys.as_array().ok_or_else(invalid)?;
        next_keys.retain(|k| k.user_id != user.id);
        for item in keys {
            let mut item = item.clone();
            let map = item.as_object_mut().ok_or_else(invalid)?;
            if map
                .get("user_id")
                .is_some_and(|v| v.as_str() != Some(user.id.as_str()))
            {
                return Err(invalid());
            }
            map.insert("user_id".into(), json!(user.id));
            let token = map.remove("key");
            if !map.contains_key("key_ref") && !map.contains_key("key_env") {
                let name = map
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                map.insert("key_ref".into(), json!(format!("user-key-{name}")));
            }
            let key: crate::config::ApiKey = serde_json::from_value(item).map_err(|_| invalid())?;
            if next_keys.iter().any(|k| k.name == key.name) {
                return Err(conflict());
            }
            if snapshot.bundle.secrets.contains_key(&key.key_env)
                && !snapshot
                    .bundle
                    .config
                    .keys
                    .iter()
                    .any(|k| k.user_id == user.id && k.name == key.name && k.key_env == key.key_env)
            {
                return Err(conflict());
            }
            if let Some(token) = token {
                let token = token
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(invalid)?;
                if snapshot
                    .bundle
                    .config
                    .keys
                    .iter()
                    .any(|k| k.user_id != user.id && k.key_env == key.key_env)
                    || snapshot.bundle.config.server.admin_key_env.as_deref() == Some(&key.key_env)
                {
                    return Err(conflict());
                }
                secrets.insert(key.key_env.clone(), token.to_owned());
            }
            next_keys.push(key);
        }
    }
    let patch = json!({"users":users,"keys":next_keys});
    let deletes = unused_secrets(&snapshot.bundle, &patch);
    runtime
        .apply(
            Update {
                expected_revision: Some(snapshot.revision),
                config: Some(patch),
                secrets,
                delete_secrets: deletes,
                ..Update::default()
            },
            &secret,
        )
        .await
        .map(Json)
}
fn unused_secrets(bundle: &crate::vault::Provision, patch: &Value) -> Vec<String> {
    let mut config = serde_json::to_value(&bundle.config).expect("serializable config");
    crate::vault::operations::merge(&mut config, patch);
    fn mentions(value: &Value, name: &str) -> bool {
        match value {
            Value::String(s) => s == name,
            Value::Array(a) => a.iter().any(|v| mentions(v, name)),
            Value::Object(o) => o.values().any(|v| mentions(v, name)),
            _ => false,
        }
    }
    bundle
        .config
        .keys
        .iter()
        .filter(|k| !mentions(&config, &k.key_env))
        .map(|k| k.key_env.clone())
        .collect()
}
pub(super) async fn delete(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let secret = credential(request.headers()).unwrap_or("");
    let snapshot = runtime.engine().vault.snapshot().map_err(storage)?;
    if !snapshot.bundle.config.users.iter().any(|u| u.id == id) {
        return Err(missing());
    }
    let patch = json!({"users":snapshot.bundle.config.users.iter().filter(|u|u.id!=id).collect::<Vec<_>>(),"keys":snapshot.bundle.config.keys.iter().filter(|k|k.user_id!=id).collect::<Vec<_>>()});
    let deletes = unused_secrets(&snapshot.bundle, &patch);
    runtime
        .apply(
            Update {
                expected_revision: Some(snapshot.revision),
                config: Some(patch),
                delete_secrets: deletes,
                ..Update::default()
            },
            secret,
        )
        .await
        .map(Json)
}

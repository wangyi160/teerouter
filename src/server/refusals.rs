//! Admin pull/delete API for the unarchived refusal queue.
use super::*;
use crate::runtime::{Runtime, storage};
use axum::extract::Query;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListQuery {
    #[serde(default)]
    after_id: u64,
    limit: Option<usize>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteQuery {
    through_id: u64,
}
pub(super) async fn list(
    State(runtime): State<Arc<Runtime>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let limit = query.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(ApiError::new(
            400,
            "invalid_refusal_limit",
            "Limit must be between 1 and 500",
        ));
    }
    let engine = runtime.engine();
    let data = engine.vault.lock().map_err(storage)?;
    if query.after_id > data.refusal_queue.last_id {
        return Err(ApiError::new(
            409,
            "invalid_refusal_cursor",
            "Cursor exceeds the last issued refusal ID",
        ));
    }
    let mut page = data.refusal_queue.page(query.after_id, limit);
    page["instance_id"] = json!(data.instance);
    Ok(([("cache-control", "no-store")], Json(page)).into_response())
}
pub(super) async fn delete(
    State(runtime): State<Arc<Runtime>>,
    Query(query): Query<DeleteQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let result = runtime
        .delete_refusals(query.through_id, credential(&headers).unwrap_or(""))
        .await?;
    Ok(([("cache-control", "no-store")], Json(result)).into_response())
}

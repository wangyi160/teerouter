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
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    if method == "POST" && path == "/merchant/bind" {
        return Ok((
            [("cache-control", "no-store")],
            Json(bind(&runtime, &token).await?),
        )
            .into_response());
    }
    let item = authenticate(&runtime, &token)?;
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
        ("PUT", "/merchant/accounts/sync") => {
            let (_, body) = crate::server::admin::input(&runtime, request).await?;
            account_sync::sync(&runtime, &token, body).await?
        }
        ("PUT", "/merchant/accounts/access") => {
            let (_, body) = crate::server::admin::input(&runtime, request).await?;
            access::replace(&runtime, &token, body).await?
        }
        ("POST", "/merchant/accounts/test") => {
            let (_, body) = crate::server::admin::input(&runtime, request).await?;
            probe::test(&runtime, &item, body).await?
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

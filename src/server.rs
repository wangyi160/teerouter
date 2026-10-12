use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    error::ApiError,
    router::{Engine, Protocol},
    upstream::Output,
};

#[path = "server/admin.rs"]
mod admin;
use admin::*;
#[path = "server/users.rs"]
mod users;

#[derive(Clone)]
struct Identity {
    engine: Arc<Engine>,
    account: Option<usize>,
    request_id: String,
}

pub fn app(engine: Arc<Engine>) -> Router {
    let runtime = crate::runtime::Runtime::new(engine);
    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({"status":"ok"})) }),
        )
        .route("/health/ready", get(ready))
        .route("/v1/models", get(models))
        .route("/v1/pools", get(pools))
        .route("/v1/usage", get(usage))
        .route("/v1/chat/completions", post(generate))
        .route("/v1/messages", post(generate))
        .route("/v1/responses", post(generate))
        .route("/admin/status", get(status))
        .route("/admin/usage", get(usage))
        .route("/admin/models", get(discovery_status))
        .route("/admin/models/refresh", post(refresh_models))
        .route(
            "/admin/config",
            get(admin_config).patch(admin_config_update),
        )
        .route("/admin/apply", post(admin_apply))
        .route("/admin/users", get(users::list).post(users::create))
        .route(
            "/admin/users/{id}",
            get(users::get_one)
                .put(users::replace)
                .delete(users::delete),
        )
        .route("/admin/accounts", get(admin_accounts))
        .route(
            "/admin/accounts/{name}/credential",
            get(admin_account_credential),
        )
        .route(
            "/admin/accounts/{name}",
            axum::routing::put(admin_account_update).delete(admin_account_delete),
        )
        .route(
            "/admin/accounts/{name}/refresh",
            post(admin_account_refresh),
        )
        .route("/admin/checkpoint", get(admin_checkpoint))
        .route("/admin/checkpoint/ack", post(admin_acknowledge))
        .route("/admin/initialize", post(admin_initialize))
        .route("/admin/logs", get(admin_logs))
        .route("/admin/metrics", get(admin_metrics))
        .route("/admin/maintenance", post(admin_maintenance))
        .fallback(|| async { ApiError::new(404, "not_found", "Endpoint not found") })
        .method_not_allowed_fallback(|| async {
            ApiError::new(405, "method_not_allowed", "HTTP method not allowed")
        })
        .layer(middleware::from_fn_with_state(
            runtime.clone(),
            authenticate,
        ))
        .with_state(runtime)
}

async fn authenticate(
    State(runtime): State<Arc<crate::runtime::Runtime>>,
    mut request: Request,
    next: Next,
) -> Response {
    let engine = runtime.engine();
    let request_id = engine.request_id();
    let path = request.uri().path();
    let health_request = matches!(path, "/health/live" | "/health/ready");
    let mut result = if health_request {
        Ok(None)
    } else if path.starts_with("/admin/") {
        if !engine.admin_enabled() {
            Err(ApiError::new(404, "not_found", "Admin API is disabled"))
        } else if credential(request.headers()).is_some_and(|key| engine.authenticate_admin(key)) {
            Ok(None)
        } else {
            Err(ApiError::new(
                401,
                "authentication_error",
                "Invalid admin API key",
            ))
        }
    } else if !engine.vault.serving() {
        Err(ApiError::new(
            503,
            "manager_unavailable",
            "Router awaits initialization or management synchronization",
        ))
    } else {
        credential(request.headers())
            .and_then(|key| engine.authenticate(key))
            .map(Some)
            .ok_or(ApiError::new(
                401,
                "authentication_error",
                "Missing or invalid API key",
            ))
    };
    result = result.and_then(|identity| {
        if let Some(index) = identity {
            engine.authorize(index)?;
        }
        Ok(identity)
    });
    let permit = if result.is_ok() && !health_request {
        let permit = runtime.ingress.acquire(engine.config.server.max_ingress);
        if permit.is_none() {
            result = Err(ApiError::new(
                503,
                "ingress_busy",
                "Request admission limit reached",
            ));
        }
        permit
    } else {
        None
    };
    let mut response = match result {
        Ok(account) => {
            request.extensions_mut().insert(Identity {
                engine: engine.clone(),
                account,
                request_id: request_id.clone(),
            });
            next.run(request).await
        }
        Err(error) => error.into_response(),
    };
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id).expect("generated request ID"),
    );
    if let Some(permit) = permit {
        let (parts, body) = response.into_parts();
        let stream = futures_util::stream::unfold(
            (body.into_data_stream(), permit),
            |(mut body, permit)| async move { body.next().await.map(|chunk| (chunk, (body, permit))) },
        );
        response = Response::from_parts(parts, axum::body::Body::from_stream(stream));
    }
    response
}

fn credential(headers: &HeaderMap) -> Option<&str> {
    // Never accept credentials in URL queries or forward client credentials upstream.
    if let Some(value) = headers.get("authorization") {
        let (scheme, key) = value.to_str().ok()?.split_once(' ')?;
        return scheme
            .eq_ignore_ascii_case("bearer")
            .then_some(key)
            .filter(|s| !s.is_empty());
    }
    headers.get("x-api-key").and_then(|v| v.to_str().ok())
}

async fn ready(State(runtime): State<Arc<crate::runtime::Runtime>>) -> Response {
    let engine = runtime.engine();
    if engine.ready() {
        Json(json!({"status":"ready"})).into_response()
    } else {
        ApiError::new(
            503,
            "not_ready",
            "Router awaits moderation configuration, ready deployments or healthy accounting",
        )
        .into_response()
    }
}

async fn models(request: Request) -> Result<Json<Value>, ApiError> {
    let engine = request
        .extensions()
        .get::<Identity>()
        .unwrap()
        .engine
        .clone();
    engine.require_moderation()?;
    Ok(Json(
        engine
            .models(
                request
                    .extensions()
                    .get::<Identity>()
                    .unwrap()
                    .account
                    .unwrap(),
            )
            .await,
    ))
}

async fn discovery_status(State(runtime): State<Arc<crate::runtime::Runtime>>) -> Json<Value> {
    let engine = runtime.engine();
    Json(engine.discovery_status())
}

async fn pools(request: Request) -> Json<Value> {
    let identity = request.extensions().get::<Identity>().unwrap();
    Json(identity.engine.pools(identity.account.unwrap()))
}
async fn refresh_models(State(runtime): State<Arc<crate::runtime::Runtime>>) -> Json<Value> {
    let engine = runtime.engine();
    engine.refresh_models(true).await;
    Json(engine.discovery_status())
}

async fn usage(request: Request) -> Result<Json<Value>, ApiError> {
    let engine = request
        .extensions()
        .get::<Identity>()
        .unwrap()
        .engine
        .clone();
    engine
        .usage(request.extensions().get::<Identity>().unwrap().account)
        .map(Json)
}

async fn status(State(runtime): State<Arc<crate::runtime::Runtime>>) -> Json<Value> {
    let engine = runtime.engine();
    Json(engine.status())
}

async fn generate(request: Request) -> Result<Response, ApiError> {
    let engine = request
        .extensions()
        .get::<Identity>()
        .unwrap()
        .engine
        .clone();
    let (parts, body) = request.into_parts();
    let identity = parts.extensions.get::<Identity>().unwrap();
    let protocol = match parts.uri.path() {
        "/v1/chat/completions" => Protocol::Chat,
        "/v1/messages" => Protocol::Messages,
        "/v1/responses" => Protocol::Responses,
        _ => return Err(ApiError::new(404, "not_found", "Unknown model endpoint")),
    };
    let bytes = tokio::time::timeout(
        Duration::from_secs(engine.config.server.body_timeout_secs),
        to_bytes(body, engine.config.server.max_body_bytes),
    )
    .await
    .map_err(|_| ApiError::new(408, "body_timeout", "Request body timed out"))?
    .map_err(|_| {
        ApiError::new(
            413,
            "body_too_large",
            "Cannot read request body within configured limit",
        )
    })?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::new(400, "invalid_json", "Request body must be valid JSON"))?;
    if !body.is_object() {
        return Err(ApiError::new(
            400,
            "invalid_request",
            "Request body must be a JSON object",
        ));
    }
    let alias = body["model"]
        .as_str()
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
        .ok_or(ApiError::new(
            400,
            "invalid_request",
            "A model ID is required",
        ))?;
    if body.get("stream").is_some_and(|v| !v.is_boolean()) {
        return Err(ApiError::new(
            400,
            "invalid_request",
            "stream must be a boolean",
        ));
    }
    if body.get("stream_options").is_some_and(|v| !v.is_object()) {
        return Err(ApiError::new(
            400,
            "invalid_request",
            "stream_options must be an object",
        ));
    }
    if protocol == Protocol::Responses && body["background"] == true {
        return Err(ApiError::new(
            400,
            "unsupported_parameter",
            "Background Responses jobs are not supported",
        ));
    }
    let mut headers = HeaderMap::new();
    for name in ["x-router-session", "x-router-account", "x-router-pool"] {
        if let Some(value) = parts.headers.get(name) {
            headers.insert(name, value.clone());
        }
    }
    if protocol == Protocol::Messages {
        for name in ["anthropic-beta", "anthropic-version"] {
            if let Some(value) = parts.headers.get(name) {
                headers.insert(name, value.clone());
            }
        }
    }
    let routed = engine
        .execute(
            identity.account.unwrap(),
            protocol,
            &alias,
            body,
            headers,
            identity.request_id.clone(),
        )
        .await?;
    let mut response = match routed.output {
        Output::Json(value) => Json(value).into_response(),
        Output::Stream(stream) => {
            let events = stream.map(|frame| {
                let frame = frame.expect("router converts upstream errors into SSE error frames");
                let mut event = Event::default().data(frame.data);
                if let Some(name) = frame.event {
                    event = event.event(name);
                }
                Ok::<_, Infallible>(event)
            });
            Sse::new(events)
                .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
                .into_response()
        }
    };
    response.headers_mut().insert(
        "x-router-deployment",
        HeaderValue::from_str(&routed.deployment).expect("validated deployment ID"),
    );
    response.headers_mut().insert(
        "x-router-attempts",
        HeaderValue::from_str(&routed.attempts.to_string()).unwrap(),
    );
    Ok(response)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (
            status,
            Json(json!({"error":{"type":self.code,"code":self.code,"message":self.message}})),
        )
            .into_response();
        if self.status == 401 {
            response
                .headers_mut()
                .insert("www-authenticate", HeaderValue::from_static("Bearer"));
        }
        if let Some(seconds) = self.retry_after {
            response.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&seconds.to_string()).unwrap(),
            );
        }
        response
    }
}

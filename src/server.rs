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
pub(crate) mod admin;
use admin::*;
#[path = "server/refusals.rs"]
mod refusals;
#[path = "server/users.rs"]
mod users;

#[derive(Clone)]
struct Identity {
    runtime: Arc<crate::runtime::Runtime>,
    engine: Arc<Engine>,
    account: Option<usize>,
    request_id: String,
}

pub fn app(engine: Arc<Engine>) -> Router {
    let runtime = crate::runtime::Runtime::new(engine);
    app_runtime(runtime)
}
// Provider protocol fixtures exercise a merchant engine directly. This entry
// point is absent from production; platform boundary tests use app/app_runtime.
#[cfg(test)]
pub(crate) fn app_upstream_fixture(engine: Arc<Engine>) -> Router {
    app_runtime(crate::runtime::Runtime::merchant(engine))
}
pub(crate) fn app_runtime(runtime: Arc<crate::runtime::Runtime>) -> Router {
    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({"status":"ok", "routing_protocol_versions":[1],"cache_affinity_version":1,"price_routing":"estimated_total_tokens_with_cache","speed_routing":"proxy_https_latency","proxy_probe_interval_secs":300})) }),
        )
        .route("/health/ready", get(ready))
        .route("/v1/models", get(models))
        .route("/v1/pricing", get(crate::pricing::public_get))
        .route("/v1/pools", get(pools))
        .route("/v1/usage", get(usage))
        .route("/v1/identity", get(user_identity))
        .route("/v1/merchants", get(user_merchants))
        .route(
            "/admin/merchants",
            get(crate::merchants::http::platform_list).put(crate::merchants::http::platform_put),
        )
        .route(
            "/merchant/{*path}",
            axum::routing::any(crate::merchants::http::dispatch),
        )
        .route("/v1/chat/completions", post(generate))
        .route("/v1/messages", post(generate))
        .route("/v1/responses", post(generate))
        .route("/admin/status", get(status))
        .route(
            "/admin/public-catalog",
            get(crate::merchants::public_catalog::get),
        )
        .route(
            "/admin/pricing",
            get(crate::pricing::get).put(crate::pricing::put),
        )
        .route("/admin/usage", get(usage))
        .route(
            "/admin/pricing/source",
            get(crate::pricing::source::get).put(crate::pricing::source::put),
        )
        .route(
            "/admin/pricing/refresh",
            post(crate::pricing::source::refresh),
        )
        .route("/admin/models", get(discovery_status))
        .route("/admin/models/refresh", post(refresh_models))
        .route(
            "/admin/config",
            get(admin_config).patch(admin_config_update),
        )
        .route("/admin/apply", post(admin_apply))
        .route("/admin/users", get(users::list).post(users::create))
        .route(
            "/admin/refusals",
            get(refusals::list).delete(refusals::delete),
        )
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
        .route("/admin/checkpoint", get(admin_checkpoint))
        .route("/admin/checkpoint/diff", post(admin_checkpoint_diff))
        .route("/admin/receipts/evidence", post(admin_receipt_evidence))
        .route("/admin/receipts/relay", get(admin_relay_status).put(admin_relay_configure))
        .route("/admin/receipts/relay/flush", post(admin_relay_flush))
        .route("/admin/receipts/relay/settle", post(admin_relay_settle))
        .route("/admin/checkpoint/ack", post(admin_acknowledge))
        .route("/admin/initialize", post(admin_initialize))
        .route("/admin/quiesce", post(admin_quiesce))
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
    } else if path.starts_with("/merchant/") {
        credential(request.headers())
            .ok_or_else(|| {
                ApiError::new(401, "authentication_error", "Merchant credential required")
            })
            .and_then(|token| {
                if path == "/merchant/bind" && request.method() == axum::http::Method::POST {
                    crate::merchants::authorize_binding(&runtime, token).map(|_| None)
                } else {
                    crate::merchants::authenticate(&runtime, token).map(|_| None)
                }
            })
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
            "Router awaits initialization after startup",
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
        if runtime.is_platform()
            && (path == "/admin/accounts"
                || path.starts_with("/admin/accounts/")
                || path == "/admin/models"
                || path.starts_with("/admin/models/"))
        {
            return Err(ApiError::new(
                404,
                "merchant_owned_resource",
                "Manage upstream accounts and models through the merchant API",
            ));
        }
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
    // Admission precedes the pause check: a request racing with quiesce is
    // either counted by the drain barrier or rejected before its handler runs.
    let drain_control = path.starts_with("/admin/")
        && (request.method() == axum::http::Method::GET
            || matches!(
                path,
                "/admin/quiesce"
                    | "/admin/checkpoint/ack"
                    | "/admin/checkpoint/diff"
                    | "/admin/receipts/evidence"
                    | "/admin/receipts/relay/flush"
                    | "/admin/receipts/relay/settle"
            ));
    if result.is_ok() && !health_request && !drain_control && runtime.quiescing() {
        result = Err(ApiError::new(
            503,
            "router_draining",
            "Router is paused for an administrative shutdown",
        ));
    }
    let mut response = match result {
        Ok(account) => {
            request.extensions_mut().insert(Identity {
                runtime: runtime.clone(),
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

pub(crate) fn credential(headers: &HeaderMap) -> Option<&str> {
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
    let merchant_ready = runtime
        .merchants
        .live
        .lock()
        .unwrap()
        .values()
        .any(|m| m.runtime.engine().ready());
    if !runtime.quiescing()
        && ((!runtime.is_platform() && engine.ready())
            || (engine.vault.serving() && merchant_ready))
    {
        Json(json!({"status":"ready"})).into_response()
    } else {
        ApiError::new(
            503,
            "not_ready",
            "Router awaits ready deployments or healthy accounting",
        )
        .into_response()
    }
}

async fn models(request: Request) -> Result<Json<Value>, ApiError> {
    let identity = request.extensions().get::<Identity>().unwrap();
    let account = identity.account.unwrap();
    let mut result = if identity.runtime.is_platform() {
        json!({"object":"list","data":[]})
    } else {
        identity.engine.models(account).await
    };
    let merchants =
        crate::merchants::routing::models(&identity.runtime, &identity.engine, account).await?;
    result["data"].as_array_mut().unwrap().extend(merchants);
    Ok(Json(result))
}

async fn discovery_status(State(runtime): State<Arc<crate::runtime::Runtime>>) -> Json<Value> {
    let engine = runtime.engine();
    Json(engine.discovery_status())
}

async fn pools(request: Request) -> Result<Json<Value>, ApiError> {
    let identity = request.extensions().get::<Identity>().unwrap();
    let mut result = if identity.runtime.is_platform() {
        json!({"object":"list","data":[]})
    } else {
        identity.engine.pools(
            identity.account.unwrap(),
            &identity.runtime.merchants.latency,
        )
    };
    result["data"].as_array_mut().unwrap().extend(
        crate::merchants::routing::pools(
            &identity.runtime,
            &identity.engine,
            identity.account.unwrap(),
        )
        .await?,
    );
    Ok(Json(result))
}
async fn user_identity(request: Request) -> Json<Value> {
    let identity = request.extensions().get::<Identity>().unwrap();
    Json(json!({"user_id":identity.engine.config.keys[identity.account.unwrap()].user_id}))
}
async fn user_merchants(request: Request) -> Result<Json<Value>, ApiError> {
    let identity = request.extensions().get::<Identity>().unwrap();
    let user = &identity.engine.config.keys[identity.account.unwrap()].user_id;
    Ok(Json(
        json!({"data":crate::merchants::routing::selections(&identity.runtime,user).await?.into_iter().map(|s| {
            let settings=&s.settings;
            let quotes=crate::pricing::quotes(settings,&s.engine.config,&s.price_book);
            json!({"id":s.item.id,"name":s.item.name,"price_version":settings.revision,
                "official_price_revision":s.price_book.revision,
                "prices":quotes.iter().filter(|q|s.engine.model_allowed(s.account,&q.price.pool,&q.price.model) && crate::merchants::routing::platform_allows(&identity.engine,identity.account.unwrap(),&format!("{}.{}",s.item.id,q.price.pool),&q.price.model,s.engine.config.models.iter().find(|p|p.name==q.price.pool).and_then(|p|p.provider))).collect::<Vec<_>>()})
        }).collect::<Vec<_>>()}),
    ))
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
    let mut status = engine.status();
    status["agent_provider_policy"] = json!({
        "version": 1,
        "codex": "openai",
        "claude_code": "anthropic",
        "grok_build": "xai",
        "workbuddy_test_exception": cfg!(feature = "workbuddy-lab"),
        "identification": "client_headers_not_attestation"
    });
    status["lifecycle"] = runtime.lifecycle_status();
    if runtime.quiescing() {
        status["ready"] = false.into();
    }
    if runtime.is_platform() {
        status["platform_ready"] = (engine.vault.serving() && !runtime.quiescing()).into();
        status["merchant_only"] = true.into();
        status["pool_multiplier_pricing"] = true.into();
        status["provider_billing_version"] = 2.into();
        status["balance_sync_version"] = 1.into();
        status["checkpoint_diff_version"] = 1.into();
        status["pricing_source_sync_version"] = 1.into();
    }
    Json(status)
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
    let agent = crate::agent::Agent::detect(&parts.headers)?;
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
    agent.check_model(&alias)?;
    // Parse before reducing headers, so duplicate rules cannot be silently ignored.
    let rule = crate::merchants::route_rule::Rule::parse(&parts.headers)?;
    let connection_headers: Vec<_> = parts
        .headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    for (name, value) in &parts.headers {
        let name_str = name.as_str();
        let router_control = matches!(
            name_str,
            "x-router-session"
                | "x-router-account"
                | "x-router-pool"
                | "x-router-price-version"
                | "x-router-official-price-revision"
                | "x-router-routing"
        );
        if connection_headers.iter().any(|v| v == name_str)
            || (name_str.starts_with("x-router-") && !router_control)
            || name_str.starts_with("proxy-")
            || matches!(
                name_str,
                "authorization"
                    | "x-api-key"
                    | "cookie"
                    | "host"
                    | "chatgpt-account-id"
                    | "content-length"
                    | "connection"
                    | "transfer-encoding"
                    | "keep-alive"
                    | "te"
                    | "trailer"
                    | "upgrade"
            )
        {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    if let Some(query) = parts.uri.query() {
        headers.insert(
            crate::client::NATIVE_QUERY,
            query
                .parse()
                .map_err(|_| ApiError::new(400, "invalid_request", "Invalid native query"))?,
        );
    }
    let merchant_request = rule.is_some()
        || identity.runtime.is_platform()
        || alias
            .split_once('/')
            .is_some_and(|(id, _)| id.starts_with("m_"))
        || headers
            .get("x-router-pool")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|p| p.starts_with("m_") && p.contains('.'));
    let (routed, merchant_guard) = if merchant_request {
        let result = crate::merchants::routing::execute(
            &identity.runtime,
            engine.clone(),
            identity.account.unwrap(),
            protocol,
            &alias,
            body,
            headers,
            identity.request_id.clone(),
            agent,
        )
        .await?;
        (
            result.routed,
            Some((
                result.permit,
                result.merchant,
                result.price_version,
                result.pool,
                result.multiplier,
                result.rule_applied,
            )),
        )
    } else {
        (
            engine
                .execute(
                    identity.account.unwrap(),
                    protocol,
                    &alias,
                    body,
                    headers,
                    identity.request_id.clone(),
                    agent,
                )
                .await?,
            None,
        )
    };
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
    if let Some((permit, merchant, version, pool, multiplier, rule_applied)) = merchant_guard {
        response
            .headers_mut()
            .insert("x-router-pool", pool.parse().unwrap());
        response
            .headers_mut()
            .insert("x-router-multiplier-millionths", multiplier.into());
        if rule_applied {
            response
                .headers_mut()
                .insert("x-router-routing-version", HeaderValue::from_static("1"));
        }
        response
            .headers_mut()
            .insert("x-router-merchant", merchant.parse().unwrap());
        response.headers_mut().insert(
            "x-router-price-version",
            version.to_string().parse().unwrap(),
        );
        let (parts, body) = response.into_parts();
        let stream = futures_util::stream::unfold(
            (body.into_data_stream(), permit),
            |(mut body, permit)| async move { body.next().await.map(|chunk| (chunk, (body, permit))) },
        );
        response = Response::from_parts(parts, axum::body::Body::from_stream(stream));
    }
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

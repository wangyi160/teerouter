//! Disposable credential checks through the same official clients and egress as routing.
use super::*;
use crate::{Client, Error};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    pool: String,
    draft_pool: Option<DraftPool>,
    proxy: String,
    credential: Option<Value>,
    api_key: Option<String>,
    // Older callers may still send this; account checks never select or run a model.
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftPool {
    provider: crate::Provider,
    allowed_models: Option<Vec<String>>,
}

fn failure(stage: &str, error: Error) -> Value {
    let (code, status) = match error {
        Error::Api {
            status: status @ (401 | 403),
            ..
        } => ("credential_rejected", Some(status)),
        Error::Api { status: 429, .. } => ("upstream_rate_limited", Some(429)),
        Error::Api { status, .. } => ("upstream_error", Some(status)),
        Error::OAuth(_) | Error::MissingApiKey(_) => ("credential_invalid_or_expired", None),
        Error::Http(_) | Error::Io(_) => ("proxy_or_upstream_unreachable", None),
        _ => ("invalid_upstream_response", None),
    };
    json!({"ok":false,"stage":stage,"code":code,"upstream_status":status})
}

pub(super) async fn test(
    runtime: &Runtime,
    item: &Registration,
    body: Value,
) -> Result<Value, ApiError> {
    if serde_json::to_vec(&body).map_err(|_| invalid())?.len() > 128 * 1024 {
        return Err(invalid());
    }
    let input: Input = serde_json::from_value(body).map_err(|_| invalid())?;
    let live = live(runtime, &item.id)?;
    let _global = runtime
        .merchants
        .probe_gate
        .acquire(8)
        .ok_or_else(|| ApiError::limited("Account tests are busy"))?;
    let _merchant = live
        .probe_gate
        .acquire(1)
        .ok_or_else(|| ApiError::limited("An account test is already running"))?;
    let config = live.runtime.engine().vault.config().map_err(storage)?;
    if !crate::config::valid_name(&input.pool) {
        return Err(invalid());
    }
    let provider = if let Some(pool) = config.models.iter().find(|p| p.name == input.pool) {
        // A draft never overrides a published pool's provider or model policy.
        if input.draft_pool.is_some() {
            return Err(invalid());
        }
        pool.provider.ok_or_else(invalid)?
    } else if let Some(draft) = input.draft_pool {
        if let Some(models) = &draft.allowed_models {
            let unique: BTreeSet<_> = models.iter().collect();
            if models.len() > 4096
                || unique.len() != models.len()
                || models
                    .iter()
                    .any(|model| !crate::config::valid_model_id(model))
            {
                return Err(invalid());
            }
        }
        draft.provider
    } else {
        return Err(ApiError::new(
            404,
            "probe_pool_missing",
            "Provide the local draft pool when testing its first account",
        ));
    };
    if input.proxy.len() > 8192 || crate::network::validate_proxy(provider, &input.proxy).is_err() {
        return Err(ApiError::new(
            400,
            "probe_proxy_required",
            "A valid account proxy is required",
        ));
    }
    if input.credential.is_some() == input.api_key.is_some()
        || input
            .model
            .as_ref()
            .is_some_and(|m| !crate::config::valid_model_id(m))
    {
        return Err(invalid());
    }
    let profile = config
        .compatibility
        .get(crate::compatibility::name(provider))
        .cloned()
        .unwrap_or_default();
    let http = crate::network::http_client(provider, &input.proxy, 45).map_err(|_| invalid())?;
    let client = if let Some(credential) = input.credential {
        let vault = Vault::memory().map_err(storage)?;
        if vault
            .import_account("probe", provider, &credential)
            .is_err()
        {
            return Ok(failure("credentials", Error::OAuth("invalid credential")));
        }
        vault
            .set_account_proxy("probe", provider, &input.proxy)
            .map_err(|_| invalid())?;
        vault
            .lock()
            .map_err(storage)?
            .bundle
            .as_mut()
            .unwrap()
            .config
            .compatibility = config.compatibility.clone();
        let mut account = crate::oauth::TokenStore::prepared(provider, vault, "probe", input.proxy)
            .map_err(|_| invalid())?;
        Arc::get_mut(&mut account).unwrap().profile = profile.clone();
        Client::with_oauth(account, http)
    } else {
        Client::with_http_client(provider, input.api_key.unwrap(), http)
    };
    let mut client = match client {
        Ok(c) => c,
        Err(e) => return Ok(failure("credentials", e)),
    };
    client.profile = profile;
    client.limits = crate::resources::Limits {
        bytes: 1024 * 1024,
        event: 256 * 1024,
        idle: 30,
    };
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(75),
        run(&client, &config.discovery.codex_client_version),
    )
    .await;
    let mut result =
        result.unwrap_or_else(|_| json!({"ok":false,"stage":"upstream","code":"probe_timeout"}));
    result["latency_ms"] = (started.elapsed().as_millis() as u64).into();
    result["provider"] = serde_json::to_value(provider).map_err(|_| invalid())?;
    Ok(result)
}

// A connection test proves authenticated catalog access through the account's
// proxy. Generation, model allowlists and pricing are separate routing concerns.
async fn run(client: &Client, version: &str) -> Value {
    match crate::discovery::fetch::models(client, version).await {
        Ok((catalog, _)) => json!({"ok":true,"stage":"models","model_count":catalog.len()}),
        Err(error) => failure("models", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;
    use axum::{Json, Router, routing::get};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn catalog_access_succeeds_without_generation_for_every_provider() {
        for provider in [Provider::OpenAI, Provider::Anthropic, Provider::XAI] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let unexpected = Arc::new(AtomicUsize::new(0));
            let calls = unexpected.clone();
            let app = Router::new()
                .route(
                    "/v1/models",
                    get(|| async { Json(json!({"data":[{"id":"model-without-a-price"}]})) }),
                )
                .route(
                    "/v1/language-models",
                    get(|| async { Json(json!({"models":[{"id":"model-without-a-price"}]})) }),
                )
                .fallback(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { axum::http::StatusCode::TOO_MANY_REQUESTS }
                });
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut client = Client::new(provider, "PRIVATE_TEST_KEY").unwrap();
            client.base_url = reqwest::Url::parse(&base).unwrap();
            let result = run(&client, "0.162.0").await;
            assert_eq!(result, json!({"ok":true,"stage":"models","model_count":1}));
            assert_eq!(
                unexpected.load(Ordering::SeqCst),
                0,
                "must never send inference"
            );
            task.abort();
        }
    }

    #[tokio::test]
    async fn catalog_failures_remain_failures_and_do_not_echo_secrets() {
        for (status, body, code) in [
            (
                401,
                json!({"error":"PRIVATE_TEST_KEY"}),
                "credential_rejected",
            ),
            (
                403,
                json!({"error":"PRIVATE_TEST_KEY"}),
                "credential_rejected",
            ),
            (
                429,
                json!({"error":"PRIVATE_TEST_KEY"}),
                "upstream_rate_limited",
            ),
            (500, json!({"error":"PRIVATE_TEST_KEY"}), "upstream_error"),
            (
                200,
                json!({"error":"PRIVATE_TEST_KEY"}),
                "invalid_upstream_response",
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let app = Router::new().route(
                "/v1/models",
                get(move || async move {
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        Json(body),
                    )
                }),
            );
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut client = Client::new(Provider::OpenAI, "PRIVATE_TEST_KEY").unwrap();
            client.base_url = reqwest::Url::parse(&base).unwrap();
            let result = run(&client, "0.162.0").await;
            assert_eq!(result["ok"], false);
            assert_eq!(result["stage"], "models");
            assert_eq!(result["code"], code);
            assert!(!result.to_string().contains("PRIVATE_TEST_KEY"));
            task.abort();
        }
    }
}

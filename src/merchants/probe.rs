//! Disposable credential checks through the same official clients and egress as routing.
use super::*;
use crate::{
    Client, Error,
    upstream::{Output, Protocol},
};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    pool: String,
    proxy: String,
    credential: Option<Value>,
    api_key: Option<String>,
    model: Option<String>,
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
    let pool = config
        .models
        .iter()
        .find(|p| p.name == input.pool)
        .ok_or_else(|| {
            ApiError::new(
                404,
                "probe_pool_missing",
                "Synchronize the account pool before testing",
            )
        })?;
    let provider = pool.provider.ok_or_else(invalid)?;
    if provider.is_local_test() && !item.allow_test {
        return Err(forbidden());
    }
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
    let prices = crate::pricing::book(runtime)?
        .providers
        .into_iter()
        .find(|p| p.provider == provider)
        .map(|p| p.models)
        .unwrap_or_default();
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(75),
        run(
            &client,
            &config.discovery.codex_client_version,
            &prices,
            pool.allowed_models.as_deref(),
            input.model.as_deref(),
        ),
    )
    .await;
    let mut result =
        result.unwrap_or_else(|_| json!({"ok":false,"stage":"upstream","code":"probe_timeout"}));
    result["latency_ms"] = (started.elapsed().as_millis() as u64).into();
    result["provider"] = serde_json::to_value(provider).map_err(|_| invalid())?;
    Ok(result)
}

async fn run(
    client: &Client,
    version: &str,
    prices: &[crate::pricing::ModelPrice],
    allowed: Option<&[String]>,
    requested: Option<&str>,
) -> Value {
    let catalog = match crate::discovery::fetch::models(client, version).await {
        Ok((models, _)) => models,
        Err(error) => return failure("models", error),
    };
    let mut eligible: Vec<_> = prices
        .iter()
        .filter(|p| catalog.contains_key(&p.model))
        .collect();
    eligible.sort_by_key(|p| {
        (
            u128::from(p.rates.output) + u128::from(p.rates.input),
            &p.model,
        )
    });
    let candidates: Vec<&str> = if client.provider.is_local_test() {
        catalog.keys().map(String::as_str).collect()
    } else {
        eligible.iter().map(|p| p.model.as_str()).collect()
    };
    let model = candidates.into_iter().find(|m| {
        allowed.is_none_or(|list| list.iter().any(|v| v == m)) && requested.is_none_or(|r| r == *m)
    });
    let Some(model) = model else {
        return json!({"ok":false,"stage":"models","code":"probe_no_text_model","model_count":catalog.len()});
    };
    let body = json!({"messages":[{"role":"user","content":"Reply with OK."}],"max_completion_tokens":32,"stream":false});
    match crate::upstream::call(
        client,
        Protocol::Chat,
        model,
        body,
        false,
        Default::default(),
    )
    .await
    {
        Ok(Output::Json(value))
            if value["choices"].as_array().is_some_and(|v| !v.is_empty())
                && value.get("error").is_none_or(Value::is_null) =>
        {
            json!({"ok":true,"model":model,"model_count":catalog.len()})
        }
        Ok(_) => failure(
            "generation",
            Error::InvalidResponse("missing model response"),
        ),
        Err(error) => failure("generation", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;
    use axum::{
        Json, Router,
        routing::{get, post},
    };

    #[tokio::test]
    async fn discovery_and_generation_are_both_required_and_errors_do_not_echo_secrets() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/models",get(||async{Json(json!({"data":[{"id":"model-test"}]}))}))
            .route("/v1/chat/completions",post(|headers:axum::http::HeaderMap,Json(body):Json<Value>|async move{
                assert_eq!(body["model"],"model-test");assert_eq!(body["max_completion_tokens"],32);
                if headers["authorization"] == "Bearer REJECTED_TEST_KEY" {
                    return (axum::http::StatusCode::FORBIDDEN, Json(json!({"error":"PRIVATE_TEST_KEY"})));
                }
                (axum::http::StatusCode::OK,Json(json!({"choices":[{"message":{"content":"OK"},"finish_reason":"stop"}]})))
            }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut client = Client::new(Provider::OpenAI, "PRIVATE_TEST_KEY").unwrap();
        client.base_url = reqwest::Url::parse(&base).unwrap();
        let prices = vec![
            serde_json::from_value(json!({"model":"model-test","rates":{"input":1,"output":1}}))
                .unwrap(),
        ];
        let result = run(&client, "0.162.0", &prices, None, None).await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["model"], "model-test");
        assert_eq!(
            run(&client, "0.162.0", &prices, Some(&[]), None).await["code"],
            "probe_no_text_model"
        );
        let mut rejected = Client::new(Provider::OpenAI, "REJECTED_TEST_KEY").unwrap();
        rejected.base_url = client.base_url.clone();
        let denied = run(&rejected, "0.162.0", &prices, None, None).await;
        assert_eq!(denied["stage"], "generation");
        assert_eq!(denied["upstream_status"], 403);
        assert_eq!(denied["ok"], false);
        assert!(!denied.to_string().contains("PRIVATE_TEST_KEY"));
        let error = failure(
            "generation",
            Error::Api {
                status: 401,
                body: "PRIVATE_TEST_KEY".into(),
                retry_after_secs: None,
            },
        );
        assert_eq!(error["code"], "credential_rejected");
        assert!(!error.to_string().contains("PRIVATE_TEST_KEY"));
        task.abort();
    }
}

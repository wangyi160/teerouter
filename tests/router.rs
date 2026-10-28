use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use futures_util::StreamExt;
use rstest::rstest;
use serde_json::{json, Value};
use teellm::{
    config::{Config, Strategy},
    router::Engine,
    server, Provider,
};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};

#[path = "router/agents.rs"]
mod agents;
#[path = "router/cache.rs"]
mod cache;
#[path = "router/catalog_policy.rs"]
mod catalog_policy;
#[path = "router/compatibility.rs"]
mod compatibility;
#[path = "router/discovery.rs"]
mod discovery;
#[path = "router/faithful.rs"]
mod faithful;
#[path = "router/oauth.rs"]
mod oauth;
#[path = "router/operations.rs"]
mod operations;
#[path = "router/pools.rs"]
mod pools;
#[path = "router/proxy.rs"]
mod proxy;
#[path = "router/refusals.rs"]
mod refusals;
#[path = "router/restrictions.rs"]
mod restrictions;
#[path = "router/trust.rs"]
mod trust;
#[path = "router/users.rs"]
mod users;

struct Running {
    url: String,
    task: JoinHandle<()>,
    agent: &'static str,
    runtime: Option<Arc<crate::runtime::Runtime>>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(app: Router) -> Running {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Running {
        url,
        task,
        agent: agent_ua(Provider::OpenAI),
        runtime: None,
    }
}

async fn run_fixture(engine: Arc<Engine>) -> Running {
    let provider = engine
        .config
        .models
        .iter()
        .flat_map(|p| &p.deployments)
        .next()
        .map(|d| d.provider)
        .unwrap_or(Provider::OpenAI);
    let runtime=crate::runtime::Runtime::merchant(engine);
    let mut running = run(server::app_runtime(runtime.clone())).await;
    running.runtime=Some(runtime);
    running.agent = agent_ua(provider);
    running
}

struct Capture {
    path: String,
    uri: String,
    headers: HeaderMap,
    body: Value,
}
fn assert_cached_forward(actual: &Value, expected: &Value) {
    assert_eq!(actual, expected);
}
struct Mock {
    running: Running,
    calls: Arc<Mutex<Vec<Capture>>>,
    called: Arc<Notify>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
}

#[derive(Clone)]
struct Reply {
    headers: HeaderMap,
    status: u16,
    content_type: &'static str,
    body: String,
    retry_after: Option<u64>,
    delay: Duration,
    tail: Option<(Duration, String)>,
}
impl Reply {
    fn json(value: Value) -> Self {
        Self {
            headers: HeaderMap::new(),
            status: 200,
            content_type: "application/json",
            body: value.to_string(),
            retry_after: None,
            delay: Duration::ZERO,
            tail: None,
        }
    }
    fn error(status: u16) -> Self {
        Self {
            status,
            ..Self::json(json!({"error":{"message":"secret upstream diagnostic DO_NOT_LEAK"}}))
        }
    }
    fn sse(body: String) -> Self {
        Self {
            content_type: "text/event-stream",
            body,
            ..Self::json(Value::Null)
        }
    }
}

async fn mock(replies: Vec<Reply>) -> Mock {
    mock_inner(replies, true).await
}

// Other router tests exercise generation/operations; provide a real HTTP catalog
// fixture independently of their generation reply queue. Discovery tests supply
// and capture every response explicitly with catalog_mock.
async fn catalog_mock(replies: Vec<Reply>) -> Mock {
    mock_inner(replies, false).await
}

fn fixture_catalog() -> Value {
    let entries: Vec<_> = [
        "chat",
        "other",
        "actual-model",
        "fixture-model",
        "vendor/Future-v99:preview",
    ]
    .into_iter()
    .map(|id| {
        json!({"id":id,"slug":id,
            "supported_reasoning_levels":[{"effort":"future-ULTRA-v99"}],
            "capabilities":{"reasoning_effort":["future-ULTRA-v99"],
                "effort":{"supported":true,"future-ULTRA-v99":{"supported":true}},
                "thinking":{"types":{"future-mode":{"supported":true}}}},
            "fixture_budget_min":1,"fixture_budget_max":20000,
            "fixture_levels":["future-ULTRA-v99"]})
    })
    .collect();
    json!({"data":entries,"models":entries})
}

async fn mock_inner(replies: Vec<Reply>, automatic_catalog: bool) -> Mock {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let pending = replies.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let called = Arc::new(Notify::new());
    let captured = calls.clone();
    let notified = called.clone();
    let app = Router::new().fallback(any(move |request: Request| {
        let replies = replies.clone();
        let captured = captured.clone();
        let notified = notified.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = to_bytes(body, 2_000_000).await.unwrap();
            if automatic_catalog
                && (parts.uri.path().ends_with("/models")
                    || parts.uri.path().ends_with("/language-models"))
            {
                return axum::Json(fixture_catalog()).into_response();
            }
            captured.lock().unwrap().push(Capture {
                path: parts.uri.path_and_query().unwrap().to_string(),
                uri: parts.uri.to_string(),
                headers: parts.headers,
                body: if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                },
            });
            let reply = replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Reply::error(599));
            notified.notify_one();
            tokio::time::sleep(reply.delay).await;
            let body = if let Some((delay, tail)) = reply.tail {
                Body::from_stream(futures_util::stream::unfold(
                    (0, reply.body, tail),
                    move |(n, first, tail)| async move {
                        match n {
                            0 => Some((Ok::<_, std::io::Error>(first.clone()), (1, first, tail))),
                            1 => {
                                tokio::time::sleep(delay).await;
                                Some((Ok(tail.clone()), (2, first, tail)))
                            }
                            _ => None,
                        }
                    },
                ))
            } else {
                Body::from(reply.body)
            };
            let mut response: Response = (
                StatusCode::from_u16(reply.status).unwrap(),
                [("content-type", reply.content_type)],
                body,
            )
                .into_response();
            if reply.content_type.is_empty() {
                response.headers_mut().remove("content-type");
            }
            if let Some(n) = reply.retry_after {
                response
                    .headers_mut()
                    .insert("retry-after", n.to_string().parse().unwrap());
            }
            response.headers_mut().extend(reply.headers);
            response
        }
    }));
    Mock {
        replies: pending,
        running: run(app).await,
        calls,
        called,
    }
}

fn configuration(mock: &Mock) -> Config {
    let mut config = Config::parse(
        r#"
[server]
admin_key_env = "ADMIN_KEY"
[[users]]
id = "test-user"
max_concurrent = 16

[[keys]]
user_id = "test-user"
name = "user"
key_env = "CLIENT_KEY"
[[models]]
name = "chat"
retry_delay_ms = 0
[[models.deployments]]
id = "a"
provider = "openai"
model = "actual-model"
api_key_env = "UPSTREAM_KEY"
"#,
    )
    .unwrap();
    config.models[0].deployments[0].api_base = Some(format!("{}/v1", mock.running.url));
    config.models[0].deployments[0].proxy = Some(mock.running.url.clone());
    config
}

fn engine(config: Config) -> Arc<Engine> {
    let lookup = |name: &str| {
        Some(
            match name {
                "CLIENT_KEY" => "client-secret",
                "ADMIN_KEY" => "admin-secret",
                _ => "provider-secret",
            }
            .into(),
        )
    };
    Engine::new(config, lookup).unwrap()
}

async fn gateway(config: Config) -> Running {
    run_fixture(engine(config)).await
}
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(agent_ua(Provider::OpenAI))
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}
fn request() -> Value {
    json!({"model":"chat","messages":[{"role":"user","content":"hello"}]})
}
fn agent_ua(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAI => "codex_cli_rs/1.0 (router fixture)",
        Provider::Anthropic => "claude-cli/2.1.0 (external, cli)",
        Provider::XAI => "grok-shell/1.0 (router fixture)",
    }
}
fn completion() -> Value {
    json!({"id":"reply","model":"actual-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}], "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})
}
fn native(provider: Provider) -> Value {
    match provider {
        Provider::Anthropic => {
            json!({"id":"reply","model":"actual-model","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}})
        }
        _ => {
            json!({"id":"reply","object":"response","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],"status":"completed","usage":{"input_tokens":10,"output_tokens":5}})
        }
    }
}
async fn post(gateway: &Running, path: &str, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}{path}", gateway.url))
        .header("user-agent", gateway.agent)
        .bearer_auth("client-secret")
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn post_as(
    gateway: &Running,
    path: &str,
    body: Value,
    provider: Provider,
) -> reqwest::Response {
    client()
        .post(format!("{}{path}", gateway.url))
        .header("user-agent", agent_ua(provider))
        .bearer_auth("client-secret")
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn usage(gateway: &Running) -> Value {
    gateway.runtime.as_ref().unwrap().engine().usage(Some(0)).unwrap()["usage"].clone()
}
async fn fixture_models(gateway: &Running, key: &str) -> Value {
    let engine=gateway.runtime.as_ref().unwrap().engine();
    engine.models(engine.authenticate(key).unwrap()).await
}
async fn fixture_catalog_status(gateway: &Running) -> Value {
    let engine=gateway.runtime.as_ref().unwrap().engine();
    engine.refresh_models(false).await;
    engine.discovery_status()
}

#[rstest]
#[case(Provider::OpenAI, "/v1/chat/completions")]
#[case(Provider::Anthropic, "/v1/messages")]
#[case(Provider::XAI, "/v1/chat/completions")]
#[tokio::test]
async fn native_requests_route_all_three_providers(#[case] provider: Provider, #[case] path: &str) {
    let response = if provider == Provider::Anthropic {
        native(provider)
    } else {
        completion()
    };
    let mock = mock(vec![Reply::json(response)]).await;
    let mut config = configuration(&mock);
    config.models[0].deployments[0].provider = provider;
    let gateway = gateway(config).await;
    let response = post(&gateway, path, request()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-deployment"], "a");
    let result = response.json::<Value>().await.unwrap();
    let text = if provider == Provider::Anthropic {
        &result["content"][0]["text"]
    } else {
        &result["choices"][0]["message"]["content"]
    };
    assert_eq!(text, "ok");
    {
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls[0].path, path);
        assert_eq!(calls[0].body["model"], "chat");
        assert_eq!(
            calls[0]
                .headers
                .get(if provider == Provider::Anthropic {
                    "x-api-key"
                } else {
                    "authorization"
                })
                .unwrap(),
            if provider == Provider::Anthropic {
                "provider-secret"
            } else {
                "Bearer provider-secret"
            }
        );
    }
    let usage = usage(&gateway).await;
    assert_eq!(usage["input_tokens"], 10);
    assert!(usage.get("cost_microusd").is_none());
    assert_eq!(usage["unknown_usage_requests"], 0);
}

#[rstest]
#[case(Provider::Anthropic, "/v1/messages", "/v1/messages", "x-api-key")]
#[case(Provider::OpenAI, "/v1/responses", "/v1/responses", "authorization")]
#[case(Provider::XAI, "/v1/responses", "/v1/responses", "authorization")]
#[tokio::test]
async fn native_protocols_preserve_payload_and_use_provider_auth(
    #[case] provider: Provider,
    #[case] endpoint: &str,
    #[case] path: &str,
    #[case] auth: &str,
) {
    let expected = native(provider);
    let mock = mock(vec![Reply::json(expected.clone())]).await;
    let mut config = configuration(&mock);
    config.models[0].deployments[0].provider = provider;
    let gateway = gateway(config).await;
    let body = json!({"model":"chat","max_tokens":32,"messages":[{"role":"user","content":"hi"}],"input":"hi","contents":[{"parts":[{"text":"hi"}]}],"provider_extension":{"nested":42}});
    let response = client()
        .post(format!("{}{endpoint}", gateway.url))
        .header("user-agent", agent_ua(provider))
        .header(
            auth,
            if auth == "authorization" {
                "Bearer client-secret"
            } else {
                "client-secret"
            },
        )
        .header("anthropic-beta", "test-beta")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), expected);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls[0].path, path);
    assert_eq!(calls[0].body["provider_extension"]["nested"], 42);
    assert_eq!(
        calls[0].headers[auth],
        if auth == "authorization" {
            "Bearer provider-secret"
        } else {
            "provider-secret"
        }
    );

    if provider == Provider::Anthropic {
        assert_eq!(calls[0].headers["anthropic-beta"], "test-beta");
    }
}

#[tokio::test]
async fn authentication_allowlists_and_admin_isolation() {
    let mock = mock(vec![]).await;
    let mut config = configuration(&mock);
    let mut hidden = config.models[0].clone();
    hidden.name = "hidden".into();
    hidden.deployments[0].id = "hidden".into();
    config.models.push(hidden);
    config.keys[0].models = vec!["chat/*".into()];
    let gateway = gateway(config).await;
    let http = client();
    assert_eq!(
        http.get(format!("{}/health/live", gateway.url))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        http.get(format!("{}/v1/models", gateway.url))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let models = fixture_models(&gateway,"client-secret").await;
    assert_eq!(models["data"].as_array().unwrap().len(), 5);
    assert!(models["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["router_pools"] == json!(["chat"])));
    let mut body = request();
    body["model"] = "hidden".into();
    // Model IDs are not pool selectors. This ID is absent from the authorized
    // pool's official catalog; naming a different pool does not select it.
    assert_eq!(
        post(&gateway, "/v1/chat/completions", body).await.status(),
        404
    );
    assert_eq!(
        http.get(format!("{}/admin/status", gateway.url))
            .bearer_auth("client-secret")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = http
        .get(format!("{}/admin/status", gateway.url))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(!text.contains("secret"));
    assert_eq!(
        http.get(format!("{}/v1/models", gateway.url))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert!(mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn weighted_round_robin_balances_requests() {
    let mock = mock(vec![Reply::json(completion()); 6]).await;
    let mut config = configuration(&mock);
    let mut b = config.models[0].deployments[0].clone();
    b.id = "b".into();
    config.models[0].deployments[0].weight = 2;
    config.models[0].deployments.push(b);
    let gateway = gateway(config).await;
    let mut chosen = Vec::new();
    for i in 0..6 {
        let mut body = request();
        body["messages"][0]["content"] = format!("independent conversation {i}").into();
        chosen.push(
            post(&gateway, "/v1/chat/completions", body).await.headers()["x-router-deployment"]
                .to_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(chosen, ["a", "a", "b", "a", "a", "b"]);
}

#[tokio::test]
async fn failover_cooldown_and_explicit_fallback_alias() {
    let failing = mock(vec![Reply {
        retry_after: Some(60),
        ..Reply::error(429)
    }])
    .await;
    let good = mock(vec![Reply::json(completion()); 2]).await;
    let mut config = configuration(&failing);
    let mut fallback = config.models[0].clone();
    fallback.name = "backup".into();
    fallback.deployments[0].id = "b".into();
    fallback.deployments[0].api_base = Some(format!("{}/v1", good.running.url));
    fallback.deployments[0].proxy = Some(good.running.url.clone());
    config.models[0].fallbacks = vec!["backup".into()];
    config.models.push(fallback);
    config.keys[0].models = vec!["chat".into()];
    let gateway = gateway(config).await;
    let response = post(&gateway, "/v1/chat/completions", request()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-deployment"], "b");
    assert_eq!(response.headers()["x-router-attempts"], "2");
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        200
    );
    assert_eq!(failing.calls.lock().unwrap().len(), 1);
    assert_eq!(good.calls.lock().unwrap().len(), 2);
    assert_eq!(usage(&gateway).await["unknown_usage_requests"], 1);
}

#[tokio::test]
async fn retries_transient_failure_but_never_retries_invalid_request() {
    let mock = mock(vec![
        Reply::error(503),
        Reply::json(completion()),
        Reply::error(400),
        Reply::json(completion()),
    ])
    .await;
    let gateway = gateway(configuration(&mock)).await;
    let response = post(&gateway, "/v1/chat/completions", request()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-attempts"], "2");
    let response = post(&gateway, "/v1/chat/completions", request()).await;
    assert_eq!(response.status(), 400);
    assert!(!response.text().await.unwrap().contains("DO_NOT_LEAK"));
    assert_eq!(mock.calls.lock().unwrap().len(), 3);
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn key_request_and_observed_token_limits(#[case] tokens: bool) {
    let mock = mock(vec![Reply::json(completion()); 2]).await;
    let mut config = configuration(&mock);
    if tokens {
        config.keys[0].tpm = 15;
    } else {
        config.keys[0].rpm = 1;
    }
    let gateway = gateway(config).await;
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        200
    );
    let response = post(&gateway, "/v1/chat/completions", request()).await;
    assert_eq!(response.status(), 429);
    assert!(response.headers().contains_key("retry-after"));
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn concurrency_is_held_until_stream_ends_and_released_on_cancellation() {
    let chunk = format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"first"},"finish_reason":null}]})
    );
    let mock = mock(vec![
        Reply {
            tail: Some((Duration::from_secs(2), "data: [DONE]\n\n".into())),
            ..Reply::sse(chunk)
        },
        Reply::json(completion()),
    ])
    .await;
    let mut config = configuration(&mock);
    config.keys[0].max_concurrent = 1;
    let gateway = gateway(config).await;
    let mut body = request();
    body["stream"] = true.into();
    let first = post(&gateway, "/v1/chat/completions", body).await;
    let mut bytes = first.bytes_stream();
    assert!(bytes.next().await.unwrap().is_ok());
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        429
    );
    drop(bytes);
    let mut admitted = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let response = post(&gateway, "/v1/chat/completions", request()).await;
        if response.status() == 200 {
            admitted = true;
            break;
        }
        assert_eq!(response.status(), 429);
    }
    assert!(
        admitted,
        "cancelled client must release its concurrency slot"
    );
    assert_eq!(usage(&gateway).await["errors"], 1);
}

#[tokio::test]
async fn least_busy_avoids_an_inflight_deployment() {
    let mock = mock(vec![
        Reply {
            delay: Duration::from_millis(200),
            ..Reply::json(completion())
        },
        Reply::json(completion()),
    ])
    .await;
    let mut config = configuration(&mock);
    config.models[0].strategy = Strategy::LeastBusy;
    let mut b = config.models[0].deployments[0].clone();
    b.id = "b".into();
    config.models[0].deployments.push(b);
    let gateway = gateway(config).await;
    let url = gateway.url.clone();
    let first = tokio::spawn(async move {
        client()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("client-secret")
            .json(&request())
            .send()
            .await
            .unwrap()
    });
    mock.called.notified().await;
    let mut independent = request();
    independent["messages"][0]["content"] = "independent request".into();
    assert_eq!(
        post(&gateway, "/v1/chat/completions", independent)
            .await
            .headers()["x-router-deployment"],
        "b"
    );
    assert_eq!(first.await.unwrap().headers()["x-router-deployment"], "a");
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn streaming_preserves_usage_and_does_not_force_include_usage(#[case] include_usage: bool) {
    let chunk = json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]});
    let usage_chunk = json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}});
    let mock = mock(vec![Reply::sse(format!(
        "data: {chunk}\n\ndata: {usage_chunk}\n\ndata: [DONE]\n\n"
    ))])
    .await;
    let gateway = gateway(configuration(&mock)).await;
    let mut body = request();
    body["stream"] = true.into();
    body["stream_options"] = json!({"include_usage":include_usage,"vendor_setting":42});
    let response = post(&gateway, "/v1/chat/completions", body).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("prompt_tokens"));
    assert_eq!(
        mock.calls.lock().unwrap()[0].body["stream_options"]["include_usage"],
        include_usage
    );
    assert_eq!(text.matches("[DONE]").count(), 1);
    assert!(usage(&gateway).await.get("cost_microusd").is_none());
    assert_eq!(
        mock.calls.lock().unwrap()[0].body["stream_options"]["vendor_setting"],
        42
    );
}

#[tokio::test]
async fn stream_retries_before_first_frame_but_not_after_output() {
    let chunk = json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]});
    let mock = mock(vec![
        Reply::sse("data: not-json\n\n".into()),
        Reply::sse(format!("data: {chunk}\n\n")),
        Reply::json(completion()),
    ])
    .await;
    let gateway = gateway(configuration(&mock)).await;
    let mut body = request();
    body["stream"] = true.into();
    let response = post(&gateway, "/v1/chat/completions", body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-attempts"], "2");
    let text = response.text().await.unwrap();
    assert!(text.contains("partial"));
    assert!(text.contains("stream_interrupted"));
    assert!(!text.contains("[DONE]"));
    assert_eq!(mock.calls.lock().unwrap().len(), 2);
    assert_eq!(usage(&gateway).await["errors"], 1);
}

#[rstest]
#[case(Provider::Anthropic, "/v1/messages")]
#[case(Provider::OpenAI, "/v1/responses")]
#[case(Provider::XAI, "/v1/responses")]
#[tokio::test]
async fn native_streams_preserve_events_and_account_tokens(
    #[case] provider: Provider,
    #[case] endpoint: &str,
) {
    let native = native(provider);
    let sse = match provider {
        Provider::Anthropic => format!(
            "event: message_start\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            json!({"type":"message_start","message":native})
        ),
        _ => format!(
            "event: response.completed\ndata: {}\n\n",
            json!({"type":"response.completed","response":native})
        ),
    };
    let mock = mock(vec![Reply::sse(sse)]).await;
    let mut config = configuration(&mock);
    config.models[0].deployments[0].provider = provider;
    let gateway = gateway(config).await;
    let response = post(
        &gateway,
        endpoint,
        json!({"model":"chat","stream":true,"input":"hi","max_tokens":32,
            "messages":[{"role":"user","content":"hi"}],"contents":[{"parts":[{"text":"hi"}]}]}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("reply") || text.contains("candidates"));
    assert_eq!(usage(&gateway).await["input_tokens"], 10);
    assert_eq!(usage(&gateway).await["output_tokens"], 5);
}

#[tokio::test]
async fn enforces_body_limit_json_validation_protocol_and_deadline() {
    let mock = mock(vec![Reply {
        delay: Duration::from_secs(2),
        ..Reply::json(completion())
    }])
    .await;
    let mut config = configuration(&mock);
    config.server.request_timeout_secs = 1;
    config.server.max_body_bytes = 1024;
    let gateway = gateway(config).await;
    assert_eq!(
        post(&gateway, "/v1/messages", request()).await.status(),
        400
    );
    assert_eq!(
        post(&gateway, "/v1/chat/completions", json!("invalid"))
            .await
            .status(),
        400
    );
    assert_eq!(
        post(
            &gateway,
            "/v1/chat/completions",
            json!({"model":"chat","messages":["x".repeat(2000)]})
        )
        .await
        .status(),
        413
    );
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        504
    );
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rejects_bad_configs_and_missing_env_before_listening() {
    let mock = mock(vec![]).await;
    let mut config = configuration(&mock);
    assert!(Engine::new(config.clone(), |_| None).is_err());
    assert!(Engine::new(config.clone(), |_| Some("duplicate".into())).is_err());
    config.models[0].fallbacks = vec!["chat".into()];
    assert!(config.validate().unwrap_err().contains("cycle"));
    config.models[0].fallbacks = vec!["missing".into()];
    assert!(config.validate().is_err());
    config.models[0].fallbacks.clear();
    config.models[0].deployments[0].weight = 0;
    assert!(config.validate().is_err());
}

#[rstest]
#[case(true)]
#[case(false)]
#[tokio::test]
async fn global_and_deployment_concurrency_limits(#[case] global: bool) {
    let mock = mock(vec![Reply {
        delay: Duration::from_millis(150),
        ..Reply::json(completion())
    }])
    .await;
    let mut config = configuration(&mock);
    if global {
        config.server.max_concurrent = 1;
    } else {
        config.models[0].deployments[0].max_concurrent = 1;
    }
    let gateway = gateway(config).await;
    let url = gateway.url.clone();
    let first = tokio::spawn(async move {
        client()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("client-secret")
            .json(&request())
            .send()
            .await
            .unwrap()
    });
    mock.called.notified().await;
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        503
    );
    assert_eq!(first.await.unwrap().status(), 200);
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn saturated_deployment_rate_limit_uses_another_deployment() {
    let mock = mock(vec![Reply::json(completion()); 2]).await;
    let mut config = configuration(&mock);
    config.models[0].deployments[0].rpm = 1;
    let mut b = config.models[0].deployments[0].clone();
    b.id = "b".into();
    config.models[0].deployments.push(b);
    config.models[0].deployments[0].weight = 100;
    let gateway = gateway(config).await;
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .headers()["x-router-deployment"],
        "a"
    );
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .headers()["x-router-deployment"],
        "b"
    );
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        503
    );
    assert_eq!(mock.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn circuit_allows_only_one_probe_and_recovers_after_success() {
    let mock = mock(vec![
        Reply::error(503),
        Reply {
            delay: Duration::from_millis(200),
            ..Reply::json(completion())
        },
        Reply::json(completion()),
    ])
    .await;
    let mut config = configuration(&mock);
    config.models[0].max_attempts = 1;
    config.models[0].deployments[0].failure_threshold = 1;
    config.models[0].deployments[0].cooldown_secs = 1;
    let gateway = gateway(config).await;
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        502
    );
    mock.called.notified().await;
    assert_eq!(
        client()
            .get(format!("{}/health/ready", gateway.url))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        503
    );
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let url = gateway.url.clone();
    let probe = tokio::spawn(async move {
        client()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("client-secret")
            .json(&request())
            .send()
            .await
            .unwrap()
    });
    mock.called.notified().await;
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        503
    );
    assert_eq!(probe.await.unwrap().status(), 200);
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        200
    );
    assert_eq!(
        client()
            .get(format!("{}/health/ready", gateway.url))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn invalid_claude_tool_json_is_a_client_error_without_retries() {
    let mock = mock(vec![]).await;
    let mut config = configuration(&mock);
    config.models[0].deployments[0].provider = Provider::Anthropic;
    let gateway = gateway(config).await;
    let body = json!({"model":"chat","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"tool1","type":"function","function":{"name":"f","arguments":"not-json"}}]}]});
    assert_eq!(
        post(&gateway, "/v1/chat/completions", body).await.status(),
        400
    );
    assert!(mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_error_event_can_failover_before_first_frame() {
    let complete = json!({"type":"response.completed","response":native(Provider::OpenAI)});
    let mock = mock(vec![
        Reply::sse(
            "event: error\ndata: {\"type\":\"error\",\"message\":\"DO_NOT_LEAK\"}\n\n".into(),
        ),
        Reply::sse(format!("event: response.completed\ndata: {complete}\n\n")),
    ])
    .await;
    let gateway = gateway(configuration(&mock)).await;
    let response = post(
        &gateway,
        "/v1/responses",
        json!({"model":"chat","input":"hello","stream":true}),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-router-attempts"], "2");
    let text = response.text().await.unwrap();
    assert!(text.contains("response.completed"));
    assert!(!text.contains("DO_NOT_LEAK"));
}

#[tokio::test]
async fn missing_usage_is_marked_unknown_and_not_fabricated() {
    let mut response = completion();
    response.as_object_mut().unwrap().remove("usage");
    let mock = mock(vec![Reply::json(response)]).await;
    let gateway = gateway(configuration(&mock)).await;
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        200
    );
    let usage = usage(&gateway).await;
    assert_eq!(usage["unknown_usage_requests"], 1);
    assert!(usage.get("cost_microusd").is_none());
}

#[rstest]
fn example_config_requires_only_enabled_credentials_and_model() {
    let config = Config::parse(include_str!("../config.example.toml")).unwrap();
    let engine = Engine::new(config, |name| match name {
        "TEELLM_API_KEY" => Some("client-secret".into()),
        "OPENAI_API_KEY" => Some("provider-secret".into()),
        "OPENAI_MODEL" => Some("actual-model".into()),
        _ => None,
    });
    assert!(engine.is_ok());
}

#[tokio::test]
async fn outbound_proxy_is_loaded_from_encrypted_configuration() {
    let proxy = mock(vec![Reply::json(completion())]).await;
    let mut config = configuration(&proxy);
    config.server.proxy = Some(proxy.running.url.clone());
    config.models[0].deployments[0].api_base = Some("http://fixture.invalid/v1".into());
    let gateway = gateway(config).await;
    assert_eq!(
        post(&gateway, "/v1/chat/completions", request())
            .await
            .status(),
        200
    );
    let calls = proxy.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].uri.contains("fixture.invalid"));
}

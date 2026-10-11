use futures_util::TryStreamExt;
use rstest::rstest;
use teellm::{Client, Error, Provider, Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

struct Captured {
    headers: String,
    body: Value,
}

async fn server(status: u16, content_type: &str, body: String) -> (String, JoinHandle<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let response = format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut data = Vec::new();
        let header_end = loop {
            let mut buffer = [0_u8; 1024];
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            data.extend_from_slice(&buffer[..count]);
            if let Some(position) = data.windows(4).position(|s| s == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8(data[..header_end].to_vec()).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        while data.len() < header_end + length {
            let mut buffer = [0_u8; 1024];
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            data.extend_from_slice(&buffer[..count]);
        }
        let body = serde_json::from_slice(&data[header_end..header_end + length]).unwrap();
        socket.write_all(response.as_bytes()).await.unwrap();
        Captured { headers, body }
    });
    (format!("http://{address}/v1/"), handle)
}

fn request() -> Value {
    json!({"model":"test-model", "messages":[{"role":"user", "content":"hello"}], "max_completion_tokens":42})
}

fn openai_response() -> Value {
    json!({"id":"reply", "model":"test-model", "object":"chat.completion", "choices":[{
        "index":0, "message":{"role":"assistant", "content":"world"}, "finish_reason":"stop"
    }], "usage":{"prompt_tokens":2, "completion_tokens":1, "total_tokens":3}, "vendor_extension":{"signature":"kept"}})
}

fn native_response() -> Value {
    json!({"id":"reply", "model":"test-model", "content":[{"type":"text", "text":"world"}],
        "stop_reason":"end_turn", "usage":{"input_tokens":2, "output_tokens":1}})
}

#[rstest]
#[case::openai(Provider::OpenAI)]
#[case::gemini(Provider::Gemini)]
#[case::grok(Provider::XAI)]
#[case::claude(Provider::Anthropic)]
#[tokio::test]
async fn sends_provider_request_and_returns_chat_response(#[case] provider: Provider) {
    let reply = if provider == Provider::Anthropic {
        native_response()
    } else {
        openai_response()
    };
    let (url, captured) = server(200, "application/json", reply.to_string()).await;
    let client = Client::new(provider, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let response = client.completion(request()).await.unwrap();
    let captured = captured.await.unwrap();
    assert_eq!(response["choices"][0]["message"]["content"], "world");
    assert_eq!(response["usage"]["total_tokens"], 3);
    assert_eq!(captured.body["model"], "test-model");
    assert_eq!(captured.body["stream"], false);
    if provider == Provider::Anthropic {
        assert!(captured.headers.starts_with("POST /v1/messages "));
        assert!(captured.headers.contains("x-api-key: test-key"));
        assert!(captured.headers.contains("anthropic-version:"));
        assert!(!captured.headers.contains("authorization:"));
        assert_eq!(captured.body["max_tokens"], 42);
        assert_eq!(captured.body["messages"][0]["content"][0]["text"], "hello");
    } else {
        assert!(captured.headers.starts_with("POST /v1/chat/completions "));
        assert!(captured.headers.contains("authorization: Bearer test-key"));
        let token_parameter = if provider == Provider::OpenAI {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        assert_eq!(captured.body[token_parameter], 42);
        assert_eq!(response, reply);
    }
}

#[rstest]
#[tokio::test]
async fn claude_converts_system_images_tools_and_results_and_preserves_replay_blocks() {
    let response = json!({"id":"r", "model":"test-model", "content":[
        {"type":"thinking", "thinking":"reasoning", "signature":"signature"},
        {"type":"tool_use", "id":"call3", "name":"weather", "input":{"city":"Tokyo"}}
    ], "stop_reason":"tool_use", "usage":{"input_tokens":5,"output_tokens":4,
        "cache_read_input_tokens":7,"cache_creation_input_tokens":2}});
    let (url, captured) = server(200, "application/json", response.to_string()).await;
    let client = Client::new(Provider::Anthropic, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let result = client.completion(json!({"model":"test-model", "messages":[
        {"role":"system","content":"one"}, {"role":"developer","content":[{"type":"text","text":"two"}]},
        {"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}}]},
        {"role":"assistant","content":null,"tool_calls":[
            {"id":"call1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}},
            {"id":"call2","type":"function","function":{"name":"weather","arguments":"{}"}}
        ]},
        {"role":"tool","tool_call_id":"call1","content":"sunny"},
        {"role":"tool","tool_call_id":"call2","content":"rainy"}
    ], "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}],
        "tool_choice":"required","parallel_tool_calls":false,"stop":"END","max_tokens":100})).await.unwrap();
    let body = captured.await.unwrap().body;
    assert_eq!(
        body["system"],
        json!([{"type":"text","text":"one"},{"type":"text","text":"two"}])
    );
    assert_eq!(
        body["messages"][0]["content"][0]["source"],
        json!({"type":"base64","media_type":"image/png","data":"YQ=="})
    );
    assert_eq!(
        body["messages"][1]["content"][0]["input"],
        json!({"city":"Paris"})
    );
    assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][2]["content"][1]["tool_use_id"], "call2");
    assert_eq!(
        body["tool_choice"],
        json!({"type":"any","disable_parallel_tool_use":true})
    );
    assert_eq!(body["tools"][0]["input_schema"], json!({"type":"object"}));
    assert_eq!(body["stop_sequences"], json!(["END"]));
    assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(result["usage"]["prompt_tokens"], 14);
    assert_eq!(result["usage"]["total_tokens"], 18);
    let message = result["choices"][0]["message"].clone();
    assert_eq!(message["provider_content"], response["content"]);
    assert_eq!(message["reasoning_content"], "reasoning");
    let args: Value = serde_json::from_str(
        message["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args, json!({"city":"Tokyo"}));
    let (url, replay) = server(200, "application/json", native_response().to_string()).await;
    client.with_base_url(&url).unwrap().completion(json!({"model":"test-model", "messages":[
        {"role":"user","content":"weather?"}, message, {"role":"tool","tool_call_id":"call3","content":"cloudy"}
    ]})).await.unwrap();
    assert_eq!(
        replay.await.unwrap().body["messages"][1]["content"],
        response["content"]
    );
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|value| format!("data: {value}\r\n\r\n"))
        .collect()
}

#[rstest]
#[case::openai(Provider::OpenAI)]
#[case::gemini(Provider::Gemini)]
#[case::grok(Provider::XAI)]
#[tokio::test]
async fn compatible_streams_preserve_tools_signatures_and_usage(#[case] provider: Provider) {
    let events = vec![
        json!({"id":"r","choices":[{"index":0,"delta":{"role":"assistant","content":"你好"},"finish_reason":null}]}),
        json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call1","type":"function","function":{"name":"weather","arguments":"{}"},"extra_content":{"google":{"thought_signature":"sig"}}}]},"finish_reason":"tool_calls"}]}),
        json!({"id":"r","choices":[],"usage":{"total_tokens":9}}),
    ];
    let (url, captured) = server(
        200,
        "text/event-stream",
        format!("{}data: [DONE]\n\n", sse(&events)),
    )
    .await;
    let client = Client::new(provider, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let mut request = request();
    request["stream_options"] = json!({"include_usage":true});
    let chunks: Vec<Value> = client
        .stream(request)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(chunks, events);
    assert_eq!(captured.await.unwrap().body["stream"], true);
}

#[rstest]
#[case::with_usage(true)]
#[case::without_usage(false)]
#[tokio::test]
async fn claude_stream_maps_sparse_tool_indices_arguments_finish_and_cached_usage(
    #[case] include_usage: bool,
) {
    let events = vec![
        json!({"type":"message_start","message":{"id":"r","model":"test-model","usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":7,"cache_creation_input_tokens":2}}}),
        json!({"type":"ping"}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call1","name":"weather","input":{}}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}),
        json!({"type":"content_block_stop","index":3}),
        json!({"type":"content_block_start","index":7,"content_block":{"type":"tool_use","id":"call2","name":"clock","input":{}}}),
        json!({"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":""}}),
        json!({"type":"content_block_stop","index":7}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ];
    let (url, captured) = server(200, "text/event-stream", sse(&events)).await;
    let client = Client::new(Provider::Anthropic, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let mut request = request();
    request["stream_options"] = json!({"include_usage":include_usage});
    let chunks: Vec<Value> = client
        .stream(request)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    captured.await.unwrap();
    assert!(
        chunks
            .iter()
            .all(|c| c["id"] == "r" && c["model"] == "test-model")
    );
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "你好");
    let calls: Vec<&Value> = chunks
        .iter()
        .filter_map(|c| {
            c["choices"][0]["delta"]["tool_calls"][0]
                .as_object()
                .map(|_| &c["choices"][0]["delta"]["tool_calls"][0])
        })
        .collect();
    let args = |index: u64| {
        calls
            .iter()
            .filter(|c| c["index"] == index)
            .filter_map(|c| c["function"]["arguments"].as_str())
            .collect::<String>()
    };
    assert_eq!(
        serde_json::from_str::<Value>(&args(0)).unwrap(),
        json!({"city":"Paris"})
    );
    assert_eq!(args(1), "{}");
    assert_eq!(calls.iter().filter(|c| c["id"].is_string()).count(), 2);
    assert!(
        chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "tool_calls")
    );
    let usage: Vec<&Value> = chunks.iter().filter_map(|c| c.get("usage")).collect();
    if include_usage {
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0]["prompt_tokens"], 14);
        assert_eq!(usage[0]["completion_tokens"], 4);
        assert_eq!(usage[0]["total_tokens"], 18);
    } else {
        assert!(usage.is_empty());
    }
}

#[rstest]
#[case::unauthorized(401)]
#[case::rate_limit(429)]
#[case::upstream_failure(503)]
#[tokio::test]
async fn retains_http_status_and_error_body(#[case] status: u16) {
    let (url, captured) = server(
        status,
        "application/json",
        "{\"error\":\"test failure\"}".into(),
    )
    .await;
    let error = Client::new(Provider::OpenAI, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap()
        .completion(request())
        .await
        .unwrap_err();
    captured.await.unwrap();
    assert!(
        matches!(error, Error::Api { status: code, body, .. } if code == status && body.contains("test failure"))
    );
}

#[rstest]
#[case::truncated("data: {\"choices\":[]}\n\n")]
#[case::bad_json("data: not-json\n\n")]
#[case::error_event("data: {\"error\":{\"message\":\"failure\"}}\n\n")]
#[tokio::test]
async fn stream_errors_are_visible_and_terminate(#[case] body: &str) {
    let (url, captured) = server(200, "text/event-stream", body.into()).await;
    let stream = Client::new(Provider::OpenAI, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap()
        .stream(request())
        .await
        .unwrap();
    let result: Result<Vec<Value>, Error> = stream.try_collect().await;
    assert!(result.is_err());
    captured.await.unwrap();
}

#[rstest]
#[case::unknown_parameter(json!({"n":2}))]
#[case::invalid_tokens(json!({"max_completion_tokens":0}))]
#[case::invalid_tool(json!({"tools":[{"type":"web_search"}]}))]
#[case::unsupported_content(json!({"messages":[{"role":"user","content":[{"type":"input_audio"}]}]}))]
#[tokio::test]
async fn rejects_unsupported_claude_input_before_network(#[case] extra: Value) {
    let mut request = request();
    request
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let client = Client::new(Provider::Anthropic, "test-key")
        .unwrap()
        .with_base_url("http://127.0.0.1:1")
        .unwrap();
    assert!(matches!(
        client.completion(request).await,
        Err(Error::Unsupported(_) | Error::InvalidRequest(_))
    ));
}

#[rstest]
#[case::mime_case("Text/Event-Stream; charset=utf-8", true)]
#[case::json("application/json", false)]
#[case::wrong_subtype("text/event-stream-invalid", false)]
#[tokio::test]
async fn checks_stream_media_type(#[case] content_type: &str, #[case] valid: bool) {
    let (url, captured) = server(200, content_type, "data: [DONE]\n\n".into()).await;
    let client = Client::new(Provider::OpenAI, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let result = client.stream(request()).await;
    assert_eq!(result.is_ok(), valid);
    captured.await.unwrap();
}

#[rstest]
#[case::missing_start(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}))]
#[case::missing_finish(json!({"type":"message_stop"}))]
#[case::provider_error(json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}))]
#[tokio::test]
async fn claude_stream_rejects_invalid_lifecycle_and_provider_errors(#[case] event: Value) {
    let (url, captured) = server(200, "text/event-stream", sse(&[event])).await;
    let stream = Client::new(Provider::Anthropic, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap()
        .stream(request())
        .await
        .unwrap();
    let result: Result<Vec<Value>, Error> = stream.try_collect().await;
    assert!(result.is_err());
    captured.await.unwrap();
}

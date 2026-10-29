use crate::upstream::{self, Output, Protocol};
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

async fn native(
    client: &Client,
    protocol: Protocol,
    body: Value,
    streaming: bool,
) -> Result<Output, Error> {
    upstream::call(
        client,
        protocol,
        "test-model",
        body,
        streaming,
        Default::default(),
    )
    .await
}

#[rstest]
#[case(Provider::OpenAI, Protocol::Chat)]
#[case(Provider::XAI, Protocol::Chat)]
#[case(Provider::Anthropic, Protocol::Messages)]
#[case(Provider::OpenAI, Protocol::Responses)]
#[case(Provider::XAI, Protocol::Responses)]
#[tokio::test]
async fn native_request_and_response_are_not_converted(
    #[case] provider: Provider,
    #[case] protocol: Protocol,
) {
    let reply = match protocol {
        Protocol::Chat => openai_response(),
        Protocol::Messages => native_response(),
        Protocol::Responses => {
            json!({"id":"r","output":[],"vendor_extension":{"signature":"kept"}})
        }
    };
    let (url, captured) = server(200, "application/json", reply.to_string()).await;
    let client = Client::new(provider, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let mut body = request();
    body["unknown_future_parameter"] = json!({"null":null,"list":[1,2]});
    if protocol == Protocol::Messages {
        body.as_object_mut()
            .unwrap()
            .remove("max_completion_tokens");
        body["max_tokens"] = 42.into();
        body["system"] =
            json!([{"type":"text","text":"exact system","cache_control":{"type":"ephemeral"}}]);
        body["tools"] = json!([{"type":"web_search_20250305","name":"web_search"}]);
    } else if protocol == Protocol::Responses {
        body = json!({"model":"test-model","input":"hello","store":false,"unknown_future_parameter":[null,1]});
    }
    let Output::Json(result) = native(&client, protocol, body.clone(), false)
        .await
        .unwrap()
    else {
        panic!("expected JSON")
    };
    assert_eq!(result, reply);
    let sent = captured.await.unwrap();
    assert_eq!(sent.body, body);
    let path = match protocol {
        Protocol::Chat => "chat/completions",
        Protocol::Messages => "messages",
        Protocol::Responses => "responses",
    };
    assert!(
        sent.headers
            .starts_with(&format!("POST /v1/{path} HTTP/1.1"))
    );
    let auth = if provider == Provider::Anthropic {
        "x-api-key: test-key"
    } else {
        "authorization: Bearer test-key"
    };
    assert!(sent.headers.contains(auth));
}

#[tokio::test]
async fn native_claude_billing_counts_cache_usage_without_chat_conversion() {
    for streaming in [false, true] {
        let usage = json!({"input_tokens":60,"output_tokens":20,"cache_read_input_tokens":30,"cache_creation_input_tokens":10,
            "cache_creation":{"ephemeral_5m_input_tokens":6,"ephemeral_1h_input_tokens":4}});
        let mut reply = native_response();
        reply["usage"] = usage.clone();
        let body = if streaming {
            let mut initial = reply.clone();
            initial["usage"]["output_tokens"] = json!(0);
            [json!({"type":"message_start","message":initial}),
             json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}),
             json!({"type":"message_stop"})].iter().map(|v|format!("data: {v}\n\n")).collect::<String>()
        } else {
            reply.to_string()
        };
        let (url, captured) = server(
            200,
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
            body,
        )
        .await;
        let mut request = request();
        request
            .as_object_mut()
            .unwrap()
            .remove("max_completion_tokens");
        request["max_tokens"] = 42.into();
        request["stream"] = streaming.into();
        let observer = crate::billing::Observer::new(
            Provider::Anthropic,
            &request,
            crate::billing::tests::quote(),
        )
        .unwrap();
        let mut client = Client::new(Provider::Anthropic, "test-key")
            .unwrap()
            .with_base_url(&url)
            .unwrap();
        client.billing = Some(observer.clone());
        match native(&client, Protocol::Messages, request.clone(), streaming)
            .await
            .unwrap()
        {
            Output::Stream(events) => {
                let frames: Vec<_> = events.try_collect().await.unwrap();
                assert_eq!(frames.len(), 3);
            }
            Output::Json(value) => assert_eq!(value, reply),
        }
        assert_eq!(captured.await.unwrap().body, request);
        assert_eq!(observer.finish(200, 1)["official_cost_microusd"], 354);
    }
}

#[rstest]
#[case(401)]
#[case(429)]
#[case(503)]
#[tokio::test]
async fn native_http_errors_retain_status_and_body(#[case] status: u16) {
    let (url, captured) = server(
        status,
        "application/json",
        "{\"error\":\"test failure\"}".into(),
    )
    .await;
    let client = Client::new(Provider::OpenAI, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let error = native(&client, Protocol::Chat, request(), false)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, Error::Api {status:code,body,..} if code == status && body.contains("test failure"))
    );
    captured.await.unwrap();
}

#[rstest]
#[case("data: {\"choices\":[]}\n\n")]
#[case("data: not-json\n\n")]
#[case("data: {\"error\":{\"message\":\"failure\"}}\n\n")]
#[tokio::test]
async fn native_stream_errors_are_visible_and_terminate(#[case] body: &str) {
    let (url, captured) = server(200, "text/event-stream", body.into()).await;
    let client = Client::new(Provider::OpenAI, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let Output::Stream(events) = native(&client, Protocol::Chat, request(), true)
        .await
        .unwrap()
    else {
        panic!("expected stream")
    };
    assert!(events.try_collect::<Vec<_>>().await.is_err());
    captured.await.unwrap();
}

#[rstest]
#[case("Text/Event-Stream; charset=utf-8", true)]
#[case("application/json", false)]
#[case("text/event-stream-invalid", false)]
#[tokio::test]
async fn native_stream_checks_media_type(#[case] content_type: &str, #[case] valid: bool) {
    let (url, captured) = server(200, content_type, "data: [DONE]\n\n".into()).await;
    let client = Client::new(Provider::OpenAI, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    assert_eq!(
        native(&client, Protocol::Chat, request(), true)
            .await
            .is_ok(),
        valid
    );
    captured.await.unwrap();
}

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::XAI)]
#[tokio::test]
async fn native_stream_preserves_tool_deltas_and_usage(#[case] provider: Provider) {
    let event = json!({"id":"r","choices":[{"delta":{"tool_calls":[{"index":7,"id":"t","function":{"arguments":"{\"x\":1}"}}]},"finish_reason":"tool_calls"}],
                      "usage":{"prompt_tokens":10,"completion_tokens":5},"signature":"unchanged"});
    let (url, captured) = server(
        200,
        "text/event-stream",
        format!("data: {event}\n\ndata: [DONE]\n\n"),
    )
    .await;
    let client = Client::new(provider, "test-key")
        .unwrap()
        .with_base_url(&url)
        .unwrap();
    let Output::Stream(events) = native(&client, Protocol::Chat, request(), true)
        .await
        .unwrap()
    else {
        panic!("expected stream")
    };
    let frames: Vec<_> = events.try_collect().await.unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].value, Some(event));
    assert_eq!(frames[1].data, "[DONE]");
    captured.await.unwrap();
}

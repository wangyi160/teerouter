use super::*;
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};

fn observer() -> Observer {
    Observer::new(
        Provider::XAI,
        &json!({"messages":[{"role":"user","content":"hello"}]}),
        crate::billing::tests::quote(),
    )
    .unwrap()
}
fn request(url: &str) -> reqwest::Request {
    reqwest::Client::new().post(url).json(&json!({"model":"grok-test","messages":[{"role":"user","content":"actual PRIVATE input"}]})).build().unwrap()
}
async fn read_request(socket: &mut TcpStream) -> String {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        headers.push(socket.read_u8().await.unwrap());
        assert!(headers.len() < 8192);
    }
    let headers = String::from_utf8(headers).unwrap();
    let len = headers
        .lines()
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    let mut body = vec![0; len];
    socket.read_exact(&mut body).await.unwrap();
    assert!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["messages"].is_array());
    headers
}

#[tokio::test]
async fn cancellation_while_waiting_closes_upstream_and_charges_only_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let (sent, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        sent.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
    });
    let o = observer();
    let held = o.clone();
    let transport = Transport::new(Provider::XAI, &proxy, 5).unwrap();
    let task = tokio::spawn(async move {
        transport
            .send(request("http://official.fixture/v1/chat/completions"), held)
            .await
    });
    received.await.unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    let bill = o.finish(499, 1);
    assert_eq!(bill["transport"]["phase"], "awaiting_response", "{bill}");
    assert_eq!(bill["transport"]["request_flushed"], true);
    assert_eq!(bill["usage_source"], "estimated");
    assert_eq!(bill["usage"]["output"], 0);
    assert!(bill["charge_microusd"].as_u64().unwrap() > 0);
    assert!(!bill.to_string().contains("PRIVATE"));
}

#[tokio::test]
async fn network_drop_after_complete_request_is_input_estimate() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
    });
    let o = observer();
    assert!(
        Transport::new(Provider::XAI, &proxy, 5)
            .unwrap()
            .send(
                request("http://official.fixture/v1/chat/completions"),
                o.clone()
            )
            .await
            .is_err()
    );
    server.await.unwrap();
    let bill = o.finish(502, 1);
    assert_eq!(bill["state"], "calculated", "{bill}");
    assert_eq!(bill["usage_source"], "estimated");
    assert_eq!(bill["usage"]["output"], 0);
    assert!(bill["usage"]["input"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn failed_connection_and_rejected_proxy_tunnel_never_charge_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let o = observer();
    assert!(
        Transport::new(Provider::XAI, &proxy, 2)
            .unwrap()
            .send(
                request("https://official.fixture/v1/chat/completions"),
                o.clone()
            )
            .await
            .is_err()
    );
    assert_eq!(o.finish(502, 1)["charge_microusd"], 0);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(socket.read_u8().await.unwrap());
        }
        assert!(headers.starts_with(b"CONNECT official.fixture:443 HTTP/1.1"));
        socket
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });
    let o = observer();
    assert!(
        Transport::new(Provider::XAI, &proxy, 2)
            .unwrap()
            .send(
                request("https://official.fixture/v1/chat/completions"),
                o.clone()
            )
            .await
            .is_err()
    );
    server.await.unwrap();
    let bill = o.finish(502, 1);
    assert_eq!(bill["charge_microusd"], 0);
    assert_eq!(bill["transport"]["request_flushed"], false);
}

#[tokio::test]
async fn partial_write_is_not_complete_transmission_and_does_not_block_other_requests() {
    let (io, mut reader) = tokio::io::duplex(32);
    let o = observer();
    o.request_connecting();
    let progress = Arc::new(Mutex::new(Progress {
        observer: Some(o.clone()),
        expected: 1024,
        ..Default::default()
    }));
    let mut io = Tracked {
        io: Box::new(io),
        progress,
    };
    let write = tokio::spawn(async move {
        io.write_all(
            &[
                b"POST / HTTP/1.1\r\nContent-Length: 1024\r\n\r\n".as_slice(),
                &vec![b'x'; 1024],
            ]
            .concat(),
        )
        .await
        .unwrap();
        io.flush().await.unwrap();
    });
    let mut read = [0; 64];
    reader.read_exact(&mut read).await.unwrap();
    write.abort();
    let _ = write.await;
    let bill = o.finish(499, 1);
    assert_eq!(bill["transport"]["phase"], "writing_request");
    assert_eq!(bill["transport"]["request_flushed"], false);
    assert_eq!(bill["charge_microusd"], 0);
}

#[tokio::test]
async fn completed_responses_reuse_connections_with_separate_progress() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for _ in 0..2 {
            read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .await
                .unwrap();
        }
    });
    let transport = Transport::new(Provider::XAI, &proxy, 5).unwrap();
    for _ in 0..2 {
        let o = observer();
        let response = transport
            .send(
                request("http://official.fixture/v1/chat/completions"),
                o.clone(),
            )
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "{}");
        assert_eq!(o.finish(499, 1)["transport"]["request_flushed"], true);
    }
    server.await.unwrap();
}

#[tokio::test]
async fn per_account_deadline_still_applies_after_response_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
            .await
            .unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
    });
    let mut transport = Transport::new(Provider::XAI, &proxy, 2).unwrap();
    transport.timeout = Duration::from_millis(300);
    let o = observer();
    let response = transport
        .send(
            request("http://official.fixture/v1/chat/completions"),
            o.clone(),
        )
        .await
        .unwrap();
    o.headers(response.status().as_u16());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), response.text())
            .await
            .unwrap()
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(o.finish(504, 1)["usage_source"], "estimated");
}

#[tokio::test]
async fn socks_uses_remote_dns_and_keeps_proxy_credentials_out_of_provider_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!(
        "socks5h://alice:pass%3Aword@{}",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut greeting = [0; 3];
        socket.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 1, 2]);
        socket.write_all(&[5, 2]).await.unwrap();
        assert_eq!(socket.read_u8().await.unwrap(), 1);
        let len = socket.read_u8().await.unwrap();
        let mut user = vec![0; len as usize];
        socket.read_exact(&mut user).await.unwrap();
        let len = socket.read_u8().await.unwrap();
        let mut pass = vec![0; len as usize];
        socket.read_exact(&mut pass).await.unwrap();
        assert_eq!(user, b"alice");
        assert_eq!(pass, b"pass:word");
        socket.write_all(&[1, 0]).await.unwrap();
        let mut head = [0; 4];
        socket.read_exact(&mut head).await.unwrap();
        assert_eq!(head, [5, 1, 0, 3]);
        let len = socket.read_u8().await.unwrap();
        let mut host = vec![0; len as usize];
        socket.read_exact(&mut host).await.unwrap();
        assert_eq!(host, b"official.fixture");
        assert_eq!(socket.read_u16().await.unwrap(), 80);
        socket
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let headers = read_request(&mut socket).await;
        assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(!headers.contains("proxy-authorization"));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .unwrap();
    });
    let response = Transport::new(Provider::XAI, &proxy, 5)
        .unwrap()
        .send(
            request("http://official.fixture/v1/chat/completions"),
            observer(),
        )
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "{}");
    server.await.unwrap();
}

#[tokio::test]
async fn untrusted_tls_is_rejected_before_any_model_request_is_written() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let cert = rcgen::generate_simple_self_signed(vec!["official.fixture".into()]).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(socket.read_u8().await.unwrap());
        }
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        assert!(
            tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(socket)
                .await
                .is_err()
        );
    });
    let o = observer();
    assert!(
        Transport::new(Provider::XAI, &proxy, 5)
            .unwrap()
            .send(
                request("https://official.fixture/v1/chat/completions"),
                o.clone()
            )
            .await
            .is_err()
    );
    server.await.unwrap();
    assert_eq!(o.finish(502, 1)["charge_microusd"], 0);
    assert_eq!(o.finish(502, 1)["transport"]["request_flushed"], false);
}

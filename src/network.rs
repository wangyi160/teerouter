//! Explicit outbound bindings. Never inherit system proxies or bypass lists.
use crate::{Error, Provider};
use std::time::Duration;
pub(crate) mod latency;
pub(crate) mod metered;
mod vless;

pub(crate) fn proxy(value: &str) -> Result<reqwest::Proxy, Error> {
    let invalid = || Error::OAuth("invalid proxy: expected http, https or socks5h URL");
    if value.trim() != value || value.bytes().any(|b| b.is_ascii_control()) {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https" | "socks5h")
        || url.host_str().is_none()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err(invalid());
    }
    // socks5h delegates destination DNS to the proxy. Do not accept socks5's local DNS.
    reqwest::Proxy::all(url)
        .map(|proxy| proxy.no_proxy(None))
        .map_err(|_| invalid())
}

pub(crate) fn validate_proxy(provider: Provider, value: &str) -> Result<(), Error> {
    if value.starts_with("vless://") {
        return vless::Node::parse(value).map(|_| ());
    }
    let _ = provider;
    proxy(value).map(|_| ())
}

pub(crate) fn http_client(
    provider: Provider,
    proxy_url: &str,
    timeout_secs: u64,
) -> Result<reqwest::Client, Error> {
    let _ = provider;
    let mut builder = reqwest::Client::builder().no_proxy();
    if proxy_url.starts_with("vless://") {
        let node = vless::bind(vless::Node::parse(proxy_url)?)?;
        builder = builder
            .proxy(node.proxy()?)
            .connector_layer(vless::LeaseLayer(node));
    } else {
        builder = builder.proxy(proxy(proxy_url)?);
    }
    builder
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::OAuth("cannot create outbound HTTP client"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn invalid_proxy_inputs_are_rejected_without_echoing_credentials() {
        for provider in [Provider::OpenAI, Provider::Anthropic, Provider::XAI] {
            assert!(validate_proxy(provider, "direct://").is_err());
            assert!(http_client(provider, "direct://", 3).is_err());
        }
        for url in [
            "",
            "direct://",
            "localhost:8080",
            "ftp://user:PRIVATE_PASSWORD@host",
            "socks5://host:1080",
            "http://host/path",
            "https://host?password=PRIVATE_PASSWORD",
            "http://host/#PRIVATE_PASSWORD",
            "http://host:0",
            " http://host",
            "http://host\n",
        ] {
            let error = proxy(url).expect_err("must reject").to_string();
            assert!(!error.contains("PRIVATE_PASSWORD"));
        }
        for url in [
            "http://host",
            "https://user:pass%3Aword@host:443",
            "socks5h://user:pass@[::1]:1080",
        ] {
            assert!(proxy(url).is_ok());
        }
    }

    #[tokio::test]
    async fn socks5h_authenticates_and_passes_destination_hostname_to_proxy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "socks5h://user:pass%3Aword@{}",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(socket.read_u8().await.unwrap(), 5);
            let n = socket.read_u8().await.unwrap();
            let mut methods = vec![0; n as usize];
            socket.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&2));
            socket.write_all(&[5, 2]).await.unwrap();
            assert_eq!(socket.read_u8().await.unwrap(), 1);
            let n = socket.read_u8().await.unwrap();
            let mut user = vec![0; n as usize];
            socket.read_exact(&mut user).await.unwrap();
            let n = socket.read_u8().await.unwrap();
            let mut pass = vec![0; n as usize];
            socket.read_exact(&mut pass).await.unwrap();
            assert_eq!(user, b"user");
            assert_eq!(pass, b"pass:word");
            socket.write_all(&[1, 0]).await.unwrap();
            let mut header = [0; 4];
            socket.read_exact(&mut header).await.unwrap();
            assert_eq!(header, [5, 1, 0, 3]); // CONNECT, hostname (not local DNS result).
            let n = socket.read_u8().await.unwrap();
            let mut host = vec![0; n as usize];
            socket.read_exact(&mut host).await.unwrap();
            assert_eq!(host, b"target.fixture.invalid");
            assert_eq!(socket.read_u16().await.unwrap(), 8081);
            socket
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();
            let request = read_headers(&mut socket).await;
            assert!(request.starts_with("GET /resource HTTP/1.1"));
            assert!(!request.to_lowercase().contains("proxy-authorization"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let result = http_client(Provider::OpenAI, &url, 3)
            .unwrap()
            .get("http://target.fixture.invalid:8081/resource")
            .send()
            .await
            .unwrap();
        assert_eq!(result.text().await.unwrap(), "ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn https_target_uses_authenticated_connect_and_rejects_denied_tunnel() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://user:secret@{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_headers(&mut socket).await.to_lowercase();
            assert!(request.starts_with("connect target.fixture.invalid:443 http/1.1"));
            assert!(request.contains("proxy-authorization: basic dxnlcjpzzwnyzxq="));
            socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        assert!(
            http_client(Provider::OpenAI, &url, 3)
                .unwrap()
                .get("https://target.fixture.invalid/resource")
                .send()
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    async fn read_headers(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 8192);
            bytes.push(socket.read_u8().await.unwrap());
        }
        String::from_utf8(bytes).unwrap()
    }
}

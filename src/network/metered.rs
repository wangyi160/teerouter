//! HTTP/1 inference transport with a local write/flush boundary. This proves
//! local transmission, never remote execution or TCP acknowledgement. No bodies,
//! headers, destinations or proxy credentials are added to billing evidence.
use crate::{Error, Provider, billing::Observer};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use reqwest::{
    Url,
    header::{CONTENT_LENGTH, HOST, HeaderValue, TRANSFER_ENCODING},
};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
#[cfg(test)]
mod tests;

trait Socket: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Socket for T {}
type Io = Box<dyn Socket>;
type Sender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;
type Pool = Arc<Mutex<Vec<Idle>>>;

#[derive(Clone)]
pub(crate) struct Transport {
    proxy: Option<Url>,
    _vless: Option<Arc<super::vless::Lease>>,
    pool: Pool,
    timeout: Duration,
}
struct Idle {
    sender: Sender,
    progress: Arc<Mutex<Progress>>,
    driver: tokio::task::JoinHandle<()>,
    origin: String,
    used: Instant,
}
impl Drop for Idle {
    fn drop(&mut self) {
        self.driver.abort();
    }
}
#[derive(Default)]
struct Progress {
    observer: Option<Observer>,
    tail: [u8; 4],
    headers: bool,
    body: usize,
    expected: usize,
    flushed: bool,
}
struct Tracked {
    io: Io,
    progress: Arc<Mutex<Progress>>,
}
impl AsyncRead for Tracked {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl AsyncWrite for Tracked {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.io).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = result {
            let mut p = self.progress.lock().unwrap();
            if !p.flushed {
                if let Some(o) = &p.observer {
                    o.request_writing();
                }
                let mut offset = 0;
                while !p.headers && offset < n {
                    p.tail.rotate_left(1);
                    p.tail[3] = buf[offset];
                    offset += 1;
                    p.headers = p.tail == *b"\r\n\r\n";
                }
                p.body = p.body.saturating_add(n - offset);
            }
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let result = Pin::new(&mut self.io).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            let mut p = self.progress.lock().unwrap();
            if !p.flushed && p.headers && p.body == p.expected {
                p.flushed = true;
                if let Some(o) = &p.observer {
                    o.request_flushed();
                }
            }
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
fn failure(message: &'static str) -> Error {
    Error::Io(std::io::Error::other(message))
}
fn timed_out() -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "upstream transport timeout",
    ))
}

impl Transport {
    pub(crate) fn new(provider: Provider, proxy: &str, timeout: u64) -> Result<Self, Error> {
        super::validate_proxy(provider, proxy)?;
        let (proxy, node) = if proxy.starts_with("vless://") {
            let node = super::vless::bind(super::vless::Node::parse(proxy)?)?;
            (Some(node.proxy_url()), Some(node))
        } else if proxy == "direct://" {
            (None, None)
        } else {
            (
                Some(Url::parse(proxy).map_err(|_| failure("invalid proxy"))?),
                None,
            )
        };
        Ok(Self {
            proxy,
            _vless: node,
            pool: Default::default(),
            timeout: Duration::from_secs(timeout),
        })
    }
    pub(crate) async fn send(
        &self,
        request: reqwest::Request,
        observer: Observer,
    ) -> Result<reqwest::Response, Error> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        tokio::time::timeout_at(deadline, self.send_inner(request, observer, deadline))
            .await
            .map_err(|_| timed_out())?
    }
    async fn send_inner(
        &self,
        request: reqwest::Request,
        observer: Observer,
        deadline: tokio::time::Instant,
    ) -> Result<reqwest::Response, Error> {
        let body = request
            .body()
            .and_then(|b| b.as_bytes())
            .ok_or(Error::InvalidRequest("buffered model request required"))?;
        observer.request_body(&serde_json::from_slice(body)?);
        let target = request.url();
        let origin = target.origin().ascii_serialization();
        observer.request_connecting();
        let reused = {
            let mut pool = self.pool.lock().unwrap();
            pool.retain(|c| !c.sender.is_closed() && c.used.elapsed() < Duration::from_secs(60));
            pool.iter()
                .position(|c| c.origin == origin)
                .map(|i| pool.swap_remove(i))
        };
        let mut conn = match reused {
            Some(conn) => conn,
            None => {
                let io = tokio::time::timeout(
                    Duration::from_secs(10),
                    connect(target, self.proxy.as_ref()),
                )
                .await
                .map_err(|_| timed_out())??;
                let progress = Arc::new(Mutex::new(Progress::default()));
                let (sender, driver) =
                    hyper::client::conn::http1::handshake(TokioIo::new(Tracked {
                        io,
                        progress: progress.clone(),
                    }))
                    .await
                    .map_err(|_| failure("HTTP handshake failed"))?;
                let driver = tokio::spawn(async move {
                    let _ = driver.await;
                });
                Idle {
                    sender,
                    driver,
                    progress,
                    origin,
                    used: Instant::now(),
                }
            }
        };
        conn.sender
            .ready()
            .await
            .map_err(|_| failure("upstream connection closed"))?;
        *conn.progress.lock().unwrap() = Progress {
            observer: Some(observer),
            expected: body.len(),
            ..Default::default()
        };
        let absolute = target.scheme() == "http"
            && self
                .proxy
                .as_ref()
                .is_some_and(|p| matches!(p.scheme(), "http" | "https"));
        let path = format!(
            "{}{}",
            target.path(),
            target.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        let uri = if absolute { target.as_str() } else { &path };
        let mut outgoing = hyper::Request::builder()
            .method(request.method())
            .uri(uri)
            .body(Full::new(Bytes::copy_from_slice(body)))
            .map_err(|_| failure("invalid upstream request"))?;
        *outgoing.headers_mut() = request.headers().clone();
        let headers = outgoing.headers_mut();
        headers.insert(
            HOST,
            HeaderValue::from_str(&authority(target))
                .map_err(|_| failure("invalid upstream host"))?,
        );
        headers.remove(TRANSFER_ENCODING);
        headers.insert(CONTENT_LENGTH, body.len().into());
        if absolute {
            proxy_auth(headers, self.proxy.as_ref().unwrap())?;
        }
        let response = conn
            .sender
            .send_request(outgoing)
            .await
            .map_err(|_| failure("upstream HTTP exchange failed"))?;
        let (parts, body) = response.into_parts();
        let pool = self.pool.clone();
        // Keep the connection alive while consumed; dropping a response aborts
        // the driver immediately. Only fully consumed responses return to pool.
        let stream = futures_util::stream::unfold(
            (body.into_data_stream(), Some(conn), pool, false),
            move |(mut body, mut conn, pool, ended)| async move {
                if ended {
                    return None;
                }
                let next = tokio::time::timeout_at(deadline, body.next()).await;
                match next {
                    Err(_) => {
                        conn.take();
                        Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "upstream request deadline exceeded",
                            )),
                            (body, conn, pool, true),
                        ))
                    }
                    Ok(Some(chunk)) => {
                        if chunk.is_err() {
                            conn.take();
                        }
                        let ended = chunk.is_err();
                        Some((
                            chunk.map_err(|_| {
                                std::io::Error::other("upstream response read failed")
                            }),
                            (body, conn, pool, ended),
                        ))
                    }
                    Ok(None) => {
                        if let Some(mut conn) = conn.take() {
                            conn.progress.lock().unwrap().observer = None;
                            conn.used = Instant::now();
                            let mut pool = pool.lock().unwrap();
                            if pool.len() < 8 && !conn.sender.is_closed() {
                                pool.push(conn);
                            }
                        }
                        None
                    }
                }
            },
        );
        Ok(hyper::Response::from_parts(parts, reqwest::Body::wrap_stream(stream)).into())
    }
}
fn authority(url: &Url) -> String {
    format!(
        "{}:{}",
        url.host_str().unwrap(),
        url.port_or_known_default().unwrap()
    )
}
fn decoded(s: &str) -> Vec<u8> {
    percent_encoding::percent_decode_str(s).collect()
}
fn proxy_auth(headers: &mut reqwest::header::HeaderMap, proxy: &Url) -> Result<(), Error> {
    use base64::Engine;
    if !proxy.username().is_empty() || proxy.password().is_some() {
        let mut bytes = decoded(proxy.username());
        bytes.push(b':');
        bytes.extend(decoded(proxy.password().unwrap_or("")));
        let mut value = HeaderValue::from_str(&format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        ))
        .map_err(|_| failure("invalid proxy credentials"))?;
        value.set_sensitive(true);
        headers.insert("proxy-authorization", value);
    }
    Ok(())
}
async fn tls(io: Io, host: &str) -> Result<Io, Error> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| failure("TLS configuration failed"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let name = rustls::pki_types::ServerName::try_from(host.trim_matches(['[', ']']).to_owned())
        .map_err(|_| failure("invalid TLS host"))?;
    Ok(Box::new(
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, io)
            .await
            .map_err(|_| failure("TLS handshake failed"))?,
    ))
}
async fn connect(target: &Url, proxy: Option<&Url>) -> Result<Io, Error> {
    let peer = proxy.unwrap_or(target);
    let socket = tokio::net::TcpStream::connect((
        peer.host_str().unwrap().trim_matches(['[', ']']),
        peer.port_or_known_default().unwrap_or(1080),
    ))
    .await?;
    socket.set_nodelay(true)?;
    let mut io: Io = Box::new(socket);
    if let Some(proxy) = proxy {
        if proxy.scheme() == "https" {
            io = tls(io, proxy.host_str().unwrap()).await?;
        }
        if proxy.scheme() == "socks5h" {
            socks(&mut io, target, proxy).await?;
        } else if target.scheme() == "https" {
            let mut headers = reqwest::header::HeaderMap::new();
            proxy_auth(&mut headers, proxy)?;
            let auth = headers
                .get("proxy-authorization")
                .map(|v| format!("Proxy-Authorization: {}\r\n", v.to_str().unwrap()))
                .unwrap_or_default();
            let host = authority(target);
            io.write_all(
                format!("CONNECT {host} HTTP/1.1\r\nHost: {host}\r\n{auth}\r\n").as_bytes(),
            )
            .await?;
            io.flush().await?;
            let mut response = Vec::new();
            while !response.ends_with(b"\r\n\r\n") {
                if response.len() >= 16384 {
                    return Err(failure("proxy headers too large"));
                }
                response.push(io.read_u8().await?);
            }
            let line = response.split(|b| *b == b'\n').next().unwrap_or_default();
            if !line.starts_with(b"HTTP/1.") || line.get(9..12) != Some(b"200") {
                return Err(failure("proxy tunnel rejected"));
            }
        }
    }
    if target.scheme() == "https" {
        io = tls(io, target.host_str().unwrap()).await?;
    }
    Ok(io)
}
async fn socks(io: &mut Io, target: &Url, proxy: &Url) -> Result<(), Error> {
    let auth = !proxy.username().is_empty() || proxy.password().is_some();
    io.write_all(if auth { &[5, 1, 2] } else { &[5, 1, 0] })
        .await?;
    let mut reply = [0; 2];
    io.read_exact(&mut reply).await?;
    if reply != [5, if auth { 2 } else { 0 }] {
        return Err(failure("SOCKS authentication rejected"));
    }
    if auth {
        let user = decoded(proxy.username());
        let pass = decoded(proxy.password().unwrap_or(""));
        if user.len() > 255 || pass.len() > 255 {
            return Err(failure("SOCKS credentials too long"));
        }
        let mut packet = vec![1, user.len() as u8];
        packet.extend(user);
        packet.push(pass.len() as u8);
        packet.extend(pass);
        io.write_all(&packet).await?;
        io.read_exact(&mut reply).await?;
        if reply != [1, 0] {
            return Err(failure("SOCKS authentication rejected"));
        }
    }
    let host = target.host_str().unwrap().trim_matches(['[', ']']);
    let mut packet = vec![5, 1, 0];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            packet.push(1);
            packet.extend(ip.octets());
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            packet.push(4);
            packet.extend(ip.octets());
        }
        Err(_) => {
            if host.len() > 255 {
                return Err(failure("SOCKS host too long"));
            }
            packet.extend([3, host.len() as u8]);
            packet.extend(host.as_bytes());
        }
    }
    packet.extend(target.port_or_known_default().unwrap().to_be_bytes());
    io.write_all(&packet).await?;
    let mut header = [0; 4];
    io.read_exact(&mut header).await?;
    if header[..3] != [5, 0, 0] {
        return Err(failure("SOCKS tunnel rejected"));
    }
    let len = match header[3] {
        1 => 4,
        4 => 16,
        3 => io.read_u8().await? as usize,
        _ => return Err(failure("invalid SOCKS reply")),
    };
    let mut discard = vec![0; len + 2];
    io.read_exact(&mut discard).await?;
    Ok(())
}

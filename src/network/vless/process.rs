//! The only helper control channel is the parent's anonymous pipe.
use super::Node;
use crate::Error;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, OnceLock, Weak, mpsc},
    task::{Context, Poll},
    time::Duration,
};

fn unavailable() -> Error {
    Error::OAuth("VLESS egress is unavailable; request was not sent directly")
}

struct Control {
    child: Child,
    input: ChildStdin,
    replies: mpsc::Receiver<Value>,
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Core {
    port: u16,
    password: zeroize::Zeroizing<String>,
    control: Mutex<Control>,
}

impl Core {
    fn start() -> Result<Arc<Self>, Error> {
        let path = match std::env::var_os("TEEROUTER_EGRESS_BIN") {
            Some(path) => std::path::PathBuf::from(path),
            None => std::env::current_exe()
                .map_err(|_| unavailable())?
                .with_file_name(if cfg!(windows) {
                    "teerouter-egress.exe"
                } else {
                    "teerouter-egress"
                }),
        };
        let mut command = Command::new(path);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(key);
        }
        command.env("GOMAXPROCS", "2").env("GOMEMLIMIT", "128MiB");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn().map_err(|_| unavailable())?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, replies) = mpsc::sync_channel(1);
        let mut control = Control {
            child,
            input,
            replies,
        };
        std::thread::Builder::new()
            .name("vless-control".into())
            .spawn(move || {
                let mut output = BufReader::new(output);
                loop {
                    let mut bytes = Vec::new();
                    // Bound even malformed helper output; never log its contents.
                    use std::io::Read;
                    let read = output.by_ref().take(4097).read_until(b'\n', &mut bytes);
                    if read.is_err()
                        || bytes.is_empty()
                        || bytes.len() > 4096
                        || !bytes.ends_with(b"\n")
                    {
                        break;
                    }
                    let Ok(value) = serde_json::from_slice(&bytes) else {
                        break;
                    };
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            })
            .map_err(|_| unavailable())?;
        let ready = control
            .replies
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| unavailable())?;
        let port = ready["port"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= u16::MAX as u64)
            .ok_or_else(unavailable)? as u16;
        let password = ready["password"]
            .as_str()
            .filter(|p| p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(unavailable)?
            .to_owned();
        if ready["ready"] != true
            || ready["version"] != 1
            || control
                .child
                .try_wait()
                .map_err(|_| unavailable())?
                .is_some()
        {
            return Err(unavailable());
        }
        Ok(Arc::new(Self {
            port,
            password: zeroize::Zeroizing::new(password),
            control: Mutex::new(control),
        }))
    }

    fn alive(&self) -> bool {
        self.control
            .lock()
            .is_ok_and(|mut c| matches!(c.child.try_wait(), Ok(None)))
    }

    fn call(&self, request: Value) -> Result<(), Error> {
        let mut c = self.control.lock().map_err(|_| unavailable())?;
        let mut bytes =
            zeroize::Zeroizing::new(serde_json::to_vec(&request).map_err(|_| unavailable())?);
        bytes.push(b'\n');
        let result = (|| {
            if bytes.len() > 65536 || c.child.try_wait().map_err(|_| unavailable())?.is_some() {
                return Err(unavailable());
            }
            c.input.write_all(&bytes).map_err(|_| unavailable())?;
            let reply = c
                .replies
                .recv_timeout(Duration::from_secs(10))
                .map_err(|_| unavailable())?;
            reply["ok"].as_bool().ok_or_else(unavailable)
        })();
        if result.is_err() {
            // Protocol desynchronization must never reuse a previous acknowledgement.
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
        if result? { Ok(()) } else { Err(unavailable()) }
    }
}

#[derive(Default)]
struct Registry {
    // Keep the process alive even when a response outlives its reqwest Client.
    core: Option<Arc<Core>>,
    leases: HashMap<String, Weak<Lease>>,
}
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

pub(in crate::network) struct Lease {
    id: String,
    core: Arc<Core>,
}

impl Lease {
    pub(in crate::network) fn proxy_url(&self) -> reqwest::Url {
        reqwest::Url::parse(&format!(
            "socks5h://{}:{}@127.0.0.1:{}",
            self.id,
            self.core.password.as_str(),
            self.core.port
        ))
        .expect("local proxy URL")
    }
    pub(in crate::network) fn proxy(&self) -> Result<reqwest::Proxy, Error> {
        let url = zeroize::Zeroizing::new(format!(
            "socks5h://{}:{}@127.0.0.1:{}",
            self.id,
            self.core.password.as_str(),
            self.core.port
        ));
        reqwest::Proxy::all(url.as_str())
            .map(|proxy| proxy.no_proxy(None))
            .map_err(|_| unavailable())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.core.call(json!({"op":"remove", "id":self.id}));
    }
}

pub(in crate::network) fn bind(node: Node) -> Result<Arc<Lease>, Error> {
    let config = node.config();
    let bytes = zeroize::Zeroizing::new(serde_json::to_vec(&config).map_err(|_| unavailable())?);
    let key = format!("{:x}", Sha256::digest(&bytes));
    let mut registry = REGISTRY
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| unavailable())?;
    if let Some(lease) = registry
        .leases
        .get(&key)
        .and_then(Weak::upgrade)
        .filter(|l| l.core.alive())
    {
        return Ok(lease);
    }
    registry.leases.retain(|_, lease| lease.strong_count() > 0);
    if !registry.core.as_ref().is_some_and(|c| c.alive()) {
        registry.core = Some(Core::start()?);
        registry.leases.clear();
    }
    let core = registry.core.as_ref().unwrap().clone();
    // Distinct generations cannot remove a replacement route during concurrent drop/bind.
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).map_err(|_| unavailable())?;
    let id = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
    core.call(json!({"op":"add", "id":id, "config":config}))?;
    let lease = Arc::new(Lease {
        id: id.clone(),
        core,
    });
    registry.leases.insert(key, Arc::downgrade(&lease));
    Ok(lease)
}

#[derive(Clone)]
pub(in crate::network) struct LeaseLayer(pub(in crate::network) Arc<Lease>);
#[derive(Clone)]
pub(in crate::network) struct LeaseService<S> {
    inner: S,
    _lease: Arc<Lease>,
}
impl<S> tower_layer::Layer<S> for LeaseLayer {
    type Service = LeaseService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        LeaseService {
            inner,
            _lease: self.0.clone(),
        }
    }
}
impl<S, R> tower_service::Service<R> for LeaseService<S>
where
    S: tower_service::Service<R>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, request: R) -> Self::Future {
        self.inner.call(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Provider, network::http_client};

    #[tokio::test]
    #[ignore = "requires built helper; set TEEROUTER_EGRESS_BIN"]
    async fn helper_reuses_nodes_recovers_after_exit_and_never_connects_directly() {
        let uri = "vless://12345678-1234-1234-1234-123456789abc@127.0.0.1:1?security=tls&sni=node.invalid&type=xhttp&mode=stream-one";
        let first = bind(Node::parse(uri).unwrap()).unwrap();
        let second = bind(Node::parse(&format!("{uri}#other-label")).unwrap()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let different =
            bind(Node::parse(&uri.replace("node.invalid", "another.invalid")).unwrap()).unwrap();
        assert!(Arc::ptr_eq(&first.core, &different.core));
        assert_ne!(first.id, different.id);
        let client = http_client(Provider::XAI, uri, 2).unwrap();
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/must-not-arrive", target.local_addr().unwrap());
        assert!(client.get(&url).send().await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target.accept())
                .await
                .is_err()
        );
        {
            let mut control = first.core.control.lock().unwrap();
            control.child.kill().unwrap();
            control.child.wait().unwrap();
        }
        assert!(client.get(&url).send().await.is_err());
        let replacement = bind(Node::parse(uri).unwrap()).unwrap();
        assert!(!Arc::ptr_eq(&first.core, &replacement.core));
        assert!(replacement.core.alive());
        // Drop old-generation clients after replacement; their cleanup cannot delete it.
        drop(client);
        drop(first);
        drop(second);
        drop(different);
        assert!(replacement.core.alive());
    }

    #[tokio::test]
    #[ignore = "requires an operator-supplied VLESS_TEST_URI and internet access"]
    async fn live_vless_node_reaches_official_tls_endpoint() {
        let uri = zeroize::Zeroizing::new(std::env::var("VLESS_TEST_URI").expect("set test node"));
        let client = http_client(Provider::XAI, &uri, 30).unwrap();
        // No provider credentials, paid inference, or user request data are sent.
        let response = client.get("https://api.x.ai/v1/models").send().await;
        assert!(response.is_ok(), "VLESS HTTPS connection failed");
        let response = response.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(response.bytes().await.is_ok(), "response body failed");
    }
}

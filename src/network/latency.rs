//! Credential-free, bounded probes through the same explicit egress as accounts.
use crate::{Provider, runtime::Runtime};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) const INTERVAL: Duration = Duration::from_secs(300);
const FRESH: Duration = Duration::from_secs(600);
const TIMEOUT: u64 = 10;

pub(crate) struct Target {
    key: String,
    provider: Provider,
    proxy: zeroize::Zeroizing<String>,
    url: reqwest::Url,
}
impl Target {
    pub(crate) fn new(provider: Provider, proxy: &str, base: &reqwest::Url) -> Arc<Self> {
        let mut url = base.clone();
        url.set_path(&format!("{}/models", base.path().trim_end_matches('/')));
        url.set_query(None);
        url.set_fragment(None);
        let key = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(proxy, url.as_str())).unwrap())
        );
        Arc::new(Self {
            key,
            provider,
            proxy: proxy.to_owned().into(),
            url,
        })
    }
    async fn probe(&self) -> Option<Duration> {
        // Fresh client: consistently include tunnel/TLS setup, never share account
        // authorization headers, OAuth refresh logic or inference connection pools.
        crate::official::validate(self.url.as_str(), Some(self.provider)).ok()?;
        let client = super::http_client(self.provider, &self.proxy, TIMEOUT).ok()?;
        let start = Instant::now();
        let response = client.head(self.url.clone()).send().await.ok()?;
        // An unauthenticated API may return 401, 404 or 405. They still establish
        // reachability; a fast block/rate-limit/server error must not win routing.
        let status = response.status();
        (status.is_success() || matches!(status.as_u16(), 401 | 404 | 405)).then(|| start.elapsed())
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reading {
    Measured(u64), // smoothed microseconds, includes proxy/tunnel/TLS/HTTP headers
    #[default]
    Unknown,
    Failed,
}
impl Reading {
    pub(crate) fn failed(self) -> bool {
        self == Self::Failed
    }
}
struct Sample {
    at: Instant,
    checked_at: u64,
    latency: Option<u64>,
}
/// Display-only view. Never contains the target, proxy, credentials or raw errors.
#[derive(Clone, serde::Serialize)]
pub(crate) struct Snapshot {
    pub status: &'static str,
    pub latency_ms: Option<f64>,
    pub checked_at: Option<u64>,
    pub expires_at: Option<u64>,
}
impl Snapshot {
    pub(crate) fn unavailable() -> Self {
        Self {
            status: "unavailable",
            latency_ms: None,
            checked_at: None,
            expires_at: None,
        }
    }
    /// Best measured link among ready, visible accounts; never average in failures.
    pub(crate) fn fastest(samples: impl Iterator<Item = Self>) -> Self {
        fn rank(s: &Snapshot) -> u8 {
            match s.status {
                "measured" => 0,
                "unknown" => 1,
                "stale" => 2,
                "failed" => 3,
                _ => 4,
            }
        }
        samples
            .min_by(|a, b| {
                rank(a).cmp(&rank(b)).then_with(|| {
                    a.latency_ms
                        .unwrap_or(f64::MAX)
                        .total_cmp(&b.latency_ms.unwrap_or(f64::MAX))
                })
            })
            .unwrap_or_else(Self::unavailable)
    }
}
#[derive(Default)]
pub(crate) struct State {
    samples: Mutex<BTreeMap<String, Sample>>,
}
impl State {
    pub(crate) fn snapshot(&self, target: Option<&Target>) -> Snapshot {
        let samples = self.samples.lock().unwrap();
        let sample = target.and_then(|t| samples.get(&t.key));
        let Some(sample) = sample else {
            return Snapshot {
                status: "unknown",
                ..Snapshot::unavailable()
            };
        };
        let stale = sample.at.elapsed() >= FRESH;
        Snapshot {
            status: if stale {
                "stale"
            } else if sample.latency.is_some() {
                "measured"
            } else {
                "failed"
            },
            latency_ms: if stale {
                None
            } else {
                sample.latency.map(|us| us as f64 / 1000.0)
            },
            checked_at: Some(sample.checked_at),
            expires_at: Some(sample.checked_at.saturating_add(FRESH.as_secs())),
        }
    }
    pub(crate) fn status(&self) -> serde_json::Value {
        let samples = self.samples.lock().unwrap();
        let fresh: Vec<_> = samples
            .values()
            .filter(|s| s.at.elapsed() < FRESH)
            .collect();
        serde_json::json!({"interval_secs":INTERVAL.as_secs(),"sample_ttl_secs":FRESH.as_secs(),
            "targets":samples.len(),"measured":fresh.iter().filter(|s| s.latency.is_some()).count(),
            "failed":fresh.iter().filter(|s| s.latency.is_none()).count(),"stale":samples.len()-fresh.len()})
    }
    pub(crate) fn reading(&self, target: &Target) -> Reading {
        self.samples
            .lock()
            .unwrap()
            .get(&target.key)
            .filter(|s| s.at.elapsed() < FRESH)
            .map_or(Reading::Unknown, |s| {
                s.latency.map_or(Reading::Failed, Reading::Measured)
            })
    }
    pub(crate) fn record(&self, target: &Target, latency: Option<Duration>) {
        let mut samples = self.samples.lock().unwrap();
        let latency = latency.map(|v| {
            let new = v.as_micros().min(u64::MAX as u128) as u64;
            samples
                .get(&target.key)
                .filter(|s| s.at.elapsed() < FRESH)
                .and_then(|s| s.latency)
                .map_or(new, |old| {
                    old.saturating_mul(7).saturating_add(new.saturating_mul(3)) / 10
                })
        });
        samples.insert(
            target.key.clone(),
            Sample {
                at: Instant::now(),
                checked_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                latency,
            },
        );
    }
    async fn refresh(
        &self,
        targets: BTreeMap<String, Arc<Target>>,
        allowed: impl Fn() -> bool + Sync,
    ) {
        let due = {
            let mut samples = self.samples.lock().unwrap();
            samples.retain(|key, _| targets.contains_key(key));
            let mut due: Vec<_> = targets
                .into_values()
                .filter(|t| {
                    samples
                        .get(&t.key)
                        .is_none_or(|s| s.at.elapsed() >= INTERVAL)
                })
                .collect();
            due.sort_by_key(|t| samples.get(&t.key).map(|s| s.at));
            due.truncate(128);
            due
        };
        let allowed = &allowed;
        futures_util::stream::iter(due)
            .for_each_concurrent(8, |target| async move {
                // Bounds asynchronous connection and response-header waits.
                if !allowed() {
                    return;
                }
                let result = tokio::time::timeout(Duration::from_secs(TIMEOUT), target.probe())
                    .await
                    .ok()
                    .flatten();
                self.record(&target, result);
            })
            .await;
    }
}

pub(crate) fn start(runtime: &Arc<Runtime>) {
    if !runtime.is_platform() {
        return;
    }
    let weak = Arc::downgrade(runtime);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let Some(runtime) = weak.upgrade() else { break };
            if let Some(_job) = runtime.background_job() {
                let root = runtime.engine();
                if root.vault.serving() {
                    let mut engines = vec![root.clone()];
                    let ids: Vec<_> = root
                        .vault
                        .lock()
                        .ok()
                        .map(|d| {
                            d.merchants
                                .values()
                                .filter(|m| m.enabled)
                                .map(|m| m.id.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    {
                        let live = runtime.merchants.live.lock().unwrap();
                        engines.extend(
                            ids.iter()
                                .filter_map(|id| live.get(id))
                                .map(|m| m.runtime.engine()),
                        );
                    }
                    let mut targets = BTreeMap::new();
                    for engine in engines {
                        if !engine.vault.serving() {
                            continue;
                        }
                        for target in engine.latency_targets() {
                            if targets.len() >= 30000 {
                                break;
                            }
                            targets.entry(target.key.clone()).or_insert(target);
                        }
                    }
                    runtime
                        .merchants
                        .latency
                        .refresh(targets, || !runtime.quiescing())
                        .await;
                }
            }
            drop(runtime);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn proxy(status: u16) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(socket.read_u8().await.unwrap());
                    assert!(bytes.len() < 4096);
                }
                let headers = String::from_utf8(bytes).unwrap().to_lowercase();
                // Unresolvable destination proves there is no direct connection.
                assert!(headers.starts_with("head http://fixture.invalid/v1/models http/1.1"));
                assert!(!headers.contains("authorization:"));
                assert!(!headers.contains("content-type:"));
                assert!(!headers.contains("cookie:"));
                count.fetch_add(1, Ordering::SeqCst);
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 10000000\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            }
        });
        (url, calls, task)
    }
    fn target(proxy: &str) -> Arc<Target> {
        Target::new(
            Provider::OpenAI,
            proxy,
            &"http://fixture.invalid/v1".parse().unwrap(),
        )
    }
    #[tokio::test]
    async fn probes_use_proxy_without_credentials_inference_or_response_body_and_deduplicate() {
        let (proxy, calls, task) = proxy(401).await;
        let first = target(&proxy);
        let second = target(&proxy);
        let state = State::default();
        let targets = || {
            [first.clone(), second.clone()]
                .into_iter()
                .map(|t| (t.key.clone(), t))
                .collect()
        };
        state.refresh(targets(), || true).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(state.reading(&first), Reading::Measured(_)));
        state.refresh(targets(), || true).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        state
            .samples
            .lock()
            .unwrap()
            .get_mut(&first.key)
            .unwrap()
            .at -= INTERVAL;
        state.refresh(targets(), || false).await; // quiesce starts no queued work
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        state.refresh(targets(), || true).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        state.refresh(BTreeMap::new(), || true).await;
        assert_eq!(state.reading(&first), Reading::Unknown);
        task.abort();
    }
    #[tokio::test]
    async fn blocked_and_unavailable_endpoints_do_not_look_fast() {
        for status in [403, 429, 500, 302] {
            let (proxy, _, task) = proxy(status).await;
            assert_eq!(target(&proxy).probe().await, None);
            task.abort();
        }
        for status in [200, 404, 405] {
            let (proxy, _, task) = proxy(status).await;
            assert!(target(&proxy).probe().await.is_some());
            task.abort();
        }
    }
    #[test]
    fn samples_are_scoped_to_proxy_and_entry_expire_and_do_not_reuse_failures() {
        let state = State::default();
        let first = target("http://user:private@proxy.test:80");
        assert!(!first.key.contains("private"));
        state.record(&first, Some(Duration::from_millis(200)));
        state.record(&first, Some(Duration::from_millis(100)));
        assert_eq!(state.reading(&first), Reading::Measured(170000));
        assert_eq!(
            state.reading(&target("http://other@proxy.test:80")),
            Reading::Unknown
        );
        let other_host = Target::new(
            Provider::OpenAI,
            &first.proxy,
            &"https://chatgpt.com/backend-api/codex".parse().unwrap(),
        );
        assert_eq!(state.reading(&other_host), Reading::Unknown);
        state.record(&first, None);
        assert_eq!(state.reading(&first), Reading::Failed);
        state.record(&first, Some(Duration::from_millis(300)));
        assert_eq!(state.reading(&first), Reading::Measured(300000));
        state
            .samples
            .lock()
            .unwrap()
            .get_mut(&first.key)
            .unwrap()
            .at -= FRESH;
        assert_eq!(state.reading(&first), Reading::Unknown);
        state.record(&first, Some(Duration::from_millis(100)));
        assert_eq!(state.reading(&first), Reading::Measured(100000));
    }
    #[test]
    fn display_snapshots_expire_hide_targets_and_choose_fastest_fresh_account() {
        let state = State::default();
        let a = target("http://private:secret@proxy.test:80");
        let b = target("http://other:secret@proxy.test:80");
        assert_eq!(state.snapshot(Some(&a)).status, "unknown");
        state.record(&a, Some(Duration::from_millis(240)));
        state.record(&b, Some(Duration::from_millis(120)));
        let best =
            Snapshot::fastest([state.snapshot(Some(&a)), state.snapshot(Some(&b))].into_iter());
        assert_eq!(best.latency_ms, Some(120.0));
        assert_eq!(best.expires_at.unwrap() - best.checked_at.unwrap(), 600);
        let value = serde_json::to_string(&best).unwrap();
        for secret in ["secret", "proxy.test", &a.key, &b.key] {
            assert!(!value.contains(secret));
        }
        state.samples.lock().unwrap().get_mut(&b.key).unwrap().at -= FRESH;
        let stale = state.snapshot(Some(&b));
        assert_eq!(stale.status, "stale");
        assert_eq!(stale.latency_ms, None);
        assert_eq!(
            Snapshot::fastest([stale, state.snapshot(Some(&a))].into_iter()).latency_ms,
            Some(240.0)
        );
        state.record(&a, None);
        assert_eq!(state.snapshot(Some(&a)).status, "failed");
        assert_eq!(state.snapshot(Some(&a)).latency_ms, None);
        assert_eq!(Snapshot::fastest(std::iter::empty()).status, "unavailable");
    }
}

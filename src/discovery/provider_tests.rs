use super::*;
use axum::{Json, Router, routing::any};
use std::sync::atomic::AtomicUsize;

struct Upstream {
    url: String,
    code: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    hold: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn upstream() -> Upstream {
    let code = Arc::new(AtomicUsize::new(200));
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let hold = Arc::new(AtomicBool::new(false));
    let (co, ca, en, re, ho) = (
        code.clone(),
        calls.clone(),
        entered.clone(),
        release.clone(),
        hold.clone(),
    );
    let app = Router::new().fallback(any(move || {
        let (co, ca, en, re, ho) = (co.clone(), ca.clone(), en.clone(), re.clone(), ho.clone());
        async move {
            ca.fetch_add(1, Ordering::SeqCst);
            en.notify_one();
            if ho.load(Ordering::SeqCst) {
                re.notified().await;
            }
            (
                axum::http::StatusCode::from_u16(co.load(Ordering::SeqCst) as u16).unwrap(),
                Json(json!({"data":[{"id":"model-test"}]})),
            )
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream {
        url,
        code,
        calls,
        entered,
        release,
        hold,
        task,
    }
}
fn catalog(url: &str, provider: Provider) -> Arc<Catalog> {
    let client = crate::Client::new(provider, "test-secret")
        .unwrap()
        .with_base_url(&format!("{url}/v1"))
        .unwrap();
    let deployment =
        serde_json::from_value(json!({"id":"account","provider":provider,"api_key_ref":"key"}))
            .unwrap();
    Catalog::new(
        "pool",
        &[(deployment, client, "binding".into())],
        Discovery::default(),
        crate::vault::Vault::memory().unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn provider_refresh_falls_back_then_clears_on_next_five_minute_check_and_recovers() {
    let a = upstream().await;
    let b = upstream().await;
    a.code.store(503, Ordering::SeqCst);
    let registry = Registry::new(&Discovery::default(), &BTreeMap::new());
    let enabled = Arc::new(AtomicBool::new(true));
    let ca = catalog(&a.url, Provider::OpenAI);
    let cb = catalog(&b.url, Provider::OpenAI);
    registry.replace("a", &enabled, vec![ca.clone()]);
    registry.replace("b", &enabled, vec![cb.clone()]);
    ca.refresh(false).await;
    assert_eq!(a.calls.load(Ordering::SeqCst), 1);
    assert_eq!(b.calls.load(Ordering::SeqCst), 1);
    assert!(ca.models().contains("model-test"));
    assert_eq!(ca.models(), cb.models());
    b.code.store(503, Ordering::SeqCst);
    cb.refresh(false).await;
    assert_eq!(
        b.calls.load(Ordering::SeqCst),
        1,
        "no per-pool or per-request probing"
    );
    let shared = &registry.providers[&Provider::OpenAI];
    shared.refresh.lock().await.completed = Some(Instant::now() - Duration::from_secs(300));
    shared
        .snapshot
        .write()
        .unwrap()
        .as_mut()
        .unwrap()
        .checked_at = Instant::now() - Duration::from_secs(300);
    cb.refresh(false).await;
    assert!(ca.models().is_empty());
    assert!(cb.models().is_empty());
    assert!(shared.snapshot.read().unwrap().is_none());
    assert_eq!(ca.status()["complete_catalog"], false);
    assert!(ca.evidence("model-test").is_err());
    assert_eq!(a.calls.load(Ordering::SeqCst), 2);
    assert_eq!(b.calls.load(Ordering::SeqCst), 2);
    b.code.store(200, Ordering::SeqCst);
    ca.refresh(true).await;
    assert!(cb.models().contains("model-test"));
}

#[tokio::test]
async fn removed_source_cannot_publish_in_flight_catalog_and_is_not_retained() {
    let up = upstream().await;
    up.hold.store(true, Ordering::SeqCst);
    let registry = Registry::new(&Discovery::default(), &BTreeMap::new());
    let enabled = Arc::new(AtomicBool::new(true));
    let catalog = catalog(&up.url, Provider::OpenAI);
    registry.replace("merchant", &enabled, vec![catalog.clone()]);
    let weak = Arc::downgrade(&catalog);
    let refresh = {
        let catalog = catalog.clone();
        tokio::spawn(async move { catalog.refresh(true).await })
    };
    up.entered.notified().await;
    registry.remove("merchant");
    up.release.notify_one();
    refresh.await.unwrap();
    assert!(catalog.models().is_empty());
    assert_eq!(catalog.status()["last_error"], "account_changed");
    drop(catalog);
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn equivalent_committed_source_keeps_inflight_evidence_through_unrelated_config_update() {
    let up = upstream().await;
    up.hold.store(true, Ordering::SeqCst);
    let registry = Registry::new(&Discovery::default(), &BTreeMap::new());
    let enabled = Arc::new(AtomicBool::new(true));
    let old = catalog(&up.url, Provider::OpenAI);
    registry.replace("merchant", &enabled, vec![old.clone()]);
    let refresh = {
        let old = old.clone();
        tokio::spawn(async move { old.refresh(true).await })
    };
    up.entered.notified().await;
    let current = catalog(&up.url, Provider::OpenAI);
    registry.replace("merchant", &enabled, vec![current.clone()]);
    up.release.notify_one();
    refresh.await.unwrap();
    assert!(current.models().contains("model-test"));
    current.refresh(false).await;
    assert_eq!(up.calls.load(Ordering::SeqCst), 1);
}

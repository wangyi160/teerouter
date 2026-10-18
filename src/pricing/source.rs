//! Platform-owned daily GitHub price sync, persisted with encrypted checkpoints.
use super::*;
use axum::{
    Json,
    extract::{Request, State as HttpState},
};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const DEFAULT_URL: &str =
    "https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json";
pub const INTERVAL: u64 = 86400;
const LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct State {
    pub revision: u64,
    pub enabled: bool,
    pub url: String,
    pub last_attempt: u64,
    pub last_success: u64,
    pub last_error: Option<String>,
    pub sha256: Option<String>,
    pub git_blob: Option<String>,
    pub history: Vec<Value>,
    pub unpriced_models: Vec<String>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: false,
            url: DEFAULT_URL.into(),
            last_attempt: 0,
            last_success: 0,
            last_error: None,
            sha256: None,
            git_blob: None,
            history: vec![],
            unpriced_models: vec![],
        }
    }
}
fn invalid() -> ApiError {
    ApiError::new(
        400,
        "invalid_pricing_source",
        "Use a public HTTPS GitHub blob or raw JSON URL without credentials, query or fragment",
    )
}
fn unavailable() -> ApiError {
    ApiError::new(
        502,
        "pricing_fetch_failed",
        "Could not fetch the GitHub price source; previous prices retained",
    )
}
fn now() -> u64 {
    crate::oauth::tokens::now() as u64
}

/// A branch, tag or commit SHA is a single unambiguous ref segment.
pub(super) fn github(url: &str) -> Result<reqwest::Url, ApiError> {
    let u = reqwest::Url::parse(url).map_err(|_| invalid())?;
    if url.len() > 2048
        || u.scheme() != "https"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(invalid());
    }
    let mut p: Vec<_> = u.path_segments().ok_or_else(invalid)?.collect();
    match u.host_str() {
        Some("github.com") if p.len() >= 5 && p[2] == "blob" => {
            p.remove(2);
        }
        Some("raw.githubusercontent.com") if p.len() >= 4 => {}
        _ => return Err(invalid()),
    }
    if p.iter().any(|s| {
        s.is_empty()
            || *s == "."
            || *s == ".."
            || s.contains('%')
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    }) || !p.last().unwrap().ends_with(".json")
    {
        return Err(invalid());
    }
    let mut api = reqwest::Url::parse(&format!(
        "https://api.github.com/repos/{}/{}/contents/{}",
        p[0],
        p[1],
        p[3..].join("/")
    ))
    .map_err(|_| invalid())?;
    api.query_pairs_mut().append_pair("ref", p[2]);
    Ok(api)
}
pub(crate) fn validate(state: &State) -> Result<(), ApiError> {
    github(&state.url)?;
    if state.history.len() > 32
        || state.unpriced_models.len() > 8192
        || state.last_error.as_ref().is_some_and(|s| s.len() > 128)
    {
        return Err(invalid());
    }
    Ok(())
}
pub(super) fn due(s: &State, time: u64) -> bool {
    s.enabled && (s.last_attempt == 0 || time >= s.last_attempt.saturating_add(INTERVAL))
}
fn status(s: &State, running: bool) -> Value {
    let mut v = serde_json::to_value(s).unwrap();
    v["interval_secs"] = INTERVAL.into();
    v["next_update_at"] = if s.enabled {
        s.last_attempt.saturating_add(INTERVAL).into()
    } else {
        Value::Null
    };
    v["running"] = running.into();
    v
}
pub(crate) async fn get(
    HttpState(runtime): HttpState<Arc<Runtime>>,
) -> Result<Json<Value>, ApiError> {
    let s = runtime
        .engine()
        .vault
        .lock()
        .map_err(storage)?
        .pricing_source
        .clone();
    Ok(Json(status(&s, runtime.pricing_sync.try_lock().is_err())))
}
pub(crate) async fn put(
    HttpState(runtime): HttpState<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Update {
        expected_revision: u64,
        enabled: bool,
        url: String,
    }
    let (_, body) = crate::server::admin::input(&runtime, request).await?;
    let input: Update = serde_json::from_value(body).map_err(|_| invalid())?;
    github(&input.url)?;
    let engine = runtime.engine();
    if !engine.vault.initialized() {
        return Err(ApiError::new(
            409,
            "not_initialized",
            "Initialize router first",
        ));
    }
    let mut data = engine.vault.lock().map_err(storage)?;
    if data.pricing_source.revision != input.expected_revision {
        return Err(ApiError::new(
            409,
            "pricing_source_conflict",
            "Reload pricing source configuration",
        ));
    }
    let s = &mut data.pricing_source;
    s.revision = s.revision.checked_add(1).ok_or_else(invalid)?;
    if s.url != input.url || s.enabled != input.enabled {
        s.last_attempt = 0;
    }
    s.url = input.url;
    s.enabled = input.enabled;
    s.last_error = None;
    let result = status(s, runtime.pricing_sync.try_lock().is_err());
    data.changed();
    Ok(Json(result))
}
pub(crate) async fn refresh(
    HttpState(runtime): HttpState<Arc<Runtime>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let (_, body) = crate::server::admin::input(&runtime, request).await?;
    if body.as_object().is_none_or(|o| !o.is_empty()) {
        return Err(invalid());
    }
    let engine = runtime.engine();
    if !engine.vault.initialized() || !engine.vault.serving() {
        return Err(ApiError::new(
            503,
            "not_ready",
            "Router must be initialized and serving before refreshing prices",
        ));
    }
    let guard = runtime.pricing_sync.clone().try_lock_owned().map_err(|_| {
        ApiError::new(
            409,
            "pricing_sync_running",
            "A pricing update is already running",
        )
    })?;
    let mut result = status(&engine.vault.lock().map_err(storage)?.pricing_source, true);
    result["accepted"] = true.into();
    tokio::spawn(async move {
        let _guard = guard;
        let _ = run(&runtime, true).await;
    });
    Ok(Json(result))
}

async fn json(client: &reqwest::Client, url: reqwest::Url) -> Result<Value, ApiError> {
    let mut response = client.get(url).send().await.map_err(|_| unavailable())?;
    if !response.status().is_success()
        || response.content_length().is_some_and(|n| n > LIMIT as u64)
    {
        return Err(unavailable());
    }
    let mut bytes = vec![];
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if bytes.len() + chunk.len() > LIMIT {
            return Err(unavailable());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| unavailable())
}
pub(super) async fn fetch(
    client: &reqwest::Client,
    url: &str,
) -> Result<(Vec<u8>, String), ApiError> {
    let api = github(url)?;
    let meta = json(client, api.clone()).await?;
    let sha = meta["sha"]
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(unavailable)?
        .to_owned();
    if meta["type"] != "file" || meta["size"].as_u64().is_none_or(|n| n > LIMIT as u64) {
        return Err(unavailable());
    }
    let blob = if meta["encoding"] == "base64"
        && meta["content"].as_str().is_some_and(|s| !s.is_empty())
    {
        meta
    } else {
        // Rebuild the URL from validated components; never follow URLs from remote JSON.
        let parts: Vec<_> = api.path_segments().unwrap().collect();
        json(
            client,
            reqwest::Url::parse(&format!(
                "https://api.github.com/repos/{}/{}/git/blobs/{sha}",
                parts[1], parts[2]
            ))
            .map_err(|_| unavailable())?,
        )
        .await?
    };
    if blob["encoding"] != "base64" || blob["sha"] != sha {
        return Err(unavailable());
    }
    let content = blob["content"].as_str().ok_or_else(unavailable)?;
    let encoded: Vec<u8> = content
        .bytes()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| unavailable())?;
    if bytes.is_empty() || bytes.len() > LIMIT || blob["size"].as_u64() != Some(bytes.len() as u64)
    {
        return Err(unavailable());
    }
    Ok((bytes, sha))
}

pub(super) fn replace(old: &Book, mut incoming: Vec<ProviderBook>) -> Result<Book, ApiError> {
    let mut changed = false;
    for next in &mut incoming {
        if let Some(previous) = old.providers.iter().find(|p| p.provider == next.provider) {
            if previous.source == next.source
                && serde_json::to_value(&previous.models).unwrap()
                    == serde_json::to_value(&next.models).unwrap()
            {
                *next = previous.clone();
                continue;
            }
            next.version = previous.version.checked_add(1).ok_or_else(invalid)?;
        }
        changed = true;
    }
    incoming.extend(
        old.providers
            .iter()
            .filter(|p| p.provider.is_local_test())
            .cloned(),
    );
    let book = Book {
        revision: if changed {
            old.revision.checked_add(1).ok_or_else(invalid)?
        } else {
            old.revision
        },
        providers: incoming,
    };
    super::validate(&book)?;
    Ok(book)
}
pub(crate) async fn sync(runtime: &Runtime, manual: bool) -> Result<(), ApiError> {
    let _sync = runtime.pricing_sync.try_lock().map_err(|_| {
        ApiError::new(
            409,
            "pricing_sync_running",
            "A pricing update is already running",
        )
    })?;
    run(runtime, manual).await
}

async fn run(runtime: &Runtime, manual: bool) -> Result<(), ApiError> {
    let _job = runtime.background_job().ok_or_else(|| {
        ApiError::new(
            503,
            "router_draining",
            "Router is paused for an administrative shutdown",
        )
    })?;
    let engine = runtime.engine();
    if !engine.vault.initialized() || !engine.vault.serving() {
        return Err(ApiError::new(
            503,
            "not_ready",
            "Router must be initialized and serving before refreshing prices",
        ));
    }
    let config = {
        let mut data = engine.vault.lock().map_err(storage)?;
        if !manual && !due(&data.pricing_source, now()) {
            return Ok(());
        }
        data.pricing_source.last_attempt = now();
        data.changed();
        data.pricing_source.clone()
    };
    let result = async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("teerouter-pricing/1")
            .connect_timeout(std::time::Duration::from_secs(8))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|_| unavailable())?;
        let (bytes, sha) = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            fetch(&client, &config.url),
        )
        .await
        .map_err(|_| unavailable())??;
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let imported = super::litellm::parse(&bytes, &config.url, now())?;
        Ok::<_, ApiError>((imported, sha, hash))
    }
    .await;
    let mut data = engine.vault.lock().map_err(storage)?;
    finish(&mut data, &config, result)
}
type Download = Result<(super::litellm::Import, String, String), ApiError>;
pub(super) fn finish(
    data: &mut crate::vault::Data,
    config: &State,
    result: Download,
) -> Result<(), ApiError> {
    if data.pricing_source.revision != config.revision {
        return Err(ApiError::new(
            409,
            "pricing_source_conflict",
            "Pricing source changed during fetch; fetched prices discarded",
        ));
    }
    let outcome=result.and_then(|(imported,sha,hash)|{
        let next=replace(&data.official_pricing,imported.providers)?;
        let changed=next.revision!=data.official_pricing.revision;
        let revision=next.revision;
        data.official_pricing=next;
        let s=&mut data.pricing_source;
        s.unpriced_models=imported.unpriced_models;
        s.last_success=now();s.last_error=None;s.sha256=Some(hash.clone());s.git_blob=Some(sha.clone());
        s.history.push(json!({"at":s.last_success,"revision":revision,"changed":changed,"url":config.url,"sha256":hash,"git_blob":sha}));
        if s.history.len()>32 {s.history.remove(0);}
        Ok(())
    });
    if let Err(ref e) = outcome {
        data.pricing_source.last_error = Some(e.code.into());
    }
    data.changed();
    outcome
}
pub(crate) fn start(runtime: &Arc<Runtime>) {
    if !runtime.is_platform() {
        return;
    }
    let weak = Arc::downgrade(runtime);
    tokio::spawn(async move {
        loop {
            let Some(runtime) = weak.upgrade() else { break };
            let _ = sync(&runtime, false).await;
            drop(runtime);
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

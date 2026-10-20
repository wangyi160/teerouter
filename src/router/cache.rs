//! Soft account affinity and conservative prompt-cache estimates. Only hashes,
//! byte counts and observed token counters are retained, never prompt text.
use super::*;
use sha2::{Digest, Sha256};
use std::sync::Mutex;

const CAPACITY: usize = 4096;
const STICKY_TTL: Duration = Duration::from_secs(86400);
// A routing hint, not a promise about a provider's cache lifetime.
const WARM_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub(crate) struct Request {
    key: [u8; 32],
    prefixes: Vec<([u8; 32], u64)>,
    bytes: u64,
    output: u64,
    started: Instant,
}

struct HashWriter<'a>(&'a mut Sha256, u64);
impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        self.1 += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn append(hash: &mut Sha256, value: &Value) -> u64 {
    let mut writer = HashWriter(hash, 0);
    serde_json::to_writer(&mut writer, value).expect("JSON value serialization");
    let bytes = writer.1;
    hash.update([0]);
    bytes
}

impl Request {
    pub(crate) fn new(
        owner: &str,
        protocol: Protocol,
        model: &str,
        body: &Value,
        headers: &HeaderMap,
    ) -> Self {
        let mut scope = Sha256::new();
        append(
            &mut scope,
            &json!([
                owner,
                model,
                match protocol {
                    Protocol::Chat => "chat",
                    Protocol::Messages => "messages",
                    Protocol::Responses => "responses",
                }
            ]),
        );
        let explicit = [
            "x-router-session",
            "x-grok-conv-id",
            "session_id",
            "x-session-id",
            "x-conversation-id",
        ]
        .iter()
        .find_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()))
        .or_else(|| body["prompt_cache_key"].as_str())
        .filter(|id| !id.is_empty() && id.len() <= 512);
        let mut prefix = scope.clone();
        append(
            &mut prefix,
            &json!([
                body.get("prompt_cache_key"),
                headers.get("anthropic-beta").and_then(|v| v.to_str().ok())
            ]),
        );
        // Include every non-history field conservatively, except transport,
        // output limits and caller identifiers which are not prompt content.
        let mut context = body.as_object().cloned().unwrap_or_default();
        for key in [
            "model",
            "messages",
            "input",
            "stream",
            "stream_options",
            "max_tokens",
            "max_completion_tokens",
            "max_output_tokens",
            "metadata",
            "user",
            "prompt_cache_key",
            "previous_response_id",
            "store",
        ] {
            context.remove(key);
        }
        let mut bytes = append(&mut prefix, &Value::Object(context));
        let mut prefixes = vec![(prefix.clone().finalize().into(), bytes)];
        let history = body
            .get("messages")
            .or_else(|| body.get("input"))
            .unwrap_or(&Value::Null);
        let items: Vec<&Value> = match history {
            Value::Array(a) => a.iter().collect(),
            v => vec![v],
        };
        let mut first = None;
        for (i, item) in items.iter().enumerate() {
            bytes += append(&mut prefix, item);
            let digest: [u8; 32] = prefix.clone().finalize().into();
            if first.is_none() && !matches!(item["role"].as_str(), Some("system" | "developer")) {
                first = Some(digest);
            }
            // Keep the start and tail, with a strict memory bound for huge histories.
            if i < 32 || i + 32 >= items.len() {
                prefixes.push((digest, bytes));
            }
        }
        if let Some(id) = explicit {
            append(&mut scope, &json!(["explicit", id]));
        } else {
            scope.update(b"prefix\0");
            scope.update(first.unwrap_or_else(|| prefix.finalize().into()));
        }
        let output = ["max_output_tokens", "max_completion_tokens", "max_tokens"]
            .iter()
            .find_map(|k| body[*k].as_u64())
            .unwrap_or(1024)
            .min(1_000_000);
        Self {
            key: scope.finalize().into(),
            prefixes,
            bytes,
            output,
            started: Instant::now(),
        }
    }
    pub(crate) fn identity(&self) -> String {
        self.key.iter().map(|b| format!("{b:02x}")).collect()
    }
    fn input(&self) -> u64 {
        self.bytes.div_ceil(3).max(1)
    }
    fn overlap(&self, other: &Self) -> u64 {
        self.prefixes
            .iter()
            .filter(|p| other.prefixes.contains(p))
            .map(|p| p.1)
            .max()
            .unwrap_or(0)
    }
}

struct Entry {
    created: Instant,
    request: Request,
    binding: String,
    touched: Instant,
    warm: Option<Instant>,
    tokens: upstream::Tokens,
    discount: bool,
}

#[derive(Default)]
pub(crate) struct State {
    entries: Mutex<Vec<Entry>>,
}

impl State {
    pub(super) fn invalidate(&self, binding: &str) {
        self.entries
            .lock()
            .unwrap()
            .retain(|e| e.binding != binding);
    }
    fn preferred<'a>(entries: &'a [Entry], request: &Request) -> Option<&'a str> {
        entries
            .iter()
            .filter(|e| e.request.key == request.key && e.touched.elapsed() < STICKY_TTL)
            .max_by_key(|e| e.touched)
            .map(|e| e.binding.as_str())
    }
    pub(crate) fn sticky(&self, request: &Request, binding: &str) -> bool {
        Self::preferred(&self.entries.lock().unwrap(), request) == Some(binding)
    }
    // Account acquisition and reservation share a lock, so concurrent first turns
    // do not independently rotate accounts. Hard affinity is checked by the caller.
    pub(super) fn select(
        &self,
        request: &Request,
        select: impl FnOnce(Option<&str>) -> Option<Lease>,
    ) -> Option<Lease> {
        let mut entries = self.entries.lock().unwrap();
        let lease = select(Self::preferred(&entries, request))?;
        let binding = Backend::binding(&lease.backend.config);
        if let Some(e) = entries
            .iter_mut()
            .find(|e| e.request.key == request.key && e.binding == binding)
        {
            e.touched = Instant::now();
        } else {
            if entries.len() >= CAPACITY {
                let oldest = entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.touched)
                    .unwrap()
                    .0;
                entries.swap_remove(oldest);
            }
            entries.push(Entry {
                created: request.started,
                request: request.clone(),
                binding,
                touched: Instant::now(),
                warm: None,
                tokens: Default::default(),
                discount: true,
            });
        }
        Some(lease)
    }
    pub(super) fn complete(&self, request: &Request, backend: &Backend, tokens: upstream::Tokens) {
        if !tokens.known {
            return;
        }
        let binding = Backend::binding(&backend.config);
        let mut entries = self.entries.lock().unwrap();
        if let Some(e) = entries
            .iter_mut()
            .find(|e| e.request.key == request.key && e.binding == binding)
        {
            if request.started < e.created || e.warm.is_some_and(|t| t > request.started) {
                return;
            }
            let overlap = request.overlap(&e.request);
            // A repeat with no cache use disproves our optimistic warm-up hint.
            // Retain stickiness, but stop claiming a discount until usage proves it.
            e.discount = tokens.cache_read > 0
                || tokens.cache_write > 0
                || tokens.cache_write_hour > 0
                || e.warm.is_none()
                || overlap < e.request.bytes / 2;
            e.request = request.clone();
            e.tokens = tokens;
            e.warm = Some(request.started);
        }
    }
    pub(super) fn failed(&self, request: &Request, backend: &Backend) {
        let binding = Backend::binding(&backend.config);
        if let Some(e) = self
            .entries
            .lock()
            .unwrap()
            .iter_mut()
            .find(|e| e.request.key == request.key && e.binding == binding)
        {
            e.warm = None;
            e.discount = false;
        }
    }
    pub(crate) fn estimate(
        &self,
        request: &Request,
        binding: &str,
        provider: crate::Provider,
        body: &Value,
        quote: &crate::pricing::Quote,
    ) -> Option<u128> {
        let input = request.input();
        let rates = crate::billing::routing_rates(quote, body, input)?;
        let entries = self.entries.lock().unwrap();
        let warm = entries.iter().find(|e| {
            e.request.key == request.key
                && e.binding == binding
                && e.discount
                && e.warm.is_some_and(|t| t.elapsed() < WARM_TTL)
        });
        let read = warm.map_or(0, |e| {
            let available = if provider == crate::Provider::Anthropic {
                e.tokens
                    .cache_read
                    .saturating_add(e.tokens.cache_write)
                    .saturating_add(e.tokens.cache_write_hour)
            } else if e.tokens.input >= 1024 {
                e.tokens.input
            } else {
                0
            };
            // Tokenization and server routing are unknown. Use only half the
            // reusable observed prefix, never assume a guaranteed full cache hit.
            let overlap = request.overlap(&e.request);
            let reusable = (u128::from(e.tokens.input) * u128::from(overlap)
                / u128::from(e.request.bytes.max(1))) as u64;
            available.min(reusable).min(input) / 2
        });
        let read = if rates.cache_read.is_some() { read } else { 0 };
        let hour = has_hour_cache(body);
        let write_rate = if hour {
            rates.cache_write_hour
        } else {
            rates.cache_write
        };
        // Claude's automatic cache writes are charged separately. Other providers
        // use ordinary input unless their price book explicitly charges writes.
        let cold_rate = if provider == crate::Provider::Anthropic {
            write_rate.unwrap_or(rates.input).max(rates.input)
        } else {
            rates.input.max(write_rate.unwrap_or(0))
        };
        let cost = u128::from(input - read) * u128::from(cold_rate)
            + u128::from(read) * u128::from(rates.cache_read.unwrap_or(rates.input))
            + u128::from(request.output) * u128::from(rates.output);
        cost.checked_mul(u128::from(quote.multiplier_millionths))
    }
}

fn has_hour_cache(body: &Value) -> bool {
    match body {
        Value::Object(o) => {
            o.get("cache_control").is_some_and(|v| v["ttl"] == "1h")
                || o.values().any(has_hour_cache)
        }
        Value::Array(a) => a.iter().any(has_hour_cache),
        _ => false,
    }
}
fn has_cache_control(body: &Value) -> bool {
    match body {
        Value::Object(o) => o.contains_key("cache_control") || o.values().any(has_cache_control),
        Value::Array(a) => a.iter().any(has_cache_control),
        _ => false,
    }
}
pub(super) fn prepare(
    request: &Request,
    provider: crate::Provider,
    protocol: Protocol,
    body: &mut Value,
    client: &mut Client,
    headers: &mut HeaderMap,
) {
    let id = request.identity();
    match provider {
        crate::Provider::OpenAI => {
            // Also survives Chat -> Responses translation and OAuth auth headers.
            if body.get("prompt_cache_key").is_none() {
                body["prompt_cache_key"] = id.clone().into();
            }
            client.cache_session("session_id", &id);
            headers.remove("session_id");
        }
        crate::Provider::XAI => {
            if protocol == Protocol::Chat {
                client.cache_session("x-grok-conv-id", &id);
            } else if body.get("prompt_cache_key").is_none() {
                body["prompt_cache_key"] = id.into();
            }
        }
        crate::Provider::Anthropic if !has_cache_control(body) => {
            body["cache_control"] = json!({"type":"ephemeral"});
        }
        #[allow(unreachable_patterns)]
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(body: &Value) -> Request {
        Request::new("alice", Protocol::Chat, "model", body, &HeaderMap::new())
    }
    fn quote() -> crate::pricing::Quote {
        crate::pricing::Quote {
            price: serde_json::from_value(json!({"pool":"p","model":"model","input":1000000,"output":2000000,"cache_read":100000,"cache_write":1250000,"cache_write_hour":2000000})).unwrap(),
            basis: "official_pool_multiplier", multiplier_millionths: 1000000,
            official_price_revision: 1, official_version: 1, official_source: String::new(),
            official_checked_at: 1, billing_rules: Default::default(),
        }
    }
    fn warmed(body: &Value) -> State {
        State {
            entries: Mutex::new(vec![Entry {
                created: Instant::now(),
                request: request(body),
                binding: "a".into(),
                touched: Instant::now(),
                warm: Some(Instant::now()),
                tokens: upstream::Tokens {
                    input: 4000,
                    output: 1,
                    cache_read: 3000,
                    known: true,
                    ..Default::default()
                },
                discount: true,
            }]),
        }
    }
    #[test]
    fn history_hashes_follow_append_only_turns_and_are_scoped() {
        let first = json!({"messages":[{"role":"system","content":"system"},{"role":"user","content":"question"}]});
        let mut next = first.clone();
        next["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":"answer"}),
            json!({"role":"user","content":"next"}),
        ]);
        let a = request(&first);
        let b = request(&next);
        assert_eq!(a.key, b.key);
        assert_eq!(a.bytes, a.overlap(&b));
        assert_ne!(
            a.key,
            Request::new("bob", Protocol::Chat, "model", &first, &HeaderMap::new()).key
        );
        assert_ne!(
            a.key,
            Request::new("alice", Protocol::Chat, "other", &first, &HeaderMap::new()).key
        );
        assert_ne!(
            a.key,
            Request::new(
                "alice",
                Protocol::Messages,
                "model",
                &first,
                &HeaderMap::new()
            )
            .key
        );
        next["messages"][0]["content"] = "changed system".into();
        assert_ne!(a.key, request(&next).key);
        let mut h = HeaderMap::new();
        h.insert("session_id", "explicit".parse().unwrap());
        let explicit = |v| Request::new("alice", Protocol::Chat, "model", v, &h);
        assert_eq!(explicit(&first).key, explicit(&next).key);
        assert!(explicit(&first).overlap(&explicit(&next)) < a.bytes);
    }
    #[test]
    fn estimate_uses_account_local_prefix_cache_expiry_and_current_prices() {
        let body = json!({"messages":[{"role":"user","content":"x".repeat(12000)}],"max_tokens":1});
        let state = warmed(&body);
        let req = request(&body);
        let q = quote();
        let cost = |s: &State, b: &str, q: &crate::pricing::Quote| {
            s.estimate(&req, b, crate::Provider::OpenAI, &body, q)
                .unwrap()
        };
        let cold = cost(&state, "b", &q);
        let warm = cost(&state, "a", &q);
        assert!(warm < cold);
        let mut dearer = q.clone();
        dearer.multiplier_millionths = 1200000;
        assert!(cost(&state, "a", &dearer) < cold);
        dearer.multiplier_millionths = 10000000;
        assert!(cost(&state, "a", &dearer) > cold);
        state.entries.lock().unwrap()[0].warm = Some(Instant::now() - Duration::from_secs(301));
        assert_eq!(cost(&state, "a", &q), cold);
        state.entries.lock().unwrap()[0].warm = Some(Instant::now());
        state.entries.lock().unwrap()[0].discount = false;
        assert_eq!(cost(&state, "a", &q), cold);
        state.invalidate("a");
        assert!(!state.sticky(&req, "a"));
    }
    #[test]
    fn claude_cache_write_and_long_context_rates_are_included() {
        let body = json!({"messages":[{"role":"user","content":"x".repeat(12000)}],"max_tokens":1,"cache_control":{"type":"ephemeral","ttl":"1h"}});
        let s = State::default();
        let req = request(&body);
        let mut q = quote();
        let hourly = s
            .estimate(&req, "a", crate::Provider::Anthropic, &body, &q)
            .unwrap();
        let normal = s
            .estimate(&req, "a", crate::Provider::Anthropic, &json!({}), &q)
            .unwrap();
        assert!(hourly > normal);
        q.price.long_context = Some(serde_json::from_value(json!({"above_input_tokens":2000,"rates":{"input":3000000,"output":6000000,"cache_read":300000,"cache_write":3750000,"cache_write_hour":6000000}})).unwrap());
        assert_eq!(
            s.estimate(&req, "a", crate::Provider::Anthropic, &body, &q)
                .unwrap(),
            hourly * 3
        );
        let warm = warmed(&body);
        assert!(
            warm.estimate(&req, "a", crate::Provider::Anthropic, &body, &q)
                .unwrap()
                < s.estimate(&req, "a", crate::Provider::Anthropic, &body, &q)
                    .unwrap()
        );
    }
    #[tokio::test]
    async fn scoped_provider_identity_and_claude_breakpoints_survive_preparation() {
        let original = json!({"messages":[{"role":"user","content":"hello"}]});
        let req = request(&original);
        for (provider, name) in [
            (crate::Provider::OpenAI, "session_id"),
            (crate::Provider::XAI, "x-grok-conv-id"),
        ] {
            let mut client = Client::new(provider, "fixture-key").unwrap();
            let mut body = original.clone();
            let mut h = HeaderMap::new();
            prepare(
                &req,
                provider,
                Protocol::Chat,
                &mut body,
                &mut client,
                &mut h,
            );
            let h = client.auth_headers(h).await.unwrap();
            assert_eq!(h[name], req.identity());
        }
        let mut client = Client::new(crate::Provider::Anthropic, "fixture-key").unwrap();
        let mut body = original.clone();
        prepare(
            &req,
            crate::Provider::Anthropic,
            Protocol::Chat,
            &mut body,
            &mut client,
            &mut HeaderMap::new(),
        );
        let converted = crate::anthropic::request(body, false).unwrap();
        assert_eq!(converted["cache_control"], json!({"type":"ephemeral"}));
        let mut body = json!({"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]});
        let before = body.clone();
        prepare(
            &req,
            crate::Provider::Anthropic,
            Protocol::Messages,
            &mut body,
            &mut client,
            &mut HeaderMap::new(),
        );
        assert_eq!(body, before);
    }
}

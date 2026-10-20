//! Passive, bounded router-side speed samples; no prompt/response text is retained.
use super::route_rule::Priority;
use crate::router::Protocol;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(crate) struct Candidate {
    pub pool: String,
    model: String,
    protocol: &'static str,
    multiplier: u64,
    pub estimated_cost: Option<u128>,
    pub sticky: bool,
    pub binding: Option<String>,
}
impl Candidate {
    pub fn new(pool: String, model: &str, protocol: Protocol, multiplier: u64) -> Self {
        Self {
            pool,
            model: model.into(),
            protocol: match protocol {
                Protocol::Chat => "chat",
                Protocol::Responses => "responses",
                Protocol::Messages => "messages",
            },
            multiplier,
            estimated_cost: None,
            sticky: false,
            binding: None,
        }
    }
    fn key(&self) -> String {
        format!("{}\n{}\n{}", self.pool, self.model, self.protocol)
    }
}
#[derive(Default)]
struct Metric {
    first: Option<f64>,
    tokens_per_sec: Option<f64>,
    sampled: Option<Instant>,
    cooldown: Option<Instant>,
    in_flight: usize,
}
#[derive(Default)]
pub(crate) struct State {
    history: Mutex<HashMap<String, Metric>>,
    sessions: Mutex<HashMap<String, (String, Instant)>>,
}
impl State {
    pub(crate) fn prefer_session(&self, rows: &mut [Candidate], session: &str) {
        let sessions = self.sessions.lock().unwrap();
        if let Some((pool, _)) = sessions
            .get(session)
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(86400))
        {
            for row in rows {
                row.sticky = row.sticky && row.pool == *pool;
            }
        }
    }
    pub(crate) fn remember_session(&self, session: String, pool: String) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() >= 4096
            && !sessions.contains_key(&session)
            && let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone())
        {
            sessions.remove(&oldest);
        }
        sessions.insert(session, (pool, Instant::now()));
    }
    #[cfg(test)]
    pub(super) fn sample(&self, pool: &str, model: &str) -> Option<(f64, Option<f64>)> {
        let history = self.history.lock().unwrap();
        let m = history.get(&Candidate::new(pool.into(), model, Protocol::Chat, 0).key())?;
        Some((m.first?, m.tokens_per_sec))
    }
    pub fn select(&self, rows: &[Candidate], priority: Priority, automatic: bool) -> Option<usize> {
        let history = self.history.lock().unwrap();
        let now = Instant::now();
        let metric = |c: &Candidate| history.get(&c.key());
        let fresh = |c: &Candidate| {
            metric(c)
                .filter(|m| {
                    m.sampled
                        .is_some_and(|t| now.duration_since(t) < Duration::from_secs(600))
                })
                .and_then(|m| m.first)
        };
        let rank = |c: &Candidate| {
            if fresh(c).is_some() {
                1
            } else if metric(c).is_some_and(|m| m.in_flight > 0) {
                2
            } else {
                0
            }
        };
        rows.iter()
            .enumerate()
            .filter(|(_, c)| {
                !automatic || !metric(c).is_some_and(|m| m.cooldown.is_some_and(|t| t > now))
            })
            .min_by(|(_, a), (_, b)| {
                let price = a
                    .estimated_cost
                    .unwrap_or(u128::from(a.multiplier))
                    .cmp(&b.estimated_cost.unwrap_or(u128::from(b.multiplier)))
                    .then(b.sticky.cmp(&a.sticky))
                    .then(a.multiplier.cmp(&b.multiplier))
                    .then(a.pool.cmp(&b.pool));
                if priority == Priority::Price {
                    return price;
                }
                b.sticky.cmp(&a.sticky).then_with(|| {
                    rank(a)
                        .cmp(&rank(b))
                        .then_with(|| match (fresh(a), fresh(b)) {
                            (Some(a_first), Some(b_first)) => (a_first * 10.0)
                                .floor()
                                .total_cmp(&(b_first * 10.0).floor())
                                .then_with(|| {
                                    metric(b)
                                        .and_then(|m| m.tokens_per_sec)
                                        .unwrap_or(0.0)
                                        .total_cmp(
                                            &metric(a)
                                                .and_then(|m| m.tokens_per_sec)
                                                .unwrap_or(0.0),
                                        )
                                }),
                            _ => metric(a)
                                .and_then(|m| m.sampled)
                                .cmp(&metric(b).and_then(|m| m.sampled)),
                        })
                        .then(price)
                })
            })
            .map(|(i, _)| i)
    }
    pub fn observe(self: &Arc<Self>, candidate: Candidate) -> Observation {
        let mut history = self.history.lock().unwrap();
        // Evict oldest idle entries; active observations have bounded ingress permits.
        if history.len() >= 30000
            && !history.contains_key(&candidate.key())
            && let Some(key) = history
                .iter()
                .filter(|(_, m)| m.in_flight == 0)
                .min_by_key(|(_, m)| m.sampled)
                .map(|(k, _)| k.clone())
        {
            history.remove(&key);
        }
        history.entry(candidate.key()).or_default().in_flight += 1;
        Observation {
            binding: candidate.binding.clone(),
            state: self.clone(),
            key: candidate.key(),
            started: Instant::now(),
            first: None,
            last: None,
            tokens: None,
            terminal: false,
            error: false,
            finished: false,
        }
    }
}
pub(crate) struct Observation {
    binding: Option<String>,
    state: Arc<State>,
    key: String,
    started: Instant,
    first: Option<f64>,
    last: Option<f64>,
    tokens: Option<u64>,
    terminal: bool,
    error: bool,
    finished: bool,
}
impl Observation {
    pub(crate) fn preferred_account(&self) -> Option<&str> {
        self.binding.as_deref()
    }
    pub fn value(&mut self, value: &Value) {
        if value.get("error").is_some_and(|v| !v.is_null())
            || matches!(
                value["type"].as_str(),
                Some("error" | "response.failed" | "response.incomplete")
            )
        {
            self.error = true;
        }
        let nonempty = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
        let content = nonempty(&value["delta"])
            || ["text", "thinking", "partial_json"]
                .iter()
                .any(|k| nonempty(&value["delta"][k]))
            || value["choices"].as_array().is_some_and(|cs| {
                cs.iter().any(|c| {
                    ["content", "reasoning_content", "refusal"]
                        .iter()
                        .any(|k| nonempty(&c["delta"][k]))
                        || c["delta"]["tool_calls"].as_array().is_some_and(|ts| {
                            ts.iter().any(|t| nonempty(&t["function"]["arguments"]))
                        })
                })
            });
        if content {
            let at = self.started.elapsed().as_secs_f64();
            self.first.get_or_insert(at);
            self.last = Some(at);
        }
        for pointer in [
            "/usage/completion_tokens",
            "/usage/output_tokens",
            "/response/usage/output_tokens",
        ] {
            if let Some(tokens) = value.pointer(pointer).and_then(Value::as_u64) {
                self.tokens = Some(tokens);
            }
        }
        if matches!(
            value["type"].as_str(),
            Some("message_stop" | "response.completed")
        ) || value["choices"]
            .as_array()
            .is_some_and(|cs| cs.iter().any(|c| c["finish_reason"].as_str().is_some()))
        {
            self.terminal = true;
        }
    }
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut history = self.state.history.lock().unwrap();
        let metric = history.entry(self.key.clone()).or_default();
        if self.error || !self.terminal {
            metric.cooldown = Some(Instant::now() + Duration::from_secs(30));
            return;
        }
        if let Some(first) = self.first {
            metric.first = Some(metric.first.map_or(first, |old| old * 0.7 + first * 0.3));
            if let Some(speed) = self
                .last
                .filter(|t| *t >= first + 0.25)
                .zip(self.tokens.filter(|n| *n > 1))
                .map(|(last, n)| (n - 1) as f64 / (last - first))
            {
                metric.tokens_per_sec = Some(
                    metric
                        .tokens_per_sec
                        .map_or(speed, |old| old * 0.7 + speed * 0.3),
                );
            }
        }
        metric.sampled = Some(Instant::now());
        metric.cooldown = None;
    }
    pub fn nonstream(&mut self) {
        self.terminal = true;
        self.finish();
    }
    pub fn fail(&mut self) {
        self.error = true;
        self.finish();
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        if let Some(metric) = self.state.history.lock().unwrap().get_mut(&self.key) {
            metric.in_flight = metric.in_flight.saturating_sub(1);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn router_speed_samples_ignore_headers_use_content_and_compare_real_rates() {
        let state = Arc::new(State::default());
        let rows = vec![
            Candidate::new("m_a.pool".into(), "model", Protocol::Chat, 200000),
            Candidate::new("m_b.pool".into(), "model", Protocol::Chat, 500000),
        ];
        let a = state.select(&rows, Priority::Speed, true).unwrap();
        let mut first = state.observe(rows[a].clone());
        let b = state.select(&rows, Priority::Speed, true).unwrap();
        assert_ne!(a, b);
        let mut second = state.observe(rows[b].clone());
        for (observation, latency, tokens) in [(&mut first, 2, 10), (&mut second, 1, 20)] {
            observation.value(&json!({"choices":[{"delta":{"role":"assistant"}}]}));
            assert!(observation.first.is_none());
            observation.started -= Duration::from_secs(latency);
            observation.value(&json!({"choices":[{"delta":{"content":"a"}}]}));
            observation.started -= Duration::from_secs(1);
            observation.value(&json!({"choices":[{"delta":{"content":"b"},"finish_reason":"stop"}],"usage":{"completion_tokens":tokens}}));
            observation.finish();
        }
        drop(first);
        drop(second);
        assert_eq!(state.select(&rows, Priority::Speed, true), Some(b));
        assert_eq!(state.select(&rows, Priority::Price, true), Some(a));
        let history = state.history.lock().unwrap();
        assert!(history[&rows[b].key()].tokens_per_sec.unwrap() > 18.0);
    }
    #[test]
    fn cancellation_failures_and_buffered_bursts_never_create_fast_successes() {
        let state = Arc::new(State::default());
        let c = Candidate::new("m_a.pool".into(), "model", Protocol::Chat, 1000000);
        {
            let mut canceled = state.observe(c.clone());
            canceled.value(&json!({"choices":[{"delta":{"content":"a"}}]}));
        }
        assert!(state.history.lock().unwrap()[&c.key()].first.is_none());
        let mut burst = state.observe(c.clone());
        burst.value(&json!({"choices":[{"delta":{"content":"a"},"finish_reason":"stop"}],"usage":{"completion_tokens":100}}));
        burst.finish();
        drop(burst);
        assert!(
            state.history.lock().unwrap()[&c.key()]
                .tokens_per_sec
                .is_none()
        );
        let mut failed = state.observe(c.clone());
        failed.value(&json!({"error":{"type":"upstream_error"}}));
        failed.finish();
        drop(failed);
        assert!(
            state
                .select(std::slice::from_ref(&c), Priority::Speed, true)
                .is_none()
        );
        assert_eq!(
            state.select(std::slice::from_ref(&c), Priority::Speed, false),
            Some(0)
        );
        assert_eq!(state.history.lock().unwrap()[&c.key()].in_flight, 0);
    }
}

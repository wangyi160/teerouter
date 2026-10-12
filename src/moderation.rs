//! Full-request input moderation. No conversation cache, text rewriting or output scanning.
mod input;
mod report;

pub(crate) use report::{Decision, Event, Reason, Report, UserRecord};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

const ENDPOINT: &str = "https://api.lockllm.com/v1/scan";

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub api_key_ref: String,
    /// Explicit optional proxy for moderation only. None means direct HTTPS.
    pub proxy: Option<String>,
    pub sensitivity: Sensitivity,
    pub timeout_secs: u64,
    pub max_input_bytes: usize,
    pub max_concurrent: usize,
    pub rpm: u64,
    #[cfg(test)]
    pub(crate) test_endpoint: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            api_key_ref: String::new(),
            proxy: None,
            sensitivity: Sensitivity::Medium,
            timeout_secs: 30,
            max_input_bytes: 16 * 1024 * 1024,
            max_concurrent: 16,
            rpm: 300,
            #[cfg(test)]
            test_endpoint: None,
        }
    }
}
#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Low,
    #[default]
    Medium,
    High,
}
impl Sensitivity {
    fn header(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}
impl Config {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !crate::config::valid_name(&self.api_key_ref)
            || !(1..=300).contains(&self.timeout_secs)
            || !(1024..=256 * 1024 * 1024).contains(&self.max_input_bytes)
            || !(1..=256).contains(&self.max_concurrent)
            || !(1..=200_000).contains(&self.rpm)
        {
            return Err("invalid moderation configuration".into());
        }
        if let Some(proxy) = &self.proxy {
            crate::network::proxy(proxy).map_err(|_| "invalid moderation proxy")?;
        }
        Ok(())
    }
}

/// Kept across all config generations, including disable/re-enable and key rotation.
#[derive(Default)]
pub(crate) struct Shared {
    gate: Arc<crate::resources::Gate>,
    minute: Mutex<Option<(Instant, u64)>>,
}
pub(crate) struct Scanner {
    config: Config,
    client: reqwest::Client,
    shared: Arc<Shared>,
}
impl Scanner {
    pub(crate) fn new(config: &Config, secret: &str, shared: Arc<Shared>) -> Result<Self, String> {
        config.validate()?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {secret}"))
            .map_err(|_| "invalid moderation credential")?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("authorization", authorization);
        headers.insert("x-lockllm-scan-mode", HeaderValue::from_static("combined"));
        headers.insert("x-lockllm-scan-action", HeaderValue::from_static("block"));
        headers.insert("x-lockllm-policy-action", HeaderValue::from_static("block"));
        headers.insert(
            "x-lockllm-on-unavailable",
            HeaderValue::from_static("block"),
        );
        headers.insert(
            "x-lockllm-sensitivity",
            HeaderValue::from_static(config.sensitivity.header()),
        );
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(config.timeout_secs));
        if let Some(proxy) = &config.proxy {
            builder = builder
                .proxy(crate::network::proxy(proxy).map_err(|_| "invalid moderation proxy")?);
        }
        let client = builder
            .build()
            .map_err(|_| "cannot create moderation client")?;
        Ok(Self {
            config: config.clone(),
            client,
            shared,
        })
    }
    pub(crate) async fn check(
        &self,
        protocol: crate::upstream::Protocol,
        body: &Value,
        deadline: Instant,
        revision: i64,
    ) -> Report {
        let started = Instant::now();
        let mut report = Report::unavailable(Reason::TransportError, revision);
        let text = match input::extract(protocol, body, self.config.max_input_bytes) {
            Ok(text) => text,
            Err(reason) => {
                report.reason = reason;
                report.decision = Decision::Rejected;
                return report;
            }
        };
        report.input_chars = text.chars().count() as u64;
        let Some(_permit) = self.shared.gate.acquire(self.config.max_concurrent) else {
            report.reason = Reason::ConcurrencyLimit;
            return report;
        };
        {
            let mut minute = self.shared.minute.lock().unwrap();
            let (since, count) = minute.get_or_insert((Instant::now(), 0));
            if since.elapsed() >= Duration::from_secs(60) {
                *since = Instant::now();
                *count = 0;
            }
            if *count >= self.config.rpm {
                report.reason = Reason::RateLimit;
                return report;
            }
            *count += 1;
        }
        let endpoint = ENDPOINT;
        #[cfg(test)]
        let endpoint = self.config.test_endpoint.as_deref().unwrap_or(endpoint);
        let result = timeout_at(
            deadline.min(started + Duration::from_secs(self.config.timeout_secs)),
            async {
                let response = self
                    .client
                    .post(endpoint)
                    .json(&json!({"input":text}))
                    .send()
                    .await
                    .map_err(|e| {
                        if e.is_timeout() {
                            Reason::Timeout
                        } else {
                            Reason::TransportError
                        }
                    })?;
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let body = crate::resources::json(response, 256 * 1024)
                    .await
                    .map_err(|_| Reason::InvalidResponse)?;
                Ok::<_, Reason>((status, headers, body))
            },
        )
        .await;
        match result {
            Ok(Ok((status, headers, body))) => report.classify(status, &headers, &body),
            Ok(Err(reason)) => report.reason = reason,
            Err(_) => report.reason = Reason::Timeout,
        }
        report.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        report
    }
}

#[cfg(test)]
mod tests;

use std::{sync::Arc, time::Duration};

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;

use crate::{CompletionStream, Error, Provider, anthropic, stream};

#[derive(Clone)]
pub struct Client {
    pub(crate) limits: crate::resources::Limits,
    pub(crate) profile: crate::compatibility::Profile,
    pub(crate) provider: Provider,
    pub(crate) http: reqwest::Client,
    pub(crate) refusal: Option<crate::refusal::Observer>,
    pub(crate) billing: Option<crate::billing::Observer>,
    pub(crate) activity: Option<crate::ledger::Activity>,
    headers: HeaderMap,
    pub(crate) base_url: reqwest::Url,
    pub(crate) oauth: Option<Arc<crate::oauth::TokenStore>>,
}

impl Client {
    pub(crate) fn cache_session(&mut self, name: &'static str, id: &str) {
        self.headers.insert(
            name,
            HeaderValue::from_str(id).expect("hashed cache identity"),
        );
    }
    pub fn new(provider: Provider, api_key: impl AsRef<str>) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Self::with_http_client(provider, api_key, http)
    }

    pub fn from_env(provider: Provider) -> Result<Self, Error> {
        let key = std::env::var(provider.api_key_env())
            .map_err(|_| Error::MissingApiKey(provider.api_key_env()))?;
        Self::new(provider, key)
    }

    pub fn with_http_client(
        provider: Provider,
        api_key: impl AsRef<str>,
        http: reqwest::Client,
    ) -> Result<Self, Error> {
        if api_key.as_ref().trim().is_empty() {
            return Err(Error::MissingApiKey(provider.api_key_env()));
        }
        let credential = match provider {
            Provider::Anthropic => api_key.as_ref().to_owned(),
            _ => format!("Bearer {}", api_key.as_ref()),
        };
        let mut value = HeaderValue::from_str(&credential)
            .map_err(|_| Error::InvalidRequest("invalid API key header"))?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        if provider == Provider::Anthropic {
            headers.insert("x-api-key", value);
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            crate::oauth::apply_claude_oauth(&mut headers, api_key.as_ref())?;
        } else {
            headers.insert("authorization", value);
        }
        Ok(Self {
            limits: Default::default(),
            profile: Default::default(),
            provider,
            http,
            headers,
            base_url: parse_base_url(provider.base_url())?,
            oauth: None,
            refusal: None,
            billing: None,
            activity: None,
        })
    }

    pub(crate) fn with_oauth(
        account: Arc<crate::oauth::TokenStore>,
        http: reqwest::Client,
    ) -> Result<Self, Error> {
        let provider = account.provider;
        let mut headers = HeaderMap::new();
        if provider == Provider::Anthropic {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        let base = match provider {
            Provider::OpenAI => crate::oauth::CHATGPT_API_BASE,
            _ => provider.base_url(),
        };
        Ok(Self {
            limits: Default::default(),
            profile: Default::default(),
            provider,
            http,
            headers,
            base_url: parse_base_url(base)?,
            oauth: Some(account),
            refusal: None,
            billing: None,
            activity: None,
        })
    }

    pub(crate) async fn auth_headers(&self, extra: HeaderMap) -> Result<HeaderMap, Error> {
        let mut headers = self.headers.clone();
        for (key, value) in &self.profile.headers {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|_| Error::InvalidRequest("invalid profile header"))?,
                HeaderValue::from_str(value)
                    .map_err(|_| Error::InvalidRequest("invalid profile header"))?,
            );
        }
        headers.extend(extra);
        for name in ["session_id", "x-grok-conv-id"] {
            if let Some(value) = self.headers.get(name) {
                headers.insert(name, value.clone());
            }
        }
        if let Some(account) = &self.oauth {
            let credential = account.credential().await?;
            let mut value = HeaderValue::from_str(&format!("Bearer {}", credential.access_token))
                .map_err(|_| Error::OAuth("invalid access token header"))?;
            value.set_sensitive(true);
            headers.remove("x-api-key");
            headers.insert("authorization", value);
            if self.provider == Provider::Anthropic
                && !crate::oauth::apply_claude_oauth(&mut headers, &credential.access_token)?
            {
                return Err(Error::OAuth(
                    "account file does not contain a recognized Claude OAuth token",
                ));
            }
            if self.provider == Provider::OpenAI {
                headers
                    .entry("originator")
                    .or_insert(HeaderValue::from_static("codex_cli_rs"));
                headers
                    .entry("user-agent")
                    .or_insert(HeaderValue::from_static(
                        "codex_cli_rs/0.0.0 (Unknown 0; unknown) unknown",
                    ));
                headers.insert("accept", HeaderValue::from_static("text/event-stream"));
                if !headers.contains_key("session_id") {
                    headers.insert(
                        "session_id",
                        HeaderValue::from_str(&crate::oauth::random_string()?)
                            .expect("base64 session ID"),
                    );
                }
                if let Some(id) = credential.account_id {
                    headers.insert(
                        "chatgpt-account-id",
                        HeaderValue::from_str(&id)
                            .map_err(|_| Error::OAuth("invalid ChatGPT account ID"))?,
                    );
                }
            }
        } else if self.provider == Provider::Anthropic {
            // Extra beta headers must not overwrite the OAuth beta supplied by upstream Rust logic.
            if let Some(value) = self
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
            {
                crate::oauth::apply_claude_oauth(&mut headers, value)?;
            }
        }
        Ok(headers)
    }

    pub fn with_base_url(self, base_url: &str) -> Result<Self, Error> {
        crate::official::validate(base_url, Some(self.provider))?;
        Ok(Self {
            base_url: parse_base_url(base_url)?,
            ..self
        })
    }

    pub async fn completion(&self, request: Value) -> Result<Value, Error> {
        let response = self.send(request, false).await?;
        let body = crate::resources::json(response, self.limits.bytes).await?;
        if let Some(observer) = &self.billing {
            observer.observe(self.chat_protocol(), &body, false);
        }
        if let Some(observer) = &self.refusal {
            observer.observe(self.chat_protocol(), &body, false);
        }
        if self.provider == Provider::Anthropic {
            anthropic::response(body)
        } else {
            if !body.get("choices").is_some_and(Value::is_array) {
                return Err(Error::InvalidResponse("missing choices array"));
            }
            Ok(body)
        }
    }

    pub async fn stream(&self, request: Value) -> Result<CompletionStream, Error> {
        let include_usage = request["stream_options"]["include_usage"] == true;
        let response = self.send(request, true).await?;
        if !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
        {
            return Err(Error::InvalidResponse("expected text/event-stream"));
        }
        Ok(stream::decode(
            response,
            self.provider,
            include_usage,
            self.limits,
            self.refusal.clone(),
            self.billing.clone(),
        ))
    }

    async fn send(&self, request: Value, streaming: bool) -> Result<reqwest::Response, Error> {
        validate(&request)?;
        let body = if self.provider == Provider::Anthropic {
            anthropic::request(request, streaming)?
        } else {
            let mut body = request;
            if self.provider == Provider::XAI {
                let object = body
                    .as_object_mut()
                    .ok_or(Error::InvalidRequest("request must be an object"))?;
                if let Some(limit) = object.remove("max_completion_tokens") {
                    object.insert("max_tokens".into(), limit);
                }
            }
            body["stream"] = streaming.into();
            if self.provider == Provider::OpenAI || self.provider.is_local_test() {
                body["store"] = false.into();
            }
            body
        };
        let (operation, endpoint) = if self.provider == Provider::Anthropic {
            ("messages", self.profile.path("messages", "messages"))
        } else {
            (
                "chat_completions",
                self.profile.path("chat_completions", "chat/completions"),
            )
        };
        let mut url = self.base_url.clone();
        url.set_path(&format!("{}/{endpoint}", url.path().trim_end_matches('/')));
        let response = self
            .profile
            .json(
                operation,
                &self.http,
                url,
                self.auth_headers(HeaderMap::new()).await?,
                body,
            )?
            .send()
            .await?;
        self.checked(response, self.chat_protocol()).await
    }

    pub(crate) async fn native(
        &self,
        protocol: crate::upstream::Protocol,
        model: &str,
        mut body: Value,
        extra_headers: HeaderMap,
    ) -> Result<reqwest::Response, Error> {
        use crate::upstream::Protocol;
        if self.oauth.is_some() && self.provider == Provider::OpenAI {
            return crate::oauth::chatgpt::send(self, model, body).await;
        }
        let object = body
            .as_object_mut()
            .ok_or(Error::InvalidRequest("request must be an object"))?;
        let mut url = self.base_url.clone();
        let headers = self.auth_headers(extra_headers).await?;
        let operation = if protocol == Protocol::Messages {
            "messages"
        } else {
            "responses"
        };
        object.insert("model".into(), model.into());
        let endpoint = if protocol == Protocol::Messages {
            self.profile.path("messages", "messages")
        } else {
            self.profile.path("responses", "responses")
        };
        url.set_path(&format!("{}/{endpoint}", url.path().trim_end_matches('/')));
        let response = self
            .profile
            .json(operation, &self.http, url, headers, body)?
            .send()
            .await?;
        self.checked(response, protocol).await
    }
    fn chat_protocol(&self) -> crate::upstream::Protocol {
        if self.provider == Provider::Anthropic {
            crate::upstream::Protocol::Messages
        } else {
            crate::upstream::Protocol::Chat
        }
    }
    pub(crate) async fn checked(
        &self,
        response: reqwest::Response,
        protocol: crate::upstream::Protocol,
    ) -> Result<reqwest::Response, Error> {
        // Response headers prove that the model request reached the upstream.
        // Catalog discovery, local validation and connection failures never call this hook.
        if let Some(activity) = &self.activity {
            activity.observed();
        }
        if let Some(observer) = &self.refusal {
            observer.headers(&response, protocol);
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok());
            let body = crate::resources::error_text(response).await;
            if let Some(observer) = &self.refusal
                && let Ok(value) = serde_json::from_str(&body)
            {
                observer.observe(protocol, &value, false);
            }
            return Err(Error::Api {
                status,
                body,
                retry_after_secs,
            });
        }
        Ok(response)
    }
}

pub(crate) fn parse_base_url(base_url: &str) -> Result<reqwest::Url, Error> {
    let url =
        reqwest::Url::parse(base_url).map_err(|_| Error::InvalidRequest("invalid base URL"))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::InvalidRequest(
            "base URL must be HTTP(S), without credentials, query or fragment",
        ));
    }
    Ok(url)
}

fn validate(request: &Value) -> Result<(), Error> {
    if !request
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
    {
        return Err(Error::InvalidRequest("model must be a non-empty string"));
    }
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(Error::InvalidRequest("messages must be an array"))?;
    if messages.is_empty() {
        return Err(Error::InvalidRequest("messages must not be empty"));
    }
    for message in messages {
        if !matches!(
            message["role"].as_str(),
            Some("system" | "developer" | "user" | "assistant" | "tool")
        ) {
            return Err(Error::InvalidRequest("unsupported message role"));
        }
    }
    Ok(())
}

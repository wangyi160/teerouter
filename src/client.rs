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
    pub(crate) transport: Option<crate::network::metered::Transport>,
    headers: HeaderMap,
    pub(crate) base_url: reqwest::Url,
    pub(crate) oauth: Option<Arc<crate::oauth::TokenStore>>,
    pub(crate) latency: Option<Arc<crate::network::latency::Target>>,
}

impl Client {
    /// Keep the validated caller's identity through chat/OAuth adapters too.
    /// This runs on a per-request clone, never on catalog/probe clients.
    pub(crate) fn preserve_agent_headers(&mut self, headers: &HeaderMap) {
        for &name in crate::agent::HEADERS {
            self.headers.remove(name);
            self.profile.headers.remove(name);
            for profile in self.profile.requests.values_mut() {
                profile.headers.remove(name);
                profile.remove_headers.retain(|h| h != name);
            }
            if let Some(value) = headers.get(name) {
                self.headers.insert(name, value.clone());
            }
        }
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
            latency: None,
            refusal: None,
            billing: None,
            activity: None,
            transport: None,
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
            latency: None,
            refusal: None,
            billing: None,
            activity: None,
            transport: None,
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
            if self.provider == Provider::OpenAI
                && let Some(id) = credential.account_id
            {
                headers.insert(
                    "chatgpt-account-id",
                    HeaderValue::from_str(&id)
                        .map_err(|_| Error::OAuth("invalid ChatGPT account ID"))?,
                );
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
            if self.provider == Provider::OpenAI {
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
        let request = self.profile.json(
            operation,
            &self.http,
            url,
            self.auth_headers(HeaderMap::new()).await?,
            body,
        )?;
        let response = self.dispatch(request).await?;
        self.checked(response, self.chat_protocol()).await
    }

    pub(crate) fn supports_native(&self, protocol: crate::upstream::Protocol) -> bool {
        protocol.supports(self.provider)
            && !(self.provider == Provider::OpenAI
                && self.oauth.is_some()
                && protocol != crate::upstream::Protocol::Responses)
    }

    pub(crate) async fn native(
        &self,
        protocol: crate::upstream::Protocol,
        model: &str,
        mut body: Value,
        mut extra_headers: HeaderMap,
    ) -> Result<reqwest::Response, Error> {
        use crate::upstream::Protocol;
        if self.oauth.is_some() && self.provider == Provider::OpenAI {
            if protocol != Protocol::Responses {
                return Err(Error::InvalidRequest(
                    "Codex accounts require the native Responses protocol",
                ));
            }
            return crate::oauth::chatgpt::send(self, model, body, extra_headers).await;
        }
        if !protocol.supports(self.provider) {
            return Err(Error::InvalidRequest(
                "provider does not support this native protocol",
            ));
        }
        let object = body
            .as_object_mut()
            .ok_or(Error::InvalidRequest("request must be an object"))?;
        // Resolving the public model namespace is the only body transformation.
        object.insert("model".into(), model.into());
        if protocol == Protocol::Chat {
            validate(&body)?;
        }
        let mut url = self.base_url.clone();
        let (operation, default_path) = match protocol {
            Protocol::Chat => ("chat_completions", "chat/completions"),
            Protocol::Messages => ("messages", "messages"),
            Protocol::Responses => ("responses", "responses"),
        };
        let endpoint = self.profile.path(operation, default_path);
        url.set_path(&format!("{}/{endpoint}", url.path().trim_end_matches('/')));
        take_native_query(&mut extra_headers, &mut url)?;
        let mut native_client = self.clone();
        native_client.profile.headers.clear();
        let headers = native_client.auth_headers(extra_headers).await?;
        let request = self
            .profile
            .json(operation, &self.http, url, headers, body)?;
        let response = self.dispatch(request).await?;
        self.checked(response, protocol).await
    }
    pub(crate) async fn dispatch(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Error> {
        if let (Some(transport), Some(observer)) = (&self.transport, &self.billing) {
            transport.send(request.build()?, observer.clone()).await
        } else {
            Ok(request.send().await?)
        }
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
        if let Some(observer) = &self.billing {
            observer.headers(response.status().as_u16());
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok());
            let (body, value) = crate::resources::error_metering(response, self.limits.bytes).await;
            if let Some(observer) = &self.billing
                && let Some(value) = &value
            {
                observer.observe(protocol, value, false);
            }
            if let Some(observer) = &self.refusal
                && let Some(value) = &value
            {
                observer.observe(protocol, value, false);
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

// Set by the HTTP ingress only; never accepted as a caller-supplied header.
pub(crate) const NATIVE_QUERY: &str = "x-router-internal-query";
pub(crate) fn take_native_query(
    headers: &mut HeaderMap,
    url: &mut reqwest::Url,
) -> Result<(), Error> {
    if let Some(query) = headers.remove(NATIVE_QUERY) {
        url.set_query(Some(
            query
                .to_str()
                .map_err(|_| Error::InvalidRequest("invalid native query"))?,
        ));
    }
    Ok(())
}

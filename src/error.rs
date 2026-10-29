#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("OAuth authentication failed: {0}")]
    OAuth(&'static str),
    #[error("missing API key: {0}")]
    MissingApiKey(&'static str),
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("invalid provider response: {0}")]
    InvalidResponse(&'static str),
    #[error("provider returned HTTP {status}: {body}")]
    Api {
        status: u16,
        body: String,
        retry_after_secs: Option<u64>,
    },
    #[error("provider stream error: {0}")]
    Stream(String),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("body stream failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SSE field is not UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: &'static str,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            retry_after: None,
        }
    }
    pub fn limited(message: &'static str) -> Self {
        Self {
            retry_after: Some(60),
            ..Self::new(429, "rate_limit_exceeded", message)
        }
    }
    pub fn upstream(error: &Error) -> Self {
        let (status, code, message) = match error {
            Error::OAuth(_) => (
                502,
                "upstream_authentication_error",
                "Provider OAuth credential is unavailable; check account login",
            ),
            Error::InvalidRequest(_) => (
                400,
                "invalid_request",
                "Request is invalid or unsupported by this provider",
            ),
            Error::Api {
                status: 400 | 422, ..
            } => (
                400,
                "upstream_rejected_request",
                "Provider rejected the request; check the model's supported parameters",
            ),
            Error::Api { status: 413, .. } => (
                413,
                "upstream_body_too_large",
                "Provider rejected the request body as too large",
            ),
            Error::Api { status: 429, .. } => {
                (429, "upstream_rate_limit", "Provider rate limit exceeded")
            }
            Error::Api {
                status: 401 | 403, ..
            } => (
                502,
                "upstream_authentication_error",
                "Provider denied this account or permission",
            ),
            Error::Api { status: 402, .. } => (
                502,
                "upstream_quota_exhausted",
                "Provider account requires quota or payment",
            ),
            Error::Api { status: 404, .. } => (
                502,
                "upstream_model_unavailable",
                "Provider model or endpoint is unavailable",
            ),
            Error::Http(e) if e.is_connect() => (
                502,
                "upstream_connect_error",
                "Cannot connect through the account proxy to the provider",
            ),
            Error::Io(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                (504, "upstream_idle_timeout", "Provider stream stalled")
            }
            Error::Http(e) if e.is_timeout() => {
                (504, "upstream_timeout", "Provider request timed out")
            }
            _ => (502, "upstream_error", "Provider request failed"),
        };
        let retry_after = match error {
            Error::Api {
                retry_after_secs, ..
            } => retry_after_secs.map(|n| n.min(3600)),
            _ => None,
        };
        Self {
            status,
            code,
            message,
            retry_after,
        }
    }
}

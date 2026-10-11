#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenAI,
    #[serde(alias = "claude")]
    Anthropic,
    Gemini,
    #[serde(alias = "grok")]
    XAI,
    #[cfg(feature = "workbuddy-lab")]
    Workbuddy,
}

impl Provider {
    pub fn api_key_env(self) -> &'static str {
        match self {
            Self::OpenAI => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
            Self::XAI => "XAI_API_KEY",
            #[cfg(feature = "workbuddy-lab")]
            Self::Workbuddy => "WORKBUDDY_API_KEY",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::OpenAI => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai",
            Self::XAI => "https://api.x.ai/v1",
            #[cfg(feature = "workbuddy-lab")]
            Self::Workbuddy => "http://127.0.0.1:8001/v1",
        }
    }

    pub fn is_local_test(self) -> bool {
        match self {
            #[cfg(feature = "workbuddy-lab")]
            Self::Workbuddy => true,
            _ => false,
        }
    }
}

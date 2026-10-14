#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenAI,
    #[serde(alias = "claude")]
    Anthropic,
    #[serde(alias = "grok")]
    XAI,
    #[cfg(feature = "workbuddy-lab")]
    Workbuddy,
}

impl Provider {
    /// Public model identity, independent of the merchant supplying an account.
    pub fn model_namespace(self) -> &'static str {
        match self {
            Self::OpenAI => "chatgpt",
            Self::Anthropic => "claude",
            Self::XAI => "grok",
            #[cfg(feature = "workbuddy-lab")]
            Self::Workbuddy => "workbuddy",
        }
    }

    pub fn split_model(id: &str) -> (Option<Self>, &str) {
        let Some((namespace, model)) = id.split_once('/') else {
            return (None, id);
        };
        let provider = match namespace {
            "chatgpt" => Self::OpenAI,
            "claude" => Self::Anthropic,
            "grok" => Self::XAI,
            #[cfg(feature = "workbuddy-lab")]
            "workbuddy" => Self::Workbuddy,
            _ => return (None, id),
        };
        (Some(provider), model)
    }

    pub fn api_key_env(self) -> &'static str {
        match self {
            Self::OpenAI => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::XAI => "XAI_API_KEY",
            #[cfg(feature = "workbuddy-lab")]
            Self::Workbuddy => "WORKBUDDY_API_KEY",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::OpenAI => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::XAI => "https://api.x.ai/v1",
            #[cfg(feature = "workbuddy-lab")]
            Self::Workbuddy => match option_env!("TEEROUTER_WORKBUDDY_BASE_URL") {
                Some(url) => url,
                None => "http://127.0.0.1:8001/v1",
            },
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

#[cfg(test)]
mod model_namespace_tests {
    use super::*;
    #[test]
    fn namespaces_preserve_official_model_ids_and_unqualified_compatibility() {
        for (id, provider, raw) in [
            ("chatgpt/gpt-test", Some(Provider::OpenAI), "gpt-test"),
            (
                "claude/claude-test",
                Some(Provider::Anthropic),
                "claude-test",
            ),
            ("grok/grok-4.7", Some(Provider::XAI), "grok-4.7"),
            (
                "chatgpt/vendor/model",
                Some(Provider::OpenAI),
                "vendor/model",
            ),
            ("vendor/model", None, "vendor/model"),
            ("grok-4.7", None, "grok-4.7"),
        ] {
            assert_eq!(Provider::split_model(id), (provider, raw));
            if let Some(p) = provider {
                assert_eq!(format!("{}/{raw}", p.model_namespace()), id);
            }
        }
    }
}

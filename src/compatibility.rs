//! Deliberately bounded vendor metadata; never executable configuration.
use crate::Provider;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod wire;
pub use crate::discovery::capabilities::CapabilityPaths;
pub use wire::RequestProfile;

pub(crate) fn storage_field(key: &str) -> bool {
    key.chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .collect::<String>()
        .eq_ignore_ascii_case("store")
}

// User content and generation controls are never administrator protocol metadata.
// Normalize vendor snake_case/camelCase spellings, as with the other invariants.
pub(crate) fn content_field(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "message"
            | "messages"
            | "input"
            | "inputs"
            | "prompt"
            | "prompts"
            | "instruction"
            | "instructions"
            | "system"
            | "developer"
            | "systeminstruction"
            | "systeminstructions"
            | "content"
            | "contents"
            | "parts"
            | "text"
            | "inputtext"
            | "inputimage"
            | "inputaudio"
            | "image"
            | "imageurl"
            | "audio"
            | "video"
            | "document"
            | "documents"
            | "attachment"
            | "attachments"
            | "file"
            | "files"
            | "inlinedata"
            | "cachedcontent"
            | "tool"
            | "tools"
            | "toolchoice"
            | "toolconfig"
            | "toolcalls"
            | "tooluse"
            | "toolresult"
            | "toolresults"
            | "function"
            | "functions"
            | "functioncall"
            | "functioncalls"
            | "functionresponse"
            | "functiondeclarations"
            | "paralleltoolcalls"
            | "providercontent"
            | "responseformat"
            | "responseschema"
            | "responsejsonschema"
            | "temperature"
            | "topp"
            | "topk"
            | "maxtokens"
            | "maxcompletiontokens"
            | "maxoutputtokens"
            | "stop"
            | "stopsequences"
            | "seed"
            | "presencepenalty"
            | "frequencypenalty"
            | "logitbias"
    )
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub headers: BTreeMap<String, String>,
    pub client_id: Option<String>,
    pub client_secret_ref: Option<String>,
    pub scope: Option<String>,
    pub paths: BTreeMap<String, String>,
    pub chatgpt_extra_parameters: Vec<String>,
    pub requests: BTreeMap<String, RequestProfile>,
    pub catalog_capabilities: CapabilityPaths,
}
pub(crate) fn name(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAI => "openai",
        Provider::Anthropic => "anthropic",
        Provider::Gemini => "gemini",
        Provider::XAI => "xai",
        #[cfg(feature = "workbuddy-lab")]
        Provider::Workbuddy => "workbuddy",
    }
}
impl Profile {
    pub(crate) fn client_secret_for(&self, operation: &str) -> Option<&String> {
        self.requests
            .get(operation)
            .and_then(|r| r.client_secret_ref.as_ref())
            .or(self.client_secret_ref.as_ref())
    }
    pub fn validate(&self, provider: &str) -> Result<(), String> {
        if !matches!(provider, "openai" | "anthropic" | "gemini" | "xai")
            && !(cfg!(feature = "workbuddy-lab") && provider == "workbuddy")
        {
            return Err("unknown compatibility provider".into());
        }
        wire::validate_headers(&self.headers, &[])?;
        self.catalog_capabilities.validate()?;
        if serde_json::to_vec(self)
            .map_err(|_| "invalid profile")?
            .len()
            > 256 * 1024
        {
            return Err("compatibility profile exceeds 256 KiB".into());
        }
        for (operation, request) in &self.requests {
            request.validate(provider, operation)?;
        }
        for value in [&self.client_id, &self.client_secret_ref, &self.scope]
            .into_iter()
            .flatten()
        {
            if value.is_empty() || value.len() > 4096 || value.bytes().any(|b| b.is_ascii_control())
            {
                return Err("invalid OAuth metadata".into());
            }
        }
        if self.chatgpt_extra_parameters.len() > 64
            || self.chatgpt_extra_parameters.iter().any(|p| {
                p.is_empty()
                    || storage_field(p)
                    || p.len() > 64
                    || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    || matches!(
                        p.as_str(),
                        "authorization" | "api_key" | "access_token" | "refresh_token"
                    )
            })
        {
            return Err("invalid extra parameter".into());
        }
        for (key, path) in &self.paths {
            if !matches!(
                key.as_str(),
                "responses"
                    | "messages"
                    | "chat_completions"
                    | "models"
                    | "language_models"
                    | "generate_content"
                    | "stream_generate_content"
                    | "gemini_load"
                    | "gemini_onboard"
                    | "gemini_quota"
            ) || path.is_empty()
                || path.len() > 512
                || path.starts_with('/')
                || !path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
                || path
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
            {
                return Err("invalid vendor relative path".into());
            }
            if matches!(
                key.as_str(),
                "generate_content"
                    | "stream_generate_content"
                    | "gemini_load"
                    | "gemini_onboard"
                    | "gemini_quota"
            ) && !path.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err("Gemini method must be an identifier".into());
            }
        }
        Ok(())
    }
    pub(crate) fn path<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.paths.get(key).map(String::as_str).unwrap_or(default)
    }
}

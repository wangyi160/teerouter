//! Configurable discovery metadata; Agent generation parameters are immutable.
//! No scripts, secret interpolation, URL substitution or response-parser replacement.
use super::Profile;
use crate::Error;
use reqwest::{
    RequestBuilder, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestProfile {
    pub headers: BTreeMap<String, String>,
    pub remove_headers: Vec<String>,
    pub query: BTreeMap<String, String>,
    pub remove_query: Vec<String>,
    pub body_defaults: BTreeMap<String, Value>,
    pub body_set: BTreeMap<String, Value>,
    pub body_remove: Vec<String>,
    pub body_null: Vec<String>,
}

fn valid_header(key: &str) -> bool {
    key == key.to_ascii_lowercase()
        && !super::content_field(key)
        && key.len() <= 128
        && HeaderName::from_bytes(key.as_bytes()).is_ok()
        && !key.starts_with("x-router-")
        && !key.starts_with("proxy-")
        && !matches!(
            key,
            "authorization"
                | "x-api-key"
                | "x-goog-api-key"
                | "cookie"
                | "set-cookie"
                | "host"
                | "content-type"
                | "accept"
                | "content-length"
                | "content-encoding"
                | "accept-encoding"
                | "transfer-encoding"
                | "connection"
                | "te"
                | "trailer"
                | "upgrade"
                | "session_id"
                | "chatgpt-account-id"
        )
}
pub(super) fn validate_headers(
    headers: &BTreeMap<String, String>,
    remove: &[String],
) -> Result<(), String> {
    if headers.len() + remove.len() > 64 {
        return Err("too many compatibility headers".into());
    }
    let mut names = HashSet::new();
    for key in headers.keys().chain(remove) {
        if !valid_header(key) || !names.insert(key) {
            return Err("invalid, protected or conflicting compatibility header".into());
        }
    }
    for value in headers.values() {
        if value.len() > 4096 || HeaderValue::from_str(value).is_err() {
            return Err("invalid compatibility header value".into());
        }
    }
    Ok(())
}
// Caller content, model/reasoning, credentials and account/session identity are invariants.
fn protected(key: &str) -> bool {
    if super::storage_field(key) || super::content_field(key) {
        return true;
    }
    let normalized: String = key
        .chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "model"
            | "servicetier"
            | "speed"
            | "inferencegeo"
            | "cachecontrol"
            | "reasoningeffort"
            | "effort"
            | "thinking"
            | "thinkingconfig"
            | "thinkinglevel"
            | "thinkingbudget"
            | "budgettokens"
            | "previousresponseid"
            | "responseid"
            | "conversation"
            | "conversationid"
            | "sessionid"
            | "userpromptid"
            | "requestid"
            | "fileid"
            | "filedata"
            | "fileuri"
            | "inputfile"
            | "accountid"
            | "chatgptaccountid"
            | "project"
            | "projectid"
            | "cloudaicompanionproject"
            | "duetproject"
            | "tierid"
            | "authorization"
            | "apikey"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "clientsecret"
            | "clientid"
            | "scope"
            | "granttype"
            | "responsetype"
            | "redirecturi"
            | "state"
            | "nonce"
            | "code"
            | "codeverifier"
            | "codechallenge"
            | "codechallengemethod"
            | "deviceauthid"
            | "usercode"
            | "tokenendpoint"
            | "authorizationendpoint"
    )
}

#[cfg(test)]
mod tests;
impl RequestProfile {
    pub(super) fn validate(&self, provider: &str, operation: &str) -> Result<(), String> {
        let supported = match operation {
            "models" => true,
            "chat_completions" => provider != "anthropic",
            "messages" => provider == "anthropic",
            "responses" => matches!(provider, "openai" | "xai"),
            _ => false,
        };
        if !supported {
            return Err("unsupported provider request operation".into());
        }
        validate_headers(&self.headers, &self.remove_headers)?;
        if self.query.len() + self.remove_query.len() > 64 {
            return Err("too many query parameters".into());
        }
        let mut names = HashSet::new();
        for key in self.query.keys().chain(&self.remove_query) {
            if key.is_empty()
                || key.len() > 128
                || !key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                || protected(key)
                || matches!(key.as_str(), "key" | "alt" | "after_id" | "pageToken")
                || !names.insert(key)
            {
                return Err("invalid, protected or conflicting query parameter".into());
            }
        }
        if self
            .query
            .values()
            .any(|v| v.len() > 4096 || v.chars().any(char::is_control))
        {
            return Err("invalid compatibility query value".into());
        }
        self.validate_native_parameters(operation)
            .map_err(str::to_owned)?;
        Ok(())
    }
    fn validate_native_parameters(&self, operation: &str) -> Result<(), &'static str> {
        if !(self.body_defaults.is_empty()
            && self.body_set.is_empty()
            && self.body_remove.is_empty()
            && self.body_null.is_empty())
        {
            return Err("request body rewrites are forbidden; preserve the agent parameters");
        }
        if operation != "models"
            && !(self.headers.is_empty()
                && self.remove_headers.is_empty()
                && self.query.is_empty()
                && self.remove_query.is_empty())
        {
            return Err("generation request metadata rewrites are forbidden");
        }
        Ok(())
    }
}
impl Profile {
    pub(crate) fn prepare(
        &self,
        operation: &str,
        url: &mut Url,
        headers: &mut HeaderMap,
    ) -> Result<(), Error> {
        crate::official::validate(url.as_str(), None)?;
        let Some(rule) = self.requests.get(operation) else {
            return Ok(());
        };
        // Fail closed even for legacy profiles that bypassed configuration validation.
        rule.validate_native_parameters(operation)
            .map_err(Error::InvalidRequest)?;
        for key in &rule.remove_headers {
            headers.remove(key.as_str());
        }
        for (key, value) in &rule.headers {
            headers.insert(
                HeaderName::from_bytes(key.as_bytes())
                    .map_err(|_| Error::InvalidRequest("invalid compatibility header"))?,
                HeaderValue::from_str(value)
                    .map_err(|_| Error::InvalidRequest("invalid compatibility header"))?,
            );
        }
        if !(rule.query.is_empty() && rule.remove_query.is_empty()) {
            let mut pairs: Vec<(String, String)> = url
                .query_pairs()
                .into_owned()
                .filter(|(k, _)| !rule.query.contains_key(k) && !rule.remove_query.contains(k))
                .collect();
            pairs.extend(rule.query.iter().map(|(k, v)| (k.clone(), v.clone())));
            url.set_query(None);
            if !pairs.is_empty() {
                url.query_pairs_mut().extend_pairs(pairs);
            }
        }
        Ok(())
    }
    pub(crate) fn get(
        &self,
        operation: &str,
        http: &reqwest::Client,
        url: impl AsRef<str>,
        mut headers: HeaderMap,
    ) -> Result<RequestBuilder, Error> {
        let mut url =
            Url::parse(url.as_ref()).map_err(|_| Error::InvalidRequest("invalid request URL"))?;
        self.prepare(operation, &mut url, &mut headers)?;
        Ok(http.get(url).headers(headers))
    }
    pub(crate) fn json(
        &self,
        operation: &str,
        http: &reqwest::Client,
        url: impl AsRef<str>,
        mut headers: HeaderMap,
        body: Value,
    ) -> Result<RequestBuilder, Error> {
        let mut url =
            Url::parse(url.as_ref()).map_err(|_| Error::InvalidRequest("invalid request URL"))?;
        self.prepare(operation, &mut url, &mut headers)?;
        Ok(http.post(url).headers(headers).json(&body))
    }
}

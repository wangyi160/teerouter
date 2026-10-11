//! Declarative request metadata, applied after the Rust protocol adapter.
//! No scripts, secret interpolation, URL substitution or response-parser replacement.
use super::Profile;
use crate::Error;
use reqwest::{
    RequestBuilder, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_ref: Option<String>,
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

// Static refresh application metadata is configurable; account credentials and
// model/reasoning fields remain protected.
fn oauth_metadata(operation: &str, key: &str) -> bool {
    operation == "oauth_refresh" && matches!(key, "client_id" | "scope")
}
fn body_pointer(operation: &str, path: &str) -> Result<Vec<String>, String> {
    if path
        .strip_prefix('/')
        .is_some_and(|key| oauth_metadata(operation, key))
    {
        return Ok(vec![path[1..].to_owned()]);
    }
    pointer(path)
}

#[cfg(test)]
mod tests;
fn pointer(path: &str) -> Result<Vec<String>, String> {
    if !path.starts_with('/') || path.len() > 512 {
        return Err("body key must be a JSON pointer".into());
    }
    let mut parts = Vec::new();
    for part in path[1..].split('/') {
        let mut decoded = String::new();
        let mut chars = part.chars();
        while let Some(c) = chars.next() {
            decoded.push(if c == '~' {
                match chars.next() {
                    Some('0') => '~',
                    Some('1') => '/',
                    _ => return Err("invalid JSON pointer escape".into()),
                }
            } else {
                c
            });
        }
        if decoded.is_empty() || decoded.chars().any(char::is_control) || protected(&decoded) {
            return Err("invalid or protected body field".into());
        }
        parts.push(decoded);
    }
    if parts.len() > 16
        || parts.last().is_some_and(|p| {
            matches!(
                p.as_str(),
                "request"
                    | "reasoning"
                    | "generationConfig"
                    | "generation_config"
                    | "output_config"
                    | "outputConfig"
            )
        })
    {
        return Err("body pointer is too deep or replaces a protected parameter container".into());
    }
    Ok(parts)
}
fn safe_value(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .all(|(key, v)| !protected(key) && key != "request" && safe_value(v)),
        Value::Array(values) => values.iter().all(safe_value),
        _ => true,
    }
}
impl RequestProfile {
    pub(super) fn validate(&self, provider: &str, operation: &str) -> Result<(), String> {
        let supported = match operation {
            "models" => true,
            "chat_completions" => provider != "anthropic",
            "messages" => provider == "anthropic",
            "responses" => matches!(provider, "openai" | "xai"),
            "generate_content"
            | "stream_generate_content"
            | "gemini_load"
            | "gemini_onboard"
            | "gemini_quota"
            | "gemini_operation" => provider == "gemini",
            "oauth_discovery" => provider == "xai",
            "oauth_refresh" => matches!(provider, "openai" | "gemini" | "xai"),
            _ => false,
        };
        if !supported {
            return Err("unsupported provider request operation".into());
        }
        if let Some(reference) = &self.client_secret_ref
            && (provider != "gemini"
                || operation != "oauth_refresh"
                || reference.is_empty()
                || reference.len() > 4096
                || reference.chars().any(char::is_control))
        {
            return Err("invalid operation client secret reference".into());
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
        let count = self.body_set.len()
            + self.body_defaults.len()
            + self.body_remove.len()
            + self.body_null.len();
        if count > 64
            || (count > 0 && matches!(operation, "models" | "gemini_operation" | "oauth_discovery"))
        {
            return Err("body rules exceed limit or target a bodyless operation".into());
        }
        let mut paths: Vec<Vec<String>> = Vec::new();
        let form = operation == "oauth_refresh" && provider != "openai";
        if form && !self.body_null.is_empty() {
            return Err("form parameters cannot be null".into());
        }
        for path in self
            .body_set
            .keys()
            .chain(self.body_defaults.keys())
            .chain(&self.body_remove)
            .chain(&self.body_null)
        {
            let parts = if self.body_set.contains_key(path) {
                body_pointer(operation, path)?
            } else {
                pointer(path)?
            };
            if path
                .strip_prefix('/')
                .is_some_and(|key| oauth_metadata(operation, key))
                && !self
                    .body_set
                    .get(path)
                    .and_then(Value::as_str)
                    .is_some_and(|v| {
                        !v.is_empty() && v.len() <= 4096 && !v.chars().any(char::is_control)
                    })
            {
                return Err("OAuth metadata must be a nonempty string".into());
            }
            if form && parts.len() != 1 {
                return Err("form parameters must be top-level".into());
            }
            if paths
                .iter()
                .any(|p| p.starts_with(&parts) || parts.starts_with(p))
            {
                return Err("overlapping body rules".into());
            }
            paths.push(parts);
        }
        for value in self.body_set.values().chain(self.body_defaults.values()) {
            if !safe_value(value)
                || (form && !(value.is_string() || value.is_number() || value.is_boolean()))
            {
                return Err("protected or invalid compatibility body value".into());
            }
        }
        Ok(())
    }
}
enum Edit<'a> {
    Set(&'a Value),
    Default(&'a Value),
    Remove,
}
fn edit(body: &mut Value, parts: &[String], operation: Edit<'_>) -> Result<(), Error> {
    let (key, rest) = parts
        .split_first()
        .ok_or(Error::InvalidRequest("invalid body pointer"))?;
    if body.is_null() && !matches!(operation, Edit::Remove) {
        *body = Value::Object(Map::new());
    }
    let map = body.as_object_mut().ok_or(Error::InvalidRequest(
        "compatibility body pointer crosses a non-object",
    ))?;
    if rest.is_empty() {
        match operation {
            Edit::Set(value) => {
                map.insert(key.clone(), value.clone());
            }
            Edit::Default(value) => {
                map.entry(key.clone()).or_insert_with(|| value.clone());
            }
            Edit::Remove => {
                map.remove(key);
            }
        }
        return Ok(());
    }
    if matches!(operation, Edit::Remove) && !map.contains_key(key) {
        return Ok(());
    }
    edit(
        map.entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new())),
        rest,
        operation,
    )
}
impl Profile {
    pub(crate) fn prepare(
        &self,
        operation: &str,
        url: &mut Url,
        headers: &mut HeaderMap,
        body: Option<&mut Value>,
    ) -> Result<(), Error> {
        crate::official::validate(url.as_str(), None)?;
        let Some(rule) = self.requests.get(operation) else {
            return Ok(());
        };
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
        if let Some(body) = body {
            let invariants = operation_invariants(operation, body);
            for key in &rule.body_null {
                edit(
                    body,
                    &pointer(key)
                        .map_err(|_| Error::InvalidRequest("invalid compatibility pointer"))?,
                    Edit::Set(&Value::Null),
                )?;
            }
            for key in &rule.body_remove {
                edit(
                    body,
                    &pointer(key)
                        .map_err(|_| Error::InvalidRequest("invalid compatibility pointer"))?,
                    Edit::Remove,
                )?;
            }
            for (key, value) in &rule.body_defaults {
                edit(
                    body,
                    &pointer(key)
                        .map_err(|_| Error::InvalidRequest("invalid compatibility pointer"))?,
                    Edit::Default(value),
                )?;
            }
            for (key, value) in &rule.body_set {
                edit(
                    body,
                    &body_pointer(operation, key)
                        .map_err(|_| Error::InvalidRequest("invalid compatibility pointer"))?,
                    Edit::Set(value),
                )?;
            }
            if invariants != operation_invariants(operation, body) {
                return Err(Error::InvalidRequest(
                    "compatibility rule changes caller content or protected request fields",
                ));
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
        self.prepare(operation, &mut url, &mut headers, None)?;
        Ok(http.get(url).headers(headers))
    }
    pub(crate) fn json(
        &self,
        operation: &str,
        http: &reqwest::Client,
        url: impl AsRef<str>,
        mut headers: HeaderMap,
        mut body: Value,
    ) -> Result<RequestBuilder, Error> {
        // Responses storage is a router invariant, including native requests and
        // ChatGPT's Chat adapter. Apply before edits so invariant checks protect it.
        if operation == "responses" {
            body.as_object_mut()
                .ok_or(Error::InvalidRequest("request must be an object"))?
                .insert("store".into(), Value::Bool(false));
        }
        let mut url =
            Url::parse(url.as_ref()).map_err(|_| Error::InvalidRequest("invalid request URL"))?;
        self.prepare(operation, &mut url, &mut headers, Some(&mut body))?;
        Ok(http.post(url).headers(headers).json(&body))
    }
    pub(crate) fn form(
        &self,
        operation: &str,
        http: &reqwest::Client,
        url: impl AsRef<str>,
        params: &[(&str, &str)],
    ) -> Result<RequestBuilder, Error> {
        let mut body = Value::Object(
            params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned())))
                .collect(),
        );
        let mut url =
            Url::parse(url.as_ref()).map_err(|_| Error::InvalidRequest("invalid request URL"))?;
        let mut headers = HeaderMap::new();
        self.prepare(operation, &mut url, &mut headers, Some(&mut body))?;
        let params: Result<BTreeMap<_, _>, Error> = body
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    Value::String(s) => s.clone(),
                    Value::Bool(_) | Value::Number(_) => v.to_string(),
                    _ => {
                        return Err(Error::InvalidRequest(
                            "invalid compatibility form parameter",
                        ));
                    }
                };
                Ok((k, value))
            })
            .collect();
        Ok(http.post(url).headers(headers).form(&params?))
    }
}

fn operation_invariants(operation: &str, body: &Value) -> BTreeMap<String, Value> {
    let mut values = invariants(body);
    values.retain(|path, _| {
        !path
            .strip_prefix('/')
            .is_some_and(|key| oauth_metadata(operation, key))
    });
    values
}

// Removing/replacing an ancestor must not erase a protected descendant (including
// per-message reasoning settings). Check presence as well as value, including null.
fn invariants(body: &Value) -> BTreeMap<String, Value> {
    fn collect(value: &Value, path: &str, out: &mut BTreeMap<String, Value>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let path = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                    if protected(key) {
                        out.insert(path, value.clone());
                    } else {
                        collect(value, &path, out);
                    }
                }
            }
            Value::Array(values) => {
                for (i, value) in values.iter().enumerate() {
                    collect(value, &format!("{path}/{i}"), out);
                }
            }
            _ => {}
        }
    }
    let mut result = BTreeMap::new();
    collect(body, "", &mut result);
    result
}

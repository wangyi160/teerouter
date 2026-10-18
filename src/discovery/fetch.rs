//! Provider wire formats, based on the primary sources recorded in UPSTREAM.json.
use super::{ModelInfo, capabilities::Capabilities};
use crate::{Client, Error, Provider, config::valid_model_id};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

type ModelCatalog = (BTreeMap<String, ModelInfo>, Option<i64>);

pub(crate) async fn models(client: &Client, version: &str) -> Result<ModelCatalog, Error> {
    let revision = || client.oauth.as_ref().map(|a| a.revision()).transpose();
    let mut headers = HeaderMap::new();
    let mut stable = false;
    let mut identity = None;
    for _ in 0..3 {
        identity = revision()?;
        headers = client.auth_headers(HeaderMap::new()).await?;
        if revision()? == identity {
            stable = true;
            break;
        }
    }
    if !stable {
        return Err(Error::OAuth("account changed during model discovery"));
    }
    headers.insert("accept", HeaderValue::from_static("application/json"));
    let is_codex = client.provider == Provider::OpenAI && client.oauth.is_some();
    let base = client.base_url.as_str().trim_end_matches('/');
    let endpoint = if client.provider == Provider::XAI {
        format!(
            "{base}/{}",
            client.profile.path("language_models", "language-models")
        )
    } else {
        format!("{base}/{}", client.profile.path("models", "models"))
    };
    let mut models: BTreeMap<String, ModelInfo> = BTreeMap::new();
    let mut cursor: Option<String> = None;
    let mut seen = BTreeSet::new();
    for _ in 0..20 {
        let mut url = reqwest::Url::parse(&endpoint)
            .map_err(|_| Error::InvalidRequest("invalid model discovery URL"))?;
        if is_codex {
            url.query_pairs_mut().append_pair("client_version", version);
        }
        if client.provider == Provider::Anthropic {
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(c) = &cursor {
                url.query_pairs_mut().append_pair("after_id", c);
            }
        }
        let request = client
            .profile
            .get("models", &client.http, url, headers.clone())?;
        let body = read(request.send().await?).await?;
        let (field, id) = if is_codex {
            ("models", "slug")
        } else if client.provider == Provider::XAI {
            ("models", "id")
        } else {
            ("data", "id")
        };
        let entries = body[field]
            .as_array()
            .ok_or(Error::InvalidResponse("model array missing"))?;
        for entry in entries {
            if entry["visibility"] == "hide" || entry["lifecycle"] == "retired" {
                continue;
            }
            let Some(raw) = entry[id].as_str() else {
                return Err(Error::InvalidResponse("model ID missing"));
            };
            let model = raw;
            let capability = Capabilities::read(
                client.provider,
                is_codex,
                entry,
                &client.profile.catalog_capabilities,
            )?;
            let mut names = vec![model];
            if client.provider == Provider::XAI
                && let Some(aliases) = entry.get("aliases")
            {
                for alias in aliases
                    .as_array()
                    .ok_or(Error::InvalidResponse("invalid model aliases"))?
                {
                    names.push(
                        alias
                            .as_str()
                            .ok_or(Error::InvalidResponse("invalid model alias"))?,
                    );
                }
            }
            for name in names {
                if !valid_model_id(name) {
                    return Err(Error::InvalidResponse("invalid model ID"));
                }
                if models
                    .get(name)
                    .is_some_and(|old| old.capabilities != capability || old.canonical_id != model)
                {
                    return Err(Error::InvalidResponse("conflicting model capabilities"));
                }
                models.insert(
                    name.to_owned(),
                    ModelInfo {
                        canonical_id: model.to_owned(),
                        capabilities: capability.clone(),
                    },
                );
            }
            if models.len() > 10000 {
                return Err(Error::InvalidResponse("model catalog too large"));
            }
        }
        cursor = if client.provider == Provider::Anthropic && body["has_more"] == true {
            Some(
                body["last_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or(Error::InvalidResponse("model cursor missing"))?
                    .to_owned(),
            )
        } else {
            None
        };
        let Some(c) = &cursor else {
            if revision()? != identity {
                return Err(Error::OAuth("account changed during model discovery"));
            }
            return Ok((models, identity));
        };
        if c.len() > 2048 || !seen.insert(c.clone()) {
            return Err(Error::InvalidResponse("invalid model pagination"));
        }
    }
    Err(Error::InvalidResponse("model catalog page limit exceeded"))
}

async fn read(mut response: reqwest::Response) -> Result<Value, Error> {
    if !response.status().is_success() {
        return Err(Error::Api {
            status: response.status().as_u16(),
            body: String::new(),
            retry_after_secs: None,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(Error::InvalidResponse("model catalog too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse("invalid model catalog JSON"))
}

//! Ported from google-gemini/gemini-cli code_assist/{setup,server,converter}.ts,
//! revision 44d764ee, Copyright Google LLC, Apache-2.0 (see LICENSES/Gemini-CLI.txt).
use crate::{Client, Error};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub(crate) struct CodeAssist {
    configured_project: Option<String>,
    project: Mutex<Option<String>>,
}

impl CodeAssist {
    pub fn new(project: Option<String>) -> Self {
        Self {
            configured_project: project,
            project: Mutex::new(None),
        }
    }

    pub(crate) async fn project(&self, client: &Client) -> Result<String, Error> {
        let mut cached = self.project.lock().await;
        if let Some(project) = cached.as_ref() {
            return Ok(project.clone());
        }
        let mut metadata = json!({"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"});
        let mut request = json!({});
        if let Some(project) = &self.configured_project {
            metadata["duetProject"] = project.clone().into();
            request["cloudaicompanionProject"] = project.clone().into();
        }
        request["metadata"] = metadata.clone();
        let loaded = metadata_json(
            checked(
                client
                    .profile
                    .json(
                        "gemini_load",
                        &client.http,
                        method_url(client, client.profile.path("gemini_load", "loadCodeAssist")),
                        client.auth_headers(HeaderMap::new()).await?,
                        request,
                    )?
                    .send()
                    .await?,
            )
            .await?,
        )
        .await?;
        let project = if loaded["currentTier"].is_object() {
            loaded["cloudaicompanionProject"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| self.configured_project.clone())
        } else {
            let tier = loaded["allowedTiers"]
                .as_array()
                .and_then(|tiers| tiers.iter().find(|tier| tier["isDefault"] == true))
                .and_then(|tier| tier["id"].as_str())
                .unwrap_or("legacy-tier");
            let mut request = json!({"tierId":tier,"metadata":metadata});
            if tier != "free-tier" {
                if let Some(project) = &self.configured_project {
                    request["cloudaicompanionProject"] = project.clone().into();
                }
            } else {
                request["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .remove("duetProject");
            }
            let mut operation = metadata_json(
                checked(
                    client
                        .profile
                        .json(
                            "gemini_onboard",
                            &client.http,
                            method_url(
                                client,
                                client.profile.path("gemini_onboard", "onboardUser"),
                            ),
                            client.auth_headers(HeaderMap::new()).await?,
                            request,
                        )?
                        .send()
                        .await?,
                )
                .await?,
            )
            .await?;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
            while operation["done"] != true {
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::OAuth("Gemini CLI onboarding timed out"));
                }
                let name = operation["name"]
                    .as_str()
                    .ok_or(Error::OAuth("Gemini CLI onboarding returned no operation"))?;
                // Operation names are paths under this configured API base, never arbitrary URLs.
                if !name.starts_with("operations/")
                    || name.split('/').any(|s| s == "..")
                    || name.contains(['?', '#', '\\'])
                {
                    return Err(Error::OAuth("invalid Gemini CLI operation name"));
                }
                let url = format!(
                    "{}/{}",
                    client.base_url.as_str().trim_end_matches('/'),
                    name
                );
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                operation = metadata_json(
                    checked(
                        client
                            .profile
                            .get(
                                "gemini_operation",
                                &client.http,
                                url,
                                client.auth_headers(HeaderMap::new()).await?,
                            )?
                            .send()
                            .await?,
                    )
                    .await?,
                )
                .await?;
            }
            if operation["error"].is_object() {
                return Err(Error::OAuth("Gemini CLI onboarding failed"));
            }
            operation["response"]["cloudaicompanionProject"]["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| self.configured_project.clone())
        }
        .filter(|s| !s.is_empty())
        .ok_or(Error::OAuth(
            "Gemini CLI account is ineligible or requires a project",
        ))?;
        *cached = Some(project.clone());
        Ok(project)
    }

    pub async fn send(
        &self,
        client: &Client,
        model: &str,
        mut body: Value,
        streaming: bool,
    ) -> Result<reqwest::Response, Error> {
        let project = self.project(client).await?;
        let object = body
            .as_object_mut()
            .ok_or(Error::InvalidRequest("Gemini request must be an object"))?;
        object.remove("model");
        object.remove("stream");
        let request = json!({"model":model,"project":project,"user_prompt_id":super::random_string()?,"request":body});
        let operation = if streaming {
            "stream_generate_content"
        } else {
            "generate_content"
        };
        let mut url = reqwest::Url::parse(&method_url(
            client,
            if streaming {
                client.profile.path(operation, "streamGenerateContent")
            } else {
                client.profile.path(operation, "generateContent")
            },
        ))
        .map_err(|_| Error::InvalidRequest("invalid Code Assist URL"))?;
        if streaming {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
        checked(
            client
                .profile
                .json(
                    operation,
                    &client.http,
                    url,
                    client.auth_headers(HeaderMap::new()).await?,
                    request,
                )?
                .send()
                .await?,
        )
        .await
    }
}

fn method_url(client: &Client, method: &str) -> String {
    format!(
        "{}:{method}",
        client.base_url.as_str().trim_end_matches('/')
    )
}

pub(crate) async fn checked(response: reqwest::Response) -> Result<reqwest::Response, Error> {
    if !response.status().is_success() {
        return Err(Error::Api {
            status: response.status().as_u16(),
            retry_after_secs: response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok()),
            body: String::new(),
        });
    }
    Ok(response)
}
async fn metadata_json(response: reqwest::Response) -> Result<Value, Error> {
    crate::resources::json(response, 1024 * 1024).await
}

pub(crate) fn unwrap_response(mut value: Value) -> Result<Value, Error> {
    let mut response = value
        .get_mut("response")
        .map(Value::take)
        .filter(Value::is_object)
        .ok_or(Error::InvalidResponse(
            "Gemini CLI response wrapper missing",
        ))?;
    if value["traceId"].is_string() {
        response["responseId"] = value["traceId"].take();
    }
    Ok(response)
}

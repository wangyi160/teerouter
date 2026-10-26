//! Native Codex forwarding; response decoding originated in LiteLLM.
use crate::{Client, Error};
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

pub(crate) async fn send(
    client: &Client,
    model: &str,
    mut request: Value,
    mut headers: HeaderMap,
) -> Result<reqwest::Response, Error> {
    request
        .as_object_mut()
        .ok_or(Error::InvalidRequest("Responses request must be an object"))?
        .insert("model".into(), model.into());
    let mut url = client.base_url.clone();
    url.set_path(&format!(
        "{}/{}",
        url.path().trim_end_matches('/'),
        client.profile.path("responses", "responses")
    ));
    crate::client::take_native_query(&mut headers, &mut url)?;
    let mut native_client = client.clone();
    native_client.profile.headers.clear();
    client
        .checked(
            client
                .dispatch(client.profile.json(
                    "responses",
                    &client.http,
                    url,
                    native_client.auth_headers(headers).await?,
                    request,
                )?)
                .await?,
            crate::upstream::Protocol::Responses,
        )
        .await
}

// Some backends return SSE even for JSON callers and omit the terminal output array.
pub(crate) async fn collect(mut events: crate::upstream::EventStream) -> Result<Value, Error> {
    let mut items = std::collections::BTreeMap::new();
    let mut texts = std::collections::BTreeMap::<(u64, u64), Value>::new();
    while let Some(frame) = events.next().await {
        let Some(mut value) = frame?.value else {
            continue;
        };
        match value["type"].as_str() {
            Some("response.output_item.done") => {
                items.insert(
                    value["output_index"].as_u64().unwrap_or(items.len() as u64),
                    value["item"].take(),
                );
            }
            Some("response.output_text.done") => {
                texts.insert(
                    (
                        value["output_index"].as_u64().unwrap_or(0),
                        value["content_index"].as_u64().unwrap_or(0),
                    ),
                    json!({"type":"output_text","text":value["text"],"annotations":[]}),
                );
            }
            Some("response.refusal.delta" | "response.refusal.done") => {
                let key = (
                    value["output_index"].as_u64().unwrap_or(0),
                    value["content_index"].as_u64().unwrap_or(0),
                );
                if value["type"] == "response.refusal.done" {
                    texts.insert(key, json!({"type":"refusal","refusal":value["refusal"]}));
                } else if let Some(delta) = value["delta"].as_str() {
                    let part = texts
                        .entry(key)
                        .or_insert_with(|| json!({"type":"refusal","refusal":""}));
                    let text = part["refusal"].as_str().unwrap_or("").to_owned() + delta;
                    part["refusal"] = text.into();
                }
            }
            Some("response.content_part.done") if value["part"]["type"] == "refusal" => {
                texts.insert(
                    (
                        value["output_index"].as_u64().unwrap_or(0),
                        value["content_index"].as_u64().unwrap_or(0),
                    ),
                    value["part"].clone(),
                );
            }
            Some("response.completed" | "response.incomplete") => {
                let mut response = value["response"].take();
                if !response.is_object() {
                    return Err(Error::InvalidResponse("missing terminal response"));
                }
                if response["output"].as_array().is_none_or(Vec::is_empty) {
                    let mut text_items = std::collections::BTreeMap::new();
                    for ((output_index, _), text) in texts {
                        let item = text_items.entry(output_index).or_insert_with(||json!({"type":"message","role":"assistant","status":"completed","content":[]}));
                        item["content"].as_array_mut().unwrap().push(text);
                    }
                    // Preserve text-only indices alongside actual output items; actual items win.
                    text_items.extend(items);
                    items = text_items;
                    response["output"] = items.into_values().collect::<Vec<_>>().into();
                }
                return Ok(response);
            }
            _ => {}
        }
    }
    Err(Error::InvalidResponse("missing completed Responses event"))
}

pub(crate) async fn decode(
    response: reqwest::Response,
    streaming: bool,
    limits: crate::resources::Limits,
    refusal: Option<crate::refusal::Observer>,
    billing: Option<crate::billing::Observer>,
) -> Result<crate::upstream::Output, Error> {
    use crate::upstream::{Output, Protocol, decode_native};
    // The Python implementation also accepts an ordinary JSON response for nonstream callers.
    let is_json = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        });
    let output = decode_native(
        response,
        Protocol::Responses,
        streaming || !is_json,
        limits,
        refusal,
        billing,
    )
    .await?;
    match output {
        Output::Stream(events) if !streaming => collect(events).await.map(Output::Json),
        output => Ok(output),
    }
}

use super::*;
use crate::runtime::Runtime;
use std::{sync::Arc, time::Duration};

pub(crate) fn start(runtime: &Arc<Runtime>) {
    if !runtime.is_platform() {
        return;
    }
    let weak = Arc::downgrade(runtime);
    tokio::spawn(async move {
        loop {
            let Some(runtime) = weak.upgrade() else {
                break;
            };
            if let Some(_job) = runtime.background_job() {
                let vault = runtime.engine().vault.clone();
                let _ = vault.publish_public(false).await;
            }
            drop(runtime);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
impl Vault {
    pub(crate) async fn publish_public(&self, force: bool) -> Result<Value> {
        let _single = self.relay_upload.lock().await;
        let result = self.publish_one(force).await;
        if let Err(ref error) = result {
            // Only local fixed error codes, never URLs, bearer headers or bodies.
            self.relay_publishing.lock().unwrap().last_error = Some(error.0);
        }
        result
    }
    async fn publish_one(&self, force: bool) -> Result<Value> {
        let Some((config, body)) = self.prepare_public(force)? else {
            return self.relay_status();
        };
        let manifest: Value = serde_json::from_str(body["manifest"].as_str().unwrap()).unwrap();
        let endpoint = format!(
            "{}/v1/streams/{}/batches/{}",
            config.url.trim_end_matches('/'),
            manifest["instance_id"].as_str().unwrap(),
            manifest["batch_no"]
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| VaultError("relay_client_unavailable"))?;
        let mut response = client
            .put(endpoint)
            .bearer_auth(&config.upload_token)
            .json(&body)
            .send()
            .await
            .map_err(|_| VaultError("relay_network_unavailable"))?;
        if !response.status().is_success() {
            return Err(VaultError(match response.status().as_u16() {
                401 => "relay_upload_unauthorized",
                403 => "relay_signer_not_registered",
                409 => "relay_chain_conflict",
                _ => "relay_service_unavailable",
            }));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| VaultError("relay_reply_incomplete"))?
        {
            if bytes.len() + chunk.len() > 16_384 {
                return Err(VaultError("relay_reply_too_large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let ack: Value =
            serde_json::from_slice(&bytes).map_err(|_| VaultError("invalid_relay_ack"))?;
        if ack["durable"] != true
            || ack["batch_hash"] != body["batch_hash"]
            || !matches!(ack["detail_state"].as_str(), Some("pending" | "archived"))
            || ack["settlement_acknowledged"] != false
        {
            return Err(VaultError("invalid_relay_ack"));
        }
        self.uploaded_public(body["batch_hash"].as_str().unwrap())?;
        self.relay_status()
    }
}

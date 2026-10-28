//! Owner-only, retry-safe access-token replacement. No refresh credentials cross this API.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    expected_access_sha256: String,
    credential: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    runtime_id: String,
    accounts: BTreeMap<String, Replacement>,
}

pub(super) async fn replace(
    runtime: &Runtime,
    token: &str,
    body: Value,
) -> Result<Value, ApiError> {
    let input: Input = serde_json::from_value(body).map_err(|_| invalid())?;
    if input.accounts.is_empty() || input.accounts.len() > 256 {
        return Err(invalid());
    }
    let _write = runtime.merchants.write.lock().await;
    let item = authenticate(runtime, token)?;
    let live = live(runtime, &item.id)?;
    let engine = live.runtime.engine();
    if engine.vault.lock().map_err(storage)?.instance != input.runtime_id {
        return Err(ApiError::new(
            409,
            "merchant_runtime_changed",
            "Reconnect before uploading access tokens",
        ));
    }
    let snapshot = engine.vault.snapshot().map_err(storage)?;
    let mut updates = BTreeMap::new();
    for (name, replacement) in input.accounts {
        let account = snapshot.bundle.accounts.get(&name).ok_or_else(invalid)?;
        // Validate even an idempotent retry; never accept a refresh field hidden
        // alongside an otherwise unchanged access token.
        let preview = Vault::memory().map_err(storage)?;
        preview
            .import_account(&name, account.provider, &replacement.credential)
            .map_err(|_| {
                ApiError::new(
                    400,
                    "access_token_only",
                    "Only access-token credentials are accepted",
                )
            })?;
        if replacement.credential == account.credential {
            continue;
        }
        if crate::oauth::tokens::access_fingerprint(&account.credential)
            != replacement.expected_access_sha256
        {
            return Err(ApiError::new(
                409,
                "account_credential_changed",
                "Reload account state before replacing credentials",
            ));
        }
        updates.insert(name, json!({"credential":replacement.credential}));
    }
    if !updates.is_empty() {
        live.runtime
            .apply(
                Update {
                    expected_revision: Some(snapshot.revision),
                    accounts: updates,
                    ..Update::default()
                },
                &live.admin,
            )
            .await?;
    }
    state(runtime, &item)
}

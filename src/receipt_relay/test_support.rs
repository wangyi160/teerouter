//! Test adapters call the real relay implementation with a truthful storage ACK.
use super::*;

pub(crate) fn enable(v: &Vault, users: &[&str]) {
    if v.lock().unwrap().receipt_relay.is_some() { return; }
    let mut names: std::collections::BTreeSet<String> = users.iter().map(|u| (*u).into()).collect();
    if let Some(bundle) = &v.lock().unwrap().bundle {
        names.extend(bundle.config.keys.iter().map(|k| k.user_id.clone()));
    }
    v.configure_relay(Config { url:"https://example.org".into(), upload_token:"t".repeat(64),
        interval_seconds:300, user_ids:names.into_iter().enumerate().map(|(i,u)| (u,format!("{:064x}",i+1))).collect() }).unwrap();
}

pub(crate) fn upload(v: &Vault) -> Option<Head> {
    while let Some((_, body)) = v.prepare_public(true).unwrap() {
        v.uploaded_public(body["batch_hash"].as_str().unwrap()).unwrap();
    }
    v.lock().unwrap().receipt_relay.as_ref().unwrap().uploaded.clone()
}

pub(crate) fn settlement(v: &Vault, balances: BTreeMap<String,i64>) -> Value {
    let through = upload(v);
    let revision = v.lock().unwrap().receipt_relay.as_ref().unwrap().balance_revision+1;
    json!({"boot_id":v.boot_id,"revision":revision,"through":through,"balances":balances,"balances_patch":null})
}

pub(crate) fn settle(v: &Vault, balances: BTreeMap<String,i64>) {
    let ack = settlement(v, balances);
    v.relay_settle(serde_json::from_value(ack).unwrap()).unwrap();
}

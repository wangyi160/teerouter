//! One bounded, process-local baseline. A missing/stale base falls back to a full
//! snapshot. Export never signs receipts or advances financial settlement.
use super::*;

fn changes(before: &Value, after: &Value, path: &mut Vec<String>, out: &mut Vec<Value>) {
    if before == after {
        return;
    }
    if let (Some(old), Some(new)) = (before.as_object(), after.as_object()) {
        for key in old.keys().filter(|key| !new.contains_key(*key)) {
            path.push(key.clone());
            out.push(json!({"path":path,"remove":true}));
            path.pop();
        }
        for (key, value) in new {
            path.push(key.clone());
            if let Some(previous) = old.get(key) {
                changes(previous, value, path, out);
            } else {
                out.push(json!({"path":path,"value":value}));
            }
            path.pop();
        }
    } else {
        // Arrays are atomic, including ordered receipt batches. Null is a value,
        // not a deletion marker; this also preserves optional credential fields.
        out.push(json!({"path":path,"value":after}));
    }
}

impl Vault {
    pub(crate) fn checkpoint_export(
        &self,
        boot: Option<&str>,
        base: Option<&str>,
    ) -> Result<Value> {
        let data = self.lock()?;
        let mut sync = self.sync.lock().unwrap();
        let state = serde_json::to_value(&*data).map_err(|_| VaultError("invalid snapshot"))?;
        let id = random_id()?;
        let mut result = json!({"version":1,"balance_sync_version":1,"router_access_version":1,"checkpoint_diff_version":1,"receipt_proof_version":1,"merchant_refusal_limit_version":1,
            "receipt_relay_version":1,"receipt_public_key":self.receipt_signer.public_key(),
            "boot_id":self.boot_id,"initialized":sync.initialized,"checkpoint_id":id});
        if let Some((previous_id, previous)) = &sync.diff_base
            && boot == Some(self.boot_id.as_str())
            && base == Some(previous_id.as_str())
        {
            let mut delta = Vec::new();
            changes(previous, &state, &mut Vec::new(), &mut delta);
            result["base_id"] = previous_id.clone().into();
            result["changes"] = delta.into();
        } else {
            result["state"] = state.clone();
        }
        sync.diff_base = Some((id, state));
        // Relay-only managers no longer call the retired private ACK endpoint.
        // A successful authenticated checkpoint pull is also evidence of contact.
        if sync.managed && sync.initialized {
            sync.last_contact = Some(Instant::now());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_pull_updates_contact_without_a_legacy_ack() {
        let vault = Vault::memory().unwrap();
        vault.require_manager();
        vault.initialize(Data::default()).unwrap();
        assert!(!vault.manager_connected());
        vault.checkpoint_export(None, None).unwrap();
        assert!(vault.manager_connected());
        vault.sync.lock().unwrap().last_contact = Instant::now().checked_sub(std::time::Duration::from_secs(631));
        assert!(!vault.manager_connected());
        assert!(vault.serving(), "contact freshness must not become a serving lease");
    }

    #[test]
    fn checkpoint_diff_excludes_unchanged_state_and_falls_back_for_wrong_base() {
        let vault = Vault::memory().unwrap();
        vault.lock().unwrap().balances.insert("u".into(), 50);
        let first = vault.checkpoint_export(None, None).unwrap();
        assert!(first.get("state").is_some());
        let unchanged = vault
            .checkpoint_export(Some(&vault.boot_id), first["checkpoint_id"].as_str())
            .unwrap();
        assert_eq!(unchanged["changes"], json!([]));
        assert!(unchanged.get("state").is_none());
        vault.lock().unwrap().balances.insert("u".into(), 75);
        let changed = vault
            .checkpoint_export(Some(&vault.boot_id), unchanged["checkpoint_id"].as_str())
            .unwrap();
        assert_eq!(
            changed["changes"],
            json!([{"path":["balances","u"],"value":75}])
        );
        assert!(
            vault
                .checkpoint_export(Some("old-boot"), changed["checkpoint_id"].as_str())
                .unwrap()
                .get("state")
                .is_some()
        );
        assert!(
            vault
                .checkpoint_export(Some(&vault.boot_id), first["checkpoint_id"].as_str())
                .unwrap()
                .get("state")
                .is_some()
        );
    }

    #[test]
    fn delta_distinguishes_null_deletion_and_array_replacement() {
        let before = json!({"null":3,"removed":1,"array":[1,2],"map":{"same":1,"x":false}});
        let after = json!({"null":null,"array":[3],"map":{"same":1,"x":true},"new":{}});
        let mut delta = Vec::new();
        changes(&before, &after, &mut Vec::new(), &mut delta);
        assert!(delta.contains(&json!({"path":["removed"],"remove":true})));
        assert!(delta.contains(&json!({"path":["null"],"value":null})));
        assert!(delta.contains(&json!({"path":["array"],"value":[3]})));
        assert!(delta.contains(&json!({"path":["map","x"],"value":true})));
        assert!(delta.contains(&json!({"path":["new"],"value":{}})));
        assert_eq!(delta.len(), 5);
    }

}

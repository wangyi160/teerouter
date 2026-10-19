//! One bounded, process-local baseline. A missing/stale base falls back to a full
//! snapshot; acknowledgements still refer to the exact exported generation.
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
    #[cfg(test)]
    pub(crate) fn checkpoint_diff(&self, boot: Option<&str>, base: Option<&str>) -> Result<Value> {
        self.checkpoint_export(boot, base, false)
    }
    pub(crate) fn checkpoint_export(
        &self,
        boot: Option<&str>,
        base: Option<&str>,
        signed: bool,
    ) -> Result<Value> {
        let data = self.lock()?;
        let mut sync = self.sync.lock().unwrap();
        if (signed || data.receipt_head.is_some())
            && sync.initialized
            && sync.receipt_export.is_none()
        {
            sync.receipt_export = Some(self.receipt_signer.seal(&data, &self.boot_id)?);
        }
        let state = match &sync.receipt_export {
            Some(export) => export.state.clone(),
            None => serde_json::to_value(&*data).map_err(|_| VaultError("invalid snapshot"))?,
        };
        let id = random_id()?;
        let mut result = json!({"version":1,"balance_sync_version":1,"checkpoint_diff_version":1,"receipt_proof_version":1,
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
        if let Some(export) = &sync.receipt_export {
            result["receipt_batch"] = export.batch.clone();
        }
        sync.exported = (
            state["generation"].as_u64().unwrap(),
            state["sequence"].as_u64().unwrap(),
        );
        sync.diff_base = Some((id, state));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append(vault: &Vault, charge: u64) {
        let mut data = vault.lock().unwrap();
        data.sequence += 1;
        let seq = data.sequence;
        data.events.push_back(json!({"seq":seq,"event":{"user":"u","billing":{"state":"calculated","charge_microusd":charge}}}));
        data.changed();
    }
    #[test]
    fn signed_checkpoint_retries_freeze_records_and_ack_preserves_new_spend() {
        let vault = Vault::memory().unwrap();
        append(&vault, 7);
        let first = vault.checkpoint_export(None, None, true).unwrap();
        append(&vault, 9);
        let retry = vault.checkpoint_export(None, None, true).unwrap();
        assert_eq!(first["receipt_batch"], retry["receipt_batch"]);
        assert_eq!(first["state"], retry["state"]);
        let generation = first["state"]["generation"].as_u64().unwrap();
        assert!(
            vault
                .acknowledge_balances(&vault.boot_id, generation, 1, None, BTreeMap::new())
                .is_err()
        );
        assert_eq!(vault.lock().unwrap().events.len(), 2);
        let hash = first["receipt_batch"]["batch_hash"].as_str().unwrap();
        for _ in 0..2 {
            vault
                .acknowledge_receipts(
                    &vault.boot_id,
                    generation,
                    1,
                    None,
                    BTreeMap::new(),
                    Some(hash),
                )
                .unwrap();
        }
        assert_eq!(vault.lock().unwrap().events.len(), 1);
        assert_eq!(vault.lock().unwrap().events[0]["seq"], 2);
        let second = vault.checkpoint_export(None, None, true).unwrap();
        let m: Value =
            serde_json::from_str(second["receipt_batch"]["manifest"].as_str().unwrap()).unwrap();
        assert_eq!(m["previous_batch_hash"], hash);
        assert_eq!(m["sequence_before"], 1);
        assert_eq!(m["sequence_through"], 2);
        assert_eq!(m["record_count"], 1);
        assert_eq!(m["charge_microusd"], "9");
    }
    #[test]
    fn restart_is_explicit_and_restored_events_cannot_be_signed() {
        let old = Vault::memory().unwrap();
        append(&old, 5);
        let export = old.checkpoint_export(None, None, true).unwrap();
        old.acknowledge_receipts(
            &old.boot_id,
            export["state"]["generation"].as_u64().unwrap(),
            1,
            None,
            BTreeMap::new(),
            export["receipt_batch"]["batch_hash"].as_str(),
        )
        .unwrap();
        let saved = old.lock().unwrap().clone();
        append(&old, 999); // Deliberately lost: not part of the durable saved state.
        let next = Vault::memory().unwrap();
        next.require_manager();
        assert!(next.initialize(old.lock().unwrap().clone()).is_err());
        next.initialize(saved).unwrap();
        append(&next, 8);
        let export = next.checkpoint_export(None, None, true).unwrap();
        let m: Value =
            serde_json::from_str(export["receipt_batch"]["manifest"].as_str().unwrap()).unwrap();
        assert_eq!(m["continuity"], "restart");
        assert_eq!(m["charge_microusd"], "8");
        assert_eq!(m["previous_boot_id"], old.boot_id);
        assert_ne!(m["public_key"], old.receipt_signer.public_key());
    }

    #[test]
    fn checkpoint_diff_excludes_unchanged_state_and_falls_back_for_wrong_base() {
        let vault = Vault::memory().unwrap();
        vault.lock().unwrap().balances.insert("u".into(), 50);
        let first = vault.checkpoint_diff(None, None).unwrap();
        assert!(first.get("state").is_some());
        let unchanged = vault
            .checkpoint_diff(Some(&vault.boot_id), first["checkpoint_id"].as_str())
            .unwrap();
        assert_eq!(unchanged["changes"], json!([]));
        assert!(unchanged.get("state").is_none());
        vault.lock().unwrap().balances.insert("u".into(), 75);
        let changed = vault
            .checkpoint_diff(Some(&vault.boot_id), unchanged["checkpoint_id"].as_str())
            .unwrap();
        assert_eq!(
            changed["changes"],
            json!([{"path":["balances","u"],"value":75}])
        );
        assert!(
            vault
                .checkpoint_diff(Some("old-boot"), changed["checkpoint_id"].as_str())
                .unwrap()
                .get("state")
                .is_some()
        );
        assert!(
            vault
                .checkpoint_diff(Some(&vault.boot_id), first["checkpoint_id"].as_str())
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

    #[test]
    fn delta_balance_ack_preserves_others_and_is_idempotent() {
        let vault = Vault::memory().unwrap();
        vault.lock().unwrap().balances = BTreeMap::from([("u".into(), 100), ("other".into(), 42)]);
        let cp = vault.checkpoint_diff(None, None).unwrap();
        let generation = cp["state"]["generation"].as_u64().unwrap();
        let patch = BTreeMap::from([("u".into(), Some(70))]);
        for _ in 0..2 {
            vault
                .acknowledge_balances(&vault.boot_id, generation, 0, None, patch.clone())
                .unwrap();
        }
        assert_eq!(
            vault.lock().unwrap().balances,
            BTreeMap::from([("u".into(), 70), ("other".into(), 42)])
        );
        assert!(
            vault
                .acknowledge_balances(
                    &vault.boot_id,
                    generation,
                    0,
                    None,
                    BTreeMap::from([("u".into(), Some(60))])
                )
                .is_err()
        );
        let cp = vault.checkpoint().unwrap();
        vault
            .acknowledge_balances(
                &vault.boot_id,
                cp["state"]["generation"].as_u64().unwrap(),
                0,
                None,
                BTreeMap::from([("other".into(), None)]),
            )
            .unwrap();
        assert!(!vault.lock().unwrap().balances.contains_key("other"));
    }
}

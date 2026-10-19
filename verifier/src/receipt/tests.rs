use super::*;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::json;

fn fixture() -> Value {
    let key = Ed25519KeyPair::from_seed_unchecked(&[42; 32]).unwrap();
    let records:Vec<_>=(1..=3).map(|seq|json!({"salt":hex(&[seq as u8;32]),"payload":json!({"seq":seq,"event":{
        "user_id":"u","key":"k","merchant_id":"m_fixture","model":"grok-4.7","provider":"xai","created_at":1792368000,
        "billing":{"state":"calculated","charge_microusd":24},"request_id":format!("test-{seq}")}}).to_string()})).collect();
    let root_hash = hex(&root(records.iter().map(|r| leaf(r).unwrap()).collect()));
    let m = json!({"schema":"teerouter.consumption-batch.v1","scope":"router_consumption_only","algorithm":"ed25519",
        "public_key":hex(key.public_key().as_ref()),"instance_id":"11".repeat(16),"boot_id":"22".repeat(16),
        "batch_no":1,"previous_batch_hash":null,"previous_boot_id":null,"continuity":"start","created_at_ms":1792368000000u64,
        "sequence_before":0,"sequence_through":3,"record_count":3,"priced_record_count":3,"charge_microusd":"72","currency":"USD","merkle_root":root_hash});
    let manifest = m.to_string();
    let message = [DOMAIN, manifest.as_bytes()].concat();
    json!({"batch":{"manifest":manifest,"signature":hex(key.sign(&message).as_ref()),"batch_hash":hex(&Sha256::digest(&message)),"records":records}})
}

#[test]
fn genuine_signatures_merkle_proofs_and_mutations() {
    let bundle = fixture();
    let batch = &bundle["batch"];
    let (manifest, count) = verify_batch(batch).unwrap();
    assert_eq!(count, 3);
    let leaves: Vec<_> = batch["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| leaf(r).unwrap())
        .collect();
    for index in 0..3 {
        let mut proof = batch["records"][index].clone();
        proof["index"] = index.into();
        proof["siblings"] = if index < 2 {
            json!([
                hex(&leaves[index ^ 1]),
                hex(&parent(&leaves[2], &leaves[2]))
            ])
        } else {
            json!([hex(&leaves[2]), hex(&parent(&leaves[0], &leaves[1]))])
        };
        verify_inclusion(&manifest, &proof).unwrap();
        proof["payload"] = format!("{} ", proof["payload"].as_str().unwrap()).into();
        assert!(verify_inclusion(&manifest, &proof).is_err());
    }
    for field in ["manifest", "signature", "batch_hash"] {
        let mut changed = batch.clone();
        changed[field] = format!("{}0", changed[field].as_str().unwrap()).into();
        assert!(verify_batch(&changed).is_err());
    }
    let mut changed = batch.clone();
    changed["records"][0]["salt"] = "00".repeat(32).into();
    assert!(verify_batch(&changed).is_err());
    changed = batch.clone();
    changed["records"].as_array_mut().unwrap().swap(0, 1);
    assert!(verify_batch(&changed).is_err());
    changed = batch.clone();
    changed["records"].as_array_mut().unwrap().pop();
    assert!(verify_batch(&changed).is_err());
    let mut public = batch.clone();
    public.as_object_mut().unwrap().remove("records");
    assert_eq!(verify_batch(&public).unwrap().1, 0);
    if let Ok(path) = std::env::var("TEEROUTER_FIXTURE_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    }
}
#[test]
fn identity_rejects_sample_and_binds_boot_key_and_challenge() {
    let mut identity = json!({"schema":"teerouter.receipt-key.v1","evidence_type":"dstack_tdx","public_key":"11".repeat(32),"boot_id":"22".repeat(16),"nonce":"33".repeat(32)});
    let first = identity_binding(&identity, Some(&"33".repeat(32))).unwrap();
    identity["boot_id"] = "44".repeat(16).into();
    assert_ne!(first, identity_binding(&identity, None).unwrap());
    assert!(identity_binding(&identity, Some(&"55".repeat(32))).is_err());
    identity["evidence_type"] = "sample".into();
    assert!(identity_binding(&identity, None).is_err());
}

use super::*;

const QUOTE: &[u8] = include_bytes!("../tests/fixtures/tdx/tdx_quote");
const COLLATERAL: &[u8] = include_bytes!("../tests/fixtures/tdx/tdx_quote_collateral.json");
// Historical fixture time inside all four collateral validity windows.
// Production verification always uses the client's current clock.
const FIXTURE_TIME: u64 = 1_750_406_400; // 2025-06-20 08:00:00 UTC

fn fixture_policy(now: u64) -> QuotePolicy {
    // This Intel-signed fixture uses a registered multi-package platform.
    // Only the test opts into its PCK flags. TCB must still be UpToDate.
    QuotePolicy::strict(now)
        .allow_dynamic_platform(true)
        .allow_cached_keys(true)
        .allow_smt(true)
}

fn collateral() -> dcap_qvl::QuoteCollateralV3 {
    serde_json::from_slice(COLLATERAL).unwrap()
}

fn signed_report() -> TDReport10 {
    *QuoteVerifier::new_prod()
        .verify_with_policy(
            QUOTE,
            collateral(),
            FIXTURE_TIME,
            &fixture_policy(FIXTURE_TIME),
        )
        .expect("Intel-signed upstream fixture must pass cryptographic and TCB verification")
        .report
        .as_td10()
        .unwrap()
}

#[test]
fn intel_signed_fixture_validates_and_tampering_and_expiry_fail() {
    signed_report();
    assert!(
        QuoteVerifier::new_prod()
            .verify_with_policy(
                QUOTE,
                collateral(),
                FIXTURE_TIME,
                &QuotePolicy::strict(FIXTURE_TIME)
            )
            .is_err()
    );
    // Quote v4: 48-byte header, REPORT_DATA at byte 520 of TDReport10.
    // Mutate both measured code and the TLS/challenge binding under a real signature.
    for index in [100, 48 + 520] {
        let mut tampered = QUOTE.to_vec();
        tampered[index] ^= 1;
        assert!(
            QuoteVerifier::new_prod()
                .verify_with_policy(
                    &tampered,
                    collateral(),
                    FIXTURE_TIME,
                    &fixture_policy(FIXTURE_TIME)
                )
                .is_err()
        );
    }
    let expired = 1_800_000_000;
    assert!(
        QuoteVerifier::new_prod()
            .verify_with_policy(QUOTE, collateral(), expired, &fixture_policy(expired))
            .is_err()
    );
}

// These fixtures exercise policy appraisal only, never stand in for a signed quote.
fn fixture() -> (TDReport10, Vec<Event>, Policy) {
    let mut td = signed_report();
    td.mr_td = [1; 48];
    td.rt_mr0 = [2; 48];
    td.rt_mr1 = [3; 48];
    td.rt_mr2 = [4; 48];
    td.report_data = [5; 64];
    let rules = vec![
        EventRule {
            event: "compose-hash".into(),
            payload: Some(hex(&[6; 32])),
        },
        EventRule {
            event: "instance-id".into(),
            payload: None,
        },
        EventRule {
            event: "boot-mr-done".into(),
            payload: Some(String::new()),
        },
        EventRule {
            event: "key-provider".into(),
            payload: Some(hex(b"reviewed-kms")),
        },
    ];
    let mut mr = [0; 48];
    let events = rules
        .iter()
        .map(|rule| {
            let payload = rule.payload.clone().unwrap_or_else(|| hex(&[7; 20]));
            let mut bytes = RUNTIME_EVENT.to_le_bytes().to_vec();
            bytes.extend_from_slice(format!(":{}:", rule.event).as_bytes());
            bytes.extend(unhex(&payload).unwrap());
            let digest = Sha384::digest(&bytes);
            let mut extension = mr.to_vec();
            extension.extend_from_slice(&digest);
            mr = Sha384::digest(extension).into();
            Event {
                imr: 3,
                event_type: RUNTIME_EVENT,
                digest: hex(&digest),
                event: rule.event.clone(),
                event_payload: payload,
                version: None,
            }
        })
        .collect();
    td.rt_mr3 = mr;
    let policy = Policy {
        schema_version: 1,
        allow_dynamic_platform: false,
        allow_cached_keys: false,
        allow_smt: false,
        releases: vec![Release {
            id: "unit-test-only".into(),
            event_log_version: 1,
            mr_td: hex(&td.mr_td),
            rtmrs: [hex(&td.rt_mr0), hex(&td.rt_mr1), hex(&td.rt_mr2)],
            events: rules,
        }],
    };
    (td, events, policy)
}

#[test]
fn policy_matches_complete_launch_and_current_channel() {
    let (td, events, policy) = fixture();
    policy.validate().unwrap();
    appraise(&td, &events, &policy.releases[0], &[5; 64]).unwrap();
    assert!(appraise(&td, &events, &policy.releases[0], &[8; 64]).is_err());
}

#[test]
fn production_event_logs_without_redundant_digests_still_require_signed_rtmr3() {
    let (td, mut events, policy) = fixture();
    for event in &mut events {
        event.digest.clear();
    }
    // This is the format returned by the production dstack 0.5.9 GetQuote RPC.
    appraise(&td, &events, &policy.releases[0], &[5; 64]).unwrap();
    let missing: Event = serde_json::from_value(serde_json::json!({
        "imr": 3, "event_type": RUNTIME_EVENT,
        "event": "instance-id", "event_payload": hex(&[7; 20])
    }))
    .unwrap();
    events[1] = missing;
    appraise(&td, &events, &policy.releases[0], &[5; 64]).unwrap();
    // A wildcard instance ID still belongs to the hardware-signed hash chain.
    // Removing the redundant digest must not let an attacker change its bytes.
    events[1].event_payload = hex(&[8; 20]);
    assert!(appraise(&td, &events, &policy.releases[0], &[5; 64]).is_err());
    events[1].event_payload = hex(&[7; 20]);
    let mut changed = td;
    changed.rt_mr3[0] ^= 1;
    assert!(appraise(&changed, &events, &policy.releases[0], &[5; 64]).is_err());
}

#[test]
fn explicitly_approved_v2_launch_replays_canonical_event_bytes() {
    let (mut td, mut events, mut policy) = fixture();
    policy.releases[0].event_log_version = 2;
    policy.validate().unwrap();
    let mut mr = [0u8; 48];
    for event in &mut events {
        event.version = Some(2);
        // BTreeMap serializes this ASCII schema in JCS key order, independently
        // of the production verifier's construction of the digest preimage.
        let object = serde_json::json!({
            "name": event.event, "payload": event.event_payload, "type": 134217729
        });
        let canonical: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_value(object).unwrap();
        let digest = Sha384::digest(serde_json::to_vec(&canonical).unwrap());
        event.digest = hex(&digest);
        let mut bytes = mr.to_vec();
        bytes.extend_from_slice(&digest);
        mr = Sha384::digest(bytes).into();
    }
    td.rt_mr3 = mr;
    appraise(&td, &events, &policy.releases[0], &[5; 64]).unwrap();
    events[0].event_payload = hex(&[9; 32]);
    assert!(appraise(&td, &events, &policy.releases[0], &[5; 64]).is_err());
}

#[test]
fn modified_os_debug_mode_or_rtmr_are_rejected() {
    let (td, events, policy) = fixture();
    for field in 0..6 {
        let mut changed = td;
        match field {
            0 => changed.mr_td[0] ^= 1,
            1 => changed.rt_mr0[0] ^= 1,
            2 => changed.rt_mr1[0] ^= 1,
            3 => changed.rt_mr2[0] ^= 1,
            4 => changed.rt_mr3[0] ^= 1,
            _ => changed.td_attributes[0] |= 1,
        }
        assert!(appraise(&changed, &events, &policy.releases[0], &[5; 64]).is_err());
    }
}

#[test]
fn altered_launch_payload_digest_order_or_version_is_rejected() {
    let (td, events, policy) = fixture();
    for attack in 0..7 {
        let mut changed = events.clone();
        match attack {
            0 => changed[0].event_payload = hex(&[9; 32]),
            1 => changed[0].digest = hex(&[9; 48]),
            2 => changed.swap(0, 1),
            3 => changed.push(changed[0].clone()),
            4 => {
                changed.pop();
            }
            5 => changed[0].version = Some(2),
            _ => changed[0].event_type = 0,
        }
        assert!(appraise(&td, &changed, &policy.releases[0], &[5; 64]).is_err());
    }
}

#[test]
fn policy_rejects_wildcards_duplicates_zero_mrs_and_late_compose_claims() {
    let (_, _, policy) = fixture();
    for attack in 0..5 {
        let mut changed = policy.clone();
        let release = &mut changed.releases[0];
        match attack {
            0 => release.events[0].payload = None,
            1 => release.events.push(release.events[0].clone()),
            2 => release.mr_td = hex(&[0; 48]),
            3 => release.events.swap(0, 2),
            _ => {
                release.events.pop();
            }
        }
        assert!(changed.validate().is_err());
    }
}

#[tokio::test]
async fn empty_allowlist_fails_before_network_or_quote_parsing() {
    let verifier = TdxVerifier::new(Policy {
        schema_version: 1,
        allow_dynamic_platform: false,
        allow_cached_keys: false,
        allow_smt: false,
        releases: vec![],
    })
    .unwrap();
    let error = verifier
        .verify(&serde_json::json!({}), &[0; 64])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("允许表为空"));
    assert!(verifier.http.get("http://127.0.0.1/private").await.is_err());
    assert!(verifier.http.get("https://example.com/cert").await.is_err());
}

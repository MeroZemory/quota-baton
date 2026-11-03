//! Production lifecycle response -> startup adapter -> old prefix resolution.
use super::*;
use zeroize::Zeroizing;

async fn reply_bytes(source: &Fixture) -> Zeroizing<Vec<u8>> {
    let response = source
        .relay
        .takeover_reply(source.relay.bindings.export().unwrap(), true);
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), crate::relay::MAX_TAKEOVER_REPLY_BYTES)
        .await
        .unwrap();
    Zeroizing::new(bytes.to_vec())
}

#[tokio::test]
async fn launch_handover_old_prefix_without_c8_keeps_managed_account_model_and_policy() {
    let source = Fixture::new().await;
    warm_issue_catalog(&source, &source.id, &["common"]).await;
    let issued = source
        .relay
        .issue_launch(issue_request(Some("common")), 10200)
        .await
        .unwrap();
    let old = issued_handle(&issued);
    let old_pin = source.relay.launch_tokens.get_or_pin(old).unwrap();
    let anchor = old_pin.metadata().binding_anchor_key;
    drop(old_pin);
    let bytes = reply_bytes(&source).await;
    let mut successor = Fixture::new().await;
    Arc::get_mut(&mut successor.relay)
        .expect("unpublished fresh relay")
        .restore_takeover(&bytes)
        .unwrap();
    let pin = successor.relay.launch_tokens.get_or_pin(old).unwrap();
    assert_eq!(pin.metadata().binding_anchor_key, anchor);
    let ingress = LaunchIngress {
        pin: Some(Arc::new(pin)),
    };
    for _ in 0..2 {
        // Same resolver used by actual POST and WS upgrade, both with null C8.
        let info = successor
            .relay
            .resolve_data_launch(
                Some(&ingress),
                None,
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(info.managed);
        assert_eq!(info.account, issued.account);
        assert_eq!(info.current_model.as_deref(), Some("common"));
        assert_eq!(
            successor
                .relay
                .bindings
                .routing_plan(Some(&info.canonical_key))
                .settings,
            source.relay.bindings.routing_plan(Some(&anchor)).settings
        );
    }
    assert_eq!(
        source.upstream.state.requests().len(),
        1,
        "only original catalog fetch"
    );
    assert!(successor.upstream.state.requests().is_empty());
}

#[tokio::test]
async fn launch_handover_managed_failure_falls_back_without_partial_grants() {
    let source = Fixture::new().await;
    warm_issue_catalog(&source, &source.id, &["common"]).await;
    let issued = source
        .relay
        .issue_launch(issue_request(Some("common")), 10200)
        .await
        .unwrap();
    let pin = source
        .relay
        .launch_tokens
        .get_or_pin(issued_handle(&issued))
        .unwrap();
    source
        .relay
        .bindings
        .resolve_launch(
            Some(&pin.metadata().binding_anchor_key),
            Some("codex:12345678-1234-4234-8234-123456789abc"),
            chrono::Utc::now(),
        )
        .unwrap();
    drop(pin);
    let bytes = reply_bytes(&source).await;
    let text = Zeroizing::new(
        std::str::from_utf8(&bytes)
            .unwrap()
            .replace("\"managed\":{\"schema\":1", "\"managed\":{\"schema\":9"),
    );
    assert_ne!(text.as_bytes(), bytes.as_slice());
    let mut successor = Fixture::new().await;
    Arc::get_mut(&mut successor.relay)
        .expect("unpublished fresh relay")
        .restore_takeover(text.as_bytes())
        .unwrap();
    assert!(
        successor
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&issued))
            .is_err()
    );
    let exported = successor.relay.bindings.export().unwrap();
    assert!(exported.get("launches").is_none());
    assert!(exported.get("launch_anchors").is_none());
    assert!(exported.get("launch_aliases").is_none());
    assert!(!exported["bindings"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn launch_handover_rejects_oversized_before_parse_and_lease_false_before_mutation() {
    let mut successor = Fixture::new().await;
    let before = successor.relay.bindings.export().unwrap();
    let relay = Arc::get_mut(&mut successor.relay).expect("unpublished fresh relay");
    let oversized = Zeroizing::new(vec![b' '; crate::relay::MAX_TAKEOVER_REPLY_BYTES + 1]);
    assert!(relay.restore_takeover(&oversized).is_err());
    assert!(
        relay
            .restore_takeover(br#"{"bindings":[],"lease_released":false}"#)
            .is_err()
    );
    assert_eq!(relay.bindings.export().unwrap(), before);
}

#[tokio::test]
async fn launch_handover_source_grant_activity_precedes_binding_import_prune() {
    let source = Fixture::new().await;
    warm_issue_catalog(&source, &source.id, &["common"]).await;
    let issued = source
        .relay
        .issue_launch(issue_request(Some("common")), 10200)
        .await
        .unwrap();
    // A valid source grant can have newer activity than its last flushed table.
    // Keep real source grant identity; only the snapshot clock is deliberately old.
    let mut state = source.relay.bindings.export().unwrap();
    let old = chrono::Utc::now() - chrono::Duration::days(2);
    for row in state["bindings"].as_array_mut().unwrap() {
        row["last_used"] = json!(old);
    }
    for (_, row) in state["launch_anchors"].as_object_mut().unwrap() {
        row["last_used"] = json!(old);
    }
    let response = source.relay.takeover_reply(state, true);
    assert_eq!(response.status(), StatusCode::OK);
    let wire = axum::body::to_bytes(response.into_body(), crate::relay::MAX_TAKEOVER_REPLY_BYTES)
        .await
        .unwrap();
    let bytes = Zeroizing::new(wire.to_vec());
    let mut successor = Fixture::new().await;
    Arc::get_mut(&mut successor.relay)
        .expect("unpublished fresh relay")
        .restore_takeover(&bytes)
        .unwrap();
    let pin = successor
        .relay
        .launch_tokens
        .get_or_pin(issued_handle(&issued))
        .unwrap();
    let info = successor
        .relay
        .resolve_data_launch(
            Some(&LaunchIngress {
                pin: Some(Arc::new(pin)),
            }),
            None,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(info.managed);
    assert_eq!(info.account, issued.account);
    assert_eq!(info.current_model.as_deref(), Some("common"));
}

#[tokio::test]
async fn launch_handover_old_successor_receives_no_managed_grants() {
    let source = Fixture::new().await;
    warm_issue_catalog(&source, &source.id, &["common"]).await;
    let _issued = source
        .relay
        .issue_launch(issue_request(Some("common")), 10200)
        .await
        .unwrap();
    let response = source
        .relay
        .takeover_reply(source.relay.bindings.export().unwrap(), false);
    let bytes = axum::body::to_bytes(response.into_body(), crate::relay::MAX_TAKEOVER_REPLY_BYTES)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value.get("managed").is_none());
    assert!(value.get("bindings").is_some());
    assert_eq!(value["lease_released"], true);
}

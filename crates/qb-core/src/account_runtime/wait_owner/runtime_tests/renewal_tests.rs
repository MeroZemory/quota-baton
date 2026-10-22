//! Proposed child of wait_owner::runtime_tests, reusing its native Fixture.
//! No direct proof mint, receipt construction, injected WaitIntent or send.
use super::*;

async fn publish_renewal_poll(f: &Fixture, reset: DateTime<Utc>, complete: bool) {
    // Actual producer provenance is captured before report collection.
    let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
    let mut raw = Fixture::raw(reset);
    if !complete {
        raw["windows"].as_array_mut().unwrap().truncate(1);
    }
    let snapshot = f
        .runtime
        .obs
        .observe_poll(&f.id, raw.clone(), true)
        .unwrap();
    f.runtime
        .apply_quota_snapshot_reported_origin(&f.id, snapshot, &raw, None, Some(origin))
        .await
        .unwrap();
}

#[tokio::test]
async fn n5a_runtime_wait_renewal_repeated_full_poll_preserves_original_envelope() {
    let f = Fixture::new();
    assert_eq!(f.model, None, "ordinary unscoped HTTP request");
    let reset = f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    let mut receipt = f
        .admit(&operation)
        .expect("full account poll admits None model");
    let wake = receipt.wake();
    let deadline = receipt.deadline();
    let key = receipt.key().to_owned();
    assert_eq!(receipt.reset_at(), reset);
    assert_eq!(deadline, operation.deadline());
    assert!(wake < deadline);

    for replacement in 0..3 {
        // Separate genuine native receipt lets us prove replacement revokes
        // old ticket identity without cloning any opaque ticket or receipt.
        let old_checker = f.admit(&operation).unwrap();
        assert!(f.is_current(&operation, &receipt));
        publish_renewal_poll(&f, reset, true).await;
        assert!(
            f.has_current_proof(),
            "replacement {replacement} full proof"
        );
        assert!(!f.is_current(&operation, &old_checker));
        assert!(!f.is_current(&operation, &receipt));
        receipt = f
            .runtime
            .renew_current_http_quota_wait(&f.table, f.request(&operation), receipt)
            .unwrap()
            .expect("fresh full poll must renew through native gates");
        assert!(f.is_current(&operation, &receipt));
        assert!(!f.is_current(&operation, &old_checker));
        assert_eq!(receipt.account(), f.id);
        assert_eq!(receipt.key(), key);
        assert_eq!(receipt.model(), None);
        assert_eq!(receipt.reset_at(), reset);
        assert_eq!(receipt.wake(), wake, "replacement must not restart wait");
        assert_eq!(receipt.deadline(), deadline);
        assert_eq!(f.table.current(Some(KEY)).as_deref(), Some(f.id.as_str()));
        assert_eq!(f.table.pending_count(&f.id), 0);
    }
}

#[tokio::test]
async fn n5a_runtime_wait_renewal_partial_poll_revokes_and_cannot_authorize() {
    let f = Fixture::new();
    let reset = f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    let receipt = f.admit(&operation).unwrap();
    let old_checker = f.admit(&operation).unwrap();
    assert!(f.is_current(&operation, &receipt));

    // Newer causal partial report must advance provenance/revoke old proof,
    // even though its visible exhausted window still has the SAME reset.
    publish_renewal_poll(&f, reset, false).await;
    assert!(!f.has_current_proof());
    assert!(!f.is_current(&operation, &old_checker));
    assert!(!f.is_current(&operation, &receipt));
    assert!(
        f.runtime
            .renew_current_http_quota_wait(&f.table, f.request(&operation), receipt)
            .unwrap()
            .is_none(),
        "no grace interval or old-proof rescue after partial report"
    );
    assert!(f.admit(&operation).is_none());
    assert_eq!(f.table.current(Some(KEY)).as_deref(), Some(f.id.as_str()));
    assert_eq!(f.table.pending_count(&f.id), 0);
}

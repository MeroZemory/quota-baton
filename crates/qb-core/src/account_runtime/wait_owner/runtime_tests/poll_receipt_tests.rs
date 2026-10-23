//! Native Store/report units only; no C9 transport, WS or resume acceptance.
use super::*;
use crate::account_runtime::ObservedUsagePoll;

async fn observed(
    f: &Fixture,
    raw: Value,
    complete: bool,
    http_origin: bool,
) -> (Account, Option<ObservedUsagePoll>) {
    let origin = if http_origin {
        f.runtime.capture_http_origin(&f.id).await.unwrap()
    } else {
        f.runtime.capture_poll_origin(&f.id).await.unwrap()
    };
    let outside = f.runtime.capture_outside_poll_origin(&f.id);
    let snapshot = f
        .runtime
        .obs
        .observe_poll(&f.id, raw.clone(), complete)
        .unwrap();
    f.runtime
        .apply_polled_quota_snapshot_observed(&f.id, snapshot, &raw, Some(origin), outside)
        .await
        .unwrap()
}

fn current(f: &Fixture, receipt: &ObservedUsagePoll) -> bool {
    let _store = f.runtime.store.transaction().unwrap();
    f.runtime.wait_owner.proofs.admit(&receipt.native).is_ok()
}

#[tokio::test]
async fn n5d_ws_poll_receipt_exact_application_and_full_replacement() {
    let f = Fixture::new();
    let reset = Utc::now() + Duration::hours(1);
    let (account, first) = observed(&f, Fixture::raw(reset), true, false).await;
    assert!(matches!(
        account.health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    let first = first.expect("complete actual poll application yields original receipt");
    assert!(current(&f, &first));
    let (_, second) = observed(&f, Fixture::raw(reset), true, false).await;
    let second = second.expect("later full report has its own receipt");
    assert!(
        !current(&f, &first),
        "old receipt cannot borrow replacement"
    );
    assert!(current(&f, &second));
}

#[tokio::test]
async fn n5d_ws_poll_receipt_partial_merged_snapshot_cannot_borrow_long_window() {
    let f = Fixture::new();
    let reset = Utc::now() + Duration::hours(1);
    let (_, first) = observed(&f, Fixture::raw(reset), true, false).await;
    let first = first.unwrap();
    let mut partial = Fixture::raw(reset);
    partial["windows"].as_array_mut().unwrap().truncate(1);
    let (account, partial_receipt) = observed(&f, partial, false, false).await;
    assert!(matches!(
        account.health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    assert!(partial_receipt.is_none());
    assert!(!current(&f, &first));
    assert!(
        !f.has_current_proof(),
        "merged long window grants no raw authority"
    );
}

#[tokio::test]
async fn n5d_ws_poll_receipt_http_origin_cannot_masquerade_as_poll() {
    let f = Fixture::new();
    let reset = Utc::now() + Duration::hours(1);
    let (account, receipt) = observed(&f, Fixture::raw(reset), true, true).await;
    assert!(matches!(
        account.health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    assert!(receipt.is_none());
    assert!(!f.has_current_proof());
}

#[tokio::test]
async fn n5d_ws_poll_receipt_healthy_or_weekly_only_never_qualifies() {
    for weekly_only in [false, true] {
        let f = Fixture::new();
        let reset = Utc::now() + Duration::hours(1);
        let mut raw = Fixture::raw(reset);
        raw["windows"][0]["used_pct"] = json!(20.0);
        if weekly_only {
            raw["windows"][1]["used_pct"] = json!(100.0);
        }
        let (_, receipt) = observed(&f, raw, true, false).await;
        assert!(receipt.is_none());
        assert!(!f.has_current_proof());
    }
}

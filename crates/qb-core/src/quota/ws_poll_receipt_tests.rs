// Synthetic C9 GET coverage for the poll_ws_quota API.
// Receipt presence only: no native ticket access, registration, WS or resume.
use super::*;

fn body(primary: u64, secondary: u64, reset: i64) -> serde_json::Value {
    json!({"rate_limit": {
        "primary_window": {"used_percent": primary, "limit_window_seconds": 18000,
            "reset_at": reset},
        "secondary_window": {"used_percent": secondary, "limit_window_seconds": 604800,
            "reset_at": reset + 86400}
    }})
}

fn enqueue(f: &Fixture, body: serde_json::Value) {
    f.fake.state.push(ScriptedResponse::json(
        reqwest::StatusCode::OK,
        body.to_string(),
    ));
}

fn assert_gets(f: &Fixture, count: usize) {
    let base = reqwest::Url::parse(&f.fake.url).unwrap();
    assert_eq!(base.scheme(), "http");
    assert_eq!(base.host_str(), Some("127.0.0.1"));
    assert!(base.port().is_some());
    assert_eq!(f.relay.quota.bases[1], f.fake.url);
    let requests = f.fake.state.requests();
    assert_eq!(requests.len(), count);
    for request in requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path_and_query, "/backend-api/wham/usage");
        assert!(request.body.is_empty());
    }
}

#[tokio::test]
async fn c9_ws_complete_exhausted_5h_receipt_once_and_too_soon_sends_no_get() {
    let f = Fixture::new(1).await;
    let id = &f.ids[0];
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    enqueue(&f, body(100, 20, reset));
    let (account, receipt) = f.relay.quota.poll_ws_quota(id).await.unwrap();
    assert!(matches!(account.health,
        Health::Cooling { reason: HealthReason::Quota, until, .. }
        if until.timestamp() == reset));
    assert!(receipt.is_some());
    drop(receipt);
    assert_gets(&f, 1);

    // Leave a valid response queued so an accidental second send is observable.
    enqueue(&f, body(100, 20, reset));
    assert!(matches!(
        f.relay.quota.poll_ws_quota(id).await,
        Err(PollError::TooSoon { .. })
    ));
    assert_gets(&f, 1);
}

#[tokio::test]
async fn c9_ws_primary_only_cannot_borrow_cached_weekly_window() {
    let f = Fixture::new(1).await;
    let id = &f.ids[0];
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    // Seed the other window through an actual ordinary production usage GET.
    enqueue(&f, body(20, 30, reset));
    f.relay.quota.poll(id).await.unwrap();
    assert_gets(&f, 1);
    let cached = f.relay.quota.obs.quota_snapshot(id).unwrap();
    let weekly = cached["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "7d")
        .unwrap()
        .clone();

    // Advance only Tokio time past the capped 3600s poll interval. Resume before
    // doing socket IO; no product clock mutation or wall-clock reset change.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3601)).await;
    tokio::time::resume();
    let mut partial = body(100, 30, reset);
    partial["rate_limit"]
        .as_object_mut()
        .unwrap()
        .remove("secondary_window");
    enqueue(&f, partial);
    let (account, receipt) = f.relay.quota.poll_ws_quota(id).await.unwrap();
    assert!(matches!(account.health,
        Health::Cooling { reason: HealthReason::Quota, until, .. }
        if until.timestamp() == reset));
    assert!(receipt.is_none());
    let merged = f.relay.quota.obs.quota_snapshot(id).unwrap();
    let windows = merged["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2);
    assert!(
        windows
            .iter()
            .any(|w| w["name"] == "5h" && w["used_pct"] == 100.0)
    );
    assert!(windows.iter().any(|w| w["name"] == "7d"
        && w["used_pct"] == weekly["used_pct"]
        && w["reset_at"] == weekly["reset_at"]));
    assert_gets(&f, 2);
}

#[tokio::test]
async fn c9_ws_complete_healthy_or_weekly_only_exhausted_has_no_receipt() {
    for (primary, secondary) in [(20, 30), (20, 100)] {
        let f = Fixture::new(1).await;
        let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
        enqueue(&f, body(primary, secondary, reset));
        let (_, receipt) = f.relay.quota.poll_ws_quota(&f.ids[0]).await.unwrap();
        assert!(
            receipt.is_none(),
            "primary={primary}, secondary={secondary}"
        );
        assert_gets(&f, 1);
    }
}

#[tokio::test]
async fn c9_ws_malformed_unrecognized_or_failed_response_preserves_error() {
    for (status, response) in [
        (reqwest::StatusCode::OK, "{"),
        (reqwest::StatusCode::OK, r#"{"opaque":true}"#),
        (
            reqwest::StatusCode::BAD_GATEWAY,
            "synthetic upstream failure",
        ),
    ] {
        let f = Fixture::new(1).await;
        f.fake.state.push(ScriptedResponse::json(status, response));
        assert!(matches!(
            f.relay.quota.poll_ws_quota(&f.ids[0]).await,
            Err(PollError::Unavailable)
        ));
        assert_gets(&f, 1);
    }
}

#[tokio::test]
async fn c9_ws_cancelled_inflight_poll_keeps_budget_and_publishes_no_receipt() {
    let f = Fixture::new(1).await;
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    let mut delayed =
        ScriptedResponse::json(reqwest::StatusCode::OK, body(100, 20, reset).to_string());
    delayed.chunks[0].0 = Duration::from_secs(30);
    f.fake.state.push(delayed);
    let poller = f.relay.quota.clone();
    let id = f.ids[0].clone();
    let task = tokio::spawn(async move { poller.poll_ws_quota(&id).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("synthetic usage GET starts before cancellation");
    task.abort();
    assert!(task.await.is_err_and(|error| error.is_cancelled()));
    assert!(f.relay.quota.obs.quota_snapshot(&f.ids[0]).is_none());
    assert!(matches!(f.account(0).health, Health::Ready));
    assert!(matches!(
        f.relay.quota.poll_ws_quota(&f.ids[0]).await,
        Err(PollError::TooSoon { .. })
    ));
    assert_gets(&f, 1);
}

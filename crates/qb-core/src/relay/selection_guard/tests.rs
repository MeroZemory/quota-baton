use super::*;
use std::{sync::Arc, time::Duration};

use crate::{
    accounts::{HealthReason, Provider, UserState},
    bindings::{BindingTable, Decision},
    config::RoutingConfig,
};
use serde_json::json;

const KEY: &str = "codex:r14-held";

fn fixture() -> (tempfile::TempDir, Arc<BindingTable>, Vec<Candidate>) {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.sandbox/r14-selection-guard-tests");
    std::fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(base).unwrap();
    let mut config = RoutingConfig::default();
    config.move_guard.global_ten_minutes = 1;
    let table = BindingTable::new(home.path(), config).unwrap();
    let candidates = ["cx-111111", "cx-222222", "cx-333333"]
        .into_iter()
        .map(|id| {
            let mut account = crate::accounts::health::tests::fixture();
            account.id = id.into();
            Candidate {
                account,
                quota: None,
                inflight: 0,
            }
        })
        .collect();
    (home, table, candidates)
}

fn choose(
    table: &Arc<BindingTable>,
    key: &str,
    candidates: &[Candidate],
    pin: &str,
    now: DateTime<Utc>,
    admission: impl FnOnce(&str) -> bool,
) -> Decision {
    table.choose_with_hold_at_admitted(
        Provider::Codex,
        Some(key),
        None,
        None,
        candidates,
        Some(pin),
        100,
        false,
        false,
        now,
        admission,
    )
}

fn selected(choice: Decision) -> String {
    match choice {
        Decision::Selected { account, .. } => account,
        other => panic!("expected selection: {other:?}"),
    }
}

fn prime_global(table: &Arc<BindingTable>, cs: &[Candidate], now: DateTime<Utc>) {
    selected(choose(
        table,
        "codex:r14-other",
        cs,
        &cs[0].account.id,
        now,
        |_| true,
    ));
    selected(choose(
        table,
        "codex:r14-other",
        cs,
        &cs[1].account.id,
        now,
        |_| true,
    ));
    selected(choose(table, KEY, cs, &cs[1].account.id, now, |_| true));
}

fn forced_hold(table: &Arc<BindingTable>, cs: &[Candidate], now: DateTime<Utc>) -> DateTime<Utc> {
    match choose(table, KEY, cs, &cs[2].account.id, now, |_| {
        panic!("a refused move must not call admission")
    }) {
        Decision::MoveHeld { until } => until,
        other => panic!("expected forced hold: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn r14_move_hold_account_recovery_between_choice_and_wait_is_not_lost() {
    let (_home, table, mut cs) = fixture();
    let now = Utc::now();
    prime_global(&table, &cs, now);
    cs[1].account.user_state = UserState::Paused;
    let (events, _) = broadcast::channel(8);
    let mut changes = Changes::new(&table.policies.changed, events.subscribe());
    let until = forced_hold(&table, &cs, now);

    // Recovery is published after the real refusal, before wait is first polled.
    cs[1].account.user_state = UserState::Active;
    events
        .send(json!({"event":"account_changed","data":{"id":cs[1].account.id}}))
        .unwrap();
    let start = Instant::now();
    assert!(
        !changes
            .wait(
                Some(&cs[1].account.id),
                start + Duration::from_secs(600),
                start + Duration::from_secs(300),
                std::future::pending()
            )
            .await
    );
    assert_eq!(Instant::now(), start);
    assert_eq!(
        until,
        now + chrono::Duration::seconds(
            crate::bindings::MoveGuardLimits::default().trip_seconds as i64
        )
    );
    assert_eq!(
        selected(choose(&table, KEY, &cs, &cs[1].account.id, now, |_| true)),
        cs[1].account.id
    );
    assert!(
        table.move_guard_until().is_some(),
        "same-account recovery needs no clear"
    );
}

#[tokio::test(start_paused = true)]
async fn r14_move_hold_clear_between_choice_and_wait_is_not_lost() {
    let (_home, table, mut cs) = fixture();
    let now = Utc::now();
    prime_global(&table, &cs, now);
    cs[1].account.user_state = UserState::Paused;
    let (events, _) = broadcast::channel(8);
    let mut changes = Changes::new(&table.policies.changed, events.subscribe());
    forced_hold(&table, &cs, now);

    table.clear_move_guard(); // no waiter is being polled yet
    let start = Instant::now();
    assert!(
        !changes
            .wait(
                Some(&cs[1].account.id),
                start + Duration::from_secs(600),
                start + Duration::from_secs(300),
                std::future::pending()
            )
            .await
    );
    assert_eq!(Instant::now(), start);
    assert_eq!(
        selected(choose(&table, KEY, &cs, &cs[2].account.id, now, |_| true)),
        cs[2].account.id
    );
}

#[tokio::test(start_paused = true)]
async fn r14_move_hold_rate_cooling_expiry_reselects_before_breaker_timeout() {
    let (_home, table, mut cs) = fixture();
    let now = Utc::now();
    prime_global(&table, &cs, now);
    let reset = now + chrono::Duration::seconds(10);
    cs[1].account.health = Health::Cooling {
        reason: HealthReason::Rate,
        until: reset,
        scope: "account".into(),
    };
    let (events, _) = broadcast::channel(8);
    let mut changes = Changes::new(&table.policies.changed, events.subscribe());
    let guard_until = forced_hold(&table, &cs, now);
    let until = recovery_deadline(guard_until, &cs, Some(&cs[1].account.id), now);
    assert_eq!(until, reset);
    let start = Instant::now();
    assert!(
        !changes
            .wait(
                Some(&cs[1].account.id),
                start + (until - now).to_std().unwrap(),
                start + Duration::from_secs(300),
                std::future::pending()
            )
            .await
    );
    assert_eq!(Instant::now() - start, Duration::from_secs(10));
    assert_eq!(
        selected(choose(&table, KEY, &cs, &cs[1].account.id, reset, |_| true)),
        cs[1].account.id
    );
}

#[test]
fn r14_move_hold_recovers_on_payment_and_observed_quota_without_expired_timer_loops() {
    let (_home, table, mut cs) = fixture();
    let now = Utc::now();
    prime_global(&table, &cs, now);
    cs[0].account.payment_until = Some(now + chrono::Duration::seconds(1));
    cs[1].account.payment_until = Some(now + chrono::Duration::seconds(20));
    cs[1].account.quota_cooling = Some(crate::accounts::QuotaCooling {
        window: "5h".into(),
        reset_at: now + chrono::Duration::seconds(15),
        started_at: now,
    });
    cs[1].quota = Some(json!({"windows":[
        {"used_pct":100,"reset_at":now + chrono::Duration::seconds(10)},
        {"used_pct":99,"reset_at":now + chrono::Duration::seconds(2)}
    ]}));
    let guard_until = match table.choose_with_hold_at_admitted_evidence(
        Provider::Codex,
        Some(KEY),
        None,
        None,
        &cs[2..], // The held source is evidence, never an eligible retry target.
        &cs,
        crate::bindings::MoveObservations::untracked(),
        Some(&cs[2].account.id),
        100,
        false,
        false,
        now,
        None,
        |_| panic!("a global refusal must not admit the target"),
    ) {
        Decision::MoveHeld { until } => until,
        other => panic!("expected forced hold: {other:?}"),
    };
    let source = Some(cs[1].account.id.as_str());
    assert_eq!(
        recovery_deadline(guard_until, &cs, source, now),
        now + chrono::Duration::seconds(10)
    );
    assert_eq!(
        recovery_deadline(
            guard_until,
            &cs,
            source,
            now + chrono::Duration::seconds(10)
        ),
        now + chrono::Duration::seconds(15)
    );
    assert_eq!(
        recovery_deadline(
            guard_until,
            &cs,
            source,
            now + chrono::Duration::seconds(20)
        ),
        guard_until
    );
}

#[tokio::test(start_paused = true)]
async fn r14_move_hold_original_deadline_wins_queued_recovery() {
    let policy = Notify::new();
    let (events, _) = broadcast::channel(8);
    let mut changes = Changes::new(&policy, events.subscribe());
    let start = Instant::now();
    events
        .send(json!({"event":"account_changed","data":{"id":"cx-222222"}}))
        .unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(
        changes
            .wait(
                Some("cx-222222"),
                start + Duration::from_secs(600),
                start + Duration::from_secs(10),
                std::future::pending()
            )
            .await
    );
    assert_eq!(Instant::now() - start, Duration::from_secs(10));
}

async fn abandoned_selection(cancel: bool) {
    let (_home, table, cs) = fixture();
    let now = Utc::now();
    selected(choose(&table, KEY, &cs, &cs[0].account.id, now, |_| true));
    let operation = IssueOperation::new(Instant::now() + Duration::from_secs(10));
    let worker_operation = operation.clone();
    let worker_table = table.clone();
    let worker_cs = cs.clone();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (finished_tx, finished) = tokio::sync::oneshot::channel();
    let mut selection = Box::pin(async move {
        let _caller = operation.caller_guard();
        tokio::task::spawn_blocking(move || {
            let choice = choose(
                &worker_table,
                KEY,
                &worker_cs,
                &worker_cs[1].account.id,
                now,
                |_| {
                    admit(&worker_operation, || {
                        // The real chooser holds mutations here. Stop after initial
                        // checks but before caller/deadline commit admission.
                        entered_tx.send(()).unwrap();
                        released.recv().unwrap();
                        true
                    })
                },
            );
            finished_tx.send(matches!(choice, Decision::None)).unwrap();
        })
        .await
        .unwrap();
    });
    assert!(futures_util::poll!(selection.as_mut()).is_pending());
    entered.await.unwrap();
    if cancel {
        drop(selection); // drops the actual async owner, leaving its worker alive
    } else {
        tokio::time::advance(Duration::from_secs(10)).await;
        release.send(()).unwrap();
        selection.await;
        assert!(finished.await.unwrap());
        assert_eq!(
            table.current(Some(KEY)).as_deref(),
            Some(cs[0].account.id.as_str())
        );
        assert_eq!(
            selected(choose(&table, KEY, &cs, &cs[1].account.id, now, |_| true)),
            cs[1].account.id
        );
        return;
    }
    release.send(()).unwrap();
    assert!(finished.await.unwrap());
    assert_eq!(
        table.current(Some(KEY)).as_deref(),
        Some(cs[0].account.id.as_str())
    );
    // The rejected prepared move must not consume either damage allowance.
    assert_eq!(
        selected(choose(&table, KEY, &cs, &cs[1].account.id, now, |_| true)),
        cs[1].account.id
    );
}

#[tokio::test(start_paused = true)]
async fn r14_ordinary_selection_caller_drop_during_admission_never_commits() {
    abandoned_selection(true).await;
}

#[tokio::test(start_paused = true)]
async fn r14_ordinary_selection_deadline_during_admission_never_commits() {
    abandoned_selection(false).await;
}

#[tokio::test(start_paused = true)]
async fn r141_move_hold_trip_expiry_without_notifications_reselects_at_240_before_300() {
    let (_home, table, mut cs) = fixture();
    let now = Utc::now();
    prime_global(&table, &cs, now);
    cs[1].account.user_state = UserState::Paused;
    // Subscribe after setup. Keep the sender alive but never publish an event;
    // no clear/recovery/drain notification can supply the wake in this test.
    let (events, _) = broadcast::channel(8);
    let mut changes = Changes::new(&table.policies.changed, events.subscribe());
    let guard_until = forced_hold(&table, &cs, now);
    assert_eq!(guard_until - now, chrono::Duration::seconds(240));
    let until = recovery_deadline(guard_until, &cs, Some(&cs[1].account.id), now);
    assert_eq!(
        until, guard_until,
        "no earlier account recovery is available"
    );
    assert_eq!(table.current(Some(KEY)).as_ref(), Some(&cs[1].account.id));
    let start = Instant::now();
    let deadline = start + Duration::from_secs(300);
    {
        let wait = changes.wait(
            Some(&cs[1].account.id),
            start + (until - now).to_std().unwrap(),
            deadline,
            std::future::pending(),
        );
        tokio::pin!(wait);
        assert!(futures_util::poll!(wait.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(239)).await;
        assert!(futures_util::poll!(wait.as_mut()).is_pending());
        assert_eq!(table.current(Some(KEY)).as_ref(), Some(&cs[1].account.id));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            !wait.await,
            "trip expiry must reselect, not end the request hold"
        );
    }
    assert_eq!(Instant::now() - start, Duration::from_secs(240));
    assert!(Instant::now() < deadline);
    // The breaker uses UTC and observation chronology uses std::Instant;
    // paused Tokio does not advance either. Supply the explicitly corresponding
    // UTC selection time instead of restamping evidence or granting an exception.
    let after = now + chrono::Duration::seconds(240);
    assert_eq!(
        selected(choose(&table, KEY, &cs, &cs[2].account.id, after, |_| true)),
        cs[2].account.id
    );
    assert_eq!(table.current(Some(KEY)).as_ref(), Some(&cs[2].account.id));
    assert!(table.move_guard_until().is_none());
    assert!(matches!(
        changes.accounts.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

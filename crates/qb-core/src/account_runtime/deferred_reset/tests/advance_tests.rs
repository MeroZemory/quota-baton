use super::*;
use std::sync::Barrier;

fn advance(f: &Fixture, scope: &LaunchOwnerScope, id: DeferredResetId) -> bool {
    f.runtime
        .advance_deferred_reset(&f.table, scope, id, &Fixture::operation())
        .unwrap()
}

fn try_register(
    f: &Fixture,
    observed: &ObservedHttp429,
) -> Option<(DeferredResetId, LaunchOwnerScope)> {
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    f.runtime
        .register_http_deferred_reset(&f.table, f.request(&operation), observed, &f.pin)
        .unwrap()
}

async fn consume_physical(f: &Fixture) -> (DeferredResetId, LaunchOwnerScope, ObservedHttp429) {
    let (observed, reset) = f.physical(3).await;
    let (id, scope) = f.register(&observed);
    let DeferredResetStatus::Waiting { wake, .. } = f.runtime.deferred_reset_status(&scope, THREAD)
    else {
        panic!("missing physical episode");
    };
    wait_until_original_reset(wake, reset).await;
    assert!(Utc::now() >= reset);
    assert!(
        f.runtime
            .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
            .unwrap()
            .is_some()
    );
    (id, scope, observed)
}

#[tokio::test]
async fn n5d_advance_refuses_missing_waiting_ready_and_cancelled_episodes() {
    let f = Fixture::new();
    let scope = f.lease.scope();
    assert!(!advance(&f, &scope, DeferredResetId(1)));
    let (observed, reset) = f.physical(3).await;
    let (id, scope) = f.register(&observed);
    assert!(!advance(&f, &scope, id));
    assert!(!advance(&f, &scope, DeferredResetId(0)));
    assert!(!advance(&f, &scope, DeferredResetId(id.0 + 1)));
    let DeferredResetStatus::Waiting { wake, .. } = f.runtime.deferred_reset_status(&scope, THREAD)
    else {
        panic!("advance must not alter the waiting episode");
    };
    wait_until_original_reset(wake, reset).await;
    assert!(Utc::now() >= reset);
    assert!(
        f.runtime
            .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
            .unwrap()
    );
    assert!(!advance(&f, &scope, id));
    assert!(f.runtime.cancel_deferred_reset(&scope, id));
    assert!(!advance(&f, &scope, id));
    assert_eq!(
        f.runtime.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Cancelled
    );
}

#[tokio::test]
async fn n5d_advance_survives_reclamation_but_cannot_reuse_old_source_or_id() {
    let f = Fixture::new();
    let (id, scope, old_source) = consume_physical(&f).await;
    let (before_advance, _) = f.physical(120).await;
    // A genuine new source can be retained, but is not current until Advance.
    // This uses production !live reclamation, removing the consumed record.
    let (next, next_scope) = try_register(&f, &before_advance).unwrap();
    assert!(next.0 > id.0 && next_scope.same_owner(&scope));
    assert_eq!(scope.episode_id(), Ok(Some(id.0)));
    assert!(try_register(&f, &before_advance).is_none());
    assert!(f.runtime.wait_owner.deferred.record(&scope, next).is_none());
    assert!(
        !f.runtime
            .inspect_deferred_reset(&f.table, &scope, next, &Fixture::operation())
            .unwrap()
    );
    assert!(
        f.runtime
            .consume_deferred_reset(&f.table, &scope, next, &Fixture::operation())
            .unwrap()
            .is_none()
    );
    assert!(
        !f.runtime
            .wait_owner
            .deferred
            .state
            .lock()
            .unwrap()
            .records
            .contains_key(&id.0)
    );
    let foreign = Fixture::new();
    assert!(!advance(&foreign, &scope, id));
    assert!(!advance(&f, &foreign.lease.scope(), id));
    assert!(!advance(&f, &scope, next));
    assert!(try_register(&f, &old_source).is_none());
    assert!(advance(&f, &scope, id));
    assert!(!advance(&f, &scope, id));
    assert_eq!(scope.root(), Some(ROOT));
    assert!(scope.matches_pin(&f.pin));
    assert!(matches!(f.runtime.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Waiting { id: current, .. } if current == next));
    assert_eq!(
        f.runtime
            .deferred_reset_status(&scope, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"),
        DeferredResetStatus::Cancelled
    );
    assert!(try_register(&f, &old_source).is_none());
    // Neither the original nor the pending proof can spend another slot.
    assert!(try_register(&f, &before_advance).is_none());
    assert!(!advance(&f, &scope, id));
    assert!(!advance(&f, &scope, next));
    assert!(!f.runtime.cancel_deferred_reset(&scope, id));
    assert!(
        !f.runtime
            .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
            .unwrap()
    );
    assert!(
        f.runtime
            .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
            .unwrap()
            .is_none()
    );
    assert!(matches!(f.runtime.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Waiting { id: current, .. } if current == next));
}

#[tokio::test]
async fn n5d_concurrent_advance_and_registration_each_have_one_winner() {
    let f = Fixture::new();
    let (id, scope, _) = consume_physical(&f).await;
    let barrier = Barrier::new(2);
    let wins = std::thread::scope(|threads| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                threads.spawn(|| {
                    barrier.wait();
                    advance(&f, &scope, id)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|advanced| *advanced)
            .count()
    });
    assert_eq!(wins, 1);
    let (fresh_source, _) = f.physical(120).await;
    let registered = std::thread::scope(|threads| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                threads.spawn(|| {
                    barrier.wait();
                    try_register(&f, &fresh_source)
                })
            })
            .collect();
        workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(registered.len(), 1);
    assert!(registered[0].0.as_u64() > id.0 && registered[0].1.same_owner(&scope));
    assert!(!advance(&f, &scope, id));
}

#[tokio::test]
async fn n5d_new_source_registration_racing_advance_is_retained_in_either_order() {
    let f = Fixture::new();
    let (id, scope, _) = consume_physical(&f).await;
    let (fresh_source, _) = f.physical(120).await;
    let barrier = Barrier::new(2);
    let (advanced, registered) = std::thread::scope(|threads| {
        let advance = threads.spawn(|| {
            barrier.wait();
            advance(&f, &scope, id)
        });
        let register = threads.spawn(|| {
            barrier.wait();
            try_register(&f, &fresh_source)
        });
        (advance.join().unwrap(), register.join().unwrap())
    });
    assert!(advanced);
    let (next, next_scope) = registered.unwrap();
    assert!(next.0 > id.0 && next_scope.same_owner(&scope));
    assert!(matches!(f.runtime.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Waiting { id: current, .. } if current == next));
}

#[tokio::test]
async fn n5d_pending_registration_revalidates_cancellation_policy_and_account() {
    for change in 0..3 {
        let f = Fixture::new();
        let (id, scope, _) = consume_physical(&f).await;
        let (fresh_source, _) = f.physical(120).await;
        let (pending, _) = try_register(&f, &fresh_source).unwrap();
        let operation = Fixture::operation();
        drop(operation.caller_guard());
        assert!(
            !f.runtime
                .advance_deferred_reset(&f.table, &scope, id, &operation)
                .unwrap()
        );
        assert_eq!(scope.episode_id(), Ok(Some(id.0)));
        match change {
            0 => assert!(f.runtime.cancel_deferred_reset(&scope, pending)),
            1 => {
                f.table
                    .set_routing_session_urgent(&session_id(ROOT), On5hLimit::Move)
                    .unwrap();
                f.table
                    .set_routing_session_urgent(&session_id(ROOT), On5hLimit::Wait)
                    .unwrap();
            }
            _ => {
                f.runtime
                    .store
                    .update_metadata(&f.id, |account| {
                        account.user_state = crate::accounts::UserState::Paused
                    })
                    .unwrap();
            }
        }
        assert!(!advance(&f, &scope, id));
        assert_eq!(scope.episode_id(), Ok(Some(id.0)));
        assert!(
            f.runtime
                .wait_owner
                .deferred
                .record(&scope, pending)
                .is_none()
        );
    }
}

#[tokio::test]
async fn n5d_advanced_initial_fallback_also_requires_a_fresh_original_proof() {
    let f = Fixture::new();
    let (id, scope, _) = consume_physical(&f).await;
    let reset = Utc::now() + Duration::seconds(120);
    f.poll(reset, true).await;
    let old_operation = Fixture::initial_operation();
    let old_fallback = f.fallback(&old_operation);
    let (next, next_scope) = f
        .runtime
        .register_initial_deferred_reset(&f.table, f.request(&old_operation), &old_fallback, &f.pin)
        .unwrap()
        .unwrap();
    assert!(next.0 > id.0 && next_scope.same_owner(&scope));
    assert_eq!(scope.episode_id(), Ok(Some(id.0)));
    assert!(advance(&f, &scope, id));
    assert!(
        f.runtime
            .register_initial_deferred_reset(
                &f.table,
                f.request(&old_operation),
                &old_fallback,
                &f.pin,
            )
            .unwrap()
            .is_none()
    );
    assert!(matches!(f.runtime.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Waiting { id: current, .. } if current == next));
}

#[tokio::test]
async fn n5d_consumed_receipt_cannot_advance_after_owner_or_runtime_close() {
    for close_runtime in [false, true] {
        let f = Fixture::new();
        let (id, scope, _) = consume_physical(&f).await;
        let runtime = f.runtime.clone();
        let table = f.table.clone();
        if close_runtime {
            f.runtime.wait_owner.deferred.close();
        } else {
            drop(f.lease);
        }
        assert!(
            !runtime
                .advance_deferred_reset(&table, &scope, id, &Fixture::operation())
                .unwrap()
        );
    }
}

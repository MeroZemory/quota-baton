//! Child of wait_owner::runtime_tests; native Store/runtime paths, no new hooks.
use super::*;

async fn full_save_before_wait_inspection(renewal: bool, reject_save: bool) {
    let f = Arc::new(Fixture::new());
    let reset = f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    let checker = f.admit(&operation).unwrap();
    let consumed = if renewal {
        Some(f.admit(&operation).unwrap())
    } else {
        None
    };
    let before = f.account();
    let before_tokens = f.runtime.store.tokens(&before).unwrap();
    let replacement = Tokens {
        access_token: "synthetic-full-save-replacement-access".into(),
        refresh_token: "synthetic-full-save-replacement-refresh".into(),
        id_token: None,
    };
    assert!(f.is_current(&operation, &checker));
    let changed = f.runtime.wait_owner.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();

    // This guard establishes the ordering, not thread start or mutex FIFO.
    // The runtime API takes Store before released/probes/volatile/binding/
    // publication/owner gates, so it cannot inspect until this guard is dropped.
    let gate = f.runtime.store.transaction().unwrap();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let worker = {
        let f = f.clone();
        let operation = operation.clone();
        std::thread::spawn(move || {
            // Owned Arc retains the synthetic home even on receiver timeout.
            // No scoped-thread join runs during panic unwinding.
            if started_tx.send(()).is_err() {
                return;
            }
            let request = f.request(&operation);
            let result = match consumed {
                Some(receipt) => f
                    .runtime
                    .renew_current_http_quota_wait(&f.table, request, receipt),
                None => f.runtime.current_http_quota_wait(&f.table, request),
            };
            // Drop any opaque result after API gates, before the bounded send.
            // Capacity one and a single sender means this cannot wait for recv.
            let _ = finished_tx.send(result.map(|receipt| receipt.is_some()));
        })
    };
    started_rx
        .recv_timeout(SAFETY_TIMEOUT)
        .expect("owned wait-inspection worker must start");
    assert!(matches!(
        finished_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    // This notification must be from this save, not previous proof publication.
    assert!(changed.as_mut().now_or_never().is_none());

    let expected = before.generation + u64::from(reject_save);
    let saved = f
        .runtime
        .store
        .save_unlocked(&before, &replacement, Some(expected));
    if reject_save {
        assert!(matches!(
            saved,
            Err(crate::accounts::StoreError::GenerationConflict {
                expected: Some(wanted), actual: Some(actual),
            }) if wanted == expected && actual == before.generation
        ));
    } else {
        assert_eq!(saved.unwrap().generation, before.generation + 1);
    }
    assert!(
        changed.as_mut().now_or_never().is_some(),
        "native FullSave prehook must notify even when save is rejected"
    );
    let current = f.runtime.store.read_account_unlocked(&f.id).unwrap();
    assert!(
        f.runtime
            .wait_owner
            .acquire_current_unlocked(&current, None, Utc::now())
            .is_none(),
        "full save must revoke authority before releasing Store"
    );
    let current_tokens = f.runtime.store.tokens_unlocked(&current).unwrap();
    if reject_save {
        assert_eq!(current, before, "failed CAS must preserve native metadata");
        assert!(current_tokens.access_token == before_tokens.access_token);
        assert!(current_tokens.refresh_token == before_tokens.refresh_token);
    } else {
        let mut expected_account = before.clone();
        expected_account.generation += 1;
        assert_eq!(current, expected_account);
        assert!(current_tokens.access_token == replacement.access_token);
        assert!(current_tokens.refresh_token == replacement.refresh_token);
    }
    assert!(matches!(current.health, Health::Cooling {
        reason: HealthReason::Quota, until, ..
    } if until == reset));
    assert!(!current_tokens.access_token.is_empty());
    drop(gate);

    let admitted = finished_rx
        .recv_timeout(SAFETY_TIMEOUT)
        .expect("native wait inspection must finish after Store release")
        .expect("native runtime API remains available");
    // The timed result proves the API returned. Never unconditionally join an
    // owned thread: on timeout, dropping its handle detaches instead of hanging.
    drop(worker);
    assert!(!admitted, "queued initial/consuming renewal must refuse");
    assert!(operation.check().is_ok());
    assert!(Utc::now() < reset, "refusal must precede original reset");
    assert!(!f.is_current(&operation, &checker));
    assert!(!f.has_current_proof());
    assert!(f.admit(&operation).is_none());

    // Independent NEW authority: capture only after save and queued refusal.
    // Keep the original reset; neither valid replacement tokens nor unchanged
    // saved Cooling suffice without this actual complete reported poll.
    let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
    let raw = Fixture::raw(reset);
    let snapshot = f
        .runtime
        .obs
        .observe_poll(&f.id, raw.clone(), true)
        .unwrap();
    f.runtime
        .apply_quota_snapshot_reported_origin(&f.id, snapshot, &raw, None, Some(origin))
        .await
        .unwrap();
    assert!(f.has_current_proof());
    let fresh = f.admit(&operation).expect("new full poll may admit anew");
    assert_eq!(fresh.account(), f.id);
    assert_eq!(fresh.reset_at(), reset);
    assert!(f.is_current(&operation, &fresh));
    assert!(
        !f.is_current(&operation, &checker),
        "old ticket stays revoked"
    );
    assert_eq!(f.table.current(Some(KEY)).as_deref(), Some(f.id.as_str()));
    assert_eq!(f.table.pending_count(&f.id), 0);
}

#[tokio::test]
async fn n5a_runtime_full_save_before_queued_initial_admission_revokes_even_on_rejection() {
    for reject_save in [false, true] {
        full_save_before_wait_inspection(false, reject_save).await;
    }
}

#[tokio::test]
async fn n5a_runtime_full_save_before_queued_consuming_renewal_revokes_even_on_rejection() {
    for reject_save in [false, true] {
        full_save_before_wait_inspection(true, reject_save).await;
    }
}

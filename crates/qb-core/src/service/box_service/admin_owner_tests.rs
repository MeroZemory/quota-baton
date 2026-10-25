//! Publication, queue and real grant-watcher ownership regressions.
use super::*;
use crate::admin::boxes::prepare_add_response;

fn fixture() -> (
    tempfile::TempDir,
    BoxRegistry,
    BoxScope,
    zeroize::Zeroizing<String>,
) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/tests/box-owner-publication");
    std::fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(base).unwrap();
    let registry = BoxRegistry::open(home.path()).unwrap();
    let (scope, key) = registry
        .add(BoxName::parse("synthetic-box").unwrap())
        .unwrap()
        .into_parts();
    (home, registry, scope, key)
}

fn handle(registry: BoxRegistry) -> (AdminHandle, mpsc::Receiver<Command>) {
    let (sender, receiver) = mpsc::channel(QUEUE);
    (
        AdminHandle {
            sender,
            registry,
            closed: Arc::new(AtomicBool::new(false)),
        },
        receiver,
    )
}

#[tokio::test]
async fn buffered_publication_ack_is_not_rolled_back_when_worker_polls_late() {
    let (_home, registry, scope, key) = fixture();
    let (handle, _receiver) = handle(registry);
    let operation = IssueOperation::new(tokio::time::Instant::now() + Duration::from_secs(60));
    let _caller = operation.caller_guard();
    let (commit_ack, acknowledged) = oneshot::channel();
    let ready = Ready {
        response: prepare_add_response(&scope, &key)
            .unwrap_or_else(|_| panic!("synthetic response")),
        scope: scope.clone(),
        commit_ack,
    };
    assert!(ready.publish(&handle, &operation).is_ok());
    assert!(
        wait_ack(
            acknowledged,
            tokio::time::Instant::now() - Duration::from_secs(1)
        )
        .await
    );
    assert_eq!(handle.registry.try_identify(&key).unwrap().scope(), &scope);
}

#[tokio::test]
async fn caller_drop_revoke_and_closed_ack_each_refuse_publication() {
    for failure in 0..3 {
        let (_home, registry, scope, key) = fixture();
        let (handle, _receiver) = handle(registry.clone());
        let operation = IssueOperation::new(tokio::time::Instant::now() + Duration::from_secs(60));
        let caller = operation.caller_guard();
        let (commit_ack, mut acknowledged) = oneshot::channel();
        if failure == 0 {
            drop(caller);
        }
        if failure == 1 {
            registry.revoke(scope.name()).unwrap();
        }
        if failure == 2 {
            acknowledged.close();
        }
        let ready = Ready {
            response: prepare_add_response(&scope, &key)
                .unwrap_or_else(|_| panic!("synthetic response")),
            scope,
            commit_ack,
        };
        assert!(ready.publish(&handle, &operation).is_err());
        assert!(acknowledged.await.is_err());
    }
}

#[tokio::test]
async fn full_admin_queue_cannot_consume_reserved_stop_capacity() {
    let (_home, registry, _scope, _key) = fixture();
    let (handle, mut receiver) = handle(registry);
    let stop = handle.sender.clone().try_reserve_owned().ok().unwrap();
    for _ in 0..QUEUE - 1 {
        let (reply, _receive) = oneshot::channel();
        assert!(handle.send(Command::List(reply)).is_ok());
    }
    let (reply, _receive) = oneshot::channel();
    assert!(handle.send(Command::List(reply)).is_err());
    stop.send(Command::Stop);
    for _ in 0..QUEUE - 1 {
        assert!(matches!(receiver.recv().await, Some(Command::List(_))));
    }
    assert!(matches!(receiver.recv().await, Some(Command::Stop)));
}

#[tokio::test]
async fn grant_close_is_observed_before_retained_writer_returns() {
    let (_home, registry, scope, key) = fixture();
    let authority = registry.authenticate(&scope, &key).unwrap();
    let request = authority.admit().unwrap();
    let (release, held) = std::sync::mpsc::channel();
    let (closed, observed) = oneshot::channel();
    let (_drain, drain) = watch::channel(false);
    let name = scope.name().clone();
    let task = tokio::spawn(async move {
        let mut closed = Some(closed);
        watched_write(
            Some(&authority),
            None,
            drain,
            move || {
                let result = registry.revoke(&name);
                held.recv().unwrap(); // Actual durable writer remains owned.
                result
            },
            |event| {
                if matches!(event, WriteEvent::GrantClosed)
                    && let Some(closed) = closed.take()
                {
                    let _ = closed.send(());
                }
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    assert!(!task.is_finished());
    release.send(()).unwrap();
    assert!(task.await.unwrap().is_ok());
    drop(request);
}

#[tokio::test]
async fn cancelled_before_admission_does_not_close_active_grant() {
    let (_home, registry, scope, key) = fixture();
    let authority = registry.authenticate(&scope, &key).unwrap();
    let operation = IssueOperation::new(tokio::time::Instant::now() + Duration::from_secs(60));
    drop(operation.caller_guard());
    let writer_operation = operation.clone();
    // The real owner retains its registry throughout every blocking write.
    // Dropping the last registry intentionally revokes all grants on teardown.
    let writer_registry = registry.clone();
    let (_drain, drain) = watch::channel(false);
    let mut closed = false;
    let result = watched_write(
        Some(&authority),
        Some(&operation),
        drain,
        move || writer_registry.revoke_for_operation(scope.name(), &writer_operation),
        |event| {
            closed |= matches!(event, WriteEvent::GrantClosed);
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(BoxAdminError::Registry(RegistryError::Cancelled))
    ));
    assert!(!closed);
    assert!(!authority.is_revoked());
    assert!(registry.try_identify(&key).is_ok());
}

#[tokio::test]
async fn drain_is_observed_without_abandoning_blocking_writer() {
    let (_home, registry, scope, _key) = fixture();
    let (entered, started) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let (noticed, observed) = oneshot::channel();
    let (drain, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut noticed = Some(noticed);
        watched_write(
            None,
            None,
            receiver,
            move || {
                let _ = entered.send(());
                held.recv().unwrap();
                registry.revoke(scope.name())
            },
            |event| {
                if matches!(event, WriteEvent::Drain)
                    && let Some(noticed) = noticed.take()
                {
                    let _ = noticed.send(());
                }
            },
        )
        .await
    });
    started.await.unwrap();
    drain.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    assert!(!task.is_finished());
    release.send(()).unwrap();
    assert!(task.await.unwrap().is_ok());
}

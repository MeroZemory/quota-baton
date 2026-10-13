use super::*;

fn session(state: LoginState, finished_at: Option<tokio::time::Instant>) -> Arc<Session> {
    let (status, _) = watch::channel(LoginStatus {
        status: state,
        account_id: None,
        error: None,
    });
    let (cancel, _) = watch::channel(false);
    Arc::new(Session {
        status,
        cancel,
        finished_at: Mutex::new(finished_at),
    })
}

#[tokio::test(start_paused = true)]
async fn terminal_sessions_remain_pollable_then_are_pruned_without_cancelling_pending() {
    let now = tokio::time::Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert("pending".to_owned(), session(LoginState::Pending, None));
    for (id, state) in [
        ("completed", LoginState::Completed),
        ("failed", LoginState::Failed),
        ("expired", LoginState::Expired),
        ("cancelled", LoginState::Cancelled),
    ] {
        sessions.insert(id.to_owned(), session(state, Some(now)));
    }
    tokio::time::advance(TERMINAL_RETENTION - Duration::from_secs(1)).await;
    prune_sessions(&mut sessions);
    assert_eq!(sessions.len(), 5);
    tokio::time::advance(Duration::from_secs(1)).await;
    prune_sessions(&mut sessions);
    assert_eq!(sessions.len(), 1);
    assert!(!*sessions["pending"].cancel.borrow());
}

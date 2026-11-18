use super::*;

fn raw(model: &str) -> Value {
    json!({"models":[{"slug":model,"display_name":"provider metadata"}]})
}

#[test]
fn client_version_commits_with_catalog_and_legacy_finish_uses_none() {
    let f = Fixture::new();
    f.begin("a")
        .finish_with_client_version(Ok(raw("old")), Some("0.170.0".to_owned()))
        .unwrap();
    let old = f
        .cache
        .view("a")
        .unwrap()
        .observation
        .last_known()
        .unwrap()
        .clone();
    assert_eq!(old.client_version(), Some("0.170.0"));
    assert_eq!(old.raw(), &raw("old"));
    f.advance(1);
    f.begin("a")
        .finish_with_client_version(Ok(raw("new")), Some("0.171.0".to_owned()))
        .unwrap();
    let view = f.cache.view("a").unwrap();
    let known = view.observation.last_known().unwrap();
    assert_eq!(known.client_version(), Some("0.171.0"));
    assert_eq!(known.supported(), &BTreeSet::from(["new".to_owned()]));
    assert!(known.observed_at() > old.observed_at());
    assert_eq!(old.client_version(), Some("0.170.0"));
    assert_eq!(old.raw(), &raw("old"));
    f.begin("a").finish(Ok(raw("legacy"))).unwrap();
    let view = f.cache.view("a").unwrap();
    assert_eq!(
        view.observation.last_known().unwrap().client_version(),
        None
    );
}

#[test]
fn failed_invalid_and_cancelled_refresh_keep_original_version_and_facts() {
    let f = Fixture::new();
    f.begin("a")
        .finish_with_client_version(Ok(raw("m")), Some("0.170.0".to_owned()))
        .unwrap();
    let known = f
        .cache
        .view("a")
        .unwrap()
        .observation
        .last_known()
        .unwrap()
        .clone();
    f.cache.deny_model("a", "m").unwrap();
    for outcome in 0..3 {
        f.advance(1);
        let guard = f.begin("a");
        match outcome {
            0 => guard
                .finish_with_client_version(
                    Err(RefreshFailure::Transport),
                    Some("0.171.0".to_owned()),
                )
                .unwrap(),
            1 => assert_eq!(
                guard.finish_with_client_version(
                    Ok(json!({"models":true})),
                    Some("0.171.0".to_owned())
                ),
                Err(CatalogError::InvalidShape)
            ),
            _ => drop(guard),
        }
        let view = f.cache.view("a").unwrap();
        assert!(matches!(
            &view.observation,
            Observation::RefreshFailed { .. }
        ));
        let retained = view.observation.last_known().unwrap();
        assert!(Arc::ptr_eq(retained, &known));
        assert_eq!(retained.client_version(), Some("0.170.0"));
        assert_eq!(retained.raw(), &raw("m"));
        assert_eq!(retained.observed_at(), known.observed_at());
        assert_eq!(
            retained.observed_at_wallclock(),
            known.observed_at_wallclock()
        );
        assert!(f.cache.is_model_denied("a", "m").unwrap());
    }
}

#[test]
fn mark_due_preserves_cached_facts_denials_and_other_accounts() {
    let f = Fixture::new();
    for id in ["a", "b"] {
        f.begin(id)
            .finish_with_client_version(Ok(raw("m")), Some("0.170.0".to_owned()))
            .unwrap();
    }
    f.cache.deny_model("a", "m").unwrap();
    let before = f
        .cache
        .view("a")
        .unwrap()
        .observation
        .last_known()
        .unwrap()
        .clone();
    assert!(!f.cache.view("a").unwrap().refresh_due);
    f.cache.mark_due("a").unwrap();
    let marked = f.cache.view("a").unwrap();
    assert!(marked.refresh_due && !marked.refreshing);
    assert!(Arc::ptr_eq(
        marked.observation.last_known().unwrap(),
        &before
    ));
    assert!(f.cache.is_model_denied("a", "m").unwrap());
    assert!(!f.cache.view("b").unwrap().refresh_due);
    f.cache
        .begin_refresh("a", RefreshMode::IfDue)
        .unwrap()
        .unwrap()
        .finish_with_client_version(Ok(raw("m")), Some("0.171.0".to_owned()))
        .unwrap();
    let updated = f.cache.view("a").unwrap();
    assert!(!updated.refresh_due && !updated.refreshing);
    assert_eq!(
        updated.observation.last_known().unwrap().client_version(),
        Some("0.171.0")
    );
    assert!(!f.cache.is_model_denied("a", "m").unwrap());
}

#[test]
fn mark_during_active_refresh_survives_every_completion_and_is_consumed_by_next() {
    for outcome in 0..4 {
        let f = Fixture::new();
        f.begin("a")
            .finish_with_client_version(Ok(raw("m")), Some("0.170.0".to_owned()))
            .unwrap();
        f.cache.deny_model("a", "m").unwrap();
        let old = f.begin("a");
        let ticket = f.cache.0.lock().entries["a"].active_ticket;
        f.cache.mark_due("a").unwrap();
        f.cache.mark_due("a").unwrap();
        let marked = f.cache.view("a").unwrap();
        assert!(marked.refreshing && marked.refresh_due);
        assert!(f.cache.is_model_denied("a", "m").unwrap());
        assert_eq!(f.cache.0.lock().entries["a"].active_ticket, ticket);
        for mode in [RefreshMode::IfDue, RefreshMode::Force] {
            assert_eq!(
                f.cache.begin_refresh("a", mode).unwrap_err(),
                CatalogError::RefreshInFlight
            );
        }
        match outcome {
            0 => old
                .finish_with_client_version(Ok(raw("m")), Some("0.170.0".to_owned()))
                .unwrap(),
            1 => old
                .finish_with_client_version(
                    Err(RefreshFailure::Upstream),
                    Some("0.170.0".to_owned()),
                )
                .unwrap(),
            2 => assert_eq!(
                old.finish_with_client_version(
                    Ok(json!({"models":true})),
                    Some("0.170.0".to_owned())
                ),
                Err(CatalogError::InvalidShape)
            ),
            _ => drop(old),
        }
        let completed = f.cache.view("a").unwrap();
        assert!(completed.refresh_due && !completed.refreshing);
        assert_eq!(
            completed.observation.last_known().unwrap().client_version(),
            Some("0.170.0")
        );
        assert_eq!(f.cache.is_model_denied("a", "m").unwrap(), outcome != 0);
        f.cache
            .begin_refresh("a", RefreshMode::IfDue)
            .unwrap()
            .unwrap()
            .finish_with_client_version(Ok(raw("m")), Some("0.171.0".to_owned()))
            .unwrap();
        assert!(!f.cache.view("a").unwrap().refresh_due);
        assert!(
            f.cache
                .begin_refresh("a", RefreshMode::IfDue)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn mark_due_validates_unknown_ids_without_allocating_even_at_capacity() {
    let f = Fixture::new();
    for id in ["", "bad\naccount", &"x".repeat(MAX_ID_BYTES + 1)] {
        assert_eq!(f.cache.mark_due(id), Err(CatalogError::InvalidAccount));
    }
    assert!(f.cache.0.lock().entries.is_empty());
    for i in 0..MAX_ACCOUNTS {
        f.cache.deny_model(&format!("a-{i}"), "m").unwrap();
    }
    f.cache.mark_due("unknown").unwrap();
    let view = f.cache.view("unknown").unwrap();
    assert!(view.refresh_due && !view.refreshing);
    assert!(matches!(view.observation, Observation::Unknown));
    assert_eq!(f.cache.0.lock().entries.len(), MAX_ACCOUNTS);
}

#[test]
fn stale_ticket_cannot_publish_client_version_or_consume_pending_invalidation() {
    let f = Fixture::new();
    let first = f.begin("a");
    let stale = RefreshGuard {
        inner: Arc::clone(&first.inner),
        account: first.account.clone(),
        ticket: first.ticket,
        finished: false,
    };
    first
        .finish_with_client_version(Ok(raw("old")), Some("0.170.0".to_owned()))
        .unwrap();
    let current = f.begin("a");
    f.cache.mark_due("a").unwrap();
    assert_eq!(
        stale.finish_with_client_version(Ok(raw("stale")), Some("0.999.0".to_owned())),
        Err(CatalogError::StaleObservation)
    );
    let view = f.cache.view("a").unwrap();
    assert!(view.refreshing && view.refresh_due);
    let known = view.observation.last_known().unwrap();
    assert_eq!(known.client_version(), Some("0.170.0"));
    assert_eq!(known.raw(), &raw("old"));
    current
        .finish_with_client_version(Ok(raw("new")), Some("0.171.0".to_owned()))
        .unwrap();
    assert!(f.cache.view("a").unwrap().refresh_due);
}

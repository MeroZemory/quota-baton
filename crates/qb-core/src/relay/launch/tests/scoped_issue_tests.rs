//! Actual catalog/placement/publication pipeline, using the parent launch fixture.
use super::*;

#[tokio::test]
async fn host_box_launch_router_issues_exact_scope_and_refuses_unknown_or_revoked() {
    let f = Fixture::new().await;
    let boxes = crate::boxes::BoxRegistry::open(f._root.path()).unwrap();
    let (scope_a, _key_a) = boxes
        .add(crate::boxes::BoxName::parse("router-a").unwrap())
        .unwrap()
        .into_parts();
    let (scope_b, _key_b) = boxes
        .add(crate::boxes::BoxName::parse("router-b").unwrap())
        .unwrap()
        .into_parts();
    // Product integration must nest this router under host admin safety. This
    // regression exercises its actual route and issue pipeline, not that wrapper.
    let router = crate::admin::boxes::scoped_launch_routes(boxes.clone(), f.relay.clone(), 10200);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = crate::boxes::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let before = f.relay.bindings.export().unwrap();
    let response = client
        .post(format!("http://{address}/admin/v1/boxes/unknown/launches"))
        .json(&json!({"model":"common"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(f.upstream.state.requests().is_empty());
    assert_eq!(f.relay.bindings.export().unwrap(), before);
    assert_eq!(f.relay.launch_tokens.entry_count_for_test(), 0);

    f.reply(StatusCode::OK, json!({"models":[{"slug":"common"}]}));
    let response = client
        .post(format!("http://{address}/admin/v1/boxes/router-a/launches"))
        .json(&json!({"model":"common"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let reply: qb_proto::admin_launch::LaunchReply = response.json().await.unwrap();
    assert_eq!(reply.account, f.id);
    let token = issued_handle(&reply);
    assert!(
        f.relay
            .launch_tokens
            .get_or_pin_scoped(token, Some(&scope_a))
            .is_ok()
    );
    for scope in [Some(&scope_b), None] {
        assert!(matches!(
            f.relay.launch_tokens.get_or_pin_scoped(token, scope),
            Err(crate::launch_tokens::LaunchTokenError::ScopeMismatch)
        ));
    }
    assert_eq!(f.upstream.state.requests().len(), 1);
    assert_eq!(f.relay.launch_tokens.entry_count_for_test(), 1);
    let published = f.relay.bindings.export().unwrap();

    boxes.revoke(scope_a.name()).unwrap();
    let response = client
        .post(format!("http://{address}/admin/v1/boxes/router-a/launches"))
        .json(&json!({"model":"common"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(f.upstream.state.requests().len(), 1);
    assert_eq!(f.relay.launch_tokens.entry_count_for_test(), 1);
    assert_eq!(f.relay.bindings.export().unwrap(), published);
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("isolated router must stop")
        .unwrap();
}

#[tokio::test]
async fn box_issue_pipeline_publishes_a_token_for_only_the_authenticated_scope() {
    let f = Fixture::new().await;
    let boxes = crate::boxes::BoxRegistry::open(f._root.path()).unwrap();
    let (scope_a, key_a) = boxes
        .add(crate::boxes::BoxName::parse("issue-a").unwrap())
        .unwrap()
        .into_parts();
    let (scope_b, _key_b) = boxes
        .add(crate::boxes::BoxName::parse("issue-b").unwrap())
        .unwrap()
        .into_parts();
    let authority = boxes.authenticate(&scope_a, &key_a).unwrap();
    f.reply(StatusCode::OK, json!({"models":[{"slug":"common"}]}));
    let operation = crate::issue_operation::IssueOperation::new(
        tokio::time::Instant::now() + Duration::from_secs(10),
    );
    let _caller = operation.caller_guard();
    let reply = f
        .relay
        .issue_box_launch_for_operation(
            issue_request(Some("common")),
            10200,
            operation,
            boxes.clone(),
            authority,
        )
        .await
        .unwrap();
    assert_eq!(reply.account, f.id);
    assert_eq!(f.upstream.state.requests().len(), 1);
    let token = issued_handle(&reply);
    let pin = f
        .relay
        .launch_tokens
        .get_or_pin_scoped(token, Some(&scope_a))
        .unwrap();
    let placement = f
        .relay
        .bindings
        .launch_info(&pin.metadata().binding_anchor_key, chrono::Utc::now())
        .unwrap();
    assert_eq!(placement.account, f.id);
    assert!(matches!(
        f.relay
            .launch_tokens
            .get_or_pin_scoped(token, Some(&scope_b)),
        Err(crate::launch_tokens::LaunchTokenError::ScopeMismatch)
    ));
    assert!(matches!(
        f.relay.launch_tokens.get_or_pin(token),
        Err(crate::launch_tokens::LaunchTokenError::ScopeMismatch)
    ));
}

#[tokio::test]
async fn box_issue_pipeline_refuses_publication_after_revoke_during_catalog_await() {
    let f = Fixture::new().await;
    let boxes = crate::boxes::BoxRegistry::open(f._root.path()).unwrap();
    let (scope, key) = boxes
        .add(crate::boxes::BoxName::parse("issue-revoked").unwrap())
        .unwrap()
        .into_parts();
    let authority = boxes.authenticate(&scope, &key).unwrap();
    let (arrived, release) = f.upstream.state.pause_next_response();
    f.reply(StatusCode::OK, json!({"models":[{"slug":"common"}]}));
    let operation = crate::issue_operation::IssueOperation::new(
        tokio::time::Instant::now() + Duration::from_secs(10),
    );
    let _caller = operation.caller_guard();
    let issue = f.relay.issue_box_launch_for_operation(
        issue_request(Some("common")),
        10200,
        operation,
        boxes.clone(),
        authority,
    );
    tokio::pin!(issue);
    // Observe the actual catalog HTTP request before revoking; no sleeps or
    // scheduler assumptions stand in for reaching the awaited catalog stage.
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = &mut issue => panic!("issue completed before the held catalog response"),
            observed = arrived => observed.unwrap(),
        }
    })
    .await
    .expect("catalog request must reach the existing response gate");
    assert_eq!(f.upstream.state.requests().len(), 1);
    boxes.revoke(scope.name()).unwrap();
    release.send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), &mut issue)
            .await
            .expect("revoked issuance must finish within its operation budget"),
        Err(LaunchError::Unavailable)
    ));
}

#[tokio::test]
async fn box_issue_revoke_behind_actual_mutations_gate_leaves_no_placement_or_token() {
    let f = Fixture::new().await;
    warm_issue_catalog(&f, &f.id, &["common"]).await;
    let before = f.relay.bindings.export().unwrap();
    let boxes = crate::boxes::BoxRegistry::open(f._root.path()).unwrap();
    let (scope, key) = boxes
        .add(crate::boxes::BoxName::parse("issue-queued").unwrap())
        .unwrap()
        .into_parts();
    let authority = boxes.authenticate(&scope, &key).unwrap();
    let operation = crate::issue_operation::IssueOperation::new(
        tokio::time::Instant::now() + Duration::from_secs(15),
    );
    let _caller = operation.caller_guard();
    assert_eq!(f.relay.launch_tokens.entry_count_for_test(), 0);
    let held = f.relay.bindings.lock_launch_mutations_for_test();
    let issue = f.relay.issue_box_launch_for_operation(
        issue_request(Some("common")),
        10200,
        operation.clone(),
        boxes.clone(),
        authority,
    );
    tokio::pin!(issue);
    // The pending token is allocated AFTER relay's optimistic revoked check and
    // immediately BEFORE actual placement. Holding mutations prevents admission.
    // Observe that concrete state while polling the real issuance future.
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = &mut issue => panic!("issue escaped the held binding mutation gate"),
            _ = async {
                while f.relay.launch_tokens.entry_count_for_test() == 0 {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
    })
    .await
    .expect("pending issuance must reach the actual held placement gate");
    assert_eq!(f.relay.launch_tokens.entry_count_for_test(), 1);
    assert!(!operation.is_admitted());
    boxes.revoke(scope.name()).unwrap();
    drop(held);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), &mut issue)
            .await
            .expect("queued revoked issuance must finish"),
        Err(LaunchError::Unavailable)
    ));
    assert!(!operation.is_admitted());
    assert_eq!(f.relay.bindings.export().unwrap(), before);
    assert_eq!(f.relay.launch_tokens.entry_count_for_test(), 0);
}

#[tokio::test]
async fn box_operation_admission_preserves_admitted_completion_after_revoke() {
    let f = Fixture::new().await;
    let boxes = crate::boxes::BoxRegistry::open(f._root.path()).unwrap();
    let (scope, key) = boxes
        .add(crate::boxes::BoxName::parse("issue-admitted").unwrap())
        .unwrap()
        .into_parts();
    let request = boxes.authenticate(&scope, &key).unwrap().admit().unwrap();
    let operation = crate::issue_operation::IssueOperation::new(
        tokio::time::Instant::now() + Duration::from_secs(10),
    );
    let fresh = crate::issue_operation::IssueOperation::new(
        tokio::time::Instant::now() + Duration::from_secs(10),
    );
    crate::boxes::scope_request(request, async {
        crate::boxes::admit_operation(&operation).unwrap();
        boxes.revoke(scope.name()).unwrap();
        assert!(crate::boxes::admit_operation(&operation).is_ok());
        assert!(crate::boxes::admit_operation(&fresh).is_err());
        assert!(!fresh.is_admitted());
    })
    .await;
    // Ordinary callers still use the original operation admission semantics.
    assert!(crate::boxes::admit_operation(&fresh).is_ok());
}

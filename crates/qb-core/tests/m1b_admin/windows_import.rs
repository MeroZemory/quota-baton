use super::*;

// Import uses the same protected HTTP/auth shell, with an explicitly supplied
// synthetic source directory and no running quota scheduler/profile poller.
async fn import_contract_rows(
    h: &Harness,
    request: &Value,
) -> Vec<qb_core::accounts::import::ImportRow> {
    let response = h
        .request(reqwest::Method::POST, "/accounts/import")
        .json(request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.unwrap();
    secret_free(std::str::from_utf8(&body).expect("import body is not UTF-8"));
    let rows: Vec<qb_core::accounts::import::ImportRow> = serde_json::from_slice(&body)
        .unwrap_or_else(|_| panic!("import response violates shared row contract"));
    for secret in ["synthetic-import-access-", "synthetic-import-refresh-"] {
        assert!(
            !std::str::from_utf8(&body).unwrap().contains(secret),
            "import body exposed synthetic token"
        );
    }
    let wire: Value = serde_json::from_slice(&body).unwrap();
    let expected = [
        "id",
        "provider",
        "email",
        "plan",
        "expires_at",
        "user_state",
        "action",
        "reason",
        "stored_timestamp",
        "file_timestamp",
    ];
    for row in wire.as_array().unwrap() {
        assert!(
            row.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .eq(expected),
            "import row key set or order changed"
        );
    }
    let encoded = serde_json::to_vec(&rows).unwrap();
    assert!(
        encoded.as_slice() == body.as_ref(),
        "import body roundtrip changed bytes"
    );
    rows
}

// Dedicated to the new contract case; existing Harness drop semantics stay unchanged.
async fn import_contract_cleanup(h: Harness) -> Result<(), String> {
    let Harness {
        mut servers,
        service,
        store,
        state,
        obs,
        client,
        oauth,
        refresh,
        data,
        _usage,
        home,
        _root,
        _serial,
    } = h;
    let limit = Duration::from_secs(10);
    let mut errors = Vec::new();
    servers.shutdown();
    drop(client);
    if tokio::time::timeout(limit, service.cancel_all())
        .await
        .is_err()
    {
        errors.push("login cancellation timed out");
    }
    for task in [&mut servers.data_task, &mut servers.admin_task] {
        match tokio::time::timeout(limit, &mut *task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(_) => errors.push("server task failed"),
            Err(_) => {
                errors.push("server shutdown timed out");
                task.abort();
                // Reap after abort too: dropping a JoinHandle is not completion.
                let _ = task.await;
            }
        }
    }
    drop(servers);
    // Retain on any teardown failure (including a panicking cleanup thread).
    // No TempDir destructor may race outstanding owners.
    let root = _root.keep();
    let cleanup = std::thread::spawn(move || {
        drop(service);
        drop(state);
        drop((oauth, refresh, data, _usage));
        if Arc::strong_count(&store) != 1 {
            errors.push("fixture store owners remain after server join");
        }
        // AccountRuntime installs a store observer that owns an obs clone.
        // Release that callback before checking the remaining writer owner.
        drop(store);
        if Arc::strong_count(&obs) != 1 {
            errors.push("fixture observability owners remain after store drop");
        }
        if obs.flush().is_err() {
            errors.push("observability flush failed");
        }
        // Outside Tokio, Observability::drop stops and joins its writer inline.
        drop(obs);
        drop(home);
        if !errors.is_empty() {
            return Err(format!(
                "{}; retained {}",
                errors.join("; "),
                root.display()
            ));
        }
        std::fs::remove_dir_all(&root)
            .map_err(|error| format!("synthetic home removal failed: {error}"))?;
        if root.exists() {
            return Err("synthetic home remains after removal".into());
        }
        Ok(())
    })
    .join()
    .unwrap_or_else(|_| Err("ordinary cleanup thread panicked; synthetic home retained".into()));
    drop(_serial);
    cleanup
}

#[tokio::test]
async fn r15_protected_import_returns_exact_shared_rows_without_token_leaks() {
    use futures_util::FutureExt;
    use qb_core::accounts::import::{ImportAction, SkipReason};
    let h = Harness::new(0).await;
    let body = std::panic::AssertUnwindSafe(async {
        let cpa = h._root.path().join("synthetic-import-source");
        fs::create_dir(&cpa).unwrap();
        let fixture = |version| {
            json!({"type":"claude", "email":"import@example.invalid",
        "account_uuid":"synthetic-import-user", "organization_uuid":"synthetic-import-org",
        "access_token":format!("synthetic-import-access-{version}"),
        "refresh_token":format!("synthetic-import-refresh-{version}"),
        "expired":"2099-01-01T00:00:00Z",
        "last_refresh":format!("2030-01-0{version}T00:00:00Z")})
        };
        let file = cpa.join("a-valid.json");
        fs::write(&file, serde_json::to_vec(&fixture(1)).unwrap()).unwrap();
        fs::write(cpa.join("b-unknown.json"), br#"{"type":"unsupported"}"#).unwrap();
        fs::write(cpa.join("c-invalid.json"), b"{").unwrap();
        let mut request =
            json!({"source":"cpa", "mode":"linked", "dir":cpa, "activate":[], "dry_run":true});
        let denied = h
            .client
            .post(h.url("/accounts/import"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        denied.bytes().await.unwrap();
        assert!(h.store.list().unwrap().is_empty());
        for invalid in [
            json!({"source":"other","mode":"linked","dir":cpa}),
            json!({"source":"cpa","mode":"adopted","dir":cpa}),
        ] {
            let refused = h
                .request(reqwest::Method::POST, "/accounts/import")
                .json(&invalid)
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
            refused.bytes().await.unwrap();
            assert!(h.store.list().unwrap().is_empty());
        }
        let empty_dir = h._root.path().join("synthetic-empty-import");
        fs::create_dir(&empty_dir).unwrap();
        let empty_request =
            json!({"source":"cpa","mode":"linked","dir":empty_dir,"activate":[],"dry_run":true});
        assert!(import_contract_rows(&h, &empty_request).await.is_empty());
        let preview = import_contract_rows(&h, &request).await;
        assert_eq!(preview.len(), 3);
        assert_eq!(preview[0].action, ImportAction::Created);
        assert_eq!(preview[1].reason, Some(SkipReason::UnknownType));
        assert_eq!(preview[2].reason, Some(SkipReason::InvalidData));
        assert!(h.store.list().unwrap().is_empty());
        request["dry_run"] = json!(false);
        let created = import_contract_rows(&h, &request).await;
        assert_eq!(created[0].action, ImportAction::Created);
        let id = created[0].id.as_deref().unwrap();
        assert_eq!(h.store.list().unwrap().len(), 1);
        fs::write(&file, serde_json::to_vec(&fixture(2)).unwrap()).unwrap();
        let updated = import_contract_rows(&h, &request).await;
        assert_eq!(updated[0].action, ImportAction::Updated);
        assert_eq!(updated[0].id.as_deref(), Some(id));
        fs::write(&file, serde_json::to_vec(&fixture(1)).unwrap()).unwrap();
        let older = import_contract_rows(&h, &request).await;
        assert_eq!(older[0].action, ImportAction::Skipped);
        assert_eq!(older[0].reason, Some(SkipReason::OlderToken));
        assert!(older[0].stored_timestamp.is_some());
        assert!(older[0].file_timestamp.is_some());
        let mut account = h.store.load(id).unwrap();
        let tokens = h.store.tokens(&account).unwrap();
        account.source = Source::Oauth;
        h.store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
        let owned = import_contract_rows(&h, &request).await;
        assert_eq!(owned[0].reason, Some(SkipReason::OwnedAccount));
        assert_eq!(owned[0].action, ImportAction::Skipped);
        assert!(h.oauth.state.requests().is_empty());
        assert!(h.refresh.state.requests().is_empty());
    })
    .catch_unwind()
    .await;
    let cleanup = import_contract_cleanup(h).await;
    if let Err(panic) = body {
        if let Err(error) = cleanup {
            eprintln!("{error}");
        }
        std::panic::resume_unwind(panic);
    }
    cleanup.unwrap_or_else(|error| panic!("{error}"));
}

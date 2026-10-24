use super::*;

#[tokio::test]
async fn catalog_current_401_uses_shared_linked_reload_without_marking_relogin() {
    let f = Fixture::new().await;
    f.reply(
        StatusCode::UNAUTHORIZED,
        json!({"error":{"code":"invalid_token"}}),
    );
    f.reply(StatusCode::OK, json!({"models":[{"slug":"after-reload"}]}));
    let (arrived, release) = f.upstream.state.pause_next_response();
    let (catalog, ()) = tokio::join!(f.relay.launch_catalog(&f.id, false, false), async {
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .expect("first catalog request did not arrive")
            .expect("first catalog response gate dropped");
        // The first before_request reload must still read token 0.
        // Publish its replacement only after that request is recorded.
        fs::write(
            f.cpa.join("account.json"),
            serde_json::to_vec(&credential(1)).unwrap(),
        )
        .unwrap();
        release.send(()).expect("catalog response gate abandoned");
    });
    assert!(catalog.unwrap().supported().contains("after-reload"));
    let calls = f.upstream.state.requests();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].headers["authorization"],
        "Bearer catalog-synthetic-access-0"
    );
    assert_eq!(
        calls[1].headers["authorization"],
        "Bearer catalog-synthetic-access-1"
    );
    assert!(!matches!(
        f.relay.store.load(&f.id).unwrap().health,
        Health::NeedsRelogin { .. }
    ));
}

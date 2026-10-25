//! Real deny-delete fault injection, selected only on Windows.
use super::*;

#[tokio::test]
async fn r6_fresh_login_discards_pending_rotation_after_real_write_failure_and_drain_finishes() {
    use std::os::windows::fs::OpenOptionsExt;
    let f = EngineFixture::new();
    let request = f.inspected().await;
    let Preparation::Commit(journal) =
        takeover::prepare(f.store.clone(), f.deps.clone(), request.clone())
            .await
            .unwrap()
    else {
        panic!("plan")
    };
    let journal = takeover::install(&f.store, &f.deps, *journal).unwrap();
    let mut account = f.store.load(&journal.account.id).unwrap();
    let tokens = f
        .store
        .takeover_tokens(&account, &journal.operation)
        .unwrap();
    let before = f.store.takeover_receipt(&account.id).unwrap().unwrap();
    account.token_meta.refresh_uncertain = true;
    let started = Receipt {
        operation: journal.operation.clone(),
        state: ReceiptState::Started,
    };
    account = f
        .store
        .save_takeover_receipt(
            &account,
            &tokens,
            Some(account.generation),
            Some(&before),
            &started,
        )
        .unwrap();
    let mut rotated = account.clone();
    rotated.token_meta.refresh_uncertain = false;
    rotated.token_meta.last_refresh_at = Some(chrono::Utc::now());
    let rotation = Rotation {
        account: rotated,
        tokens: Tokens {
            access_token: tokens.access_token.clone(),
            refresh_token: "synthetic-held-rotated".into(),
            id_token: None,
        },
        failed_token: zeroize::Zeroizing::new(tokens.access_token.clone()),
        store_error_in_memory: false,
        takeover_operation: Some(journal.operation.clone()),
        box_request: None,
    };
    // A real deny-delete file handle makes the atomic replacement fail.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(
            f.store
                .home()
                .join("accounts")
                .join(format!("{}.json", account.id)),
        )
        .unwrap();
    assert!(persist_async(f.store.clone(), &rotation).await.is_err());
    assert_eq!(
        f.store
            .takeover_receipt(&account.id)
            .unwrap()
            .unwrap()
            .state,
        ReceiptState::Started
    );
    let gate = f.gates.gate(&account.id).await;
    gate.lock().await.pending = Some(rotation);
    drop(held);
    account.source = Source::Oauth;
    account.token_meta.refresh_uncertain = false;
    let login_tokens = Tokens {
        access_token: tokens.access_token.clone(),
        refresh_token: "synthetic-fresh-family".into(),
        id_token: None,
    };
    let saved = crate::accounts::login::save(&f.store, &account, &login_tokens, false)
        .unwrap()
        .account;
    let activity = f.gates.lifecycle.track_write();
    let (g, s, o, retry_gate) = (
        f.gates.clone(),
        f.store.clone(),
        f.obs.clone(),
        gate.clone(),
    );
    tokio::spawn(async move {
        let _activity = activity;
        g.retry_rotation(s, o, retry_gate).await;
    });
    tokio::time::timeout(Duration::from_secs(3), f.gates.shutdown_refreshes())
        .await
        .unwrap()
        .unwrap();
    assert!(gate.lock().await.pending.is_none());
    assert_eq!(
        f.store
            .tokens(&f.store.load(&saved.id).unwrap())
            .unwrap()
            .refresh_token,
        "synthetic-fresh-family"
    );
    assert_eq!(
        takeover::finalize(&f.store, journal).unwrap()["refresh"]["ok"],
        false
    );
}

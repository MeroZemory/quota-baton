use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::accounts::{ImportMode, SourceMatch, linked};

#[test]
fn n5d_linked_reload_keeps_source_settings_and_native_health_recovery() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store, before) = fixture();
    let dir = home.path().join("synthetic-linked-source");
    fs::create_dir_all(&dir).unwrap();
    let id_token = format!(
        "e30.{}.synthetic-signature",
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "email":"continuity@example.invalid",
                "https://api.openai.com/auth":{
                    "chatgpt_account_id":"synthetic-continuity-account",
                    "chatgpt_user_id":"synthetic-continuity-user"
                }
            }))
            .unwrap()
        )
    );
    fs::write(dir.join("account.json"), serde_json::to_vec(&json!({
        "type":"codex","account_id":"synthetic-continuity-account",
        "email":"continuity@example.invalid","id_token":id_token,
        "access_token":"synthetic-linked-new-access","refresh_token":"synthetic-linked-new-refresh",
        "last_refresh":"2026-02-01T00:00:00Z","expired":"2099-01-01T00:00:00Z"
    })).unwrap()).unwrap();
    let mut linked_account = before.clone();
    linked_account.source = Source::Cpa {
        mode: ImportMode::Linked,
        dir: dir.clone(),
        r#match: SourceMatch::from(&before.identity),
    };
    linked_account.drain = true;
    linked_account.allow_credits = true;
    let linked_account = store
        .save(&linked_account, &tokens("initial"), Some(before.generation))
        .unwrap();
    let reloaded = linked::reload_and_store(&store, &linked_account).unwrap();
    assert_eq!(reloaded.source, linked_account.source);
    assert_eq!(reloaded.identity, linked_account.identity);
    assert!(reloaded.drain && reloaded.allow_credits);
    assert!(store.tokens(&reloaded).unwrap().access_token == "synthetic-linked-new-access");

    let mut stale = reloaded.clone();
    stale.health = Health::Cooling {
        reason: HealthReason::LinkStale,
        until: Utc::now() + Duration::hours(1),
        scope: "account".into(),
    };
    let stale = store
        .save(&stale, &tokens("initial"), Some(reloaded.generation))
        .unwrap();
    // A genuinely newer candidate requires native health recovery and therefore
    // must retain the ordinary FullSave path rather than fail the linked reload.
    let path = dir.join("account.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["last_refresh"] = json!("2026-03-01T00:00:00Z");
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    let recovered = linked::reload_and_store(&store, &stale).unwrap();
    assert_eq!(recovered.health, Health::Ready);
    assert_eq!(recovered.source, stale.source);
    assert!(recovered.drain && recovered.allow_credits);
}

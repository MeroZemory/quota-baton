use super::*;

#[tokio::test]
async fn r10_own_login_leaves_label_unset_for_both_providers() {
    use axum::http::StatusCode;
    use qb_testkit::{ScriptedResponse, ScriptedUpstream};
    use serde_json::json;
    for provider in [Provider::Claude, Provider::Codex] {
        let fake = ScriptedUpstream::start().await.unwrap();
        let claims = json!({"email":"login@example.invalid", "https://api.openai.com/auth": {
            "chatgpt_account_id":"synthetic-account", "chatgpt_user_id":"synthetic-user"
        }});
        let id_token = format!(
            "e30.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        fake.state.push(ScriptedResponse::json(StatusCode::OK, json!({
            "access_token":"synthetic-access", "refresh_token":"synthetic-refresh", "expires_in":3600, "id_token":id_token
        }).to_string()));
        if provider == Provider::Claude {
            fake.state.push(ScriptedResponse::json(
                StatusCode::OK,
                json!({
                    "account":{"uuid":"synthetic-user", "email":"login@example.invalid"},
                    "organization":{"uuid":"synthetic-org"}
                })
                .to_string(),
            ));
        }
        let mut params = Parameters::new(provider);
        params.token = format!("{}/token", fake.url);
        params.profile = format!("{}/api/oauth/profile", fake.url);
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let (account, tokens) = exchange(
            &client,
            &params,
            "synthetic-code",
            "synthetic-verifier",
            "synthetic-state",
        )
        .await
        .unwrap();
        assert!(account.label.is_empty());
        assert_eq!(account.email, "login@example.invalid");
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r10-exchange-tests");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let store = crate::accounts::AccountStore::acquire(root.path()).unwrap();
        crate::accounts::login::save(&store, &account, &tokens, false).unwrap();
        for label in [
            "Synthetic Research",
            "My custom label",
            "login@example.invalid",
            "login@example.invalid's Organization",
        ] {
            store
                .update_metadata(&account.id, |a| a.label = label.into())
                .unwrap();
            let saved = crate::accounts::login::save(&store, &account, &tokens, false).unwrap();
            assert_eq!(saved.account.label, label);
        }
    }
}

#[test]
fn claude_exchange_serializes_borrowed_secrets_in_zeroizing_body() {
    let params = Parameters::new(Provider::Claude);
    let (content_type, body) = exchange_body(
        &params,
        "synthetic\"code",
        "synthetic\\verifier",
        "synthetic-state",
    )
    .unwrap();
    assert_eq!(content_type, "application/json");
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["code"], "synthetic\"code");
    assert_eq!(value["code_verifier"], "synthetic\\verifier");
    assert_eq!(value["state"], "synthetic-state");
    assert_eq!(value["redirect_uri"], params.redirect());
    assert_eq!(value["grant_type"], "authorization_code");
}

#[test]
fn codex_exchange_percent_encodes_secrets_without_state() {
    let params = Parameters::new(Provider::Codex);
    let (content_type, body) = exchange_body(
        &params,
        "synthetic+&= code",
        "synthetic/verifier",
        "synthetic-state",
    )
    .unwrap();
    assert_eq!(content_type, "application/x-www-form-urlencoded");
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("code=synthetic%2B%26%3D%20code&"));
    assert!(text.contains("code_verifier=synthetic%2Fverifier"));
    assert!(!text.contains("state="));
}

#[test]
fn http_body_retains_the_zeroizing_owner_without_copying() {
    let secret = SecretBody(Zeroizing::new(b"synthetic-body".to_vec()));
    let _: &Zeroizing<Vec<u8>> = &secret.0;
    let pointer = secret.as_ref().as_ptr();
    let body = bytes::Bytes::from_owner(secret);
    let cloned = body.clone();
    assert_eq!(body.as_ptr(), pointer);
    drop(body);
    assert_eq!(cloned.as_ptr(), pointer);
    assert_eq!(cloned.as_ref(), b"synthetic-body");
}

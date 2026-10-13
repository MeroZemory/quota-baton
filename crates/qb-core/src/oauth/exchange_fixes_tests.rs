use super::*;

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

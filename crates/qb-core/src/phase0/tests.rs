use super::*;
use axum::{body::Body, response::Response};

fn directory() -> tempfile::TempDir {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/phase0-tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn capture(dir: &Path, route: &str, body: &Value, zstd: bool) -> (PathBuf, Vec<u8>) {
    let plain = serde_json::to_vec(body).unwrap();
    let raw = if zstd {
        zstd::stream::encode_all(plain.as_slice(), 0).unwrap()
    } else {
        plain
    };
    let mut headers = HeaderMap::new();
    if zstd {
        headers.insert("content-encoding", "zstd".parse().unwrap());
    }
    headers.insert("authorization", "Bearer synthetic-token".parse().unwrap());
    headers.insert("x-api-key", "synthetic-key".parse().unwrap());
    headers.insert("cookie", "synthetic-cookie".parse().unwrap());
    headers.insert("host", "synthetic-host".parse().unwrap());
    headers.append("anthropic-beta", "first".parse().unwrap());
    headers.append("anthropic-beta", "second".parse().unwrap());
    let path = BodyCapture::new(dir.to_owned(), 1)
        .unwrap()
        .record_file(route, &headers, "synthetic-account", &raw)
        .unwrap()
        .unwrap();
    (path, raw)
}

fn value(replay: &PreparedReplay) -> Value {
    serde_json::from_slice(&decode(&replay.headers, &replay.body).unwrap()).unwrap()
}

#[test]
fn codex_plain_and_zstd_transform_presence_and_raw_preservation() {
    let dir = directory();
    let body = json!({"input": [
        {"type":"reasoning","encrypted_content":"reasoning-secret"},
        {"type":"compaction","encrypted_content":"compaction-secret"},
        {"type":"message","content":[{"encrypted_content":"part-secret","text":"conversation-secret"}]},
        {"type":"reasoning","summary":[]}
    ]});
    for zstd in [false, true] {
        let (path, raw) = capture(dir.path(), "/v1/responses", &body, zstd);
        let unchanged = prepare(&path, Provider::Codex, StripMode::None, false).unwrap();
        assert_eq!(unchanged.body.as_ref(), raw);
        assert_eq!(
            unchanged.presence["encrypted_content"],
            json!({"reasoning":1,"compaction":1,"message_parts":1})
        );
        assert!(
            unchanged
                .forbidden
                .contains(&"conversation-secret".to_owned())
        );
        assert_eq!(
            unchanged.headers.get_all("anthropic-beta").iter().count(),
            2
        );
        assert!(unchanged.headers.get("authorization").is_none());
        let stripped = prepare(&path, Provider::Codex, StripMode::Reasoning, false).unwrap();
        assert_eq!(value(&stripped)["input"].as_array().unwrap().len(), 3);
        assert_eq!(
            value(&stripped)["input"][0]["encrypted_content"],
            "compaction-secret"
        );
        let both = prepare(&path, Provider::Codex, StripMode::Reasoning, true).unwrap();
        assert_eq!(
            value(&both)["input"][0]["encrypted_content"],
            "Aompaction-secret"
        );
        assert_eq!(both.presence, unchanged.presence);
        assert_eq!(
            value(&prepare(&path, Provider::Codex, StripMode::None, true).unwrap())["input"][0]["encrypted_content"],
            "Aeasoning-secret"
        );
        if zstd {
            assert!(serde_json::from_slice::<Value>(&both.body).is_err());
        }
        let saved = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "synthetic-token",
            "synthetic-key",
            "synthetic-cookie",
            "synthetic-host",
        ] {
            assert!(!saved.contains(secret));
        }
    }
}

#[test]
fn claude_plain_and_zstd_preserve_last_assistant_turn() {
    let dir = directory();
    let body = json!({"messages":[
        {"role":"assistant","content":[{"type":"thinking","thinking":"old-content","signature":"old-signature"},{"type":"redacted_thinking","data":"old-data"},{"type":"text","text":"old-text"}]},
        {"role":"user","content":"user-content"},
        {"role":"assistant","content":[{"type":"thinking","thinking":"last-content","signature":"last-signature"}]},
        {"role":"assistant","content":[{"type":"redacted_thinking","data":"last-data"}]},
        {"role":"user","content":"next-content"}
    ]});
    for zstd in [false, true] {
        let (path, raw) = capture(dir.path(), "/v1/messages", &body, zstd);
        assert_eq!(
            prepare(&path, Provider::Claude, StripMode::None, false)
                .unwrap()
                .body
                .as_ref(),
            raw
        );
        let stripped = prepare(&path, Provider::Claude, StripMode::Reasoning, false).unwrap();
        let parsed = value(&stripped);
        assert_eq!(stripped.presence["signed_thinking"], 2);
        assert_eq!(
            parsed["messages"][0]["content"].as_array().unwrap().len(),
            1
        );
        assert_eq!(parsed["messages"][2], body["messages"][2]);
        assert_eq!(parsed["messages"][3], body["messages"][3]);
        let both = value(&prepare(&path, Provider::Claude, StripMode::Reasoning, true).unwrap());
        assert_eq!(
            both["messages"][2]["content"][0]["signature"],
            "Aast-signature"
        );
        let tampered = value(&prepare(&path, Provider::Claude, StripMode::None, true).unwrap());
        assert_eq!(
            tampered["messages"][0]["content"][0]["signature"],
            "Ald-signature"
        );
    }
}

#[test]
fn path_route_and_capture_limits() {
    let dir = directory();
    assert!(BodyCapture::new(dir.path().to_owned(), 0).is_err());
    assert!(BodyCapture::new(dir.path().join("absent"), 1).is_err());
    assert!(BodyCapture::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")), 1).is_err());
    let capture = BodyCapture::new(dir.path().to_owned(), 2).unwrap();
    let headers = HeaderMap::new();
    assert!(
        capture
            .record_file("/v1/other", &headers, "account", b"{}")
            .unwrap()
            .is_none()
    );
    assert!(
        capture
            .record_file("/v1/messages?x=1", &headers, "account", b"{}")
            .unwrap()
            .is_none()
    );
    let first = capture
        .record_file("/v1/messages", &headers, "account", b"{}")
        .unwrap()
        .unwrap();
    assert!(
        capture
            .record_file("/v1/responses", &headers, "account", b"{}")
            .unwrap()
            .is_some()
    );
    assert!(
        capture
            .record_file("/v1/messages", &headers, "account", b"{}")
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    assert!(prepare(&first, Provider::Codex, StripMode::None, false).is_err());
    assert!(
        prepare(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("Cargo.toml")
                .as_path(),
            Provider::Claude,
            StripMode::None,
            false
        )
        .is_err()
    );
    let oversized = dir.path().join("oversized.json");
    File::create(&oversized)
        .unwrap()
        .set_len(MAX_CAPTURE_FILE_BYTES as u64 + 1)
        .unwrap();
    assert!(prepare(&oversized, Provider::Claude, StripMode::None, false).is_err());
}

#[test]
fn no_op_flags_preserve_whitespace_and_invalid_capture_errors_hide_content() {
    let dir = directory();
    let capture = BodyCapture::new(dir.path().to_owned(), DEFAULT_CAPTURE_LIMIT).unwrap();
    let raw = b"  { \"input\" : [ { \"role\" : \"user\", \"content\" : \"synthetic-private-prompt\" } ] }\n";
    let path = capture
        .record_file("/v1/responses", &HeaderMap::new(), "account", raw)
        .unwrap()
        .unwrap();
    let replay = prepare(&path, Provider::Codex, StripMode::Reasoning, true).unwrap();
    assert_eq!(replay.body.as_ref(), raw);
    assert_eq!(replay.presence["encrypted_content"]["message_parts"], 0);
    assert!(
        replay
            .forbidden
            .contains(&"synthetic-private-prompt".to_owned())
    );
    let saved = CaptureFile {
        route: "/v1/responses".to_owned(),
        headers: vec![
            (
                "authorization".to_owned(),
                "Bearer forged-secret".to_owned(),
            ),
            ("x-api-key".to_owned(), "forged-key".to_owned()),
        ],
        account_id: "account".to_owned(),
        raw_body_base64: STANDARD.encode(raw),
        decoded_body: None,
    };
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let replay = prepare(&path, Provider::Codex, StripMode::None, false).unwrap();
    assert!(replay.headers.get("authorization").is_none());
    assert!(replay.headers.get("x-api-key").is_none());
    std::fs::write(&path, b"{\"synthetic-private-invalid-body\"").unwrap();
    let error = prepare(&path, Provider::Codex, StripMode::None, false)
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "invalid capture file");
    let outside =
        tempfile::tempdir_in(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"))
            .unwrap();
    let misleading = outside.path().join(".sandbox");
    std::fs::write(&misleading, serde_json::to_vec(&saved).unwrap()).unwrap();
    assert!(prepare(&misleading, Provider::Codex, StripMode::None, false).is_err());
}

#[test]
fn bounded_raw_decoded_and_gzip_reencoding() {
    let dir = directory();
    let capture = BodyCapture::new(dir.path().to_owned(), 1).unwrap();
    let large = vec![b' '; MAX_BODY_BYTES + 1];
    assert!(
        capture
            .record_file("/v1/responses", &HeaderMap::new(), "account", &large)
            .is_err()
    );
    let compressed = zstd::stream::encode_all(large.as_slice(), 0).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("content-encoding", "zstd".parse().unwrap());
    assert!(
        capture
            .record_file("/v1/responses", &headers, "account", &compressed)
            .is_err()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let body = br#"{"input":[{"type":"compaction","encrypted_content":"secret"}]}"#;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(body).unwrap();
    let raw = encoder.finish().unwrap();
    let saved = CaptureFile {
        route: "/v1/responses".to_owned(),
        headers: vec![("content-encoding".to_owned(), "gzip".to_owned())],
        account_id: "account".to_owned(),
        raw_body_base64: STANDARD.encode(&raw),
        decoded_body: None,
    };
    let path = dir.path().join("gzip.json");
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    assert_eq!(
        prepare(&path, Provider::Codex, StripMode::None, false)
            .unwrap()
            .body
            .as_ref(),
        raw
    );
    let changed = prepare(&path, Provider::Codex, StripMode::None, true).unwrap();
    assert_eq!(changed.headers["content-encoding"], "gzip");
    assert_eq!(value(&changed)["input"][0]["encrypted_content"], "Aecret");
}

#[test]
fn capture_larger_than_body_limit_round_trips() {
    let dir = directory();
    let mut raw = vec![b' '; 49 * 1024 * 1024];
    raw.extend_from_slice(b"{}");
    let path = BodyCapture::new(dir.path().to_owned(), 1)
        .unwrap()
        .record("/v1/responses", &HeaderMap::new(), "account", &raw)
        .unwrap()
        .unwrap();
    assert!(path.metadata().unwrap().len() > MAX_BODY_BYTES as u64);
    assert!(path.metadata().unwrap().len() <= MAX_CAPTURE_FILE_BYTES as u64);
    let replay = prepare(&path, Provider::Codex, StripMode::None, false).unwrap();
    assert_eq!(replay.body.as_ref(), raw);
}

#[test]
fn strip_modes_and_diagnostics_for_all_request_encodings() {
    let dir = directory();
    assert_eq!(StripMode::default(), StripMode::None);
    assert_eq!(
        serde_json::to_value(StripMode::Reasoning).unwrap(),
        "reasoning"
    );
    assert!(serde_json::from_str::<StripMode>("false").is_err());
    let codex = json!({"input":[
        {"type":"reasoning","encrypted_content":"reasoning-secret"},
        {"type":"compaction","encrypted_content":"compaction-secret"},
        {"role":"user","content":[{"encrypted_content":"part-secret"}]},
        {"type":"reasoning","summary":[]},
        {"role":"user","content":"keep-text"}
    ]});
    let claude = json!({"messages":[
        {"role":"assistant","content":[{"type":"thinking","signature":"old-signature"},{"type":"redacted_thinking","data":"old-data"}]},
        {"role":"user","content":"keep-user"},
        {"role":"assistant","content":[{"type":"thinking","signature":"last-signature"},{"type":"redacted_thinking","data":"last-data"},{"type":"text","text":"keep-text"}]}
    ]});
    for coding in ["identity", "zstd", "gzip", "deflate", "br"] {
        for (provider, route, body) in [
            (Provider::Codex, "/v1/responses", &codex),
            (Provider::Claude, "/v1/messages", &claude),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("content-encoding", coding.parse().unwrap());
            headers.insert("content-length", "999".parse().unwrap());
            let raw = encode(&headers, &serde_json::to_vec(body).unwrap()).unwrap();
            let path = BodyCapture::new(dir.path().to_owned(), 1)
                .unwrap()
                .record(route, &headers, "account", &raw)
                .unwrap()
                .unwrap();
            let untouched = prepare(&path, provider, StripMode::None, false).unwrap();
            assert_eq!(untouched.body.as_ref(), raw);
            assert_eq!(untouched.diagnostics["strip_encrypted"], "none");
            assert!(untouched.diagnostics["tampered_type"].is_null());
            for mode in [StripMode::Reasoning, StripMode::All] {
                let replay = prepare(&path, provider, mode, true).unwrap();
                assert_eq!(replay.headers["content-encoding"], coding);
                assert!(!replay.headers.contains_key("content-length"));
                assert_eq!(replay.presence, untouched.presence);
                let transformed = value(&replay);
                match (provider, mode) {
                    (Provider::Codex, StripMode::Reasoning) => {
                        assert_eq!(
                            replay.diagnostics["removed_counts"],
                            json!({"reasoning":1,"compaction":0,"message":0,"other":0})
                        );
                        assert_eq!(replay.diagnostics["tampered_type"], "compaction");
                        assert_eq!(
                            transformed["input"][0]["encrypted_content"],
                            "Aompaction-secret"
                        );
                    }
                    (Provider::Codex, StripMode::All) => {
                        assert_eq!(
                            replay.diagnostics["removed_counts"],
                            json!({"reasoning":1,"compaction":1,"message":1,"other":0})
                        );
                        assert!(replay.diagnostics["tampered_type"].is_null());
                        assert_eq!(transformed["input"].as_array().unwrap().len(), 2);
                    }
                    (Provider::Claude, StripMode::Reasoning) => {
                        assert_eq!(
                            replay.diagnostics["removed_counts"],
                            json!({"thinking":1,"redacted_thinking":1})
                        );
                        assert_eq!(replay.diagnostics["tampered_type"], "thinking");
                    }
                    (Provider::Claude, StripMode::All) => {
                        assert_eq!(
                            replay.diagnostics["removed_counts"],
                            json!({"thinking":2,"redacted_thinking":2})
                        );
                        assert!(replay.diagnostics["tampered_type"].is_null());
                        assert_eq!(
                            transformed["messages"][2]["content"]
                                .as_array()
                                .unwrap()
                                .len(),
                            1
                        );
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}

#[tokio::test]
async fn response_encodings_and_machine_fields_preserved() {
    for coding in ["identity", "zstd", "gzip", "deflate", "br"] {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", coding.parse().unwrap());
        let body = json!({"error":{"type":"invalid_reasoning_error","code":"message_invalid","message":"synthetic-credential synthetic-prompt"},"usage":{"input_tokens":3,"input_tokens_details":{"cached_tokens":2}},"content":"synthetic-never-output"});
        let mut response = Response::new(Body::from(
            encode(&headers, &serde_json::to_vec(&body).unwrap()).unwrap(),
        ));
        *response.headers_mut() = headers;
        let forbidden = [
            "reasoning",
            "message",
            "account",
            "synthetic-credential",
            "synthetic-prompt",
        ]
        .map(str::to_owned);
        let summary = summarize(response, "account", &forbidden).await.unwrap();
        assert_eq!(summary["account"], "account");
        assert_eq!(summary["error"]["type"], "invalid_reasoning_error");
        assert_eq!(summary["error"]["code"], "message_invalid");
        assert_eq!(summary["error"]["message"], "[redacted] [redacted]");
        assert_eq!(summary["usage"]["input_tokens_details"]["cached_tokens"], 2);
        assert!(!summary.to_string().contains("synthetic-never-output"));
    }
}

#[test]
fn malformed_headers_and_encoding_errors_are_fixed() {
    let dir = directory();
    let capture = BodyCapture::new(dir.path().to_owned(), 1).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-synthetic",
        axum::http::HeaderValue::from_bytes(&[255]).unwrap(),
    );
    assert_eq!(
        capture
            .record("/v1/responses", &headers, "account", b"{}")
            .unwrap_err()
            .to_string(),
        "capture header is not text"
    );
    let mut saved = CaptureFile {
        route: "/v1/responses".to_owned(),
        headers: vec![(
            "invalid header secret".to_owned(),
            "synthetic-secret".to_owned(),
        )],
        account_id: "account".to_owned(),
        raw_body_base64: STANDARD.encode(b"{}"),
        decoded_body: None,
    };
    let path = dir.path().join("malformed.json");
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    assert_eq!(
        prepare(&path, Provider::Codex, StripMode::None, false)
            .err()
            .unwrap()
            .to_string(),
        "invalid capture header"
    );
    saved.headers = vec![(
        "content-encoding".to_owned(),
        "synthetic-unsupported".to_owned(),
    )];
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    assert_eq!(
        prepare(&path, Provider::Codex, StripMode::All, true)
            .err()
            .unwrap()
            .to_string(),
        "unsupported content encoding"
    );
    saved.headers.clear();
    saved.raw_body_base64 = "synthetic-invalid-base64!".to_owned();
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    assert_eq!(
        prepare(&path, Provider::Codex, StripMode::None, false)
            .err()
            .unwrap()
            .to_string(),
        "invalid capture body encoding"
    );
}

#[tokio::test]
async fn error_identifiers_are_bounded_ascii_and_hide_token_patterns() {
    let forbidden = vec!["synthetic-access".to_owned()];
    let long = "identifier_".repeat(20);
    for (label, expected) in [
        (
            "invalid_request.error-401",
            Some("invalid_request.error-401".to_owned()),
        ),
        (long.as_str(), Some(long[..100].to_owned())),
        ("arbitrary conversation content", None),
        ("비밀내용", None),
        ("<synthetic-content>", None),
        ("synthetic-access", None),
        ("invalid_sk-synthetic", None),
        ("invalid_eyJsynthetic", None),
        ("Bearer synthetic-opaque-token", None),
        ("bearer", None),
    ] {
        let response = Response::new(Body::from(serde_json::to_vec(&json!({"error":{
            "type":label,"code":label,"message":"Bearer synthetic-opaque-token sk-synthetic-token eyJsynthetic-token synthetic-access"
        }})).unwrap()));
        let summary = summarize(response, "account", &forbidden).await.unwrap();
        for key in ["type", "code"] {
            assert_eq!(
                summary["error"][key],
                expected.clone().map(Value::String).unwrap_or(Value::Null)
            );
            if let Some(text) = summary["error"][key].as_str() {
                assert!(text.is_ascii() && text.len() <= 100);
            }
        }
        let message = summary["error"]["message"].as_str().unwrap();
        for secret in [
            "synthetic-opaque-token",
            "sk-synthetic-token",
            "eyJsynthetic-token",
            "synthetic-access",
        ] {
            assert!(!message.contains(secret));
        }
        assert_eq!(summary["account"], "account");
    }
}

#[tokio::test]
async fn brotli_large_window_markers_are_rejected_before_decoding() {
    let dir = directory();
    let mut headers = HeaderMap::new();
    headers.insert("content-encoding", "br".parse().unwrap());
    for raw in [&[0x11][..], &[0x11, 30, 0, 0][..], &[0x91, 30, 0, 0][..]] {
        assert_eq!(
            decode(&headers, raw).unwrap_err().to_string(),
            "brotli large windows are unsupported"
        );
        let path = BodyCapture::new(dir.path().to_owned(), 1)
            .unwrap()
            .record("/v1/responses", &headers, "account", raw)
            .unwrap()
            .unwrap();
        assert_eq!(
            prepare(&path, Provider::Codex, StripMode::None, false)
                .err()
                .unwrap()
                .to_string(),
            "brotli large windows are unsupported"
        );
        let mut response = Response::new(Body::from(raw.to_vec()));
        *response.headers_mut() = headers.clone();
        assert_eq!(
            summarize(response, "account", &[])
                .await
                .unwrap_err()
                .to_string(),
            "brotli large windows are unsupported"
        );
    }
    let valid = encode(&headers, br#"{"input":[]}"#).unwrap();
    assert_eq!(decode(&headers, &valid).unwrap(), br#"{"input":[]}"#);
}

#[tokio::test]
async fn summary_redacts_secrets_and_echoed_conversation_for_both_encodings() {
    let dir = directory();
    let captured = json!({"input":[{"type":"reasoning","encrypted_content":"encrypted-synthetic-secret"}],"instructions":"conversation-synthetic-secret"});
    let (path, _) = capture(dir.path(), "/v1/responses", &captured, false);
    let mut forbidden = prepare(&path, Provider::Codex, StripMode::None, false)
        .unwrap()
        .forbidden;
    forbidden.extend(
        [
            "synthetic-access",
            "synthetic-refresh",
            "synthetic-id",
            "synthetic-local-key",
        ]
        .map(str::to_owned),
    );
    let message = format!(
        "synthetic-access synthetic-refresh synthetic-id synthetic-local-key encrypted-synthetic-secret conversation-synthetic-secret {} {}",
        captured,
        "가".repeat(250)
    );
    for zstd in [false, true] {
        let body = serde_json::to_vec(&json!({"error":{"type":"synthetic-access","code":"synthetic-local-key","message":message},"usage":{"input_tokens":3,"output_tokens":999},"content":"never-print-content"})).unwrap();
        let raw = if zstd {
            zstd::stream::encode_all(body.as_slice(), 0).unwrap()
        } else {
            body
        };
        let mut response = Response::new(Body::from(raw));
        *response.status_mut() = axum::http::StatusCode::BAD_REQUEST;
        if zstd {
            response
                .headers_mut()
                .insert("content-encoding", "zstd".parse().unwrap());
        }
        let summary = summarize(response, "synthetic-account", &forbidden)
            .await
            .unwrap();
        let output = serde_json::to_string(&summary).unwrap();
        for secret in forbidden
            .iter()
            .chain(["never-print-content".to_owned()].iter())
        {
            assert!(!output.contains(secret));
        }
        assert_eq!(summary["status"], 400);
        assert_eq!(summary["usage"], json!({"input_tokens":3}));
        assert!(
            summary["error"]["message"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 200
        );
    }
}

#[tokio::test]
async fn summary_sse_usage_only_and_stream_failure_is_sanitized() {
    for zstd in [false, true] {
        for (sse, expected) in [
            (
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"synthetic-private-output\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":13,\"input_tokens_details\":{\"cached_tokens\":7},\"output_tokens\":29}}}\n\ndata: [DONE]\n\n",
                json!({"input_tokens":13,"input_tokens_details":{"cached_tokens":7}}),
            ),
            (
                "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":1}}}\r\n\r\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\r\n\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n",
                json!({"input_tokens":4,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}),
            ),
        ] {
            let raw = if zstd {
                zstd::stream::encode_all(sse.as_bytes(), 0).unwrap()
            } else {
                sse.as_bytes().to_vec()
            };
            let chunks: Vec<Result<Bytes, std::io::Error>> = raw
                .chunks(7)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            let mut response = Response::new(Body::from_stream(futures_util::stream::iter(chunks)));
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            if zstd {
                response
                    .headers_mut()
                    .insert("content-encoding", "zstd".parse().unwrap());
            }
            let summary = summarize(response, "account", &[]).await.unwrap();
            assert_eq!(summary["usage"], expected);
            assert!(!summary.to_string().contains("synthetic-private-output"));
        }
    }
    let chunks = vec![Err::<Bytes, _>(std::io::Error::other(
        "synthetic-secret-stream-error",
    ))];
    let response = Response::new(Body::from_stream(futures_util::stream::iter(chunks)));
    assert_eq!(
        summarize(response, "account", &[])
            .await
            .unwrap_err()
            .to_string(),
        "replay response read failed"
    );
}

//! Current full account contract through actual AdminClient; synthetic only.
use qb_tray::client::{AdminClient, CoreHealth, TrayRead, TraySnapshot};
use serde_json::{Value, json};

fn account(source: Value) -> Value {
    let mut account = json!({
        "outside_used":true,"pool_eligible":false,"takeover_pending":true,
        "quota_cooling":{"window":"7d","reset_at":"2030-01-02T00:00:00Z","started_at":"2030-01-01T00:00:00Z"},
        "quota_credit_releasable":false,"observed_at":"2030-01-01T00:00:00Z",
        "quota_notice":{"action":"exhausted","window":"7d","reset_at":null,
            "next_account":null,"earliest_account":null,"observed_at":"2030-01-01T00:00:00Z","model_limited":true},
        "credits_in_use":false,
        "usage_forecast":{
            "as_of":"2030-01-01T00:00:00Z","observed_at":null,
            "windows":[{
                "name":"7d","window_seconds":604800,"observed_at":"2030-01-01T00:00:00Z",
                "used_pct":37.5,"reset_at":"2030-01-02T00:00:00Z","awaiting_usage":false,
                "recent_pct_per_hour":1.5,"average_pct_per_hour":1.0,
                "recent_span_start":"2029-12-31T23:30:00Z","measurement_start":"2029-12-31T22:00:00Z",
                "basis":"recent","exhaustion_at":null,"seconds_to_exhaustion":null,
                "outcome":"no_exhaustion_before_reset","changing":true,
                "secondary_recent":{"pct_per_hour":1.5,"measurement_start":"2029-12-31T23:30:00Z",
                    "exhaustion_at":null,"seconds_to_exhaustion":null,"outcome":"no_finite_forecast","changing":false},
                "truncated":true
            }],
            "earliest_window":null,
            "credit":{"observed_at":"2030-01-01T00:00:00Z","balance":12.5,
                "recent_credits_per_hour":null,"measurement_start":null,"exhaustion_at":null,
                "seconds_to_exhaustion":null,"outcome":"measuring","changing":false,"truncated":false},
            "truncated":true
        },
        "has_alternative":false,"id":"sample-acct-integration","generation":9,"last_refresh_at":null,
        "provider":"codex","label":"sample-acct-integration","email":"sample-acct-integration@example.test","plan":null,
        "user_state":"active",
        "source":{"kind":"cpa","mode":"linked","dir":"synthetic-source",
            "match":{"account_id":"sample-acct-origin","email":"sample-acct-integration@example.test"}},
        "health":{"kind":"cooling","reason":"auth","until":"2030-01-01T00:00:00Z","scope":"account","probe_pending":true},
        "probe_pending":true,
        "pacing":{"backoff_until":null,"step":2,"release_interval_ms":500,"waiting":3},
        "windows":[{"name":"7d","used_pct":37.5,"reset_at":null,"observed_at":null,"status":null}],
        "credits":{"has_credits":null,"unlimited":false,"balance":12.5},
        "last_used_at":null,"bound_sessions":4,"token_expires_at":null,"drain":false,
        "allow_credits":true,"refresh_attention":true,"active_sessions":2,"spread_threshold":3
    });
    account["source"] = source;
    account
}
fn wire(source: Value) -> Value {
    json!({
        "pools":{"claude":{"eligible":0,"cooling":{},"unavailable":0},
            "codex":{"eligible":0,"cooling":{},"unavailable":1}},
        "warnings":[],"draining":false,
        "capabilities":{"account_adopt":true,"manual_refresh":true,"account_takeover":true},
        "accounts":[account(source)],
        "inflight":{"requests":0,"bytes":0},"memory":{"private_bytes":0},
        "bindings":{"count":4},"data_port":12345,"version":"synthetic"
    })
}

#[test]
fn typed_account_keeps_source_objects_and_presentation_keeps_incumbent_tags() {
    for (source, tag) in [
        (json!({"kind":"oauth"}), "oauth"),
        (
            json!({"kind":"takeover","from":"synthetic-origin","at":"2030-01-01T00:00:00Z"}),
            "oauth",
        ),
        (
            json!({"kind":"cpa","mode":"linked","dir":"synthetic-source","match":{
            "account_id":"sample-acct-origin","email":"sample-acct-integration@example.test"}}),
            "linked",
        ),
        (
            json!({"kind":"cpa","mode":"adopted","dir":"synthetic-source","match":{
            "account_uuid":"sample-acct-uuid","email":"sample-acct-integration@example.test"}}),
            "adopted",
        ),
    ] {
        let original = wire(source.clone());
        let raw: TraySnapshot =
            serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&raw).unwrap(), original);
        assert_eq!(
            serde_json::to_vec(&raw).unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
        let presentation = raw.presentation().unwrap();
        assert_eq!(presentation.accounts[0].source, tag);
        assert_eq!(presentation.accounts[0].id, "sample-acct-integration");
        assert_eq!(raw.accounts[0].generation, 9);
        assert!(raw.accounts[0].takeover_pending);
        // Deriving presentation must never mutate or replace the wire object.
        assert_eq!(
            serde_json::to_value(&raw).unwrap()["accounts"][0]["source"],
            source
        );
    }
}

#[test]
fn raw_account_required_members_and_duplicates_are_not_legacy_presentation_defaults() {
    let mut missing = wire(json!({"kind":"oauth"}));
    missing["accounts"][0]
        .as_object_mut()
        .unwrap()
        .remove("generation");
    assert!(serde_json::from_value::<TraySnapshot>(missing).is_err());
    assert!(serde_json::from_str::<TraySnapshot>(r#"{"accounts":[],"accounts":[]}"#).is_err());
    let complete = serde_json::to_string(&wire(json!({"kind":"oauth"}))).unwrap();
    let duplicate = complete.replacen(r#""generation":9"#, r#""generation":9,"generation":10"#, 1);
    assert_ne!(duplicate, complete);
    assert!(serde_json::from_str::<TraySnapshot>(&duplicate).is_err());
}

#[tokio::test]
async fn actual_admin_client_preserves_complete_account_before_layout_projection() {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::time::{Duration, Instant};
    let original = wire(
        json!({"kind":"cpa","mode":"adopted","dir":"synthetic-source",
        "match":{"account_id":"sample-acct-origin","email":"sample-acct-integration@example.test"}}),
    );
    let bytes = serde_json::to_vec(&original).unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "synthetic client did not connect"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("synthetic accept: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            request.push(byte[0]);
            assert!(request.len() < 16 * 1024);
        }
        assert!(
            std::str::from_utf8(&request)
                .unwrap()
                .starts_with("GET /admin/v1/state HTTP/1.1\r\n")
        );
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
        stream.write_all(&bytes).unwrap();
    });
    let client = AdminClient::new(port, "synthetic-admin").unwrap();
    let raw = tokio::time::timeout(Duration::from_secs(6), client.snapshot())
        .await
        .unwrap()
        .unwrap();
    server.join().unwrap();
    assert_eq!(serde_json::to_value(&raw).unwrap(), original);
    assert_eq!(raw.presentation().unwrap().accounts[0].source, "adopted");
    let read = TrayRead {
        snapshot: raw,
        health: CoreHealth::Unreachable {
            reason: "synthetic".into(),
        },
    };
    let now: std::time::SystemTime = "2030-01-01T00:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap()
        .into();
    let rows = qb_tray::client::layout_rows(&read, now).unwrap();
    assert!(!rows.is_empty());
    let projected = qb_tray::v2::project(rows[0].view_input.as_deref().unwrap(), now).unwrap();
    assert!(!projected.sections[1].rows[0].raw.source.linked());
    assert!(projected.sections[1].rows[0].raw.source.owned_or_adopted());
    assert_eq!(serde_json::to_value(&read.snapshot).unwrap(), original);
}

#[test]
fn takeover_layout_keeps_refresh_actions_and_structured_raw_source() {
    let source = json!({"kind":"takeover","from":"sample-acct-origin",
        "at":"2030-01-01T00:00:00Z"});
    let mut original = wire(source.clone());
    let account = &mut original["accounts"][0];
    account["health"] = json!({"kind":"ready"});
    account["probe_pending"] = json!(false);
    account["takeover_pending"] = json!(false);
    account["pool_eligible"] = json!(true);
    account["quota_cooling"] = Value::Null;
    account["quota_notice"] = Value::Null;
    original["pools"]["codex"]["eligible"] = json!(1);
    original["pools"]["codex"]["unavailable"] = json!(0);
    let read = TrayRead {
        snapshot: serde_json::from_value::<TraySnapshot>(original.clone()).unwrap(),
        health: CoreHealth::Unreachable {
            reason: "synthetic".into(),
        },
    };
    let now = "2030-01-01T00:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap()
        .into();
    let rows = qb_tray::client::layout_rows(&read, now).unwrap();
    let row = rows
        .iter()
        .find(|row| row.id == "sample-acct-integration")
        .unwrap();
    assert_eq!(row.source, "oauth");
    assert!(qb_tray::actions::menu_items(row).contains(&qb_tray::actions::MenuItem::RefreshTokens));
    assert_eq!(
        qb_tray::account_menu_metadata(row)[0],
        qb_tray::i18n::static_text("직접 갱신")
    );
    let input = row.view_input.as_deref().unwrap();
    let raw_input: Value = serde_json::from_str(input).unwrap();
    assert_eq!(raw_input["accounts"][0]["source"], source);
    assert_eq!(raw_input["accounts"][0]["generation"], 9);
    assert!(
        !raw_input["accounts"][0]["takeover_pending"]
            .as_bool()
            .unwrap()
    );
    assert!(
        raw_input["accounts"][0]
            .get("last_relevant_state_at")
            .is_none()
    );
    let projected = qb_tray::v2::project(input, now).unwrap();
    let owned = &projected.sections[1].rows[0].raw.source;
    assert!(owned.owned_or_adopted());
    match owned {
        qb_tray::v2::Source::Object(object) => {
            assert_eq!(object.kind, "takeover");
            assert_eq!(object.extra.get("from"), Some(&json!("sample-acct-origin")));
            assert_eq!(object.extra.get("at"), Some(&json!("2030-01-01T00:00:00Z")));
        }
        other => panic!("lost structured source: {other:?}"),
    }
    assert_eq!(serde_json::to_value(&read.snapshot).unwrap(), original);
}

#[test]
fn typed_warning_pool_facts_survive_raw_to_presentation_without_invented_counts() {
    let mut original = wire(json!({"kind":"oauth"}));
    original["warnings"] = json!([
        {"kind":"burst","burst_new_sessions":33,"observed_at":"2030-01-01T00:00:00Z"},
        {"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z",
            "reread_tokens":100001,"known_count":2,"unknown_count":1},
        {"kind":"move_breaker","observed_at":"2030-01-01T00:00:00Z",
            "move_list":[{"from":"sample-acct-a","to":"sample-acct-b"}],
            "until":"2030-01-01T00:04:00Z","session":"0123456789ab",
            "from":"sample-acct-a","to":"sample-acct-b","reason":"global_trip","forced":true}
    ]);
    let raw: TraySnapshot =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    let presentation = raw.presentation().unwrap();
    assert_eq!(
        serde_json::to_vec(&raw).unwrap(),
        serde_json::to_vec(&original).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&presentation.warnings).unwrap(),
        original["warnings"]
    );
    assert_eq!(
        serde_json::to_value(&presentation.pools).unwrap(),
        original["pools"]
    );
    for pools in [
        Value::Null,
        json!({"codex":{"eligible":2}}),
        json!({"codex":{"cooling":{"rate":1,"sample-acct-future":2}}}),
    ] {
        let mut legacy = original.clone();
        legacy["pools"] = pools;
        let raw: TraySnapshot =
            serde_json::from_slice(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        let presentation = raw.presentation().unwrap();
        assert_eq!(raw.pools, presentation.pools);
        if let Some(pools) = &raw.pools {
            assert!(pools.claude.is_none());
            let codex = pools.codex.as_ref().unwrap();
            assert!(codex.unavailable.is_none());
            if let Some(cooling) = &codex.cooling {
                assert_eq!(cooling.get("sample-acct-future"), Some(2));
                assert!(codex.eligible.is_none());
            } else {
                assert_eq!(codex.eligible, Some(2));
            }
        } else {
            assert!(presentation.pools.is_none());
        }
        assert_eq!(
            serde_json::to_value(&raw.warnings).unwrap(),
            original["warnings"]
        );
    }
}

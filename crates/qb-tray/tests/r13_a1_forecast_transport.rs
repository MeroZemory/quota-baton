//! Exercise the production HTTP DTO before projecting either dashboard tab.
#![allow(dead_code, clippy::field_reassign_with_default)]
#[path = "../src/client.rs"]
mod client;
#[path = "support/r13_usage_forecast_fixture.rs"]
mod fixture;

use qb_tray::{
    dashboard_model as m,
    geometry::{Draw, Metrics, Scene},
    state::{DashboardState, Tab},
    v2,
};
#[path = "support/r15_admin_account_fixture.rs"]
mod admin_fixture;

use serde_json::json;

#[tokio::test]
async fn r13_a1_http_forecast_survives_poll_dto_flyout_and_both_dashboard_tabs() {
    use std::io::{Read, Write};
    let (clock, _) = fixture::fixture();
    let forecast = fixture::forecast(clock.utc(), false);
    let summary = qb_core::usage_forecast_display::summary(&forecast, false);
    let expected = vec![summary.text.clone()];
    let detail = qb_core::usage_forecast_display::lines(&forecast, false).join("\n");
    assert!(!summary.text.is_empty());
    assert!(!detail.is_empty());
    let mut codex = admin_fixture::account("cx-a1", "codex", json!(forecast));
    codex["label"] = json!("합성 Codex");
    codex["windows"] = json!([{"name":"5h","used_pct":70.,
        "reset_at":clock.utc()+chrono::Duration::hours(4),"observed_at":clock.utc(),"status":null}]);
    let mut claude = admin_fixture::account("cl-a1", "claude", json!(forecast));
    claude["label"] = json!("합성 Claude");
    claude["source"] = json!({"kind":"cpa","mode":"adopted","dir":"synthetic-source",
        "match":{"account_uuid":"synthetic-claude-origin","email":"cl-a1@example.invalid"}});
    claude["windows"] = codex["windows"].clone();
    let payload = json!({"version":"synthetic-admin", "accounts":[codex, claude]});
    // Live polling uses /state, whose account entries use the same view as
    // /accounts. Do not inject v2 directly: the DTO round trip was the bug.
    let wire = serde_json::to_vec(&payload).unwrap();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(![10100, 10101].contains(&port));
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            headers.push(byte[0]);
            assert!(headers.len() < 16 * 1024);
        }
        let headers = String::from_utf8(headers).unwrap();
        assert!(headers.starts_with("GET /admin/v1/state HTTP/1.1\r\n"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer synthetic-a1")
        );
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",wire.len()).unwrap();
        stream.write_all(&wire).unwrap();
    });
    let snapshot = client::AdminClient::new(port, "synthetic-a1")
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    server.join().unwrap();
    let transported = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(
        transported["accounts"][0]["usage_forecast"],
        payload["accounts"][0]["usage_forecast"]
    );
    let now = clock.utc().into();
    let read = client::TrayRead {
        snapshot,
        health: client::CoreHealth::Unreachable {
            reason: "synthetic".into(),
        },
    };
    let rows = client::layout_rows(&read, now).unwrap();
    let accounts = v2::project(rows[0].view_input.as_deref().unwrap(), now).unwrap();
    let dashboard_accounts =
        v2::project(&serde_json::to_string(&read.snapshot).unwrap(), now).unwrap();
    for row in dashboard_accounts.sections.iter().flat_map(|s| &s.rows) {
        assert_eq!(row.forecast_lines, expected, "dashboard poll lost forecast");
        assert_eq!(row.forecast_tooltip, detail, "dashboard poll lost detail");
        assert_eq!(row.forecast_warning, summary.warning);
        assert_eq!(row.raw.usage_forecast.as_ref(), Some(&forecast));
    }
    for row in accounts.sections.iter().flat_map(|s| &s.rows) {
        assert_eq!(row.forecast_lines, expected, "flyout model lost forecast");
        assert_eq!(row.forecast_tooltip, detail, "flyout model lost detail");
        assert_eq!(row.forecast_warning, summary.warning);
        assert_eq!(row.raw.usage_forecast.as_ref(), Some(&forecast));
    }
    let mut data = m::DashboardSnapshot::new(dashboard_accounts);
    data.sessions = serde_json::from_value(json!({"sessions":[
        {"id":"0123456789ab","provider":"codex","account":"cx-a1","active":true,"last_used_at":clock.utc()},
        {"id":"abcdef012345","provider":"claude","account":"cl-a1","active":true,"last_used_at":clock.utc()}
    ]})).unwrap();
    for tab in [Tab::Overview, Tab::Usage] {
        let mut state = DashboardState::default();
        state.tab = tab;
        let view = m::project(&data, &mut state, &clock);
        for id in ["cx-a1", "cl-a1"] {
            let group = view.groups.iter().find(|g| g.id == id).unwrap();
            assert_eq!(group.forecast_lines, expected);
            assert_eq!(group.forecast_tooltip, detail);
            assert_eq!(group.forecast_warning, summary.warning);
            let tile = view.usage.iter().find(|t| t.id == id).unwrap();
            assert_eq!(tile.forecast_lines, expected);
            assert_eq!(tile.forecast_tooltip, detail);
            assert_eq!(tile.forecast_warning, summary.warning);
            assert!(!tile.projections.is_empty());
        }
        let scene = Scene::build(&view, &state, [1600., 1800.], &Metrics::default());
        assert!(scene.tooltips.iter().any(|tip| tip.text == detail));
        for line in &expected {
            assert!(
                scene
                    .draw
                    .iter()
                    .any(|d| matches!(d,Draw::Text{measure,..} if measure.text==*line)),
                "{tab:?} did not draw {line}"
            );
        }
    }
}

#[test]
fn r13_a1_archived_missing_and_null_forecast_keep_presentation_compatible() {
    for extra in [json!({}), json!({"usage_forecast":null})] {
        let mut account = json!({"id":"legacy","provider":"codex","user_state":"active","health":{"kind":"ready"}});
        account
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        // Archived presentation compatibility is separate from the current live DTO.
        let projected = v2::project(
            &serde_json::to_string(&json!({"accounts":[account]})).unwrap(),
            std::time::SystemTime::now(),
        )
        .unwrap();
        let row = &projected.sections[1].rows[0];
        assert!(row.raw.usage_forecast.is_none());
        assert_eq!(row.forecast_lines, ["측정 중"]);
        assert_eq!(row.forecast_tooltip, "측정 중");
        assert!(!row.forecast_warning);
    }
}

#[test]
fn r15_live_complete_account_keeps_required_keys_and_rejects_malformed_fields() {
    let account = admin_fixture::account(
        "live-contract",
        "codex",
        admin_fixture::measuring_forecast(),
    );
    let wire = json!({"accounts":[account.clone()]});
    let decoded: client::TraySnapshot = serde_json::from_value(wire).unwrap();
    assert_eq!(
        serde_json::to_value(decoded).unwrap()["accounts"][0],
        account
    );
    assert_eq!(account.as_object().unwrap().len(), 32);
    // A missing required nullable member must not silently become null.
    for key in account.as_object().unwrap().keys() {
        let mut missing = account.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<client::TraySnapshot>(json!({"accounts":[missing]})).is_err(),
            "accepted missing live account key {key}"
        );
    }
    for (key, invalid) in [
        ("usage_forecast", json!(null)),
        ("generation", json!(null)),
        ("takeover_pending", json!("false")),
        ("source", json!({"kind":"cpa","mode":"adopted"})),
        ("pacing", json!({"waiting":0})),
    ] {
        let mut malformed = account.clone();
        malformed[key] = invalid;
        assert!(
            serde_json::from_value::<client::TraySnapshot>(json!({"accounts":[malformed]}))
                .is_err(),
            "accepted malformed live account key {key}"
        );
    }
}

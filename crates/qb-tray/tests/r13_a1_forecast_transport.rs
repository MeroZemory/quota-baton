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
use serde_json::json;

#[tokio::test]
async fn r13_a1_http_forecast_survives_poll_dto_flyout_and_both_dashboard_tabs() {
    use std::io::{Read, Write};
    let (clock, _) = fixture::fixture();
    let forecast = fixture::forecast(clock.utc(), false);
    let expected = qb_core::usage_forecast_display::lines(&forecast, false);
    assert!(!expected.is_empty());
    let payload = json!({"version":"synthetic-admin", "accounts":[
        {"id":"cx-a1","provider":"codex","label":"합성 Codex","user_state":"active",
         "health":{"kind":"ready"},"source":{"kind":"oauth"},
         "windows":[{"name":"5h","used_pct":70.,"reset_at":clock.utc()+chrono::Duration::hours(4)}],
         "usage_forecast":forecast},
        {"id":"cl-a1","provider":"claude","label":"합성 Claude","user_state":"active",
         "health":{"kind":"ready"},"source":{"kind":"cpa","mode":"adopted"},
         "windows":[{"name":"5h","used_pct":70.,"reset_at":clock.utc()+chrono::Duration::hours(4)}],
         "usage_forecast":forecast}
    ]});
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
        assert_eq!(row.raw.usage_forecast.as_ref(), Some(&forecast));
    }
    for row in accounts.sections.iter().flat_map(|s| &s.rows) {
        assert_eq!(row.forecast_lines, expected, "flyout model lost forecast");
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
            assert_eq!(
                view.groups
                    .iter()
                    .find(|g| g.id == id)
                    .unwrap()
                    .forecast_lines,
                expected
            );
            let tile = view.usage.iter().find(|t| t.id == id).unwrap();
            assert_eq!(tile.forecast_lines, expected);
            assert!(!tile.projections.is_empty());
        }
        let scene = Scene::build(&view, &state, [1600., 1800.], &Metrics::default());
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
fn r13_a1_missing_and_null_forecast_keep_legacy_admin_payload_compatible() {
    for extra in [json!({}), json!({"usage_forecast":null})] {
        let mut account = json!({"id":"legacy","provider":"codex","user_state":"active","health":{"kind":"ready"}});
        account
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let snapshot: client::TraySnapshot =
            serde_json::from_value(json!({"accounts":[account]})).unwrap();
        let projected = v2::project(
            &serde_json::to_string(&snapshot).unwrap(),
            std::time::SystemTime::now(),
        )
        .unwrap();
        let row = &projected.sections[1].rows[0];
        assert!(row.raw.usage_forecast.is_none());
        assert!(row.forecast_lines.is_empty());
    }
}

//! Actual admin DTO transport -> snapshot projection -> production renderer.
#![cfg(windows)]
use chrono::{DateTime, Duration, FixedOffset, Utc};
use qb_tray::{
    client::{self, snapshot},
    dashboard_model, geometry,
    i18n::{self, UiLanguage},
    snapshot_render::{self, Options, View},
    state::{DashboardState, Tab},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    path::PathBuf,
};

const AT: &str = "2026-10-23T03:12:08Z";
fn now() -> DateTime<Utc> {
    AT.parse().unwrap()
}

fn bundle() -> Value {
    let mut accounts = vec![];
    let mut history = vec![];
    for (id, provider, plan, state, drain, used, hours) in [
        ("sample-a", "codex", "plus", "active", false, 20., 60),
        ("sample-b", "codex", "pro", "active", false, 40., 96),
        ("sample-free", "codex", "free", "active", false, 60., 360),
        ("sample-paused", "codex", "plus", "paused", false, 90., 60),
        (
            "sample-disabled",
            "codex",
            "plus",
            "disabled",
            false,
            90.,
            60,
        ),
        ("sample-drain", "codex", "plus", "active", true, 90., 60),
        ("sample-claude", "claude", "pro", "active", false, 30., 36),
    ] {
        let name = if plan == "free" { "43200m" } else { "7d" };
        let reset = now() + Duration::hours(hours);
        let observed = now() - Duration::seconds(60);
        let mut forecasts = qb_core::usage_forecast::Forecasts::default();
        forecasts.observe(id, name, used, Some(reset), observed);
        accounts.push(json!({"id":id,"provider":provider,"email":format!("{id}@example.test"),"plan":plan,"user_state":state,"drain":drain,"health":{"kind":"ready"},"source":"oauth","pool_eligible":state=="active"&&!drain,
            "windows":[{"name":name,"used_pct":used,"reset_at":reset,"observed_at":observed}],
            "usage_forecast":forecasts.view(id,now(),None)}));
        history.push(json!({"id":id,"windows":[{"name":name,"points":[[now()-Duration::minutes(3),used-2.],[observed,used]],"resets":[]}]}));
    }
    json!({"schema_version":1,"capture_started_at":AT,"capture_completed_at":AT,"utc_offset_seconds":32400,"consistency":"independent_reads","session_details":"not_captured",
        "query":{"sessions_range":"all","activity_days":7,"activity_limit":2000,"usage_hours":48},
        "health":{"ok":true,"version":"synthetic-r163","pid":7,"started_at":AT},
        "state":{"version":"synthetic-r163","accounts":accounts,"pools":{"codex":{"eligible":3},"claude":{"eligible":1}}},
        "sessions":{"now":AT,"sessions":[]},"activity":{"events":[],"next_seq":0},"usage_history":{"accounts":history}})
}

async fn transport(wire: &Value) -> snapshot::SnapshotBundleV1 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(![10100, 10101].contains(&port));
    listener.set_nonblocking(true).unwrap();
    let value = wire.clone();
    let worker = std::thread::spawn(move || {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(12);
        for _ in 0..5 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < end);
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(e) => panic!("synthetic accept: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut ch = [0];
                assert_eq!(stream.read(&mut ch).unwrap(), 1);
                request.push(ch[0]);
                assert!(request.len() < 16384);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(
                request
                    .to_lowercase()
                    .contains("authorization: bearer synthetic-r163-admin")
            );
            let path = request.split_whitespace().nth(1).unwrap();
            let key = match path.split('?').next().unwrap() {
                "/admin/v1/health" => "health",
                "/admin/v1/state" => "state",
                "/admin/v1/sessions" => "sessions",
                "/admin/v1/activity" => "activity",
                "/admin/v1/usage-history" => "usage_history",
                _ => panic!("unexpected synthetic route"),
            };
            let body = serde_json::to_vec(&value[key]).unwrap();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    let client = client::AdminClient::new(port, "synthetic-r163-admin").unwrap();
    let mut captured = snapshot::capture(&client).await.unwrap();
    worker.join().unwrap();
    // Freeze capture time, not source observation times, for deterministic renders.
    captured.capture_started_at = now();
    captured.capture_completed_at = now();
    captured.utc_offset_seconds = 32400;
    snapshot::decode(&snapshot::encode(&captured).unwrap()).unwrap()
}

#[tokio::test]
async fn r163_real_dto_capture_retains_provider_values_and_measured_layouts() {
    let captured = transport(&bundle()).await;
    let out = std::env::var_os("R163_RENDER_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r163/png")
        });
    std::fs::create_dir_all(&out).unwrap();
    for lang in [UiLanguage::Ko, UiLanguage::En] {
        i18n::with_language(lang, || {
            let data = captured.project(now().into()).unwrap();
            let clock = dashboard_model::Clock {
                now: now().with_timezone(&FixedOffset::east_opt(32400).unwrap()),
                day_start: now() - Duration::hours(12),
                day_end: now() + Duration::hours(12),
            };
            let mut state = DashboardState::default();
            state.tab = Tab::Usage;
            let view = dashboard_model::project(&data, &mut state, &clock);
            let codex = &view.provider_usage[1];
            assert_eq!(
                (
                    codex.cohort,
                    codex.excluded_paused,
                    codex.excluded_disabled,
                    codex.excluded_other
                ),
                (3, 1, 1, 1)
            );
            assert!((codex.current.remaining.value.unwrap() - 1.8).abs() < 1e-9);
            assert_eq!((codex.weekly, codex.free_30_day), (2, 1));
            for size in [[1024, 640], [1320, 900]] {
                for dark in [false, true] {
                    let options = Options {
                        view: View::Usage,
                        size,
                        scale: 2,
                        dark,
                        now: clock.now,
                    };
                    let png = out.join(format!(
                        "{lang:?}-{}x{}-{}.png",
                        size[0],
                        size[1],
                        if dark { "dark" } else { "light" }
                    ));
                    snapshot_render::save(&data, &options, &png).unwrap();
                    let measured = snapshot_render::measure_dashboard(&data, &options).unwrap();
                    let charts: Vec<_> = measured
                        .scene
                        .charts
                        .iter()
                        .filter(|c| c.provider_summary.is_some())
                        .collect();
                    assert_eq!(charts.len(), 2);
                    for chart in charts {
                        assert!(!chart.provider_summary.as_ref().unwrap().contains('%'));
                        assert_eq!(
                            (chart.from, chart.to),
                            dashboard_model::usage_domain(&clock)
                        );
                    }
                    let text: Vec<_> = measured
                        .scene
                        .draw
                        .iter()
                        .filter_map(|d| match d {
                            geometry::Draw::Text { measure, .. } => Some(measure.text.as_str()),
                            _ => None,
                        })
                        .collect();
                    assert!(
                        text.iter().any(|s| s.contains("1.8")),
                        "quantity must be visible, not hover-only"
                    );
                    assert!(text.iter().any(|s| s.contains(if lang == UiLanguage::En {
                        "not weighted by plan"
                    } else {
                        "요금제 가중치 없음"
                    })));
                }
            }
        });
    }
}

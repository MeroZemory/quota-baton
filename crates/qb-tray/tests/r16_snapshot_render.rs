//! Real client DTO decoding followed by the production WIC/DirectWrite renderer.
#![cfg(windows)]

use qb_tray::{client::snapshot, i18n, snapshot_render};
use serde_json::json;

#[path = "support/r13_usage_forecast_fixture.rs"]
#[allow(dead_code)]
mod forecast_fixture;

fn bundle() -> snapshot::SnapshotBundleV1 {
    let now = "2026-10-23T14:05:00+09:00";
    let forecast = forecast_fixture::forecast(
        chrono::DateTime::parse_from_rfc3339(now).unwrap().to_utc(),
        false,
    );
    snapshot::decode(&serde_json::to_vec(&json!({
        "schema_version":1,"capture_started_at":now,"capture_completed_at":now,
        "utc_offset_seconds":32400,"consistency":"independent_reads",
        "session_details":"not_captured",
        "query":{"sessions_range":"all","activity_days":7,"activity_limit":2000,"usage_hours":48},
        "health":{"ok":true,"version":"synthetic","pid":123,"started_at":now},
        "state":{"version":"synthetic","pools":{"codex":{"eligible":1}},"accounts":[
            {"id":"sample-acct","provider":"codex","label":"Sample account",
             "user_state":"active","source":{"kind":"oauth"},"health":{"kind":"ready"},
             "usage_forecast":forecast,
             "windows":[{"name":"5h","used_pct":24.0,"reset_at":"2026-10-23T18:00:00+09:00"},
                        {"name":"7d","used_pct":41.0,"reset_at":"2026-10-25T18:00:00+09:00"}]}
        ]},
        "sessions":{"sessions":[{
            "id":"0123456789ab","provider":"codex","account":"sample-acct",
            "label":{"text":"Sample project","source":"header"},
            "client":{"kind":"codex","product":"codex","version":"synthetic"},
            "active":true,"last_used_at":now,"model":"sample-model",
            "requests_today":12,"failures_today":0,"counters_complete":true,
            "inflight_known":true,"moves_known":true,"subagents_known":true
        }]},
        "activity":{"events":[{
            "seq":1,"ts":now,"kind":"session_move","provider":"codex",
            "account":"sample-acct","account_to":"sample-acct",
            "data":{"session":"0123456789ab","reason":"account_unavailable"}
        }]},"usage_history":{"accounts":[]}
    })).unwrap()).unwrap()
}

#[test]
fn r16_real_dto_renders_every_view_in_both_themes_and_languages() {
    let temp = tempfile::tempdir().unwrap();
    let output = std::env::var_os("R16_RENDER_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temp.path().to_owned());
    std::fs::create_dir_all(&output).unwrap();
    let bundle = bundle();
    std::fs::write(
        output.join("synthetic-snapshot.json"),
        snapshot::encode(&bundle).unwrap(),
    )
    .unwrap();
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T14:05:00+09:00").unwrap();
    for (language, code) in [(i18n::UiLanguage::Ko, "ko"), (i18n::UiLanguage::En, "en")] {
        i18n::with_language(language, || {
            let data = bundle.project(now.to_utc().into()).unwrap();
            if language == i18n::UiLanguage::En {
                let lines = &data
                    .accounts
                    .sections
                    .iter()
                    .flat_map(|s| &s.rows)
                    .next()
                    .unwrap()
                    .forecast_lines;
                assert!(!lines.is_empty());
                assert!(
                    lines
                        .iter()
                        .all(|s| !s.chars().any(|c| ('\u{ac00}'..='\u{d7a3}').contains(&c))),
                    "{lines:?}"
                );
            }
            for dark in [false, true] {
                for view in ["overview", "usage", "events", "flyout", "menu"] {
                    let file = output.join(format!(
                        "{code}-{}-{view}.png",
                        if dark { "dark" } else { "light" }
                    ));
                    snapshot_render::save(
                        &data,
                        &snapshot_render::Options {
                            view: view.parse().unwrap(),
                            size: [1280, 900],
                            scale: 1,
                            dark,
                            now,
                        },
                        &file,
                    )
                    .unwrap();
                    let bytes = std::fs::read(&file).unwrap();
                    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
                    assert!(bytes.len() > 2000, "blank or truncated {file:?}");
                    assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 1280);
                    assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 900);
                }
            }
        });
    }
}

#[test]
fn r16_offline_equals_form_failures_do_not_create_live_home_or_logs() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("unopened-home");
    for suffix in ["missing.json", "invalid.json"] {
        let input = directory.path().join(suffix);
        if suffix == "invalid.json" {
            std::fs::write(&input, b"{}").unwrap();
        }
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_quota-baton-tray"))
            .arg(format!("--render-snapshot={}", input.display()))
            .arg(format!(
                "--out={}",
                directory.path().join("output.png").display()
            ))
            .env("QB_HOME", &home)
            .env("LOCALAPPDATA", &home)
            .status()
            .unwrap();
        assert!(!status.success());
        assert!(!home.exists(), "offline error opened the live home");
        assert!(!directory.path().join("output.png").exists());
    }
}

#[test]
fn r16_snapshot_scale_two_doubles_output_pixels() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("scale2.png");
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T14:05:00+09:00").unwrap();
    let data = bundle().project(now.to_utc().into()).unwrap();
    snapshot_render::save(
        &data,
        &snapshot_render::Options {
            view: snapshot_render::View::Overview,
            size: [640, 480],
            scale: 2,
            dark: false,
            now,
        },
        &file,
    )
    .unwrap();
    let bytes = std::fs::read(file).unwrap();
    assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 1280);
    assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 960);
}

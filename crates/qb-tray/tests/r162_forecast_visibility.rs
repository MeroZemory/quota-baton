//! Synthetic HTTP -> actual AdminClient DTO -> production model/DirectWrite.
//! These tests do not substitute for the separately spawned D27 capture.
#![cfg(windows)]

#[path = "support/r13_usage_forecast_fixture.rs"]
#[allow(dead_code)]
mod forecast_fixture;

use chrono::{DateTime, Duration, FixedOffset, Local, Utc};
use qb_core::usage_forecast::{AccountUsageForecast, ForecastOutcome};
use qb_tray::{
    Rect, client, dashboard_model as model,
    geometry::{self, Draw, Face, TextAlign},
    i18n::{self, UiLanguage},
    snapshot_render::{self, Options, View},
    state::{DashboardState, Tab},
    v2, v2_geometry as flyout, v2_render, v2_text,
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    path::PathBuf,
    time::Instant,
};
use windows::Win32::Graphics::{
    Direct2D::Common::D2D_RECT_F,
    DirectWrite::{DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD},
};

const AT: &str = "2026-10-23T14:05:00Z";
const EPS: f32 = 0.01;

#[derive(Clone, Copy, Debug)]
enum Case {
    Near,
    Long,
    NoExhaustion,
    Exhausted,
    Measuring,
    ExactlyFiveHours,
    JustUnderFiveHours,
}

const VISIBLE_CASES: [Case; 5] = [
    Case::Near,
    Case::Long,
    Case::NoExhaustion,
    Case::Exhausted,
    Case::Measuring,
];

fn now() -> DateTime<Utc> {
    AT.parse().unwrap()
}

fn forecast(case: Case) -> AccountUsageForecast {
    let mut f = forecast_fixture::forecast(now(), false);
    if matches!(case, Case::Measuring) {
        f.windows.clear();
        f.earliest_window = None;
        return f;
    }
    if !matches!(case, Case::Near) {
        f.windows[0].outcome = ForecastOutcome::NoExhaustionBeforeReset;
        f.windows[0].exhaustion_at = None;
        f.windows[0].seconds_to_exhaustion = None;
        f.earliest_window = Some("7d".into());
    }
    match case {
        Case::Near | Case::Long => {}
        Case::NoExhaustion | Case::Exhausted => {
            f.windows[1].outcome = ForecastOutcome::NoExhaustionBeforeReset;
            f.windows[1].exhaustion_at = None;
            f.windows[1].seconds_to_exhaustion = None;
            f.earliest_window = None;
            if matches!(case, Case::Exhausted) {
                f.windows[0].used_pct = 100.;
                f.windows[0].outcome = ForecastOutcome::Exhausted;
                f.windows[0].exhaustion_at = Some(now());
                f.windows[0].seconds_to_exhaustion = Some(0);
            }
        }
        Case::ExactlyFiveHours | Case::JustUnderFiveHours => {
            let seconds = if matches!(case, Case::ExactlyFiveHours) {
                18_000
            } else {
                17_999
            };
            f.windows[1].seconds_to_exhaustion = Some(seconds);
            f.windows[1].exhaustion_at = Some(now() + Duration::seconds(seconds));
        }
        Case::Measuring => unreachable!(),
    }
    f
}

fn account(id: &str, provider: &str, case: Case) -> Value {
    let f = forecast(case);
    json!({
        "id":id,"provider":provider,"label":format!("Synthetic {id}"),
        "plan":"plus","user_state":"active","health":{"kind":"ready"},
        "source":{"kind":"oauth"},"pool_eligible":true,
        "eligible_for_new_session":true,"active_sessions":1,"bound_sessions":1,
        "credits_in_use":false,"allow_credits":true,
        "credits":{"has_credits":true,"balance":80.0,"unlimited":false},
        "windows":f.windows.iter().map(|w|json!({
            "name":w.name,"used_pct":w.used_pct,"reset_at":w.reset_at,
            "observed_at":w.observed_at
        })).collect::<Vec<_>>(),"observed_at":AT,"usage_forecast":f
    })
}

fn state(accounts: Vec<Value>) -> Value {
    let eligible = |provider| {
        accounts
            .iter()
            .filter(|a| a["provider"] == provider && a["pool_eligible"] == true)
            .count()
    };
    json!({"version":"synthetic-r162","pools":{
        "codex":{"eligible":eligible("codex")},
        "claude":{"eligible":eligible("claude")}
    },"accounts":accounts})
}

// A real numeric-loopback request with bounded accept/read time. A removed or
// broken AdminClient call cannot silently pass, or leave a forever-accept task.
async fn poll(payload: &Value) -> client::TraySnapshot {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(![10100, 10101].contains(&port));
    listener.set_nonblocking(true).unwrap();
    let wire = serde_json::to_vec(payload).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + std::time::Duration::from_secs(8);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "AdminClient did not request state"
                    );
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
                .contains("\r\nauthorization: bearer synthetic-r162-admin\r\n")
        );
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",wire.len()).unwrap();
        stream.write_all(&wire).unwrap();
    });
    let result = client::AdminClient::new(port, "synthetic-r162-admin")
        .unwrap()
        .snapshot()
        .await;
    server.join().expect("synthetic HTTP request assertions");
    result.expect("production AdminClient state DTO")
}

fn project(snapshot: &client::TraySnapshot) -> v2::View {
    v2::project(&serde_json::to_string(snapshot).unwrap(), now().into()).unwrap()
}

fn row<'a>(view: &'a v2::View, id: &str) -> &'a v2::AccountRow {
    view.sections
        .iter()
        .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
        .find(|r| r.raw.id == id)
        .unwrap_or_else(|| panic!("missing transported account {id}"))
}

fn compact(text: &str) {
    assert!(!text.trim().is_empty(), "forecast summary disappeared");
    assert!(!text.contains(['\n', '\r']));
    let lower = text.to_ascii_lowercase();
    for detail in ["%/", "/h", "rate", "속도", "구간 평균", "20.0", "12.0"] {
        assert!(
            !lower.contains(detail),
            "detailed rate leaked onto face: {text}"
        );
    }
}

fn account_contract(view: &v2::View, id: &str, case: Case, language: UiLanguage) {
    let r = row(view, id);
    assert_eq!(r.forecast_lines.len(), 1, "one compact account summary");
    let text = &r.forecast_lines[0];
    compact(text);
    let en = language == UiLanguage::En;
    let expected = match case {
        Case::Near => {
            if en {
                "2h 30m left"
            } else {
                "2시간 30분 남음"
            }
        }
        Case::Long => {
            if en {
                "2d 12h left"
            } else {
                "2일 12시간 남음"
            }
        }
        Case::NoExhaustion => {
            if en {
                "Lasts until reset"
            } else {
                "까지 충분"
            }
        }
        Case::Exhausted => {
            if en {
                "Out until"
            } else {
                "까지 소진"
            }
        }
        Case::Measuring => {
            if en {
                "Measuring"
            } else {
                "측정 중"
            }
        }
        Case::ExactlyFiveHours | Case::JustUnderFiveHours => {
            if en {
                "5h 0m left"
            } else {
                "5시간 0분 남음"
            }
        }
    };
    assert!(text.contains(expected), "{case:?}/{language:?}: {text}");
    assert_eq!(
        r.forecast_warning,
        matches!(
            case,
            Case::Near | Case::ExactlyFiveHours | Case::JustUnderFiveHours
        ),
        "only finite exhaustion within five hours warns: {case:?}"
    );
    assert!(!r.forecast_tooltip.is_empty(), "detail tooltip disappeared");
    if !matches!(case, Case::Measuring) {
        for detail in if en {
            ["1.0%/h", "20.0%/h", "5h", "7d"]
        } else {
            ["1.0%/시간", "20.0%/시간", "5시간", "7일"]
        } {
            assert!(
                r.forecast_tooltip.contains(detail),
                "lost tooltip {detail}: {}",
                r.forecast_tooltip
            );
        }
        if !matches!(case, Case::Exhausted) {
            let rate = if en { "12.0%/h" } else { "12.0%/시간" };
            assert!(r.forecast_tooltip.contains(rate), "short-window rate lost");
        }
    }
    if en {
        assert!(!text.chars().any(|c| matches!(c, '\u{ac00}'..='\u{d7af}')));
    }
}

#[tokio::test]
async fn r162_admin_dto_compact_forecasts_preserve_tooltips_and_inclusive_warning_boundary() {
    let cases = [
        Case::Near,
        Case::Long,
        Case::NoExhaustion,
        Case::Exhausted,
        Case::Measuring,
        Case::ExactlyFiveHours,
        Case::JustUnderFiveHours,
    ];
    let payload = state(
        cases
            .iter()
            .enumerate()
            .map(|(i, &case)| account(&format!("case-{i}"), "codex", case))
            .collect(),
    );
    let snapshot = poll(&payload).await;
    let transported = serde_json::to_value(&snapshot).unwrap();
    for (actual, expected) in transported["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .zip(payload["accounts"].as_array().unwrap())
    {
        assert_eq!(actual["usage_forecast"], expected["usage_forecast"]);
    }
    for language in [UiLanguage::Ko, UiLanguage::En] {
        i18n::with_language(language, || {
            let view = project(&snapshot);
            let read = client::TrayRead {
                snapshot: snapshot.clone(),
                health: client::CoreHealth::Unreachable {
                    reason: "synthetic transport test".into(),
                },
            };
            let rows = client::layout_rows(&read, now().into()).unwrap();
            let flyout = v2::project(rows[0].view_input.as_deref().unwrap(), now().into()).unwrap();
            for (i, &case) in cases.iter().enumerate() {
                let id = format!("case-{i}");
                account_contract(&view, &id, case, language);
                account_contract(&flyout, &id, case, language);
                assert_eq!(
                    row(&view, &id).forecast_tooltip,
                    row(&flyout, &id).forecast_tooltip
                );
            }
        });
    }
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.left + EPS >= outer.left
        && inner.top + EPS >= outer.top
        && inner.right <= outer.right + EPS
        && inner.bottom <= outer.bottom + EPS
}

fn same_rect(a: Rect, b: Rect) -> bool {
    [
        a.left - b.left,
        a.top - b.top,
        a.right - b.right,
        a.bottom - b.bottom,
    ]
    .iter()
    .all(|n| n.abs() <= EPS)
}

fn bounds(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

fn measured_fits(engine: &v2_text::Text, text: &str, rect: Rect, style: v2_text::Style) -> String {
    assert!(
        rect.right > rect.left && rect.bottom > rect.top,
        "empty forecast allocation"
    );
    let metrics = engine.layout(text, bounds(rect), style).unwrap().metrics;
    assert!(
        !metrics.trimmed,
        "trimmed summary {text:?}: {metrics:?} in {rect:?}"
    );
    assert_eq!(
        metrics.visible_lines, metrics.total_lines,
        "omitted summary lines"
    );
    assert!(
        metrics.required_height <= rect.bottom - rect.top + EPS,
        "summary height {text:?}: {metrics:?} in {rect:?}"
    );
    let mut space_style = style;
    space_style.wrap = v2_text::Wrap::SingleLine;
    let space = engine
        .layout(
            " ",
            D2D_RECT_F {
                left: 0.,
                top: 0.,
                right: 100.,
                bottom: style.line_height,
            },
            space_style,
        )
        .unwrap()
        .metrics
        .natural_width;
    assert!(
        // KeepAll retains a trailing separator after wrapping on trimmed ink.
        // Permit that one measured space only when actual ink does not overflow;
        // omitted lines, trimming and outer scroll clips are still checked.
        metrics.natural_width <= rect.right - rect.left + 0.5
            || (!metrics.ink_overflow
                && metrics.natural_width <= rect.right - rect.left + space + 0.5),
        "summary width {text:?}: {metrics:?} in {rect:?}"
    );
    format!("{text:?}: {metrics:?}, rect={rect:?}\n")
}

fn text_style(m: &geometry::Measure) -> v2_text::Style {
    v2_text::Style {
        face: match m.face {
            Face::Text => v2_text::Face::Text,
            Face::Mono => v2_text::Face::Mono,
            Face::Icon => v2_text::Face::Icon,
        },
        size: m.size,
        line_height: m.line_height,
        weight: if m.bold {
            DWRITE_FONT_WEIGHT_SEMI_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        },
        align: match m.align {
            TextAlign::Leading => v2_text::Align::Leading,
            TextAlign::Center => v2_text::Align::Center,
        },
        wrap: v2_text::Wrap::KeepAll,
    }
}

fn audit_dashboard(data: &model::DashboardSnapshot, options: &Options) -> String {
    let audit = snapshot_render::measure_dashboard(data, options).unwrap();
    let mut scene = audit.scene;
    if options.view == View::Usage {
        // Provider charts precede the account grid. Audit the account summary
        // after a real production scroll rather than requiring offscreen ink.
        let max_offset = scene
            .content
            .iter()
            .find(|r| r.region == qb_tray::state::ScrollRegion::Usage)
            .unwrap()
            .max_offset;
        let mut state = DashboardState::default();
        state.tab = Tab::Usage;
        state.usage_offset = max_offset;
        let start = options
            .now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(*options.now.offset())
            .single()
            .unwrap();
        let clock = model::Clock {
            now: options.now,
            day_start: start.to_utc(),
            day_end: (start + Duration::days(1)).to_utc(),
        };
        let view = model::project(data, &mut state, &clock);
        scene = qb_tray::render::Renderer::new(options.dark)
            .unwrap()
            .scene(&view, &state, options.size.map(|n| n as f32))
            .unwrap();
    }
    let scene = &scene;
    let engine = v2_text::Text::new().unwrap();
    let expected = &row(&data.accounts, "visible").forecast_lines[0];
    let account_nodes: Vec<_> = scene
        .accessibility
        .iter()
        .filter(|n| n.key == "chart:visible:forecast:0" || n.key.ends_with(":forecast:visible:0"))
        .collect();
    assert_eq!(
        account_nodes.len(),
        1,
        "missing/duplicated painted account forecast {:?}/{:?}: keys={:?}; sessions={:?}",
        options.view,
        options.size,
        scene
            .accessibility
            .iter()
            .map(|n| &n.key)
            .collect::<Vec<_>>(),
        data.sessions
    );
    assert_eq!(account_nodes[0].name, *expected);
    let provider_nodes: Vec<_> = scene
        .accessibility
        .iter()
        .filter(|n| n.key.starts_with("provider:") && n.key.ends_with(":forecast"))
        .collect();
    assert_eq!(provider_nodes.len(), data.accounts.summaries.len());
    for summary in &data.accounts.summaries {
        let nodes: Vec<_> = provider_nodes
            .iter()
            .filter(|n| n.key == format!("provider:{}:forecast", summary.provider))
            .collect();
        assert_eq!(nodes.len(), 1, "one real provider summary node");
        assert_eq!(nodes[0].name, summary.forecast_line);
    }
    let mut evidence = String::new();
    for node in account_nodes.into_iter().chain(provider_nodes) {
        let mut clips = vec![scene.rect];
        let mut matches = 0;
        for draw in &scene.draw {
            match draw {
                Draw::Clip(r) => {
                    let prior = *clips.last().unwrap();
                    clips.push(Rect {
                        left: prior.left.max(r.left),
                        top: prior.top.max(r.top),
                        right: prior.right.min(r.right),
                        bottom: prior.bottom.min(r.bottom),
                    });
                }
                Draw::Unclip => {
                    assert!(clips.len() > 1);
                    clips.pop();
                }
                Draw::Text { rect, measure, .. }
                    if measure.text == node.name && same_rect(*rect, node.rect) =>
                {
                    matches += 1;
                    assert!(
                        measure.wrap,
                        "forecast must use production paragraph layout"
                    );
                    assert!(
                        contains(*clips.last().unwrap(), *rect),
                        "forecast escapes actual client/scroll clip: {} {:?}",
                        node.key,
                        rect
                    );
                    evidence.push_str(&measured_fits(
                        &engine,
                        &measure.text,
                        *rect,
                        text_style(measure),
                    ));
                }
                _ => {}
            }
        }
        assert_eq!(
            matches,
            1,
            "semantic summary has no unique actual Draw::Text: {} {:?}; content={:?}; texts={:?}",
            node.key,
            node.rect,
            scene.content,
            scene
                .draw
                .iter()
                .filter_map(|d| match d {
                    Draw::Text { rect, measure, .. } if measure.text == node.name => Some(rect),
                    _ => None,
                })
                .collect::<Vec<_>>()
        );
    }
    assert!(
        scene
            .tooltips
            .iter()
            .any(|t| t.text == row(&data.accounts, "visible").forecast_tooltip),
        "paint lost detailed tooltip"
    );
    evidence
}

fn flyout_style(bold: bool) -> v2_text::Style {
    v2_text::Style {
        face: v2_text::Face::Text,
        size: 12.,
        line_height: 16.,
        weight: if bold {
            DWRITE_FONT_WEIGHT_SEMI_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        },
        align: v2_text::Align::Leading,
        wrap: v2_text::Wrap::KeepAll,
    }
}

fn audit_flyout(view: &v2::View, height: u32) -> String {
    let renderer = v2_render::Renderer::new(false).unwrap();
    let mut p = flyout::Presentation::default();
    renderer.measure(view, &mut p).unwrap();
    let scene = flyout::Scene::build(view, p.clone(), height as f32);
    let engine = v2_text::Text::new().unwrap();
    let r = row(view, "visible");
    let text = &r.forecast_lines[0];
    assert!(
        p.subtitle_lines.contains_key(text),
        "real flyout measurement omitted summary"
    );
    let row_box = scene.row("visible").expect("real flyout account box");
    let rect = row_box.forecast.expect("painted forecast rectangle");
    assert!(contains(row_box.rect, rect));
    assert!(
        contains(scene.list, rect),
        "fixture summary must actually be visible"
    );
    let mut evidence = measured_fits(&engine, text, rect, flyout_style(r.forecast_warning));
    assert_eq!(scene.header.forecasts.len(), view.summaries.len());
    for (summary, rect) in view.summaries.iter().zip(&scene.header.forecasts) {
        assert!(p.subtitle_lines.contains_key(&summary.forecast_line));
        assert!(contains(scene.header.rect, *rect));
        evidence.push_str(&measured_fits(
            &engine,
            &summary.forecast_line,
            *rect,
            flyout_style(false),
        ));
    }
    evidence
}

fn output() -> PathBuf {
    let root = std::env::var_os("R162_RENDER_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r162/visibility")
        });
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn r162_ko_en_overview_usage_flyout_summaries_fit_actual_directwrite_at_both_sizes() {
    let out = output();
    for case in VISIBLE_CASES {
        let snapshot = poll(&state(vec![account("visible", "codex", case)])).await;
        for language in [UiLanguage::Ko, UiLanguage::En] {
            i18n::with_language(language, || {
                let accounts = project(&snapshot);
                account_contract(&accounts, "visible", case, language);
                let mut data = model::DashboardSnapshot::new(accounts);
                data.sessions = serde_json::from_value(json!({"sessions":[{
                    "id":"0123456789ab","provider":"codex","account":"visible",
                    "last_used_at":AT,"first_seen_at":AT,"active":true,
                    "client":{"kind":"codex"}
                }]}))
                .unwrap();
                for size in [[1024, 640], [1320, 900]] {
                    for dark in [false, true] {
                        for view in [View::Overview, View::Usage, View::Flyout] {
                            let options = Options {
                                view,
                                size,
                                scale: 2,
                                dark,
                                now: AT.parse::<DateTime<FixedOffset>>().unwrap(),
                            };
                            let stem = format!(
                                "{case:?}-{language:?}-{view:?}-{}x{}-{}",
                                size[0],
                                size[1],
                                if dark { "dark" } else { "light" }
                            );
                            // Keep the actual rendered failure for the single batched visual review.
                            let png = out.join(format!("{stem}.png"));
                            snapshot_render::save(&data, &options, &png).unwrap();
                            let bytes = std::fs::read(png).unwrap();
                            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
                            let metrics = if view == View::Flyout {
                                audit_flyout(&data.accounts, size[1])
                            } else {
                                audit_dashboard(&data, &options)
                            };
                            std::fs::write(out.join(format!("{stem}.txt")), metrics).unwrap();
                        }
                    }
                }
            });
        }
    }
}

#[tokio::test]
async fn r162_provider_lines_keep_usable_next_recovery_and_actual_credit_evidence() {
    let mut active = account("current", "codex", Case::Long);
    let mut next = account("next", "codex", Case::Near);
    next["active_sessions"] = json!(0);
    let ready = poll(&state(vec![active.clone(), next.clone()])).await;
    active["health"] = json!({"kind":"cooling","reason":"quota","scope":"account",
        "until":now()+Duration::hours(4)});
    active["pool_eligible"] = json!(false);
    active["eligible_for_new_session"] = json!(false);
    next["health"] = json!({"kind":"cooling","reason":"quota","scope":"account",
        "until":now()+Duration::hours(2)});
    next["pool_eligible"] = json!(false);
    next["eligible_for_new_session"] = json!(false);
    let recovering = poll(&state(vec![active, next])).await;
    let mut credit = account("paid", "codex", Case::Exhausted);
    credit["credits_in_use"] = json!(true);
    let paid = poll(&state(vec![credit])).await;
    for language in [UiLanguage::Ko, UiLanguage::En] {
        i18n::with_language(language, || {
            let line = |snapshot: &client::TraySnapshot| {
                let view = project(snapshot);
                let summary = view
                    .summaries
                    .iter()
                    .find(|s| s.provider == "codex")
                    .unwrap();
                compact(&summary.forecast_line);
                summary.forecast_line.clone()
            };
            let ready_line = line(&ready);
            assert!(
                ready_line.contains('2'),
                "usable count absent: {ready_line}"
            );
            assert!(
                ready_line.contains("Synthetic next"),
                "next account absent: {ready_line}"
            );
            let recovery_line = line(&recovering);
            let recovery_clock = (now() + Duration::hours(2))
                .with_timezone(&Local)
                .format("%H:%M")
                .to_string();
            assert!(
                recovery_line.contains(&recovery_clock),
                "earliest recovery absent: {recovery_line}"
            );
            let paid_line = line(&paid);
            assert!(
                paid_line.to_ascii_lowercase().contains("credit") || paid_line.contains("크레딧"),
                "actual paid continuation absent: {paid_line}"
            );
            assert!(!row(&project(&paid), "paid").forecast_tooltip.is_empty());
            for (name, snapshot) in [
                ("ready", &ready),
                ("recovery", &recovering),
                ("credits", &paid),
            ] {
                let accounts = project(snapshot);
                let data = model::DashboardSnapshot::new(accounts);
                let midnight = now().date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
                let clock = model::Clock {
                    now: now().fixed_offset(),
                    day_start: midnight,
                    day_end: midnight + Duration::days(1),
                };
                for tab in [Tab::Overview, Tab::Usage] {
                    let mut state = DashboardState::default();
                    state.tab = tab;
                    let view = model::project(&data, &mut state, &clock);
                    for account in data.accounts.sections.iter().flat_map(|s| &s.rows) {
                        let group = view.groups.iter().find(|g| g.id == account.raw.id).unwrap();
                        let tile = view.usage.iter().find(|t| t.id == account.raw.id).unwrap();
                        assert_eq!(
                            group.forecast_lines, account.forecast_lines,
                            "{name}/{tab:?}"
                        );
                        assert_eq!(tile.forecast_lines, account.forecast_lines);
                        assert_eq!(group.forecast_tooltip, account.forecast_tooltip);
                        assert_eq!(tile.forecast_tooltip, account.forecast_tooltip);
                        assert_eq!(group.forecast_warning, account.forecast_warning);
                        assert_eq!(tile.forecast_warning, account.forecast_warning);
                    }
                }
            }
        });
    }
}

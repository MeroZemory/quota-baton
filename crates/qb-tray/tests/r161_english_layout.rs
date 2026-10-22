//! Windows-only production DirectWrite/PNG regressions. Every input is synthetic.
#![cfg(windows)]

use chrono::{DateTime, FixedOffset};
use qb_tray::{
    Rect,
    client::snapshot,
    geometry::{Color, Draw, Scene},
    i18n::{self, UiLanguage},
    snapshot_render::{self, MeasuredDashboard, MeasuredText, Options, View},
};
use serde_json::json;
use std::path::PathBuf;

const AT: &str = "2026-10-23T14:05:00Z";
// DirectWrite advances and rectangles are DIPs. This is floating-point roundoff,
// not a permitted ellipsis, missing glyph, padding reduction, or layout margin.
const EPS: f32 = 0.01;

fn fixture() -> Vec<u8> {
    let windows = json!([
        {"name":"5h","used_pct":27.0,"reset_at":"2026-10-23T16:40:00Z"},
        {"name":"7d","used_pct":42.0,"reset_at":"2026-10-26T16:40:00Z"}
    ]);
    serde_json::to_vec(&json!({
        "schema_version":1,"capture_started_at":AT,"capture_completed_at":AT,
        "utc_offset_seconds":0,"consistency":"independent_reads",
        "session_details":"not_captured",
        "query":{"sessions_range":"all","activity_days":7,"activity_limit":2000,"usage_hours":48},
        "health":{"ok":true,"version":"synthetic-r161","pid":7,"started_at":AT},
        "state":{"version":"synthetic-r161","accounts":[
            {"id":"synthetic-a","provider":"codex","label":"Synthetic account",
             "plan":"plus","user_state":"active","health":{"kind":"ready"},"windows":windows}
        ]},
        "sessions":{"now":AT,"active_window_secs":60,"sessions":[
            {"id":"0123456789ab","provider":"codex","account":"synthetic-a",
             "client":{"kind":"cli","product":"codex","version":"synthetic"},
             "label":{"text":"Synthetic project","source":"launcher"},
             "model":"synthetic-model","first_seen_at":"2026-10-23T12:00:00Z",
             "last_used_at":AT,"bound_since":"2026-10-23T12:00:00Z",
             "active":true,"inflight":{"requests":0},"requests_today":42,
             "counters_complete":true,"moves_known":false,"subagents_known":true}
        ]},
        "activity":{"events":[],"truncated":false},
        "usage_history":{"accounts":[{"id":"synthetic-a","windows":[
            {"name":"5h","points":[["2026-10-23T12:00:00Z",20.0],[AT,27.0]],
             "resets":["2026-10-23T16:40:00Z"],"truncated":false},
            {"name":"7d","points":[["2026-10-23T12:00:00Z",40.0],[AT,42.0]],
             "resets":["2026-10-26T16:40:00Z"],"truncated":false}
        ],"truncated":false}],"truncated":false}
    }))
    .unwrap()
}

fn options(view: View, size: [u32; 2], dark: bool) -> Options {
    Options {
        view,
        size,
        scale: 2,
        dark,
        now: AT.parse::<DateTime<FixedOffset>>().unwrap(),
    }
}

fn root(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.sandbox/r161/english-layout")
        .join(name);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn contents<'a>(audit: &'a MeasuredDashboard, text: &MeasuredText) -> &'a str {
    match &audit.scene.draw[text.draw_index] {
        Draw::Text { measure, .. } => &measure.text,
        _ => panic!("measured index must identify production text"),
    }
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.left + EPS >= outer.left
        && inner.top + EPS >= outer.top
        && inner.right <= outer.right + EPS
        && inner.bottom <= outer.bottom + EPS
}

fn visible(text: &MeasuredText) -> bool {
    text.rect.right > text.clip.left
        && text.rect.left < text.clip.right
        && text.rect.bottom > text.clip.top
        && text.rect.top < text.clip.bottom
}

fn fits(audit: &MeasuredDashboard, text: &MeasuredText, failures: &mut Vec<String>) {
    let value = contents(audit, text);
    if text.trimmed
        || text.natural_width > text.rect.right - text.rect.left + EPS
        || text.required_height > text.rect.bottom - text.rect.top + EPS
        || !contains(text.clip, text.rect)
    {
        failures.push(format!(
            "unellipsized text {value:?}: natural={} height={} trimmed={} rect={:?} clip={:?}",
            text.natural_width, text.required_height, text.trimmed, text.rect, text.clip
        ));
    }
}

fn require_text<'a>(audit: &'a MeasuredDashboard, value: &str) -> Vec<&'a MeasuredText> {
    let found: Vec<_> = audit
        .text
        .iter()
        .filter(|t| visible(t) && contents(audit, t) == value)
        .collect();
    assert!(
        !found.is_empty(),
        "fixture lost required visible text {value:?}"
    );
    found
}

fn matrix(name: &str, views: &[View], check: impl Fn(View, &MeasuredDashboard, &mut Vec<String>)) {
    let dir = root(name);
    let bytes = fixture();
    std::fs::write(dir.join("synthetic-snapshot.json"), &bytes).unwrap();
    let bundle = snapshot::decode(&bytes).expect("actual snapshot decoder");
    let mut all_failures = Vec::new();
    i18n::with_language(UiLanguage::En, || {
        let data = bundle
            .project(AT.parse::<DateTime<FixedOffset>>().unwrap().to_utc().into())
            .unwrap();
        for &view in views {
            for size in [[1024, 640], [1320, 900]] {
                for dark in [false, true] {
                    let options = options(view, size, dark);
                    let stem = format!(
                        "{view:?}-{}x{}-{}-scale2",
                        size[0],
                        size[1],
                        if dark { "dark" } else { "light" }
                    );
                    // Save before assertions so the failing geometry has reviewable PNG evidence.
                    let path = dir.join(format!("{stem}.png"));
                    snapshot_render::save(&data, &options, &path).unwrap();
                    let png = std::fs::read(path).unwrap();
                    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
                    assert_eq!(
                        u32::from_be_bytes(png[16..20].try_into().unwrap()),
                        size[0] * 2
                    );
                    assert_eq!(
                        u32::from_be_bytes(png[20..24].try_into().unwrap()),
                        size[1] * 2
                    );
                    let audit = snapshot_render::measure_dashboard(&data, &options).unwrap();
                    let mut failures = Vec::new();
                    std::fs::write(
                        dir.join(format!("{stem}-metrics.txt")),
                        format!("{audit:#?}"),
                    )
                    .unwrap();
                    check(view, &audit, &mut failures);
                    std::fs::write(
                        dir.join(format!("{stem}-metrics.txt")),
                        format!("{audit:#?}\nFailures:\n{failures:#?}"),
                    )
                    .unwrap();
                    all_failures.extend(failures.into_iter().map(|f| format!("{stem}: {f}")));
                }
            }
        }
    });
    assert!(
        all_failures.is_empty(),
        "{name}:\n{}",
        all_failures.join("\n")
    );
}

#[test]
fn r161_en_tabs_and_fixed_headers_fit_natural_directwrite_width() {
    matrix(
        "fixed-headers",
        &[View::Overview, View::Usage],
        |view, audit, failures| {
            for label in ["개요", "기록", "사용량"] {
                for text in require_text(audit, i18n::text(label).as_ref()) {
                    fits(audit, text, failures);
                }
            }
            if view == View::Overview {
                assert!(
                    audit
                        .scene
                        .accessibility
                        .iter()
                        .any(|node| node.key == "sessions:table"),
                    "Overview lost its required session table"
                );
                for label in ["세션", "오늘 요청", "최근 일", "연결된 전체"] {
                    for text in require_text(audit, i18n::text(label).as_ref()) {
                        fits(audit, text, failures);
                    }
                }
                // Every fixed column actually present must fit, including conditional Notes.
                for text in &audit.text {
                    if [
                        "클라이언트 · 번호",
                        "모델",
                        "시작",
                        "마지막 활동",
                        "메모",
                        "계정 이력 일부 확인 전",
                    ]
                    .iter()
                    .any(|ko| contents(audit, text) == i18n::text(ko).as_ref())
                        && visible(text)
                    {
                        fits(audit, text, failures);
                    }
                }
            }
        },
    );
}

#[test]
fn r161_en_account_window_labels_do_not_touch_values_or_clip() {
    matrix(
        "account-windows",
        &[View::Overview],
        |_view, audit, failures| {
            let rows: Vec<_> = audit
                .scene
                .accessibility
                .iter()
                .filter(|node| node.key.starts_with("usage:synthetic-a:"))
                .collect();
            assert_eq!(
                rows.len(),
                2,
                "both real account window rows must be projected"
            );
            for row in rows {
                let mut text: Vec<_> = audit
                    .text
                    .iter()
                    .filter(|t| contains(row.rect, t.rect) && visible(t))
                    .collect();
                assert_eq!(
                    text.len(),
                    3,
                    "window label, remaining value and reset must stay visible: {}",
                    row.name
                );
                text.sort_by(|a, b| a.rect.left.total_cmp(&b.rect.left));
                let expected = match row.key.as_str() {
                    "usage:synthetic-a:0" => ["5h", "73% left", "resets 16:40"],
                    "usage:synthetic-a:1" => ["7d", "58% left", "resets Oct 26 16:40"],
                    other => panic!("unexpected fixture window {other}"),
                };
                assert_eq!(
                    text.iter().map(|t| contents(audit, t)).collect::<Vec<_>>(),
                    expected,
                    "window facts disappeared"
                );

                for t in &text {
                    fits(audit, t, failures);
                }
                for adjacent in text.windows(2) {
                    let ink_right = adjacent[0].rect.left + adjacent[0].natural_width;
                    if ink_right + 2. > adjacent[1].rect.left + EPS {
                        failures.push(format!("window text touches/overlaps: {:?} -> {:?}: ink_right={ink_right} next_left={}",
                        contents(audit, adjacent[0]), contents(audit, adjacent[1]), adjacent[1].rect.left));
                    }
                }
            }
        },
    );
}

fn tiles(scene: &Scene) -> Vec<Rect> {
    scene
        .draw
        .iter()
        .filter_map(|draw| match draw {
            Draw::Stroke {
                rect,
                color: Color::GroupBorder,
                radius,
                ..
            } if *radius == 6. => Some(*rect),
            _ => None,
        })
        .collect()
}

#[test]
fn r161_en_usage_window_summaries_stay_inside_actual_cards() {
    matrix("usage-cards", &[View::Usage], |_view, audit, failures| {
        let cards = tiles(&audit.scene);
        assert_eq!(
            cards.len(),
            1,
            "synthetic account must produce an actual usage tile"
        );
        for card in cards {
            let rows: Vec<_> = audit
                .text
                .iter()
                .filter(|text| {
                    text.rect.top >= card.top + 30. - EPS
                        && text.rect.top < card.top + 60.
                        && text.rect.left >= card.left
                        && visible(text)
                })
                .collect();
            assert_eq!(
                rows.len(),
                2,
                "two complete English window summaries must remain visible"
            );
            for expected in [
                "5h · 73% left · resets 16:40",
                "7d · 58% left · resets Oct 26 16:40",
            ] {
                assert!(
                    rows.iter().any(|text| contents(audit, text) == expected),
                    "missing complete window summary {expected:?}"
                );
            }
            for text in rows {
                fits(audit, text, failures);
                if !contains(card, text.rect) {
                    failures.push(format!(
                        "window summary {:?} escapes usage card {:?}: {:?}",
                        contents(audit, text),
                        card,
                        text.rect
                    ));
                }
            }
        }
        // Outer-card summary and legend must not escape the client/card boundary either.
        for label in [
            "남은 양 · 최근 19시간 / 앞으로 5시간",
            "━ 5시간 (Free는 30일)  ┄ 주간  ⋮ 초기화  ▧ 크레딧으로 처리",
        ] {
            for text in require_text(audit, i18n::text(label).as_ref()) {
                fits(audit, text, failures);
                let enclosing_card = audit.scene.draw.iter().any(|draw| {
                    matches!(draw,
                    Draw::Stroke {rect, color: Color::Border, radius, ..}
                        if *radius == 8. && contains(*rect, text.rect))
                });
                if !enclosing_card {
                    failures.push(format!(
                        "usage header {:?} has no enclosing card",
                        contents(audit, text)
                    ));
                }
            }
        }
    });
}

#[test]
fn r161_ko_scene_and_png_are_byte_identical_across_english_render_scope() {
    let dir = root("ko-language-isolation");
    let bundle = snapshot::decode(&fixture()).unwrap();
    for view in [View::Overview, View::Usage] {
        let options = options(view, [1024, 640], false);
        let render = |language, path: &std::path::Path| {
            i18n::with_language(language, || {
                let data = bundle.project(options.now.to_utc().into()).unwrap();
                let scene = snapshot_render::measure_dashboard(&data, &options).unwrap();
                snapshot_render::save(&data, &options, path).unwrap();
                format!("{:?}", scene.scene)
            })
        };
        let before = dir.join(format!("{view:?}-ko-before.png"));
        let after = dir.join(format!("{view:?}-ko-after.png"));
        let original = render(UiLanguage::Ko, &before);
        render(UiLanguage::En, &dir.join(format!("{view:?}-en.png")));
        assert_eq!(
            original,
            render(UiLanguage::Ko, &after),
            "KO scene changed across EN scope"
        );
        assert_eq!(
            std::fs::read(before).unwrap(),
            std::fs::read(after).unwrap(),
            "KO PNG bytes changed across EN scope"
        );
    }
}

use super::*;
use crate::{
    dashboard_fixtures as fixtures, dashboard_model as m, geometry, state, v2_geometry as g,
    v2_session::Session,
};
use serde_json::{Value, json};

fn account(id: &str, provider: &str) -> Value {
    let subscription = if provider == "claude" {
        json!({"provider":"claude","created_at":"2026-01-01T00:00:00Z"})
    } else {
        json!({"provider":"codex","active_start":null,"active_until":null,"last_checked":null})
    };
    json!({"id":id,"provider":provider,"label":id,"plan":"free","source":{"kind":"oauth"},
        "user_state":"active","health":{"kind":"ready"},"pool_eligible":true,
        "active_sessions":2,"bound_sessions":4,
        "windows":[{"name":"5h","used_pct":40,"reset_at":"2026-10-19T10:00:00Z"}],
        "subscription":subscription})
}
fn project_accounts(accounts: Vec<Value>) -> View {
    project(
        &json!({"accounts":accounts,"pools":{"claude":{"eligible":1},"codex":{"eligible":1}}})
            .to_string(),
        fixtures::clock().utc().into(),
    )
    .unwrap()
}
fn all_rows(view: &View) -> impl Iterator<Item = &AccountRow> {
    view.sections
        .iter()
        .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
}
fn excluded(view: &View) -> &Section {
    view.sections
        .iter()
        .find(|s| s.provider == EXCLUDED_GROUP_KEY)
        .unwrap()
}

#[test]
fn only_explicit_native_facts_exclude_and_codex_free_cooling_unknowns_stay_main() {
    for (provider, user, health, plan, expected) in [
        ("codex", "active", "ready", Some("free"), false),
        ("claude", "active", "ready", Some("free"), true),
        ("claude", "active", "cooling", Some("free"), true),
        ("claude", "active", "plan_without_claude_code", None, true),
        ("codex", "paused", "ready", Some("plus"), true),
        ("codex", "disabled", "ready", None, true),
        ("codex", "active", "needs_relogin", Some("plus"), true),
        ("claude", "active", "ready", None, false),
        (
            "claude",
            "active",
            "ready",
            Some("synthetic_unknown_tier"),
            false,
        ),
    ] {
        let mut raw = account("synthetic", provider);
        raw["user_state"] = json!(user);
        raw["health"]["kind"] = json!(health);
        raw["plan"] = json!(plan);
        raw["pool_eligible"] = json!(false);
        raw["drain"] = json!(true);
        let a: Account = serde_json::from_value(raw).unwrap();
        assert_eq!(
            exclusion_reason(&a).is_some(),
            expected,
            "{provider}/{user}/{health}/{plan:?}"
        );
    }
    for reason in [
        "quota",
        "rate",
        "auth",
        "payment",
        "permission",
        "store_error",
        "link_stale",
    ] {
        let mut raw = account(reason, "codex");
        raw["health"] = json!({"kind":"cooling","reason":reason,"scope":"account","until":"2026-10-20T00:00:00Z"});
        raw["pool_eligible"] = json!(false);
        raw["probe_pending"] = json!(true);
        assert!(exclusion_reason(&serde_json::from_value(raw).unwrap()).is_none());
    }
}

#[test]
fn one_global_bottom_group_preserves_summaries_and_raw_menu_inputs() {
    let mut paid = account("claude-paid", "claude");
    paid["plan"] = json!("pro");
    let mut disabled = account("disabled", "codex");
    disabled["user_state"] = json!("disabled");
    let mut login = account("login", "codex");
    login["health"] = json!({"kind":"needs_relogin"});
    let view = project_accounts(vec![
        account("codex-free", "codex"),
        account("claude-free", "claude"),
        paid,
        disabled,
        login,
    ]);
    assert_eq!(view.raw.accounts.len(), 5);
    assert_eq!(view.summaries.len(), 2);
    assert!(view.summaries.iter().all(|s| s.eligible == Some(1)));
    assert_eq!(
        view.summaries[0].todo, 0,
        "ready metadata is unchanged before compact partition"
    );
    assert_eq!(
        view.sections
            .iter()
            .map(|s| s.provider.as_str())
            .collect::<Vec<_>>(),
        ["claude", "codex", EXCLUDED_GROUP_KEY]
    );
    assert_eq!(all_rows(&view).count(), 5);
    let group = excluded(&view);
    assert_eq!(group.disabled_rows.len(), 3);
    assert!(!group.auto_expand_recommended);
    assert_eq!(view.sections[1].rows[0].raw.id, "codex-free");
    for row in &group.disabled_rows {
        assert!(row.usage.is_none() && row.windows.is_empty() && row.forecast_lines.is_empty());
        assert!(row.subscription_lines.is_empty() && row.outside_note.is_none());
        assert!(row.credits.is_none());
        assert_eq!(row.raw.bound_sessions, 4);
        assert_eq!(row.raw.windows.len(), 1);
        assert!(!row.menu.sessions_line.is_empty());
        assert!(row.accessible_name.contains(row.excluded_reason.unwrap()));
    }
}

#[test]
fn recovery_and_new_exclusion_reclassify_immediately_despite_frozen_order_and_delete_vanishes() {
    let mut blocked = account("moving", "codex");
    blocked["user_state"] = json!("disabled");
    let initial = project_accounts(vec![blocked, account("steady", "codex")]);
    let mut session = Session::default();
    session.show(&initial);
    assert!(!session.expansion[EXCLUDED_GROUP_KEY]);
    session.set_expanded(EXCLUDED_GROUP_KEY, true);
    let frozen = g::capture_order(&initial);
    let recovered = project_accounts(vec![account("moving", "codex"), account("steady", "codex")]);
    session.update(&recovered);
    let slot = session.slots.iter().find(|s| s.id == "moving").unwrap();
    assert_eq!(slot.provider, "codex");
    assert!(!slot.disabled_on_open);
    let scene = g::Scene::build(
        &recovered,
        g::Presentation {
            frozen,
            ..Default::default()
        },
        700.,
    );
    assert!(scene.row("moving").unwrap().compact_reason.is_none());
    let mut newly_paused = account("new", "codex");
    newly_paused["user_state"] = json!("paused");
    let next = project_accounts(vec![newly_paused, account("steady", "codex")]);
    session.update(&next);
    assert!(!session.slots.iter().any(|s| s.id == "moving"));
    assert!(session.expansion[EXCLUDED_GROUP_KEY]);
    let scene = g::Scene::build(
        &next,
        g::Presentation {
            frozen: scene.resolved_order,
            expanded: session.expansion.clone(),
            overlay: g::Overlay::RowMenu("moving".into()),
            ..Default::default()
        },
        700.,
    );
    assert!(scene.row("moving").is_none() && scene.popup.is_none());
    assert!(scene.row("new").unwrap().compact_reason.is_some());
    session.hide();
    session.show(&next);
    assert!(session.expansion[EXCLUDED_GROUP_KEY]);
}

#[test]
fn preference_restore_before_or_after_show_respects_user_choice_and_transient_reveal() {
    let mut raw = account("off", "codex");
    raw["user_state"] = json!("disabled");
    let view = project_accounts(vec![raw]);
    for before_show in [false, true] {
        let mut session = Session::default();
        if before_show {
            assert!(session.restore_excluded_preference(true));
        }
        session.show(&view);
        if !before_show {
            assert!(session.restore_excluded_preference(true));
        }
        assert!(session.expansion[EXCLUDED_GROUP_KEY]);
        session.set_expanded(EXCLUDED_GROUP_KEY, false);
        assert!(!session.restore_excluded_preference(true));
        assert!(!session.expansion[EXCLUDED_GROUP_KEY]);
        session.expansion.insert(EXCLUDED_GROUP_KEY.into(), true);
        assert!(!session.manual_expansion[EXCLUDED_GROUP_KEY]);
        session.hide();
        session.show(&view);
        assert!(!session.expansion[EXCLUDED_GROUP_KEY]);
    }
}

#[test]
fn tagged_source_kinds_are_strict_but_legacy_keys_and_native_additive_modes_keep_ownership() {
    for (source, linked, owned) in [
        (json!({"kind":"oauth","mode":"linked"}), false, true),
        (json!({"kind":"takeover","mode":"synthetic"}), false, true),
        (json!({"kind":"cpa","mode":"linked"}), true, false),
        (json!({"kind":"cpa","mode":"adopted"}), false, true),
        (json!({"kind":"unknown","mode":"adopted"}), false, false),
        (json!({"kind":"adopted"}), false, false),
        (json!({"kind":"owned"}), false, false),
        (json!({"kind":"linked"}), false, false),
        (json!("owned"), false, true),
        (json!("adopted"), false, true),
        (json!("linked"), true, false),
    ] {
        let source: Source = serde_json::from_value(source).unwrap();
        assert_eq!(source.linked(), linked);
        assert_eq!(source.owned_or_adopted(), owned);
    }
}

#[test]
fn compact_geometry_uses_measured_widths_clips_hits_and_retains_full_accessible_reason_ko_en() {
    for language in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
        crate::i18n::with_language(language, || {
            let view = project_accounts(vec![account("very-long-synthetic-name", "claude")]);
            let row = &excluded(&view).disabled_rows[0];
            assert!(!compact_reason(row).contains("Free"));
            let collapsed = g::Scene::build(&view, Default::default(), 700.);
            assert!(collapsed.row(&row.raw.id).is_none());
            let mut p = g::Presentation::default();
            p.expanded.insert(EXCLUDED_GROUP_KEY.into(), true);
            p.text_widths
                .insert(g::text_key(&row.name, 13, false), 600.);
            p.text_widths
                .insert(g::text_key(row.plan.as_deref().unwrap(), 12, false), 26.);
            p.text_widths
                .insert(g::text_key(compact_reason(row), 12, false), 160.);
            let full = g::Scene::build(&view, p.clone(), 700.);
            let b = full.row(&row.raw.id).unwrap();
            let reason = b.compact_reason.as_ref().unwrap();
            assert_eq!(g::h(b.rect), 44.);
            assert_eq!(reason.rect.right - reason.rect.left, 160.);
            assert!(b.name.right + 6. <= b.plan.unwrap().left);
            assert!(b.plan.unwrap().right + 8. <= reason.rect.left);
            assert!(reason.rect.right < b.more.left);
            assert_eq!(reason.tooltip.as_deref(), row.excluded_reason);
            assert!(
                b.bar.is_none()
                    && b.forecast.is_none()
                    && b.subscription.is_empty()
                    && b.button.is_none()
            );
            for offset in [0., 30., 1000.] {
                p.offset = offset;
                let scene = g::Scene::build(&view, p.clone(), 230.);
                let b = scene.row(&row.raw.id).unwrap();
                assert_eq!(
                    b.compact_reason.as_ref().unwrap().rect.top,
                    b.rect.top + 14.
                );
                for hit in &scene.targets {
                    if matches!(&hit.target, g::Target::Row(_) | g::Target::Disclosure(_)) {
                        assert!(
                            hit.rect.top >= scene.list.top && hit.rect.bottom <= scene.list.bottom
                        );
                    }
                }
            }
        });
    }
}

#[test]
fn dashboard_retains_disabled_without_sessions_compact_and_keeps_historical_sessions() {
    let clock = fixtures::clock();
    let mut data = fixtures::snapshot();
    let mut disabled = account("cl-a", "claude");
    disabled["user_state"] = json!("disabled");
    data.accounts = project_accounts(vec![disabled]);
    let retained = data.sessions.sessions.len();
    let mut state = state::DashboardState {
        range: state::Range::All,
        ..Default::default()
    };
    let view = m::project(&data, &mut state, &clock);
    assert_eq!(view.session_count, retained);
    assert!(view.groups.is_empty());
    assert_eq!(view.excluded_rows.len(), 1);
    data.sessions.sessions.clear();
    let mut view = m::project(&data, &mut state, &clock);
    view.provider_usage.clear();
    assert_eq!(view.excluded_rows[0].raw.id, "cl-a");
    for size in [[1024., 640.], [1320., 960.]] {
        let scene = geometry::Scene::build(&view, &state, size, &geometry::Metrics::default());
        assert!(
            scene
                .accessibility
                .iter()
                .any(|node| node.key.ends_with("excluded:cl-a"))
        );
        assert!(scene.session_rows.is_empty());
        assert!(scene.targets.iter().any(
            |hit| matches!(&hit.target, geometry::Target::OpenTrayAccount(id) if id == "cl-a")
        ));
    }
}

#[test]
fn paused_overview_is_compact_but_usage_tab_keeps_original_window_facts() {
    let clock = fixtures::clock();
    let mut data = fixtures::snapshot();
    let mut raw = account("cx-a", "codex");
    raw["plan"] = json!("plus");
    data.accounts = project_accounts(vec![raw.clone()]);
    let mut state = state::DashboardState {
        range: state::Range::All,
        ..Default::default()
    };
    let ready = m::project(&data, &mut state, &clock);
    raw["user_state"] = json!("paused");
    data.accounts = project_accounts(vec![raw]);
    let paused = m::project(&data, &mut state, &clock);
    assert_eq!(paused.excluded_rows.len(), 1);
    assert!(paused.excluded_rows[0].windows.is_empty());
    assert_eq!(paused.usage.len(), ready.usage.len());
    assert_eq!(
        format!("{:?}", paused.usage[0].windows),
        format!("{:?}", ready.usage[0].windows)
    );
    assert!(!paused.usage[0].windows.is_empty());
    assert_eq!(paused.flat.len(), ready.flat.len());
}

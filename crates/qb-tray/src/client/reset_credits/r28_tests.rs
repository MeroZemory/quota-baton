use super::*;
use qb_proto::reset_credits::ResetCreditGrant;
use qb_tray::{
    i18n::{UiLanguage, with_language},
    v2, v2_geometry as g,
};
use std::time::SystemTime;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-11T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn grant(id: &str, label: &str, usable: bool) -> ClaudeResetGrant {
    ClaudeResetGrant {
        id: id.into(),
        label: label.into(),
        grant_type: Some(if label == "Fable" { "weekly" } else { "full" }.into()),
        resets_total: Some(3),
        resets_left: 2,
        starts_at: None,
        ends_at: Some("2026-11-30T10:00:00Z".into()),
        clears: if label == "Fable" {
            vec!["seven_day_omelette".into()]
        } else {
            vec!["five_hour".into(), "seven_day".into()]
        },
        blocking: vec![],
        paused: false,
        usable_now: usable,
        use_requires_limit: true,
        gives_back_weekly: false,
        percent_used: BTreeMap::new(),
        unavailable_reason: (!usable).then(|| "needs_limit".into()),
    }
}

fn inventory(grants: Vec<ClaudeResetGrant>) -> ResetCreditInventory {
    ResetCreditInventory {
        state: ResetCreditState::Fresh,
        observed_at: Some(now().to_rfc3339()),
        available_count: Some(grants.iter().map(|g| g.resets_left).sum()),
        grants: grants
            .iter()
            .map(|g| ResetCreditGrant {
                id: g.id.clone(),
                expires_at: ResetCreditExpiry::At {
                    at: g.ends_at.clone().unwrap(),
                },
                status: ResetCreditStatus::Available,
                reset_type: ResetCreditType::ClaudeRateLimits,
            })
            .collect(),
        refusal: None,
        claude: Some(ClaudeResetInventory {
            eligible: true,
            ineligible_reason: None,
            at_limit: Some(true),
            exhausted: vec!["five_hour".into()],
            next_grant_id: Some("fable-secret".into()),
            weekly_resets_at: None,
            cooldown_until: None,
            grants,
        }),
    }
}

fn preview(i: ResetCreditInventory, id: &str) -> ResetCreditPreview {
    ResetCreditPreview {
        account: "cl-secret".into(),
        label: "Chosen account".into(),
        email: "sample@example.invalid".into(),
        preview_token: "private-token".into(),
        expires_at: (now() + chrono::Duration::seconds(60)).to_rfc3339(),
        grant: i.grants.iter().find(|g| g.id == id).unwrap().clone(),
        inventory: i,
        effect: "claude_rate_limits".into(),
    }
}

fn view(i: Option<&ResetCreditInventory>, state: &str, health: &str, plan: &str) -> v2::View {
    v2::project(
        &serde_json::json!({"accounts":[{
            "id":"cl-secret", "provider":"claude", "label":"Chosen account", "plan":plan,
            "user_state":state,"health":{"kind":health},"reset_credits":i,
            "windows":[{"name":"5h","used_pct":100}]
        }]})
        .to_string(),
        SystemTime::from(now()),
    )
    .unwrap()
}

#[test]
fn unknown_or_not_offered_has_no_card_line_but_still_has_read_entry() {
    for language in [UiLanguage::Ko, UiLanguage::En] {
        with_language(language, || {
            for i in [
                None,
                Some(ResetCreditInventory::default()),
                Some(ResetCreditInventory {
                    state: ResetCreditState::Unsupported,
                    ..Default::default()
                }),
            ] {
                assert!(inventory_line("claude", i.as_ref(), now()).text.is_empty());
                let v = view(i.as_ref(), "active", "ready", "pro");
                let row = &v.sections.iter().flat_map(|s| &s.rows).next().unwrap();
                assert!(row.reset_credits.is_none());
                let scene = g::Scene::build(
                    &v,
                    g::Presentation {
                        overlay: g::Overlay::RowMenu("cl-secret".into()),
                        ..Default::default()
                    },
                    800.,
                );
                assert!(scene.popup.unwrap().items.iter().any(|i| i.enabled
                    && matches!(
                        i.target,
                        g::Target::Menu {
                            action: g::MenuAction::UseResetCredit,
                            ..
                        }
                    )));
            }
        });
    }
}

#[test]
fn choice_is_not_consent_single_skips_list_and_explicit_selection_confirms_once() {
    let i = inventory(vec![
        grant("full-secret", "Full", true),
        grant("fable-secret", "Fable", true),
        grant("disabled-secret", "Weekly", false),
    ]);
    assert_eq!(
        claude_recommendation(&i, now()).as_deref(),
        Some("fable-secret")
    );
    let mut c = Consent::default();
    let generation = c.begin("cl-secret", false).unwrap();
    c.finish_read("cl-secret", generation, None);
    assert!(c.offer_choices("cl-secret", generation, &i));
    assert!(c.preview.is_none());
    assert!(c.confirm("cl-secret", now()).is_none());
    assert!(c.can_select("cl-secret", "fable-secret"));
    assert!(!c.can_select("cl-secret", "disabled-secret"));
    assert!(!c.can_select("other", "fable-secret"));
    let selected = c.begin("cl-secret", false).unwrap();
    c.finish_read("cl-secret", selected, None);
    assert!(c.ready(selected, preview(i.clone(), "fable-secret"), now()));
    assert_eq!(c.preview.as_ref().unwrap().grant.id, "fable-secret");
    assert!(c.confirm("cl-secret", now()).unwrap().confirm);
    assert!(c.confirm("cl-secret", now()).is_none());
    c.result("cl-secret", &Err(ActionError::Unknown));
    c.reconciled("cl-secret", &i);
    assert!(
        c.unknown.contains_key("cl-secret"),
        "cached availability never settles Claude"
    );
    c.refreshed("cl-secret", "claude", &i);
    assert!(
        !c.unknown.contains_key("cl-secret"),
        "only successful explicit core refresh settles"
    );

    let single = inventory(vec![grant("one-secret", "Full", true)]);
    let generation = c.begin("cl-secret", false).unwrap();
    c.finish_read("cl-secret", generation, None);
    assert!(!c.offer_choices("cl-secret", generation, &single));
    assert!(c.ready(generation, preview(single, "one-secret"), now()));
    c.cancel();
    assert!(!c.offer_choices("cl-secret", generation, &i));
    assert!(!c.ready(generation, preview(i, "fable-secret"), now()));
}

#[test]
fn recommendation_is_usable_only_and_falls_back_to_earliest_expiry() {
    let mut grants = vec![
        grant("full-secret", "Full", true),
        grant("fable-secret", "Fable", false),
    ];
    assert_eq!(
        claude_recommendation(&inventory(grants.clone()), now()).as_deref(),
        Some("full-secret")
    );
    let mut earlier = grant("earlier-secret", "Weekly", true);
    earlier.ends_at = Some("2026-11-12T00:00:00Z".into());
    grants.push(earlier);
    let mut i = inventory(grants);
    assert_eq!(
        claude_recommendation(&i, now()).as_deref(),
        Some("earlier-secret")
    );
    i.claude.as_mut().unwrap().eligible = false;
    assert_eq!(claude_recommendation(&i, now()), None);
}

#[test]
fn recommendation_rejects_expired_paused_and_unknown_effect_next_grants_and_global_cooldown() {
    for invalid in ["expired", "paused", "unknown_clears", "cooldown"] {
        let mut i = inventory(vec![
            grant("full-secret", "Full", true),
            grant("fable-secret", "Fable", true),
        ]);
        let facts = i.claude.as_mut().unwrap();
        assert!(
            facts.grants[1].usable_now,
            "provider verdict alone is insufficient"
        );
        match invalid {
            "expired" => {
                let at = "2026-11-10T00:00:00Z".to_owned();
                facts.grants[1].ends_at = Some(at.clone());
                i.grants[1].expires_at = ResetCreditExpiry::At { at };
            }
            "paused" => facts.grants[1].paused = true,
            "unknown_clears" => facts.grants[1].clears = vec!["future-private-window".into()],
            "cooldown" => facts.cooldown_until = Some("2026-11-12T00:00:00Z".into()),
            _ => unreachable!(),
        }
        let expected = if invalid == "cooldown" {
            None
        } else {
            Some("full-secret")
        };
        assert_eq!(
            claude_recommendation(&i, now()).as_deref(),
            expected,
            "{invalid}"
        );
    }
}

#[test]
fn localized_names_expiry_effect_count_and_reasons_never_show_identifiers_or_aliases() {
    let clock = now().with_timezone(&chrono::FixedOffset::east_opt(9 * 3600).unwrap());
    for language in [UiLanguage::Ko, UiLanguage::En] {
        with_language(language, || {
            let i = inventory(vec![
                grant("full-secret", "Full", true),
                grant("fable-secret", "Fable", true),
            ]);
            let p = preview(i.clone(), "fable-secret");
            let title = claude_preview_title(&p).unwrap();
            assert_eq!(
                title,
                qb_tray::i18n::format("{0} 리셋권 1개 사용", &["Fable".into()])
            );
            let confirmation = g::Scene::build(
                &view(Some(&i), "active", "ready", "pro"),
                g::Presentation {
                    overlay: g::Overlay::ResetCredits {
                        id: "cl-secret".into(),
                        body: preview_body_at(&p, &clock),
                        ready: true,
                    },
                    reset_confirmation_title: Some(title.clone()),
                    ..Default::default()
                },
                800.,
            );
            assert_eq!(confirmation.dialog.unwrap().title_text, Some(title));
            let body = preview_body_at(&p, &clock);
            let line = inventory_line_at("claude", Some(&i), &clock);
            assert!(line.text.contains("Full") && line.text.contains("Fable"));
            assert!(line.text.contains("11/30 19:00"));
            assert!(body.contains("Fable") && body.contains("11/30 19:00"));
            assert!(body.contains(&text("모델별 주간")));
            assert!(
                !body.contains(&text("5시간")),
                "confirmation describes the selected grant only"
            );
            assert!(body.contains(&qb_tray::i18n::format(
                "사용 후 남는 횟수: {0}",
                &["1".into()]
            )));
            for s in [&line.text, &line.tooltip, &body] {
                for secret in [
                    "full-secret",
                    "fable-secret",
                    "cl-secret",
                    "private-token",
                    "T10:00:00Z",
                ] {
                    assert!(!s.contains(secret));
                }
            }
            let mut g = grant("hidden-secret", "", false);
            g.grant_type = None;
            g.clears = vec!["seven_day_omelette".into()];
            assert!(!claude_label(&g).contains("Fable"));
            assert!(!claude_label(&g).contains("omelette"));
            for reason in [
                "needs_limit",
                "paused",
                "expired",
                "cooldown",
                "blocked_five_hour",
                "blocked_opus",
                "ineligible",
            ] {
                let rendered = claude_reason_at(Some(reason), Some("2026-11-12T10:00:00Z"), &clock);
                if reason.contains('_') {
                    assert!(!rendered.contains(reason));
                }
                assert!(!rendered.is_empty());
                if language == UiLanguage::En {
                    assert!(rendered.chars().all(|c| !(('가'..='힣').contains(&c))));
                }
            }
        });
    }
}

#[test]
fn no_limit_or_ineligible_keeps_counts_and_reason_but_never_enables_cta_or_preview() {
    for language in [UiLanguage::Ko, UiLanguage::En] {
        with_language(language, || {
            let mut i = inventory(vec![grant("one-secret", "Full", false)]);
            i.claude.as_mut().unwrap().at_limit = Some(false);
            let line = inventory_line("claude", Some(&i), now());
            assert!(!line.can_use);
            assert!(
                line.tooltip
                    .contains(&text("한도에 도달했을 때만 사용할 수 있습니다."))
            );
            let v = view(Some(&i), "active", "ready", "pro");
            assert!(
                g::Scene::build(&v, g::Presentation::default(), 800.)
                    .sections
                    .iter()
                    .flat_map(|s| &s.rows)
                    .all(|r| r.reset_credit_action.is_none())
            );
            let mut c = Consent::default();
            let generation = c.begin("cl-secret", false).unwrap();
            c.finish_read("cl-secret", generation, None);
            assert!(!c.ready(generation, preview(i.clone(), "one-secret"), now()));
            i.claude.as_mut().unwrap().eligible = false;
            i.claude.as_mut().unwrap().ineligible_reason = Some("surface".into());
            assert!(!inventory_line("claude", Some(&i), now()).can_use);
        });
    }
}

#[test]
fn multiple_grants_use_bounded_scrolling_native_menu_and_keep_disabled_reasons() {
    let rows: Vec<_> = (0..40)
        .map(|n| g::ResetCreditChoice {
            id: format!("hidden-{n}"),
            label: format!("Grant {n}"),
            detail: if n == 0 {
                text("한도에 도달했을 때만 사용할 수 있습니다.")
            } else {
                "2 uses left".into()
            },
            recommended: n == 1,
            enabled: n != 0,
        })
        .collect();
    let v = view(None, "active", "ready", "pro");
    for cap in [240., 480., 800.] {
        let p = g::Presentation {
            overlay: g::Overlay::ResetCreditChoices {
                id: "cl-secret".into(),
                rows: rows.clone(),
            },
            ..Default::default()
        };
        let scene = g::Scene::build(&v, p.clone(), cap);
        let popup = scene.popup.unwrap();
        assert!(popup.rect.bottom <= cap);
        assert!(popup.content_height > popup.clip.bottom - popup.clip.top);
        assert!(popup.items[0].target == g::Target::CancelDialog && popup.items[0].enabled);
        assert!(!popup.items[1].enabled);
        assert!(
            popup.items[1]
                .detail
                .as_ref()
                .unwrap()
                .contains(&text("한도에 도달했을 때만 사용할 수 있습니다."))
        );
        assert!(popup.items.iter().all(|i| !i.text.contains("hidden-")
            && !i.detail.as_deref().unwrap_or_default().contains("hidden-")));
        assert!(
            popup.items.iter().all(|i| i.checked.is_none()),
            "recommendation never impersonates selection"
        );
        assert!(
            scene
                .targets
                .iter()
                .all(|t| !matches!(t.target, g::Target::ConfirmResetCredit(_)))
        );
        let mut scrolled = p;
        scrolled.popup_offset = f32::MAX;
        assert!(g::Scene::build(&v, scrolled, cap).targets.iter().any(|t| matches!(&t.target, g::Target::SelectResetCredit { grant_id, .. } if grant_id == "hidden-39")));
    }
}

#[test]
fn active_and_paused_same_core_state_allow_cta_but_excluded_accounts_do_not() {
    let i = inventory(vec![grant("one-secret", "Full", true)]);
    for (state, health, plan, allow) in [
        ("active", "ready", "pro", true),
        ("paused", "ready", "pro", true),
        ("disabled", "ready", "pro", false),
        ("active", "needs_relogin", "pro", false),
        ("active", "plan_without_claude_code", "pro", false),
        ("active", "ready", "free", false),
    ] {
        let v = view(Some(&i), state, health, plan);
        let row = v
            .sections
            .iter()
            .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
            .next()
            .unwrap();
        assert_eq!(v2::reset_credit_account_refusal(&row.raw).is_none(), allow);
    }
}

#[test]
fn success_and_refusal_show_native_windows_counts_and_finite_reasons() {
    for language in [UiLanguage::Ko, UiLanguage::En] {
        with_language(language, || {
            let r = qb_proto::reset_credit_action::ClaudeResetResult {
                result: "reset".into(),
                reason: None,
                resets_left: Some(1),
                cleared: vec!["five_hour".into(), "seven_day_sonnet".into()],
                weekly_resets_at: None,
                cooldown_until: None,
            };
            let body = claude_result_body(&r);
            assert!(body.contains(&text("5시간")) && body.contains(&text("Sonnet 주간")));
            assert!(body.contains(&qb_tray::i18n::format("남은 횟수: {0}", &["1".into()])));
            let mut c = Consent::default();
            c.result(
                "cl-secret",
                &Ok(ResetCreditResult {
                    account: "cl-secret".into(),
                    outcome: ResetCreditOutcome::Refused,
                    inventory: inventory(vec![grant("one-secret", "Full", false)]),
                    refresh_failed: false,
                    claude: Some(qb_proto::reset_credit_action::ClaudeResetResult {
                        result: "not_limited".into(),
                        reason: Some("not_limited".into()),
                        ..r
                    }),
                }),
            );
            assert!(
                c.result_detail
                    .unwrap()
                    .contains(&text("한도에 도달했을 때만 사용할 수 있습니다."))
            );
        });
    }
}

#[test]
fn unknown_commit_never_supplies_positive_result_detail_or_clears_the_fence() {
    let i = inventory(vec![grant("one-secret", "Full", true)]);
    let mut c = Consent::default();
    c.unknown.insert("cl-secret".into(), "one-secret".into());
    c.result(
        "cl-secret",
        &Ok(ResetCreditResult {
            account: "cl-secret".into(),
            outcome: ResetCreditOutcome::Unknown,
            inventory: i.clone(),
            refresh_failed: false,
            claude: Some(qb_proto::reset_credit_action::ClaudeResetResult {
                result: "reset".into(),
                reason: None,
                resets_left: Some(1),
                cleared: vec!["five_hour".into()],
                weekly_resets_at: None,
                cooldown_until: None,
            }),
        }),
    );
    assert!(c.unknown.contains_key("cl-secret") && c.result_detail.is_none());
    let mut missing_metadata = i;
    missing_metadata.claude = None;
    c.reconciled("cl-secret", &missing_metadata);
    assert!(c.unknown.contains_key("cl-secret"));
}

#[test]
fn codex_wire_bytes_and_confirmation_copy_remain_unchanged() {
    with_language(UiLanguage::En, || {
        let mut i = inventory(vec![grant("one-secret", "Full", true)]);
        i.claude = None;
        i.available_count = Some(1);
        i.grants[0].reset_type = ResetCreditType::CodexRateLimits;
        i.grants[0].expires_at = ResetCreditExpiry::Never;
        let wire = serde_json::to_value(&i).unwrap();
        assert!(wire.get("claude").is_none());
        assert_eq!(
            wire["grants"][0],
            serde_json::json!({"id":"one-secret","expires_at":{"type":"never"},"status":"available","reset_type":"codex_rate_limits"})
        );
        let line = inventory_line("codex", Some(&i), now());
        assert!(line.can_use);
        assert_eq!(
            line.text,
            qb_tray::i18n::format("리셋권 {0}개 · 만료 없음", &["1".into()])
        );
        let mut p = preview(i, "one-secret");
        p.effect = "eligible_codex_limits".into();
        assert!(claude_preview_title(&p).is_none());
        let body = preview_body(&p);
        assert!(body.contains(&text("이 계정의 Codex 사용 한도가 초기화됩니다.")));
        assert!(!body.contains("Claude") && !body.contains("Fable"));
        let mut c = Consent::default();
        let generation = c.begin("cl-secret", false).unwrap();
        c.finish_read("cl-secret", generation, None);
        assert!(c.ready(generation, p.clone(), now()));
        p.inventory.claude = Some(
            inventory(vec![grant("one-secret", "Full", true)])
                .claude
                .unwrap(),
        );
        assert!(
            !c.ready(generation, p, now()),
            "Codex cannot impersonate Claude inventory"
        );
    });
}

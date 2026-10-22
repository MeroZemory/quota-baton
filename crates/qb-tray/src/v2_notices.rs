//! SPEC-v2 notification copy over the same projection as the flyout.
//! Callers own C9 proof, lifecycle freshness, reset-hold suppression and debounce.
//! Options below suggest a user action; they never assert that credits are spent.
use crate::{QuotaAction, QuotaMessage, QuotaNotice, v2};
use chrono::{DateTime, Local, Utc};
use std::time::SystemTime;

pub fn account_row<'a>(view: &'a v2::View, provider: &str, id: &str) -> Option<&'a v2::AccountRow> {
    view.sections
        .iter()
        .filter(|s| s.provider == provider)
        .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
        .find(|r| r.raw.id == id)
}

pub fn time(at: DateTime<Utc>, now: SystemTime) -> String {
    v2::calendar_time(
        at.with_timezone(&Local),
        DateTime::<Utc>::from(now).with_timezone(&Local),
        false,
    )
}

/// Only the selected core IDs resolve names; an unknown ID never picks a successor.
pub fn quota_message(
    view: &v2::View,
    provider: &str,
    account_id: &str,
    notice: &QuotaNotice,
    now: SystemTime,
) -> Option<QuotaMessage> {
    if !notice.current(now) {
        return None;
    }
    let row = account_row(view, provider, account_id)?;
    let name = &row.name;
    let provider_name = v2::provider_label(provider);
    let window = match notice.window.as_deref() {
        Some("5h" | "primary") => crate::i18n::static_text("5시간"),
        Some("7d" | "secondary") => crate::i18n::static_text("주간"),
        Some(name) if name.starts_with("7d_") => crate::i18n::static_text("주간"),
        _ => "",
    };
    let limit = if window.is_empty() {
        crate::i18n::static_text("한도 도달").into()
    } else {
        crate::i18n::format("{0} 한도 도달", &[window.to_string()])
    };
    let limit = if notice.model_limited {
        crate::i18n::format("이 모델의 {0}", &[limit.to_string()])
    } else {
        limit
    };
    let reset = notice.reset_at.map(|at| time(at, now));
    let reset_sentence = reset.as_ref().map_or_else(
        || crate::i18n::static_text("초기화 시간 확인 중입니다.").into(),
        |at| crate::i18n::format("{0} 초기화.", &[at.to_string()]),
    );
    let (title, text, short) = match notice.action {
        QuotaAction::Credits => (
            crate::account_notice_title(name, provider, &limit),
            crate::i18n::format(
                "유료 크레딧으로 계속 처리합니다. {0}",
                &[reset_sentence.to_string()],
            ),
            crate::i18n::static_text("크레딧 사용 중").into(),
        ),
        QuotaAction::Failover => {
            let next = notice
                .next_account
                .as_deref()
                .and_then(|id| account_row(view, provider, id));
            let action = next.map_or_else(
                || crate::i18n::static_text("다른 계정으로 이어서 처리합니다.").into(),
                |r| crate::i18n::format("{0} 계정으로 이어서 처리합니다.", &[(r.name).to_string()]),
            );
            (
                crate::account_notice_title(name, provider, &limit),
                format!("{action} {reset_sentence}"),
                crate::i18n::static_text("다른 계정이 처리").into(),
            )
        }
        QuotaAction::Exhausted => {
            let earliest = notice
                .earliest_account
                .as_deref()
                .and_then(|id| account_row(view, provider, id));
            let prefix = match (reset.as_deref(), earliest) {
                (Some(at), Some(r)) => crate::i18n::format(
                    "가장 빠른 초기화: {0} {1}.",
                    &[(r.name).to_string(), at.to_string()],
                ),
                (Some(at), None) => {
                    crate::i18n::format("가장 빠른 초기화: {0}.", &[at.to_string()])
                }
                (None, _) => crate::i18n::static_text("초기화 시간 확인 중입니다.").into(),
            };
            // A model rejection cannot offer account-wide paid recovery or
            // announce that every account of the provider is exhausted.
            let suffix = if notice.model_limited {
                crate::i18n::static_text("그때까지 이 모델의 새 요청은 오류로 끝납니다.").into()
            } else if let Some(candidate) = credit_candidate(view, provider, now) {
                crate::i18n::format(
                    "{0}의 유료 크레딧을 허용하면 바로 이어서 처리합니다.",
                    &[(candidate.name).to_string()],
                )
            } else {
                let disabled = view
                    .sections
                    .iter()
                    .find(|s| s.provider == provider)
                    .map_or(0, |s| s.disabled_rows.len());
                if disabled > 0 {
                    crate::i18n::format(
                        "꺼진 계정 {0}개를 켜면 이어서 처리할 수 있습니다.",
                        &[disabled.to_string()],
                    )
                } else {
                    crate::i18n::static_text("그때까지 새 요청은 오류로 끝납니다.").into()
                }
            };
            (
                if notice.model_limited {
                    crate::i18n::format(
                        "{0} 이 모델의 계정 모두 한도 도달",
                        &[provider_name.to_string()],
                    )
                } else {
                    crate::i18n::format("{0} 계정 모두 한도 도달", &[provider_name.to_string()])
                },
                format!("{prefix} {suffix}"),
                crate::i18n::static_text("처리할 다른 계정 없음").into(),
            )
        }
    };
    Some(QuotaMessage { title, text, short })
}

fn credit_candidate<'a>(
    view: &'a v2::View,
    provider: &str,
    now: SystemTime,
) -> Option<&'a v2::AccountRow> {
    if provider != "codex" {
        return None;
    }
    let now = DateTime::<Utc>::from(now);
    view.sections
        .iter()
        .filter(|s| s.provider == provider)
        .flat_map(|s| &s.rows)
        .filter(|r| {
            let a = &r.raw;
            v2::credits_eligible_for_resume(a)
                && a.health
                    .until
                    .as_deref()
                    .and_then(v2::timestamp)
                    .is_some_and(|at| at > now)
                && a.extra
                    .get("payment_until")
                    .and_then(|v| v.as_str())
                    .and_then(v2::timestamp)
                    .is_none_or(|at| at <= now)
        })
        .min_by(|a, b| a.name.cmp(&b.name).then_with(|| a.raw.id.cmp(&b.raw.id)))
}

#[derive(Debug, Clone, Copy)]
pub enum AccountKind {
    Relogin,
    LinkStale,
    Billing,
    OrganisationDenied,
    StoreError,
}

pub fn account_message(
    view: &v2::View,
    provider: &str,
    id: &str,
    kind: AccountKind,
    has_alternative: bool,
    now: SystemTime,
) -> Option<(String, String)> {
    let row = account_row(view, provider, id)?;
    let title = |reason| crate::account_notice_title(&row.name, provider, reason);
    let serving = view
        .sections
        .iter()
        .filter(|s| s.provider == provider)
        .flat_map(|s| &s.rows)
        .filter(|r| {
            r.raw.id != id
                && r.raw.user_state == "active"
                && r.raw.active_sessions > 0
                && r.raw.pool_eligible == Some(true)
                && matches!(
                    r.state,
                    v2::DisplayState::InUse | v2::DisplayState::Credits | v2::DisplayState::Drain
                )
        })
        .max_by(|a, b| {
            a.raw
                .active_sessions
                .cmp(&b.raw.active_sessions)
                .then_with(|| b.name.cmp(&a.name))
        });
    let until = row
        .raw
        .health
        .until
        .as_deref()
        .and_then(v2::timestamp)
        .map(|at| time(at, now));
    Some(match kind {
        AccountKind::Relogin => (title(crate::i18n::static_text("다시 로그인 필요")), crate::i18n::format("트레이에서 이 계정의 \"다시 로그인\"을 누르세요. {0}", &[(if has_alternative { serving.map_or_else(|| crate::i18n::static_text("그동안 다른 계정이 처리합니다.").into(),
                |r| crate::i18n::format("그동안 {0}가 처리합니다.", &[(r.name).to_string()])) }
            else { crate::i18n::static_text("처리할 다른 계정이 없습니다.").into() }).to_string()])),
        AccountKind::LinkStale => (title(crate::i18n::static_text("CPA 갱신 멈춤")), crate::i18n::static_text("CPA가 이 계정의 토큰을 갱신하지 않습니다. CPA 실행을 확인하거나 계정 인수를 고려하세요.").into()),
        AccountKind::StoreError => (title(crate::i18n::static_text("토큰 저장 실패")), crate::i18n::static_text("토큰을 저장하지 못해 다시 시도하고 있습니다. 로그를 확인하세요.").into()),
        AccountKind::Billing => (title(crate::i18n::static_text("결제 거절")), until.map_or_else(
            || crate::i18n::static_text("결제 상태를 확인하는 동안 이 계정을 쉽니다.").into(),
            |at| if has_alternative { crate::i18n::format("{0}까지 이 계정을 쉬고 다른 계정이 처리합니다.", &[at.to_string()]) }
                else { crate::i18n::format("{0}까지 이 계정을 쉽니다.", &[at.to_string()]) })),
        AccountKind::OrganisationDenied => (title(crate::i18n::static_text("조직 접근 거부")), until.map_or_else(
            || crate::i18n::static_text("조직 접근 상태를 확인하는 동안 이 계정을 쉽니다.").into(),
            |at| crate::i18n::format("이 계정은 {0}까지 쉽니다.", &[at.to_string()]))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn now() -> SystemTime {
        "2026-10-18T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .unwrap()
            .into()
    }
    fn account(id: &str) -> Value {
        json!({"id":id,"provider":"codex","email":format!("{id}@example.invalid"),
        "user_state":"active","health":{"kind":"cooling","reason":"quota","scope":"account","until":"2026-10-20T12:00:00Z"}})
    }
    fn view(accounts: Vec<Value>) -> v2::View {
        v2::project(
            &json!({"accounts":accounts,"pools":{"codex":{"eligible":0}}}).to_string(),
            now(),
        )
        .unwrap()
    }
    fn context(action: QuotaAction) -> QuotaNotice {
        QuotaNotice {
            model_limited: false,
            window: Some("7d".into()),
            reset_at: Some("2026-10-20T12:00:00Z".parse().unwrap()),
            action,
            next_account: Some("b".into()),
            earliest_account: Some("a".into()),
            observed_at: "2026-10-18T11:00:00Z".parse().unwrap(),
        }
    }
    #[test]
    fn exhausted_offer_priority_disabled_errors_and_aggregate_title() {
        let mut paid = account("paid");
        paid["credits"] = json!({"has_credits":true,"balance":19395.23});
        paid["quota_credit_releasable"] = json!(true);
        let mut off = account("off");
        off["user_state"] = json!("disabled");
        let q = context(QuotaAction::Exhausted);
        for (extras, expected) in [
            (
                vec![paid.clone(), off.clone()],
                "paid의 유료 크레딧을 허용하면 바로 이어서 처리합니다.",
            ),
            (vec![off], "꺼진 계정 1개를 켜면 이어서 처리할 수 있습니다."),
            (vec![], "그때까지 새 요청은 오류로 끝납니다."),
        ] {
            let v = view([vec![account("a")], extras].concat());
            let m = quota_message(&v, "codex", "a", &q, now()).unwrap();
            assert_eq!(m.title, "Codex 계정 모두 한도 도달");
            assert!(m.text.ends_with(expected), "{}", m.text);
            assert!(!m.text.contains("지금 사용 중"));
        }
    }
    #[test]
    fn credit_suggestions_exclude_nonrecoverable_accounts_and_model_scope() {
        let mut paid = account("paid");
        paid["credits"] = json!({"has_credits":true,"balance":3});
        paid["quota_credit_releasable"] = json!(true);
        for (key, value) in [
            ("quota_credit_releasable", json!(false)),
            ("credits", json!({"has_credits":false,"balance":12})),
            ("allow_credits", json!(true)),
            ("drain", json!(true)),
            ("user_state", json!("paused")),
            ("health", json!({"kind":"needs_relogin"})),
            (
                "health",
                json!({"kind":"cooling","reason":"quota","scope":"model:x","until":"2026-10-20T12:00:00Z"}),
            ),
            ("credits", json!({"has_credits":true,"balance":0})),
        ] {
            let mut a = paid.clone();
            a[key] = value;
            assert!(credit_candidate(&view(vec![a]), "codex", now()).is_none());
        }
        let v = view(vec![account("a"), paid]);
        let mut q = context(QuotaAction::Exhausted);
        q.model_limited = true;
        assert!(
            !quota_message(&v, "codex", "a", &q, now())
                .unwrap()
                .text
                .contains("유료 크레딧")
        );
        assert!(credit_candidate(&v, "claude", now()).is_none());
    }
    #[test]
    fn selected_ids_collision_names_and_labels_match_projection() {
        let mut a = account("a");
        a["email"] = json!("same@one.invalid");
        let mut b = account("b");
        b["email"] = json!("same@two.invalid");
        b["health"] = json!({"kind":"ready"});
        let v = view(vec![a, b]);
        let mut q = context(QuotaAction::Failover);
        let m = quota_message(&v, "codex", "a", &q, now()).unwrap();
        assert!(m.title.contains("same@one.invalid"));
        assert!(m.text.contains("same@two.invalid 계정으로"));
        q.next_account = Some("absent".into());
        let m = quota_message(&v, "codex", "a", &q, now()).unwrap();
        assert!(m.text.starts_with("다른 계정으로"));
        assert!(!m.text.contains("same@two"));
        assert!(!m.text.contains("absent"));
        let mut a = account("a");
        a["label"] = json!("내 계정");
        let v = view(vec![a]);
        assert!(
            quota_message(&v, "codex", "a", &context(QuotaAction::Credits), now())
                .unwrap()
                .title
                .starts_with("내 계정 · Codex")
        );
    }
    #[test]
    fn calendar_boundaries_and_paid_copy() {
        let zone = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
        let at = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&zone)
        };
        let n = at("2026-12-31T23:50:00+09:00");
        assert_eq!(
            v2::calendar_time(at("2026-12-31T23:55:00+09:00"), n, false),
            "23:55"
        );
        assert_eq!(
            v2::calendar_time(at("2027-01-01T01:40:00+09:00"), n, false),
            "내일 01:40"
        );
        assert_eq!(
            v2::calendar_time(at("2027-01-02T15:22:00+09:00"), n, false),
            "1/2 15:22"
        );
        let m = quota_message(
            &view(vec![account("a")]),
            "codex",
            "a",
            &context(QuotaAction::Credits),
            now(),
        )
        .unwrap();
        assert!(m.text.starts_with("유료 크레딧으로 계속 처리합니다."));
    }
    #[test]
    fn health_copy_deadline_named_serving_and_honest_fallbacks() {
        let mut a = account("a");
        a["health"]["kind"] = json!("needs_relogin");
        let mut b = account("b");
        b["health"] = json!({"kind":"ready"});
        b["active_sessions"] = json!(2);
        b["pool_eligible"] = json!(true);
        let v = view(vec![a, b]);
        let msg = |kind, other| account_message(&v, "codex", "a", kind, other, now()).unwrap();
        assert!(
            msg(AccountKind::Relogin, true)
                .1
                .ends_with("그동안 b가 처리합니다.")
        );
        assert!(
            msg(AccountKind::Relogin, false)
                .1
                .ends_with("처리할 다른 계정이 없습니다.")
        );
        assert_eq!(
            msg(AccountKind::StoreError, false).0,
            "a · Codex 토큰 저장 실패"
        );
        assert_eq!(
            msg(AccountKind::StoreError, false).1,
            "토큰을 저장하지 못해 다시 시도하고 있습니다. 로그를 확인하세요."
        );
        assert!(
            msg(AccountKind::Billing, true)
                .1
                .ends_with("까지 이 계정을 쉬고 다른 계정이 처리합니다.")
        );
        assert!(
            msg(AccountKind::OrganisationDenied, false)
                .1
                .starts_with("이 계정은 ")
        );
        let v = view(vec![account("a")]);
        assert!(
            account_message(&v, "codex", "a", AccountKind::Relogin, true, now())
                .unwrap()
                .1
                .ends_with("그동안 다른 계정이 처리합니다.")
        );
    }
    #[test]
    fn expired_context_and_unknown_earliest_cannot_invent_metadata() {
        let v = view(vec![account("a"), account("b")]);
        let mut q = context(QuotaAction::Exhausted);
        q.earliest_account = Some("missing".into());
        let m = quota_message(&v, "codex", "a", &q, now()).unwrap();
        assert!(!m.text.contains("missing"));
        assert!(!m.text.contains("초기화: a"));
        q.reset_at = None;
        assert!(
            quota_message(&v, "codex", "a", &q, now())
                .unwrap()
                .text
                .starts_with("초기화 시간 확인 중입니다.")
        );
        q.reset_at = Some("2026-10-18T10:00:00Z".parse().unwrap());
        assert!(quota_message(&v, "codex", "a", &q, now()).is_none());
    }
}

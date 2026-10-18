//! Typed worker-to-shell notice projection. No raw core errors or upstream bodies.
use crate::{
    client::TraySnapshot,
    tray_icon::{
        pixels::Badge,
        signals::{self, Kind, Signal, Subject},
    },
};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, SystemTime},
};

const MAX_PENDING: usize = 32;
const BALLOON_FALLBACK: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Account(String),
    Provider(String),
    Core,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub target: Target,
    pub kind: Kind,
    pub title: String,
    pub text: String,
    queued_at: Duration,
}

#[derive(Clone, Debug)]
pub struct Presentation {
    pub badge: Badge,
    pub tooltip: String,
}

pub struct Controller {
    debounce: signals::Debouncer,
    pending: VecDeque<Notice>,
    active: Option<(Duration, Notice)>,
    presentation: Presentation,
}

/// NUL-terminated shell text with no split UTF-16 surrogate pair.
pub fn shell_text<const N: usize>(text: &str) -> [u16; N] {
    let mut out = [0u16; N];
    let mut used = 0;
    for ch in text.chars() {
        let mut pair = [0u16; 2];
        let encoded = ch.encode_utf16(&mut pair);
        if used + encoded.len() >= N {
            break;
        }
        out[used..used + encoded.len()].copy_from_slice(encoded);
        used += encoded.len();
    }
    out
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            debounce: signals::Debouncer::default(),
            pending: VecDeque::new(),
            active: None,
            presentation: presentation(Badge::Error, 0),
        }
    }
}

fn presentation(badge: Badge, count: usize) -> Presentation {
    let label = match badge {
        Badge::Normal => "정상",
        Badge::Attention => "주의 필요",
        Badge::Error => "코어 연결 끊김",
    };
    Presentation {
        badge,
        tooltip: format!("quota-baton · {label} · 계정 {count}개"),
    }
}

fn target(signal: Signal<'_>) -> Target {
    match signal.subject {
        Subject::Account { id, .. } => Target::Account(id.to_owned()),
        Subject::Provider(provider) => Target::Provider(provider.to_owned()),
        Subject::Core => Target::Core,
    }
}

fn account_message(
    _snapshot: &TraySnapshot,
    account: &crate::client::TrayAccount,
    kind: Kind,
    wall: SystemTime,
) -> (String, String) {
    let name = qb_tray::notice_account_name(
        account.email.as_deref(),
        account.label.as_deref(),
        &account.id,
    );
    let other = account.has_alternative;
    let alternative = if other {
        "그동안 다른 계정이 처리합니다."
    } else {
        "처리할 다른 계정이 없습니다."
    };
    match kind {
        Kind::Relogin => (qb_tray::account_notice_title(&name,&account.provider,"다시 로그인 필요"), format!("트레이에서 이 계정의 \"다시 로그인\"을 누르세요. {alternative}")),
        Kind::LinkStale => (qb_tray::account_notice_title(&name,&account.provider,"CPA 갱신 멈춤"), "CPA가 이 계정의 토큰을 갱신하지 않습니다. CPA 실행을 확인하거나 계정 인수를 고려하세요.".into()),
        Kind::OrganisationDenied => (qb_tray::account_notice_title(&name,&account.provider,"조직 접근 거부"), "이 계정은 6시간 동안 쉽니다.".into()),
        Kind::Billing => {
            let remaining = account.health["until"].as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|at| at.signed_duration_since(chrono::DateTime::<chrono::Utc>::from(wall)).num_seconds());
            let rest = remaining.filter(|s| *s > 0).map_or_else(|| "결제 상태를 확인하는 동안 이 계정을 쉽니다.".into(), |s| {
                // Do not round a partial hour into an invented full-hour hold.
                if s % 3600 == 0 { format!("{}시간 동안 이 계정을 쉽니다.", s / 3600) }
                else { format!("약 {:.1}시간 동안 이 계정을 쉽니다.", s as f64 / 3600.0) }
            });
            (qb_tray::account_notice_title(&name,&account.provider,"결제 거절"), format!("{rest} {alternative}"))
        }
        _ => { let (title, text) = Signal { subject: Subject::Core, kind }.message(); (title.into(), text.into()) }
    }
}

impl Controller {
    pub fn presentation(&self) -> Presentation {
        self.presentation.clone()
    }

    pub fn unavailable(&mut self, account_count: usize) {
        self.presentation = presentation(Badge::Error, account_count);
    }

    pub fn snapshot(
        &mut self,
        snapshot: &TraySnapshot,
        core_ok: bool,
        existing_attention: bool,
        account_count: usize,
        wall: SystemTime,
        elapsed: Duration,
    ) {
        if !core_ok {
            self.unavailable(account_count);
            return;
        }
        let mut found = Vec::new();
        let mut quota_attention = false;
        let mut exhausted_providers = HashSet::new();
        let now = chrono::DateTime::<chrono::Utc>::from(wall);
        for warning in &snapshot.warnings {
            let Some(observed_at) = warning.observed_at else {
                continue;
            };
            let age = now.signed_duration_since(observed_at);
            if age >= chrono::Duration::zero()
                && age < chrono::Duration::minutes(10)
                && let Some(signal) =
                    signals::burst_signal(Some(&warning.kind), warning.burst_new_sessions)
            {
                self.enqueue(signal, elapsed);
            }
        }
        for account in &snapshot.accounts {
            let health = &account.health;
            let cooling_current = health["until"]
                .as_str()
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                .is_some_and(|until| until.with_timezone(&chrono::Utc) > now);
            let health_signal = signals::account_signal(&signals::Account {
                provider: &account.provider,
                id: &account.id,
                user_state: &account.user_state,
                maintains_tokens: matches!(account.source.as_str(), "oauth" | "owned" | "adopted"),
                health_kind: health["kind"].as_str().unwrap_or("unknown"),
                health_reason: health["reason"].as_str(),
                health_scope: health["scope"].as_str(),
                cooling_current,
            });
            quota_attention |= health_signal.is_some_and(|s| s.kind == Kind::Limit);
            if account.user_state == "active"
                && !account.id.is_empty()
                && !account.provider.is_empty()
                && let Some(context) = account
                    .quota_notice
                    .as_ref()
                    .filter(|q| q.current_since(wall, account.last_relevant_state_at))
            {
                let next = context.next_account.as_deref().map(|id| {
                    snapshot
                        .accounts
                        .iter()
                        .find(|a| a.id == id && a.provider == account.provider)
                        .map_or_else(
                            || qb_tray::notice_account_name(None, None, id),
                            |a| {
                                qb_tray::notice_account_name(
                                    a.email.as_deref(),
                                    a.label.as_deref(),
                                    id,
                                )
                            },
                        )
                });
                let name = qb_tray::notice_account_name(
                    account.email.as_deref(),
                    account.label.as_deref(),
                    &account.id,
                );
                let earliest = context.earliest_account.as_deref().map(|id| {
                    snapshot
                        .accounts
                        .iter()
                        .find(|a| a.id == id && a.provider == account.provider)
                        .map_or_else(
                            || qb_tray::notice_account_name(None, None, id),
                            |a| {
                                qb_tray::notice_account_name(
                                    a.email.as_deref(),
                                    a.label.as_deref(),
                                    id,
                                )
                            },
                        )
                });
                if let Some(message) = qb_tray::quota_message(
                    context,
                    &name,
                    &account.provider,
                    next.as_deref(),
                    earliest.as_deref(),
                    wall,
                ) {
                    let kind = match context.action {
                        qb_tray::QuotaAction::Credits => Kind::QuotaCredits,
                        qb_tray::QuotaAction::Failover => Kind::QuotaFailover,
                        qb_tray::QuotaAction::Exhausted => {
                            exhausted_providers.insert(account.provider.as_str());
                            Kind::QuotaExhausted
                        }
                    };
                    found.push((
                        Signal {
                            subject: Subject::Account {
                                provider: &account.provider,
                                id: &account.id,
                            },
                            kind,
                        },
                        (message.title, message.text),
                    ));
                }
            }
            if let Some(signal) = health_signal.filter(|s| s.kind != Kind::Limit) {
                found.push((
                    signal,
                    account_message(snapshot, account, signal.kind, wall),
                ));
            }
        }
        for provider in ["claude", "codex"] {
            // A quota-cooled pool alone cannot tell whether C7 is holding.
            // Only the core's resolved context may turn it into an exhausted toast.
            let unresolved_quota = snapshot.accounts.iter().any(|a| {
                a.provider == provider
                    && a.user_state == "active"
                    && a.health["kind"] == "cooling"
                    && a.health["reason"] == "quota"
            });
            let active = snapshot
                .accounts
                .iter()
                .filter(|a| a.provider == provider && a.user_state == "active")
                .count() as u64;
            if !exhausted_providers.contains(provider)
                && !unresolved_quota
                && let Some(eligible) = snapshot.pools[provider]["eligible"].as_u64()
                && let Some(signal) = signals::pool_signal(&signals::Pool {
                    provider,
                    active,
                    eligible,
                })
            {
                let (title, text) = signal.message();
                found.push((
                    signal,
                    (
                        format!("{} · {title}", qb_tray::provider_name(provider)),
                        text.into(),
                    ),
                ));
            }
        }
        self.presentation = presentation(
            signals::badge(
                core_ok,
                existing_attention || quota_attention,
                &found.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            ),
            account_count,
        );
        let current: HashSet<_> = found
            .iter()
            .map(|(signal, _)| (target(*signal), signal.kind))
            .collect();
        self.pending.retain(|notice| {
            elapsed
                .checked_sub(notice.queued_at)
                .is_none_or(|age| age < Duration::from_secs(600))
                && (notice.target == Target::Core
                    || current.contains(&(notice.target.clone(), notice.kind)))
        });
        for (signal, message) in found {
            self.enqueue_message(signal, message, elapsed);
        }
    }

    pub fn burst(&mut self, observed: u64, elapsed: Duration) {
        if let Some(signal) = signals::burst_signal(Some("burst"), Some(observed)) {
            self.enqueue(signal, elapsed);
        }
    }

    pub fn restarted(&mut self, elapsed: Duration) {
        self.enqueue(
            Signal {
                subject: Subject::Core,
                kind: Kind::CoreRestart,
            },
            elapsed,
        );
    }

    fn enqueue(&mut self, signal: Signal<'_>, elapsed: Duration) {
        let (title, text) = signal.message();
        self.enqueue_message(signal, (title.into(), text.into()), elapsed);
    }

    fn enqueue_message(
        &mut self,
        signal: Signal<'_>,
        (title, text): (String, String),
        elapsed: Duration,
    ) {
        let notice_target = target(signal);
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.target == notice_target && pending.kind == signal.kind)
        {
            // Debounce governs delivery, not the truth of a still-queued message.
            if pending.title != title || pending.text != text {
                pending.title = title;
                pending.text = text;
                pending.queued_at = elapsed;
            }
            return;
        }
        if self.pending.len() >= MAX_PENDING || !self.debounce.allow(signal, elapsed) {
            return;
        }
        self.pending.push_back(Notice {
            target: notice_target,
            kind: signal.kind,
            title,
            text,
            queued_at: elapsed,
        });
    }

    /// Never show stale account notices while the core is unavailable. Shell
    /// hide/timeout/click completes active notices; fallback handles missing callbacks.
    pub fn next(&mut self, elapsed: Duration) -> Option<Notice> {
        if self.presentation.badge == Badge::Error {
            return None;
        }
        if self.active.as_ref().is_some_and(|(at, _)| {
            elapsed
                .checked_sub(*at)
                .is_none_or(|age| age < BALLOON_FALLBACK)
        }) {
            return None;
        }
        self.active = None;
        self.pending.retain(|notice| {
            elapsed
                .checked_sub(notice.queued_at)
                .is_none_or(|age| age < Duration::from_secs(600))
        });
        let notice = self.pending.pop_front()?;
        self.active = Some((elapsed, notice.clone()));
        Some(notice)
    }

    pub fn finish(&mut self) -> Option<Target> {
        self.active.take().map(|(_, n)| n.target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn quota_snapshot(action: &str) -> TraySnapshot {
        serde_json::from_value(json!({"accounts":[{"id":"a","provider":"codex","email":"acct-alpha@example.test",
            "user_state":"active","health":{"kind":"ready"}, "allow_credits":false,
            "quota_notice":{"window":"7d","reset_at":"2099-01-01T00:00:00Z","action":action,
            "next_account":"b","earliest_account":"b","observed_at":"2026-01-01T00:00:00Z"}},
            {"id":"b","provider":"codex","email":"hello.sample-acct@example.test","user_state":"active","health":{"kind":"ready"}}],
            "pools":{"codex":{"eligible":0}}})).unwrap()
    }

    #[test]
    fn quota_actions_renotify_but_refresh_and_allow_off_do_not_and_exhaustion_is_not_duplicated() {
        let mut c = Controller::default();
        for (t, action, kind, text) in [
            (0, "credits", Kind::QuotaCredits, "유료 크레딧"),
            (
                1,
                "failover",
                Kind::QuotaFailover,
                "hello.sample-acct 계정으로 이어서 처리",
            ),
            (2, "exhausted", Kind::QuotaExhausted, "새 요청은 한도 오류"),
        ] {
            let mut s = quota_snapshot(action);
            // Keep generic exhaustion out of the first two fixture cases.
            if action != "exhausted" {
                s.pools["codex"]["eligible"] = json!(1);
            }
            observe(&mut c, &s, t);
            let n = c.next(Duration::from_secs(t)).unwrap();
            assert_eq!(n.kind, kind);
            assert!(n.title.contains("acct-alpha"));
            assert!(!n.title.contains('@'));
            assert!(n.text.contains(text));
            if action == "exhausted" {
                assert!(n.text.contains("가장 빠른 초기화: hello.sample-acct"));
            }
            c.finish();
            assert!(c.next(Duration::from_secs(t)).is_none());
            s.accounts[0].allow_credits = true;
            observe(&mut c, &s, t);
            s.accounts[0].allow_credits = false;
            observe(&mut c, &s, t);
            assert!(c.next(Duration::from_secs(t)).is_none());
        }
    }

    #[test]
    fn absent_context_rate_preferences_and_expiry_cannot_claim_paid_credits_or_limit() {
        for reason in ["quota", "rate"] {
            let mut s = snapshot(
                json!({"kind":"cooling","reason":reason,"scope":"account","until":"2099-01-01T00:00:00Z"}),
                "active",
                Some(1),
            );
            s.accounts[0].allow_credits = true;
            let mut c = Controller::default();
            observe(&mut c, &s, 0);
            assert!(c.next(Duration::ZERO).is_none());
            s.accounts[0].allow_credits = false;
            observe(&mut c, &s, 1);
            assert!(c.next(Duration::from_secs(1)).is_none());
            if reason == "quota" {
                assert_eq!(c.presentation().badge, Badge::Attention);
            }
        }
        let mut s = quota_snapshot("credits");
        s.pools["codex"]["eligible"] = json!(1);
        s.accounts[0].quota_notice.as_mut().unwrap().reset_at =
            Some(chrono::Utc::now() - chrono::Duration::seconds(1));
        let mut c = Controller::default();
        observe(&mut c, &s, 0);
        assert!(c.next(Duration::ZERO).is_none());
        // No UI reset-hold heuristic: core alone decides whether to emit context.
        s.accounts[0].quota_notice.as_mut().unwrap().reset_at =
            Some(chrono::Utc::now() + chrono::Duration::seconds(5));
        observe(&mut c, &s, 1);
        assert_eq!(
            c.next(Duration::from_secs(1)).unwrap().kind,
            Kind::QuotaCredits
        );
        c.finish();
        let mut fresh = Controller::default();
        observe(&mut fresh, &s, 0);
        s.accounts[0].quota_notice = None;
        observe(&mut fresh, &s, 1);
        assert!(fresh.next(Duration::from_secs(1)).is_none());
    }

    #[test]
    fn account_specific_relogin_cpa_billing_and_organisation_copy() {
        let wall: SystemTime = "2026-10-18T12:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap()
            .into();
        let mut s = snapshot(
            json!({"kind":"cooling","reason":"payment","scope":"account","until":"2026-10-18T15:00:00Z"}),
            "active",
            Some(0),
        );
        s.accounts[0].email = Some("hello.sample-acct@example.test".into());
        s.accounts[0].provider = "claude".into();
        s.pools = json!({"claude":{"eligible":0}});
        let (title, body) = account_message(&s, &s.accounts[0], Kind::Billing, wall);
        assert_eq!(title, "hello.sample-acct · Claude 결제 거절");
        assert!(body.contains("3시간"));
        assert!(body.contains("처리할 다른 계정이 없습니다"));
        let (_, body) = account_message(&s, &s.accounts[0], Kind::Relogin, wall);
        assert!(body.contains("처리할 다른 계정이 없습니다"));
        s.pools["claude"]["eligible"] = json!(1);
        // A pool can include self; the explicit core field decides alternatives.
        let (_, body) = account_message(&s, &s.accounts[0], Kind::Relogin, wall);
        assert!(body.contains("처리할 다른 계정이 없습니다"));
        s.accounts[0].has_alternative = true;
        let (_, body) = account_message(&s, &s.accounts[0], Kind::Relogin, wall);
        assert!(body.contains("그동안 다른 계정이 처리합니다"));
        let (_, body) = account_message(&s, &s.accounts[0], Kind::Billing, wall);
        assert!(body.contains("그동안 다른 계정이 처리합니다"));
        let (title, body) = account_message(&s, &s.accounts[0], Kind::LinkStale, wall);
        assert!(title.contains("CPA 갱신 멈춤"));
        assert!(body.contains("CPA가 이 계정의 토큰을 갱신하지 않습니다"));
        let (_, body) = account_message(&s, &s.accounts[0], Kind::OrganisationDenied, wall);
        assert_eq!(body, "이 계정은 6시간 동안 쉽니다.");
    }

    #[test]
    fn quota_pool_without_resolved_context_never_bypasses_reset_hold_suppression() {
        for remaining in [5, 172800] {
            let mut s = quota_snapshot("exhausted");
            let context = s.accounts[0].quota_notice.take().unwrap();
            s.accounts[0].health = json!({"kind":"cooling","reason":"quota","scope":"account",
                "until":chrono::Utc::now()+chrono::Duration::seconds(remaining)});
            s.pools["codex"]["eligible"] = json!(0);
            let mut c = Controller::default();
            observe(&mut c, &s, 0);
            assert!(
                c.next(Duration::ZERO).is_none(),
                "generic pool toast bypassed the core's hold decision"
            );
            assert_eq!(c.presentation().badge, Badge::Attention);
            // Once the relay really resolves exhaustion, the same pool can notify.
            s.accounts[0].quota_notice = Some(context);
            observe(&mut c, &s, 1);
            assert_eq!(
                c.next(Duration::from_secs(1)).unwrap().kind,
                Kind::QuotaExhausted
            );
        }
    }

    #[test]
    fn long_account_title_keeps_provider_window_inside_native_utf16_buffer() {
        for local in [
            "long-synthetic-account".repeat(8),
            "synthetic-😀".repeat(30),
        ] {
            let mut s = quota_snapshot("failover");
            s.accounts[0].email = Some(format!("{local}@example.test"));
            s.pools["codex"]["eligible"] = json!(1);
            let mut c = Controller::default();
            observe(&mut c, &s, 0);
            let notice = c.next(Duration::ZERO).unwrap();
            let native = shell_text::<64>(&notice.title);
            let end = native.iter().position(|unit| *unit == 0).unwrap();
            assert_eq!(String::from_utf16(&native[..end]).unwrap(), notice.title);
            assert!(notice.title.ends_with("Codex 주간 한도 도달"));
            assert!(notice.title.contains('…'));
        }
    }

    #[test]
    fn authoritative_snapshot_updates_queued_target_and_reset_without_renotifying() {
        for action in ["failover", "exhausted"] {
            let mut s = quota_snapshot(action);
            if action == "failover" {
                s.pools["codex"]["eligible"] = json!(1);
            }
            let mut successor = s.accounts[1].clone();
            successor.id = "c".into();
            successor.email = Some("fresh-successor@example.test".into());
            s.accounts.push(successor);
            let mut c = Controller::default();
            observe(&mut c, &s, 0); // Queued, never shown.
            assert_eq!(c.pending.len(), 1);
            let original = c.pending[0].text.clone();
            let context = s.accounts[0].quota_notice.as_mut().unwrap();
            context.next_account = Some("c".into());
            context.earliest_account = Some("c".into());
            context.reset_at = Some("2099-02-02T03:04:00Z".parse().unwrap());
            observe(&mut c, &s, 1);
            assert_eq!(c.pending.len(), 1);
            let notice = c.next(Duration::from_secs(1)).unwrap();
            assert!(notice.text.contains("fresh-successor"));
            assert!(!notice.text.contains("hello.sample-acct"));
            assert_ne!(notice.text, original);
            c.finish();
            // A change after delivery remains within the same-kind debounce.
            s.accounts[0].quota_notice.as_mut().unwrap().next_account = Some("b".into());
            s.accounts[0]
                .quota_notice
                .as_mut()
                .unwrap()
                .earliest_account = Some("b".into());
            observe(&mut c, &s, 2);
            assert!(c.next(Duration::from_secs(2)).is_none());
        }
    }

    #[test]
    fn older_context_is_suppressed_and_earliest_null_or_unknown_id_stays_truthful() {
        let mut s = quota_snapshot("exhausted");
        s.pools["codex"]["eligible"] = json!(1);
        s.accounts[0].last_relevant_state_at = Some("2026-01-01T00:00:01Z".parse().unwrap());
        let mut c = Controller::default();
        observe(&mut c, &s, 0);
        assert!(c.next(Duration::ZERO).is_none());
        s.accounts[0].last_relevant_state_at = Some("2026-01-01T00:00:00Z".parse().unwrap());
        s.accounts[0]
            .quota_notice
            .as_mut()
            .unwrap()
            .earliest_account = Some("unknown-id".into());
        observe(&mut c, &s, 1);
        assert!(
            c.next(Duration::from_secs(1))
                .unwrap()
                .text
                .contains("가장 빠른 초기화: unknown-id")
        );
        s.accounts[0]
            .quota_notice
            .as_mut()
            .unwrap()
            .earliest_account = None;
        let mut c = Controller::default();
        observe(&mut c, &s, 0);
        let text = c.next(Duration::ZERO).unwrap().text;
        assert!(text.contains("가장 빠른 초기화:"));
        assert!(!text.contains("acct-alpha"));
        assert!(!text.contains("hello.sample-acct"));
    }
    fn snapshot(health: serde_json::Value, state: &str, eligible: Option<u64>) -> TraySnapshot {
        serde_json::from_value(json!({"accounts":[{"id":"cx-synthetic","provider":"codex",
            "user_state":state,"source":{"kind":"cpa","mode":"adopted"},"health":health}],
            "pools":eligible.map(|n| json!({"codex":{"eligible":n}})).unwrap_or_default()}))
        .unwrap()
    }
    fn observe(c: &mut Controller, s: &TraySnapshot, t: u64) {
        c.snapshot(
            s,
            true,
            false,
            s.accounts.len(),
            SystemTime::now(),
            Duration::from_secs(t),
        );
    }
    #[test]
    fn hidden_tray_recovers_recent_burst_from_snapshot_once_without_badge_change() {
        let mut c = Controller::default();
        let mut s = snapshot(json!({"kind":"ready"}), "active", Some(1));
        s.warnings = vec![crate::client::TrayWarning {
            model_limited: false,
            kind: "burst".into(),
            burst_new_sessions: Some(31),
            observed_at: Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
            account: None,
            window: None,
            reset_at: None,
            action: None,
            next_account: None,
            earliest_account: None,
        }];
        observe(&mut c, &s, 0);
        assert_eq!(c.presentation().badge, Badge::Normal);
        assert_eq!(c.next(Duration::ZERO).unwrap().kind, Kind::Burst);
        c.finish();
        observe(&mut c, &s, 10);
        assert!(c.next(Duration::from_secs(10)).is_none());
        s.warnings[0].observed_at = Some(chrono::Utc::now() - chrono::Duration::minutes(11));
        let mut fresh = Controller::default();
        observe(&mut fresh, &s, 0);
        assert!(fresh.next(Duration::ZERO).is_none());
    }

    #[test]
    fn owned_disabled_relogin_is_actionable_but_missing_pools_do_not_invent_exhaustion() {
        let mut c = Controller::default();
        observe(
            &mut c,
            &snapshot(json!({"kind":"needs_relogin"}), "disabled", None),
            0,
        );
        assert_eq!(c.presentation().badge, Badge::Attention);
        assert_eq!(c.next(Duration::ZERO).unwrap().kind, Kind::Relogin);
        c.finish();
        assert!(c.next(Duration::ZERO).is_none());
    }
    #[test]
    fn pending_recovery_is_pruned_and_stale_or_model_cooling_is_not_a_notice() {
        let mut c = Controller::default();
        observe(
            &mut c,
            &snapshot(json!({"kind":"needs_relogin"}), "active", None),
            0,
        );
        observe(
            &mut c,
            &snapshot(json!({"kind":"ready"}), "active", Some(1)),
            1,
        );
        assert!(c.next(Duration::from_secs(1)).is_none());
        for health in [
            json!({"kind":"cooling","reason":"quota","scope":"account","until":"2000-01-01T00:00:00Z"}),
            json!({"kind":"cooling","reason":"permission","scope":"model:x","until":"2099-01-01T00:00:00Z"}),
        ] {
            observe(&mut c, &snapshot(health, "active", None), 2);
            assert!(c.next(Duration::from_secs(2)).is_none());
        }
    }
    #[test]
    fn core_failure_defers_notices_and_thirty_minute_debounce_survives_refresh() {
        let s = snapshot(json!({"kind":"ready"}), "active", Some(0));
        let mut c = Controller::default();
        observe(&mut c, &s, 0);
        c.unavailable(1);
        assert!(c.next(Duration::ZERO).is_none());
        observe(&mut c, &s, 1);
        assert_eq!(
            c.next(Duration::from_secs(1)).unwrap().kind,
            Kind::ProviderExhausted
        );
        c.finish();
        observe(&mut c, &s, 1799);
        assert!(c.next(Duration::from_secs(1799)).is_none());
        observe(&mut c, &s, 1800);
        assert!(c.next(Duration::from_secs(1800)).is_some());
    }
    #[test]
    fn balloons_are_serialized_and_burst_data_never_becomes_user_text() {
        let mut c = Controller::default();
        observe(
            &mut c,
            &snapshot(json!({"kind":"ready"}), "active", Some(1)),
            0,
        );
        c.burst(31, Duration::ZERO);
        c.burst(999, Duration::ZERO);
        let burst = c.next(Duration::ZERO).unwrap();
        assert_eq!(burst.kind, Kind::Burst);
        assert!(!burst.text.contains("999"));
        assert!(c.next(Duration::ZERO).is_none());
        assert_eq!(c.finish(), Some(Target::Core));
        assert!(c.next(Duration::ZERO).is_none());
    }
    #[test]
    fn expired_burst_is_not_replayed_and_restart_has_its_own_debounce() {
        let mut c = Controller::default();
        observe(
            &mut c,
            &snapshot(json!({"kind":"ready"}), "active", Some(1)),
            0,
        );
        c.burst(31, Duration::ZERO);
        assert!(c.next(Duration::from_secs(600)).is_none());
        c.restarted(Duration::from_secs(600));
        let notice = c.next(Duration::from_secs(600)).unwrap();
        assert_eq!(notice.kind, Kind::CoreRestart);
        assert_eq!(c.finish(), Some(Target::Core));
        c.restarted(Duration::from_secs(601));
        assert!(c.next(Duration::from_secs(601)).is_none());
    }
    #[test]
    fn shell_tooltip_is_terminated_and_never_splits_a_surrogate_pair() {
        let short: [u16; 3] = shell_text("가😀나");
        assert_eq!(String::from_utf16(&short[..1]).unwrap(), "가");
        assert_eq!(&short[1..], &[0, 0]);
        let exact: [u16; 4] = shell_text("가😀나");
        assert_eq!(String::from_utf16(&exact[..3]).unwrap(), "가😀");
        assert_eq!(exact[3], 0);
        let empty: [u16; 0] = shell_text("😀");
        assert!(empty.is_empty());
    }
}

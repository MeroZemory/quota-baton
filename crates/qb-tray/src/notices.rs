//! Typed worker-to-shell notice projection. No raw core errors or upstream bodies.
use crate::{
    client::TraySnapshot,
    tray_icon::{
        pixels::Badge,
        signals::{self, Kind, Signal, Subject},
    },
};
use qb_tray::i18n;
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

fn presentation(badge: Badge, _count: usize) -> Presentation {
    Presentation {
        badge,
        tooltip: i18n::text("quota-baton · 코어에 연결할 수 없음").into_owned(),
    }
}

fn projection(snapshot: &TraySnapshot, wall: SystemTime) -> Option<qb_tray::v2::View> {
    qb_tray::v2::project(&serde_json::to_string(snapshot).ok()?, wall).ok()
}

fn target(signal: Signal<'_>) -> Target {
    if signal.kind == Kind::QuotaExhausted
        && let Subject::Account { provider, .. } = signal.subject
    {
        return Target::Provider(provider.to_owned());
    }
    match signal.subject {
        Subject::Account { id, .. } => Target::Account(id.to_owned()),
        Subject::Provider(provider) => Target::Provider(provider.to_owned()),
        Subject::Core => Target::Core,
    }
}

fn account_message(
    snapshot: &TraySnapshot,
    account: &crate::client::TrayAccount,
    kind: Kind,
    wall: SystemTime,
) -> (String, String) {
    use qb_tray::v2_notices::AccountKind;
    let typed = match kind {
        Kind::Relogin => Some(AccountKind::Relogin),
        Kind::LinkStale => Some(AccountKind::LinkStale),
        Kind::Billing => Some(AccountKind::Billing),
        Kind::OrganisationDenied => Some(AccountKind::OrganisationDenied),
        Kind::StoreError => Some(AccountKind::StoreError),
        _ => None,
    };
    if let Some(kind) = typed
        && let Some(view) = projection(snapshot, wall)
        && let Some(message) = qb_tray::v2_notices::account_message(
            &view,
            &account.provider,
            &account.id,
            kind,
            account.has_alternative,
            wall,
        )
    {
        return message;
    }
    let (title, text) = Signal {
        subject: Subject::Core,
        kind,
    }
    .message();
    (
        i18n::text(title).into_owned(),
        i18n::text(text).into_owned(),
    )
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
        _existing_attention: bool,
        account_count: usize,
        wall: SystemTime,
        elapsed: Duration,
    ) {
        if !core_ok {
            self.unavailable(account_count);
            return;
        }
        let Some(view) = projection(snapshot, wall) else {
            self.unavailable(account_count);
            return;
        };
        let mut found = Vec::new();
        let mut exhausted_providers = HashSet::new();
        let now = chrono::DateTime::<chrono::Utc>::from(wall);
        for warning in &snapshot.warnings {
            let Some(observed_at) = warning.observed_at else {
                continue;
            };
            let age = now.signed_duration_since(observed_at);
            if age >= chrono::Duration::zero() && age < chrono::Duration::minutes(10) {
                let kind = match warning.kind.as_str() {
                    "move_breaker" => Some(Kind::MoveBreaker),
                    "move_reread" => Some(Kind::MoveReread),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let signal = Signal {
                        subject: Subject::Core,
                        kind,
                    };
                    let (title, text) = signal.message();
                    let (title, text) = (
                        i18n::text(title).into_owned(),
                        i18n::text(text).into_owned(),
                    );
                    let account_name = |id: &str| {
                        snapshot
                            .accounts
                            .iter()
                            .find(|a| a.id == id)
                            .and_then(|a| {
                                a.label
                                    .as_deref()
                                    .filter(|s| !s.is_empty())
                                    .or(a.email.as_deref())
                            })
                            .unwrap_or(id)
                            .chars()
                            .filter(|ch| !ch.is_control())
                            .take(36)
                            .collect::<String>()
                    };
                    let detail = if kind == Kind::MoveBreaker {
                        warning
                            .move_list
                            .iter()
                            .take(6)
                            .map(|m| format!("{} → {}", account_name(&m.from), account_name(&m.to)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    } else {
                        warning
                            .reread_tokens
                            .map(|n| i18n::format("최근 1시간 최소 {} 토큰", &[n.to_string()]))
                            .unwrap_or_default()
                    };
                    found.push((signal, (title.to_owned(), format!("{text} {detail}"))));
                }
            }

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
            if account.user_state == "active"
                && !account.id.is_empty()
                && !account.provider.is_empty()
                && let Some(context) = account.quota_notice.as_ref().filter(|q| {
                    q.current_since(wall, account.last_relevant_state_at)
                        && (q.action != qb_tray::QuotaAction::Credits || account.allow_credits)
                })
                && let Some(message) = qb_tray::v2_notices::quota_message(
                    &view,
                    &account.provider,
                    &account.id,
                    context,
                    wall,
                )
            {
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
                let (title, text) = (
                    i18n::text(title).into_owned(),
                    i18n::text(text).into_owned(),
                );
                found.push((
                    signal,
                    (
                        format!("{} · {title}", qb_tray::provider_name(provider)),
                        text,
                    ),
                ));
            }
        }
        self.presentation = Presentation {
            badge: match view.badge {
                qb_tray::v2::Badge::Normal => Badge::Normal,
                qb_tray::v2::Badge::Attention => Badge::Attention,
            },
            tooltip: view.tray_tooltip,
        };
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
        let (title, text) = (
            i18n::text(title).into_owned(),
            i18n::text(text).into_owned(),
        );
        self.enqueue_message(signal, (title, text), elapsed);
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
            "user_state":"active","health":{"kind":"ready"}, "allow_credits":true,
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
            (
                2,
                "exhausted",
                Kind::QuotaExhausted,
                "새 요청은 오류로 끝납니다",
            ),
        ] {
            let mut s = quota_snapshot(action);
            // Keep generic exhaustion out of the first two fixture cases.
            if action != "exhausted" {
                s.pools["codex"]["eligible"] = json!(1);
            }
            observe(&mut c, &s, t);
            let n = c.next(Duration::from_secs(t)).unwrap();
            assert_eq!(n.kind, kind);
            if action == "exhausted" {
                assert_eq!(n.title, "Codex 계정 모두 한도 도달");
                assert_eq!(n.target, Target::Provider("codex".into()));
            } else {
                assert!(n.title.contains("acct-alpha"));
            }
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
            assert_eq!(c.presentation().badge, Badge::Normal);
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
        assert_eq!(
            body,
            format!(
                "{}까지 이 계정을 쉽니다.",
                qb_tray::v2_notices::time("2026-10-18T15:00:00Z".parse().unwrap(), wall)
            )
        );
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
        assert!(body.ends_with("까지 이 계정을 쉬고 다른 계정이 처리합니다."));
        let (title, body) = account_message(&s, &s.accounts[0], Kind::LinkStale, wall);
        assert!(title.contains("CPA 갱신 멈춤"));
        assert!(body.contains("CPA가 이 계정의 토큰을 갱신하지 않습니다"));
        let (_, body) = account_message(&s, &s.accounts[0], Kind::OrganisationDenied, wall);
        assert_eq!(
            body,
            format!(
                "이 계정은 {}까지 쉽니다.",
                qb_tray::v2_notices::time("2026-10-18T15:00:00Z".parse().unwrap(), wall)
            )
        );
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
                .starts_with("가장 빠른 초기화:")
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
            move_list: Vec::new(),
            reread_tokens: None,
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
    fn r14_runtime_warnings_decode_name_moves_and_expire_without_repeat() {
        for (kind, expected) in [
            ("move_breaker", Kind::MoveBreaker),
            ("move_reread", Kind::MoveReread),
        ] {
            let mut s = snapshot(json!({"kind":"ready"}), "active", Some(1));
            s.accounts[0].label = Some("테스트 계정".into());
            s.warnings = vec![
                serde_json::from_value(json!({
                    "kind":kind,"observed_at":chrono::Utc::now()-chrono::Duration::seconds(1),
                    "move_list":[{"from":"cx-synthetic","to":"cx-other"}],"reread_tokens":1234
                }))
                .unwrap(),
            ];
            let mut controller = Controller::default();
            observe(&mut controller, &s, 0);
            let notice = controller.next(Duration::ZERO).unwrap();
            assert_eq!(notice.kind, expected);
            assert!(notice.text.contains(if kind == "move_breaker" {
                "테스트 계정 → cx-other"
            } else {
                "최소 1234 토큰"
            }));
            controller.finish();
            observe(&mut controller, &s, 1);
            assert!(controller.next(Duration::from_secs(1)).is_none());
            s.warnings[0].observed_at = Some(chrono::Utc::now() - chrono::Duration::minutes(11));
            let mut fresh = Controller::default();
            observe(&mut fresh, &s, 0);
            assert!(fresh.next(Duration::ZERO).is_none());
        }
    }

    #[test]
    fn owned_disabled_relogin_is_actionable_but_missing_pools_do_not_invent_exhaustion() {
        let mut c = Controller::default();
        observe(
            &mut c,
            &snapshot(json!({"kind":"needs_relogin"}), "disabled", None),
            0,
        );
        assert_eq!(c.presentation().badge, Badge::Normal);
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
    fn v2_shell_matches_projected_summary_and_ignores_legacy_attention() {
        for health in [
            json!({"kind":"ready"}),
            json!({"kind":"cooling","reason":"quota","scope":"account","until":"2099-01-01T00:00:00Z"}),
        ] {
            let s = snapshot(health, "active", Some(1));
            let now = SystemTime::now();
            let v = projection(&s, now).unwrap();
            let mut c = Controller::default();
            c.snapshot(&s, true, true, s.accounts.len(), now, Duration::ZERO);
            assert_eq!(c.presentation().badge, Badge::Normal);
            assert_eq!(c.presentation().tooltip, v.tray_tooltip);
            assert!(c.presentation().tooltip.contains("Codex 사용 가능"));
            c.unavailable(s.accounts.len());
            assert_eq!(c.presentation().badge, Badge::Error);
            assert_eq!(
                c.presentation().tooltip,
                "quota-baton · 코어에 연결할 수 없음"
            );
        }
    }

    #[test]
    fn allow_off_prunes_unsent_paid_notice_and_preferences_alone_never_notify() {
        let mut s = quota_snapshot("credits");
        s.pools["codex"]["eligible"] = json!(1);
        let mut c = Controller::default();
        observe(&mut c, &s, 0);
        assert_eq!(c.pending.len(), 1);
        s.accounts[0].allow_credits = false;
        observe(&mut c, &s, 1);
        assert!(c.pending.is_empty());
        assert!(c.next(Duration::from_secs(1)).is_none());
        let mut fresh = Controller::default();
        observe(&mut fresh, &s, 0);
        assert!(fresh.next(Duration::ZERO).is_none());
        s.accounts[0].quota_notice = None;
        s.accounts[0].allow_credits = true;
        observe(&mut fresh, &s, 2);
        assert!(fresh.next(Duration::from_secs(2)).is_none());
    }

    #[test]
    fn store_error_uses_critical_kind_and_spec_copy() {
        let s = snapshot(
            json!({"kind":"cooling","reason":"store_error","scope":"account","until":"2099-01-01T00:00:00Z"}),
            "active",
            Some(0),
        );
        let mut c = Controller::default();
        observe(&mut c, &s, 0);
        let notice = c.next(Duration::ZERO).unwrap();
        assert_eq!(notice.kind, Kind::StoreError);
        assert!(notice.title.ends_with("토큰 저장 실패"));
        assert_eq!(
            notice.text,
            "토큰을 저장하지 못해 다시 시도하고 있습니다. 로그를 확인하세요."
        );
        assert_eq!(notice.target, Target::Account("cx-synthetic".into()));
        assert_eq!(c.presentation().badge, Badge::Attention);
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

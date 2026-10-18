//! Typed worker-to-shell notice projection. Never formats raw core errors or labels.
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
    pub title: &'static str,
    pub text: &'static str,
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
            if let Some(signal) = signals::account_signal(&signals::Account {
                provider: &account.provider,
                id: &account.id,
                user_state: &account.user_state,
                maintains_tokens: matches!(account.source.as_str(), "oauth" | "owned" | "adopted"),
                health_kind: health["kind"].as_str().unwrap_or("unknown"),
                health_reason: health["reason"].as_str(),
                health_scope: health["scope"].as_str(),
                cooling_current,
            }) {
                found.push(signal);
            }
        }
        for provider in ["claude", "codex"] {
            let active = snapshot
                .accounts
                .iter()
                .filter(|a| a.provider == provider && a.user_state == "active")
                .count() as u64;
            if let Some(eligible) = snapshot.pools[provider]["eligible"].as_u64()
                && let Some(signal) = signals::pool_signal(&signals::Pool {
                    provider,
                    active,
                    eligible,
                })
            {
                found.push(signal);
            }
        }
        self.presentation = presentation(
            signals::badge(core_ok, existing_attention, &found),
            account_count,
        );
        let current: HashSet<_> = found
            .iter()
            .map(|signal| (target(*signal), signal.kind))
            .collect();
        self.pending.retain(|notice| {
            elapsed
                .checked_sub(notice.queued_at)
                .is_none_or(|age| age < Duration::from_secs(600))
                && (notice.target == Target::Core
                    || current.contains(&(notice.target.clone(), notice.kind)))
        });
        for signal in found {
            self.enqueue(signal, elapsed);
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
        if self.pending.len() >= MAX_PENDING || !self.debounce.allow(signal, elapsed) {
            return;
        }
        let (title, text) = signal.message();
        self.pending.push_back(Notice {
            target: target(signal),
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
            kind: "burst".into(),
            burst_new_sessions: Some(31),
            observed_at: Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
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

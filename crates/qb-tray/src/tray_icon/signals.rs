//! Pure classification over a narrow projection of the current core payload.
//! No parsing of translated chips, clocks, bearer data, or upstream bodies.
#[cfg(test)]
use super::pixels::Badge;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Relogin,
    LinkStale,
    Limit,
    QuotaCredits,
    QuotaFailover,
    QuotaExhausted,
    Billing,
    StoreError,
    ProviderExhausted,
    OrganisationDenied,
    Burst,
    CoreRestart,
    MoveBreaker,
    MoveReread,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Subject<'a> {
    Account {
        provider: &'a str,
        id: &'a str,
    },
    Provider(&'a str),
    /// No originator/UA retention: one global burst warning per 30 minutes.
    Core,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal<'a> {
    pub subject: Subject<'a>,
    pub kind: Kind,
}

impl Signal<'_> {
    /// Generic fallback; account context is formatted by the typed adapter.
    pub fn message(self) -> (&'static str, &'static str) {
        match self.kind {
            Kind::Relogin => (
                "다시 로그인 필요",
                "트레이에서 이 계정의 \"다시 로그인\"을 누르세요.",
            ),
            Kind::LinkStale => (
                "CPA 갱신 멈춤",
                "CPA가 이 계정의 토큰을 갱신하지 않습니다. CPA 실행을 확인하거나 계정 인수를 고려하세요.",
            ),
            Kind::Limit | Kind::QuotaCredits | Kind::QuotaFailover | Kind::QuotaExhausted => (
                "계정 한도 도달",
                "트레이에서 한도 초기화 시간을 확인하거나 꺼진 계정을 켜세요.",
            ),
            Kind::ProviderExhausted => (
                "사용 가능한 계정 없음",
                "해당 제공자의 계정 상태를 확인하고 계정을 추가하거나 켜세요.",
            ),
            Kind::OrganisationDenied => {
                ("조직 접근 거부", "해당 계정의 조직 접근 권한을 확인하세요.")
            }
            Kind::Billing => ("결제 거절", "트레이에서 이 계정의 결제 상태를 확인하세요."),
            Kind::StoreError => (
                "토큰 저장 실패",
                "토큰을 저장하지 못해 다시 시도하고 있습니다. 로그를 확인하세요.",
            ),
            Kind::CoreRestart => (
                "코어 다시 시작",
                "예기치 않게 종료된 코어를 다시 시작했습니다. 계정 상태를 확인하세요.",
            ),
            Kind::MoveBreaker => (
                "반복 계정 이동 차단",
                "기존 계정에서 대기합니다. 대시보드에서 이동 기록을 확인하세요.",
            ),
            Kind::MoveReread => (
                "계정 이동 토큰 비용 증가",
                "계정 이동으로 문맥을 다시 읽는 비용이 늘었습니다. 대시보드의 측정값과 미측정 건수를 확인하세요.",
            ),
            Kind::Burst => (
                "새 대화 급증",
                "클라이언트의 반복 실행을 확인하세요. 10분 동안 새 대화가 설정 한도를 넘었습니다.",
            ),
        }
    }
}

/// Borrow directly from TrayAccount fields and health JSON string members.
pub struct Account<'a> {
    pub provider: &'a str,
    pub id: &'a str,
    pub user_state: &'a str,
    /// D17 keeps owned/adopted credentials alive even while sessions are disabled.
    pub maintains_tokens: bool,
    pub health_kind: &'a str,
    pub health_reason: Option<&'a str>,
    pub health_scope: Option<&'a str>,
    /// Adapter must expire cooling against snapshot time before setting true.
    pub cooling_current: bool,
}

pub fn account_signal<'a>(account: &Account<'a>) -> Option<Signal<'a>> {
    if account.id.is_empty()
        || account.provider.is_empty()
        || !matches!(account.user_state, "active" | "paused" | "disabled")
    {
        return None;
    }
    let maintained_relogin = account.maintains_tokens && account.health_kind == "needs_relogin";
    if account.user_state != "active" && !maintained_relogin {
        return None;
    }
    let kind = match (
        account.health_kind,
        account.health_reason,
        account.health_scope,
    ) {
        ("needs_relogin", _, _) => Kind::Relogin,
        ("cooling", Some("link_stale"), _) if account.cooling_current => Kind::LinkStale,
        ("cooling", Some("quota"), _) if account.cooling_current => Kind::Limit,
        ("cooling", Some("store_error"), Some("account")) if account.cooling_current => {
            Kind::StoreError
        }
        ("cooling", Some("permission"), Some("account")) if account.cooling_current => {
            Kind::OrganisationDenied
        }
        ("cooling", Some("payment"), Some("account")) if account.cooling_current => Kind::Billing,
        _ => return None,
    };
    Some(Signal {
        subject: Subject::Account {
            provider: account.provider,
            id: account.id,
        },
        kind,
    })
}

/// Core-derived provider pool status, not a count guessed from display rows.
pub struct Pool<'a> {
    pub provider: &'a str,
    pub active: u64,
    pub eligible: u64,
}

pub fn pool_signal<'a>(pool: &Pool<'a>) -> Option<Signal<'a>> {
    (pool.active > 0 && pool.eligible == 0 && !pool.provider.is_empty()).then_some(Signal {
        subject: Subject::Provider(pool.provider),
        kind: Kind::ProviderExhausted,
    })
}

/// Projection of SSE core_warning data, NOT the outer event envelope.
/// Current {account, code} warnings do not masquerade as burst evidence.
pub fn burst_signal(
    kind: Option<&str>,
    burst_new_sessions: Option<u64>,
) -> Option<Signal<'static>> {
    (kind == Some("burst") && burst_new_sessions.is_some_and(|n| n > 0)).then_some(Signal {
        subject: Subject::Core,
        kind: Kind::Burst,
    })
}

#[cfg(test)]
pub fn badge(core_ok: bool, existing_attention: bool, signals: &[Signal<'_>]) -> Badge {
    if !core_ok {
        Badge::Error
    } else if existing_attention
        || signals.iter().any(|s| {
            matches!(
                s.kind,
                Kind::Relogin
                    | Kind::LinkStale
                    | Kind::Billing
                    | Kind::StoreError
                    | Kind::OrganisationDenied
                    | Kind::ProviderExhausted
            )
        })
    {
        Badge::Attention
    } else {
        Badge::Normal
    }
}

/// Monotonic elapsed time supplied by worker (e.g. startup Instant::elapsed).
/// Parallel to actions::ToastDebouncer; integration should choose ONE owner.
#[derive(Default)]
pub struct Debouncer {
    entries: HashMap<(OwnedSubject, Kind), Duration>,
}

#[derive(PartialEq, Eq, Hash)]
enum OwnedSubject {
    Account(String, String),
    Provider(String),
    Core,
}

impl Debouncer {
    pub fn allow(&mut self, signal: Signal<'_>, now: Duration) -> bool {
        const WINDOW: Duration = Duration::from_secs(30 * 60);
        // Expired entries can go; a backwards tick fails closed, not spam-open.
        self.entries
            .retain(|_, at| now.checked_sub(*at).is_none_or(|age| age < WINDOW));
        let subject = match signal.subject {
            Subject::Account { provider, id } => OwnedSubject::Account(provider.into(), id.into()),
            Subject::Provider(provider) => OwnedSubject::Provider(provider.into()),
            Subject::Core => OwnedSubject::Core,
        };
        let key = (subject, signal.kind);
        if self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(key, now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account<'static> {
        Account {
            provider: "codex",
            id: "synthetic",
            user_state: "active",
            maintains_tokens: true,
            health_kind: "ready",
            health_reason: None,
            health_scope: Some("account"),
            cooling_current: true,
        }
    }

    #[test]
    fn actionable_health_cases_and_non_actionable_rate_or_model_denial() {
        let mut a = account();
        assert!(account_signal(&a).is_none());
        a.health_kind = "needs_relogin";
        assert_eq!(account_signal(&a).unwrap().kind, Kind::Relogin);
        a.health_kind = "cooling";
        for (reason, expected) in [
            ("link_stale", Kind::LinkStale),
            ("quota", Kind::Limit),
            ("permission", Kind::OrganisationDenied),
            ("payment", Kind::Billing),
            ("store_error", Kind::StoreError),
        ] {
            a.health_reason = Some(reason);
            assert_eq!(account_signal(&a).unwrap().kind, expected);
        }
        a.health_scope = Some("model:synthetic");
        assert!(account_signal(&a).is_none());
        a.health_reason = Some("rate");
        assert!(account_signal(&a).is_none());
        a.health_reason = Some("quota");
        a.cooling_current = false;
        assert!(account_signal(&a).is_none());
        a.health_kind = "needs_relogin";
        for state in ["paused", "disabled"] {
            a.user_state = state;
            assert_eq!(account_signal(&a).unwrap().kind, Kind::Relogin);
            a.maintains_tokens = false;
            assert!(account_signal(&a).is_none());
            a.maintains_tokens = true;
        }
        a.user_state = "unknown";
        assert!(account_signal(&a).is_none());
    }

    #[test]
    fn pools_are_scoped_and_empty_pools_are_not_exhaustion() {
        assert!(
            pool_signal(&Pool {
                provider: "codex",
                active: 0,
                eligible: 0
            })
            .is_none()
        );
        assert!(
            pool_signal(&Pool {
                provider: "codex",
                active: 2,
                eligible: 1
            })
            .is_none()
        );
        assert_eq!(
            pool_signal(&Pool {
                provider: "claude",
                active: 2,
                eligible: 0
            })
            .unwrap()
            .subject,
            Subject::Provider("claude")
        );
    }

    #[test]
    fn burst_needs_explicit_kind_and_positive_threshold_and_error_wins() {
        assert!(burst_signal(None, Some(30)).is_none());
        assert!(burst_signal(Some("burst"), None).is_none());
        assert!(burst_signal(Some("burst"), Some(0)).is_none());
        let burst = burst_signal(Some("burst"), Some(30)).unwrap();
        assert_eq!(badge(false, false, &[burst]), Badge::Error);
        assert_eq!(badge(true, false, &[burst]), Badge::Normal);
        assert_eq!(badge(true, false, &[]), Badge::Normal);
        assert_eq!(badge(true, true, &[]), Badge::Attention);
    }

    #[test]
    fn quota_money_burst_and_restart_do_not_make_attention_without_provider_failure() {
        for kind in [
            Kind::Limit,
            Kind::QuotaCredits,
            Kind::QuotaFailover,
            Kind::QuotaExhausted,
            Kind::Burst,
            Kind::CoreRestart,
        ] {
            assert_eq!(
                badge(
                    true,
                    false,
                    &[Signal {
                        subject: Subject::Core,
                        kind
                    }]
                ),
                Badge::Normal
            );
        }
        for kind in [
            Kind::Relogin,
            Kind::LinkStale,
            Kind::Billing,
            Kind::StoreError,
            Kind::OrganisationDenied,
            Kind::ProviderExhausted,
        ] {
            assert_eq!(
                badge(
                    true,
                    false,
                    &[Signal {
                        subject: Subject::Core,
                        kind
                    }]
                ),
                Badge::Attention
            );
        }
    }

    #[test]
    fn debounce_account_kind_provider_boundary_and_backwards_tick() {
        let mut d = Debouncer::default();
        let a = Signal {
            subject: Subject::Account {
                provider: "codex",
                id: "a",
            },
            kind: Kind::Relogin,
        };
        let tick = |s| Duration::from_secs(s);
        assert!(d.allow(a, tick(100)));
        assert!(!d.allow(a, tick(99)));
        assert!(!d.allow(a, tick(1899)));
        assert!(d.allow(
            Signal {
                kind: Kind::Limit,
                ..a
            },
            tick(100)
        ));
        assert!(d.allow(
            Signal {
                subject: Subject::Account {
                    provider: "codex",
                    id: "b"
                },
                ..a
            },
            tick(100)
        ));
        assert!(d.allow(
            Signal {
                subject: Subject::Account {
                    provider: "claude",
                    id: "a"
                },
                ..a
            },
            tick(100)
        ));
        assert!(d.allow(a, tick(1900)));
        assert!(!d.allow(a, tick(1900)));
    }
}

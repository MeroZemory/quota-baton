//! R4 dedicated-launcher admission. Does not change account health or placement.
use anyhow::{Result, bail};
use chrono::{DateTime, Local, Utc};
use qb_core::accounts::{Account, Health, HealthReason, Provider, UserState};

pub(super) fn check(
    accounts: &[Account],
    provider: Provider,
    dedicated: bool,
    now: DateTime<Utc>,
) -> Result<Option<String>> {
    let relevant: Vec<_> = accounts.iter().filter(|a| a.provider == provider).collect();
    if relevant.iter().any(|a| {
        !a.drain && !a.auth_probe_pending && a.auth_failures == 0 && a.eligible_at(None, now)
    }) {
        return Ok(None);
    }
    if dedicated {
        let reset = relevant
            .iter()
            .filter(|a| {
                a.user_state == UserState::Active
                    && !a.drain
                    && !a.auth_probe_pending
                    && a.auth_failures == 0
                    && a.payment_until.is_none_or(|until| until <= now)
            })
            .filter_map(|a| {
                let Health::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope,
                } = &a.health
                else {
                    return None;
                };
                if scope != "account" || *until <= now {
                    return None;
                }
                // A synthetic cooling deadline is not a known quota reset.
                a.quota_cooling
                    .as_ref()
                    .filter(|e| e.reset_at >= *until && e.reset_at > now)
                    .map(|e| e.reset_at)
            })
            .min();
        if let Some(reset) = reset {
            let provider = match provider {
                Provider::Codex => "Codex",
                Provider::Claude => "Claude",
            };
            let local = reset.with_timezone(&Local);
            let at = if reset.signed_duration_since(now).num_seconds() < 86400 {
                local.format("%H:%M").to_string()
            } else {
                local.format("%-m/%-d %H:%M").to_string()
            };
            let all_quota = relevant
                .iter()
                .filter(|a| a.user_state == UserState::Active && !a.drain)
                .all(|a| {
                    !a.auth_probe_pending
                        && a.auth_failures == 0
                        && a.payment_until.is_none_or(|until| until <= now)
                        && matches!(
                            a.health,
                            Health::Cooling {
                                reason: HealthReason::Quota,
                                ..
                            }
                        )
                });
            return Ok(Some(if all_quota {
                format!("지금은 모든 {provider} 계정이 한도에 걸려 있습니다 · {at}에 풀립니다")
            } else {
                format!("지금 처리 가능한 {provider} 계정이 없습니다 · 한도는 {at}에 풀립니다")
            }));
        }
    }
    bail!("각 프로바이더에 적용 가능한 활성 계정이 하나 이상 필요합니다.")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn account(provider: Provider, now: DateTime<Utc>) -> Account {
        serde_json::from_value(serde_json::json!({
            "schema":1,"id":"synthetic-r4-account","provider":provider,"label":"fixture","email":"fixture@example.test",
            "plan":null,"identity":if provider==Provider::Codex { serde_json::json!({"provider":"codex","account_id":"synthetic-workspace","email":"fixture@example.test","user_id":"synthetic-user"}) }
                else {serde_json::json!({"provider":"claude","email":"fixture@example.test","account_uuid":"synthetic-account","organization_uuid":"synthetic-org"})},
            "source":{"kind":"oauth"},"user_state":"active","health":{"kind":"ready"},"generation":1,
            "token_meta":{"expires_at":now+chrono::Duration::hours(4),"last_refresh_at":now,"refresh_uncertain":false}
        })).unwrap()
    }
    #[test]
    fn ready_accounts_keep_existing_admission_and_missing_accounts_refuse() {
        let now = Utc::now();
        for p in [Provider::Codex, Provider::Claude] {
            let a = account(p, now);
            for dedicated in [false, true] {
                assert!(
                    check(std::slice::from_ref(&a), p, dedicated, now)
                        .unwrap()
                        .is_none()
                );
                assert!(check(&[], p, dedicated, now).is_err());
            }
        }
    }
    #[test]
    fn only_dedicated_accepts_known_real_quota_reset_for_both_providers() {
        let now = Utc::now();
        for p in [Provider::Codex, Provider::Claude] {
            let mut a = account(p, now);
            let reset = now + chrono::Duration::hours(2);
            a.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: reset,
                scope: "account".into(),
            };
            a.quota_cooling = Some(qb_core::accounts::QuotaCooling {
                window: "5h".into(),
                reset_at: reset,
                started_at: now,
            });
            assert!(check(std::slice::from_ref(&a), p, false, now).is_err());
            let text = check(std::slice::from_ref(&a), p, true, now)
                .unwrap()
                .unwrap();
            assert!(text.contains(&reset.with_timezone(&Local).format("%H:%M").to_string()));
            assert!(text.contains("한도"));
            a.quota_cooling = None;
            assert!(check(&[a], p, true, now).is_err());
        }
    }
    #[test]
    fn paused_disabled_drained_relogin_and_rate_cooling_cannot_borrow_quota_exception() {
        let now = Utc::now();
        let p = Provider::Codex;
        let mut a = account(p, now);
        let reset = now + chrono::Duration::hours(2);
        a.quota_cooling = Some(qb_core::accounts::QuotaCooling {
            window: "5h".into(),
            reset_at: reset,
            started_at: now,
        });
        a.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: reset,
            scope: "account".into(),
        };
        for state in [UserState::Paused, UserState::Disabled] {
            let mut b = a.clone();
            b.user_state = state;
            assert!(check(&[b], p, true, now).is_err());
        }
        let mut b = a.clone();
        b.drain = true;
        assert!(check(&[b], p, true, now).is_err());
        let mut b = a.clone();
        b.health = Health::NeedsRelogin {
            reason: "synthetic".into(),
            since: now,
        };
        assert!(check(&[b], p, true, now).is_err());
        a.health = Health::Cooling {
            reason: HealthReason::Rate,
            until: reset,
            scope: "account".into(),
        };
        assert!(check(&[a], p, true, now).is_err());
    }
}

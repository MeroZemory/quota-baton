//! Account health transitions. Callers classify upstream signals before applying them.
use super::{Account, Health, HealthReason, Provider, QuotaCooling, UserState};
use chrono::{DateTime, Duration, Utc};

#[derive(Clone, Debug)]
pub enum HealthSignal {
    TerminalRefresh {
        reason: String,
    },
    Revoked {
        reason: String,
    },
    AuthAfterRefresh,
    StaleToken401,
    ReloginCompleted,
    StoreFailure,
    LinkStale {
        retry_at: DateTime<Utc>,
    },
    NewerLinkedToken,
    NoChange,
    Cooling {
        reason: HealthReason,
        until: DateTime<Utc>,
        scope: String,
    },
    Ready,
    UsageSnapshot {
        snapshot: serde_json::Value,
        model: Option<String>,
        projected_allowance: f64,
    },
}

impl Health {
    /// Expiry enables the next real request to probe the account. Auth backoff
    /// history remains on Account so a failed probe doubles the next delay.
    pub fn expire(&mut self, now: DateTime<Utc>) -> bool {
        if matches!(self, Self::Cooling { until, .. } if *until <= now) {
            *self = Self::Ready;
            true
        } else {
            false
        }
    }
}

/// Credential presence is checked by selection; drain gates new sessions only.
pub fn eligible(account: &Account, model: Option<&str>, tried: &[String]) -> bool {
    account.eligible(model) && !tried.iter().any(|id| id == &account.id)
}

impl Account {
    pub fn eligible(&self, model: Option<&str>) -> bool {
        self.eligible_at(model, Utc::now())
    }

    pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {
        self.user_state == UserState::Active
            && match &self.health {
                Health::Ready => true,
                Health::NeedsRelogin { .. } => false,
                Health::Cooling { until, scope, .. } => {
                    *until <= now
                        || scope
                            .strip_prefix("model:")
                            .is_some_and(|blocked| model.is_none_or(|m| blocked != m))
                }
            }
    }

    /// Expire before reading health for a transition, retaining auth probe history.
    pub(crate) fn expire_health(&mut self, now: DateTime<Utc>) -> bool {
        let expired_auth = matches!(self.health, Health::Cooling { reason: HealthReason::Auth, until, .. } if until <= now);
        if !self.health.expire(now) {
            return false;
        }
        self.auth_probe_pending |= expired_auth;
        self.quota_cooling = None;
        true
    }

    /// Returns whether health or persisted auth backoff changed. User state is
    /// independent and is never modified by an upstream signal.
    pub fn apply_health_signal(&mut self, signal: HealthSignal, now: DateTime<Utc>) -> bool {
        let before = (
            self.health.clone(),
            self.auth_failures,
            self.auth_probe_pending,
            self.quota_cooling.clone(),
        );
        self.expire_health(now);
        match signal {
            HealthSignal::TerminalRefresh { reason } | HealthSignal::Revoked { reason } => {
                self.health = Health::NeedsRelogin { reason, since: now };
                self.quota_cooling = None;
                self.auth_probe_pending = false;
            }
            HealthSignal::StaleToken401 | HealthSignal::NoChange => {}
            HealthSignal::ReloginCompleted => {
                self.health = Health::Ready;
                self.auth_failures = 0;
                self.auth_probe_pending = false;
                self.quota_cooling = None;
            }
            HealthSignal::NewerLinkedToken => {
                if matches!(
                    self.health,
                    Health::NeedsRelogin { .. }
                        | Health::Cooling {
                            reason: HealthReason::LinkStale,
                            ..
                        }
                ) {
                    self.health = Health::Ready;
                    self.auth_failures = 0;
                    self.auth_probe_pending = false;
                    self.quota_cooling = None;
                }
            }
            HealthSignal::AuthAfterRefresh => {
                if !matches!(self.health, Health::NeedsRelogin { .. }) {
                    let minutes = (15_u64 << self.auth_failures.min(4)).min(240);
                    self.auth_failures = self.auth_failures.saturating_add(1);
                    self.cool(
                        HealthReason::Auth,
                        now + Duration::minutes(minutes as i64),
                        "account".into(),
                    );
                }
            }
            HealthSignal::Cooling {
                reason,
                until,
                scope,
            } => {
                self.cool(reason, until, scope);
            }
            HealthSignal::Ready => {
                if !matches!(self.health, Health::NeedsRelogin { .. }) {
                    self.health = Health::Ready;
                    self.quota_cooling = None;
                }
            }
            HealthSignal::UsageSnapshot {
                snapshot,
                model,
                projected_allowance,
            } => {
                self.apply_snapshot(&snapshot, model.as_deref(), projected_allowance, now);
            }
            HealthSignal::StoreFailure => {
                self.cool(
                    HealthReason::StoreError,
                    now + Duration::minutes(5),
                    "account".into(),
                );
            }
            HealthSignal::LinkStale { retry_at } => {
                self.cool(HealthReason::LinkStale, retry_at, "account".into());
            }
        }
        before
            != (
                self.health.clone(),
                self.auth_failures,
                self.auth_probe_pending,
                self.quota_cooling.clone(),
            )
    }

    /// Duration and account scope merge independently; account scope never narrows.
    /// The record holds one model scope: a later deadline replaces another model's
    /// scope (C5 permits this); equal or shorter deadlines retain the old model.
    fn cool(&mut self, reason: HealthReason, until: DateTime<Utc>, scope: String) -> bool {
        if matches!(self.health, Health::NeedsRelogin { .. }) {
            return false;
        }
        let (reason, until, scope, incoming) = match &self.health {
            Health::Cooling {
                reason: old_reason,
                until: old_until,
                scope: old_scope,
            } => {
                let incoming = until > *old_until;
                (
                    if incoming { reason } else { *old_reason },
                    until.max(*old_until),
                    if old_scope == "account" || scope == "account" {
                        "account".into()
                    } else if incoming {
                        scope
                    } else {
                        old_scope.clone()
                    },
                    incoming,
                )
            }
            _ => (reason, until, scope, true),
        };
        if reason == HealthReason::Auth {
            self.auth_probe_pending = true;
        }
        let scope_changed =
            matches!(&self.health, Health::Cooling { scope: old, .. } if old != &scope);
        self.health = Health::Cooling {
            reason,
            until,
            scope,
        };
        let same_causing_reset = self
            .quota_cooling
            .as_ref()
            .is_some_and(|evidence| (until - evidence.reset_at).abs() <= Duration::seconds(300));
        if scope_changed || reason != HealthReason::Quota || (incoming && !same_causing_reset) {
            self.quota_cooling = None;
        }
        incoming
    }

    fn apply_snapshot(
        &mut self,
        snapshot: &serde_json::Value,
        model: Option<&str>,
        _allowance: f64,
        now: DateTime<Utc>,
    ) {
        use serde_json::Value;
        if !matches!(
            self.health,
            Health::Ready
                | Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
        ) {
            return;
        }
        let windows = snapshot["windows"].as_array().cloned().unwrap_or_default();
        let reset = |w: &Value| timestamp(&w["reset_at"]);
        let fresh: Vec<_> = windows
            .iter()
            .filter(|w| reset(w).is_none_or(|r| r > now))
            .collect();
        let subbucket = |w: &Value| w["name"].as_str().is_some_and(|n| n.contains('_'));
        let active = snapshot["signals"]["active_limit"]
            .as_str()
            .or(snapshot["signals"]["representative_claim"].as_str());
        let active_matches = |w: &Value| {
            active.is_some_and(|a| {
                w["name"].as_str().is_some_and(|n| {
                    n == a
                        || matches!(
                            (a, n),
                            ("primary", "5h")
                                | ("secondary", "7d")
                                | ("five_hour", "5h")
                                | ("seven_day", "7d")
                                | ("seven_day_opus", "7d_oi")
                        )
                })
            })
        };
        let credits = self.provider == Provider::Codex && !self.allow_credits;
        let global = snapshot["signals"]["limit_reached"] == true
            || snapshot["signals"]["unified_status"] == "rejected";
        let serving = credits && snapshot["signals"]["serving_from_credits"] == true;
        let qualifies = |w: &Value| {
            w["used_pct"]
                .as_f64()
                // C7 projections guide placement; only observed usage cools.
                .is_some_and(|p| p >= 100.0 || (credits && p >= 99.0))
                || w["status"] == "rejected"
                || ((global || serving) && active_matches(w))
        };
        let account_windows: Vec<_> = fresh.iter().copied().filter(|w| !subbucket(w)).collect();
        let triggering: Vec<_> = account_windows
            .iter()
            .copied()
            .filter(|w| qualifies(w))
            .collect();
        let sub_only = active.is_some_and(|a| a.contains('_') || a == "seven_day_opus")
            || (triggering.is_empty() && fresh.iter().any(|w| subbucket(w) && qualifies(w)));
        let account_hold = !triggering.is_empty()
            || ((global || serving)
                && !sub_only
                && (windows.is_empty() || !account_windows.is_empty()));
        let (scope, candidates) = if account_hold {
            (
                Some("account".to_owned()),
                if triggering.is_empty() {
                    account_windows.clone()
                } else {
                    triggering.clone()
                },
            )
        } else {
            (
                model.map(|m| format!("model:{m}")),
                fresh
                    .iter()
                    .copied()
                    .filter(|w| subbucket(w) && qualifies(w))
                    .collect::<Vec<_>>(),
            )
        };
        if let Some(scope) = scope
            && (account_hold || !candidates.is_empty())
        {
            let causing = if account_hold && triggering.is_empty() {
                candidates
                    .iter()
                    .filter_map(|w| reset(w).map(|r| (*w, r)))
                    .min_by_key(|(_, r)| *r)
            } else {
                candidates
                    .iter()
                    .filter_map(|w| reset(w).map(|r| (*w, r)))
                    .max_by_key(|(_, r)| *r)
            };
            let until = causing
                .map(|(_, r)| r)
                .or_else(|| account_windows.iter().filter_map(|w| reset(w)).min())
                .unwrap_or(now + Duration::minutes(5));
            if self.cool(HealthReason::Quota, until, scope) {
                self.quota_cooling = causing.and_then(|(w, r)| {
                    w["name"].as_str().map(|name| QuotaCooling {
                        window: name.into(),
                        reset_at: r,
                        started_at: now,
                    })
                });
            }
        } else if let Some(evidence) = &self.quota_cooling {
            let early = windows.iter().any(|w| {
                w["name"].as_str() == Some(evidence.window.as_str())
                    && timestamp(&w["observed_at"]).is_some_and(|t| t > evidence.started_at)
                    && w["used_pct"].as_f64().is_some_and(|p| p < 100.0)
                    && reset(w)
                        .is_some_and(|r| (r - evidence.reset_at).abs() > Duration::seconds(300))
            });
            if early {
                self.health = Health::Ready;
                self.quota_cooling = None;
            }
        }
    }
}

fn timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture() -> Account {
        let identity = super::super::Identity::Codex {
            account_id: "synthetic-hardening".into(),
            email: "hardening@example.invalid".into(),
        };
        Account {
            schema: 1,
            id: identity.id(),
            provider: Provider::Codex,
            label: "fixture".into(),
            email: "hardening@example.invalid".into(),
            plan: None,
            identity,
            source: super::super::Source::Oauth,
            user_state: UserState::Active,
            health: Health::Ready,
            auth_failures: 0,
            auth_probe_pending: false,
            quota_cooling: None,
            drain: false,
            allow_credits: false,
            generation: 0,
            token_meta: super::super::TokenMeta::default(),
        }
    }

    #[test]
    fn placement_projection_never_creates_credit_cooling() {
        let now = Utc::now();
        let mut account = fixture();
        let snapshot = serde_json::json!({"windows":[{
            "name":"5h", "used_pct":98.0, "reset_at":now + Duration::hours(1),
            "observed_at":now
        }]});
        account.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot,
                model: None,
                projected_allowance: 50.0,
            },
            now,
        );
        assert_eq!(account.health, Health::Ready);
        assert_eq!(account.quota_cooling, None);
    }

    #[test]
    fn r1_expired_account_cooling_does_not_widen_model_transition() {
        let now = Utc::now();
        for reason in [HealthReason::Rate, HealthReason::Auth, HealthReason::Quota] {
            let mut account = fixture();
            account.health = Health::Cooling {
                reason,
                until: now,
                scope: "account".into(),
            };
            account.quota_cooling = Some(QuotaCooling {
                window: "5h".into(),
                reset_at: now,
                started_at: now - Duration::hours(1),
            });
            account.auth_failures = 2;
            assert!(account.apply_health_signal(
                HealthSignal::Cooling {
                    reason: HealthReason::Permission,
                    until: now + Duration::hours(1),
                    scope: "model:X".into()
                },
                now
            ));
            assert!(matches!(account.health, Health::Cooling { scope, .. } if scope == "model:X"));
            assert_eq!(account.quota_cooling, None);
            assert_eq!(account.auth_failures, 2);
            assert_eq!(account.auth_probe_pending, reason == HealthReason::Auth);
        }
    }

    #[test]
    fn r1_expiry_applies_to_no_change_and_auth_transitions() {
        let now = Utc::now();
        let mut account = fixture();
        account.health = Health::Cooling {
            reason: HealthReason::Auth,
            until: now,
            scope: "account".into(),
        };
        account.auth_failures = 2;
        assert!(account.apply_health_signal(HealthSignal::NoChange, now));
        assert_eq!(account.health, Health::Ready);
        assert!(account.auth_probe_pending);
        account.apply_health_signal(HealthSignal::AuthAfterRefresh, now);
        assert_eq!(account.auth_failures, 3);
        assert_eq!(
            account.health,
            Health::Cooling {
                reason: HealthReason::Auth,
                until: now + Duration::minutes(60),
                scope: "account".into()
            }
        );
    }

    #[test]
    fn r2_single_model_scope_replacement_uses_later_deadline() {
        let now = Utc::now();
        let mut account = fixture();
        for (model, minutes) in [("X", 60), ("Y", 30), ("Y", 60), ("Y", 120)] {
            account.apply_health_signal(
                HealthSignal::Cooling {
                    reason: HealthReason::Permission,
                    until: now + Duration::minutes(minutes),
                    scope: format!("model:{model}"),
                },
                now,
            );
            let expected = if minutes == 120 { "Y" } else { "X" };
            assert!(!account.eligible_at(Some(expected), now));
            assert!(account.eligible_at(Some(if expected == "X" { "Y" } else { "X" }), now));
        }
        account.apply_health_signal(
            HealthSignal::Cooling {
                reason: HealthReason::Quota,
                until: now + Duration::minutes(10),
                scope: "account".into(),
            },
            now,
        );
        account.apply_health_signal(
            HealthSignal::Cooling {
                reason: HealthReason::Permission,
                until: now + Duration::minutes(180),
                scope: "model:Z".into(),
            },
            now,
        );
        assert!(!account.eligible_at(None, now));
        assert!(matches!(account.health, Health::Cooling { scope, .. } if scope == "account"));
    }
}

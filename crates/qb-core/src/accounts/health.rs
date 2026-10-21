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
            && self.payment_until.is_none_or(|until| until <= now)
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
        let before = (
            self.health.clone(),
            self.auth_probe_pending,
            self.quota_cooling.clone(),
            self.quota_credit_releasable,
            self.payment_until,
        );
        if let Health::Cooling {
            reason: HealthReason::Payment,
            until,
            ..
        } = self.health
        {
            self.payment_until = Some(self.payment_until.map_or(until, |old| old.max(until)));
        }
        if self.payment_until.is_some_and(|until| until <= now) {
            self.payment_until = None;
        }
        let expired_auth = matches!(self.health, Health::Cooling { reason: HealthReason::Auth, until, .. } if until <= now);
        if self.health.expire(now) {
            self.quota_credit_releasable = false;
            self.auth_probe_pending |= expired_auth;
            self.quota_cooling = None;
        }
        if self.health == Health::Ready
            && let Some(until) = self.payment_until
        {
            self.health = Health::Cooling {
                reason: HealthReason::Payment,
                until,
                scope: "account".into(),
            };
        }
        before
            != (
                self.health.clone(),
                self.auth_probe_pending,
                self.quota_cooling.clone(),
                self.quota_credit_releasable,
                self.payment_until,
            )
    }

    /// Returns whether health or persisted auth backoff changed. User state is
    /// independent and is never modified by an upstream signal.
    pub fn apply_health_signal(&mut self, signal: HealthSignal, now: DateTime<Utc>) -> bool {
        let before = (
            self.health.clone(),
            self.auth_failures,
            self.payment_failures,
            self.payment_until,
            self.auth_probe_pending,
            self.quota_cooling.clone(),
            self.quota_credit_releasable,
        );
        self.expire_health(now);
        match signal {
            HealthSignal::TerminalRefresh { reason } | HealthSignal::Revoked { reason } => {
                self.health = if matches!(
                    self.source,
                    super::Source::Cpa {
                        mode: super::ImportMode::Linked,
                        ..
                    }
                ) {
                    Health::Cooling {
                        reason: HealthReason::LinkStale,
                        until: now + Duration::minutes(5),
                        scope: "account".into(),
                    }
                } else {
                    Health::NeedsRelogin { reason, since: now }
                };
                self.quota_cooling = None;
                self.auth_probe_pending = false;
            }
            HealthSignal::StaleToken401 | HealthSignal::NoChange => {}
            HealthSignal::ReloginCompleted => {
                if !matches!(
                    self.health,
                    Health::Cooling {
                        reason: HealthReason::Payment,
                        ..
                    }
                ) {
                    self.health = Health::Ready;
                    self.quota_cooling = None;
                }
                self.auth_failures = 0;
                self.auth_probe_pending = false;
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
                let until = if reason == HealthReason::Payment {
                    let hours = (6_i64 << self.payment_failures.min(2)).min(24);
                    self.payment_failures = self.payment_failures.saturating_add(1);
                    let until = now + Duration::hours(hours);
                    self.payment_until =
                        Some(self.payment_until.map_or(until, |old| old.max(until)));
                    until
                } else {
                    until
                };
                let quota_rejection = reason == HealthReason::Quota;
                self.cool(reason, until, scope);
                if quota_rejection {
                    self.quota_credit_releasable = false;
                }
            }
            HealthSignal::Ready => {
                if !matches!(
                    self.health,
                    Health::NeedsRelogin { .. }
                        | Health::Cooling {
                            reason: HealthReason::Payment,
                            ..
                        }
                ) {
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
                if matches!(
                    self.source,
                    super::Source::Cpa {
                        mode: super::ImportMode::Linked,
                        ..
                    }
                ) && matches!(self.health, Health::NeedsRelogin { .. })
                {
                    self.health = Health::Ready;
                    self.auth_probe_pending = false;
                }
                self.cool(HealthReason::LinkStale, retry_at, "account".into());
            }
        }
        if !matches!(
            self.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ) {
            self.quota_credit_releasable = false;
        }
        self.expire_health(now);
        before
            != (
                self.health.clone(),
                self.auth_failures,
                self.payment_failures,
                self.payment_until,
                self.auth_probe_pending,
                self.quota_cooling.clone(),
                self.quota_credit_releasable,
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
        let valid: Vec<_> = windows
            .iter()
            .filter(|w| {
                self.provider != Provider::Codex
                    || w["name"].as_str().is_some_and(|name| {
                        !matches!(name, "primary" | "secondary")
                            && crate::dashboard::history::WindowName::try_from(
                                name.split('_').next().unwrap_or(name).to_owned(),
                            )
                            .is_ok()
                    })
            })
            .collect();
        let fresh: Vec<_> = valid
            .iter()
            .copied()
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
        let credit_capable = crate::credits::entitled(snapshot);
        let paid_signal = snapshot["signals"]["serving_from_credits"] == true
            || snapshot["signals"]["active_limit"] == "credits";
        let credits = self.provider == Provider::Codex
            && !self.allow_credits
            && (credit_capable || paid_signal);
        let paid_allowed = self.provider == Provider::Codex && self.allow_credits && credit_capable;
        let hard_hold = matches!(
            self.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ) && !self.quota_credit_releasable;
        // `limit_reached` reports the subscription windows. With permitted credits
        // available upstream keeps serving from credits, so only an explicit
        // rejection (or a request-level usage_limit_reached, handled in relay)
        // makes the hold global (D19 correction, 2026-10-21 outage).
        let rejected = snapshot["signals"]["unified_status"] == "rejected";
        let global = (snapshot["signals"]["limit_reached"] == true && !paid_allowed) || rejected;
        let serving = credits && snapshot["signals"]["serving_from_credits"] == true;
        let qualifies = |w: &Value| {
            w["used_pct"]
                .as_f64()
                // C7 projections guide placement; only observed usage cools.
                .is_some_and(|p| {
                    ((!paid_allowed || subbucket(w)) && p >= 100.0)
                        || (credits && !subbucket(w) && p >= 99.0)
                })
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
                && (valid.is_empty() || !account_windows.is_empty()));
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
            let releasable = self.provider == Provider::Codex
                && account_hold
                && !hard_hold
                && !rejected
                && !fresh.iter().any(|w| w["status"] == "rejected");
            let incoming = self.cool(HealthReason::Quota, until, scope);
            self.quota_credit_releasable = releasable;
            if incoming {
                self.quota_cooling = causing.and_then(|(w, r)| {
                    w["name"].as_str().map(|name| QuotaCooling {
                        window: name.into(),
                        reset_at: r,
                        started_at: now,
                    })
                });
            }
        } else if self.quota_credit_releasable
            && matches!(&self.health, Health::Cooling { scope, .. } if scope == "account")
            && paid_allowed
            && !global
        {
            self.health = Health::Ready;
            self.quota_cooling = None;
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
            user_id: None,
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
            payment_failures: 0,
            payment_until: None,
            auth_probe_pending: false,
            quota_cooling: None,
            drain: false,
            allow_credits: false,
            quota_credit_releasable: false,
            generation: 0,
            token_meta: super::super::TokenMeta::default(),
        }
    }

    #[test]
    fn m51_linked_terminal_signals_cool_and_legacy_link_stale_recovers() {
        let now = Utc::now();
        for signal in [
            HealthSignal::Revoked {
                reason: "token_revoked".into(),
            },
            HealthSignal::TerminalRefresh {
                reason: "invalid_grant".into(),
            },
            HealthSignal::LinkStale {
                retry_at: now + Duration::minutes(5),
            },
        ] {
            let mut owned = fixture();
            owned.health = Health::NeedsRelogin {
                reason: "legacy".into(),
                since: now,
            };
            let mut linked = owned.clone();
            linked.source = super::super::Source::Cpa {
                mode: super::super::ImportMode::Linked,
                dir: std::path::PathBuf::from("synthetic-unused-cpa"),
                r#match: super::super::SourceMatch::from(&linked.identity),
            };
            owned.apply_health_signal(signal.clone(), now);
            assert!(matches!(owned.health, Health::NeedsRelogin { .. }));
            linked.apply_health_signal(signal, now);
            assert_eq!(
                linked.health,
                Health::Cooling {
                    reason: HealthReason::LinkStale,
                    until: now + Duration::minutes(5),
                    scope: "account".into(),
                }
            );
            linked.apply_health_signal(HealthSignal::NewerLinkedToken, now);
            assert_eq!(linked.health, Health::Ready);
        }
    }

    #[test]
    fn outage_2026_10_21_limit_reached_with_permitted_credits_stays_ready() {
        // Live: 7d at 100 %, limit_reached=true, has_credits with a positive balance and
        // allow_credits=true. Upstream serves from credits, so the poll must not cool.
        let now = Utc::now();
        let mut account = fixture();
        account.allow_credits = true;
        let snapshot = serde_json::json!({
            "windows":[{"name":"7d","used_pct":100.0,"reset_at":now + Duration::days(5),"observed_at":now}],
            "credits":{"has_credits":true,"unlimited":false,"balance":62488.66},
            "signals":{"limit_reached":true}
        });
        account.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot: snapshot.clone(),
                model: None,
                projected_allowance: 0.0,
            },
            now,
        );
        assert_eq!(account.health, Health::Ready);
        // Without permission the same snapshot still cools.
        let mut denied = fixture();
        denied.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot,
                model: None,
                projected_allowance: 0.0,
            },
            now,
        );
        assert!(matches!(
            denied.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
    }

    #[test]
    fn outage_phantom_codex_windows_never_cool() {
        let now = Utc::now();
        for name in ["0m", "primary", "secondary", "unknown"] {
            let mut account = fixture();
            account.apply_health_signal(HealthSignal::UsageSnapshot {
                snapshot: serde_json::json!({"windows":[{"name":name,"used_pct":100,"status":"rejected","reset_at":now+Duration::days(5)}]}),
                model:None, projected_allowance:0.0,
            }, now);
            assert_eq!(account.health, Health::Ready, "{name}");
        }
    }

    #[test]
    fn outage_phantom_windows_do_not_veto_account_level_signals() {
        let now = Utc::now();
        for signals in [
            serde_json::json!({"limit_reached":true}),
            serde_json::json!({"serving_from_credits":true}),
        ] {
            for name in ["0m", "primary", "secondary", "unknown"] {
                let mut account = fixture();
                account.apply_health_signal(HealthSignal::UsageSnapshot {
                    snapshot: serde_json::json!({
                        "windows":[{"name":name,"used_pct":100,"reset_at":now+Duration::days(5)}],
                        "signals":signals,"credits":{"has_credits":true,"balance":12.5}
                    }), model:None, projected_allowance:0.0,
                }, now);
                assert_eq!(
                    account.health,
                    Health::Cooling {
                        reason: HealthReason::Quota,
                        until: now + Duration::minutes(5),
                        scope: "account".into()
                    },
                    "{name}: {signals}"
                );
                assert!(account.quota_credit_releasable);
            }
        }
    }

    #[test]
    fn outage_poll_limit_is_releasable_but_real_rejection_is_not() {
        let now = Utc::now();
        for rejected in [false, true] {
            let mut account = fixture();
            let mut snapshot = serde_json::json!({
                "windows":[{"name":"7d","used_pct":100,"reset_at":now+Duration::days(5)}],
                "signals":{"limit_reached":true},"credits":{"has_credits":true,"balance":12.5}
            });
            if rejected {
                snapshot["signals"]["unified_status"] = "rejected".into();
            }
            account.apply_health_signal(
                HealthSignal::UsageSnapshot {
                    snapshot: snapshot.clone(),
                    model: None,
                    projected_allowance: 0.0,
                },
                now,
            );
            assert!(matches!(
                account.health,
                Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
            ));
            assert_eq!(account.quota_credit_releasable, !rejected);
            account.allow_credits = true;
            account.apply_health_signal(
                HealthSignal::UsageSnapshot {
                    snapshot,
                    model: None,
                    projected_allowance: 0.0,
                },
                now,
            );
            assert_eq!(account.health == Health::Ready, !rejected);
        }
    }

    #[test]
    fn outage_expired_real_window_is_not_a_phantom_fallback() {
        let now = Utc::now();
        let mut account = fixture();
        account.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot: serde_json::json!({
                    "windows":[{"name":"5h","used_pct":100,"reset_at":now-Duration::seconds(1)}],
                    "signals":{"limit_reached":true}
                }),
                model: None,
                projected_allowance: 0.0,
            },
            now,
        );
        assert_eq!(account.health, Health::Ready);
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
    #[test]
    fn c6_4_payment_backoff_persists_and_clears_only_after_deadline() {
        let mut account = fixture();
        let mut now = Utc::now();
        for (index, hours) in [6, 12, 24, 24].into_iter().enumerate() {
            account.apply_health_signal(
                HealthSignal::Cooling {
                    reason: HealthReason::Payment,
                    until: now + Duration::hours(6),
                    scope: "account".into(),
                },
                now,
            );
            let expected = Health::Cooling {
                reason: HealthReason::Payment,
                until: now + Duration::hours(hours),
                scope: "account".into(),
            };
            assert_eq!(account.health, expected);
            assert_eq!(account.payment_failures, index as u32 + 1);
            for signal in [
                HealthSignal::Ready,
                HealthSignal::ReloginCompleted,
                HealthSignal::UsageSnapshot {
                    snapshot: serde_json::json!({"windows":[{"name":"5h","used_pct":0}]}),
                    model: None,
                    projected_allowance: 0.0,
                },
            ] {
                account.apply_health_signal(signal, now);
                assert_eq!(account.health, expected);
            }
            account = serde_json::from_value(serde_json::to_value(&account).unwrap()).unwrap();
            assert_eq!(account.payment_failures, index as u32 + 1);
            now += Duration::hours(hours);
            account.apply_health_signal(HealthSignal::NoChange, now);
            assert_eq!(account.health, Health::Ready);
        }
        let mut legacy = serde_json::to_value(account).unwrap();
        legacy.as_object_mut().unwrap().remove("payment_failures");
        let legacy: Account = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.payment_failures, 0);
    }
    #[test]
    fn c6_4_payment_deadline_survives_other_health_and_relogin() {
        let now = Utc::now();
        let mut a = fixture();
        a.source = super::super::Source::Oauth;
        let payment = now + Duration::hours(6);
        a.apply_health_signal(
            HealthSignal::Cooling {
                reason: HealthReason::Payment,
                until: payment,
                scope: "account".into(),
            },
            now,
        );
        a.apply_health_signal(
            HealthSignal::Cooling {
                reason: HealthReason::Quota,
                until: now + Duration::hours(24),
                scope: "account".into(),
            },
            now,
        );
        assert!(matches!(
            a.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        assert_eq!(a.payment_until, Some(payment));
        a.apply_health_signal(HealthSignal::Ready, now);
        assert_eq!(
            a.health,
            Health::Cooling {
                reason: HealthReason::Payment,
                until: payment,
                scope: "account".into()
            }
        );
        a.apply_health_signal(
            HealthSignal::Revoked {
                reason: "synthetic-revocation".into(),
            },
            now,
        );
        assert!(matches!(a.health, Health::NeedsRelogin { .. }));
        a.apply_health_signal(HealthSignal::ReloginCompleted, now);
        assert_eq!(a.payment_until, Some(payment));
        assert!(!a.eligible_at(None, now));
        a.apply_health_signal(HealthSignal::NoChange, payment);
        assert_eq!(a.payment_until, None);
        assert!(a.eligible_at(None, payment));
    }
}

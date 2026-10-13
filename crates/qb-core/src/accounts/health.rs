//! Account health transitions. Callers classify upstream signals before applying them.
use super::{Account, Health, HealthReason, Provider, UserState};
use chrono::{DateTime, Duration, Utc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthScope {
    Account,
    Model(String),
}

impl HealthScope {
    fn persisted(self) -> String {
        match self {
            Self::Account => "account".into(),
            Self::Model(model) => format!("model:{model}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UsageWindow {
    pub used_pct: f64,
    pub projected_pct: f64,
    pub reset_at: DateTime<Utc>,
    pub limit_reached: bool,
}

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
    Quota {
        reset_at: DateTime<Utc>,
        scope: HealthScope,
    },
    UsageSnapshot {
        windows: Vec<UsageWindow>,
        serving_credits: bool,
    },
    Rate {
        retry_after_secs: u64,
        scope: HealthScope,
    },
    Permission {
        model: Option<String>,
        account_level: bool,
    },
    ModelUnsupported {
        model: String,
    },
    Payment,
    StoreFailure,
    LinkStale {
        retry_at: DateTime<Utc>,
    },
    NewerLinkedToken,
    NoChange,
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
                        || model.is_some_and(|model| {
                            scope
                                .strip_prefix("model:")
                                .is_some_and(|blocked| blocked != model)
                        })
                }
            }
    }

    /// Returns whether health or persisted auth backoff changed. User state is
    /// independent and is never modified by an upstream signal.
    pub fn apply_health_signal(&mut self, signal: HealthSignal, now: DateTime<Utc>) -> bool {
        let before = (self.health.clone(), self.auth_failures);
        match signal {
            HealthSignal::TerminalRefresh { reason } | HealthSignal::Revoked { reason } => {
                self.health = Health::NeedsRelogin { reason, since: now };
            }
            HealthSignal::StaleToken401 | HealthSignal::NoChange => {}
            HealthSignal::ReloginCompleted => {
                self.health = Health::Ready;
                self.auth_failures = 0;
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
                }
            }
            HealthSignal::AuthAfterRefresh => {
                if !matches!(self.health, Health::NeedsRelogin { .. }) {
                    let minutes = (15_u64 << self.auth_failures.min(4)).min(240);
                    self.auth_failures = self.auth_failures.saturating_add(1);
                    self.cool(
                        HealthReason::Auth,
                        now + Duration::minutes(minutes as i64),
                        HealthScope::Account,
                    );
                }
            }
            HealthSignal::Quota { reset_at, scope } => {
                self.cool(HealthReason::Quota, reset_at, scope);
            }
            HealthSignal::UsageSnapshot {
                windows,
                serving_credits,
            } => {
                let credits_hold = self.provider == Provider::Codex && !self.allow_credits;
                let reset = windows
                    .iter()
                    .filter(|window| {
                        window.used_pct >= 100.0
                            || window.limit_reached
                            || (credits_hold && (serving_credits || window.projected_pct >= 99.0))
                    })
                    .map(|window| window.reset_at)
                    .max();
                if let Some(reset_at) = reset {
                    self.cool(HealthReason::Quota, reset_at, HealthScope::Account);
                } else if !windows.is_empty()
                    && windows
                        .iter()
                        .all(|window| window.used_pct < 100.0 && !window.limit_reached)
                    && let Health::Cooling {
                        reason: HealthReason::Quota,
                        until,
                        ..
                    } = &self.health
                    && windows.iter().all(|window| window.reset_at != *until)
                {
                    self.health = Health::Ready;
                }
            }
            HealthSignal::Rate {
                retry_after_secs,
                scope,
            } if retry_after_secs > 10 => {
                let seconds = retry_after_secs.min(i64::MAX as u64);
                let until = Duration::try_seconds(seconds as i64)
                    .and_then(|duration| now.checked_add_signed(duration))
                    .unwrap_or(DateTime::<Utc>::MAX_UTC);
                self.cool(HealthReason::Rate, until, scope);
            }
            HealthSignal::Rate { .. } => {}
            HealthSignal::Permission {
                model,
                account_level,
            } => {
                if account_level {
                    self.cool(
                        HealthReason::Permission,
                        now + Duration::hours(6),
                        HealthScope::Account,
                    );
                } else if let Some(model) = model {
                    self.cool(
                        HealthReason::Permission,
                        now + Duration::hours(1),
                        HealthScope::Model(model),
                    );
                }
            }
            HealthSignal::ModelUnsupported { model } => {
                self.cool(
                    HealthReason::Permission,
                    now + Duration::hours(6),
                    HealthScope::Model(model),
                );
            }
            HealthSignal::Payment => self.cool(
                HealthReason::Payment,
                now + Duration::hours(6),
                HealthScope::Account,
            ),
            HealthSignal::StoreFailure => self.cool(
                HealthReason::StoreError,
                now + Duration::minutes(5),
                HealthScope::Account,
            ),
            HealthSignal::LinkStale { retry_at } => {
                self.cool(HealthReason::LinkStale, retry_at, HealthScope::Account)
            }
        }
        before != (self.health.clone(), self.auth_failures)
    }

    fn cool(&mut self, reason: HealthReason, until: DateTime<Utc>, scope: HealthScope) {
        // Passive/error signals cannot resurrect a terminally invalid credential.
        if !matches!(self.health, Health::NeedsRelogin { .. }) {
            self.health = Health::Cooling {
                reason,
                until,
                scope: scope.persisted(),
            };
        }
    }
}

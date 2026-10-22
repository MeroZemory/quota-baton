//! Explicit metadata projections: never serialize the account or quota source object.
use super::{replay_recorder as tape, replay_runtime::Runtime};
use crate::accounts::{Account, Health, HealthReason, UserState};
use serde::Serialize;
#[derive(Clone, Copy, Debug, Serialize)]
pub enum HealthKind {
    Ready,
    Cooling,
    NeedsRelogin,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Reason {
    Quota,
    Rate,
    Auth,
    Permission,
    Payment,
    StoreError,
    LinkStale,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Scope {
    Account,
    Model(tape::Id),
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum State {
    Active,
    Paused,
    Disabled,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AccountFacts {
    pub account: tape::Id,
    pub provider: tape::Provider,
    pub order_rank: Option<u32>,
    pub generation: u64,
    pub state: State,
    pub health: HealthKind,
    pub reason: Option<Reason>,
    pub scope: Scope,
    pub health_until_utc_ns: Option<i64>,
    pub health_since_utc_ns: Option<i64>,
    pub auth_failures: u32,
    pub payment_failures: u32,
    pub payment_until_utc_ns: Option<i64>,
    pub auth_probe_pending: bool,
    pub cooling_window: Option<tape::Id>,
    pub cooling_reset_utc_ns: Option<i64>,
    pub cooling_started_utc_ns: Option<i64>,
    pub quota_credit_releasable: bool,
    pub drain: bool,
    pub allow_credits: bool,
    pub expires_utc_ns: Option<i64>,
    pub refresh_uncertain: bool,
    pub credential_available: Option<bool>,
}
pub fn account(
    runtime: &Runtime,
    a: &Account,
    rank: Option<u32>,
    credential_available: Option<bool>,
) -> Option<AccountFacts> {
    let (health, reason, scope, until, since) = match &a.health {
        Health::Ready => (HealthKind::Ready, None, Scope::Account, None, None),
        Health::NeedsRelogin { since, .. } => (
            HealthKind::NeedsRelogin,
            None,
            Scope::Account,
            None,
            since.timestamp_nanos_opt(),
        ),
        Health::Cooling {
            reason,
            until,
            scope,
        } => {
            let scope = if scope == "account" {
                Scope::Account
            } else if let Some(model) = scope.strip_prefix("model:") {
                runtime
                    .identity(tape::IdentityDomain::Model, model)
                    .map(Scope::Model)
                    .unwrap_or(Scope::Unknown)
            } else {
                Scope::Unknown
            };
            let reason = match reason {
                HealthReason::Quota => Reason::Quota,
                HealthReason::Rate => Reason::Rate,
                HealthReason::Auth => Reason::Auth,
                HealthReason::Permission => Reason::Permission,
                HealthReason::Payment => Reason::Payment,
                HealthReason::StoreError => Reason::StoreError,
                HealthReason::LinkStale => Reason::LinkStale,
            };
            (
                HealthKind::Cooling,
                Some(reason),
                scope,
                until.timestamp_nanos_opt(),
                None,
            )
        }
    };
    Some(AccountFacts {
        account: runtime.identity(tape::IdentityDomain::Account, &a.id)?,
        provider: match a.provider {
            crate::accounts::Provider::Codex => tape::Provider::Codex,
            crate::accounts::Provider::Claude => tape::Provider::Claude,
        },
        order_rank: rank,
        generation: a.generation,
        state: match a.user_state {
            UserState::Active => State::Active,
            UserState::Paused => State::Paused,
            UserState::Disabled => State::Disabled,
        },
        health,
        reason,
        scope,
        health_until_utc_ns: until,
        health_since_utc_ns: since,
        auth_failures: a.auth_failures,
        payment_failures: a.payment_failures,
        payment_until_utc_ns: a.payment_until.and_then(|v| v.timestamp_nanos_opt()),
        auth_probe_pending: a.auth_probe_pending,
        cooling_window: a
            .quota_cooling
            .as_ref()
            .and_then(|v| runtime.identity(tape::IdentityDomain::Window, &v.window)),
        cooling_reset_utc_ns: a
            .quota_cooling
            .as_ref()
            .and_then(|v| v.reset_at.timestamp_nanos_opt()),
        cooling_started_utc_ns: a
            .quota_cooling
            .as_ref()
            .and_then(|v| v.started_at.timestamp_nanos_opt()),
        quota_credit_releasable: a.quota_credit_releasable,
        drain: a.drain,
        allow_credits: a.allow_credits,
        expires_utc_ns: a
            .token_meta
            .expires_at
            .and_then(|v| v.timestamp_nanos_opt()),
        refresh_uncertain: a.token_meta.refresh_uncertain,
        credential_available,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Runtime, Account) {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r142-projection-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let dir = tempfile::tempdir_in(base).unwrap();
        let runtime = Runtime::start(&dir.path().join("capture"), 16).unwrap();
        let stored:Account=serde_json::from_value(serde_json::json!({
            "schema":1,"id":"private-id-canary","provider":"codex","label":"private-label-canary","email":"private-email-canary","plan":"private-plan-canary",
            "identity":{"provider":"codex","account_id":"private-workspace-canary","email":"private-email-canary","user_id":"private-user-canary"},
            "source":{"kind":"takeover","from":"C:/synthetic/private-source-canary","at":"2026-10-22T00:00:00Z"},"user_state":"paused","health":{"kind":"needs_relogin","reason":"private-relogin-canary","since":"2026-10-22T00:00:00Z"},
            "drain":true,"allow_credits":true,"generation":7,"token_meta":{"refresh_uncertain":true}
        })).unwrap();
        (dir, runtime, stored)
    }
    #[test]
    fn r142_account_projection_excludes_identity_source_labels_and_relogin_text() {
        let (_dir, runtime, stored) = fixture();
        let facts = account(&runtime, &stored, None, None).unwrap();
        let event = tape::Event::AccountState { facts };
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("private-"));
        let value = serde_json::to_value(event).unwrap();
        let f = &value["facts"]["facts"];
        assert_eq!(f["health"], "NeedsRelogin");
        assert_eq!(f["state"], "Paused");
        assert_eq!(f["generation"], 7);
        assert!(f["order_rank"].is_null());
        assert!(f["credential_available"].is_null());
        assert_eq!(f["refresh_uncertain"], true);
        runtime.shutdown();
    }
    #[test]
    fn r142_account_projection_preserves_fixed_cooling_facts_and_knownness() {
        let (_dir, runtime, mut stored) = fixture();
        let at = chrono::DateTime::from_timestamp(1_792_627_200, 123_456_789).unwrap();
        stored.user_state = UserState::Active;
        stored.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: at + chrono::Duration::hours(1),
            scope: "model:private-model-canary".into(),
        };
        stored.auth_failures = 3;
        stored.payment_failures = 2;
        stored.payment_until = Some(at);
        stored.auth_probe_pending = true;
        stored.quota_cooling = Some(crate::accounts::QuotaCooling {
            window: "private-window-canary".into(),
            reset_at: at + chrono::Duration::hours(2),
            started_at: at - chrono::Duration::hours(1),
        });
        stored.quota_credit_releasable = true;
        stored.token_meta.expires_at = Some(at + chrono::Duration::hours(3));
        let facts = account(&runtime, &stored, Some(0), Some(false)).unwrap();
        assert_eq!(facts.order_rank, Some(0));
        assert_eq!(facts.credential_available, Some(false));
        assert!(matches!(facts.state, State::Active));
        assert!(matches!(facts.health, HealthKind::Cooling));
        assert!(matches!(facts.reason, Some(Reason::Quota)));
        let model = runtime
            .identity(tape::IdentityDomain::Model, "private-model-canary")
            .unwrap();
        assert!(matches!(facts.scope, Scope::Model(id) if id == model));
        assert_eq!(
            facts.cooling_window,
            runtime.identity(tape::IdentityDomain::Window, "private-window-canary")
        );
        assert_eq!(facts.health_until_utc_ns, Some(1_792_630_800_123_456_789));
        assert_eq!(facts.health_since_utc_ns, None);
        assert_eq!(facts.payment_until_utc_ns, Some(1_792_627_200_123_456_789));
        assert_eq!(facts.cooling_reset_utc_ns, Some(1_792_634_400_123_456_789));
        assert_eq!(
            facts.cooling_started_utc_ns,
            Some(1_792_623_600_123_456_789)
        );
        assert_eq!(facts.expires_utc_ns, Some(1_792_638_000_123_456_789));
        assert_eq!(facts.auth_failures, 3);
        assert_eq!(facts.payment_failures, 2);
        assert!(facts.auth_probe_pending && facts.quota_credit_releasable);
        assert!(facts.drain && facts.allow_credits && facts.refresh_uncertain);
        assert!(!serde_json::to_string(&facts).unwrap().contains("private-"));

        if let Health::Cooling { scope, .. } = &mut stored.health {
            *scope = "private-unknown-scope-canary".into();
        }
        let unknown = account(&runtime, &stored, None, None).unwrap();
        assert!(matches!(unknown.scope, Scope::Unknown));
        assert!(
            !serde_json::to_string(&unknown)
                .unwrap()
                .contains("private-")
        );
        runtime.shutdown();
    }
}

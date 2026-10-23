//! Canonical affinity storage. The account can be read, but never borrowed mutably.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Identity {
    pub(super) account: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Binding {
    pub(super) key: String,
    #[serde(flatten)]
    identity: Identity,
    pub(super) last_used: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) accepted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) first_seen_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) bound_since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) client_kind: Option<crate::dashboard::ClientKind>,
    #[serde(skip)]
    pub(super) observed_here: bool,
}

impl std::ops::Deref for Binding {
    type Target = Identity;
    fn deref(&self) -> &Identity {
        &self.identity
    }
}
impl Binding {
    /// Initial creation and snapshot fixtures. The static write gate separately
    /// reviews constructor and map-publication sites; existing entries use rebind.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        key: String,
        account: String,
        last_used: DateTime<Utc>,
        accepted: Option<bool>,
        first_seen_at: Option<DateTime<Utc>>,
        bound_since: Option<DateTime<Utc>>,
        client_kind: Option<crate::dashboard::ClientKind>,
        observed_here: bool,
    ) -> Self {
        Self {
            key,
            identity: Identity { account },
            last_used,
            accepted,
            first_seen_at,
            bound_since,
            client_kind,
            observed_here,
        }
    }
    pub(super) fn rebind(&mut self, account: &str, committed: &super::move_guard::CommittedMove) {
        assert!(
            committed.allows(&self.identity.account, account),
            "binding move requires its own committed budget"
        );
        self.identity.account = account.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::move_guard::{Breaker, Limits, Observations};

    fn binding(account: &str) -> Binding {
        Binding::new(
            "codex:sample-session".into(),
            account.into(),
            Utc::now(),
            Some(true),
            None,
            None,
            None,
            false,
        )
    }

    fn receipt(from: &str, to: &str) -> crate::bindings::move_guard::CommittedMove {
        let mut breaker = Breaker::default();
        let permit = breaker
            .prepare(
                &Limits::default(),
                "codex:sample-session",
                from,
                to,
                1,
                2,
                false,
                Observations::untracked(),
                Utc::now(),
            )
            .unwrap();
        breaker.commit(permit, to).unwrap()
    }

    #[test]
    fn legacy_snapshot_shape_and_metadata_survive_the_sealed_account() {
        let before = serde_json::json!({"key":"codex:sample-session", "account":"sample-a",
            "last_used":"2026-10-23T00:00:00Z", "accepted":true,
            "first_seen_at":"2026-10-22T00:00:00Z", "bound_since":"2026-10-23T00:00:00Z"});
        let mut stored: Binding = serde_json::from_value(before.clone()).unwrap();
        assert_eq!(serde_json::to_value(&stored).unwrap(), before);
        stored.rebind("sample-b", &receipt("sample-a", "sample-b"));
        assert_eq!(stored.account, "sample-b");
        let after = serde_json::to_value(&stored).unwrap();
        for field in [
            "key",
            "last_used",
            "accepted",
            "first_seen_at",
            "bound_since",
        ] {
            assert_eq!(after[field], before[field]);
        }
    }

    #[test]
    fn receipt_for_another_source_or_target_never_changes_the_account() {
        for authority in [receipt("other", "sample-b"), receipt("sample-a", "other")] {
            let mut binding = binding("sample-a");
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    binding.rebind("sample-b", &authority);
                }))
                .is_err()
            );
            assert_eq!(binding.account, "sample-a");
        }
    }
}

//! Current response aliases reuse core's validated leaves. Archive DTOs retain
//! their independent compatibility defaults in the parent module.
use super::{ActivityKind, AttentionCode, SettingAction, WindowName};
use crate::{
    accounts::Provider,
    dashboard::{AccountId, MoveReason, RequestOutcome, SessionId},
};
use chrono::{DateTime, Utc};
use qb_proto::history_response as wire;

pub type ActivityData = wire::ActivityData<
    DateTime<Utc>,
    SessionId,
    WindowName,
    MoveReason,
    AttentionCode,
    RequestOutcome,
    SettingAction,
>;
pub type ActivityEvent =
    wire::ActivityEvent<DateTime<Utc>, ActivityKind, Provider, AccountId, ActivityData>;
pub type ActivityPage = wire::ActivityPage<ActivityEvent>;

impl From<super::ActivityData> for ActivityData {
    fn from(data: super::ActivityData) -> Self {
        Self {
            session: data.session,
            sessions: data.sessions,
            window: data.window,
            reset_at: data.reset_at,
            expires_at: data.expires_at,
            reason: data.reason,
            attention: data.attention,
            outcome: data.outcome,
            action: data.action,
            credits_used: data.credits_used,
            count: data.count,
            max_wait_ms: data.max_wait_ms,
            relayed: data.relayed,
            moved_groups: data.moved_groups,
            sessions_truncated: data.sessions_truncated,
            recovered: data.recovered,
            active: data.active,
            threshold: data.threshold,
            groups_hit: data.groups_hit,
        }
    }
}
impl From<super::DiskActivityEvent> for ActivityEvent {
    fn from(event: super::DiskActivityEvent) -> Self {
        Self {
            seq: event.seq,
            ts: event.ts,
            kind: event.kind,
            provider: event.provider,
            account: event.account,
            account_to: event.account_to,
            notable: event.notable,
            data: event.data.into(),
        }
    }
}
impl From<super::ActivityPage> for ActivityPage {
    fn from(page: super::ActivityPage) -> Self {
        Self {
            events: page.events.into_iter().map(Into::into).collect(),
            next_seq: page.next_seq,
            truncated: page.truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_matches_current_archive_serialization_without_changing_restore_defaults() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let mut log = super::super::ActivityLog::default();
        log.push(
            super::super::ActivityInput {
                ts: now,
                kind: ActivityKind::Burst,
                provider: Provider::Codex,
                account: None,
                account_to: None,
                notable: true,
                data: super::super::ActivityData::default(),
            },
            now,
        )
        .unwrap();
        let page = log.query(None, 100, 1, now).unwrap();
        let expected = serde_json::to_vec(&page).unwrap();
        let response: ActivityPage = page.into();
        assert_eq!(serde_json::to_vec(&response).unwrap(), expected);
        let sparse = r#"{"session":null}"#;
        // Historical persistence accepts its established sparse/null spelling.
        assert!(serde_json::from_str::<super::super::ActivityData>(sparse).is_ok());
        assert!(serde_json::from_str::<ActivityData>(sparse).is_err());
    }
    #[test]
    fn response_aliases_reuse_core_identifier_enum_and_timestamp_validation() {
        let raw = r#"{"events":[{"seq":1,"ts":"2030-01-01T00:00:00Z","kind":"burst",
            "provider":"codex","account":null,"account_to":null,"notable":true,
            "data":{"count":0,"max_wait_ms":0,"relayed":0,"moved_groups":0,
            "sessions_truncated":false,"recovered":false}}],"next_seq":2,"truncated":false}"#;
        let original: serde_json::Value = serde_json::from_str(raw).unwrap();
        for (pointer, bad) in [
            ("/events/0/ts", "invalid"),
            ("/events/0/kind", "sample-acct-invalid"),
            ("/events/0/provider", "sample-acct-invalid"),
            ("/events/0/account", "sample-acct"),
            ("/events/0/account_to", "sample-acct"),
            ("/events/0/data/session", "sample-acct"),
            ("/events/0/data/window", "sample-acct"),
            ("/events/0/data/reason", "sample-acct"),
        ] {
            let mut invalid = original.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            invalid.pointer_mut(parent).unwrap()[key] = serde_json::json!(bad);
            assert!(
                serde_json::from_value::<ActivityPage>(invalid).is_err(),
                "{pointer}"
            );
        }
    }
}

//! Explicit exact-wire -> legacy display projection. Never serialize display as wire.
use super::*;
use qb_proto::admin_sessions as wire;

impl From<wire::SessionsView> for SessionsResponse {
    fn from(v: wire::SessionsView) -> Self {
        Self {
            move_cost: v.move_cost.map(Into::into),
            now: Some(v.now),
            active_window_secs: Some(v.active_window_secs),
            sessions: v.sessions.into_iter().map(Into::into).collect(),
            truncated: v.truncated,
        }
    }
}

impl From<wire::ClientView> for Client {
    fn from(v: wire::ClientView) -> Self {
        Self {
            kind: v.kind.as_str().into(),
            product: v.product,
            version: v.version,
        }
    }
}

impl From<wire::LabelView> for Label {
    fn from(v: wire::LabelView) -> Self {
        Self {
            text: v.text,
            source: v.source.as_str().into(),
            best_effort: v.best_effort,
        }
    }
}

impl From<wire::InflightView> for Inflight {
    fn from(v: wire::InflightView) -> Self {
        Self {
            requests: Some(v.requests as u64),
            oldest_started_at: v.oldest_started_at,
            ws_open: Some(v.ws_open as u64),
            ws_response_in_flight: v.ws_response_in_flight,
        }
    }
}

impl From<wire::PacingWait> for PacingWait {
    fn from(v: wire::PacingWait) -> Self {
        Self {
            waiting: Some(v.waiting as u64),
            since: Some(v.since),
        }
    }
}

impl From<wire::RateWaitToday> for RateWait {
    fn from(v: wire::RateWaitToday) -> Self {
        Self {
            count: Some(v.count),
            total_ms: Some(v.total_ms),
        }
    }
}

impl From<wire::SessionRouting> for SessionRouting {
    fn from(v: wire::SessionRouting) -> Self {
        Self {
            action_id: v.action_id.as_str().into(),
            on_5h_limit: v.on_5h_limit.as_str().into(),
            mode: v.mode.as_str().into(),
        }
    }
}

impl From<wire::QuotaWaitView> for QuotaWait {
    fn from(v: wire::QuotaWaitView) -> Self {
        Self {
            waiting: v.waiting,
            reset_at: v.reset_at,
        }
    }
}

impl From<wire::MoveView> for Move {
    fn from(v: wire::MoveView) -> Self {
        Self {
            at: Some(v.at),
            from: v.from.map(|id| id.as_str().into()),
            to: Some(v.to.as_str().into()),
            reason: v.reason.as_str().into(),
            next_turn_cache: v.next_turn_cache.map(|x| x.as_str().into()),
        }
    }
}

impl From<wire::SessionSummary> for Session {
    fn from(v: wire::SessionSummary) -> Self {
        Self {
            id: v.id.as_str().into(),
            provider: v.provider.as_str().into(),
            client: v.client.into(),
            label: v.label.map(Into::into),
            account: v.account.as_str().into(),
            first_seen_at: v.first_seen_at,
            bound_since: v.bound_since,
            last_used_at: Some(v.last_used_at),
            active: v.active,
            inflight: v.inflight.into(),
            inflight_known: v.inflight_known,
            pacing_wait: v.pacing_wait.map(Into::into),
            routing: v.routing.map(Into::into),
            quota_wait: v.quota_wait.map(Into::into),
            quota_wait_known: v.quota_wait_known,
            model: v.model,
            subagents_today: Some(v.subagents_today as u64),
            requests_today: Some(v.requests_today),
            failures_today: Some(v.failures_today),
            rate_wait_today: v.rate_wait_today.into(),
            moves_today: v.moves_today.into_iter().map(Into::into).collect(),
            first_account_today: v.first_account_today.map(|id| id.as_str().into()),
            marked_for_move: v.marked_for_move,
            counters_complete: v.counters_complete,
            truncated: v.truncated,
            subagents_known: v.subagents_known,
            moves_known: v.moves_known,
            metadata_truncated: v.metadata_truncated,
            outside_git: None,
        }
    }
}

impl From<wire::Aggregate> for MoveCostAggregate {
    fn from(v: wire::Aggregate) -> Self {
        Self {
            tokens: v.tokens,
            known_count: v.known_count,
            unknown_count: v.unknown_count,
            observed_since: Some(v.observed_since),
            recording_complete: v.recording_complete,
            tokens_saturated: v.tokens_saturated,
        }
    }
}

impl From<wire::Snapshot> for MoveCost {
    fn from(v: wire::Snapshot) -> Self {
        Self {
            hour: v.hour.into(),
            rolling24h: v.rolling24h.into(),
            clock_adjusted: v.clock_adjusted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sessions_display_projection_retains_every_wire_evidence_field() {
        let wire_value: serde_json::Value = serde_json::from_str(r#"{"move_cost":{"hour":{"tokens":90,"known_count":1,"unknown_count":1,"observed_since":"2026-10-24T00:00:00Z","recording_complete":false,"tokens_saturated":false},"rolling24h":{"tokens":90,"known_count":1,"unknown_count":1,"observed_since":"2026-10-24T00:00:00Z","recording_complete":false,"tokens_saturated":false},"clock_adjusted":true},"now":"2026-10-24T00:00:00Z","active_window_secs":60,"sessions":[{"id":"123456abcdef","provider":"codex","client":{"kind":"codex_cli","product":"synthetic","version":"1"},"label":{"text":"synthetic-folder","source":"claude_project","best_effort":true},"account":"cx-000001","first_seen_at":"2026-10-24T00:00:00Z","bound_since":"2026-10-24T00:00:00Z","last_used_at":"2026-10-24T00:00:00Z","active":true,"inflight":{"requests":2,"oldest_started_at":"2026-10-24T00:00:00Z","ws_open":1,"ws_response_in_flight":true},"inflight_known":false,"pacing_wait":{"waiting":2,"since":"2026-10-24T00:00:00Z"},"routing":{"action_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","on_5h_limit":"move","mode":"recover"},"quota_wait":{"waiting":1,"reset_at":"2026-10-24T00:00:00Z"},"quota_wait_known":true,"model":"synthetic","subagents_today":1,"requests_today":5,"failures_today":2,"rate_wait_today":{"count":1,"total_ms":9},"moves_today":[{"at":"2026-10-24T00:00:00Z","from":"cl-000002","to":"cx-000001","reason":"reset_hold_timeout","next_turn_cache":"miss"}],"first_account_today":"cl-000002","marked_for_move":true,"counters_complete":false,"truncated":true,"subagents_known":false,"moves_known":false,"metadata_truncated":true}],"truncated":true}"#).unwrap();
        let wire: wire::SessionsView = serde_json::from_value(wire_value.clone()).unwrap();
        let display: SessionsResponse = wire.into();
        let row = &display.sessions[0];
        assert!(row.routing.as_ref().unwrap().usable());
        assert!(row.routing.as_ref().unwrap().urgent());
        assert!(row.label.as_ref().unwrap().best_effort);
        assert!(!row.inflight_known && !row.counters_complete && !row.moves_known);
        let mut projected = serde_json::to_value(display).unwrap();
        // This presentation-only field has no server wire counterpart.
        projected["sessions"][0]
            .as_object_mut()
            .unwrap()
            .remove("outside_git");
        assert_eq!(projected, wire_value);
    }
}

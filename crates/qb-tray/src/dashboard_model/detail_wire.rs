//! Explicit exact detail wire -> existing display projection.
use super::*;
use qb_proto::admin_session_detail as wire;

impl From<wire::SessionDetail> for SessionDetail {
    fn from(v: wire::SessionDetail) -> Self {
        Self {
            session: v.session.into(),
            original_short_id: v.original_short_id,
            recent_requests: v.recent_requests.into_iter().map(Into::into).collect(),
            timeline: v.timeline.into_iter().map(Into::into).collect(),
            recent_requests_known: v.recent_requests_known,
            timeline_known: v.timeline_known,
        }
    }
}

impl From<wire::TimelineView> for Placement {
    fn from(v: wire::TimelineView) -> Self {
        Self {
            at: Some(v.at),
            kind: v.kind,
            account: v.account.as_str().into(),
            from: v.from.map(|id| id.as_str().into()),
            reason: Some(v.reason.as_str().into()),
            next_turn_cache: v.next_turn_cache.map(|x| x.as_str().into()),
        }
    }
}

impl From<wire::FailoverView> for Failover {
    fn from(v: wire::FailoverView) -> Self {
        Self {
            from: v.from.as_str().into(),
            to: v.to.as_str().into(),
        }
    }
}

impl From<wire::RecentRequest> for RequestSummary {
    fn from(v: wire::RecentRequest) -> Self {
        Self {
            ts: Some(v.ts),
            outcome: v.outcome.as_str().into(),
            status: Some(v.status),
            ttfb_ms: Some(v.ttfb_ms),
            dur_ms: Some(v.dur_ms),
            rate_wait_ms: Some(v.rate_wait_ms),
            rate_retries: Some(u64::from(v.rate_retries)),
            failover: v.failover.map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detail_display_projection_preserves_all_flat_and_nested_wire_evidence() {
        let value: serde_json::Value = serde_json::from_str(r#"{"id":"123456abcdef","provider":"codex","client":{"kind":"codex_cli","product":"synthetic","version":"1"},"label":{"text":"synthetic-folder","source":"claude_project","best_effort":true},"account":"cx-000001","first_seen_at":"2026-10-24T00:00:00Z","bound_since":"2026-10-24T00:00:00Z","last_used_at":"2026-10-24T00:00:00Z","active":true,"inflight":{"requests":2,"oldest_started_at":"2026-10-24T00:00:00Z","ws_open":1,"ws_response_in_flight":true},"inflight_known":false,"pacing_wait":{"waiting":2,"since":"2026-10-24T00:00:00Z"},"routing":{"action_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","on_5h_limit":"move","mode":"recover"},"quota_wait":{"waiting":1,"reset_at":"2026-10-24T00:00:00Z"},"quota_wait_known":true,"model":"synthetic","subagents_today":1,"requests_today":5,"failures_today":2,"rate_wait_today":{"count":1,"total_ms":9},"moves_today":[{"at":"2026-10-24T00:00:00Z","from":"cl-000002","to":"cx-000001","reason":"reset_hold_timeout","next_turn_cache":"miss"}],"first_account_today":"cl-000002","marked_for_move":true,"counters_complete":false,"truncated":true,"subagents_known":false,"moves_known":false,"metadata_truncated":true,"original_short_id":"12345678","recent_requests":[{"ts":"2026-10-24T00:00:00Z","outcome":"relayed_rate429","status":429,"ttfb_ms":3,"dur_ms":17,"rate_wait_ms":10,"rate_retries":4294967295,"failover":{"from":"cl-000002","to":"cx-000001"}},{"ts":"2026-10-24T00:00:00Z","outcome":"ok","status":200,"ttfb_ms":1,"dur_ms":4,"rate_wait_ms":0,"rate_retries":0,"failover":null}],"timeline":[{"at":"2026-10-24T00:00:00Z","kind":"placed","account":"cl-000002","from":null,"reason":"new","next_turn_cache":null},{"at":"2026-10-24T00:00:00Z","kind":"move","account":"cx-000001","from":"cl-000002","reason":"reset_hold_timeout","next_turn_cache":"hit"}],"recent_requests_known":false,"timeline_known":false}"#).unwrap();
        let decoded: wire::SessionDetail = serde_json::from_value(value.clone()).unwrap();
        let display: SessionDetail = decoded.into();
        assert_eq!(display.original_short_id.as_deref(), Some("12345678"));
        assert!(!display.recent_requests_known && !display.timeline_known);
        assert!(display.session.label.as_ref().unwrap().best_effort);
        assert_eq!(
            display.recent_requests[0].rate_retries,
            Some(u64::from(u32::MAX))
        );
        let mut projected = serde_json::to_value(display).unwrap();
        projected.as_object_mut().unwrap().remove("outside_git");
        assert_eq!(projected, value);
    }
}

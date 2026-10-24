//! Current live /state account shape; archived sparse presentation inputs stay separate.
//! Mirrors AccountRuntime::views + views_with_bindings; no reader defaults or repair.
use serde_json::{Value, json};

pub fn account(id: &str, provider: &str, usage_forecast: Value) -> Value {
    assert!(matches!(provider, "codex" | "claude"));
    json!({
        "outside_used":false,"pool_eligible":true,"takeover_pending":false,
        "quota_cooling":null,"quota_credit_releasable":false,"observed_at":null,
        "quota_notice":null,"credits_in_use":false,"usage_forecast":usage_forecast,
        "has_alternative":false,"id":id,"generation":1,"last_refresh_at":null,
        "provider":provider,"label":format!("Synthetic {id}"),
        "email":format!("{id}@example.invalid"),"plan":"plus","user_state":"active",
        "source":{"kind":"oauth"},"health":{"kind":"ready"},"probe_pending":false,
        "pacing":{"backoff_until":null,"step":0,"release_interval_ms":0,"waiting":0},
        "windows":[],"credits":null,"last_used_at":null,"bound_sessions":0,
        "token_expires_at":null,"drain":false,"allow_credits":false,
        "refresh_attention":false,"active_sessions":0,"spread_threshold":3
    })
}

// The support module is included by separate test binaries with different cases.
#[allow(dead_code)]
pub fn measuring_forecast() -> Value {
    json!({"as_of":"2026-10-23T14:05:00Z","observed_at":null,"windows":[],
        "earliest_window":null,"credit":null,"truncated":false})
}

use crate::{
    account_runtime::HealthSignal,
    accounts::{HealthReason, Provider},
};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Relay,
    Quota,
    Rate,
    Revoked,
    Auth,
    Permission,
    ModelPermission,
    Payment,
}
pub(super) struct Decision {
    pub kind: Kind,
    pub signal: Option<HealthSignal>,
    pub model: Option<String>,
    pub quota_reset: Option<DateTime<Utc>>,
}
impl Decision {
    // Header observations have already passed the account-aware C5 reducer.
    // In particular, a paid-credit permission can keep a 100% account ready.
    // Do not reinterpret that evidence using an account-free header heuristic.
    pub fn reconcile_quota(&mut self, observation: &crate::account_runtime::QuotaObservation) {
        if self.kind == Kind::Rate
            && let Some(until) = observation.quota_until
        {
            self.kind = Kind::Quota;
            self.signal = None; // C5 has durably applied the causal cooling already.
            self.quota_reset = Some(until);
        }
    }
    pub fn failover(&self) -> bool {
        !matches!(self.kind, Kind::Relay | Kind::Rate)
    }
    pub fn reason(&self) -> &'static str {
        match self.kind {
            Kind::Relay => "none",
            Kind::Quota => "quota",
            Kind::Rate => "rate",
            Kind::Revoked => "revoked",
            Kind::Auth => "auth",
            Kind::Permission => "permission",
            Kind::ModelPermission => "model_permission",
            Kind::Payment => "payment",
        }
    }
}

pub(super) fn classify(
    provider: Provider,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    model: Option<&str>,
) -> Decision {
    let model = model.filter(|m| crate::observability::safe_identifier(m).is_some());
    let decoded = super::hold::decoded_side(headers, body).unwrap_or_default();
    let event = error_value(&decoded);
    let root = event.get("response").unwrap_or(&event);
    let error = root.get("error").unwrap_or(root);
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // 메시지는 판정에만 사용하고 decision이나 원장으로 복사하지 않는다.
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| std::str::from_utf8(&decoded).unwrap_or_default());
    let effective = event
        .get("status")
        .and_then(Value::as_u64)
        .filter(|s| (100..=599).contains(s))
        .and_then(|s| StatusCode::from_u16(s as u16).ok())
        .unwrap_or(status);
    let has = |name: &str, expected: &str| headers.get(name).is_some_and(|v| v == expected);
    let now = Utc::now();
    let cooling = |reason, until, scope| {
        Some(HealthSignal::Cooling {
            reason,
            until,
            scope,
        })
    };
    let mut decision = Decision {
        kind: Kind::Relay,
        signal: None,
        quota_reset: None,
        model: model.map(str::to_owned),
    };
    if matches!(
        code,
        "invalid_encrypted_content" | "thinking_signature_invalid"
    ) || message.contains("Invalid `signature` in `thinking` block")
    {
        return decision;
    }
    if matches!(kind, "overloaded_error" | "server_is_overloaded")
        || matches!(code, "overloaded_error" | "server_is_overloaded")
    {
        return decision;
    }
    let claude_limit = ["5h", "7d", "7d_oi"].iter().any(|w| {
        has(
            &format!("anthropic-ratelimit-unified-{w}-status"),
            "rejected",
        )
    }) || has("anthropic-ratelimit-unified-status", "rejected");
    let error_event = status.is_success()
        && (event["type"] == "error"
            || event["type"] == "response.failed"
            || root.get("error").is_some());
    // A bounded handshake body can end before its closing braces. Preserve an
    // explicit Codex quota marker found in that prefix; treating it as a
    // generic rate limit would incorrectly retry a cached limit response.
    let truncated_codex_quota = provider == Provider::Codex
        && (decoded
            .windows(b"\"type\":\"usage_limit_reached\"".len())
            .any(|window| window == b"\"type\":\"usage_limit_reached\"")
            || decoded
                .windows(b"\"type\": \"usage_limit_reached\"".len())
                .any(|window| window == b"\"type\": \"usage_limit_reached\"")
            || decoded
                .windows(b"\"code\":\"usage_limit_reached\"".len())
                .any(|window| window == b"\"code\":\"usage_limit_reached\""));
    if kind == "usage_limit_reached"
        || code == "usage_limit_reached"
        || truncated_codex_quota
        || (has("x-codex-limit-reached", "true")
            && (effective == StatusCode::TOO_MANY_REQUESTS || error_event))
        || (effective == StatusCode::TOO_MANY_REQUESTS && claude_limit)
    {
        decision.kind = Kind::Quota;
        let mut scope = "account".to_owned();
        let mut resets = Vec::new();
        if provider == Provider::Claude {
            for w in ["5h", "7d", "7d_oi"] {
                if has(
                    &format!("anthropic-ratelimit-unified-{w}-status"),
                    "rejected",
                ) && let Some(t) =
                    header_timestamp(headers, &format!("anthropic-ratelimit-unified-{w}-reset"))
                {
                    resets.push(t);
                }
            }
            if !has("anthropic-ratelimit-unified-5h-status", "rejected")
                && !has("anthropic-ratelimit-unified-7d-status", "rejected")
                && has("anthropic-ratelimit-unified-7d_oi-status", "rejected")
                && let Some(model) = model
            {
                scope = format!("model:{model}");
            }
            if let Some(t) = header_timestamp(headers, "anthropic-ratelimit-unified-reset") {
                resets.push(t);
            }
        } else {
            if let Some(t) = error
                .get("resets_at")
                .and_then(Value::as_i64)
                .and_then(|n| DateTime::from_timestamp(n, 0))
            {
                resets.push(t);
            }
            if let Some(s) = error
                .get("resets_in_seconds")
                .and_then(Value::as_i64)
                .filter(|s| *s >= 0)
            {
                resets.push(now + Duration::seconds(s.min(31_536_000)));
            }
            // A body reset identifies the limit directly. Otherwise inspect only
            // exhausted/active windows, never an unrelated weekly reset.
            if resets.is_empty() {
                let mut known = Vec::new();
                let active = headers
                    .get("x-codex-active-limit")
                    .and_then(|v| v.to_str().ok());
                for slot in ["primary", "secondary"] {
                    let reset = header_timestamp(headers, &format!("x-codex-{slot}-reset-at"))
                        .or_else(|| {
                            header_seconds(headers, &format!("x-codex-{slot}-reset-after-seconds"))
                                .map(|s| now + Duration::seconds(s))
                        })
                        .filter(|reset| *reset > now);
                    if let Some(reset) = reset {
                        known.push(reset);
                        let used = headers
                            .get(format!("x-codex-{slot}-used-percent"))
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<f64>().ok());
                        let minutes = headers
                            .get(format!("x-codex-{slot}-window-minutes"))
                            .and_then(|v| v.to_str().ok());
                        let active_matches = active == Some(slot)
                            || (active == Some("5h") && minutes == Some("300"))
                            || (active == Some("7d") && minutes == Some("10080"));
                        if used.is_some_and(|n| n >= 100.0) || active_matches {
                            resets.push(reset);
                        }
                    }
                }
                if resets.is_empty() {
                    resets.extend(known.into_iter().min());
                }
            }
        }
        if provider == Provider::Claude
            && scope == "account"
            && !has("anthropic-ratelimit-unified-5h-status", "rejected")
            && !has("anthropic-ratelimit-unified-7d-status", "rejected")
            && has("anthropic-ratelimit-unified-7d_oi-status", "rejected")
            && model.is_none()
        {
            return decision;
        }
        decision.quota_reset = resets.iter().copied().filter(|reset| *reset > now).max();
        let until = resets
            .into_iter()
            .filter(|reset| *reset > now)
            .max()
            .unwrap_or(
                now + Duration::seconds(header_seconds(headers, "retry-after").unwrap_or(300)),
            );
        decision.signal = cooling(HealthReason::Quota, until, scope);
    } else if !matches!(
        code,
        "websocket_connection_limit_reached" | "previous_response_not_found"
    ) && (effective == StatusCode::TOO_MANY_REQUESTS
        || matches!(code, "rate_limit_exceeded" | "slow_down")
        || kind == "rate_limit_error")
    {
        decision.kind = Kind::Rate;
    } else if effective == StatusCode::UNAUTHORIZED
        || (effective.is_success() && kind == "authentication_error")
    {
        if code == "token_revoked"
            || message
                .to_ascii_lowercase()
                .contains("invalidated oauth token")
            || (kind == "authentication_error" && message.to_ascii_lowercase().contains("revoked"))
        {
            decision.kind = Kind::Revoked;
            decision.signal = Some(HealthSignal::Revoked("token_revoked".into()));
        } else {
            decision.kind = Kind::Auth;
        }
    } else if effective == StatusCode::FORBIDDEN
        || (effective.is_success() && kind == "permission_error")
    {
        decision.kind = Kind::Permission;
        if provider == Provider::Claude
            && message
                .to_ascii_lowercase()
                .contains("not allowed for this organization")
        {
            decision.signal = cooling(
                HealthReason::Permission,
                now + Duration::hours(6),
                "account".into(),
            );
        } else if let Some(m) = model {
            decision.signal = cooling(
                HealthReason::Permission,
                now + Duration::hours(1),
                format!("model:{m}"),
            );
        }
    } else if effective == StatusCode::BAD_REQUEST
        && provider == Provider::Codex
        && message.contains("not supported when using Codex with a ChatGPT account")
    {
        decision.kind = Kind::ModelPermission;
        decision.model = model.map(str::to_owned).or_else(|| {
            message
                .split(['\'', '"', '`'])
                .nth(1)
                .filter(|m| !m.is_empty() && m.len() <= 128)
                .and_then(crate::observability::safe_identifier)
        });
        if let Some(m) = &decision.model {
            decision.signal = cooling(
                HealthReason::Permission,
                now + Duration::hours(6),
                format!("model:{m}"),
            );
        }
    } else if effective == StatusCode::PAYMENT_REQUIRED
        || (provider == Provider::Claude
            && effective == StatusCode::BAD_REQUEST
            && kind == "billing_error")
    {
        decision.kind = Kind::Payment;
        decision.signal = cooling(
            HealthReason::Payment,
            now + Duration::hours(6),
            "account".into(),
        );
    }
    decision
}

pub(super) fn header_seconds(h: &HeaderMap, name: &str) -> Option<i64> {
    let value = h.get(name)?.to_str().ok()?;
    if let Ok(number) = value.parse::<f64>() {
        return number
            .is_finite()
            .then_some(number)
            .filter(|n| *n >= 0.0)
            .map(|n| n.ceil().min(31_536_000.0) as i64);
    }
    if name == "retry-after" {
        return DateTime::parse_from_rfc2822(value).ok().map(|date| {
            (date.with_timezone(&Utc) - Utc::now())
                .num_seconds()
                .clamp(0, 31_536_000)
        });
    }
    None
}

fn header_timestamp(h: &HeaderMap, name: &str) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(h.get(name)?.to_str().ok()?.parse().ok()?, 0)
}

pub(super) fn error_value(raw: &[u8]) -> Value {
    if let Ok(value) = serde_json::from_slice(raw) {
        return value;
    }
    // 완성된 SSE 오류 이벤트의 data 필드만 읽는다.
    let text = std::str::from_utf8(raw)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    for event in text.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if let Ok(value) = serde_json::from_str(&data) {
            return value;
        }
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c6_3_classification() {
        for code in ["rate_limit_exceeded", "slow_down"] {
            let body = serde_json::json!({"error":{"code":code}}).to_string();
            assert_eq!(
                classify(
                    Provider::Codex,
                    StatusCode::BAD_REQUEST,
                    &HeaderMap::new(),
                    body.as_bytes(),
                    None
                )
                .kind,
                Kind::Rate
            );
        }
        let claude = br#"{"error":{"type":"rate_limit_error"}}"#;
        assert_eq!(
            classify(
                Provider::Claude,
                StatusCode::TOO_MANY_REQUESTS,
                &HeaderMap::new(),
                claude,
                None
            )
            .kind,
            Kind::Rate
        );
        let mut quota_headers = HeaderMap::new();
        quota_headers.insert("x-codex-limit-reached", "true".parse().unwrap());
        let quota = br#"{"error":{"type":"rate_limit_error"}}"#;
        assert_eq!(
            classify(
                Provider::Codex,
                StatusCode::TOO_MANY_REQUESTS,
                &quota_headers,
                quota,
                None
            )
            .kind,
            Kind::Quota
        );
    }

    #[test]
    fn codex_usage_limit_is_not_rate_for_complete_64k_body() {
        let mut body =
            serde_json::json!({"error":{"type":"usage_limit_reached","padding":""}}).to_string();
        body = serde_json::json!({
            "error": {"type": "usage_limit_reached", "padding": "x".repeat(65536 - body.len())}
        })
        .to_string();
        assert_eq!(body.len(), 65536);
        let decision = classify(
            Provider::Codex,
            StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            body.as_bytes(),
            None,
        );
        assert_eq!(decision.kind, Kind::Quota);
    }
}

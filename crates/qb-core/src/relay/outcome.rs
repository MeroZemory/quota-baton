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
}
impl Decision {
    pub fn failover(&self) -> bool {
        self.kind != Kind::Relay
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
    if kind == "usage_limit_reached"
        || code == "usage_limit_reached"
        || has("x-codex-limit-reached", "true")
        || (effective == StatusCode::TOO_MANY_REQUESTS
            && (provider == Provider::Codex || claude_limit))
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
            for slot in ["primary", "secondary"] {
                if let Some(t) = header_timestamp(headers, &format!("x-codex-{slot}-reset-at")) {
                    resets.push(t);
                }
                if let Some(s) =
                    header_seconds(headers, &format!("x-codex-{slot}-reset-after-seconds"))
                {
                    resets.push(now + Duration::seconds(s));
                }
            }
        }
        let until = resets.into_iter().max().unwrap_or(
            now + Duration::seconds(header_seconds(headers, "retry-after").unwrap_or(300)),
        );
        decision.signal = cooling(HealthReason::Quota, until, scope);
    } else if effective == StatusCode::TOO_MANY_REQUESTS
        && header_seconds(headers, "retry-after").is_some_and(|s| s > 10)
    {
        decision.kind = Kind::Rate;
        decision.signal = cooling(
            HealthReason::Rate,
            now + Duration::seconds(header_seconds(headers, "retry-after").unwrap()),
            model
                .map(|m| format!("model:{m}"))
                .unwrap_or("account".into()),
        );
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
    } else if effective == StatusCode::PAYMENT_REQUIRED {
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
    h.get(name)?
        .to_str()
        .ok()?
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n.ceil().min(31_536_000.0) as i64)
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

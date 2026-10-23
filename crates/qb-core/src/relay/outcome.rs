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
    classified: Kind,
    classified_provider: Provider,
    classified_model: Option<String>,
}

/// A classified failed attempt, not an exclusion-list entry. The token value,
/// response body and error message never enter this evidence.
#[derive(Clone)]
pub(crate) struct RequestFailureEvidence {
    source: String,
    generation: u64,
    provider: Provider,
    key: Option<String>,
    model: Option<String>,
    deadline: std::time::Instant,
    lifetime: std::sync::Weak<std::sync::atomic::AtomicBool>,
}

/// One bounded handoff from a precommit failed WS turn to its next upgrade.
/// The map owns the lifetime; deleting/replacing the hint revokes its proof.
pub(super) struct ReconnectEvidence {
    failure: RequestFailureEvidence,
    lifetime: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ReconnectEvidence {
    pub(super) fn key(&self) -> &str {
        self.failure
            .key
            .as_deref()
            .expect("reconnect has a session")
    }
    pub(super) fn source(&self) -> &str {
        &self.failure.source
    }
    pub(super) fn model(&self) -> Option<&str> {
        self.failure.model.as_deref()
    }
    pub(super) fn active(&self, now: std::time::Instant) -> bool {
        now < self.failure.deadline && self.lifetime.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(super) fn permits_model(&self, model: Option<&str>) -> bool {
        // An unknown-model rejection fences this session/account, as before.
        // A known-model rejection only fences that model (or its bodyless
        // reconnect). This filter never broadens the proof's exact context.
        self.model().is_none() || model.is_none() || self.model() == model
    }
    pub(super) fn failure(&self) -> RequestFailureEvidence {
        self.failure.clone()
    }
}

impl Drop for ReconnectEvidence {
    fn drop(&mut self) {
        self.lifetime
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl RequestFailureEvidence {
    pub(crate) fn applies_to(
        &self,
        key: Option<&str>,
        source: &crate::accounts::Account,
        model: Option<&str>,
        now: std::time::Instant,
    ) -> bool {
        self.source == source.id
            && self.provider == source.provider
            && source.generation >= self.generation
            && self.key.as_deref() == key
            && self.model.as_deref() == model
            && now < self.deadline
            && self
                .lifetime
                .upgrade()
                .is_some_and(|alive| alive.load(std::sync::atomic::Ordering::Acquire))
    }
}

#[cfg(test)]
mod failure_evidence_tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration as StdDuration, Instant},
    };

    fn account() -> crate::accounts::Account {
        serde_json::from_value(serde_json::json!({
            "schema":1,"id":"sample-source","provider":"codex","label":"sample",
            "email":"sample@example.test","plan":null,
            "identity":{"provider":"codex","account_id":"sample-workspace","email":"sample@example.test"},
            "source":{"kind":"oauth"},"user_state":"active","health":{"kind":"ready"},
            "generation":7,"token_meta":{"expires_at":null,"last_refresh_at":null,"refresh_uncertain":false}
        })).unwrap()
    }

    fn rejected() -> Decision {
        classify(
            Provider::Codex,
            StatusCode::UNAUTHORIZED,
            &HeaderMap::new(),
            b"{}",
            Some("sample-model"),
        )
    }

    #[test]
    fn n6t_request_failure_is_scoped_to_source_session_model_generation_and_lifetime() {
        let source = account();
        let alive = Arc::new(AtomicBool::new(true));
        let now = Instant::now();
        let until = now + StdDuration::from_secs(10);
        let proof = rejected()
            .failed_attempt(
                &source,
                Some("codex:sample"),
                Some("sample-model"),
                until,
                &alive,
            )
            .unwrap();
        assert!(proof.applies_to(Some("codex:sample"), &source, Some("sample-model"), now));
        let mut changed = source.clone();
        changed.generation += 1;
        assert!(
            proof.applies_to(Some("codex:sample"), &changed, Some("sample-model"), now),
            "a concurrent refresh does not erase an actual failed attempt or grant another token retry"
        );
        changed.generation = 6;
        assert!(!proof.applies_to(Some("codex:sample"), &changed, Some("sample-model"), now));
        changed = source.clone();
        changed.id = "other-source".into();
        assert!(!proof.applies_to(Some("codex:sample"), &changed, Some("sample-model"), now));
        changed = source.clone();
        changed.provider = Provider::Claude;
        assert!(!proof.applies_to(Some("codex:sample"), &changed, Some("sample-model"), now));
        for (key, model) in [
            (Some("codex:other"), Some("sample-model")),
            (None, Some("sample-model")),
            (Some("codex:sample"), None),
            (Some("codex:sample"), Some("other-model")),
        ] {
            assert!(
                !proof.applies_to(key, &source, model, now),
                "foreign failure context cannot authorize a move"
            );
        }
        assert!(!proof.applies_to(Some("codex:sample"), &source, Some("sample-model"), until));
        alive.store(false, Ordering::Release);
        assert!(!proof.applies_to(Some("codex:sample"), &source, Some("sample-model"), now));
        alive.store(true, Ordering::Release);
        drop(alive);
        assert!(!proof.applies_to(Some("codex:sample"), &source, Some("sample-model"), now));

        let unknown = classify(
            Provider::Codex,
            StatusCode::FORBIDDEN,
            &HeaderMap::new(),
            br#"{"error":{"type":"permission_error"}}"#,
            None,
        )
        .reconnect_evidence(&source, Some("codex:sample"), None)
        .unwrap();
        assert!(
            unknown.permits_model(Some("other-model")),
            "unknown-model session exclusion must survive a later model"
        );
        assert!(
            !unknown
                .failure()
                .applies_to(Some("codex:sample"), &source, Some("other-model"), now),
            "candidate fencing must not widen failure authority"
        );
    }

    #[test]
    fn n6t_kind_label_cannot_promote_relay_rate_or_overload_into_failure_evidence() {
        let source = account();
        let alive = Arc::new(AtomicBool::new(true));
        for (status, body) in [
            (StatusCode::OK, b"{}".as_slice()),
            (StatusCode::TOO_MANY_REQUESTS, b"{}".as_slice()),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                br#"{"error":{"type":"overloaded_error"}}"#.as_slice(),
            ),
        ] {
            let mut decision = classify(Provider::Codex, status, &HeaderMap::new(), body, None);
            decision.kind = Kind::Auth;
            assert!(
                decision
                    .failed_attempt(
                        &source,
                        None,
                        None,
                        Instant::now() + StdDuration::from_secs(10),
                        &alive
                    )
                    .is_none(),
                "a mutable reason label is not a classified rejection"
            );
        }
    }

    #[test]
    fn n6t_failure_mint_uses_the_classified_provider_and_model_not_mutable_labels() {
        let source = account();
        let alive = Arc::new(AtomicBool::new(true));
        let until = Instant::now() + StdDuration::from_secs(10);
        let mut decision = rejected();
        decision.model = Some("other-model".into());
        assert!(
            decision
                .failed_attempt(&source, None, Some("other-model"), until, &alive)
                .is_none()
        );
        assert!(
            decision
                .failed_attempt(&source, None, Some("sample-model"), until, &alive)
                .is_some()
        );
        let mut wrong_provider = source.clone();
        wrong_provider.provider = Provider::Claude;
        assert!(
            decision
                .failed_attempt(&wrong_provider, None, Some("sample-model"), until, &alive)
                .is_none()
        );
    }

    #[test]
    fn n6t_reconnect_handoff_keeps_model_scope_and_dies_with_its_map_entry() {
        let source = account();
        let decision = rejected();
        assert!(
            decision
                .reconnect_evidence(&source, None, Some("sample-model"))
                .is_none()
        );
        let entry = decision
            .reconnect_evidence(&source, Some("codex:sample"), Some("sample-model"))
            .unwrap();
        assert_eq!(entry.key(), "codex:sample");
        assert_eq!(entry.source(), source.id);
        assert_eq!(entry.model(), Some("sample-model"));
        assert!(
            entry.permits_model(None),
            "a body-free upgrade restores the recorded model"
        );
        assert!(entry.permits_model(Some("sample-model")));
        assert!(!entry.permits_model(Some("other-model")));
        let proof = entry.failure();
        let now = Instant::now();
        assert!(entry.active(now));
        assert!(!entry.active(now + StdDuration::from_secs(121)));
        assert!(proof.applies_to(Some("codex:sample"), &source, Some("sample-model"), now));
        assert!(
            !proof.applies_to(Some("codex:sample"), &source, None, now),
            "restoring the model is required for dispatch"
        );
        // A retained allocation must not keep a removed/replaced map entry valid.
        let retained_allocation = entry.lifetime.clone();
        drop(entry);
        assert!(!retained_allocation.load(Ordering::Acquire));
        assert!(!proof.applies_to(Some("codex:sample"), &source, Some("sample-model"), now));
    }

    #[test]
    fn n6t_actual_failed_attempt_context_veto_preserves_binding_policy_and_budget() {
        use crate::bindings::{BindingTable, Candidate, DispatchAdmission, MoveObservations};
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/n6t-failure");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let table =
            BindingTable::new(root.path(), crate::config::RoutingConfig::default()).unwrap();
        let source = account();
        let mut target = source.clone();
        target.id = "sample-target".into();
        let mut next = source.clone();
        next.id = "sample-next".into();
        let candidates: Vec<_> = [source.clone(), target, next]
            .into_iter()
            .map(|account| Candidate {
                account,
                quota: None,
                inflight: 0,
            })
            .collect();
        let key = "codex:sample";
        match table.choose(
            Provider::Codex,
            Some(key),
            None,
            Some("sample-model"),
            &candidates[..1],
            None,
            42,
        ) {
            crate::bindings::Decision::Selected { lease, .. } => lease.dispatched(),
            other => panic!("fixture selection failed: {other:?}"),
        }
        let before = table.export().unwrap();
        let before_plan = table.routing_plan(Some(key));
        let until = Instant::now() + StdDuration::from_secs(30);
        let alive = Arc::new(AtomicBool::new(true));
        for wrong_key in [None, Some("codex:other")] {
            let proof = rejected()
                .failed_attempt(&source, wrong_key, Some("sample-model"), until, &alive)
                .unwrap();
            let result = table.choose_with_hold_at_admitted_proof(
                Provider::Codex,
                Some(key),
                None,
                Some("sample-model"),
                &candidates[1..2],
                &candidates,
                MoveObservations::untracked(),
                None,
                42,
                false,
                false,
                Utc::now(),
                None,
                |_| Some(DispatchAdmission::ordinary().with_failure(Some(proof))),
            );
            assert!(
                matches!(result, crate::bindings::Decision::None),
                "foreign attempt evidence must precede policy and binding publication"
            );
            assert_eq!(table.export().unwrap(), before);
            let after = table.routing_plan(Some(key));
            assert_eq!(after.revision, before_plan.revision);
            assert_eq!(after.anchor, before_plan.anchor);
        }
        let proof = rejected()
            .failed_attempt(&source, Some(key), Some("sample-model"), until, &alive)
            .unwrap();
        let result = table.choose_with_hold_at_admitted_proof(
            Provider::Codex,
            Some(key),
            None,
            Some("sample-model"),
            &candidates[1..2],
            &candidates,
            MoveObservations::untracked(),
            None,
            42,
            false,
            false,
            Utc::now(),
            None,
            |_| Some(DispatchAdmission::ordinary().with_failure(Some(proof))),
        );
        match result {
            crate::bindings::Decision::Selected { account, lease, .. } => {
                assert_eq!(
                    account, "sample-target",
                    "veto must not spend the ordinary move budget"
                );
                lease.dispatched();
            }
            other => panic!("valid failed attempt rejected: {other:?}"),
        }
        // A failed attempt is not fresh unusable health evidence. It never
        // grants a same-session budget exception for another healthy source.
        let proof = rejected()
            .failed_attempt(
                &candidates[1].account,
                Some(key),
                Some("sample-model"),
                until,
                &alive,
            )
            .unwrap();
        assert!(matches!(
            table.choose_with_hold_at_admitted_proof(
                Provider::Codex,
                Some(key),
                None,
                Some("sample-model"),
                &candidates[2..],
                &candidates,
                MoveObservations::untracked(),
                None,
                42,
                false,
                false,
                Utc::now(),
                None,
                |_| Some(DispatchAdmission::ordinary().with_failure(Some(proof))),
            ),
            crate::bindings::Decision::MoveHeld { .. }
        ));
    }
}

impl Decision {
    pub(super) fn reconnect_evidence(
        &self,
        account: &crate::accounts::Account,
        key: Option<&str>,
        model: Option<&str>,
    ) -> Option<ReconnectEvidence> {
        let key = key?;
        let lifetime = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let failure = self.failed_attempt(
            account,
            Some(key),
            model,
            std::time::Instant::now() + std::time::Duration::from_secs(120),
            &lifetime,
        )?;
        Some(ReconnectEvidence { failure, lifetime })
    }
    /// Prepared for this attempt; the caller publishes it to selection only
    /// after retry/health handling decided to try a successor. Mutable labels
    /// cannot turn an ordinary response or another model into this evidence.
    pub(super) fn failed_attempt(
        &self,
        account: &crate::accounts::Account,
        key: Option<&str>,
        model: Option<&str>,
        deadline: std::time::Instant,
        lifetime: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<RequestFailureEvidence> {
        if matches!(self.classified, Kind::Relay | Kind::Rate)
            || !self.failover()
            || self.classified_provider != account.provider
            || self.classified_model.as_deref() != model
        {
            return None;
        }
        Some(RequestFailureEvidence {
            source: account.id.clone(),
            generation: account.generation,
            provider: account.provider,
            key: key.map(str::to_owned),
            model: model.map(str::to_owned),
            deadline,
            lifetime: std::sync::Arc::downgrade(lifetime),
        })
    }
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
    let mut decision = classify_inner(provider, status, headers, body, model);
    decision.classified = decision.kind;
    decision.classified_model = decision.model.clone();
    decision
}

fn classify_inner(
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
        classified: Kind::Relay,
        classified_provider: provider,
        classified_model: None,
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
                now + Duration::seconds(
                    header_seconds_at(headers, "retry-after", now).unwrap_or(300),
                ),
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
    header_seconds_at(h, name, Utc::now())
}

fn header_seconds_at(h: &HeaderMap, name: &str, now: DateTime<Utc>) -> Option<i64> {
    let value = h.get(name)?.to_str().ok()?;
    if let Ok(number) = value.parse::<f64>() {
        return number
            .is_finite()
            .then_some(number)
            .filter(|n| *n >= 0.0)
            .map(|n| n.ceil().min(31_536_000.0) as i64);
    }
    if name == "retry-after" {
        return DateTime::parse_from_rfc2822(value)
            .ok()
            .map(|date| advice_seconds_until(date.with_timezone(&Utc), now));
    }
    None
}

fn advice_seconds_until(date: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    let remaining = date - now;
    let whole = remaining.num_seconds();
    let rounded = whole.saturating_add(i64::from(remaining > Duration::seconds(whole)));
    rounded.clamp(0, 31_536_000)
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
    #[test]
    fn quota_fallback_uses_one_wall_clock_sample_across_a_second_boundary() {
        let base = DateTime::parse_from_rfc3339("2026-10-20T00:00:00.900Z")
            .unwrap()
            .with_timezone(&Utc);
        let later = DateTime::parse_from_rfc3339("2026-10-20T00:00:01.050Z")
            .unwrap()
            .with_timezone(&Utc);
        let advice = DateTime::parse_from_rfc3339("2026-10-20T00:02:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            "Tue, 20 Oct 2026 00:02:00 GMT".parse().unwrap(),
        );
        // The former mixed-sample calculation ends100ms before server advice.
        assert!(
            base + Duration::seconds(header_seconds_at(&headers, "retry-after", later).unwrap())
                < advice
        );
        let until =
            base + Duration::seconds(header_seconds_at(&headers, "retry-after", base).unwrap());
        assert!(until >= advice);
        assert!(until - advice < Duration::seconds(1));
    }

    #[test]
    fn http_date_advice_rounds_up_fractional_seconds_and_keeps_the_existing_cap() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-20T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        for (millis, expected) in [
            (-1500, 0),
            (0, 0),
            (1, 1),
            (999, 1),
            (1000, 1),
            (1001, 2),
            (119999, 120),
            (120000, 120),
        ] {
            assert_eq!(
                super::advice_seconds_until(now + chrono::Duration::milliseconds(millis), now),
                expected
            );
        }
        assert_eq!(
            super::advice_seconds_until(now + chrono::Duration::days(366), now),
            31_536_000
        );
        assert_eq!(
            super::advice_seconds_until(
                now + chrono::Duration::seconds(1) + chrono::Duration::nanoseconds(1),
                now
            ),
            2
        );
    }

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

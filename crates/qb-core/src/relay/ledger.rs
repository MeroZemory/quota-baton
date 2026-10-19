use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use axum::http::{HeaderMap, Method};
use sha2::{Digest, Sha256};

use crate::{
    accounts::Provider,
    observability::{Observability, RequestRecord, ua_product},
    session::Session,
};

pub(in crate::relay) struct Completion {
    pub record: RequestRecord,
    pub dashboard_enabled: bool,
    pub dashboard_label: Option<crate::dashboard::labels::MemoryLabel>,
    dashboard_key: Option<String>,
    dashboard_client: crate::dashboard::ClientView,
    pub(super) dashboard_request: Option<crate::dashboard::runtime::RequestGuard>,
    pub burst_client: Option<crate::burst::Identity>,
    pub start: Instant,
    pub precommit_deadline: Option<Instant>,
    pub input_bytes: Arc<AtomicU64>,
    pub runtime: Option<Arc<crate::account_runtime::AccountRuntime>>,
    pub failed_access: Option<zeroize::Zeroizing<String>>,
    pub binding: Option<crate::bindings::BindingLease>,
    pub reset_hold_used: bool,
    pub policy_blocked: bool,
    pub usage_prepolled: bool,
    pub reset_hold_timed_out: bool,
    observability: Arc<Observability>,
    inflight: Option<Arc<AtomicU64>>,
    account_inflight: Option<(String, crate::account_runtime::InflightGuard)>,
}

impl Completion {
    pub fn new(
        observability: Arc<Observability>,
        method: &Method,
        path: &str,
        provider: Provider,
        headers: &HeaderMap,
    ) -> Self {
        Self {
            dashboard_enabled: true,
            dashboard_label: if provider == Provider::Codex {
                crate::dashboard::labels::codex_workspace(headers)
            } else {
                None
            },
            dashboard_key: None,
            dashboard_client: crate::dashboard::ClientView::from_headers(
                headers.get("originator").and_then(|v| v.to_str().ok()),
                headers.get("user-agent").and_then(|v| v.to_str().ok()),
                headers.get("x-app").and_then(|v| v.to_str().ok()),
            ),
            dashboard_request: None,
            start: Instant::now(),
            precommit_deadline: None,
            burst_client: crate::burst::client_identity(headers),
            reset_hold_used: false,
            policy_blocked: false,
            usage_prepolled: false,
            reset_hold_timed_out: false,
            input_bytes: Arc::new(AtomicU64::new(0)),
            runtime: None,
            failed_access: None,
            binding: None,
            observability,
            inflight: None,
            account_inflight: None,
            record: RequestRecord {
                ts: chrono::Utc::now().to_rfc3339(),
                req: format!("{:032x}", rand::random::<u128>()),
                family: provider.as_str().into(),
                route: format!("{method} {path}"),
                transport: "http".into(),
                account: None,
                session: None,
                model: None,
                originator: headers
                    .get("originator")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                ua: ua_product(headers),
                status: 0,
                upstream_status: None,
                ttfb_ms: 0,
                dur_ms: 0,
                bytes_in: 0,
                bytes_out: 0,
                attempts: 0,
                rate_wait_ms: 0,
                rate_retries: 0,
                relayed_rate_429s: 0,
                outcome: "client_drop".into(),
                retained_copy: false,
                upstream_error_code: None,
                attempt_details: Vec::new(),
                cache_usage: None,
                after_move: false,
            },
        }
    }

    pub fn session(&mut self, session: &Session) {
        self.dashboard_key = session.key.as_ref().filter(|key| key.len() <= 256).cloned();
        if let Some(key) = session.key.as_deref().filter(|key| key.len() > 256) {
            self.observability
                .dashboard()
                .incomplete(crate::dashboard::SessionId::from_key(key));
        }
        self.record.session = session.key.as_ref().map(|key| {
            let digest = Sha256::digest(key.as_bytes());
            digest[..6]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        });
        self.record.model = session.model.clone();
        self.dashboard_begin();
        if session
            .key
            .as_deref()
            .is_some_and(|key| key.starts_with("claude:"))
        {
            self.dashboard_model(session.model.as_deref());
        }
    }

    pub fn dashboard_begin(&mut self) {
        if !self.dashboard_enabled || self.dashboard_request.is_some() {
            return;
        }
        if let Some(key) = &self.dashboard_key {
            let at = self
                .record
                .ts
                .parse()
                .unwrap_or_else(|_| chrono::Utc::now());
            self.dashboard_request = self.observability.dashboard().request_observed(
                key,
                self.dashboard_client.clone(),
                self.dashboard_label.clone(),
                at,
                if self.record.transport == "ws" {
                    crate::dashboard::Transport::Websocket
                } else {
                    crate::dashboard::Transport::Http
                },
            );
            if self.dashboard_request.is_some() {
                self.dashboard_label = None;
            }
        }
    }
    pub fn dashboard_pacing(&self) -> Option<crate::dashboard::runtime::PacingGuard> {
        self.dashboard_request.as_ref().map(|guard| guard.pacing())
    }
    pub fn dashboard_model(&self, value: Option<&str>) {
        if let Some(key) = &self.dashboard_key {
            self.observability.dashboard().response_model(key, value);
        }
    }
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub fn track(&mut self, counter: Arc<AtomicU64>) {
        counter.fetch_add(1, Ordering::Relaxed);
        self.inflight = Some(counter);
    }

    pub fn attempt(&mut self, account: &str) {
        self.dashboard_begin();
        if self
            .account_inflight
            .as_ref()
            .is_none_or(|(id, _)| id != account)
        {
            self.account_inflight = None;
            self.account_inflight = self
                .runtime
                .as_ref()
                .map(|runtime| (account.to_owned(), runtime.track_inflight(account)));
        }
        self.record.attempts += 1;
        self.record
            .attempt_details
            .push(crate::observability::AttemptRecord {
                account: account.into(),
                failover_reason: None,
            });
    }

    pub fn end_attempt(&mut self) {
        self.account_inflight = None;
        self.binding = None;
    }
    pub fn failover(&mut self, reason: &str) {
        if let Some(attempt) = self.record.attempt_details.last_mut() {
            attempt.failover_reason = Some(reason.into());
        }
    }

    pub fn outcome(&mut self, status: u16, outcome: &str) {
        self.record.status = status;
        self.record.outcome = outcome.into();
    }

    pub fn rate_wait(&mut self, millis: u64) {
        self.record.rate_wait_ms = self.record.rate_wait_ms.saturating_add(millis);
    }

    pub fn rate_retry(&mut self) {
        self.record.rate_retries = self.record.rate_retries.saturating_add(1);
    }

    pub fn relayed_rate_429(&mut self) {
        self.record.relayed_rate_429s = self.record.relayed_rate_429s.saturating_add(1);
    }

    // 채널을 닫기 전에 원장을 확정하여 body EOF 뒤의 flush와 경합하지 않는다.
    pub fn finish(mut self, outcome: &str) {
        self.record.outcome = outcome.into();
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if self.record.status == 0
            && self
                .precommit_deadline
                .is_some_and(|at| Instant::now() >= at)
        {
            self.record.status = 504;
            self.record.outcome = "request_timeout".into();
        }
        if matches!(self.record.outcome.as_str(), "ok" | "rate_hold_ok")
            && ((200..300).contains(&self.record.status)
                || (self.record.transport == "ws" && self.record.status == 101))
            && let Some(binding) = &self.binding
        {
            binding.successful();
        }
        if let Some(counter) = self.inflight.take() {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        self.record.dur_ms = self.elapsed_ms();
        self.record.bytes_in = self.input_bytes.load(Ordering::Relaxed);
        if let Some(guard) = self.dashboard_request.take() {
            if self.record.after_move
                && let Some(cached) = self
                    .record
                    .cache_usage
                    .as_ref()
                    .and_then(|usage| usage.cached_tokens)
            {
                guard.cache_result(if cached > 0 {
                    crate::dashboard::CacheResult::Hit
                } else {
                    crate::dashboard::CacheResult::Miss
                });
            }
            guard.finish(crate::dashboard::RequestEnd {
                at: chrono::Utc::now(),
                outcome: crate::dashboard::runtime::request_outcome(&self.record.outcome),
                status: self.record.status,
                ttfb_ms: self.record.ttfb_ms,
                dur_ms: self.record.dur_ms,
                rate_wait_ms: self.record.rate_wait_ms,
                rate_retries: self.record.rate_retries,
                failover: self.record.attempt_details.first().and_then(|attempt| {
                    let to = self.record.account.as_ref()?;
                    if &attempt.account == to {
                        return None;
                    }
                    Some(crate::dashboard::FailoverView {
                        from: attempt.account.clone().try_into().ok()?,
                        to: to.clone().try_into().ok()?,
                    })
                }),
            });
        }
        if let Some(account) = self
            .record
            .account
            .as_ref()
            .and_then(|id| id.clone().try_into().ok())
        {
            self.observability.dashboard().rate_hold(
                crate::dashboard::history::RateHoldObservation {
                    ts: chrono::Utc::now(),
                    provider: if self.record.family == "claude" {
                        Provider::Claude
                    } else {
                        Provider::Codex
                    },
                    account,
                    wait_ms: self.record.rate_wait_ms,
                    retries: self.record.rate_retries,
                    relayed: self.record.relayed_rate_429s,
                },
            );
        }
        self.observability.record_request(self.record.clone());
    }
}

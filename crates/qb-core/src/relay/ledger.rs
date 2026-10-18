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
    pub burst_client: Option<crate::burst::Identity>,
    pub start: Instant,
    pub input_bytes: Arc<AtomicU64>,
    pub runtime: Option<Arc<crate::account_runtime::AccountRuntime>>,
    pub failed_access: Option<zeroize::Zeroizing<String>>,
    pub binding: Option<crate::bindings::BindingLease>,
    pub reset_hold_used: bool,
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
            start: Instant::now(),
            burst_client: crate::burst::client_identity(headers),
            reset_hold_used: false,
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
        self.record.session = session.key.as_ref().map(|key| {
            let digest = Sha256::digest(key.as_bytes());
            digest[..6]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        });
        self.record.model = session.model.clone();
    }

    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub fn track(&mut self, counter: Arc<AtomicU64>) {
        counter.fetch_add(1, Ordering::Relaxed);
        self.inflight = Some(counter);
    }

    pub fn attempt(&mut self, account: &str) {
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
        if let Some(counter) = self.inflight.take() {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        self.record.dur_ms = self.elapsed_ms();
        self.record.bytes_in = self.input_bytes.load(Ordering::Relaxed);
        self.observability.record_request(self.record.clone());
    }
}

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
    pub start: Instant,
    pub input_bytes: Arc<AtomicU64>,
    pub runtime: Option<Arc<crate::account_runtime::AccountRuntime>>,
    observability: Arc<Observability>,
    inflight: Option<Arc<AtomicU64>>,
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
            input_bytes: Arc::new(AtomicU64::new(0)),
            runtime: None,
            observability,
            inflight: None,
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
                outcome: "client_drop".into(),
                retained_copy: false,
                upstream_error_code: None,
                attempt_details: Vec::new(),
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
        self.record.attempts += 1;
        self.record
            .attempt_details
            .push(crate::observability::AttemptRecord {
                account: account.into(),
                failover_reason: None,
            });
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

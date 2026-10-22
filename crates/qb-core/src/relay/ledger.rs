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
    pub selection_cost: Option<u64>,
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
    accounting_ticket: Option<crate::observability::reread_accounting::RequestTicket>,
    accounting_regime: Option<(String, Option<String>)>,
    accounting_model_hint: Option<String>,
    // Retries within one logical cold turn must never become typical samples.
    accounting_cold_consumed: bool,
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
            selection_cost: None,
            runtime: None,
            failed_access: None,
            binding: None,
            observability,
            inflight: None,
            account_inflight: None,
            accounting_ticket: None,
            accounting_regime: None,
            accounting_model_hint: None,
            accounting_cold_consumed: false,
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
        // WS calls this for handshake/queued-turn/retry bookkeeping too. Its actual
        // turn writer must call accounting_attempt immediately before dispatch.
        if self.record.transport != "ws" {
            self.accounting_attempt(account);
        }
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
        self.accounting_finish(false);
        self.account_inflight = None;
        self.binding = None;
    }

    /// Bounded request metadata for accounting only: never changes routing/admission.
    pub fn accounting_http_model(&mut self, headers: &HeaderMap, prefix: &[u8]) {
        self.accounting_model_hint = (self.record.family == "codex"
            && self.record.transport == "http"
            && matches!(
                self.record.route.as_str(),
                "POST /v1/responses" | "POST /v1/responses/compact"
            ))
        .then(|| super::launch::request_model(headers, prefix).ok())
        .flatten()
        .filter(|model| model.len() <= 256);
    }

    fn accounting_model(&self) -> Option<&str> {
        self.record
            .model
            .as_deref()
            .or(self.accounting_model_hint.as_deref())
    }

    /// Actual provider turn dispatch only; never ingress, handshake or queued WS turn.
    pub fn accounting_attempt(&mut self, account: &str) {
        self.accounting_finish(false);
        self.record.cache_usage = None; // Never let a retry inherit another attempt's usage.
        let Some(binding) = self.binding.as_ref() else {
            return;
        };
        let provider = if self.record.family == "claude" {
            Provider::Claude
        } else {
            Provider::Codex
        };
        match binding.accounting_begin(
            provider,
            account,
            self.accounting_model(),
            chrono::Utc::now(),
        ) {
            Ok(Some(ticket)) => {
                self.accounting_ticket = Some(ticket);
                self.accounting_regime = Some((
                    account.to_owned(),
                    self.accounting_model().map(str::to_owned),
                ));
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(?error, "move cost accounting begin rejected"),
        }
    }

    /// Merge only observations belonging to this turn. WS observers may use this too.
    pub fn observe_usage(&mut self, event: &serde_json::Value) {
        if let Some(usage) = crate::observability::cache_usage(event) {
            crate::observability::merge_cache_usage(&mut self.record.cache_usage, usage);
        }
    }

    /// Explicit cancellation even if transport bookkeeping subsequently reports EOF/ok.
    pub fn accounting_cancel(&mut self) {
        self.accounting_finish(false);
    }

    fn accounting_finish(&mut self, success: bool) {
        let Some(ticket) = self.accounting_ticket.take() else {
            return;
        };
        let same_regime = self
            .accounting_regime
            .take()
            .is_some_and(|(account, model)| {
                self.record.account.as_deref() == Some(account.as_str())
                    && self.accounting_model() == model.as_deref()
            });
        match self.observability.accounting_end(
            ticket,
            success && same_regime && !self.accounting_cold_consumed,
            self.record.cache_usage.as_ref(),
            chrono::Utc::now(),
        ) {
            Ok(crate::observability::reread_accounting::Completion::Move { .. }) => {
                self.accounting_cold_consumed = true;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(?error, "move cost accounting completion rejected"),
        }
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
        self.accounting_finish(
            matches!(self.record.outcome.as_str(), "ok" | "rate_hold_ok")
                && ((200..300).contains(&self.record.status)
                    || (self.record.transport == "ws" && self.record.status == 101)),
        );
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

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use crate::{
        bindings::{BindingLease, BindingTable, Candidate, Decision},
        observability::CacheUsage,
    };

    const ROOT: &str = "claude:canonical";
    const CHILD: &str = "claude:canonical:agent:child";

    struct Fixture {
        table: Arc<BindingTable>,
        obs: Arc<Observability>,
        _home: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/r14-ledger-tests");
            std::fs::create_dir_all(&root).unwrap();
            let home = tempfile::tempdir_in(root).unwrap();
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            let table =
                BindingTable::new(home.path(), crate::config::RoutingConfig::default()).unwrap();
            table.attach_move_accounting(&obs);
            Self {
                table,
                obs,
                _home: home,
            }
        }
        fn choose(&self, key: &str, candidates: &[Candidate], expected: &str) -> BindingLease {
            let Decision::Selected { account, lease, .. } = self.table.choose(
                Provider::Claude,
                Some(key),
                (key == CHILD).then_some(ROOT),
                Some("m"),
                candidates,
                None,
                42,
            ) else {
                panic!("synthetic binding selection must succeed");
            };
            assert_eq!(account, expected);
            lease
        }
        fn request(&self, lease: BindingLease, account: &str, raw_key: &str) -> Completion {
            let mut c = Completion::new(
                self.obs.clone(),
                &Method::POST,
                "/v1/messages",
                Provider::Claude,
                &HeaderMap::new(),
            );
            c.dashboard_key = Some(raw_key.into());
            c.record.account = Some(account.into());
            c.record.model = Some("m".into());
            c.record.status = 200;
            c.binding = Some(lease);
            c
        }
        fn baseline(&self) {
            let lease = self.choose(ROOT, &[candidate("a")], "a");
            let mut c = self.request(lease, "a", ROOT);
            c.binding.as_ref().unwrap().dispatched();
            c.attempt("a");
            assert!(c.accounting_ticket.is_some());
            c.record.cache_usage = Some(usage(10));
            c.finish("ok");
        }
        fn move_to_b(&self, key: &str) -> BindingLease {
            let mut a = candidate("a");
            a.account.user_state = crate::accounts::UserState::Paused;
            self.choose(key, &[a, candidate("b")], "b")
        }
        fn finish_b(&self, lease: BindingLease, raw_key: &str) {
            let mut c = self.request(lease, "b", raw_key);
            c.attempt("b");
            assert!(c.accounting_ticket.is_some());
            c.record.cache_usage = Some(usage(100));
            c.finish("ok");
            let totals = self.obs.accounting_snapshot(chrono::Utc::now()).hour;
            assert_eq!(
                (totals.tokens, totals.known_count, totals.unknown_count),
                (90, 1, 0)
            );
        }
    }
    fn candidate(id: &str) -> Candidate {
        let mut account = crate::accounts::health::tests::fixture();
        account.id = id.into();
        account.provider = Provider::Claude;
        account.identity = crate::accounts::Identity::Claude {
            account_uuid: id.into(),
            organization_uuid: None,
            email: account.email.clone(),
        };
        let now = chrono::Utc::now();
        Candidate {
            account,
            quota: Some(serde_json::json!({"observed_at":now,"windows":[
                {"name":"5h","used_pct":10.0,"reset_at":now+chrono::Duration::hours(4),"observed_at":now},
                {"name":"7d","used_pct":10.0,"reset_at":now+chrono::Duration::days(4),"observed_at":now}
            ]})),
            inflight: 0,
        }
    }
    fn usage(value: u64) -> CacheUsage {
        CacheUsage {
            input_tokens: Some(value),
            cached_tokens: Some(500),
            cache_creation_input_tokens: Some(0),
        }
    }

    #[test]
    fn ingress_and_ws_handshake_never_claim_but_actual_turn_does() {
        let f = Fixture::new();
        f.baseline();
        let lease = f.move_to_b(ROOT);
        let mut c = f.request(lease, "b", ROOT);
        assert!(c.accounting_ticket.is_none());
        c.record.transport = "ws".into();
        c.record.status = 101;
        c.attempt("b");
        assert!(c.accounting_ticket.is_none());
        c.accounting_attempt("b");
        assert!(c.accounting_ticket.is_some());
        c.accounting_cancel();
        c.finish("ok");
        assert_eq!(
            f.obs
                .accounting_snapshot(chrono::Utc::now())
                .hour
                .unknown_count,
            1
        );
    }
    #[test]
    fn stale_inflight_completion_cannot_consume_next_move() {
        let f = Fixture::new();
        f.baseline();
        let mut old = f.request(f.table.begin_request(Some(ROOT), "a"), "a", ROOT);
        old.attempt("a");
        assert!(old.accounting_ticket.is_some());
        let lease = f.move_to_b(ROOT);
        old.record.cache_usage = Some(usage(999));
        old.finish("ok");
        assert_eq!(
            f.obs
                .accounting_snapshot(chrono::Utc::now())
                .hour
                .unknown_count,
            1
        );
        f.finish_b(lease, ROOT);
    }
    #[test]
    fn stale_selected_lease_start_after_move_cannot_reset_pending_destination() {
        let f = Fixture::new();
        f.baseline();
        let mut old = f.request(f.table.begin_request(Some(ROOT), "a"), "a", ROOT);
        let destination = f.move_to_b(ROOT);
        old.attempt("a"); // The race: A was selected, then B committed, then A started.
        assert!(old.accounting_ticket.is_none());
        old.record.cache_usage = Some(usage(999));
        old.finish("ok");
        assert_eq!(f.table.current(Some(ROOT)).as_deref(), Some("b"));
        f.finish_b(destination, ROOT);
    }
    #[test]
    fn raw_child_key_uses_lease_canonical_group_baseline_and_pending_move() {
        let f = Fixture::new();
        f.baseline();
        let lease = f.move_to_b(CHILD);
        assert_eq!(f.table.current(Some(CHILD)).as_deref(), Some("b"));
        f.finish_b(lease, CHILD);
    }
    #[test]
    fn stale_epoch_is_rejected_even_if_group_has_returned_to_same_account() {
        let f = Fixture::new();
        f.baseline();
        let mut old = f.request(f.table.begin_request(Some(ROOT), "a"), "a", ROOT);
        drop(f.move_to_b(ROOT));
        // Real semantic changes permit this fixture return: A recovered from
        // paused; B became paused after being ready at its placement.
        let mut b = candidate("b");
        b.account.user_state = crate::accounts::UserState::Paused;
        let returned = f.choose(ROOT, &[candidate("a"), b], "a");
        old.accounting_attempt("a");
        assert!(old.accounting_ticket.is_none());
        let mut current = f.request(returned, "a", ROOT);
        current.accounting_attempt("a");
        assert!(current.accounting_ticket.is_some());
        current.accounting_cancel();
        assert_eq!(
            f.obs
                .accounting_snapshot(chrono::Utc::now())
                .hour
                .unknown_count,
            2
        );
    }
    #[test]
    fn mismatched_attempt_account_cannot_claim_current_lease() {
        let f = Fixture::new();
        f.baseline();
        let lease = f.move_to_b(ROOT);
        let mut c = f.request(lease, "b", ROOT);
        c.accounting_attempt("a");
        assert!(c.accounting_ticket.is_none());
        c.accounting_attempt("b");
        assert!(c.accounting_ticket.is_some());
        c.record.cache_usage = Some(usage(100));
        c.finish("ok");
        assert_eq!(
            f.obs.accounting_snapshot(chrono::Utc::now()).hour.tokens,
            90
        );
    }
    #[test]
    fn cancellation_and_retry_do_not_reuse_usage() {
        let f = Fixture::new();
        f.baseline();
        let lease = f.move_to_b(ROOT);
        let mut c = f.request(lease, "b", ROOT);
        c.attempt("b");
        c.record.cache_usage = Some(usage(100));
        c.accounting_cancel();
        c.attempt("b");
        assert_eq!(c.record.cache_usage, None);
        c.finish("ok");
        let totals = f.obs.accounting_snapshot(chrono::Utc::now()).hour;
        assert_eq!((totals.known_count, totals.unknown_count), (0, 1));
    }
    #[test]
    fn changed_response_model_invalidates_measurement() {
        let f = Fixture::new();
        f.baseline();
        let lease = f.move_to_b(ROOT);
        let mut c = f.request(lease, "b", ROOT);
        c.attempt("b");
        c.record.model = Some("different-model".into());
        c.record.cache_usage = Some(usage(100));
        c.finish("ok");
        let totals = f.obs.accounting_snapshot(chrono::Utc::now()).hour;
        assert_eq!((totals.known_count, totals.unknown_count), (0, 1));
    }

    #[test]
    fn accounting_http_hint_is_bounded_separate_and_never_overrides_authority() {
        let f = Fixture::new();
        let mut c = Completion::new(
            f.obs.clone(),
            &Method::POST,
            "/v1/responses",
            Provider::Codex,
            &HeaderMap::new(),
        );
        c.accounting_http_model(
            &HeaderMap::new(),
            br#"{"model":"ordinary-model","input":[]}"#,
        );
        assert_eq!(c.accounting_model(), Some("ordinary-model"));
        assert_eq!(c.record.model, None);
        c.record.model = Some("authoritative-model".into());
        assert_eq!(c.accounting_model(), Some("authoritative-model"));
        c.record.model = None;
        for body in [
            br#"{"model":"a","model":"b"}"#.as_slice(),
            br#"{"model":0}"#,
            br#"{"model":"a"}garbage"#,
        ] {
            c.accounting_http_model(&HeaderMap::new(), body);
            assert_eq!(c.accounting_model(), None);
        }
        let mut headers = HeaderMap::new();
        headers.append("content-encoding", "identity".parse().unwrap());
        headers.append("content-encoding", "gzip".parse().unwrap());
        c.accounting_http_model(&headers, br#"{"model":"a"}"#);
        assert_eq!(c.accounting_model(), None);
        c.record.transport = "ws".into();
        c.accounting_http_model(&HeaderMap::new(), br#"{"model":"a"}"#);
        assert_eq!(c.accounting_model(), None);
    }

    fn cold_retry_baseline(independent_normal: bool) {
        let f = Fixture::new();
        f.baseline();
        let lease = f.move_to_b(ROOT);
        let mut cold = f.request(lease, "b", ROOT);
        cold.attempt("b");
        cold.record.cache_usage = Some(usage(1000));
        cold.accounting_cancel();
        assert!(cold.accounting_cold_consumed);
        cold.attempt("b");
        cold.record.cache_usage = Some(usage(10));
        cold.finish("ok");
        if independent_normal {
            let lease = f.choose(ROOT, &[candidate("b")], "b");
            let mut normal = f.request(lease, "b", ROOT);
            normal.attempt("b");
            normal.record.cache_usage = Some(usage(30));
            normal.finish("ok");
        }
        let mut b = candidate("b");
        b.account.user_state = crate::accounts::UserState::Paused;
        let lease = f.choose(ROOT, &[b, candidate("c")], "c");
        let mut next = f.request(lease, "c", ROOT);
        next.attempt("c");
        next.record.cache_usage = Some(usage(100));
        next.finish("ok");
        let totals = f.obs.accounting_snapshot(chrono::Utc::now()).hour;
        if independent_normal {
            assert_eq!(
                (totals.tokens, totals.known_count, totals.unknown_count),
                (70, 1, 1)
            );
        } else {
            assert_eq!(
                (totals.tokens, totals.known_count, totals.unknown_count),
                (0, 0, 2)
            );
        }
    }

    #[test]
    fn failed_cold_turn_successful_retry_cannot_train_next_move_baseline() {
        cold_retry_baseline(false);
    }

    #[test]
    fn independent_turn_after_cold_retry_can_train_next_move_baseline() {
        cold_retry_baseline(true);
    }
}
#[cfg(test)]
mod admission_tests {
    use super::*;
    #[test]
    fn attempt_bookkeeping_never_admits_a_binding_or_spends_a_dispatch_receipt() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/ledger-admission");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let table = crate::bindings::BindingTable::new(
            home.path(),
            crate::config::RoutingConfig::default(),
        )
        .unwrap();
        let account = crate::accounts::health::tests::fixture();
        let id = account.id.clone();
        let key = "codex:accounting";
        let candidate = crate::bindings::Candidate {
            account,
            quota: None,
            inflight: 0,
        };
        let crate::bindings::Decision::Selected { lease, .. } = table.choose(
            Provider::Codex,
            Some(key),
            None,
            None,
            &[candidate],
            None,
            42,
        ) else {
            panic!("fixture placement")
        };
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let mut completion = Completion::new(
            obs,
            &axum::http::Method::GET,
            "/v1/responses",
            Provider::Codex,
            &axum::http::HeaderMap::new(),
        );
        completion.binding = Some(lease);
        completion.attempt(&id);
        completion.attempt(&id);
        assert!(
            !table.existing_placement(Some(key), &id),
            "ledger accounting granted dispatch authority"
        );
        completion.binding.as_ref().unwrap().dispatched();
        assert!(table.existing_placement(Some(key), &id));
    }
}

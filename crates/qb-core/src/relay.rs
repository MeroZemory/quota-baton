pub(crate) mod credentials;
mod headers;
mod hold;
mod ledger;
mod outcome;
pub(crate) mod notices;
mod response;
mod upload;
mod websocket;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(all(feature = "phase0", debug_assertions))]
use axum::body::Body;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use tokio::sync::Notify;

use crate::{
    accounts::{Account, AccountStore, Health, HealthReason, Provider, UserState},
    config::Config,
    debug_pin::DebugPins,
    guard::{InflightBudget, MemoryGuard, mib_bytes},
    observability::Observability,
    session,
};
use credentials::TokenGates;
pub use credentials::{ManualRefreshError, ManualRefreshResult, ManualRefreshTerminal};
pub use hold::CommitDecision;
use ledger::Completion;

pub(crate) fn poll_revoked(provider: Provider, body: &[u8]) -> bool {
    outcome::classify(
        provider,
        StatusCode::UNAUTHORIZED,
        &HeaderMap::new(),
        body,
        None,
    )
    .kind
        == outcome::Kind::Revoked
}

pub struct RelayState {
    pub accounts: Arc<crate::account_runtime::AccountRuntime>,
    pub quota: Arc<crate::quota::UsagePoller>,
    pub bindings: Arc<crate::bindings::BindingTable>,
    burst: Mutex<crate::burst::BurstTracker>,
    reset_hold_secs: u64,
    rate_hold_secs: u64,
    rate_hold_max_retries: u32,
    ws_upgrade_hold_secs: u64,
    pub inflight_requests: Arc<AtomicU64>,
    store: Arc<AccountStore>,
    client: reqwest::Client,
    anthropic_base: String,
    codex_base: String,
    pub budget: Arc<InflightBudget>,
    retain_cap: usize,
    memory_guard: Arc<MemoryGuard>,
    observability: Arc<Observability>,
    pins: Arc<DebugPins>,
    tokens: TokenGates,
    websocket: crate::config::WebsocketConfig,
    reconnect_exclusions: Mutex<BTreeMap<(String, String), Instant>>,
    draining: AtomicBool,
    drain_started: AtomicBool,
    drain_ready: AtomicBool,
    lease_released: AtomicBool,
    drain_gate: tokio::sync::Mutex<()>,
    drain_timeout_secs: AtomicU64,
    drain_notify: Notify,
    drain_start_notify: Notify,
    lease_notify: Notify,
    handover_port: u16,
    #[cfg(all(feature = "phase0", debug_assertions))]
    close_after_response: bool,
    #[cfg(all(feature = "phase0", debug_assertions))]
    body_capture: Option<Arc<crate::phase0::BodyCapture>>,
}

impl RelayState {
    pub(crate) fn recent_warnings(&self) -> Vec<serde_json::Value> {
        self.observability.recent_warnings()
    }
    pub fn login_service(&self) -> crate::oauth::LoginService {
        crate::oauth::LoginService::new(
            self.store.clone(),
            self.client.clone(),
            self.observability.clone(),
        )
        .with_runtime(self.accounts.clone())
    }
    /// Manual refresh uses the same lease owner, gate and write-ahead as relay 401s.
    pub async fn refresh_account(
        &self,
        id: String,
        force: bool,
    ) -> Result<serde_json::Value, ManualRefreshError> {
        if self.is_draining() {
            return Err(ManualRefreshError::Unavailable);
        }
        match self
            .tokens
            .manual_refresh(
                self.store.clone(),
                self.observability.clone(),
                id.clone(),
                force,
            )
            .await
        {
            Ok(summary) => {
                serde_json::to_value(summary).map_err(|_| ManualRefreshError::Unavailable)
            }
            Err(ManualRefreshError::Terminal { outcome }) => {
                let store = self.store.clone();
                let account = tokio::task::spawn_blocking(move || store.load(&id))
                    .await
                    .map_err(|_| ManualRefreshError::Unavailable)?
                    .map_err(|_| ManualRefreshError::Unavailable)?;
                Ok(serde_json::json!({
                    "outcome":outcome,
                    "token_expires_at":account.token_meta.expires_at,
                    "last_refresh_at":account.token_meta.last_refresh_at
                }))
            }
            Err(error) => Err(error),
        }
    }

    pub fn start_refresh_scheduler(&self) -> tokio::task::JoinHandle<()> {
        self.tokens
            .start_refresh_scheduler(self.store.clone(), self.observability.clone())
    }
    pub async fn shutdown_refreshes(&self) -> Result<(), ()> {
        self.tokens.shutdown_refreshes().await
    }
    pub fn start_memory_guard(&self) -> tokio::task::JoinHandle<()> {
        let guard = self.memory_guard.clone();
        let obs = self.observability.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let sample = guard.sample();
                match sample.transition {
                    crate::guard::MemoryGuardTransition::On => {
                        obs.record_guard("guard_on");
                        obs.core_warning("", "memory_guard");
                    }
                    crate::guard::MemoryGuardTransition::Off => obs.record_guard("guard_off"),
                    crate::guard::MemoryGuardTransition::Unchanged => {}
                }
            }
        })
    }

    #[cfg(feature = "testkit")]
    pub fn force_memory_sample_for_test(
        &self,
        private_bytes: u64,
    ) -> crate::guard::MemoryGuardSample {
        let sample = self.memory_guard.force_sample(private_bytes);
        match sample.transition {
            crate::guard::MemoryGuardTransition::On => {
                self.observability.record_guard("guard_on");
                self.observability.core_warning("", "memory_guard");
            }
            crate::guard::MemoryGuardTransition::Off => {
                self.observability.record_guard("guard_off");
            }
            crate::guard::MemoryGuardTransition::Unchanged => {}
        }
        sample
    }

    /// Begin a takeover drain exactly once.  Admission is closed before any
    /// refresh or binding work begins, so a successor cannot observe a writer
    /// still consuming the old credential lease.
    pub async fn begin_drain(&self) -> Result<serde_json::Value, ()> {
        let _gate = self.drain_gate.lock().await;
        if !self.draining.swap(true, Ordering::AcqRel) {
            self.observability.record_takeover("drain_started");
            self.drain_started.store(true, Ordering::Release);
            self.drain_start_notify.notify_waiters();
        }
        if self.tokens.shutdown_refreshes().await.is_err() {
            return Err(());
        }
        let bindings = self.bindings.clone();
        let store = self.store.clone();
        let obs = self.observability.clone();
        let exported = tokio::task::spawn_blocking(move || {
            store.stop_writes();
            obs.freeze_quota()?;
            bindings.freeze()?;
            bindings.export()
        })
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(())?;
        self.drain_ready.store(true, Ordering::Release);
        self.drain_notify.notify_waiters();
        Ok(exported)
    }

    pub fn release_lease_after_drain(&self) {
        self.store.release_lease();
        self.observability.record_takeover("lease_released");
        self.lease_released.store(true, Ordering::Release);
        self.lease_notify.notify_waiters();
    }

    pub async fn wait_for_lease_release(&self) {
        loop {
            let notified = self.lease_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lease_released.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub async fn wait_for_drain(&self) {
        loop {
            let notified = self.drain_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.drain_ready.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub async fn wait_for_drain_start(&self) {
        if self.drain_started.load(Ordering::Acquire) {
            return;
        }
        let notified = self.drain_start_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.drain_started.load(Ordering::Acquire) {
            notified.await;
        }
    }

    pub fn set_drain_timeout(&self, seconds: u64) {
        self.drain_timeout_secs
            .store(seconds.clamp(1, 30 * 60), Ordering::Release);
    }
    pub fn drain_timeout(&self) -> Duration {
        Duration::from_secs(self.drain_timeout_secs.load(Ordering::Acquire))
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }
    async fn unavailable(
        &self,
        completion: &mut Completion,
        provider: Provider,
        websocket: bool,
        model: Option<&str>,
    ) -> Response {
        if !completion.usage_prepolled {
            completion.usage_prepolled = true;
            self.quota.before_unavailable(provider).await;
        }
        self.accounts
            .unavailable_known(provider, websocket, model)
            .await
    }
    fn memory_rejection(&self, provider: Provider) -> Option<Response> {
        // A zero limit is the deterministic test hook; production sampling is
        // performed by the 10-second guard task.
        if self.memory_guard.hard_limit() == 0 {
            let _ = self.memory_guard.sample();
        }
        if !self.memory_guard.is_guarded() {
            return None;
        }
        let mut response = native_error(
            StatusCode::SERVICE_UNAVAILABLE,
            provider,
            if provider == Provider::Claude {
                "api_error"
            } else {
                "server_error"
            },
            "quota-baton: memory guard tripped",
        );
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("5"));
        Some(response)
    }
    pub fn new(
        store: Arc<AccountStore>,
        config: &Config,
        observability: Arc<Observability>,
        pins: Arc<DebugPins>,
    ) -> anyhow::Result<Self> {
        config.upstream.validate()?;
        config.websocket.validate()?;
        config.routing.validate()?;
        config.quota.validate()?;
        let client = upstream_client()?;
        let accounts = crate::account_runtime::AccountRuntime::new(
            store.clone(),
            observability.clone(),
            config.routing.inflight_allowance_pct,
        );
        accounts.set_rate_hold_secs(config.routing.rate_hold_secs);
        let tokens = TokenGates::new(client.clone(), accounts.clone());
        let quota = crate::quota::UsagePoller::new(
            store.clone(),
            accounts.clone(),
            observability.clone(),
            client.clone(),
            tokens.clone(),
            config,
        );
        accounts.set_poller(&quota);
        // The lease is held and account expiry/reconciliation precedes affinity restore.
        store.list()?;
        let bindings = crate::bindings::BindingTable::new(store.home(), config.routing.clone())?;
        for (id, settings) in &config.accounts {
            bindings.set_account_threshold(id, settings.spread_threshold);
        }
        Ok(Self {
            accounts,
            quota,
            bindings,
            reset_hold_secs: config.routing.reset_hold_secs,
            rate_hold_secs: config.routing.rate_hold_secs,
            rate_hold_max_retries: config.routing.rate_hold_max_retries,
            ws_upgrade_hold_secs: config.routing.ws_upgrade_hold_secs,
            burst: Mutex::new(crate::burst::BurstTracker::new(
                config.tray.burst_new_sessions,
            )),
            inflight_requests: Arc::new(AtomicU64::new(0)),
            store,
            client,
            anthropic_base: config.upstream.anthropic_base.trim_end_matches('/').into(),
            codex_base: config.upstream.codex_base.trim_end_matches('/').into(),
            budget: InflightBudget::new(mib_bytes(config.guard.inflight_budget_mib)),
            retain_cap: mib_bytes(config.guard.retain_max_per_request_mib),
            memory_guard: Arc::new(MemoryGuard::new(
                mib_bytes(config.guard.memory_hard_limit_mib) as u64,
            )),
            observability,
            pins,
            tokens,
            websocket: config.websocket.clone(),
            reconnect_exclusions: Mutex::new(BTreeMap::new()),
            draining: AtomicBool::new(false),
            drain_started: AtomicBool::new(false),
            drain_ready: AtomicBool::new(false),
            lease_released: AtomicBool::new(false),
            drain_gate: tokio::sync::Mutex::new(()),
            drain_timeout_secs: AtomicU64::new(30 * 60),
            drain_notify: Notify::new(),
            drain_start_notify: Notify::new(),
            lease_notify: Notify::new(),
            handover_port: config.server.data_port,
            #[cfg(all(feature = "phase0", debug_assertions))]
            close_after_response: false,
            #[cfg(all(feature = "phase0", debug_assertions))]
            body_capture: None,
        })
    }

    #[cfg(all(feature = "phase0", debug_assertions))]
    pub fn with_body_capture(mut self, capture: Arc<crate::phase0::BodyCapture>) -> Self {
        self.body_capture = Some(capture);
        self
    }

    #[cfg(all(feature = "phase0", debug_assertions))]
    pub fn with_close_after_response(mut self, enabled: bool) -> Self {
        self.close_after_response = enabled;
        self
    }

    #[cfg(debug_assertions)]
    pub fn with_token_endpoint(mut self, provider: Provider, url: String) -> Self {
        self.tokens = self.tokens.with_token_endpoint(provider, url);
        self.quota.set_tokens(self.tokens.clone());
        self
    }

    async fn claim_probe(&self, account: &mut Account) -> Option<crate::account_runtime::Probe> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut delay = Duration::from_millis(10);
        loop {
            match self.accounts.begin_probe(account) {
                Ok(probe) => return probe,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(
                        delay.min(deadline.saturating_duration_since(Instant::now())),
                    )
                    .await;
                    delay = (delay * 2).min(Duration::from_millis(500));
                    let store = self.store.clone();
                    let id = account.id.clone();
                    match tokio::task::spawn_blocking(move || store.load(&id)).await {
                        Ok(Ok(updated)) => *account = updated,
                        _ => {
                            account.user_state = UserState::Paused;
                            return None;
                        }
                    }
                    if !account.eligible(None) {
                        return None;
                    }
                }
                Err(_) => {
                    account.user_state = UserState::Paused;
                    return None;
                }
            }
        }
    }

    pub(super) fn exclude_reconnect(&self, key: Option<&str>, account: &str) {
        if let Some(key) = key
            && let Ok(mut exclusions) = self.reconnect_exclusions.lock()
        {
            exclusions.retain(|_, expiry| *expiry > Instant::now());
            if exclusions.len() >= 4096 {
                exclusions.pop_first();
            }
            exclusions.insert(
                (key.to_owned(), account.to_owned()),
                Instant::now() + Duration::from_secs(120),
            );
        }
    }

    async fn candidates(
        &self,
        provider: Provider,
        key: Option<&str>,
        _model: Option<&str>,
        tried: &BTreeSet<String>,
    ) -> Result<Vec<crate::bindings::Candidate>, ()> {
        let store = self.store.clone();
        let mut tried = tried.clone();
        if let Some(key) = key
            && let Ok(mut exclusions) = self.reconnect_exclusions.lock()
        {
            exclusions.retain(|_, expiry| *expiry > Instant::now());
            tried.extend(
                exclusions
                    .keys()
                    .filter(|(session, _)| session == key)
                    .map(|(_, id)| id.clone()),
            );
        }
        let read_store = store.clone();
        let mut accounts = tokio::task::spawn_blocking(move || read_store.list())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        for account in &mut accounts {
            if account.provider == provider
                && account.user_state == UserState::Active
                && matches!(
                    account.health,
                    Health::Cooling {
                        reason: HealthReason::LinkStale,
                        ..
                    }
                )
                && let Ok(updated) = self
                    .tokens
                    .selection_reload(store.clone(), self.observability.clone(), account.clone())
                    .await
            {
                *account = updated;
            }
        }
        let runtime = self.accounts.clone();
        let obs = self.observability.clone();
        let bindings = self.bindings.clone();
        tokio::task::spawn_blocking(move || {
            accounts
                .into_iter()
                .filter(|a| {
                    a.provider == provider
                        && !tried.contains(&a.id)
                        && !runtime.probing(&a.id)
                        && !runtime.blocked_in_memory(&a.id)
                        && store.tokens(a).is_ok_and(|t| !t.access_token.is_empty())
                })
                .map(|account| {
                    let pacing = runtime.pacing_snapshot(&account.id);
                    bindings.set_pacing(
                        &account.id,
                        pacing.rate_step > 0
                            || pacing.rate_backoff_until.is_some()
                            || pacing.waiting > 0,
                    );
                    Ok(crate::bindings::Candidate {
                        quota: obs.quota_snapshot(&account.id),
                        // Every production attempt reserves the live table;
                        // a runtime count sampled before completion can be stale.
                        inflight: 0,
                        account,
                    })
                })
                .collect::<Result<Vec<_>, ()>>()
        })
        .await
        .map_err(|_| ())?
    }

    async fn note_client_rate(&self, provider: Provider, account: &str, key: Option<&str>) {
        let Ok(mut candidates) = self
            .candidates(provider, None, None, &BTreeSet::new())
            .await
        else {
            return;
        };
        let now = chrono::Utc::now();
        // Cooling/disabled accounts must not supply phantom rebalance room.
        // Keep them in ordinary selection so near-reset holds still work.
        candidates.retain(|candidate| candidate.account.eligible_at(None, now));
        let (active, threshold, groups_hit, marked) =
            self.bindings
                .note_client_rate(provider, account, key, &candidates, now);
        if marked > 0 || (groups_hit >= 3 && self.bindings.overload_report_due(account, now)) {
            self.observability
                .record_overload(account, active, threshold, groups_hit, marked);
        }
    }

    // Availability checks must never mutate affinity or reserve an attempt.
    async fn select(
        &self,
        provider: Provider,
        key: Option<&str>,
        model: Option<&str>,
        tried: &BTreeSet<String>,
    ) -> Result<Option<Account>, ()> {
        let candidates = self.candidates(provider, key, model, tried).await?;
        let pinned = self.pins.get(provider, key);
        match self.bindings.preview(
            provider,
            key,
            None,
            model,
            &candidates,
            pinned.as_deref(),
            0,
        ) {
            crate::bindings::PreviewDecision::Selected { account } => Ok(candidates
                .into_iter()
                .find(|c| c.account.id == account)
                .map(|c| c.account)),
            _ => Ok(None),
        }
    }

    async fn select_request(
        &self,
        provider: Provider,
        session: &session::Session,
        completion: &mut Completion,
        model: Option<&str>,
        tried: &BTreeSet<String>,
    ) -> Result<Option<Account>, ()> {
        // Post-commit/nonreplayable quota errors cannot retry the old body.
        // On its next request, resolve C7 before publishing any context.
        let prior = self.bindings.current(session.key.as_deref());
        let passive_sources = if prior.is_some() || session.key.is_none() {
            self.accounts.quota_observations(provider, prior.as_deref())
        } else { Vec::new() };
        let mut quota_sources = Vec::new();
        for ticket in self.accounts.pending_quota_tickets(crate::account_runtime::quota_session(session.key.as_deref()).as_deref()) {
            let store = self.store.clone();
            let pending_id = ticket.id.clone();
            let same_provider = tokio::task::spawn_blocking(move || store.load(&pending_id)
                .is_ok_and(|a| a.provider == provider)).await.unwrap_or(false);
            if same_provider && self.accounts.take_pending_quota(&ticket)
                && !self.retry_after_reset(session, completion, &ticket.id, model).await? {
                quota_sources.push((ticket.id, ticket.model));
            }
        }
        // A failed pre-commit attempt is finished before reserving its successor.
        completion.end_attempt();
        let excluded = tried.clone();
        let mut held_account = None;
        loop {
            let candidates = self
                .candidates(provider, session.key.as_deref(), model, &excluded)
                .await?;
            let pinned = self.pins.get(provider, session.key.as_deref());
            let bytes = completion
                .input_bytes
                .load(std::sync::atomic::Ordering::Relaxed);
            let choice = self.bindings.choose_with_hold(
                provider,
                session.key.as_deref(),
                session.parent.as_deref(),
                model,
                &candidates,
                pinned.as_deref(),
                bytes,
                !completion.reset_hold_used,
                completion.reset_hold_timed_out,
            );
            match choice {
                crate::bindings::Decision::Selected {
                    account,
                    lease,
                    moves,
                } => {
                    if completion.reset_hold_timed_out
                        && held_account.as_deref() == Some(account.as_str())
                    {
                        completion.reset_hold_timed_out = false;
                    }
                    for event in moves {
                        // Shared HTTP/WS path: only creation of a bound group counts.
                        if event.reason == "new"
                            && let Some(client) = completion.burst_client
                        {
                            let (crossed, excess) = {
                                let mut tracker =
                                    self.burst.lock().unwrap_or_else(|e| e.into_inner());
                                let crossed =
                                    tracker.new_placement(Instant::now(), client, &event.key);
                                (crossed, tracker.excess(client))
                            };
                            if let Some(count) = crossed {
                                self.observability.burst_warning(count);
                            } else if let Some(count) = excess {
                                self.observability.remember_burst(count);
                            }
                        }
                        self.observability.record_group_move(
                            &event.key,
                            event.from.as_deref(),
                            &event.to,
                            &event.reason,
                            event.bytes,
                            event.keys,
                            event.debug_pin,
                        );
                    }
                    completion.record.after_move |= lease.after_move;
                    completion.binding = Some(lease);
                    let selected = candidates.into_iter().find(|c| c.account.id == account).map(|c| c.account);
                    for (id, origin_model) in &quota_sources {
                        self.notice_quota(id, selected.as_ref(), origin_model.as_deref()).await;
                    }
                    for (id, state, observed_model) in &passive_sources {
                        if selected.as_ref().is_none_or(|selected| &selected.id != id)
                            && !quota_sources.iter().any(|(pending, _)| pending == id)
                            && self.accounts.quota_observation_current(id, state) {
                            self.notice_quota(id, selected.as_ref(), model.or(observed_model.as_deref())).await;
                        }
                    }
                    return Ok(selected);
                }
                crate::bindings::Decision::None if !completion.usage_prepolled => {
                    completion.usage_prepolled = true;
                    self.quota.before_unavailable(provider).await;
                }
                crate::bindings::Decision::None => {
                    for (id, origin_model) in &quota_sources {
                        self.notice_quota(id, None, origin_model.as_deref()).await;
                    }
                    for (id, state, observed_model) in &passive_sources {
                        if !quota_sources.iter().any(|(pending, _)| pending == id)
                            && self.accounts.quota_observation_current(id, state) {
                            self.notice_quota(id, None, model.or(observed_model.as_deref())).await;
                        }
                    }
                    return Ok(None);
                }
                crate::bindings::Decision::Hold { until } => {
                    held_account = self.bindings.current(session.key.as_deref());
                    let wait = (until - chrono::Utc::now()).to_std().unwrap_or_default();
                    completion.reset_hold_used = true;
                    tokio::time::sleep(wait.min(Duration::from_secs(self.reset_hold_secs))).await;
                    // This flag is request-local; a later turn may hold again.
                    completion.reset_hold_timed_out = true;
                }
            }
        }
    }

    /// C7 permits one same-account retry after a near reset, before commit.
    /// The request owns the budget; no session-wide hold flag survives it.
    async fn retry_after_reset(
        &self,
        session: &session::Session,
        completion: &mut Completion,
        id: &str,
        model: Option<&str>,
    ) -> Result<bool, ()> {
        if completion.reset_hold_used
            || self.bindings.current(session.key.as_deref()).as_deref() != Some(id)
        {
            tracing::debug!(account = %id, reason = "request_hold_already_used_or_binding_changed", "reset hold skipped");
            return Ok(false);
        }
        let store = self.store.clone();
        let account_id = id.to_owned();
        let account = tokio::task::spawn_blocking(move || store.load(&account_id))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        let now = chrono::Utc::now();
        let reset = match &account.health {
            Health::Cooling {
                reason: HealthReason::Quota,
                until,
                scope,
            } if account.user_state == UserState::Active
                && (scope == "account"
                    || scope
                        .strip_prefix("model:")
                        .is_some_and(|m| model == Some(m))) =>
            {
                account
                    .quota_cooling
                    .as_ref()
                    .map_or(*until, |q| q.reset_at)
            }
            _ => {
                tracing::debug!(account = %id, reason = "account_not_active_quota_cooling", "reset hold skipped");
                return Ok(false);
            }
        };
        let Ok(wait) = (reset - now).to_std() else {
            tracing::debug!(account = %id, reason = "reset_elapsed", "reset hold skipped");
            return Ok(false);
        };
        if wait > Duration::from_secs(self.reset_hold_secs) {
            tracing::debug!(account = %id, reason = "reset_beyond_hold_bound", wait_ms = wait.as_millis() as u64, "reset hold skipped");
            return Ok(false);
        }
        completion.end_attempt();
        completion.reset_hold_used = true;
        completion.failover("reset_hold_retry");
        tokio::time::sleep(wait).await;
        let store = self.store.clone();
        let account_id = id.to_owned();
        let account = tokio::task::spawn_blocking(move || store.load(&account_id))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        if !account.eligible(model) {
            completion.reset_hold_timed_out = true;
            tracing::debug!(account = %id, reason = "account_still_ineligible_after_reset", "reset hold skipped");
            return Ok(false);
        }
        completion.binding = Some(self.bindings.begin_request(session.key.as_deref(), id));
        Ok(true)
    }
}

fn upstream_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(16)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(600))
        .build()
        .map_err(|_| anyhow::anyhow!("Could not create upstream HTTP client"))
}

pub fn router(state: Arc<RelayState>) -> Router {
    Router::new().fallback(relay).with_state(state)
}

#[cfg(all(feature = "phase0", debug_assertions))]
#[derive(Clone)]
struct ForcedAccount(String);

#[cfg(all(feature = "phase0", debug_assertions))]
#[derive(serde::Deserialize)]
pub struct ReplayRequest {
    pub capture_path: std::path::PathBuf,
    pub account_id: String,
    #[serde(default)]
    pub strip_encrypted: crate::phase0::StripMode,
    #[serde(default)]
    pub tamper_encrypted: bool,
}

#[cfg(all(feature = "phase0", debug_assertions))]
pub fn phase0_routes(state: Arc<RelayState>) -> Router {
    Router::new()
        .route(
            "/admin/v1/phase0/replay",
            axum::routing::post(replay_capture),
        )
        .with_state(state)
}

#[cfg(all(feature = "phase0", debug_assertions))]
async fn replay_capture(
    State(state): State<Arc<RelayState>>,
    payload: Result<Json<ReplayRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let result = async {
        let Json(payload) =
            payload.map_err(|_| anyhow::anyhow!("Invalid phase0 replay request"))?;
        let store = state.store.clone();
        let id = payload.account_id.clone();
        let (account, tokens) = tokio::task::spawn_blocking(move || {
            let account = store.load(&id)?;
            let tokens = store.tokens(&account)?;
            Ok::<_, crate::accounts::StoreError>((account, tokens))
        })
        .await??;
        let prepared = tokio::task::spawn_blocking(move || {
            crate::phase0::prepare(
                &payload.capture_path,
                account.provider,
                payload.strip_encrypted,
                payload.tamper_encrypted,
            )
        })
        .await??;
        // 내보내는 것은 암호화 항목의 개수뿐이며 본문과 원래 헤더는 로그에 넣지 않는다.
        tracing::info!(presence = %prepared.presence, "Phase 0 replay presence");
        let diagnostics = prepared.diagnostics;
        let mut forbidden = prepared.forbidden;
        forbidden.push(tokens.access_token.clone());
        forbidden.push(tokens.refresh_token.clone());
        if let Some(token) = &tokens.id_token {
            forbidden.push(token.clone());
        }
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(prepared.route)
            .body(Body::from(prepared.body))?;
        *request.headers_mut() = prepared.headers;
        request
            .extensions_mut()
            .insert(ForcedAccount(account.id.clone()));
        // 제품의 선택 이후 경로를 그대로 실행한다. pin 변경이나 별도 토큰 읽기 경로를 만들지 않는다.
        let response = relay(State(state.clone()), request).await;
        let store = state.store.clone();
        let id = account.id.clone();
        if let Ok(Ok(tokens)) = tokio::task::spawn_blocking(move || {
            let current = store.load(&id)?;
            store.tokens(&current)
        })
        .await
        {
            forbidden.push(tokens.access_token.clone());
            forbidden.push(tokens.refresh_token.clone());
            if let Some(token) = &tokens.id_token {
                forbidden.push(token.clone());
            }
        }
        let mut summary = crate::phase0::summarize(response, &account.id, &forbidden).await?;
        if let Some(fields) = diagnostics.as_object() {
            for (key, value) in fields {
                summary[key] = value.clone();
            }
        }
        Ok::<_, anyhow::Error>(summary)
    }
    .await;
    match result {
        Ok(summary) => Json(summary).into_response(),
        Err(_) => native_error(
            StatusCode::BAD_REQUEST,
            Provider::Codex,
            "invalid_request_error",
            "Phase 0 replay failed; check capture path and account",
        ),
    }
}

fn route(method: &Method, path: &str, headers: &HeaderMap) -> Option<(Provider, String)> {
    match (method, path) {
        (&Method::POST, "/v1/messages" | "/v1/messages/count_tokens") => {
            Some((Provider::Claude, path.into()))
        }
        (&Method::GET, "/v1/models") if headers.contains_key("anthropic-version") => {
            Some((Provider::Claude, path.into()))
        }
        (&Method::GET, "/v1/models") => Some((Provider::Codex, "/models".into())),
        (
            &Method::POST,
            "/v1/responses"
            | "/v1/responses/compact"
            | "/v1/alpha/search"
            | "/v1/images/generations"
            | "/v1/images/edits",
        ) => Some((
            Provider::Codex,
            path.strip_prefix("/v1").expect("matched route").into(),
        )),
        _ => None,
    }
}

fn native_error(status: StatusCode, provider: Provider, kind: &str, message: &str) -> Response {
    let mut body = serde_json::json!({"error": {"type": kind, "message": message}});
    if provider == Provider::Claude {
        body["type"] = "error".into();
    }
    (status, Json(body)).into_response()
}

fn failure(
    completion: &mut Completion,
    status: StatusCode,
    provider: Provider,
    outcome: &str,
    message: &str,
) -> Response {
    completion.outcome(status.as_u16(), outcome);
    let kind = if status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND {
        "invalid_request_error"
    } else if provider == Provider::Claude {
        "api_error"
    } else {
        "server_error"
    };
    native_error(status, provider, kind, message)
}

/// Requests already accepted on an old keep-alive connection wait for the
/// successor listener, then use its normal authenticated data route. The old
/// process performs no account selection or token work for these requests.
async fn handover_http(state: Arc<RelayState>, request: Request) -> Response {
    let unavailable = || {
        native_error(
            StatusCode::SERVICE_UNAVAILABLE,
            Provider::Codex,
            "server_error",
            "quota-baton successor is unavailable",
        )
    };
    if state.handover_port == 0 {
        return unavailable();
    }
    let ready = tokio::time::timeout(Duration::from_secs(5), async {
        state.wait_for_lease_release().await;
        loop {
            match tokio::net::TcpStream::connect((
                std::net::Ipv4Addr::LOCALHOST,
                state.handover_port,
            ))
            .await
            {
                Ok(socket) => {
                    let _ = socket.set_nodelay(true);
                    drop(socket);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await;
    if ready.is_err() {
        return unavailable();
    }
    let (parts, body) = request.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
    let url = format!("http://127.0.0.1:{}{path}", state.handover_port);
    let mut headers = parts.headers;
    for name in [
        "connection",
        "transfer-encoding",
        "keep-alive",
        "proxy-connection",
    ] {
        headers.remove(name);
    }
    let result = state
        .client
        .request(parts.method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await;
    match result {
        Ok(response) => {
            let status = response.status();
            let mut headers = response.headers().clone();
            for name in ["connection", "transfer-encoding", "keep-alive"] {
                headers.remove(name);
            }
            let mut relayed = Response::new(axum::body::Body::from_stream(response.bytes_stream()));
            *relayed.status_mut() = status;
            *relayed.headers_mut() = headers;
            relayed
        }
        Err(_) => unavailable(),
    }
}

async fn relay(State(state): State<Arc<RelayState>>, request: Request) -> Response {
    if state.is_draining() {
        if request.method() != Method::GET || !request.headers().contains_key("upgrade") {
            return handover_http(state, request).await;
        }
        let mut response = native_error(
            StatusCode::SERVICE_UNAVAILABLE,
            Provider::Claude,
            "server_error",
            "quota-baton is handing over",
        );
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("1"));
        return response;
    }
    if request.method() == Method::GET
        && request.uri().path() == "/v1/responses"
        && request
            .headers()
            .get("upgrade")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return websocket::upgrade(state, request).await;
    }
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    let family = if path == "/v1/messages" || path.starts_with("/v1/messages/") {
        Provider::Claude
    } else {
        Provider::Codex
    };
    let routed = route(&parts.method, path, &parts.headers);
    let provider = routed.as_ref().map_or(family, |(family, _)| *family);
    let mut completion = Completion::new(
        state.observability.clone(),
        &parts.method,
        path,
        provider,
        &parts.headers,
    );
    completion.track(state.inflight_requests.clone());
    completion.runtime = Some(state.accounts.clone());
    let Some((_, upstream_path)) = routed else {
        return failure(
            &mut completion,
            StatusCode::NOT_FOUND,
            provider,
            "unsupported",
            "Unsupported route",
        );
    };
    if let Some(response) = state.memory_rejection(provider) {
        completion.outcome(response.status().as_u16(), "memory_guard");
        return response;
    }
    let incoming_zstd = parts
        .headers
        .get("content-encoding")
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"zstd"));
    let upload = match upload::read(
        body,
        state.retain_cap,
        completion.input_bytes.clone(),
        state.budget.clone(),
        incoming_zstd,
    )
    .await
    {
        Ok(upload) => upload,
        Err(_) => {
            return failure(
                &mut completion,
                StatusCode::BAD_REQUEST,
                provider,
                "body_error",
                "Could not read request body",
            );
        }
    };
    let mut retained = upload.retained;
    completion.record.retained_copy = retained.is_some();
    let mut session_body = upload.prefix.clone();
    if provider == Provider::Claude
        && !parts.headers.contains_key("x-claude-code-session-id")
        && let Some(handle) = retained.as_ref()
    {
        let _ = handle.complete().await;
        session_body = handle.metadata();
    }
    let session = session::classify(provider, path, &parts.headers, &session_body);
    completion.session(&session);
    let mut tried = BTreeSet::new();
    let mut request_model = session.model.clone();
    #[cfg(all(feature = "shape-capture", debug_assertions))]
    let shape_ticket = {
        let observability = state.observability.clone();
        let headers = parts.headers.clone();
        let prefix = upload.prefix.clone();
        // 선택적 키 관찰은 런타임 스레드에서 큰 JSON을 파싱하지 않는다.
        tokio::task::spawn_blocking(move || observability.capture_request(&headers, &prefix))
            .await
            .ok()
            .flatten()
    };
    #[cfg(all(feature = "phase0", debug_assertions))]
    let selection = if let Some(ForcedAccount(id)) = parts.extensions.get::<ForcedAccount>() {
        let store = state.store.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            store
                .load(&id)
                .map(|account| (account.provider == provider).then_some(account))
                .map_err(|_| ())
        })
        .await
        .unwrap_or(Err(()))
    } else {
        state
            .select_request(
                provider,
                &session,
                &mut completion,
                request_model.as_deref(),
                &tried,
            )
            .await
    };
    #[cfg(not(all(feature = "phase0", debug_assertions)))]
    let selection = state
        .select_request(
            provider,
            &session,
            &mut completion,
            request_model.as_deref(),
            &tried,
        )
        .await;
    let account = match selection {
        Ok(Some(account)) => account,
        Ok(None) => {
            let response = state
                .unavailable(&mut completion, provider, false, request_model.as_deref())
                .await;
            completion.outcome(response.status().as_u16(), "no_account");
            return response;
        }
        Err(()) => {
            return failure(
                &mut completion,
                StatusCode::BAD_GATEWAY,
                provider,
                "store_error",
                "Could not read account store",
            );
        }
    };
    completion.record.account = Some(account.id.clone());
    // Phase-0 forced replay bypasses affinity, but still participates in the
    // authoritative placement reservation count.
    if completion.binding.is_none() {
        completion.binding = Some(state.bindings.begin_request(None, &account.id));
    }
    tried.insert(account.id.clone());
    #[cfg(all(feature = "phase0", debug_assertions))]
    if state.retain_cap != 0
        && parts.extensions.get::<ForcedAccount>().is_none()
        && let Some(capture) = &state.body_capture
    {
        let capture = capture.clone();
        let route = path.to_owned();
        let headers = parts.headers.clone();
        let account = account.id.clone();
        let raw = upload.prefix.clone();
        if !matches!(
            tokio::task::spawn_blocking(move || capture.record(&route, &headers, &account, &raw))
                .await,
            Ok(Ok(_))
        ) {
            tracing::warn!("Phase 0 body capture failed");
        }
    }
    let (mut account, mut tokens) = match state
        .tokens
        .before_request(state.store.clone(), state.observability.clone(), account.id)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(()) => {
            return failure(
                &mut completion,
                StatusCode::BAD_GATEWAY,
                provider,
                "store_error",
                "Could not read account tokens",
            );
        }
    };
    let base = match provider {
        Provider::Claude => &state.anthropic_base,
        Provider::Codex => &state.codex_base,
    };
    let mut url = format!("{base}{upstream_path}");
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let mut body_to_send = upload.body;
    let mut network_retry = false;
    let mut token_retry = false;
    let mut rate_retries = 0_u32;
    let rate_hold_started = Instant::now();
    let rate_hold_deadline = rate_hold_started + Duration::from_secs(state.rate_hold_secs);
    let mut rate_relay_retry_after = None;
    let mut rate_relayed = false;
    let mut permission_denials = 0;
    let mut probe = state.claim_probe(&mut account).await;
    let (status, response_headers, held) = loop {
        if !account.eligible(request_model.as_deref()) {
            let next = state
                .select_request(
                    provider,
                    &session,
                    &mut completion,
                    request_model.as_deref(),
                    &tried,
                )
                .await;
            if tried.len() >= 4 || !matches!(next, Ok(Some(_))) {
                let response = state
                    .unavailable(&mut completion, provider, false, request_model.as_deref())
                    .await;
                completion.outcome(response.status().as_u16(), "accounts_exhausted");
                return response;
            }
            let next = next.unwrap().unwrap();
            drop(probe.take());
            tried.insert(next.id.clone());
            match state
                .tokens
                .before_request(state.store.clone(), state.observability.clone(), next.id)
                .await
            {
                Ok((updated, refreshed)) => {
                    account = updated;
                    tokens = refreshed;
                }
                Err(()) => {
                    return failure(
                        &mut completion,
                        StatusCode::BAD_GATEWAY,
                        provider,
                        "store_error",
                        "Could not read account tokens",
                    );
                }
            }
            probe = state.claim_probe(&mut account).await;
            network_retry = false;
            token_retry = false;
            continue;
        }
        completion.record.account = Some(account.id.clone());
        let admission_started = Instant::now();
        let admission = state
            .accounts
            .admit_until(&account.id, rate_hold_deadline)
            .await;
        completion.rate_wait(admission_started.elapsed().as_millis() as u64);
        let upstream_headers =
            match headers::upstream(&parts.headers, &account, &tokens.access_token) {
                Ok(headers) => headers,
                Err(()) => {
                    return failure(
                        &mut completion,
                        StatusCode::BAD_GATEWAY,
                        provider,
                        "credential_header_error",
                        "Could not create upstream credential headers",
                    );
                }
            };
        completion.attempt(&account.id);
        let sent_at = Instant::now();
        let send = state
            .client
            .request(parts.method.clone(), &url)
            .headers(upstream_headers)
            .body(body_to_send)
            .send();
        let response = match tokio::time::timeout(Duration::from_secs(300), send).await {
            Ok(Ok(response)) => response,
            _ => {
                if !network_retry
                    && let Some(copy) = &retained
                    && let Some(body) = copy.retry_body().await
                {
                    network_retry = true;
                    completion.failover("network_retry");
                    body_to_send = body;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                return failure(
                    &mut completion,
                    StatusCode::BAD_GATEWAY,
                    provider,
                    "upstream_error",
                    "Could not connect to upstream",
                );
            }
        };
        let status = response.status();
        completion.record.upstream_status = Some(status.as_u16());
        let response_headers = response.headers().clone();
        if status.is_success() || status == StatusCode::TOO_MANY_REQUESTS {
            state
                .accounts
                .observe_headers_for_model(
                    &account.id,
                    provider,
                    &response_headers,
                    request_model.as_deref(),
                )
                .await;
        }
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        state
            .observability
            .capture_response(shape_ticket.as_deref(), &response_headers);
        let held = hold::hold(
            status,
            &response_headers,
            Box::pin(response.bytes_stream()),
            Duration::from_secs(10),
        )
        .await;
        if held.decision == CommitDecision::StreamError && held.prefix.is_empty() {
            if !network_retry
                && let Some(copy) = &retained
                && let Some(body) = copy.retry_body().await
            {
                network_retry = true;
                completion.failover("network_retry");
                body_to_send = body;
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            return failure(
                &mut completion,
                StatusCode::BAD_GATEWAY,
                provider,
                "upstream_error",
                "Could not read upstream response",
            );
        }
        let before_commit = !status.is_success() || held.decision == CommitDecision::FirstError;
        if state.is_draining() {
            // Once admission closes the old owner only finishes byte relay.
            // In particular a delayed 401 cannot refresh or change health.
            break (status, response_headers, held);
        }
        if !before_commit {
            if status.is_success() {
                state.accounts.credit_headers(&account.id, &response_headers).await;
            }
            if probe.is_some()
                && status.is_success()
                && state.accounts.probe_success(&account.id).await.is_err()
            {
                return failure(
                    &mut completion,
                    StatusCode::BAD_GATEWAY,
                    provider,
                    "store_error",
                    "Could not persist account probe result",
                );
            }
            break (status, response_headers, held);
        }
        let mut decision = outcome::classify(
            provider,
            status,
            &response_headers,
            &held.prefix,
            request_model.as_deref(),
        );
        completion.record.upstream_error_code = hold::error_code(&response_headers, &held.prefix);
        if let Some(model) = decision.model.take() {
            request_model = Some(model);
        }
        if decision.kind == outcome::Kind::Revoked && credentials::is_linked(&account) {
            decision.kind = outcome::Kind::Auth;
            decision.signal = None;
        }
        if decision.kind == outcome::Kind::Revoked {
            match state
                .tokens
                .revoke_if_current(state.store.clone(), &account, &tokens)
                .await
            {
                Ok(updated) if !matches!(updated.health, Health::NeedsRelogin { .. }) => {
                    decision.kind = outcome::Kind::Auth;
                }
                Ok(_) => {}
                Err(()) => {
                    return failure(
                        &mut completion,
                        StatusCode::BAD_GATEWAY,
                        provider,
                        "store_error",
                        "Could not persist account health",
                    );
                }
            }
            decision.signal = None;
        }
        if decision.kind == outcome::Kind::Auth {
            if token_retry {
                if state
                    .tokens
                    .auth_failed(
                        state.store.clone(),
                        state.observability.clone(),
                        account.clone(),
                        &tokens,
                    )
                    .await
                    .is_err()
                {
                    return failure(
                        &mut completion,
                        StatusCode::BAD_GATEWAY,
                        provider,
                        "store_error",
                        "Could not persist account health",
                    );
                }
            } else {
                match state
                    .tokens
                    .after_401(
                        state.store.clone(),
                        state.observability.clone(),
                        account.clone(),
                        &tokens,
                    )
                    .await
                {
                    Ok(Some((updated, refreshed))) => {
                        if !token_retry
                            && let Some(copy) = &retained
                            && let Some(body) = copy.retry_body().await
                        {
                            token_retry = true;
                            completion.failover("token_retry");
                            account = updated;
                            tokens = refreshed;
                            body_to_send = body;
                            continue;
                        }
                    }
                    Ok(None) => {
                        if state
                            .tokens
                            .cool_unavailable_auth(state.store.clone(), &account, &tokens)
                            .await
                            .is_err()
                        {
                            return failure(
                                &mut completion,
                                StatusCode::BAD_GATEWAY,
                                provider,
                                "store_error",
                                "Could not persist account health",
                            );
                        }
                    }
                    Err(()) => {
                        return failure(
                            &mut completion,
                            StatusCode::BAD_GATEWAY,
                            provider,
                            "store_error",
                            "Could not update account credentials",
                        );
                    }
                }
            }
        } else if decision.kind == outcome::Kind::Rate {
            let retry_after = outcome::header_seconds(&response_headers, "retry-after")
                .map(|seconds| Duration::from_secs(seconds.min(60) as u64));
            let pacing = state.accounts.observe_429_at(
                &account.id,
                sent_at,
                Instant::now(),
                retry_after,
                1.0,
            );
            if retry_after.is_none() {
                rate_relay_retry_after = pacing.rate_backoff_until.map(|until| {
                    let remaining = until.saturating_duration_since(Instant::now());
                    (remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0)).clamp(1, 60)
                });
            }
            if let Some(copy) = retained.as_ref()
                && rate_retries < state.rate_hold_max_retries
                && !admission.forced()
                && Instant::now() < rate_hold_deadline
                && let Some(body) = copy.retry_body().await
            {
                rate_retries += 1;
                completion.rate_retry();
                body_to_send = body;
                // The next shared admission owns all rate delay. Its
                // absolute deadline is unchanged across same-account retries.
                continue;
            }
            rate_relayed = true;
            state
                .note_client_rate(provider, &account.id, session.key.as_deref())
                .await;
            completion.relayed_rate_429();
            completion.outcome(status.as_u16(), "rate_hold_relayed_429");
        } else if let Some(signal) = decision.signal.take()
            && state
                .accounts
                .transition(&account.id, signal)
                .await
                .is_err()
        {
            return failure(
                &mut completion,
                StatusCode::BAD_GATEWAY,
                provider,
                "store_error",
                "Could not persist account health",
            );
        }
        if decision.kind == outcome::Kind::Quota {
            state.accounts.observe_quota_reset(&account.id, decision.quota_reset);
        }
        if matches!(decision.kind, outcome::Kind::Quota | outcome::Kind::Rate) && held.complete {
            state
                .accounts
                .cache_limit(&account.id, status, &response_headers, &held.prefix);
        }
        if decision.kind == outcome::Kind::Permission {
            permission_denials += 1;
        }
        let quota_pending = if decision.kind == outcome::Kind::Quota {
            state.accounts.pending_quota(&account.id, completion.record.session.clone(), request_model.as_deref()).await
        } else { None };
        // M4b는 select만 교체한다. 보관본 없는 요청과 commit 이후에는 재전송하지 않는다.
        if !decision.failover() || retained.is_none() || permission_denials >= 2 {
            break (status, response_headers, held);
        }
        if let Some(copy) = retained.as_ref()
            && copy.complete().await.is_none()
        {
            break (status, response_headers, held);
        }
        if decision.kind == outcome::Kind::Quota {
            if let Some(ticket) = &quota_pending { state.accounts.take_pending_quota(ticket); }
            drop(probe.take());
            match state
                .retry_after_reset(
                    &session,
                    &mut completion,
                    &account.id,
                    request_model.as_deref(),
                )
                .await
            {
                Ok(true) => {
                    drop(probe.take());
                    match state
                        .tokens
                        .before_request(
                            state.store.clone(),
                            state.observability.clone(),
                            account.id.clone(),
                        )
                        .await
                    {
                        Ok((updated, refreshed)) => {
                            account = updated;
                            tokens = refreshed;
                        }
                        Err(()) => {
                            return failure(
                                &mut completion,
                                StatusCode::BAD_GATEWAY,
                                provider,
                                "store_error",
                                "Could not read account tokens",
                            );
                        }
                    }
                    probe = state.claim_probe(&mut account).await;
                    body_to_send = retained
                        .as_ref()
                        .expect("retained reset retry")
                        .complete()
                        .await
                        .expect("retained body completed")
                        .retry_body();
                    continue;
                }
                Ok(false) => {}
                Err(()) => {
                    return failure(
                        &mut completion,
                        StatusCode::BAD_GATEWAY,
                        provider,
                        "store_error",
                        "Could not check account reset",
                    );
                }
            }
        }
        if tried.len() >= 4 {
            if decision.kind == outcome::Kind::Quota {
                state.notice_quota(&account.id, None, request_model.as_deref()).await;
            }
            if state
                .select(
                    provider,
                    session.key.as_deref(),
                    request_model.as_deref(),
                    &tried,
                )
                .await
                .is_ok_and(|next| next.is_none())
                && matches!(
                    decision.kind,
                    outcome::Kind::Quota
                        | outcome::Kind::Rate
                        | outcome::Kind::Auth
                        | outcome::Kind::Revoked
                )
            {
                let response = state
                    .unavailable(&mut completion, provider, false, request_model.as_deref())
                    .await;
                completion.outcome(response.status().as_u16(), "accounts_exhausted");
                return response;
            }
            break (status, response_headers, held);
        }
        completion.failover(decision.reason());
        let next = state
            .select_request(
                provider,
                &session,
                &mut completion,
                request_model.as_deref(),
                &tried,
            )
            .await;
        if decision.kind == outcome::Kind::Quota && let Ok(selected) = &next {
            state.notice_quota(&account.id, selected.as_ref(), request_model.as_deref()).await;
        }
        match next {
            Ok(Some(next)) => {
                drop(probe.take());
                tried.insert(next.id.clone());
                let snapshot = state
                    .tokens
                    .before_request(state.store.clone(), state.observability.clone(), next.id)
                    .await;
                match snapshot {
                    Ok((updated, refreshed)) => {
                        account = updated;
                        tokens = refreshed;
                    }
                    Err(()) => {
                        return failure(
                            &mut completion,
                            StatusCode::BAD_GATEWAY,
                            provider,
                            "store_error",
                            "Could not read account tokens",
                        );
                    }
                }
                probe = state.claim_probe(&mut account).await;
                network_retry = false;
                token_retry = false;
                body_to_send = retained
                    .as_ref()
                    .expect("retained failover")
                    .complete()
                    .await
                    .expect("retained body completed")
                    .retry_body();
            }
            Ok(None)
                if matches!(
                    decision.kind,
                    outcome::Kind::Payment
                        | outcome::Kind::ModelPermission
                        | outcome::Kind::Permission
                ) =>
            {
                break (status, response_headers, held);
            }
            Ok(None) => {
                let response = state
                    .unavailable(&mut completion, provider, false, request_model.as_deref())
                    .await;
                completion.outcome(response.status().as_u16(), "accounts_exhausted");
                return response;
            }
            Err(()) => {
                return failure(
                    &mut completion,
                    StatusCode::BAD_GATEWAY,
                    provider,
                    "store_error",
                    "Could not select an account",
                );
            }
        }
    };
    let decision = held.decision;
    if !status.is_success() || decision == CommitDecision::FirstError {
        completion.record.upstream_error_code = hold::error_code(&response_headers, &held.prefix);
    }
    if let Some(handle) = retained.as_ref() {
        let _ = handle.complete().await;
    }
    completion.record.retained_copy = retained
        .as_ref()
        .is_some_and(|handle| handle.has_retention());
    drop(retained.take());
    completion.record.status = status.as_u16();
    completion.record.ttfb_ms = completion.elapsed_ms();
    let success_outcome = if rate_relayed {
        "rate_hold_relayed_429"
    } else if decision == CommitDecision::FirstError {
        "first_event_error"
    } else if rate_retries > 0 && status.is_success() {
        "rate_hold_ok"
    } else if status.is_success() {
        "ok"
    } else {
        "upstream_status_error"
    };
    let mut output_headers = headers::filtered(&response_headers, false);
    if rate_relayed && !output_headers.contains_key("retry-after") {
        let seconds = response_headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .or(rate_relay_retry_after)
            .unwrap_or(1)
            .clamp(1, 60);
        output_headers.insert(
            "retry-after",
            HeaderValue::from_str(&seconds.to_string()).expect("bounded retry-after"),
        );
    }
    if let Ok(value) = HeaderValue::from_str(&account.id) {
        output_headers.insert("x-quota-baton-account", value);
    }
    completion.failed_access = Some(zeroize::Zeroizing::new(tokens.access_token.clone()));
    let body = response::body(completion, held, status, &response_headers, success_outcome);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = output_headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_methods_families_paths() {
        let mut headers = HeaderMap::new();
        for path in ["/v1/messages", "/v1/messages/count_tokens"] {
            assert_eq!(
                route(&Method::POST, path, &headers),
                Some((Provider::Claude, path.into()))
            );
            assert!(route(&Method::GET, path, &headers).is_none());
        }
        for path in [
            "/v1/responses",
            "/v1/responses/compact",
            "/v1/alpha/search",
            "/v1/images/generations",
            "/v1/images/edits",
        ] {
            assert_eq!(
                route(&Method::POST, path, &headers),
                Some((Provider::Codex, path[3..].into()))
            );
        }
        assert_eq!(
            route(&Method::GET, "/v1/models", &headers),
            Some((Provider::Codex, "/models".into()))
        );
        headers.insert("anthropic-version", "fixture".parse().unwrap());
        assert_eq!(
            route(&Method::GET, "/v1/models", &headers),
            Some((Provider::Claude, "/v1/models".into()))
        );
        for path in ["/v1/memories/trace_summarize", "/unknown", "/v1/responses/"] {
            assert!(route(&Method::POST, path, &headers).is_none());
        }
    }
    #[tokio::test]
    async fn tray2_three_accounts_reconnect_exclusions_name_actual_upstream_or_suppress_false_exhaustion() {
        use qb_testkit::{ScriptedResponse, ScriptedUpstream};
        use serde_json::{Value, json};
        for exclude_all in [false, true] {
            let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r1-tray2-fixtures");
            std::fs::create_dir_all(&base).unwrap();
            let root = tempfile::tempdir_in(base).unwrap();
            let cpa = root.path().join("cpa");
            std::fs::create_dir_all(&cpa).unwrap();
            let mut activate = Vec::new();
            for n in 0..3 {
                let email = format!("tray2-{n}@example.invalid");
                std::fs::write(cpa.join(format!("{n}.json")), serde_json::to_vec(&json!({
                    "type":"codex","email":email,"account_id":format!("synthetic-{n}"),
                    "access_token":format!("synthetic-access-{n}"),"refresh_token":"synthetic-refresh",
                    "expired":"2099-01-01T00:00:00Z", "last_refresh":chrono::Utc::now().to_rfc3339()
                })).unwrap()).unwrap();
                activate.push(format!("codex:{email}"));
            }
            let store = Arc::new(AccountStore::acquire(&root.path().join("home")).unwrap());
            crate::accounts::import::run(&store, &cpa, &activate, false).unwrap();
            let fake = ScriptedUpstream::start().await.unwrap();
            let mut config = Config::default();
            config.server.data_port = 0;
            config.server.admin_port = 0;
            config.upstream.codex_base = fake.url.clone();
            config.upstream.anthropic_base = fake.url.clone();
            let obs = Arc::new(Observability::new(store.home(), None).unwrap());
            let state = Arc::new(RelayState::new(store.clone(), &config, obs,
                Arc::new(DebugPins::default())).unwrap());
            let servers = crate::http::spawn_with_routes(&config, crate::local_secrets::LocalSecrets {
                client_key:"synthetic-tray2-key".into(),admin_token:"synthetic-tray2-admin".into()
            }, router(state.clone()), axum::Router::new()).await.unwrap();
            let client = reqwest::Client::new();
            let request = || client.post(format!("http://{}/v1/responses", servers.data_addr))
                .bearer_auth("synthetic-tray2-key").header("session-id", "tray2-exclusions").body("{}");
            fake.state.push(ScriptedResponse::json(StatusCode::OK, "{}"));
            let warm = request().send().await.unwrap();
            let source = warm.headers()["x-quota-baton-account"].to_str().unwrap().to_owned();
            warm.bytes().await.unwrap();
            let others: Vec<_> = store.list().unwrap().into_iter().filter(|a| a.id != source).collect();
            state.exclude_reconnect(Some("codex:tray2-exclusions"), &others[0].id);
            if exclude_all { state.exclude_reconnect(Some("codex:tray2-exclusions"), &others[1].id); }
            let reset = chrono::Utc::now().timestamp() + 172800;
            fake.state.push(ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS,
                json!({"error":{"type":"usage_limit_reached","resets_at":reset}}).to_string()));
            fake.state.push(ScriptedResponse::json(StatusCode::OK, "{}"));
            let mut events = state.accounts.subscribe();
            let response = request().send().await.unwrap();
            let status = response.status();
            let served = response.headers().get("x-quota-baton-account").and_then(|v| v.to_str().ok()).map(str::to_owned);
            response.bytes().await.unwrap();
            let mut notices: Vec<Value> = Vec::new();
            while let Ok(event) = events.try_recv() {
                if event["event"] == "core_warning" && event["data"]["kind"] == "quota_notice" { notices.push(event["data"].clone()); }
            }
            if exclude_all {
                assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
                assert!(notices.is_empty(), "request exclusions are not provider exhaustion");
                assert!(state.accounts.views().await.unwrap().iter().all(|v| v["quota_notice"].is_null()));
            } else {
                assert_eq!(status, StatusCode::OK);
                assert_eq!(served.as_deref(), Some(others[1].id.as_str()));
                let notice = notices.iter().find(|n| n["action"] == "failover").unwrap();
                assert_eq!(notice["account"], source);
                assert_eq!(notice["next_account"], served.unwrap());
                let token = store.tokens(&others[1]).unwrap();
                assert_eq!(fake.state.requests().last().unwrap().headers["authorization"], format!("Bearer {}", token.access_token));
            }
        }
    }

    #[tokio::test]
    async fn selection_reload_wait_does_not_block_another_placement_in_the_family() {
        use crate::{
            config::Config,
            debug_pin::DebugPins,
            relay::{RelayState, ledger::Completion},
            session::Session,
        };
        let sandbox = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/hardening-fixtures");
        std::fs::create_dir_all(&sandbox).unwrap();
        let root = tempfile::tempdir_in(sandbox).unwrap();
        let cpa = root.path().join("cpa");
        std::fs::create_dir_all(&cpa).unwrap();
        std::fs::write(cpa.join("synthetic.json"), serde_json::to_vec(&serde_json::json!({
            "type":"codex", "email":"placement@example.invalid", "account_id":"synthetic-placement",
            "access_token":"synthetic-placement-access", "refresh_token":"synthetic-placement-refresh",
            "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-10-13T00:00:01Z"
        })).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let store = loop {
            match AccountStore::acquire(&root.path().join("home")) {
                Ok(store) => break Arc::new(store),
                Err(crate::accounts::StoreError::LeaseBusy) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => panic!("synthetic fixture lease: {error}"),
            }
        };
        crate::accounts::import::run(
            &store,
            &cpa,
            &["codex:placement@example.invalid".into()],
            false,
        )
        .unwrap();
        let id = store.list().unwrap()[0].id.clone();
        let obs = Arc::new(Observability::new(store.home(), None).unwrap());
        let mut config = Config::default();
        config.upstream.codex_base = "http://127.0.0.1:9".into();
        config.upstream.anthropic_base = "http://127.0.0.1:9".into();
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap(),
        );
        store
            .update_metadata(&id, |account| {
                account.health = Health::Cooling {
                    reason: HealthReason::LinkStale,
                    until: chrono::Utc::now() + chrono::Duration::hours(1),
                    scope: "account".into(),
                }
            })
            .unwrap();
        let guard = state.tokens.block_selection_for_test(&id).await;
        let waiting_state = state.clone();
        let waiting_obs = obs.clone();
        let first = tokio::spawn(async move {
            let mut completion = Completion::new(
                waiting_obs,
                &axum::http::Method::POST,
                "/v1/responses",
                Provider::Codex,
                &axum::http::HeaderMap::new(),
            );
            let session = Session {
                key: Some("codex:waiting".into()),
                parent: None,
                model: None,
            };
            waiting_state
                .select_request(
                    Provider::Codex,
                    &session,
                    &mut completion,
                    None,
                    &std::collections::BTreeSet::new(),
                )
                .await
        });
        // The first selection is suspended at its credential gate. Restore the
        // account so an independent selection can bypass that reload.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!first.is_finished());
        store
            .update_metadata(&id, |account| account.health = Health::Ready)
            .unwrap();
        let mut completion = Completion::new(
            obs,
            &axum::http::Method::POST,
            "/v1/responses",
            Provider::Codex,
            &axum::http::HeaderMap::new(),
        );
        let session = Session {
            key: Some("codex:independent".into()),
            parent: None,
            model: None,
        };
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            state.select_request(
                Provider::Codex,
                &session,
                &mut completion,
                None,
                &std::collections::BTreeSet::new(),
            ),
        )
        .await;
        first.abort();
        let _ = first.await;
        drop(guard);
        assert_eq!(second.unwrap().unwrap().unwrap().id, id);
    }
}

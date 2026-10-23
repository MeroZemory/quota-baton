pub(crate) mod credentials;
mod headers;
mod hold;
mod http_data;
mod images;
mod launch;
mod ledger;
mod models;
pub(crate) mod notices;
mod outcome;
mod quota_wait;
mod response;
mod selection_guard;
mod upload;
mod websocket;
mod wire_metadata;

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
pub use launch::{LaunchError, LaunchReply, LaunchRequest};
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

#[cfg(feature = "testkit")]
#[derive(Clone, Debug)]
pub struct HttpQuotaWaitEvent {
    pub stage: &'static str,
    pub source: String,
}

#[cfg(feature = "testkit")]
#[derive(Clone, Debug)]
pub struct WsQuotaWaitEvent {
    pub stage: &'static str,
    pub source: String,
    pub deadline: tokio::time::Instant,
}

/// Read-only bounds of a genuine native result, never proof or send authority.
#[cfg(feature = "testkit")]
#[derive(Debug)]
pub struct HttpQuotaWaitResultBounds {
    pub source: String,
    pub reset_at: chrono::DateTime<chrono::Utc>,
    pub wake: tokio::time::Instant,
    pub deadline: tokio::time::Instant,
}

/// One owner-local, first-inspection delivery barrier. Drop releases the worker.
#[cfg(feature = "testkit")]
pub struct HttpQuotaWaitResultGate {
    entered: tokio::sync::oneshot::Receiver<HttpQuotaWaitResultBounds>,
    release: Option<std::sync::mpsc::SyncSender<()>>,
}

#[cfg(feature = "testkit")]
impl HttpQuotaWaitResultGate {
    pub async fn entered(
        &mut self,
    ) -> Result<HttpQuotaWaitResultBounds, tokio::sync::oneshot::error::RecvError> {
        (&mut self.entered).await
    }

    pub fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.try_send(());
        }
    }
}

#[cfg(feature = "testkit")]
impl Drop for HttpQuotaWaitResultGate {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.try_send(());
        }
    }
}

#[cfg(feature = "testkit")]
struct HttpQuotaWaitResultHold {
    key: String,
    source: String,
    entered: tokio::sync::oneshot::Sender<HttpQuotaWaitResultBounds>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(feature = "testkit")]
impl HttpQuotaWaitResultHold {
    fn wait(self, bounds: HttpQuotaWaitResultBounds) {
        if self.entered.send(bounds).is_ok() {
            self.release
                .recv_timeout(Duration::from_secs(10))
                .expect("bounded native-result delivery gate abandoned");
        }
    }
}

pub struct RelayState {
    pub accounts: Arc<crate::account_runtime::AccountRuntime>,
    pub quota: Arc<crate::quota::UsagePoller>,
    pub bindings: Arc<crate::bindings::BindingTable>,
    pub(crate) launch_tokens: crate::launch_tokens::LaunchTokenRegistry,
    pub(crate) catalogs: crate::catalog::CatalogCache,
    catalog_slots: tokio::sync::Semaphore,
    catalog_changed: Notify,
    burst: Mutex<crate::burst::BurstTracker>,
    rate_hold_secs: u64,
    rate_hold_max_retries: u32,
    ws_upgrade_hold_secs: u64,
    request_timeout_secs: u64,
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
    images: Mutex<images::ImageState>,
    image_account_config: BTreeMap<String, crate::config::AccountConfig>,
    #[cfg(all(feature = "testkit", debug_assertions))]
    takeover_test_dependencies: Option<crate::accounts::takeover::Dependencies>,
    websocket: crate::config::WebsocketConfig,
    ws_capacity: Arc<websocket::capacity::Capacity>,
    reconnect_exclusions: Mutex<BTreeMap<(String, String), Instant>>,
    #[cfg(feature = "testkit")]
    http_quota_wait_events: tokio::sync::broadcast::Sender<HttpQuotaWaitEvent>,
    #[cfg(feature = "testkit")]
    http_quota_wait_inspections: tokio::sync::broadcast::Sender<HttpQuotaWaitEvent>,
    #[cfg(feature = "testkit")]
    http_quota_wait_result_hold: Mutex<Option<HttpQuotaWaitResultHold>>,
    #[cfg(feature = "testkit")]
    ws_quota_wait_events: tokio::sync::broadcast::Sender<WsQuotaWaitEvent>,
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
    pub fn routing_view(&self) -> serde_json::Value {
        let mut view = self.bindings.routing_view();
        let accounts = self.store.list().unwrap_or_default();
        view["forecast_shadow"] = serde_json::Value::Array(accounts.iter().map(|a| {
            serde_json::json!({"account":a.id,"forecast":self.observability.forecast_view(&a.id,
                self.bindings.active_sessions(&a.id, chrono::Utc::now()), self.accounts.inflight_count(&a.id))})
        }).collect());
        view
    }
    async fn prepare_project(
        &self,
        session: &session::Session,
        hint: Option<&crate::routing::ProjectHint>,
    ) -> Result<(), ()> {
        let Some(project) = hint.and_then(|hint| hint.0.clone()) else {
            return Ok(());
        };
        let key = session.key.clone().ok_or(())?;
        let table = self.bindings.clone();
        tokio::task::spawn_blocking(move || table.associate_project(&key, &project))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())
    }
    #[cfg(all(feature = "testkit", debug_assertions))]
    pub fn validate_takeover_test_inputs(
        home: &std::path::Path,
        source_root: &std::path::Path,
        profile_endpoint: &str,
    ) -> anyhow::Result<()> {
        crate::accounts::takeover_native::validate_synthetic_inputs(
            home,
            source_root,
            profile_endpoint,
        )
        .map(|_| ())
        .map_err(|_| {
            anyhow::anyhow!(
                "합성 인계 검사 경로에는 junction을 쓸 수 없으며 .sandbox 안이어야 합니다."
            )
        })
    }

    #[cfg(all(feature = "testkit", debug_assertions))]
    pub fn with_takeover_test_source(
        mut self,
        source_root: &std::path::Path,
        profile_endpoint: String,
    ) -> anyhow::Result<Self> {
        self.takeover_test_dependencies = Some(
            crate::accounts::takeover_native::synthetic(
                self.client.clone(),
                self.store.home(),
                source_root,
                profile_endpoint,
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "합성 인계 검사는 .sandbox 홈과 numeric loopback 주소가 필요합니다."
                )
            })?,
        );
        Ok(self)
    }

    pub(crate) async fn takeover_account(
        &self,
        request: crate::accounts::takeover::TakeoverRequest,
    ) -> Result<serde_json::Value, crate::accounts::takeover::TakeoverError> {
        if self.is_draining() {
            return Err(crate::accounts::takeover::TakeoverError::Unavailable);
        }
        let dependencies = crate::accounts::takeover_native::production(self.client.clone());
        #[cfg(all(feature = "testkit", debug_assertions))]
        let dependencies = self
            .takeover_test_dependencies
            .clone()
            .unwrap_or(dependencies);
        self.tokens
            .takeover(
                self.store.clone(),
                self.observability.clone(),
                request,
                dependencies,
            )
            .await
    }

    pub(crate) fn recent_warnings(&self) -> Vec<serde_json::Value> {
        self.observability.recent_warnings()
    }
    pub fn move_cost_snapshot(&self) -> crate::observability::reread_accounting::Snapshot {
        self.observability.accounting_snapshot(chrono::Utc::now())
    }
    pub fn dashboard(&self) -> &Arc<crate::dashboard::runtime::Runtime> {
        self.observability.dashboard()
    }
    pub fn login_service(&self) -> crate::oauth::LoginService {
        crate::oauth::LoginService::new(
            self.store.clone(),
            self.client.clone(),
            self.observability.clone(),
        )
        .with_runtime(self.accounts.clone())
    }
    #[cfg(all(feature = "testkit", debug_assertions))]
    pub fn test_login_service(
        &self,
        config: crate::oauth::LoginTestConfig,
    ) -> Result<crate::oauth::LoginService, crate::oauth::LoginError> {
        crate::oauth::LoginService::with_test_config(
            self.store.clone(),
            self.client.clone(),
            self.observability.clone(),
            config,
        )
        .map(|service| service.with_runtime(self.accounts.clone()))
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
            let exported = bindings.export()?;
            obs.shutdown_replay();
            Ok::<_, anyhow::Error>(exported)
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

    #[cfg(feature = "testkit")]
    pub fn subscribe_http_quota_wait(
        &self,
    ) -> tokio::sync::broadcast::Receiver<HttpQuotaWaitEvent> {
        self.http_quota_wait_events.subscribe()
    }

    /// Arm exactly one matching first-result delivery gate on this relay.
    #[cfg(feature = "testkit")]
    pub fn hold_next_http_quota_wait_result(
        &self,
        key: &str,
        source: &str,
    ) -> HttpQuotaWaitResultGate {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let mut slot = self.http_quota_wait_result_hold.lock().unwrap();
        assert!(slot.is_none(), "one delivery gate per relay");
        *slot = Some(HttpQuotaWaitResultHold {
            key: key.to_owned(),
            source: source.to_owned(),
            entered: entered_tx,
            release: release_rx,
        });
        HttpQuotaWaitResultGate {
            entered: entered_rx,
            release: Some(release_tx),
        }
    }

    #[cfg(feature = "testkit")]
    fn take_http_quota_wait_result_hold(
        &self,
        key: &str,
        source: &str,
    ) -> Option<HttpQuotaWaitResultHold> {
        let mut slot = self.http_quota_wait_result_hold.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|hold| hold.key == key && hold.source == source)
        {
            slot.take()
        } else {
            None
        }
    }

    /// Observation only; separate channel preserves existing wait-event order.
    #[cfg(feature = "testkit")]
    pub fn subscribe_http_quota_wait_inspections(
        &self,
    ) -> tokio::sync::broadcast::Receiver<HttpQuotaWaitEvent> {
        self.http_quota_wait_inspections.subscribe()
    }

    #[cfg(feature = "testkit")]
    fn observe_http_quota_wait_inspection(&self, stage: &'static str, source: &str) {
        let _ = self.http_quota_wait_inspections.send(HttpQuotaWaitEvent {
            stage,
            source: source.to_owned(),
        });
    }

    #[cfg(feature = "testkit")]
    fn observe_http_quota_wait(&self, stage: &'static str, source: &str) {
        let _ = self.http_quota_wait_events.send(HttpQuotaWaitEvent {
            stage,
            source: source.to_owned(),
        });
    }

    #[cfg(feature = "testkit")]
    pub fn subscribe_ws_quota_wait(&self) -> tokio::sync::broadcast::Receiver<WsQuotaWaitEvent> {
        self.ws_quota_wait_events.subscribe()
    }

    /// Synthetic cache-loss setup; refuses to remove an active catalog refresh.
    #[cfg(all(feature = "testkit", debug_assertions))]
    pub fn clear_codex_catalog_for_test(
        &self,
        account: &str,
    ) -> Result<bool, crate::catalog::CatalogError> {
        self.catalogs.remove(account)
    }

    #[cfg(feature = "testkit")]
    fn observe_ws_quota_wait(
        &self,
        stage: &'static str,
        source: &str,
        deadline: tokio::time::Instant,
    ) {
        let _ = self.ws_quota_wait_events.send(WsQuotaWaitEvent {
            stage,
            source: source.to_owned(),
            deadline,
        });
    }

    fn http_wait_reset_source_current(&self, key: &str, source: &str, model: Option<&str>) -> bool {
        self.bindings
            .models_launch_snapshot(Some(key), None)
            .is_ok_and(|info| {
                info.account == source && (!info.managed || info.current_model.as_deref() == model)
            })
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
        if completion.record.outcome == "move_guard_hold" {
            let mut response = native_error(
                StatusCode::TOO_MANY_REQUESTS,
                provider,
                "account_move_guard",
                "계정 이동 안전장치가 반복 이동을 막았습니다. 기존 계정에서 대기 중입니다. 잠시 후 다시 요청하세요.",
            );
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("60"));
            return response;
        }
        if completion.policy_blocked {
            return native_error(
                StatusCode::CONFLICT,
                provider,
                "routing_policy_blocked",
                "자동 계정 전환이 꺼져 있거나 이동 예산이 소진되었습니다. 계정 상태와 세션 정책을 확인해 주세요.",
            );
        }
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
        config.routing_replay.validate()?;
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
        observability.configure_move_notice(
            config
                .routing
                .move_guard
                .clone()
                .bounded()
                .reread_notice_tokens,
        );
        bindings.attach_move_accounting(&observability);
        let guard_observability = observability.clone();
        bindings.set_move_guard_observer(Arc::new(move |event| {
            guard_observability.record_move_guard(event)
        }));
        bindings.policies.attach_observability(&observability);
        quota.set_bindings(&bindings);
        observability.configure_forecast(config.routing.forecast_shadow);
        observability.dashboard().attach(&bindings, config);
        for (id, settings) in &config.accounts {
            bindings.set_account_threshold(id, settings.spread_threshold);
        }
        observability.configure_replay(store.home(), &config.routing_replay)?;
        Ok(Self {
            accounts,
            quota,
            bindings,
            launch_tokens: crate::launch_tokens::LaunchTokenRegistry::default(),
            catalogs: crate::catalog::CatalogCache::default(),
            catalog_slots: tokio::sync::Semaphore::new(4),
            catalog_changed: Notify::new(),
            rate_hold_secs: config.routing.rate_hold_secs,
            rate_hold_max_retries: config.routing.rate_hold_max_retries,
            ws_upgrade_hold_secs: config.routing.ws_upgrade_hold_secs,
            request_timeout_secs: config.routing.request_timeout_secs,
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
            images: Mutex::new(images::ImageState::default()),
            image_account_config: config.accounts.clone(),
            #[cfg(all(feature = "testkit", debug_assertions))]
            takeover_test_dependencies: None,
            websocket: config.websocket.clone(),
            ws_capacity: Arc::new(websocket::capacity::Capacity::default()),
            reconnect_exclusions: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "testkit")]
            http_quota_wait_events: tokio::sync::broadcast::channel(32).0,
            #[cfg(feature = "testkit")]
            http_quota_wait_inspections: tokio::sync::broadcast::channel(32).0,
            #[cfg(feature = "testkit")]
            http_quota_wait_result_hold: Mutex::new(None),
            #[cfg(feature = "testkit")]
            ws_quota_wait_events: tokio::sync::broadcast::channel(32).0,
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
        model: Option<&str>,
        tried: &BTreeSet<String>,
    ) -> Result<Vec<crate::bindings::Candidate>, ()> {
        self.candidates_with_evidence(provider, key, model, tried)
            .await
            .map(|(candidates, _, _)| candidates)
    }

    async fn candidates_with_evidence(
        &self,
        provider: Provider,
        key: Option<&str>,
        _model: Option<&str>,
        tried: &BTreeSet<String>,
    ) -> Result<
        (
            Vec<crate::bindings::Candidate>,
            Vec<crate::bindings::Candidate>,
            crate::bindings::MoveObservation,
        ),
        (),
    > {
        let observed = crate::bindings::MoveObservation::before_collection();
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
            let mut candidates = Vec::new();
            let mut evidence = Vec::new();
            use crate::observability::replay_recorder as tape;
            let recording = obs.replay();
            let collection = recording.as_ref().and_then(|r| r.next_id());
            let started = recording
                .as_ref()
                .and_then(|r| observed.lower_bound().and_then(|at| r.stamp_at(at)));
            if let (Some(r), Some(collection)) = (&recording, collection) {
                r.record(
                    tape::Owner::Collector,
                    None,
                    tape::Event::CollectionBoundary {
                        collection,
                        started,
                        rows: 0,
                        complete: false,
                    },
                );
            }
            for (rank, mut account) in accounts
                .into_iter()
                .filter(|a| a.provider == provider)
                .enumerate()
            {
                if account.quota_cooling.is_none()
                    && let Some(reset) = runtime.observed_quota_reset(&account)
                {
                    account.quota_cooling = Some(crate::accounts::QuotaCooling {
                        window: "observed".into(),
                        reset_at: reset,
                        started_at: chrono::Utc::now(),
                    });
                }
                let candidate = crate::bindings::Candidate {
                    quota: runtime.selection_quota(&account, obs.quota_snapshot(&account.id)),
                    // Every production attempt reserves the live table;
                    // a runtime count sampled before completion can be stale.
                    inflight: 0,
                    account,
                };
                let account = &candidate.account;
                // Preserve the original short-circuit order and credential reads.
                let retry_excluded = tried.contains(&account.id);
                let runtime_excluded = !retry_excluded
                    && (runtime.probing(&account.id) || runtime.blocked_in_memory(&account.id));
                let credential = if !retry_excluded && !runtime_excluded {
                    Some(
                        store
                            .tokens(account)
                            .is_ok_and(|t| !t.access_token.is_empty()),
                    )
                } else {
                    None
                };
                let fact_sequence =
                    obs.replay_account(account, u32::try_from(rank).ok(), credential);
                if let (Some(r), Some(collection), Some(started)) =
                    (&recording, collection, started)
                    && let Some(account) = r.identity(tape::IdentityDomain::Account, &account.id)
                {
                    r.record(
                        tape::Owner::Collector,
                        fact_sequence,
                        tape::Event::CollectionMember {
                            collection,
                            account,
                            side: tape::CollectionSide::SourceAndTarget,
                            started,
                            fact_revision: None,
                            order_rank: u32::try_from(rank).unwrap_or(u32::MAX),
                            exclusion: if retry_excluded {
                                tape::Exclusion::Tried
                            } else if runtime_excluded {
                                tape::Exclusion::Health
                            } else if credential == Some(false) {
                                tape::Exclusion::Unknown
                            } else {
                                tape::Exclusion::None
                            },
                        },
                    );
                }
                if credential == Some(true) {
                    obs.forecast_load(
                        &account.id,
                        bindings.active_sessions(&account.id, chrono::Utc::now()),
                    );
                    let pacing = runtime.pacing_snapshot(&account.id);
                    bindings.set_pacing(
                        &account.id,
                        pacing.rate_step > 0
                            || pacing.rate_backoff_until.is_some()
                            || pacing.waiting > 0,
                    );
                    candidates.push(candidate.clone());
                }
                // A retry exclusion is not a health fact. Preserve normalized
                // source evidence even when this account cannot be selected.
                evidence.push(candidate);
            }
            if let (Some(r), Some(collection)) = (&recording, collection) {
                r.record(
                    tape::Owner::Collector,
                    None,
                    tape::Event::CollectionBoundary {
                        collection,
                        started,
                        rows: u32::try_from(evidence.len()).unwrap_or(u32::MAX),
                        complete: true,
                    },
                );
            }
            Ok((candidates, evidence, observed))
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
        let table = self.bindings.clone();
        let owned_account = account.to_owned();
        let owned_key = key.map(str::to_owned);
        let (active, threshold, groups_hit, marked) = tokio::task::spawn_blocking(move || {
            table.note_client_rate(
                provider,
                &owned_account,
                owned_key.as_deref(),
                &candidates,
                now,
            )
        })
        .await
        .unwrap_or_default();
        self.observability
            .record_unknown_cost_skips(self.bindings.take_unknown_cost_skips());
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
        if key.is_none()
            && self.bindings.routing_plan(None).settings.mode == crate::routing::Mode::Strict
        {
            return Ok(None);
        }
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

    /// Recheck after every await that can outlive a policy/account edit. An
    /// admitted auth probe owns its slot, so do not reject its own probe flag.
    async fn dispatch_allowed(&self, key: Option<&str>, id: &str, model: Option<&str>) -> bool {
        self.dispatch_checked(key, id, model, false).await
    }
    async fn dispatch_data_allowed(
        &self,
        key: Option<&str>,
        id: &str,
        model: Option<&str>,
    ) -> bool {
        self.dispatch_checked(key, id, model, true).await
    }
    async fn dispatch_checked(
        &self,
        key: Option<&str>,
        id: &str,
        model: Option<&str>,
        data: bool,
    ) -> bool {
        if self.is_draining() || self.accounts.blocked_in_memory(id) {
            return false;
        }
        let store = self.store.clone();
        let obs = self.observability.clone();
        let bindings = self.bindings.clone();
        let owned_id = id.to_owned();
        let owned_model = model.map(str::to_owned);
        let owned_key = key.map(str::to_owned);
        let runtime = self.accounts.clone();
        let eligible = tokio::task::spawn_blocking(move || {
            if !store.takeover_fenced(&owned_id).is_ok_and(|fenced| !fenced) {
                return false;
            }
            let Ok(account) = store.load(&owned_id) else {
                return false;
            };
            if data && owned_model.is_none() && matches!(&account.health, Health::Cooling {until, scope, ..} if *until>chrono::Utc::now() && scope.starts_with("model:")) { return false; }
            if account.drain && !bindings.existing_placement(owned_key.as_deref(), &owned_id) {
                return false;
            }
            bindings.account_dispatch_eligible(
                &crate::bindings::Candidate {
                    quota: runtime.selection_quota(&account, obs.quota_snapshot(&owned_id)),
                    account,
                    inflight: 0,
                },
                owned_model.as_deref(),
            )
        })
        .await
        .unwrap_or(false);
        // Last synchronous check: policy publication is immutable and never
        // waits for the disk writer. No await separates it from the caller.
        eligible
            && !self.accounts.blocked_in_memory(id)
            && self.bindings.permits_dispatch(key, id)
            && key.is_none_or(|key| {
                self.bindings
                    .current(Some(key))
                    .is_none_or(|current| current == id)
            })
            && !self.is_draining()
    }

    async fn select_request(
        &self,
        provider: Provider,
        session: &session::Session,
        completion: &mut Completion,
        model: Option<&str>,
        tried: &BTreeSet<String>,
    ) -> Result<Option<Account>, ()> {
        self.select_request_admitted(provider, session, completion, model, tried, None)
            .await
    }

    async fn select_request_admitted(
        &self,
        provider: Provider,
        session: &session::Session,
        completion: &mut Completion,
        model: Option<&str>,
        tried: &BTreeSet<String>,
        admit: Option<websocket::capacity::Admission>,
    ) -> Result<Option<Account>, ()> {
        self.select_request_configured(provider, session, completion, model, tried, admit, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn select_request_configured(
        &self,
        provider: Provider,
        session: &session::Session,
        completion: &mut Completion,
        model: Option<&str>,
        tried: &BTreeSet<String>,
        admit: Option<websocket::capacity::Admission>,
        launch: Option<&http_data::ManagedHttp>,
    ) -> Result<Option<Account>, ()> {
        if session.key.is_none()
            && self.bindings.routing_plan(None).settings.mode == crate::routing::Mode::Strict
        {
            completion.policy_blocked = true;
            return Ok(None);
        }
        let deadline = completion
            .precommit_deadline
            .map(tokio::time::Instant::from_std)
            .unwrap_or_else(|| {
                tokio::time::Instant::now()
                    + Duration::from_secs(if completion.record.transport == "ws" {
                        self.ws_upgrade_hold_secs
                    } else {
                        self.request_timeout_secs
                    })
            });
        let operation = crate::issue_operation::IssueOperation::new(deadline);
        let _caller = operation.caller_guard();
        // Post-commit/nonreplayable quota errors cannot retry the old body.
        // On its next request, resolve C7 before publishing any context.
        let prior = self.bindings.current(session.key.as_deref());
        let mut proof_reset_source = None;
        if completion.record.transport == "http"
            && !completion.reset_hold_used
            && self.pins.get(provider, session.key.as_deref()).is_none()
            && let (Some(key), Some(source)) = (session.key.as_deref(), prior.as_deref())
        {
            completion.end_attempt();
            if let Some(waited) = quota_wait::wait_http_quota(
                self, provider, key, model, source, &operation, &_caller,
            )
            .await
            {
                completion.reset_hold_used = true;
                completion.reset_hold_timed_out = waited.end != quota_wait::WaitEnd::Reset;
                if self.is_draining() || waited.end == quota_wait::WaitEnd::WallResetPending {
                    return Ok(None);
                }
                if waited.end == quota_wait::WaitEnd::Reset
                    && self.http_wait_reset_source_current(key, &waited.source, model)
                {
                    proof_reset_source = Some(waited.source);
                }
            }
        }
        if self.is_draining() {
            return Ok(None);
        }
        let passive_sources = if prior.is_some() || session.key.is_none() {
            self.accounts.quota_observations(provider, prior.as_deref())
        } else {
            Vec::new()
        };
        let mut quota_sources = Vec::new();
        for ticket in self.accounts.pending_quota_tickets(
            crate::account_runtime::quota_session(session.key.as_deref()).as_deref(),
        ) {
            let store = self.store.clone();
            let pending_id = ticket.id.clone();
            let same_provider = tokio::task::spawn_blocking(move || {
                store
                    .load(&pending_id)
                    .is_ok_and(|a| a.provider == provider)
            })
            .await
            .unwrap_or(false);
            if same_provider
                && self.accounts.take_pending_quota(&ticket)
                && !self
                    .retry_after_reset(session, completion, &ticket.id, model)
                    .await?
            {
                quota_sources.push((ticket.id, ticket.model));
            }
        }
        // A failed pre-commit attempt is finished before reserving its successor.
        completion.end_attempt();
        let mut excluded = tried.clone();
        if let Some(source) = proof_reset_source {
            // A reset is only a hint for ordinary selection. Clear this one
            // local retry exclusion; current native gates still own dispatch.
            excluded.remove(&source);
            #[cfg(feature = "testkit")]
            self.observe_http_quota_wait("reselect", &source);
        }
        let mut held_account = None;
        loop {
            // Register before reading candidates or choosing: recovery/clear
            // between the decision and its wait must remain observable.
            let mut changes = selection_guard::Changes::new(
                &self.bindings.policies.changed,
                self.accounts.subscribe(),
            );
            let (candidates, mut move_evidence, target_observed) = self
                .candidates_with_evidence(provider, session.key.as_deref(), model, &excluded)
                .await?;
            // Retain this conservative target bound through any launch refresh;
            // a later source read cannot make an older destination snapshot fresh.
            let mut source_observed = target_observed;
            let (candidates, coverage) = if let Some(launch) = launch {
                let selected = self
                    .launch_candidates(launch, candidates, &excluded, deadline)
                    .await?;
                // Catalog I/O can outlive a recovery or rejection of an excluded
                // source. Refresh evidence without changing launch eligibility.
                let (_, refreshed_evidence, observed) = self
                    .candidates_with_evidence(provider, session.key.as_deref(), model, &excluded)
                    .await?;
                move_evidence = refreshed_evidence;
                source_observed = observed;
                selected
            } else {
                (candidates, None)
            };
            #[cfg(feature = "testkit")]
            if admit.is_some() {
                self.ws_capacity.before_selection().await;
            }
            let pinned = self.pins.get(provider, session.key.as_deref());
            let bytes = completion.selection_cost.unwrap_or(0);
            // Strict anchors/moves are durable before dispatch. Disk I/O must
            // not block a Tokio reactor thread while holding the table gate.
            let table = self.bindings.clone();
            let key = session.key.clone();
            let parent = session.parent.clone();
            let owned_model = model.map(str::to_owned);
            let choices = candidates.clone();
            let evidence = move_evidence.clone();
            let hold_allowed = !completion.reset_hold_used;
            let hold_timed_out = completion.reset_hold_timed_out;
            let admit = admit.clone();
            let denied = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let denied_choice = denied.clone();
            let admission_runtime = launch.map(|_| self.accounts.clone());
            let selection_operation = operation.clone();
            let choice = tokio::task::spawn_blocking(move || {
                let choose = |eligible: &dyn Fn(&str, bool) -> bool| {
                    table.choose_with_hold_at_admitted_evidence(
                        provider,
                        key.as_deref(),
                        parent.as_deref(),
                        owned_model.as_deref(),
                        &choices,
                        &evidence,
                        crate::bindings::MoveObservations {
                            source: source_observed,
                            target: target_observed,
                        },
                        pinned.as_deref(),
                        bytes,
                        hold_allowed,
                        hold_timed_out,
                        chrono::Utc::now(),
                        coverage.as_ref(),
                        |account| {
                            // BindingTable holds mutations through this
                            // admission and publication, including ordinary HTTP.
                            let allowed = selection_guard::admit(&selection_operation, || {
                                eligible(account, table.existing_placement(key.as_deref(), account))
                                    && admit.as_ref().is_none_or(|admit| admit(account))
                            });
                            denied_choice.store(!allowed, Ordering::Release);
                            allowed
                        },
                    )
                };
                if let Some(runtime) = admission_runtime {
                    runtime.with_selection_admission(
                        &table,
                        provider,
                        owned_model.as_deref(),
                        choose,
                    )
                } else {
                    Ok(choose(&|_, _| true))
                }
            })
            .await
            .map_err(|_| ())??;
            match choice {
                crate::bindings::Decision::None if denied.load(Ordering::Acquire) => {
                    return Ok(None);
                }
                crate::bindings::Decision::MoveHeld { until } => {
                    let current = self.bindings.current(session.key.as_deref());
                    let now = chrono::Utc::now();
                    let until = selection_guard::recovery_deadline(
                        until,
                        &move_evidence,
                        current.as_deref(),
                        now,
                    );
                    let wait = (until - now).to_std().unwrap_or_default();
                    let wake = tokio::time::Instant::now() + wait;
                    // A wake only retries the guarded choice; it never grants
                    // a move or renews this request's transport budget.
                    if changes
                        .wait(
                            current.as_deref(),
                            wake,
                            deadline,
                            self.wait_for_drain_start(),
                        )
                        .await
                    {
                        completion.record.outcome = "move_guard_hold".into();
                        return Ok(None);
                    }
                }
                crate::bindings::Decision::Blocked => {
                    completion.policy_blocked = true;
                    return Ok(None);
                }
                crate::bindings::Decision::None
                    if self
                        .bindings
                        .routing_plan(session.key.as_deref())
                        .settings
                        .mode
                        == crate::routing::Mode::Strict =>
                {
                    completion.policy_blocked = true;
                    return Ok(None);
                }
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
                        if event.is_placement {
                            self.observability.record_placement(&event);
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
                    let selected = candidates
                        .into_iter()
                        .find(|c| c.account.id == account)
                        .map(|c| c.account);
                    for (id, origin_model) in &quota_sources {
                        self.notice_quota(id, selected.as_ref(), origin_model.as_deref())
                            .await;
                    }
                    for (id, state, observed_model) in &passive_sources {
                        if selected.as_ref().is_none_or(|selected| &selected.id != id)
                            && !quota_sources.iter().any(|(pending, _)| pending == id)
                            && self.accounts.quota_observation_current(id, state)
                        {
                            self.notice_quota(
                                id,
                                selected.as_ref(),
                                model.or(observed_model.as_deref()),
                            )
                            .await;
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
                            && self.accounts.quota_observation_current(id, state)
                        {
                            self.notice_quota(id, None, model.or(observed_model.as_deref()))
                                .await;
                        }
                    }
                    return Ok(None);
                }
                crate::bindings::Decision::Hold { until } => {
                    held_account = self.bindings.current(session.key.as_deref());
                    let wait = (until - chrono::Utc::now()).to_std().unwrap_or_default();
                    completion.reset_hold_used = true;
                    if !reset_fits_transport(completion, wait) {
                        // A reset beyond this transport's remaining budget is
                        // not a viable hold. Re-select without waiting; strict
                        // still cannot choose another account.
                        continue;
                    }
                    // Convert wall time once; policy/account edits interrupt
                    // the monotonic wait. A later request has its own budget.
                    completion.reset_hold_timed_out = self
                        .wait_reset(session.key.as_deref(), held_account.as_deref(), wait)
                        .await;
                }
            }
        }
    }

    async fn wait_reset(&self, key: Option<&str>, id: Option<&str>, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        let mut events = self.accounts.subscribe();
        loop {
            let changed = self.bindings.policies.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let plan = self.bindings.routing_plan(key);
            if self.is_draining()
                || plan.settings.reset_wait_secs == 0
                || deadline.saturating_duration_since(tokio::time::Instant::now())
                    > Duration::from_secs(plan.settings.reset_wait_secs)
                || id.is_some_and(|id| !self.bindings.permits_dispatch(key, id))
            {
                return false;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return true,
                _ = &mut changed => return false,
                _ = self.drain_start_notify.notified() => return false,
                event = events.recv() => match event {
                    Ok(event) if id.is_some_and(|id| event["data"]["id"] == id) => return false,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                    Err(_) => return false,
                },
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
                scope,
                ..
            } if account.user_state == UserState::Active
                && (scope == "account"
                    || scope
                        .strip_prefix("model:")
                        .is_some_and(|m| model == Some(m))) =>
            {
                let Some(reset) = account
                    .quota_cooling
                    .as_ref()
                    .map(|q| q.reset_at)
                    .or_else(|| self.accounts.observed_quota_reset(&account))
                else {
                    tracing::debug!(account = %id, reason = "no_observed_reset_evidence", "reset hold skipped");
                    return Ok(false);
                };
                reset
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
        let reset_budget = self
            .bindings
            .routing_plan(session.key.as_deref())
            .settings
            .reset_wait_secs;
        if wait > Duration::from_secs(reset_budget) {
            tracing::debug!(account = %id, reason = "reset_beyond_hold_bound", wait_ms = wait.as_millis() as u64, "reset hold skipped");
            return Ok(false);
        }
        if !reset_fits_transport(completion, wait) {
            completion.reset_hold_used = true;
            tracing::debug!(account = %id, reason = "reset_beyond_transport_budget", "reset hold skipped");
            return Ok(false);
        }
        completion.end_attempt();
        completion.reset_hold_used = true;
        completion.failover("reset_hold_retry");
        if !self
            .wait_reset(session.key.as_deref(), Some(id), wait)
            .await
        {
            tracing::debug!(account = %id, reason = "reset_hold_interrupted", "reset hold skipped");
            return Ok(false);
        }
        let store = self.store.clone();
        let account_id = id.to_owned();
        let account = tokio::task::spawn_blocking(move || store.load(&account_id))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        if !account.eligible(model)
            || !self
                .dispatch_allowed(session.key.as_deref(), id, model)
                .await
        {
            completion.reset_hold_timed_out = true;
            tracing::debug!(account = %id, reason = "account_still_ineligible_after_reset", "reset hold skipped");
            return Ok(false);
        }
        completion.binding = Some(
            self.bindings
                .begin_request_async(session.key.as_deref(), id)
                .await
                .map_err(|_| ())?,
        );
        Ok(true)
    }
}

fn reset_fits_transport(completion: &Completion, wait: Duration) -> bool {
    completion.record.transport != "ws"
        || completion
            .precommit_deadline
            .is_none_or(|deadline| wait < deadline.saturating_duration_since(Instant::now()))
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
    if let Some(project) = parts
        .extensions
        .get::<crate::routing::ProjectHint>()
        .and_then(|hint| hint.0.as_deref())
        && let Ok(value) = HeaderValue::from_str(project)
    {
        headers.insert(crate::routing::PROJECT_HEADER, value);
    }
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

#[derive(Clone, Copy)]
struct RequestDeadline(Instant);
fn routing_input_cost(headers: &HeaderMap, prefix: &[u8], provider: Provider) -> Option<u64> {
    if provider == Provider::Codex
        && wire_metadata::Metadata::parse(prefix).is_none_or(|meta| meta.continuation())
    {
        return None;
    }
    if headers
        .get("content-encoding")
        .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return None;
    }
    headers.get("content-length")?.to_str().ok()?.parse().ok()
}
#[derive(Clone)]
struct CommitSignal(tokio::sync::watch::Sender<bool>);
async fn precommit_timeout<T>(
    duration: Duration,
    mut committed: tokio::sync::watch::Receiver<bool>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::pin!(work);
    let deadline = tokio::time::Instant::now() + duration;
    let mut closed = false;
    loop {
        tokio::select! {
            biased;
            result = &mut work => return Some(result),
            _ = tokio::time::sleep_until(deadline) => {
                return if *committed.borrow() { Some(work.await) } else { None };
            }
            result = committed.changed(), if !closed => {
                if result.is_err() { closed = true; }
                else if *committed.borrow() { return Some(work.await); }
            }
        }
    }
}
async fn relay(State(state): State<Arc<RelayState>>, mut request: Request) -> Response {
    // Strip routing handles before deadlines, ledger, capture, upstream URLs or
    // error construction. Authentication has already run on the local port.
    let normalized = match crate::launch_path::normalize(request.uri()) {
        Ok(path) => path,
        Err(()) => {
            return native_error(
                StatusCode::BAD_REQUEST,
                Provider::Codex,
                "invalid_launch",
                "실행 경로가 올바르지 않습니다. codex-qb를 다시 실행해 주세요.",
            );
        }
    };
    *request.uri_mut() = normalized.uri;
    // Scoped endpoints must resolve local binding authority before forwarding.
    if normalized.token.is_some()
        && !(request.method() == Method::POST
            && matches!(
                request.uri().path(),
                "/v1/responses" | "/v1/responses/compact" | "/v1/alpha/search"
            )
            || request.method() == Method::GET
                && matches!(request.uri().path(), "/v1/models" | "/v1/responses")
                && !request.headers().contains_key("anthropic-version"))
    {
        return native_error(
            StatusCode::SERVICE_UNAVAILABLE,
            Provider::Codex,
            "launch_unavailable",
            "계정 지정 실행 기능을 준비 중입니다. 일반 codex-qb를 사용해 주세요.",
        );
    }
    let launch_pin = normalized.token.and_then(|token| {
        let pin = state.launch_tokens.get_or_pin(&token).ok().map(Arc::new);
        request
            .extensions_mut()
            .insert(launch::LaunchIngress { pin: pin.clone() });
        pin
    });
    let image_edit = images::matches(request.method(), request.uri().path())
        .then(|| request.uri().path().ends_with("/edits"));
    let provider = if request.uri().path().starts_with("/v1/messages") {
        Provider::Claude
    } else {
        Provider::Codex
    };
    let ws = request.method() == Method::GET
        && request.uri().path() == "/v1/responses"
        && request
            .headers()
            .get("upgrade")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let timeout = Duration::from_secs(if ws {
        state.ws_upgrade_hold_secs
    } else {
        state.request_timeout_secs
    });
    request
        .extensions_mut()
        .insert(RequestDeadline(Instant::now() + timeout));
    let (commit, committed) = tokio::sync::watch::channel(false);
    request.extensions_mut().insert(CommitSignal(commit));
    let response = precommit_timeout(timeout, committed, relay_inner(state, request))
        .await
        .unwrap_or_else(|| {
            if let Some(edit) = image_edit {
                return images::timeout(edit);
            }
            native_error(
                StatusCode::GATEWAY_TIMEOUT,
                provider,
                "request_timeout",
                "계정 선택·대기·재시도의 전체 시간 한도를 초과했습니다.",
            )
        });
    launch::pin_body(response, launch_pin)
}

async fn relay_inner(state: Arc<RelayState>, mut request: Request) -> Response {
    let project = match crate::routing::take_project(request.headers_mut()) {
        Ok(project) => project,
        Err(_) => {
            return native_error(
                StatusCode::BAD_REQUEST,
                if request.uri().path().contains("responses") {
                    Provider::Codex
                } else {
                    Provider::Claude
                },
                "invalid_request_error",
                "프로젝트 식별자가 올바르지 않습니다.",
            );
        }
    };
    request
        .extensions_mut()
        .insert(crate::routing::ProjectHint(project));
    let launcher_label = crate::dashboard::labels::take_launcher_label(request.headers_mut());
    request
        .extensions_mut()
        .insert(crate::dashboard::labels::IngressLabel(
            launcher_label.clone(),
        ));
    if state.is_draining() {
        if request
            .extensions()
            .get::<launch::LaunchIngress>()
            .is_some()
        {
            return native_error(
                StatusCode::SERVICE_UNAVAILABLE,
                Provider::Codex,
                "launch_handover",
                "코어를 교체하고 있습니다. 잠시 후 다시 요청해 주세요.",
            );
        }
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
    if images::matches(request.method(), request.uri().path()) {
        return images::relay(state, request).await;
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
    completion.precommit_deadline = parts.extensions.get::<RequestDeadline>().map(|d| d.0);
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
    if provider == Provider::Codex
        && parts.method == Method::GET
        && path == "/v1/models"
        && let Some(ingress) = parts.extensions.get::<launch::LaunchIngress>()
    {
        return models::serve(state, &parts.headers, ingress, &mut completion).await;
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
    let mut session = session::classify(provider, path, &parts.headers, &session_body);
    let data_deadline = tokio::time::Instant::from_std(
        completion
            .precommit_deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(state.request_timeout_secs)),
    );
    let managed_http = if provider == Provider::Codex
        && parts.method == Method::POST
        && matches!(
            path,
            "/v1/responses" | "/v1/responses/compact" | "/v1/alpha/search"
        ) {
        match state
            .prepare_http_launch(
                parts.extensions.get::<launch::LaunchIngress>(),
                &mut session,
                &parts.headers,
                &upload.prefix,
                path == "/v1/alpha/search",
                data_deadline,
            )
            .await
        {
            Ok(managed) => managed,
            Err(error) => return http_data::error(&mut completion, error),
        }
    } else {
        None
    };
    // input_bytes may be only the first streaming chunk, not full context.
    completion.selection_cost = routing_input_cost(&parts.headers, &upload.prefix, provider);
    if let Some(bytes) = completion.selection_cost {
        state.bindings.request_bytes(session.key.as_deref(), bytes);
    } else {
        state.bindings.unknown_request_cost(session.key.as_deref());
    }
    if state
        .prepare_project(
            &session,
            parts.extensions.get::<crate::routing::ProjectHint>(),
        )
        .await
        .is_err()
    {
        return native_error(
            StatusCode::CONFLICT,
            provider,
            "routing_project_conflict",
            "등록된 프로젝트와 루트 세션 연결을 확인해 주세요.",
        );
    }
    completion.dashboard_label = launcher_label.or(completion.dashboard_label.take());
    completion.session(&session);
    completion.accounting_http_model(&parts.headers, &upload.prefix);
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
    // HTTP initial full-proof fallback is distinct from a physical provider
    // rejection and from G2's admitted cache remainder. No upstream was sent.
    #[cfg(all(feature = "phase0", debug_assertions))]
    let allow_initial_fallback = parts.extensions.get::<ForcedAccount>().is_none();
    #[cfg(not(all(feature = "phase0", debug_assertions)))]
    let allow_initial_fallback = true;
    if allow_initial_fallback
        && let Some(response) = state
            .initial_http_far_wait(provider, &session, request_model.as_deref(), data_deadline)
            .await
    {
        completion.outcome(response.status().as_u16(), "quota_wait_fallback");
        return response;
    }
    #[cfg(all(feature = "phase0", debug_assertions))]
    let selection = if managed_http.is_some() {
        state
            .select_http_request(
                provider,
                &session,
                &mut completion,
                request_model.as_deref(),
                &tried,
                managed_http.as_ref(),
            )
            .await
    } else if let Some(ForcedAccount(id)) = parts.extensions.get::<ForcedAccount>() {
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
            .select_http_request(
                provider,
                &session,
                &mut completion,
                request_model.as_deref(),
                &tried,
                managed_http.as_ref(),
            )
            .await
    };
    #[cfg(not(all(feature = "phase0", debug_assertions)))]
    let selection = state
        .select_http_request(
            provider,
            &session,
            &mut completion,
            request_model.as_deref(),
            &tried,
            managed_http.as_ref(),
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
    if completion.selection_cost.is_none() {
        state.bindings.unknown_request_cost(session.key.as_deref());
    }
    // Phase-0 forced replay bypasses affinity, but still participates in the
    // authoritative placement reservation count.
    if completion.binding.is_none() {
        completion.binding = match state.bindings.begin_request_async(None, &account.id).await {
            Ok(lease) => Some(lease),
            Err(_) => {
                return failure(
                    &mut completion,
                    StatusCode::SERVICE_UNAVAILABLE,
                    provider,
                    "routing_policy_blocked",
                    "요청 예약을 확정하지 못했습니다.",
                );
            }
        };
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
    let mut rate_retry_queued = false;
    let mut rate_hold_deadline: Option<Instant> = None;
    let mut rate_relay_retry_after = None;
    let mut rate_relayed = false;
    let mut permission_denials = 0;
    let mut probe = state.claim_probe(&mut account).await;
    let (status, response_headers, held) = loop {
        if !account.eligible(request_model.as_deref()) {
            let next = state
                .select_http_request(
                    provider,
                    &session,
                    &mut completion,
                    request_model.as_deref(),
                    &tried,
                    managed_http.as_ref(),
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
        completion.dashboard_begin();
        let dashboard_pacing = completion.dashboard_pacing();
        let admission_started = Instant::now();
        let admission_deadline = rate_hold_deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(state.rate_hold_secs));
        let admission = if rate_hold_deadline.is_some_and(|at| Instant::now() >= at) {
            None
        } else {
            Some(
                state
                    .accounts
                    .admit_until(&account.id, admission_deadline)
                    .await,
            )
        };
        if admission.as_ref().is_some_and(|a| a.waited()) {
            rate_hold_deadline.get_or_insert(admission_deadline);
        }
        drop(dashboard_pacing);
        completion.rate_wait(admission_started.elapsed().as_millis() as u64);
        let mut forced_send = admission.as_ref().is_none_or(|a| a.forced())
            || rate_hold_deadline.is_some_and(|at| Instant::now() >= at);
        if let Some(managed) = &managed_http
            && let Err(error) = state
                .guard_http_launch(managed, &account.id, data_deadline)
                .await
        {
            return http_data::error(&mut completion, error);
        }
        let wait_origin = state.accounts.capture_http_origin(&account.id).await;
        if !state
            .dispatch_allowed(
                session.key.as_deref(),
                &account.id,
                request_model.as_deref(),
            )
            .await
        {
            completion.policy_blocked = !state
                .bindings
                .permits_dispatch(session.key.as_deref(), &account.id);
            let response = state
                .unavailable(&mut completion, provider, false, request_model.as_deref())
                .await;
            completion.outcome(response.status().as_u16(), "dispatch_revoked");
            return response;
        }
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
        if completion
            .precommit_deadline
            .is_some_and(|at| Instant::now() >= at)
        {
            return failure(
                &mut completion,
                StatusCode::GATEWAY_TIMEOUT,
                provider,
                "request_timeout",
                "전송 전 전체 시간 한도를 초과했습니다.",
            );
        }
        forced_send |= rate_hold_deadline.is_some_and(|at| Instant::now() >= at);
        if forced_send {
            state.accounts.record_forced_send(&account.id);
        }
        if std::mem::take(&mut rate_retry_queued) {
            completion.rate_retry();
        }
        if let Some(binding) = &completion.binding {
            binding.dispatched();
        }
        completion.attempt(&account.id);
        let observation_asof = state.bindings.begin_usage_observation_for(&account.id);
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
        completion.dashboard_model(
            response_headers
                .get("openai-model")
                .and_then(|v| v.to_str().ok()),
        );
        let (credit_observation, pending_http_429) = if status == StatusCode::TOO_MANY_REQUESTS {
            state
                .accounts
                .observe_http_429_headers_pending(
                    &account.id,
                    provider,
                    &response_headers,
                    request_model.as_deref(),
                    (&state.bindings, observation_asof),
                    wait_origin,
                )
                .await
        } else if status.is_success() {
            (
                state
                    .accounts
                    .observe_headers_asof_origin(
                        &account.id,
                        provider,
                        &response_headers,
                        request_model.as_deref(),
                        (&state.bindings, observation_asof),
                        wait_origin,
                    )
                    .await,
                None,
            )
        } else {
            (Default::default(), None)
        };
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
        if !before_commit && let Some(commit) = parts.extensions.get::<CommitSignal>() {
            commit.0.send_replace(true);
        }
        if state.is_draining() {
            // Once admission closes the old owner only finishes byte relay.
            // In particular a delayed 401 cannot refresh or change health.
            break (status, response_headers, held);
        }
        if !before_commit {
            if status.is_success() {
                state
                    .accounts
                    .credit_headers(&account.id, &response_headers, &credit_observation)
                    .await;
            }
            if probe.is_some()
                && status.is_success()
                && state.accounts.probe_success(&account.id).await.is_err()
            {
                let mut blocked = account.clone();
                blocked.health = crate::accounts::Health::Cooling {
                    reason: crate::accounts::HealthReason::StoreError,
                    until: chrono::Utc::now() + chrono::Duration::seconds(60),
                    scope: "account".into(),
                };
                state.accounts.emit_account(&blocked);
                state
                    .observability
                    .record_health(&account.id, &account.health, &blocked.health);
                state
                    .observability
                    .core_warning(&account.id, "probe_store_failed_after_commit");
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
        decision.reconcile_quota(&credit_observation);
        completion.record.upstream_error_code = hold::error_code(&response_headers, &held.prefix);
        let mut observed_http_429 = None;
        if decision.kind == outcome::Kind::Quota {
            if status == StatusCode::TOO_MANY_REQUESTS {
                // The bounded side copy supplies optional wait/fallback evidence.
                // Native classification and downstream bytes remain unchanged.
                let decoded_body = (held.complete && held.terminal.is_none())
                    .then(|| hold::decoded_side(&response_headers, &held.prefix))
                    .flatten();
                let body = crate::account_runtime::AccountRuntime::qualify_http_429_body(
                    decoded_body.as_deref(),
                    request_model.as_deref(),
                );
                match state
                    .accounts
                    .finish_http_429_report_observed(
                        &account.id,
                        pending_http_429,
                        decision.quota_reset,
                        decision.signal.take(),
                        body,
                    )
                    .await
                {
                    Ok(observed) => observed_http_429 = observed,
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
            } else {
                state.accounts.invalidate_quota_rejection(&account.id).await;
            }
        }
        if let Some(model) = decision.model.take().filter(|_| managed_http.is_none()) {
            request_model = Some(model);
        }
        if managed_http.is_some() && decision.kind == outcome::Kind::ModelPermission {
            // A failed or unchanged catalog refresh must not erase C6 row 6a's
            // observed model-specific denial. Keep its normal durable hold.
            if let Some(signal) = decision.signal.take()
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
            // New entitlement evidence belongs to this account. Refresh its menu
            // without replaying the rejected model on a different account.
            let _ = tokio::time::timeout_at(
                data_deadline,
                state.launch_catalog(&account.id, true, true),
            )
            .await;
            return http_data::error(&mut completion, launch::DataLaunchError::UnsupportedModel);
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
            let rate_deadline = *rate_hold_deadline
                .get_or_insert_with(|| Instant::now() + Duration::from_secs(state.rate_hold_secs));
            state.bindings.note_rate(&account.id, chrono::Utc::now());
            let retry_after = outcome::header_seconds(&response_headers, "retry-after")
                .map(|seconds| Duration::from_secs(seconds as u64));
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
                && !forced_send
                && Instant::now() < rate_deadline
                && let Some(body) = copy.retry_body().await
            {
                rate_retries += 1;
                rate_retry_queued = true;
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
            state
                .accounts
                .observe_quota_reset(&account.id, decision.quota_reset);
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
            state
                .accounts
                .pending_quota(
                    &account.id,
                    completion.record.session.clone(),
                    request_model.as_deref(),
                )
                .await
        } else {
            None
        };
        // M4b는 select만 교체한다. 보관본 없는 요청과 commit 이후에는 재전송하지 않는다.
        if !decision.failover() || retained.is_none() || permission_denials >= 2 {
            break (status, response_headers, held);
        }
        if let Some(copy) = retained.as_ref()
            && copy.complete().await.is_none()
        {
            break (status, response_headers, held);
        }
        // D26 physical-response fallback only: a genuinely accepted 5h-only
        // report plus a current bound effective Wait whose reset cannot fit.
        // A mere optional-wait refusal never supplies this provenance.
        if !forced_send
            && decision.kind == outcome::Kind::Quota
            && let Some(observed) = observed_http_429.take()
            && let Some(key) = session.key.as_deref()
            && let Some(deadline) = completion.precommit_deadline
        {
            let operation = crate::issue_operation::IssueOperation::new(
                tokio::time::Instant::from_std(deadline),
            );
            let _caller = operation.caller_guard();
            let accounts = state.accounts.clone();
            let bindings = state.bindings.clone();
            let key = key.to_owned();
            let model = request_model.clone();
            let source = account.id.clone();
            let work_operation = operation.clone();
            let task = tokio::task::spawn_blocking(move || {
                accounts.observed_http_far_wait(
                    &bindings,
                    crate::bindings::HttpWaitRequest {
                        provider,
                        key: Some(&key),
                        model: model.as_deref(),
                        expected_source: Some(&source),
                        operation: &work_operation,
                    },
                    &observed,
                )
            });
            let preserve = tokio::select! {
                biased;
                _ = state.wait_for_drain_start() => false,
                _ = tokio::time::sleep_until(operation.deadline()) => false,
                result = task => matches!(result, Ok(Ok(true))),
            };
            // Caller Drop cancels detached inspection on route cancellation.
            // Native downstream delivery still owns the untouched held response.
            if state.is_draining() || (preserve && operation.can_reply()) {
                break (status, response_headers, held);
            }
        }
        let mut proof_reset_source = None;
        if decision.kind == outcome::Kind::Quota {
            if let Some(ticket) = &quota_pending {
                state.accounts.take_pending_quota(ticket);
            }
            drop(probe.take());
            let mut proof_wait = None;
            if !forced_send
                && !completion.reset_hold_used
                && let Some(key) = session.key.as_deref()
                && let Some(deadline) = completion.precommit_deadline
            {
                let operation = crate::issue_operation::IssueOperation::new(
                    tokio::time::Instant::from_std(deadline),
                );
                let caller = operation.caller_guard();
                completion.end_attempt();
                proof_wait = quota_wait::wait_http_quota(
                    &state,
                    provider,
                    key,
                    request_model.as_deref(),
                    &account.id,
                    &operation,
                    &caller,
                )
                .await;
            }
            if state.is_draining() {
                break (status, response_headers, held);
            }
            if let Some(waited) = proof_wait {
                completion.reset_hold_used = true;
                completion.reset_hold_timed_out = waited.end != quota_wait::WaitEnd::Reset;
                if waited.end == quota_wait::WaitEnd::WallResetPending {
                    break (status, response_headers, held);
                }
                if waited.end == quota_wait::WaitEnd::Reset
                    && session.key.as_deref().is_some_and(|key| {
                        state.http_wait_reset_source_current(
                            key,
                            &waited.source,
                            request_model.as_deref(),
                        )
                    })
                {
                    proof_reset_source = Some(waited.source);
                }
            } else {
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
        }
        if tried.len() >= 4 && proof_reset_source.is_none() {
            if decision.kind == outcome::Kind::Quota {
                state
                    .notice_quota(&account.id, None, request_model.as_deref())
                    .await;
            }
            if managed_http.is_none()
                && state
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
        let mut selection_tried = tried.clone();
        if let Some(source) = proof_reset_source {
            selection_tried.remove(&source);
            #[cfg(feature = "testkit")]
            state.observe_http_quota_wait("reselect", &source);
        }
        let next = state
            .select_http_request(
                provider,
                &session,
                &mut completion,
                request_model.as_deref(),
                &selection_tried,
                managed_http.as_ref(),
            )
            .await;
        if decision.kind == outcome::Kind::Quota
            && let Ok(selected) = &next
        {
            state
                .notice_quota(&account.id, selected.as_ref(), request_model.as_deref())
                .await;
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
            Ok(None) if completion.record.outcome == "move_guard_hold" => {
                let response = state
                    .unavailable(&mut completion, provider, false, request_model.as_deref())
                    .await;
                completion.outcome(response.status().as_u16(), "move_guard_hold");
                return response;
            }
            Ok(None) if completion.policy_blocked => {
                let response = state
                    .unavailable(&mut completion, provider, false, request_model.as_deref())
                    .await;
                completion.outcome(response.status().as_u16(), "routing_policy_blocked");
                return response;
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
    #[tokio::test]
    async fn precommit_timeout_never_replaces_a_committed_response_during_cleanup() {
        let (commit, observed) = tokio::sync::watch::channel(false);
        let (release, wait) = tokio::sync::oneshot::channel();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let ready = entered.clone();
        let task = tokio::spawn(super::precommit_timeout(
            std::time::Duration::from_millis(30),
            observed,
            async move {
                commit.send_replace(true);
                ready.notify_one();
                wait.await.unwrap();
                "original-200-stream"
            },
        ));
        entered.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !task.is_finished(),
            "commit must disable the precommit deadline, not return504"
        );
        release.send(()).unwrap();
        assert_eq!(task.await.unwrap(), Some("original-200-stream"));
        let (_sender, observed) = tokio::sync::watch::channel(false);
        assert_eq!(
            super::precommit_timeout(
                std::time::Duration::from_millis(10),
                observed,
                std::future::pending::<()>()
            )
            .await,
            None
        );
    }

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
    async fn tray2_three_accounts_reconnect_exclusions_name_actual_upstream_or_suppress_false_exhaustion()
     {
        use qb_testkit::{ScriptedResponse, ScriptedUpstream};
        use serde_json::{Value, json};
        for exclude_all in [false, true] {
            let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/r1-tray2-fixtures");
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
            let state = Arc::new(
                RelayState::new(store.clone(), &config, obs, Arc::new(DebugPins::default()))
                    .unwrap(),
            );
            let servers = crate::http::spawn_with_routes(
                &config,
                crate::local_secrets::LocalSecrets {
                    client_key: "synthetic-tray2-key".into(),
                    admin_token: "synthetic-tray2-admin".into(),
                },
                router(state.clone()),
                axum::Router::new(),
            )
            .await
            .unwrap();
            let client = reqwest::Client::new();
            let request = || {
                client
                    .post(format!("http://{}/v1/responses", servers.data_addr))
                    .bearer_auth("synthetic-tray2-key")
                    .header("session-id", "tray2-exclusions")
                    .body("{}")
            };
            fake.state
                .push(ScriptedResponse::json(StatusCode::OK, "{}"));
            let warm = request().send().await.unwrap();
            let source = warm.headers()["x-quota-baton-account"]
                .to_str()
                .unwrap()
                .to_owned();
            warm.bytes().await.unwrap();
            let others: Vec<_> = store
                .list()
                .unwrap()
                .into_iter()
                .filter(|a| a.id != source)
                .collect();
            state.exclude_reconnect(Some("codex:tray2-exclusions"), &others[0].id);
            if exclude_all {
                state.exclude_reconnect(Some("codex:tray2-exclusions"), &others[1].id);
            }
            let reset = chrono::Utc::now().timestamp() + 172800;
            fake.state.push(ScriptedResponse::json(
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error":{"type":"usage_limit_reached","resets_at":reset}}).to_string(),
            ));
            fake.state
                .push(ScriptedResponse::json(StatusCode::OK, "{}"));
            let mut events = state.accounts.subscribe();
            let response = request().send().await.unwrap();
            let status = response.status();
            let served = response
                .headers()
                .get("x-quota-baton-account")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            response.bytes().await.unwrap();
            let mut notices: Vec<Value> = Vec::new();
            while let Ok(event) = events.try_recv() {
                if event["event"] == "core_warning" && event["data"]["kind"] == "quota_notice" {
                    notices.push(event["data"].clone());
                }
            }
            if exclude_all {
                assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
                assert!(
                    notices.is_empty(),
                    "request exclusions are not provider exhaustion"
                );
                assert!(
                    state
                        .accounts
                        .views()
                        .await
                        .unwrap()
                        .iter()
                        .all(|v| v["quota_notice"].is_null())
                );
            } else {
                assert_eq!(status, StatusCode::OK);
                assert_eq!(served.as_deref(), Some(others[1].id.as_str()));
                let notice = notices.iter().find(|n| n["action"] == "failover").unwrap();
                assert_eq!(notice["account"], source);
                assert_eq!(notice["next_account"], served.unwrap());
                let token = store.tokens(&others[1]).unwrap();
                assert_eq!(
                    fake.state.requests().last().unwrap().headers["authorization"],
                    format!("Bearer {}", token.access_token)
                );
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

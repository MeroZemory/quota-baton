use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration as StdDuration, Instant},
};

use axum::{
    Json,
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use crate::{
    accounts::{Account, AccountStore, Health, HealthReason, Provider, UserState},
    observability::Observability,
};

#[derive(Clone, Debug)]
pub enum HealthSignal {
    Cooling {
        reason: HealthReason,
        until: DateTime<Utc>,
        scope: String,
    },
    Revoked(String),
    AuthFailure,
    Ready,
}

#[derive(Clone)]
struct LimitResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

pub struct AccountRuntime {
    pub store: Arc<AccountStore>,
    obs: Arc<Observability>,
    events: broadcast::Sender<Value>,
    limits: Mutex<BTreeMap<String, LimitResponse>>,
    probes: Mutex<BTreeSet<String>>,
    volatile: Mutex<BTreeMap<String, Health>>,
    inflight: Mutex<BTreeMap<String, usize>>,
    poller: Mutex<Option<Weak<crate::quota::UsagePoller>>>,
    refresh_attention: Mutex<BTreeSet<String>>,
    pacing: Mutex<BTreeMap<String, PacingState>>,
    rate_hold_secs: AtomicU64,
}

const MIN_RELEASE: StdDuration = StdDuration::from_millis(250);
const MAX_RELEASE: StdDuration = StdDuration::from_secs(5);
const DEFAULT_RATE_HOLD_SECS: u64 = 60;

struct PacingWaiter {
    deadline: Instant,
    forced: std::sync::atomic::AtomicBool,
    notify: Notify,
    cancelled: std::sync::atomic::AtomicBool,
}

struct PacingState {
    backoff_until: Option<Instant>,
    window_set_at: Option<Instant>,
    window_had_429: bool,
    step_raised: bool,
    step: u8,
    release_interval: StdDuration,
    last_release: Option<Instant>,
    last_429: Option<Instant>,
    step_decay_at: Option<Instant>,
    step_decay_interval: StdDuration,
    release_decay_at: Option<Instant>,
    waiting: VecDeque<Arc<PacingWaiter>>,
    release_task: bool,
    wake: Arc<Notify>,
}

impl Default for PacingState {
    fn default() -> Self {
        Self {
            backoff_until: None,
            window_set_at: None,
            window_had_429: false,
            step_raised: false,
            step: 0,
            release_interval: MIN_RELEASE,
            last_release: None,
            last_429: None,
            step_decay_at: None,
            step_decay_interval: StdDuration::from_secs(1),
            release_decay_at: None,
            waiting: VecDeque::new(),
            release_task: false,
            wake: Arc::new(Notify::new()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacingSnapshot {
    pub rate_backoff_until: Option<Instant>,
    pub rate_step: u8,
    pub rate_release_interval: StdDuration,
    pub waiting: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Released,
    Forced,
}

pub struct AdmissionGuard {
    admission: Admission,
    runtime: Weak<AccountRuntime>,
    id: String,
}

struct AdmissionCancellation {
    runtime: Weak<AccountRuntime>,
    id: String,
    waiter: Arc<PacingWaiter>,
}

impl Drop for AdmissionCancellation {
    fn drop(&mut self) {
        self.waiter.cancelled.store(true, Ordering::Release);
        if let Some(runtime) = self.runtime.upgrade()
            && let Ok(states) = runtime.pacing.lock()
            && let Some(state) = states.get(&self.id)
        {
            state.wake.notify_one();
        }
    }
}

impl AdmissionGuard {
    pub fn forced(&self) -> bool {
        self.admission == Admission::Forced
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        let _ = self.runtime.upgrade();
        let _ = &self.id;
    }
}

pub struct InflightGuard {
    runtime: Weak<AccountRuntime>,
    id: String,
}
impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade()
            && let Ok(mut counts) = runtime.inflight.lock()
            && let Some(count) = counts.get_mut(&self.id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.id);
            }
        }
    }
}

pub struct Probe {
    runtime: Weak<AccountRuntime>,
    id: String,
}
#[derive(Debug, thiserror::Error)]
#[error("account authentication probe is already running")]
pub struct ProbeBusy;
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade()
            && let Ok(mut probes) = runtime.probes.lock()
        {
            probes.remove(&self.id);
        }
    }
}

impl AccountRuntime {
    pub fn new(store: Arc<AccountStore>, obs: Arc<Observability>, _allowance: f64) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        obs.set_events(events.clone());
        let writer = obs.clone();
        let changes = events.clone();
        store.set_observer(Arc::new(move |account,previous| {
            if previous != &account.health {writer.record_health(&account.id,previous,&account.health);}
            let _=changes.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state}}));
        }));
        Arc::new(Self {
            store,
            obs,
            events,
            limits: Mutex::new(BTreeMap::new()),
            probes: Mutex::new(BTreeSet::new()),
            volatile: Mutex::new(BTreeMap::new()),
            inflight: Mutex::new(BTreeMap::new()),
            poller: Mutex::new(None),
            refresh_attention: Mutex::new(BTreeSet::new()),
            pacing: Mutex::new(BTreeMap::new()),
            rate_hold_secs: AtomicU64::new(DEFAULT_RATE_HOLD_SECS),
        })
    }

    pub fn set_rate_hold_secs(&self, secs: u64) {
        self.rate_hold_secs.store(secs.max(1), Ordering::Relaxed);
    }

    fn pacing_state<'a>(
        states: &'a mut BTreeMap<String, PacingState>,
        id: &str,
    ) -> &'a mut PacingState {
        states.entry(id.to_owned()).or_default()
    }

    pub fn pacing_snapshot(&self, id: &str) -> PacingSnapshot {
        self.pacing_snapshot_at(id, Instant::now())
    }

    pub fn snapshot(&self, id: &str) -> PacingSnapshot {
        self.pacing_snapshot(id)
    }

    /// Test-clock-friendly single-step admission check. The async `admit`
    /// method uses the same gate and additionally queues blocked callers.
    pub fn admit_at(&self, id: &str, now: Instant) -> bool {
        let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
        let state = Self::pacing_state(&mut states, id);
        Self::decay_pacing(state, now);
        let pacing = state.step > 0 || state.backoff_until.is_some();
        let open = state.waiting.is_empty()
            && (!pacing
                || (state.backoff_until.is_none_or(|until| now >= until)
                    && state.last_release.is_none_or(|last| {
                        now.saturating_duration_since(last) >= state.release_interval
                    })));
        if open && pacing {
            state.last_release = Some(now);
        }
        open
    }

    pub fn pacing_snapshot_at(&self, id: &str, now: Instant) -> PacingSnapshot {
        let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
        let state = Self::pacing_state(&mut states, id);
        Self::decay_pacing(state, now);
        PacingSnapshot {
            rate_backoff_until: state.backoff_until,
            rate_step: state.step,
            rate_release_interval: state.release_interval,
            waiting: state.waiting.len(),
        }
    }

    fn decay_pacing(state: &mut PacingState, now: Instant) {
        if state.backoff_until.is_some_and(|until| until <= now) {
            state.backoff_until = None;
        }
        if state.last_429.is_none() {
            return;
        }
        // A window containing a 429 is not quiet. Each following full quiet
        // window decays one step; successful responses do not reset this clock.
        while state.step > 0 && state.step_decay_at.is_some_and(|at| at <= now) {
            state.step -= 1;
            state.step_decay_at = state.step_decay_at.map(|at| at + state.step_decay_interval);
        }
        if state.step == 0 {
            state.step_decay_at = None;
        }
        // Release spacing has its own sixty-second quiet clock. Never advance
        // last_429 while lazily applying either kind of decay.
        while state.release_interval > MIN_RELEASE
            && state.release_decay_at.is_some_and(|at| at <= now)
        {
            state.release_interval = state.release_interval.mul_f64(0.75).max(MIN_RELEASE);
            state.release_decay_at = state
                .release_decay_at
                .map(|at| at + StdDuration::from_secs(60));
        }
        if state.release_interval == MIN_RELEASE {
            state.release_decay_at = None;
        }
    }

    pub fn observe_429(&self, id: &str, retry_after: Option<StdDuration>) -> PacingSnapshot {
        self.observe_429_at(id, Instant::now(), Instant::now(), retry_after, 1.0)
    }

    pub fn observe_429_at(
        &self,
        id: &str,
        sent_at: Instant,
        now: Instant,
        retry_after: Option<StdDuration>,
        jitter: f64,
    ) -> PacingSnapshot {
        let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
        let state = Self::pacing_state(&mut states, id);
        Self::decay_pacing(state, now);
        let new_window = state.backoff_until.is_none_or(|until| now >= until);
        if new_window && state.window_had_429 {
            state.release_interval = (state.release_interval * 2).min(MAX_RELEASE);
        }
        if new_window {
            state.step_raised = false;
        }
        let raises_step =
            !state.step_raised && state.window_set_at.is_none_or(|set| sent_at >= set);
        if raises_step {
            state.step = state.step.saturating_add(1).min(5);
            state.step_raised = true;
        }
        let exponential = StdDuration::from_secs(1u64 << state.step);
        let base = retry_after.unwrap_or_default().max(exponential);
        let factor = jitter.clamp(0.8, 1.2);
        let delay = base.mul_f64(factor);
        let candidate = now + delay;
        state.backoff_until = Some(
            state
                .backoff_until
                .map_or(candidate, |old| old.max(candidate)),
        );
        if new_window {
            state.window_set_at = Some(now);
            state.window_had_429 = true;
        }
        state.last_429 = Some(now);
        state.step_decay_interval = delay;
        state.step_decay_at = state.backoff_until.map(|until| until + delay);
        state.release_decay_at = Some(now + StdDuration::from_secs(60));
        state.wake.notify_one();
        PacingSnapshot {
            rate_backoff_until: state.backoff_until,
            rate_step: state.step,
            rate_release_interval: state.release_interval,
            waiting: state.waiting.len(),
        }
    }

    pub async fn admit(self: &Arc<Self>, id: &str) -> AdmissionGuard {
        let deadline =
            Instant::now() + StdDuration::from_secs(self.rate_hold_secs.load(Ordering::Relaxed));
        self.admit_until(id, deadline).await
    }

    /// The request budget also bounds later retries. Normal releases stay FIFO;
    /// a forced budget expiry bypasses pacing but still updates its release clock.
    pub async fn admit_until(self: &Arc<Self>, id: &str, deadline: Instant) -> AdmissionGuard {
        let local_deadline =
            Instant::now() + StdDuration::from_secs(self.rate_hold_secs.load(Ordering::Relaxed));
        let waiter = Arc::new(PacingWaiter {
            deadline: deadline.min(local_deadline),
            forced: std::sync::atomic::AtomicBool::new(false),
            notify: Notify::new(),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        });
        let immediate = {
            let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
            let state = Self::pacing_state(&mut states, id);
            let now = Instant::now();
            Self::decay_pacing(state, now);
            let pacing = state.step > 0 || state.backoff_until.is_some();
            let open = state.waiting.is_empty()
                && (!pacing
                    || (state.backoff_until.is_none_or(|until| now >= until)
                        && state.last_release.is_none_or(|last| {
                            now.saturating_duration_since(last) >= state.release_interval
                        })));
            if open || now >= waiter.deadline {
                waiter
                    .forced
                    .store(now >= waiter.deadline, Ordering::Release);
                if pacing {
                    state.last_release = Some(now);
                }
                true
            } else {
                state.waiting.push_back(waiter.clone());
                state.wake.notify_one(); // a later arrival can have less request budget
                if !state.release_task {
                    state.release_task = true;
                    let runtime = Arc::clone(self);
                    let account = id.to_owned();
                    tokio::spawn(async move {
                        runtime.release_loop(account).await;
                    });
                }
                false
            }
        };
        if !immediate {
            let cancellation = AdmissionCancellation {
                runtime: Arc::downgrade(self),
                id: id.to_owned(),
                waiter: waiter.clone(),
            };
            waiter.notify.notified().await;
            drop(cancellation);
        }
        AdmissionGuard {
            admission: if waiter.forced.load(Ordering::Acquire) {
                Admission::Forced
            } else {
                Admission::Released
            },
            runtime: Arc::downgrade(self),
            id: id.to_owned(),
        }
    }

    async fn release_loop(self: Arc<Self>, id: String) {
        loop {
            let (wait, wake, done) = {
                let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
                let state = Self::pacing_state(&mut states, &id);
                let now = Instant::now();
                Self::decay_pacing(state, now);
                while state
                    .waiting
                    .front()
                    .is_some_and(|waiter| waiter.cancelled.load(Ordering::Acquire))
                {
                    state.waiting.pop_front();
                }
                let selected = state
                    .waiting
                    .iter()
                    .position(|w| !w.cancelled.load(Ordering::Acquire) && now >= w.deadline)
                    .unwrap_or(0);
                let Some(waiter) = state.waiting.get(selected).cloned() else {
                    state.release_task = false;
                    break;
                };
                let deadline = waiter.deadline;
                let pacing = state.step > 0 || state.backoff_until.is_some();
                let gate = if pacing {
                    state.backoff_until.unwrap_or(now).max(
                        state
                            .last_release
                            .map_or(now, |last| last + state.release_interval),
                    )
                } else {
                    now
                };
                if now >= gate || now >= deadline {
                    let forced = now >= deadline;
                    state.waiting.remove(selected);
                    if pacing {
                        state.last_release = Some(now);
                    }
                    (Some(waiter), state.wake.clone(), forced)
                } else {
                    (None, state.wake.clone(), false)
                }
            };
            if let Some(waiter) = wait {
                waiter.forced.store(done, Ordering::Release);
                waiter.notify.notify_one();
                continue;
            }
            let sleep_for = {
                let states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
                let state = states.get(&id).expect("pacing state exists");
                let now = Instant::now();
                let pacing = state.step > 0 || state.backoff_until.is_some_and(|until| until > now);
                let next = if pacing {
                    state
                        .backoff_until
                        .unwrap_or(now)
                        .max(
                            state
                                .last_release
                                .map_or(now, |last| last + state.release_interval),
                        )
                        .min(
                            state
                                .waiting
                                .iter()
                                .filter(|w| !w.cancelled.load(Ordering::Acquire))
                                .map(|w| w.deadline)
                                .min()
                                .unwrap_or(now),
                        )
                } else {
                    now
                };
                next.saturating_duration_since(now)
            };
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {},
                _ = wake.notified() => {},
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub fn set_refresh_attention(&self, id: &str, attention: bool) {
        let mut flags = self
            .refresh_attention
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let changed = if attention {
            flags.insert(id.into())
        } else {
            flags.remove(id)
        };
        if changed {
            let _ = self.events.send(
                json!({"event":"account_changed","data":{"id":id,"refresh_attention":attention}}),
            );
        }
    }

    pub(crate) fn set_poller(&self, poller: &Arc<crate::quota::UsagePoller>) {
        *self.poller.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(poller));
    }

    pub fn track_inflight(self: &Arc<Self>, id: &str) -> InflightGuard {
        if let Ok(mut counts) = self.inflight.lock() {
            let count = counts.entry(id.to_owned()).or_default();
            *count = count.saturating_add(1);
        }
        InflightGuard {
            runtime: Arc::downgrade(self),
            id: id.to_owned(),
        }
    }

    pub fn inflight_count(&self, id: &str) -> usize {
        self.inflight
            .lock()
            .ok()
            .and_then(|counts| counts.get(id).copied())
            .unwrap_or(0)
    }

    pub fn probing(&self, id: &str) -> bool {
        self.probes.lock().map(|p| p.contains(id)).unwrap_or(true)
    }
    pub fn blocked_in_memory(&self, id: &str) -> bool {
        self.volatile
            .lock()
            .map(|v| v.contains_key(id))
            .unwrap_or(true)
    }
    pub fn clear_memory_health(&self, account: &Account) {
        if let Ok(mut health) = self.volatile.lock() {
            health.remove(&account.id);
        }
        let _=self.events.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state}}));
    }
    pub fn begin_probe(self: &Arc<Self>, account: &Account) -> Result<Option<Probe>, ProbeBusy> {
        if (!account.auth_probe_pending && account.auth_failures == 0)
            || account.health != Health::Ready
        {
            return Ok(None);
        }
        let mut probes = self.probes.lock().map_err(|_| ProbeBusy)?;
        if !probes.insert(account.id.clone()) {
            return Err(ProbeBusy);
        }
        Ok(Some(Probe {
            runtime: Arc::downgrade(self),
            id: account.id.clone(),
        }))
    }
    pub async fn probe_success(&self, id: &str) -> Result<(), ()> {
        let store = self.store.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            store
                .update_metadata(&id, |a| {
                    if a.health == Health::Ready {
                        a.auth_failures = 0;
                        a.auth_probe_pending = false;
                    }
                })
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())??;
        Ok(())
    }

    pub fn emit_account(&self, account: &Account) {
        if let Ok(mut health) = self.volatile.lock() {
            health.insert(account.id.clone(), account.health.clone());
        }
        let _=self.events.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state}}));
    }

    pub async fn transition(&self, id: &str, signal: HealthSignal) -> Result<Account, ()> {
        let store = self.store.clone();
        let id = id.to_owned();
        let account = tokio::task::spawn_blocking(move || {
            let updated = store
                .update_metadata(&id, |account| {
                    use crate::accounts::HealthSignal as Signal;
                    let signal = match signal {
                        HealthSignal::Cooling {
                            reason,
                            until,
                            scope,
                        } => Signal::Cooling {
                            reason,
                            until,
                            scope,
                        },
                        HealthSignal::Revoked(reason) => Signal::Revoked { reason },
                        HealthSignal::AuthFailure => Signal::AuthAfterRefresh,
                        HealthSignal::Ready => Signal::Ready,
                    };
                    account.apply_health_signal(signal, Utc::now());
                })
                .map_err(|_| ())?;
            Ok::<_, ()>(updated)
        })
        .await
        .map_err(|_| ())??;
        Ok(account)
    }

    pub async fn eligible(&self, id: &str, model: Option<&str>) -> bool {
        if self.blocked_in_memory(id) || self.probing(id) {
            return false;
        }
        let store = self.store.clone();
        let id = id.to_owned();
        let model = model.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            store.load(&id).is_ok_and(|a| a.eligible(model.as_deref()))
        })
        .await
        .unwrap_or(false)
    }

    pub async fn change(
        &self,
        id: &str,
        user_state: Option<UserState>,
        label: Option<String>,
        drain: Option<bool>,
        allow_credits: Option<bool>,
    ) -> Result<Account, ()> {
        if label
            .as_ref()
            .is_some_and(|l| l.len() > 256 || l.chars().any(char::is_control))
        {
            return Err(());
        }
        let store = self.store.clone();
        let id = id.to_owned();
        let credit_release = allow_credits == Some(true)
            && self.obs.quota_snapshot(&id).is_some_and(|q| {
                q["windows"].as_array().is_some_and(|w| {
                    !w.is_empty()
                        && w.iter()
                            .all(|w| w["used_pct"].as_f64().is_some_and(|p| p < 100.0))
                        && w.iter()
                            .any(|w| w["used_pct"].as_f64().is_some_and(|p| p >= 99.0))
                }) && q["signals"]["limit_reached"] != true
                    && q["signals"]["unified_status"] != "rejected"
            });
        let account = tokio::task::spawn_blocking(move || {
            store
                .update_settings(&id, drain, allow_credits, |a| {
                    if let Some(state) = user_state {
                        a.user_state = state;
                    }
                    if let Some(label) = label {
                        a.label = label;
                    }
                    if let Some(drain) = drain {
                        a.drain = drain;
                    }
                    if let Some(allow) = allow_credits {
                        a.allow_credits = allow;
                    }
                    if credit_release
                        && matches!(
                            a.health,
                            Health::Cooling {
                                reason: HealthReason::Quota,
                                ..
                            }
                        )
                    {
                        a.apply_health_signal(crate::accounts::HealthSignal::Ready, Utc::now());
                    }
                })
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())??;
        Ok(account)
    }

    pub async fn initialize_settings(
        &self,
        settings: std::collections::BTreeMap<String, crate::config::AccountConfig>,
    ) -> Result<(), ()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            for account in store.list().map_err(|_| ())? {
                if let Some(settings) = settings.get(&account.id)
                    && (account.drain != settings.drain
                        || account.allow_credits != settings.allow_credits)
                {
                    store
                        .update_metadata(&account.id, |a| {
                            a.drain = settings.drain;
                            a.allow_credits = settings.allow_credits;
                        })
                        .map_err(|_| ())?;
                }
            }
            Ok::<_, ()>(())
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn views(&self) -> Result<Vec<Value>, ()> {
        let store = self.store.clone();
        let accounts = tokio::task::spawn_blocking(move || store.list().map_err(|_| ()))
            .await
            .map_err(|_| ())??;
        Ok(accounts.into_iter().map(|a| {
            let quota = self.obs.quota_snapshot(&a.id).unwrap_or_default();
            let pacing = self.pacing_snapshot(&a.id);
            let backoff_until = pacing.rate_backoff_until.and_then(|until| {
                let remaining = until.saturating_duration_since(Instant::now());
                chrono::Duration::from_std(remaining).ok().map(|d| Utc::now() + d)
            });
            let health=self.volatile.lock().ok().and_then(|v|v.get(&a.id).cloned()).unwrap_or(a.health);
            let pending = self.probing(&a.id) || ((a.auth_probe_pending || a.auth_failures > 0) && health == Health::Ready);
            let health = if pending { json!({"kind":"cooling","reason":"auth","until":Utc::now(),"scope":"account","probe_pending":true}) } else { json!(health) };
            json!({"id":a.id,"generation":a.generation,"last_refresh_at":a.token_meta.last_refresh_at,"provider":a.provider,"label":a.label,"email":a.email,"plan":quota["plan"].as_str().map(str::to_owned).or(a.plan),"user_state":a.user_state,"source":a.source,"health":health,"probe_pending":pending,"pacing":{"backoff_until":backoff_until,"step":pacing.rate_step,"release_interval_ms":pacing.rate_release_interval.as_millis() as u64,"waiting":pacing.waiting},"windows":quota["windows"].as_array().cloned().unwrap_or_default(),"credits":quota["credits"],"last_used_at":null,"bound_sessions":0,"token_expires_at":a.token_meta.expires_at,"drain":a.drain,"allow_credits":a.allow_credits,"refresh_attention":self.refresh_attention.lock().map(|f|f.contains(&a.id)).unwrap_or(false)})
        }).collect())
    }

    /// Build account views with the live affinity statistics held by the
    /// bindings store.  AccountRuntime does not own that store, so callers
    /// that expose the complete AccountView pass it explicitly.
    pub async fn views_with_bindings(
        &self,
        bindings: &crate::bindings::BindingTable,
    ) -> Result<Vec<Value>, ()> {
        let mut views = self.views().await?;
        for view in &mut views {
            if let Some(id) = view["id"].as_str().map(str::to_owned) {
                let (count, last_used) = bindings.account_stats(&id);
                view["bound_sessions"] = json!(count);
                view["last_used_at"] = json!(last_used);
                let active = bindings.active_sessions(&id, Utc::now());
                view["active_sessions"] = json!(active);
                view["spread_threshold"] = json!(bindings.spread_threshold_for(&id));
            }
        }
        Ok(views)
    }

    pub async fn observe_headers(&self, id: &str, provider: Provider, headers: &HeaderMap) {
        self.observe_headers_for_model(id, provider, headers, None)
            .await;
    }

    pub async fn observe_headers_for_model(
        &self,
        id: &str,
        provider: Provider,
        headers: &HeaderMap,
        model: Option<&str>,
    ) {
        let previous = self.obs.quota_snapshot(id);
        self.obs.observe_headers(id, provider, headers);
        if previous != self.obs.quota_snapshot(id)
            && let Some(snapshot) = self.obs.quota_snapshot(id)
        {
            let _ = self.apply_quota_snapshot(id, snapshot, model).await;
        }
    }

    pub async fn observe_codex_event(&self, id: &str, event: &Value) {
        self.observe_codex_event_for_model(id, event, None).await;
    }

    pub async fn observe_codex_event_for_model(
        &self,
        id: &str,
        event: &Value,
        model: Option<&str>,
    ) {
        if event["type"] != "codex.rate_limits" {
            return;
        }
        self.obs.observe_codex_event(id, event);
        if let Some(snapshot) = self.obs.quota_snapshot(id) {
            let _ = self.apply_quota_snapshot(id, snapshot, model).await;
        }
    }

    /// Apply an observed snapshot through the same persisted reducer as outcomes
    /// and credential failures. Per-window timestamps identify fresh evidence.
    pub async fn apply_quota_snapshot(
        &self,
        id: &str,
        snapshot: Value,
        model: Option<&str>,
    ) -> Result<Account, ()> {
        let store = self.store.clone();
        let account_id = id.to_owned();
        let model = model.map(str::to_owned);
        let observation = snapshot.clone();
        let account = tokio::task::spawn_blocking(move || {
            store
                .update_metadata(&account_id, |account| {
                    account.apply_health_signal(
                        crate::accounts::HealthSignal::UsageSnapshot {
                            snapshot: observation,
                            model,
                            projected_allowance: 0.0,
                        },
                        Utc::now(),
                    );
                })
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())??;
        let _ = self
            .events
            .send(json!({"event":"quota_updated","data":{"id":id,"snapshot":snapshot}}));
        Ok(account)
    }

    pub fn cache_limit(&self, id: &str, status: StatusCode, headers: &HeaderMap, raw: &[u8]) {
        if raw.len() > 65536 {
            return;
        }
        let headers = headers
            .iter()
            .filter(|(name, _)| {
                let n = name.as_str();
                n.starts_with("x-codex-")
                    || n.starts_with("anthropic-ratelimit-")
                    || matches!(n, "retry-after" | "content-type" | "content-encoding")
            })
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        if let Ok(mut limits) = self.limits.lock() {
            limits.insert(
                id.into(),
                LimitResponse {
                    status,
                    headers,
                    body: Bytes::copy_from_slice(raw),
                },
            );
        }
    }

    pub async fn unavailable(
        &self,
        provider: Provider,
        websocket: bool,
        model: Option<&str>,
    ) -> Response {
        let poller = self
            .poller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(poller) = poller {
            poller.before_unavailable(provider).await;
        }
        self.unavailable_known(provider, websocket, model).await
    }

    pub(crate) async fn unavailable_known(
        &self,
        provider: Provider,
        websocket: bool,
        model: Option<&str>,
    ) -> Response {
        let store = self.store.clone();
        let accounts = tokio::task::spawn_blocking(move || store.list())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let candidates: Vec<_> = accounts
            .iter()
            .filter(|a| a.provider == provider && a.user_state == UserState::Active)
            .collect();
        let volatile = self.volatile.lock().map(|v| v.clone()).unwrap_or_default();
        let now = Utc::now();
        let mut cooling = Vec::new();
        for a in &candidates {
            for health in std::iter::once(&a.health).chain(volatile.get(&a.id)) {
                if let Health::Cooling {
                    reason,
                    until,
                    scope,
                } = health
                    && *until > now
                    && (scope == "account" || model.is_some_and(|m| scope == &format!("model:{m}")))
                {
                    let replay = matches!(reason, HealthReason::Quota | HealthReason::Auth);
                    cooling.push((*a, *until, replay));
                }
            }
            if self.probing(&a.id)
                || ((a.auth_probe_pending || a.auth_failures > 0) && a.health == Health::Ready)
            {
                cooling.push((*a, now + Duration::seconds(1), false));
            }
        }
        if !cooling.is_empty() {
            // Prefer genuine limit-response resets when available. Other
            // temporary blockers still produce row 14, using the native shape.
            cooling.sort_by_key(|(_, until, replay)| (!*replay, *until));
            let (a, until, replay) = cooling[0];
            let seconds = (until - Utc::now()).num_seconds().max(1);
            let mut result = if !websocket
                && replay
                && let Some(cache) = self.limits.lock().ok().and_then(|m| m.get(&a.id).cloned())
            {
                let mut r = Response::new(Body::from(cache.body));
                *r.status_mut() = cache.status;
                *r.headers_mut() = cache.headers;
                r
            } else {
                let error = if provider == Provider::Codex {
                    json!({"error":{"type":"usage_limit_reached","message":"quota-baton: all accounts are cooling","resets_at":until.timestamp(),"resets_in_seconds":seconds}})
                } else {
                    json!({"type":"error","error":{"type":"rate_limit_error","message":"quota-baton: all accounts are cooling"}})
                };
                (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response()
            };
            result.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&seconds.to_string()).unwrap(),
            );
            result
        } else {
            let mut error = json!({"error":{"type":"invalid_request_error","message":format!("quota-baton: no usable {} account — open the tray",provider.as_str())}});
            if provider == Provider::Claude {
                error["type"] = "error".into();
            }
            (StatusCode::BAD_REQUEST, Json(error)).into_response()
        }
    }
}

/// Per-provider subscription pool counts shared by admin state and CLI status.
/// Paused/disabled/relogin/probe-pending accounts are unavailable; active
/// canonical coolings are grouped by reason. No credential fields are read.
pub fn pool_counts(accounts: &[Value]) -> Value {
    let mut pools = json!({
        "claude": {"eligible":0,"cooling":{},"unavailable":0},
        "codex": {"eligible":0,"cooling":{},"unavailable":0}
    });
    for account in accounts {
        let Some(provider @ ("claude" | "codex")) = account["provider"].as_str() else {
            continue;
        };
        let pool = &mut pools[provider];
        let kind = account["health"]["kind"].as_str().unwrap_or("unknown");
        if account["user_state"] != "active"
            || account["probe_pending"] == true
            || !matches!(kind, "ready" | "cooling")
        {
            pool["unavailable"] = json!(pool["unavailable"].as_u64().unwrap_or(0) + 1);
        } else if kind == "cooling" {
            // Store-backed views have already expired their health. A volatile
            // store-error remains blocked until its pending write succeeds,
            // even after its displayed retry deadline.
            let reason = account["health"]["reason"]
                .as_str()
                .filter(|reason| {
                    matches!(
                        *reason,
                        "quota"
                            | "rate"
                            | "auth"
                            | "permission"
                            | "payment"
                            | "store_error"
                            | "link_stale"
                    )
                })
                .unwrap_or("unknown");
            pool["cooling"][reason] = json!(pool["cooling"][reason].as_u64().unwrap_or(0) + 1);
        } else {
            pool["eligible"] = json!(pool["eligible"].as_u64().unwrap_or(0) + 1);
        }
    }
    pools
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    #[test]
    fn c6_4_pools_count_each_provider_and_cooling_reason_without_credentials() {
        let accounts = vec![
            json!({"provider":"claude","user_state":"active","health":{"kind":"ready"}}),
            json!({"provider":"claude","user_state":"active","health":{"kind":"cooling","reason":"payment","until":"2099-01-01T00:00:00Z"}}),
            json!({"provider":"claude","user_state":"disabled","health":{"kind":"ready"}}),
            json!({"provider":"codex","user_state":"active","health":{"kind":"cooling","reason":"quota","until":"2099-01-01T00:00:00Z"}}),
            json!({"provider":"codex","user_state":"active","health":{"kind":"needs_relogin"}}),
            // Volatile persistence failures do not expire into eligibility.
            json!({"provider":"codex","user_state":"active","health":{"kind":"cooling","reason":"store_error","until":"2000-01-01T00:00:00Z"}}),
        ];
        assert_eq!(
            pool_counts(&accounts),
            json!({
                "claude":{"eligible":1,"cooling":{"payment":1},"unavailable":1},
                "codex":{"eligible":0,"cooling":{"quota":1,"store_error":1},"unavailable":1}
            })
        );
    }
}

// Tests-only addition for crates/qb-core/src/account_runtime.rs.
// Append after the staged production candidate; do not replace production code.
#[cfg(test)]
mod r1_rate_tests {
    use super::*;
    use futures_util::poll;
    use std::task::Poll;

    const ID: &str = "synthetic-r1-rate";

    struct Fixture {
        runtime: Arc<AccountRuntime>,
        // Declared last: runtime/store/observability drop before the home.
        _home: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/r1-rate-test-fixtures");
            std::fs::create_dir_all(&base).unwrap();
            let home = tempfile::tempdir_in(base).unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            Self {
                runtime: AccountRuntime::new(store, obs, 1.0),
                _home: home,
            }
        }
    }

    fn at(start: Instant, seconds: u64) -> Instant {
        start + StdDuration::from_secs(seconds)
    }

    // Two non-quiet windows: [0,2] and [2,6]. Quiet step windows then
    // end at 10 and 14; independent spacing decay starts at 2+60=62.
    fn two_windows(runtime: &AccountRuntime, start: Instant) {
        let first = runtime.observe_429_at(ID, start, start, None, 1.0);
        assert_eq!(first.rate_step, 1);
        assert_eq!(first.rate_backoff_until, Some(at(start, 2)));
        assert_eq!(first.rate_release_interval, MIN_RELEASE);
        let second = runtime.observe_429_at(ID, at(start, 2), at(start, 2), None, 1.0);
        assert_eq!(second.rate_step, 2);
        assert_eq!(second.rate_backoff_until, Some(at(start, 6)));
        assert_eq!(second.rate_release_interval, StdDuration::from_millis(500));
    }

    #[test]
    fn r1_step_decays_at_full_quiet_backoff_boundaries_not_sixty_seconds() {
        let f = Fixture::new();
        let start = Instant::now();
        two_windows(&f.runtime, start);
        let window_end = f.runtime.pacing_snapshot_at(ID, at(start, 6));
        assert!(window_end.rate_backoff_until.is_none());
        assert_eq!(
            window_end.rate_step, 2,
            "the 429-bearing window was not quiet"
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 10) - StdDuration::from_nanos(1))
                .rate_step,
            2
        );
        let first_quiet = f.runtime.pacing_snapshot_at(ID, at(start, 10));
        assert_eq!(
            first_quiet.rate_step, 1,
            "one complete quiet backoff interval"
        );
        assert_eq!(
            first_quiet.rate_release_interval,
            StdDuration::from_millis(500)
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 14) - StdDuration::from_nanos(1))
                .rate_step,
            1
        );
        let second_quiet = f.runtime.pacing_snapshot_at(ID, at(start, 14));
        assert_eq!(second_quiet.rate_step, 0);
        assert_eq!(
            second_quiet.rate_release_interval,
            StdDuration::from_millis(500)
        );
    }

    #[test]
    fn r1_step_snapshots_do_not_move_release_decay_anchor_or_double_apply_decay() {
        let f = Fixture::new();
        let start = Instant::now();
        two_windows(&f.runtime, start);
        assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 14)).rate_step, 0);
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 62) - StdDuration::from_nanos(1))
                .rate_release_interval,
            StdDuration::from_millis(500)
        );
        let first = f.runtime.pacing_snapshot_at(ID, at(start, 62));
        assert_eq!(first.rate_release_interval, StdDuration::from_millis(375));
        assert_eq!(
            f.runtime.pacing_snapshot_at(ID, at(start, 62)),
            first,
            "lazy decay is idempotent"
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 122))
                .rate_release_interval,
            StdDuration::from_micros(281_250)
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 182))
                .rate_release_interval,
            MIN_RELEASE
        );
        let states = f.runtime.pacing.lock().unwrap();
        assert_eq!(
            states[ID].last_429,
            Some(at(start, 2)),
            "a decay is not another 429"
        );
    }

    #[test]
    fn r1_late_inflight_429_extends_window_and_restarts_both_quiet_clocks() {
        let f = Fixture::new();
        let start = Instant::now();
        two_windows(&f.runtime, start);
        // Sent before this window was set: it may extend the gate but must not
        // count as a new step. The new quiet boundaries are 13,17 and 65.
        let late = f.runtime.observe_429_at(ID, start, at(start, 5), None, 1.0);
        assert_eq!(late.rate_step, 2);
        assert_eq!(late.rate_backoff_until, Some(at(start, 9)));
        assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 10)).rate_step, 2);
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 13) - StdDuration::from_nanos(1))
                .rate_step,
            2
        );
        assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 13)).rate_step, 1);
        assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 17)).rate_step, 0);
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 62))
                .rate_release_interval,
            StdDuration::from_millis(500)
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 65))
                .rate_release_interval,
            StdDuration::from_millis(375)
        );
    }

    #[test]
    fn r1_quiet_step_zero_removes_spacing_before_release_interval_reaches_floor() {
        let f = Fixture::new();
        let start = Instant::now();
        two_windows(&f.runtime, start);
        let now = at(start, 14);
        let snapshot = f.runtime.pacing_snapshot_at(ID, now);
        assert_eq!(snapshot.rate_step, 0);
        assert_eq!(
            snapshot.rate_release_interval,
            StdDuration::from_millis(500)
        );
        for _ in 0..32 {
            assert!(
                f.runtime.admit_at(ID, now),
                "step zero must not retain a spacing gate"
            );
        }
    }

    fn queue_deadlines(runtime: &AccountRuntime) -> Vec<Instant> {
        runtime.pacing.lock().unwrap()[ID]
            .waiting
            .iter()
            .map(|w| w.deadline)
            .collect()
    }

    fn last_release(runtime: &AccountRuntime) -> Option<Instant> {
        runtime.pacing.lock().unwrap()[ID].last_release
    }

    fn block(runtime: &AccountRuntime) {
        runtime.observe_429(ID, Some(StdDuration::from_secs(30)));
    }

    // Open the backoff only; deliberately leave last_release intact so the
    // real release_loop still enforces normal/forced release spacing.
    fn open_backoff(runtime: &AccountRuntime) {
        let mut states = runtime.pacing.lock().unwrap();
        let state = states.get_mut(ID).unwrap();
        state.backoff_until = None;
        state.wake.notify_one();
    }

    #[tokio::test]
    async fn r1_admit_until_uses_minimum_of_caller_deadline_and_local_cap() {
        let f = Fixture::new();
        block(&f.runtime);
        f.runtime.set_rate_hold_secs(1);
        let before = Instant::now();
        let mut local = Box::pin(f.runtime.admit_until(ID, at(before, 20)));
        assert!(poll!(local.as_mut()).is_pending());
        let after = Instant::now();
        let queued = queue_deadlines(&f.runtime);
        assert_eq!(queued.len(), 1);
        assert!(queued[0] >= at(before, 1) && queued[0] <= at(after, 1));
        drop(local);
        let caller = Instant::now() + StdDuration::from_millis(100);
        let mut shorter = Box::pin(f.runtime.admit_until(ID, caller));
        assert!(poll!(shorter.as_mut()).is_pending());
        assert_eq!(*queue_deadlines(&f.runtime).last().unwrap(), caller);
        drop(shorter);
        tokio::task::yield_now().await;
        assert!(queue_deadlines(&f.runtime).is_empty());
        assert!(!f.runtime.pacing.lock().unwrap()[ID].release_task);
        assert!(last_release(&f.runtime).is_none());
    }

    #[tokio::test]
    async fn r1_retries_reuse_one_absolute_budget_and_expired_retry_is_immediate() {
        let f = Fixture::new();
        block(&f.runtime);
        open_backoff(&f.runtime);
        let deadline = Instant::now() + StdDuration::from_millis(120);
        let first = f.runtime.admit_until(ID, deadline).await;
        assert!(!first.forced());
        block(&f.runtime); // A retry sees a new 429, not a new request budget.
        let mut retry = Box::pin(f.runtime.admit_until(ID, deadline));
        assert!(poll!(retry.as_mut()).is_pending());
        assert_eq!(*queue_deadlines(&f.runtime).last().unwrap(), deadline);
        let forced = tokio::time::timeout(StdDuration::from_millis(600), retry)
            .await
            .expect("retry renewed the total request budget");
        assert!(forced.forced());
        assert!(last_release(&f.runtime).unwrap() >= deadline);
        block(&f.runtime);
        let mut expired = Box::pin(f.runtime.admit_until(ID, deadline));
        let Poll::Ready(final_admission) = poll!(expired.as_mut()) else {
            panic!("already exhausted absolute budget was queued again");
        };
        assert!(final_admission.forced());
        assert!(queue_deadlines(&f.runtime).is_empty());
    }

    #[tokio::test]
    async fn r1_forced_admission_updates_shared_clock_before_next_normal_release() {
        let f = Fixture::new();
        block(&f.runtime);
        open_backoff(&f.runtime);
        let before = Instant::now();
        let forced = f
            .runtime
            .admit_until(ID, before - StdDuration::from_millis(1))
            .await;
        assert!(forced.forced());
        let stamp = last_release(&f.runtime).expect("forced release did not update the clock");
        assert!(stamp >= before && stamp <= Instant::now());
        let interval = f.runtime.pacing_snapshot(ID).rate_release_interval;
        assert!(
            !f.runtime
                .admit_at(ID, stamp + interval - StdDuration::from_nanos(1))
        );
        assert!(f.runtime.admit_at(ID, stamp + interval));
    }

    #[tokio::test]
    async fn r1_normal_waiters_stay_fifo_and_cancelled_middle_consumes_no_release() {
        let f = Fixture::new();
        block(&f.runtime);
        let deadline = at(Instant::now(), 5);
        let mut first = Box::pin(f.runtime.admit_until(ID, deadline));
        let mut cancelled = Box::pin(f.runtime.admit_until(ID, deadline));
        let mut last = Box::pin(f.runtime.admit_until(ID, deadline));
        assert!(poll!(first.as_mut()).is_pending());
        assert!(poll!(cancelled.as_mut()).is_pending());
        assert!(poll!(last.as_mut()).is_pending());
        assert_eq!(queue_deadlines(&f.runtime).len(), 3);
        drop(cancelled);
        open_backoff(&f.runtime);
        let first_guard = tokio::time::timeout(StdDuration::from_millis(200), first)
            .await
            .unwrap();
        assert!(!first_guard.forced());
        let first_stamp = last_release(&f.runtime).unwrap();
        assert!(
            poll!(last.as_mut()).is_pending(),
            "normal tail overtook release spacing"
        );
        let last_guard = tokio::time::timeout(StdDuration::from_millis(700), last)
            .await
            .unwrap();
        assert!(!last_guard.forced());
        let last_stamp = last_release(&f.runtime).unwrap();
        let gap = last_stamp.duration_since(first_stamp);
        assert!(
            gap >= MIN_RELEASE && gap < MIN_RELEASE * 2,
            "cancelled middle consumed a release slot: {gap:?}"
        );
        assert!(queue_deadlines(&f.runtime).is_empty());
    }

    #[tokio::test]
    async fn r1_earlier_tail_deadline_wakes_sleeping_head_without_normal_fifo_bypass() {
        let f = Fixture::new();
        block(&f.runtime);
        let mut head = Box::pin(f.runtime.admit_until(ID, at(Instant::now(), 5)));
        assert!(poll!(head.as_mut()).is_pending());
        // Let the actual release task sleep on the head's later deadline.
        tokio::task::yield_now().await;
        let deadline = Instant::now() + StdDuration::from_millis(80);
        let mut tail = Box::pin(f.runtime.admit_until(ID, deadline));
        assert!(poll!(tail.as_mut()).is_pending());
        let tail_guard = tokio::time::timeout(StdDuration::from_millis(600), tail)
            .await
            .expect("earlier tail failed to wake the head's release-loop sleep");
        assert!(tail_guard.forced());
        let forced_stamp = last_release(&f.runtime).unwrap();
        assert!(forced_stamp >= deadline);
        assert!(
            poll!(head.as_mut()).is_pending(),
            "head bypassed the still-closed gate"
        );
        assert_eq!(queue_deadlines(&f.runtime).len(), 1);
        open_backoff(&f.runtime);
        let head_guard = tokio::time::timeout(StdDuration::from_millis(700), head)
            .await
            .unwrap();
        assert!(!head_guard.forced());
        assert!(
            last_release(&f.runtime)
                .unwrap()
                .duration_since(forced_stamp)
                >= MIN_RELEASE
        );
        assert!(queue_deadlines(&f.runtime).is_empty());
    }

    #[tokio::test]
    async fn r1_cancelled_earlier_tail_neither_forces_nor_updates_release_clock() {
        let f = Fixture::new();
        block(&f.runtime);
        let mut head = Box::pin(f.runtime.admit_until(ID, at(Instant::now(), 5)));
        assert!(poll!(head.as_mut()).is_pending());
        let mut cancelled = Box::pin(
            f.runtime
                .admit_until(ID, Instant::now() + StdDuration::from_millis(30)),
        );
        assert!(poll!(cancelled.as_mut()).is_pending());
        drop(cancelled);
        tokio::time::sleep(StdDuration::from_millis(60)).await;
        assert!(
            last_release(&f.runtime).is_none(),
            "cancelled deadline created a phantom forced send"
        );
        assert!(poll!(head.as_mut()).is_pending());
        open_backoff(&f.runtime);
        let head_guard = tokio::time::timeout(StdDuration::from_millis(200), head)
            .await
            .unwrap();
        assert!(!head_guard.forced());
        assert!(queue_deadlines(&f.runtime).is_empty());
    }

    #[tokio::test]
    async fn r1_unpaced_admit_until_never_queues_or_starts_spacing() {
        let f = Fixture::new();
        let deadline = at(Instant::now(), 5);
        let mut guards = Vec::new();
        for _ in 0..32 {
            let mut future = Box::pin(f.runtime.admit_until(ID, deadline));
            let Poll::Ready(guard) = poll!(future.as_mut()) else {
                panic!("unpaced account was queued or concurrency-limited");
            };
            assert!(!guard.forced());
            guards.push(guard); // Keep all admissions live concurrently.
        }
        assert!(queue_deadlines(&f.runtime).is_empty());
        assert!(last_release(&f.runtime).is_none());
        assert!(!f.runtime.pacing.lock().unwrap()[ID].release_task);
        assert_eq!(guards.len(), 32);
    }
}

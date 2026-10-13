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
    queued_at: Instant,
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
        let open = state.backoff_until.is_none_or(|until| now >= until)
            && state
                .last_release
                .is_none_or(|last| now.saturating_duration_since(last) >= state.release_interval)
            && state.waiting.is_empty();
        if open {
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
        if let Some(last) = state.last_429 {
            let quiet = now.saturating_duration_since(last);
            let drops = (quiet.as_secs() / 60) as u32;
            if drops > 0 {
                state.step = state
                    .step
                    .saturating_sub(drops.min(state.step as u32) as u8);
                let mut interval = state.release_interval;
                for _ in 0..drops {
                    interval = interval.mul_f64(0.75).max(MIN_RELEASE);
                }
                state.release_interval = interval;
                state.last_429 = Some(last + StdDuration::from_secs(u64::from(drops) * 60));
            }
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
        state.wake.notify_one();
        PacingSnapshot {
            rate_backoff_until: state.backoff_until,
            rate_step: state.step,
            rate_release_interval: state.release_interval,
            waiting: state.waiting.len(),
        }
    }

    pub async fn admit(self: &Arc<Self>, id: &str) -> AdmissionGuard {
        let waiter = Arc::new(PacingWaiter {
            queued_at: Instant::now(),
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
            if open {
                if pacing {
                    state.last_release = Some(now);
                }
                true
            } else {
                state.waiting.push_back(waiter.clone());
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
                let Some(waiter) = state.waiting.front().cloned() else {
                    state.release_task = false;
                    break;
                };
                let deadline = waiter.queued_at
                    + StdDuration::from_secs(self.rate_hold_secs.load(Ordering::Relaxed));
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
                    state.waiting.pop_front();
                    if pacing {
                        state.last_release = Some(now);
                    }
                    (Some(waiter), state.wake.clone(), forced)
                } else {
                    (None, state.wake.clone(), false)
                }
            };
            if let Some(waiter) = wait {
                waiter.notify.notify_one();
                let _ = done;
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
                        .min(state.waiting.front().map_or(
                            now + StdDuration::from_secs(
                                self.rate_hold_secs.load(Ordering::Relaxed),
                            ),
                            |w| {
                                w.queued_at
                                    + StdDuration::from_secs(
                                        self.rate_hold_secs.load(Ordering::Relaxed),
                                    )
                            },
                        ))
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
            json!({"id":a.id,"provider":a.provider,"label":a.label,"email":a.email,"plan":quota["plan"].as_str().map(str::to_owned).or(a.plan),"user_state":a.user_state,"health":health,"probe_pending":pending,"pacing":{"backoff_until":backoff_until,"step":pacing.rate_step,"release_interval_ms":pacing.rate_release_interval.as_millis() as u64,"waiting":pacing.waiting},"windows":quota["windows"].as_array().cloned().unwrap_or_default(),"credits":quota["credits"],"last_used_at":null,"bound_sessions":0,"token_expires_at":a.token_meta.expires_at,"drain":a.drain,"allow_credits":a.allow_credits,"refresh_attention":self.refresh_attention.lock().map(|f|f.contains(&a.id)).unwrap_or(false)})
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
            if let Some(id) = view["id"].as_str() {
                let (count, last_used) = bindings.account_stats(id);
                view["bound_sessions"] = json!(count);
                view["last_used_at"] = json!(last_used);
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

//! Shared memory observer. The existing Observability writer drives tick/archive.
use super::{history::*, *};
use crate::{accounts::Provider, bindings::BindingTable};
use axum::http::HeaderMap;
use chrono::{Local, Offset};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

struct State {
    model: Dashboard,
    activity: ActivityLog,
    usage: UsageHistory,
    usage_forecasts: crate::usage_forecast::Forecasts,
    accounts: BTreeMap<AccountId, AccountFacts>,
    credits: BTreeMap<AccountId, bool>,
    incomplete: BTreeSet<SessionId>,
    membership_generation: u64,
}
#[derive(Clone, PartialEq, Eq)]
struct AccountFacts {
    user: crate::accounts::UserState,
    drain: bool,
    credits: bool,
    quota_cooling: Option<crate::accounts::QuotaCooling>,
}
struct Lookup {
    id: SessionId,
    sid: String,
}
pub struct Runtime {
    state: Mutex<State>,
    bindings: Mutex<Weak<BindingTable>>,
    events: Mutex<Option<tokio::sync::broadcast::Sender<serde_json::Value>>>,
    started: Instant,
    last_tick: Mutex<Instant>,
    next: AtomicU64,
    frozen: AtomicBool,
    lookup_root: Mutex<Option<PathBuf>>,
    lookup_enabled: AtomicBool,
    lookups: Mutex<VecDeque<Lookup>>,
}
pub(crate) struct Archive {
    pub activity: Option<ActivityLog>,
    pub usage: Option<UsageHistory>,
}
pub struct RequestGuard {
    runtime: Arc<Runtime>,
    pub id: u64,
    session: SessionId,
    move_at: Option<DateTime<Utc>>,
}
pub struct SocketGuard {
    runtime: Arc<Runtime>,
    id: u64,
}
pub struct PacingGuard {
    runtime: Arc<Runtime>,
    request: u64,
}
#[derive(Clone)]
pub struct PacingSource {
    runtime: Arc<Runtime>,
    request: u64,
}
impl PacingSource {
    pub fn start(&self) -> PacingGuard {
        self.runtime.pace(self.request, Some(Utc::now()));
        PacingGuard {
            runtime: self.runtime.clone(),
            request: self.request,
        }
    }
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Some(mut state) = self.runtime.writable() {
            state.model.socket_close(self.id);
        }
    }
}
impl Drop for PacingGuard {
    fn drop(&mut self) {
        self.runtime.pace(self.request, None);
    }
}
impl RequestGuard {
    pub fn pacing_source(&self) -> PacingSource {
        PacingSource {
            runtime: self.runtime.clone(),
            request: self.id,
        }
    }
    pub fn pacing(&self) -> PacingGuard {
        self.runtime.pace(self.id, Some(Utc::now()));
        PacingGuard {
            runtime: self.runtime.clone(),
            request: self.id,
        }
    }
    pub fn response_model(&self, model: Option<&str>) {
        if let Some(mut state) = self.runtime.writable() {
            state.model.display_model(&self.session, model);
        }
    }
    pub fn cache_result(&self, result: CacheResult) {
        if let Some(at) = self.move_at
            && let Some(mut state) = self.runtime.writable()
        {
            state.model.move_cache_result(&self.session, at, result);
        }
    }
    pub fn finish(self, end: RequestEnd) {
        if let Some(mut state) = self.runtime.writable() {
            state.model.finish(self.id, end);
        }
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some(mut state) = self.runtime.writable() {
            state.model.finish(
                self.id,
                RequestEnd {
                    at: Utc::now(),
                    outcome: RequestOutcome::ClientDrop,
                    status: 0,
                    ttfb_ms: 0,
                    dur_ms: 0,
                    rate_wait_ms: 0,
                    rate_retries: 0,
                    failover: None,
                },
            );
        }
    }
}
impl Runtime {
    fn writable(&self) -> Option<std::sync::MutexGuard<'_, State>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (!self.frozen.load(Ordering::Acquire)).then_some(state)
    }
    fn pace(&self, request: u64, since: Option<DateTime<Utc>>) {
        if let Some(mut state) = self.writable() {
            state.model.pacing(request, since);
        }
    }

    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                model: Dashboard::new(Local::now().offset().fix(), 300),
                activity: ActivityLog::default(),
                usage: UsageHistory::default(),
                usage_forecasts: crate::usage_forecast::Forecasts::default(),
                accounts: BTreeMap::new(),
                credits: BTreeMap::new(),
                incomplete: BTreeSet::new(),
                membership_generation: 0,
            }),
            bindings: Mutex::new(Weak::new()),
            events: Mutex::new(None),
            started: Instant::now(),
            last_tick: Mutex::new(Instant::now() - std::time::Duration::from_secs(1)),
            next: AtomicU64::new(1),
            frozen: AtomicBool::new(false),
            lookup_root: Mutex::new(None),
            lookup_enabled: AtomicBool::new(false),
            lookups: Mutex::new(VecDeque::new()),
        })
    }
    pub(crate) fn attach(&self, bindings: &Arc<BindingTable>, config: &crate::config::Config) {
        *self.bindings.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(bindings);
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .model
            .active_window_secs = config.routing.active_window_secs;
        self.lookup_enabled
            .store(config.dashboard.resolve_claude_project, Ordering::Release);
        if !config.dashboard.resolve_claude_project {
            *self.lookup_root.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        self.sync();
    }
    /// Explicit serve wiring only: constructors NEVER discover a profile root.
    /// Merely wiring this path does not scan; authenticated session reads queue work.
    pub fn configure_claude_project_root(&self, root: Option<PathBuf>) -> Result<(), &'static str> {
        if root.as_ref().is_some_and(|root| !root.is_absolute()) {
            return Err("lookup root must be absolute");
        }
        #[cfg(any(test, feature = "testkit"))]
        if root.as_ref().is_some_and(|root| {
            !root
                .components()
                .any(|part| matches!(part,std::path::Component::Normal(name) if name==".sandbox"))
        }) {
            return Err("synthetic lookup root must be inside .sandbox");
        }
        *self.lookup_root.lock().unwrap_or_else(|e| e.into_inner()) = root;
        Ok(())
    }
    #[cfg(feature = "testkit")]
    pub fn set_lookup_root(&self, root: Option<PathBuf>) -> Result<(), &'static str> {
        self.configure_claude_project_root(root)
    }
    fn queue_lookups(&self) {
        if !self.lookup_enabled.load(Ordering::Acquire)
            || self
                .lookup_root
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
        {
            return;
        }
        let Some(table) = self.table() else {
            return;
        };
        let mut pending = self.lookups.lock().unwrap_or_else(|e| e.into_inner());
        let remaining = 64usize.saturating_sub(pending.len());
        if remaining == 0 {
            return;
        }
        let ids = {
            let Some(mut state) = self.writable() else {
                return;
            };
            let ids: Vec<_> = state.model.sessions.keys().cloned().collect();
            ids.into_iter()
                .filter(|id| state.model.claude_lookup_admit(id, self.millis()))
                .take(remaining)
                .collect::<BTreeSet<_>>()
        };
        for (id, sid) in table.dashboard_lookup_candidates(&ids) {
            pending.push_back(Lookup { id, sid });
        }
    }
    pub(crate) fn set_events(&self, sender: tokio::sync::broadcast::Sender<serde_json::Value>) {
        *self.events.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
    }
    fn emit(&self, name: &str, data: impl serde::Serialize) {
        if let Some(sender) = self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = sender.send(serde_json::json!({"event":name,"data":data}));
        }
    }
    fn table(&self) -> Option<Arc<BindingTable>> {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
    }
    fn snapshot_generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .membership_generation
    }
    // The external binding lock is released before this state transaction.
    // Any intervening ensure or snapshot merge invalidates the old snapshot.
    fn merge_snapshot(&self, generation: u64, rows: Vec<BindingGroup>, now: DateTime<Utc>) -> bool {
        let Some(mut state) = self.writable() else {
            return false;
        };
        if state.membership_generation != generation || generation == u64::MAX {
            return false;
        }
        if state.model.sync_bindings(rows, now).is_err() {
            return false;
        }
        state.membership_generation = state.membership_generation.saturating_add(1);
        let retained: BTreeSet<_> = state.model.sessions.keys().cloned().collect();
        state.incomplete.retain(|id| retained.contains(id));
        let incomplete: Vec<_> = state.incomplete.iter().cloned().collect();
        for id in incomplete {
            if let Some(stats) = state.model.sessions.get_mut(&id) {
                stats.truncated = true;
                stats.day.complete = false;
                stats.inflight_known = false;
            }
        }
        true
    }
    fn sync(&self) {
        if self.frozen.load(Ordering::Acquire) {
            return;
        }
        if let Some(table) = self.table() {
            let generation = self.snapshot_generation();
            let now = Utc::now();
            let rows = table.dashboard_snapshot(now);
            self.merge_snapshot(generation, rows, now);
        }
    }
    fn ensure(&self, key: &str, client: Option<ClientKind>) -> Option<SessionId> {
        let table = self.table()?;
        if let Some(kind) = client {
            table.dashboard_client(key, kind);
        }
        let now = Utc::now();
        let row = table.dashboard_group(key, now)?;
        let id = row.id.clone();
        {
            let mut state = self.writable()?;
            state.model.upsert_binding(row, now).ok()?;
            state.membership_generation = state.membership_generation.saturating_add(1);
            if state.incomplete.contains(&id)
                && let Some(stats) = state.model.sessions.get_mut(&id)
            {
                stats.truncated = true;
                stats.day.complete = false;
                stats.inflight_known = false;
            }
        }
        Some(id)
    }
    pub fn incomplete(&self, id: SessionId) {
        let Some(mut state) = self.writable() else {
            return;
        };
        if state.incomplete.len() < GROUP_CAP {
            state.incomplete.insert(id.clone());
        }
        if let Some(stats) = state.model.sessions.get_mut(&id) {
            stats.inflight_known = false;
            stats.truncated = true;
            stats.day.complete = false;
            state.model.dirty(&id);
        }
    }
    pub fn evidence(&self, key: &str, headers: &HeaderMap, label: Option<labels::MemoryLabel>) {
        if self.frozen.load(Ordering::Acquire) {
            return;
        }
        let client = ClientView::from_headers(
            header(headers, "originator"),
            header(headers, "user-agent"),
            header(headers, "x-app"),
        );
        if let Some(id) = self.ensure(key, Some(client.kind))
            && let Some(mut state) = self.writable()
        {
            state.model.evidence(
                &id,
                PrivateEvidence::new(
                    label.or_else(|| labels::codex_workspace(headers)),
                    labels::OriginalShortId::from_root_key(key),
                ),
                Some(client),
            );
        }
    }
    pub fn request(
        self: &Arc<Self>,
        key: &str,
        headers: &HeaderMap,
        label: Option<labels::MemoryLabel>,
        at: DateTime<Utc>,
        transport: Transport,
    ) -> Option<RequestGuard> {
        let client = ClientView::from_headers(
            header(headers, "originator"),
            header(headers, "user-agent"),
            header(headers, "x-app"),
        );
        let label = label.or_else(|| {
            key.starts_with("codex:")
                .then(|| labels::codex_workspace(headers))
                .flatten()
        });
        self.request_observed(key, client, label, at, transport)
    }
    pub(crate) fn request_observed(
        self: &Arc<Self>,
        key: &str,
        client: ClientView,
        label: Option<labels::MemoryLabel>,
        at: DateTime<Utc>,
        transport: Transport,
    ) -> Option<RequestGuard> {
        if self.frozen.load(Ordering::Acquire) {
            return None;
        }
        let id = self.ensure(key, Some(client.kind))?;
        let request = self.next.fetch_add(1, Ordering::Relaxed);
        let move_at;
        {
            let mut state = self.writable()?;
            state.model.evidence(
                &id,
                PrivateEvidence::new(label, labels::OriginalShortId::from_root_key(key)),
                Some(client),
            );
            state
                .model
                .start(
                    &id,
                    RequestStart {
                        id: request,
                        at,
                        transport,
                    },
                )
                .ok()?;
            move_at = state.model.sessions.get(&id).and_then(|stats| {
                stats
                    .timeline
                    .iter()
                    .rev()
                    .find(|entry| !entry.reason.is_initial_placement())
                    .map(|entry| entry.at)
            });
            if key != group_root(key) {
                state.model.subagent(&id, key, at);
            }
        }
        Some(RequestGuard {
            runtime: self.clone(),
            id: request,
            session: id,
            move_at,
        })
    }
    pub fn socket(self: &Arc<Self>, key: &str) -> Option<SocketGuard> {
        if self.frozen.load(Ordering::Acquire) {
            return None;
        }
        let session = self.ensure(key, None)?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.writable()?.model.socket_open(id, &session).ok()?;
        Some(SocketGuard {
            runtime: self.clone(),
            id,
        })
    }
    pub fn response_model(&self, key: &str, value: Option<&str>) {
        if let Some(id) = self.ensure(key, None)
            && let Some(mut state) = self.writable()
        {
            state.model.display_model(&id, value);
        }
    }
    pub fn placement(&self, key: &str, from: Option<&str>, to: &str, reason: &str) {
        if self.frozen.load(Ordering::Acquire) {
            return;
        }
        let reason = match reason {
            "new" => MoveReason::New,
            "launch_model" => MoveReason::LaunchModel,
            "model_choice" => MoveReason::ModelChoice,
            "forced" => MoveReason::Forced,
            "cold_rebalance" => MoveReason::ColdRebalance,
            "reset_hold_timeout" => MoveReason::ResetHoldTimeout,
            "overload" => MoveReason::Overload,
            _ => return,
        };
        let (Some(id), Ok(to)) = (self.ensure(key, None), AccountId::try_from(to.to_owned()))
        else {
            return;
        };
        let from = from.and_then(|s| s.to_owned().try_into().ok());
        let now = Utc::now();
        if let Some(mut state) = self.writable() {
            state.model.placed(
                &id,
                MoveView {
                    at: now,
                    from: from.clone(),
                    to: to.clone(),
                    reason,
                    next_turn_cache: None,
                },
            );
        }
        if !reason.is_initial_placement() {
            self.activity(ActivityInput {
                ts: now,
                kind: ActivityKind::Move,
                provider: if key.starts_with("claude:") {
                    Provider::Claude
                } else {
                    Provider::Codex
                },
                account: from,
                account_to: Some(to),
                notable: true,
                data: ActivityData {
                    session: Some(id),
                    reason: Some(reason),
                    ..Default::default()
                },
            });
        }
    }
    pub fn activity(&self, input: ActivityInput) {
        if self.frozen.load(Ordering::Acquire) {
            return;
        }
        let event = self
            .writable()
            .and_then(|mut state| state.activity.push(input, Utc::now()).ok().flatten());
        if let Some(event) = event {
            self.emit("activity", serde_json::json!({"event":event}));
        }
    }
    pub fn account_event(
        &self,
        kind: ActivityKind,
        provider: Provider,
        id: Option<&str>,
        outcome: Option<RequestOutcome>,
        expires_at: Option<DateTime<Utc>>,
    ) {
        self.activity(ActivityInput {
            ts: Utc::now(),
            kind,
            provider,
            account: id.and_then(|id| id.to_owned().try_into().ok()),
            account_to: None,
            notable: true,
            data: ActivityData {
                outcome,
                expires_at,
                ..Default::default()
            },
        });
    }
    /// Called under the account store transaction before enabling its observer.
    /// Startup facts emit no actions, and cannot overwrite an already observed value.
    pub(crate) fn baseline_accounts(&self, accounts: &[crate::accounts::Account]) {
        let Some(mut state) = self.writable() else {
            return;
        };
        for account in accounts {
            let Ok(id) = AccountId::try_from(account.id.clone()) else {
                continue;
            };
            if state.accounts.len() >= 128 && !state.accounts.contains_key(&id) {
                continue;
            }
            state.accounts.entry(id).or_insert(AccountFacts {
                user: account.user_state,
                drain: account.drain,
                credits: account.allow_credits,
                quota_cooling: account.quota_cooling.clone(),
            });
        }
    }
    pub fn account_changed(&self, account: &crate::accounts::Account) {
        if self.frozen.load(Ordering::Acquire) {
            return;
        }
        let Ok(id) = AccountId::try_from(account.id.clone()) else {
            return;
        };
        let facts = AccountFacts {
            user: account.user_state,
            drain: account.drain,
            credits: account.allow_credits,
            quota_cooling: account.quota_cooling.clone(),
        };
        let previous = {
            let Some(mut state) = self.writable() else {
                return;
            };
            if state.accounts.len() >= 128 && !state.accounts.contains_key(&id) {
                return;
            }
            state.accounts.insert(id.clone(), facts.clone())
        };
        let Some(old) = previous else {
            return;
        };
        if old == facts {
            return;
        }
        let mut actions = Vec::new();
        if old.user != facts.user {
            actions.push(match facts.user {
                crate::accounts::UserState::Active => SettingAction::Resume,
                crate::accounts::UserState::Paused => SettingAction::Pause,
                crate::accounts::UserState::Disabled => SettingAction::Disable,
            });
        }
        if old.drain != facts.drain {
            actions.push(if facts.drain {
                SettingAction::DrainOn
            } else {
                SettingAction::DrainOff
            });
        }
        if old.credits != facts.credits {
            actions.push(if facts.credits {
                SettingAction::CreditsOn
            } else {
                SettingAction::CreditsOff
            });
        }
        for action in actions {
            self.activity(ActivityInput {
                ts: Utc::now(),
                kind: ActivityKind::AccountChanged,
                provider: account.provider,
                account: Some(id.clone()),
                account_to: None,
                notable: true,
                data: ActivityData {
                    action: Some(action),
                    ..Default::default()
                },
            });
        }
    }
    pub(crate) fn account_health(
        &self,
        account: &crate::accounts::Account,
        from: &crate::accounts::Health,
    ) {
        // The callback precedes account_changed; facts retain the old C5
        // evidence after expiry/early recovery clears the account's evidence.
        let evidence = if matches!(account.health, crate::accounts::Health::Ready) {
            AccountId::try_from(account.id.clone())
                .ok()
                .and_then(|id| self.writable()?.accounts.get(&id)?.quota_cooling.clone())
        } else {
            account.quota_cooling.clone()
        };
        self.health_with_evidence(&account.id, from, &account.health, evidence.as_ref());
    }

    pub fn health(&self, id: &str, from: &crate::accounts::Health, to: &crate::accounts::Health) {
        self.health_with_evidence(id, from, to, None);
    }

    fn health_with_evidence(
        &self,
        id: &str,
        from: &crate::accounts::Health,
        to: &crate::accounts::Health,
        evidence: Option<&crate::accounts::QuotaCooling>,
    ) {
        use crate::accounts::{Health, HealthReason};
        let now = Utc::now();
        let kind = match to {
            Health::NeedsRelogin { .. } => ActivityKind::NeedsRelogin,
            Health::Cooling {
                reason: HealthReason::Quota,
                scope,
                ..
            } if !scope.starts_with("model:") => {
                // Extending a still-active account cooldown is the same episode.
                // Health persistence and the raw health_changed ledger remain independent.
                if matches!(from, Health::Cooling { reason: HealthReason::Quota, scope, until }
                    if !scope.starts_with("model:") && *until > now)
                {
                    return;
                }
                ActivityKind::QuotaReached
            }
            Health::Cooling {
                reason: HealthReason::LinkStale,
                ..
            } => ActivityKind::LinkStale,
            Health::Cooling {
                reason: HealthReason::Payment,
                ..
            } => ActivityKind::Payment,
            Health::Cooling {
                reason: HealthReason::Permission,
                ..
            } => ActivityKind::Permission,
            Health::Cooling {
                reason: HealthReason::StoreError,
                ..
            } => ActivityKind::StoreError,
            Health::Ready if matches!(from,Health::Cooling{reason:HealthReason::Quota,scope,..} if !scope.starts_with("model:")) => {
                ActivityKind::QuotaReset
            }
            _ => return,
        };
        // Health scope is never a window name. Fallback cooldown deadlines
        // are not observed reset evidence; leave unknown fields absent.
        let (window, reset) =
            if matches!(kind, ActivityKind::QuotaReached | ActivityKind::QuotaReset) {
                (
                    evidence.and_then(|e| e.window.clone().try_into().ok()),
                    evidence.map(|e| e.reset_at),
                )
            } else {
                (
                    None,
                    match to {
                        Health::Cooling { until, .. } => Some(*until),
                        _ => None,
                    },
                )
            };
        self.activity(ActivityInput {
            ts: now,
            kind,
            provider: if id.starts_with("cl-") {
                Provider::Claude
            } else {
                Provider::Codex
            },
            account: id.to_owned().try_into().ok(),
            account_to: None,
            notable: true,
            data: ActivityData {
                window,
                reset_at: reset,
                ..Default::default()
            },
        });
    }
    pub fn rate_hold(&self, input: RateHoldObservation) {
        if self.frozen.load(Ordering::Acquire) {
            return;
        }
        let event = self
            .writable()
            .and_then(|mut state| state.activity.rate_hold(input, Utc::now()).ok().flatten());
        if let Some(event) = event {
            self.emit("activity", serde_json::json!({"event":event}));
        }
    }
    /// Display-only input before chart deduplication; never replay persisted quota.
    pub fn observe_usage_forecast(
        &self,
        id: &str,
        name: &str,
        used_pct: f64,
        reset_at: Option<DateTime<Utc>>,
        observed_at: DateTime<Utc>,
    ) {
        if observed_at > Utc::now() || AccountId::try_from(id.to_owned()).is_err() {
            return;
        }
        if let Some(mut state) = self.writable() {
            state
                .usage_forecasts
                .observe(id, name, used_pct, reset_at, observed_at);
        }
    }
    pub fn usage_forecast(
        &self,
        id: &str,
        now: DateTime<Utc>,
        count_changed_at: Option<DateTime<Utc>>,
    ) -> crate::usage_forecast::AccountUsageForecast {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .usage_forecasts
            .view(id, now, count_changed_at)
    }
    pub fn observe_forecast_reset(
        &self,
        id: &str,
        name: &str,
        reset_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
    ) {
        if observed_at > Utc::now() || AccountId::try_from(id.to_owned()).is_err() {
            return;
        }
        if let Some(mut state) = self.writable() {
            state
                .usage_forecasts
                .observe_reset(id, name, reset_at, observed_at);
        }
    }
    pub fn observe_credit_forecast(&self, id: &str, balance: f64, observed_at: DateTime<Utc>) {
        if observed_at > Utc::now() || AccountId::try_from(id.to_owned()).is_err() {
            return;
        }
        if let Some(mut state) = self.writable() {
            state
                .usage_forecasts
                .observe_credit(id, balance, observed_at);
        }
    }
    pub fn usage(&self, input: UsageObservation) {
        if self.frozen.load(Ordering::Acquire) {
            return;
        }
        let id = input.account.clone();
        let serving = input.credits.as_ref().map(|c| c.serving);
        let at = input.observed_at;
        let transition = {
            let Some(mut state) = self.writable() else {
                return;
            };
            let Ok(spent) = state.usage.observe(input, Utc::now()) else {
                return;
            };
            serving.and_then(|serving| {
                if state.credits.len() >= 128 && !state.credits.contains_key(&id) {
                    return None;
                }
                let old = state.credits.insert(id.clone(), serving);
                (old != Some(serving) && (old.is_some() || serving)).then_some((serving, spent))
            })
        };
        if let Some((serving, spent)) = transition {
            self.activity(ActivityInput {
                ts: at,
                kind: if serving {
                    ActivityKind::CreditsStarted
                } else {
                    ActivityKind::CreditsStopped
                },
                provider: if id.as_str().starts_with("cl-") {
                    Provider::Claude
                } else {
                    Provider::Codex
                },
                account: Some(id),
                account_to: None,
                notable: true,
                data: ActivityData {
                    credits_used: spent,
                    ..Default::default()
                },
            });
        }
    }
    pub fn sessions(&self, range: SessionRange) -> SessionsView {
        self.sync();
        self.queue_lookups();
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .model
            .sessions(range, Utc::now())
    }
    pub fn detail(&self, id: &SessionId) -> Option<SessionDetail> {
        self.sync();
        self.queue_lookups();
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .model
            .detail(id, Utc::now())
    }
    pub fn activity_page(
        &self,
        since: Option<u64>,
        limit: usize,
        days: u8,
    ) -> Result<ActivityPage, Invalid> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activity
            .query(since, limit, days, Utc::now())
    }
    pub fn usage_page(&self, hours: u8) -> Result<UsageView, Invalid> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .usage
            .query(hours, Utc::now())
    }
    fn millis(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    /// Called on the ONE existing writer, including when its bounded command queue is full.
    pub(crate) fn tick(&self) {
        if self.frozen.load(Ordering::Acquire) {
            return;
        }
        {
            let mut last = self.last_tick.lock().unwrap_or_else(|e| e.into_inner());
            if last.elapsed() < std::time::Duration::from_secs(1) {
                return;
            }
            *last = Instant::now();
        }
        self.sync();
        let changes = {
            let Some(mut state) = self.writable() else {
                return;
            };
            state.activity.prune(Utc::now());
            state.usage.advance(Utc::now());
            state.model.take_changes(self.millis())
        };
        if let Some(changes) = changes {
            self.emit("sessions_changed", changes);
        }
        // One bounded filename scan/tick, off the request/runtime hot path.
        let lookup = self
            .lookups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        if let Some(lookup) = lookup {
            let root = self
                .lookup_root
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(label) = root.and_then(|root| super::resolver::scan(&root, &lookup.sid))
                && let Some(mut state) = self.writable()
            {
                state
                    .model
                    .evidence(&lookup.id, PrivateEvidence::new(Some(label), None), None);
            }
        }
    }
    pub(crate) fn revisions(&self) -> (u64, u64) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.activity.revision(), state.usage.revision())
    }
    pub(crate) fn archive(&self, activity: bool, usage: bool) -> Archive {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Archive {
            activity: activity.then(|| state.activity.clone()),
            usage: usage.then(|| state.usage.clone()),
        }
    }
    pub(crate) fn restore(&self, activity: ActivityLog, usage: UsageHistory) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.activity = activity;
        state.usage = usage;
    }
    pub(crate) fn freeze(&self) {
        {
            let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.frozen.store(true, Ordering::Release);
        }
        self.lookups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}
pub fn request_outcome(outcome: &str) -> RequestOutcome {
    match outcome {
        "ok" | "success" | "updated" | "completed" | "ws_complete" => RequestOutcome::Ok,
        "rate_hold_ok" => RequestOutcome::RateHoldOk,
        "client_drop" => RequestOutcome::ClientDrop,
        "rate_429" | "relayed_rate_429" | "rate_hold_relayed_429" => RequestOutcome::RelayedRate429,
        "network_error" => RequestOutcome::NetworkError,
        _ => RequestOutcome::UpstreamError,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn r13_forecast_runtime_rejects_future_and_restarts_cold() {
        let runtime = super::Runtime::new();
        let now = chrono::Utc::now();
        let start = now - chrono::Duration::minutes(10);
        for (minutes, used) in [(0, 10.0), (5, 12.0), (10, 14.0)] {
            runtime.observe_usage_forecast(
                "cx-123abc",
                "5h",
                used,
                Some(now + chrono::Duration::hours(2)),
                start + chrono::Duration::minutes(minutes),
            );
        }
        let forecast = runtime.usage_forecast("cx-123abc", now, Some(start));
        assert_eq!(forecast.windows[0].recent_pct_per_hour, Some(24.0));
        assert!(forecast.windows[0].changing);
        runtime.observe_usage_forecast(
            "cx-123abc",
            "5h",
            99.,
            None,
            now + chrono::Duration::hours(1),
        );
        assert_eq!(
            runtime.usage_forecast("cx-123abc", now, None).windows[0].used_pct,
            14.0
        );
        assert!(
            super::Runtime::new()
                .usage_forecast("cx-123abc", now, None)
                .windows
                .is_empty()
        );
    }
    use super::*;
    #[test]
    fn r101_activity_carries_c5_window_through_reached_extension_and_reset() {
        use crate::accounts::{Health, HealthReason, QuotaCooling};
        for name in ["5h", "7d", "43200m", "primary"] {
            let runtime = Runtime::new();
            let mut account = crate::accounts::health::tests::fixture();
            runtime.baseline_accounts(&[account.clone()]);
            let now = Utc::now();
            let reset = now + chrono::Duration::hours(2);
            account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: reset,
                scope: "account".into(),
            };
            account.quota_cooling = Some(QuotaCooling {
                window: name.into(),
                reset_at: reset,
                started_at: now,
            });
            runtime.account_health(&account, &Health::Ready);
            runtime.account_changed(&account);
            let old = account.health.clone();
            account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: reset + chrono::Duration::seconds(1),
                scope: "account".into(),
            };
            account.quota_cooling.as_mut().unwrap().reset_at += chrono::Duration::seconds(1);
            runtime.account_health(&account, &old);
            runtime.account_changed(&account);
            let old = account.health.clone();
            account.health = Health::Ready;
            account.quota_cooling = None;
            runtime.account_health(&account, &old);
            runtime.account_changed(&account);
            let events = runtime.activity_page(None, 100, 1).unwrap().events;
            assert_eq!(events.len(), 2, "extension must remain one episode");
            for kind in [ActivityKind::QuotaReached, ActivityKind::QuotaReset] {
                let event = events.iter().find(|e| e.kind == kind).unwrap();
                assert_eq!(event.data.window.as_ref().unwrap().as_str(), name);
                assert_eq!(
                    event.data.reset_at,
                    Some(if kind == ActivityKind::QuotaReset {
                        reset + chrono::Duration::seconds(1)
                    } else {
                        reset
                    })
                );
            }
        }
    }

    #[test]
    fn r101_activity_unknown_window_does_not_use_account_scope_or_hold_deadline() {
        use crate::accounts::{Health, HealthReason};
        let runtime = Runtime::new();
        let mut account = crate::accounts::health::tests::fixture();
        runtime.baseline_accounts(&[account.clone()]);
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: Utc::now() + chrono::Duration::minutes(5),
            scope: "account".into(),
        };
        runtime.account_health(&account, &Health::Ready);
        let events = runtime.activity_page(None, 100, 1).unwrap().events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ActivityKind::QuotaReached);
        assert!(events[0].data.window.is_none());
        assert!(events[0].data.reset_at.is_none());
    }

    #[test]
    fn r101_activity_restored_cooling_keeps_reset_window_and_unknown_stays_unknown() {
        use crate::accounts::{Health, HealthReason, QuotaCooling};
        for known in [false, true] {
            let runtime = Runtime::new();
            let mut account = crate::accounts::health::tests::fixture();
            let reset = Utc::now() + chrono::Duration::hours(1);
            account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: reset,
                scope: "account".into(),
            };
            account.quota_cooling = known.then(|| QuotaCooling {
                window: "43200m".into(),
                reset_at: reset,
                started_at: Utc::now(),
            });
            runtime.baseline_accounts(&[account.clone()]);
            let old = account.health.clone();
            account.health = Health::Ready;
            account.quota_cooling = None;
            runtime.account_health(&account, &old);
            let events = runtime.activity_page(None, 100, 1).unwrap().events;
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, ActivityKind::QuotaReset);
            assert_eq!(
                events[0].data.window.as_ref().map(WindowName::as_str),
                known.then_some("43200m")
            );
            assert_eq!(events[0].data.reset_at, known.then_some(reset));
        }
    }

    #[test]
    fn r10_quota_activity_suppresses_only_continuous_account_cooling() {
        use crate::accounts::{Health, HealthReason};
        let now = Utc::now();
        let quota = |scope: &str, until| Health::Cooling {
            reason: HealthReason::Quota,
            scope: scope.into(),
            until,
        };
        let deadline = now + chrono::Duration::hours(2);
        let account = quota("account", deadline);
        let extended = quota("account", deadline + chrono::Duration::milliseconds(1));
        let relogin = Health::NeedsRelogin {
            reason: "synthetic".into(),
            since: now,
        };
        for (from, to, expected) in [
            (
                Health::Ready,
                account.clone(),
                Some(ActivityKind::QuotaReached),
            ),
            (account.clone(), account.clone(), None),
            (account.clone(), extended, None),
            (
                quota("model:synthetic", deadline),
                account.clone(),
                Some(ActivityKind::QuotaReached),
            ),
            (
                quota("account", now - chrono::Duration::seconds(1)),
                account.clone(),
                Some(ActivityKind::QuotaReached),
            ),
            (
                account.clone(),
                Health::Ready,
                Some(ActivityKind::QuotaReset),
            ),
            (
                account.clone(),
                relogin.clone(),
                Some(ActivityKind::NeedsRelogin),
            ),
            (relogin, account.clone(), Some(ActivityKind::QuotaReached)),
            (
                Health::Cooling {
                    reason: HealthReason::LinkStale,
                    scope: "account".into(),
                    until: deadline,
                },
                account.clone(),
                Some(ActivityKind::QuotaReached),
            ),
            (account, quota("model:synthetic", deadline), None),
        ] {
            let runtime = Runtime::new();
            runtime.health("cx-123abc", &from, &to);
            let kinds: Vec<_> = runtime
                .activity_page(None, 100, 1)
                .unwrap()
                .events
                .into_iter()
                .map(|e| e.kind)
                .collect();
            assert_eq!(
                kinds,
                expected.into_iter().collect::<Vec<_>>(),
                "{from:?} -> {to:?}"
            );
        }
    }

    #[test]
    fn r10_quota_activity_preserves_same_reset_reentry_after_ready_or_relogin() {
        use crate::accounts::{Health, HealthReason};
        let now = Utc::now();
        let quota = Health::Cooling {
            reason: HealthReason::Quota,
            scope: "account".into(),
            until: now + chrono::Duration::hours(2),
        };
        for middle in [
            Health::Ready,
            Health::NeedsRelogin {
                reason: "synthetic".into(),
                since: now,
            },
        ] {
            let runtime = Runtime::new();
            runtime.health("cx-123abc", &Health::Ready, &quota);
            runtime.health("cx-123abc", &quota, &middle);
            runtime.health("cx-123abc", &middle, &quota);
            let events = runtime.activity_page(None, 100, 1).unwrap().events;
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e.kind == ActivityKind::QuotaReached)
                    .count(),
                2
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e.kind == ActivityKind::QuotaReset)
                    .count(),
                usize::from(middle == Health::Ready)
            );
        }
    }

    #[test]
    fn r7_stale_empty_snapshot_cannot_remove_ensured_row_before_request_start() {
        let home = tempfile::tempdir().unwrap();
        let config = crate::config::Config::default();
        let table = BindingTable::new(home.path(), config.routing.clone()).unwrap();
        let runtime = Runtime::new();
        runtime.attach(&table, &config);
        let generation = runtime.snapshot_generation();
        let now = Utc::now();
        let rows = table.dashboard_snapshot(now);
        assert!(rows.is_empty());
        let (release, held) = std::sync::mpsc::sync_channel(0);
        let old_reader = runtime.clone();
        let merger = std::thread::spawn(move || {
            held.recv().unwrap();
            old_reader.merge_snapshot(generation, rows, now)
        });
        let key = "codex:snapshot-race-fixture";
        let id = SessionId::from_key(key);
        table.import(serde_json::json!({"schema":1,"bindings":[{"key":key,"account":"cx-123abc","last_used":Utc::now()}]})).unwrap();
        assert_eq!(runtime.ensure(key, None), Some(id.clone()));
        runtime.evidence(
            key,
            &HeaderMap::new(),
            labels::MemoryLabel::new("preserved", labels::LabelSource::Launcher),
        );
        // Reproduce the vulnerable interval: ensure inserted the row, but no
        // request/socket handle exists yet to protect it from sync_bindings.
        assert!(
            runtime.state.lock().unwrap().model.sessions[&id]
                .live
                .is_empty()
        );
        release.send(()).unwrap();
        assert!(!merger.join().unwrap());
        assert_eq!(
            runtime
                .state
                .lock()
                .unwrap()
                .model
                .detail(&id, Utc::now())
                .unwrap()
                .session
                .label
                .unwrap()
                .text,
            "preserved"
        );
        let request = runtime
            .request(key, &HeaderMap::new(), None, Utc::now(), Transport::Http)
            .unwrap();
        let pacing = request.pacing();
        // A current snapshot can still merge; the stale one cannot replay.
        let current = runtime.snapshot_generation();
        let rows = table.dashboard_snapshot(Utc::now());
        assert!(runtime.merge_snapshot(current, rows.clone(), Utc::now()));
        assert!(!runtime.merge_snapshot(current, rows, Utc::now()));
        let detail = runtime
            .state
            .lock()
            .unwrap()
            .model
            .detail(&id, Utc::now())
            .unwrap();
        assert_eq!(detail.session.inflight.requests, 1);
        assert_eq!(detail.session.pacing_wait.unwrap().waiting, 1);
        assert_eq!(detail.session.requests_today, 1);
        assert_eq!(detail.session.label.unwrap().text, "preserved");
        drop(pacing);
        request.finish(RequestEnd {
            at: Utc::now(),
            outcome: RequestOutcome::Ok,
            status: 200,
            ttfb_ms: 1,
            dur_ms: 2,
            rate_wait_ms: 0,
            rate_retries: 0,
            failover: None,
        });
        let detail = runtime
            .state
            .lock()
            .unwrap()
            .model
            .detail(&id, Utc::now())
            .unwrap();
        assert_eq!(detail.session.inflight.requests, 0);
        assert_eq!(detail.session.requests_today, 1);
        assert_eq!(detail.recent_requests.len(), 1);
    }
    #[test]
    fn r7_default_constructor_never_discovers_profile_and_filename_lookup_is_lazy_explicit() {
        let base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r7-filename-tests");
        std::fs::create_dir_all(&base).unwrap();
        let temp = tempfile::tempdir_in(std::fs::canonicalize(base).unwrap()).unwrap();
        let projects = temp.path().join("projects");
        std::fs::create_dir_all(projects.join("fixture-private-folder")).unwrap();
        std::fs::write(
            projects.join("fixture-private-folder/filename-fixture.jsonl"),
            [0xff, 0xfe],
        )
        .unwrap();
        let config = crate::config::Config::default();
        assert!(config.dashboard.resolve_claude_project);
        let table = BindingTable::new(&temp.path().join("home"), config.routing.clone()).unwrap();
        let key = "claude:filename-fixture";
        let id = SessionId::from_key(key);
        table.import(serde_json::json!({"schema":1,"bindings":[{"key":key,"account":"cl-123abc","last_used":Utc::now()}]})).unwrap();
        let runtime = Runtime::new();
        runtime.attach(&table, &config);
        runtime.sessions(SessionRange::All);
        runtime.detail(&id);
        assert!(runtime.lookup_root.lock().unwrap().is_none());
        assert!(runtime.lookups.lock().unwrap().is_empty());
        runtime
            .configure_claude_project_root(Some(projects))
            .unwrap();
        let request = runtime
            .request(key, &HeaderMap::new(), None, Utc::now(), Transport::Http)
            .unwrap();
        // Even an explicitly configured scanner is never scheduled by traffic.
        assert!(runtime.lookups.lock().unwrap().is_empty());
        assert!(
            runtime
                .state
                .lock()
                .unwrap()
                .model
                .detail(&id, Utc::now())
                .unwrap()
                .session
                .label
                .is_none()
        );
        runtime.detail(&id);
        assert_eq!(runtime.lookups.lock().unwrap().len(), 1);
        *runtime.last_tick.lock().unwrap() = Instant::now() - std::time::Duration::from_secs(1);
        runtime.tick();
        assert_eq!(
            runtime.detail(&id).unwrap().session.label.unwrap().text,
            "folder"
        );
        drop(request);
    }
    #[test]
    fn r7_guard_drop_finishes_once_and_pacing_guard_cannot_resurrect_request() {
        let runtime = Runtime::new();
        let now = Utc::now();
        let id = SessionId::from_key("codex:guard-fixture");
        {
            let mut state = runtime.writable().unwrap();
            state
                .model
                .upsert_binding(
                    BindingGroup {
                        id: id.clone(),
                        provider: Provider::Codex,
                        account: "cx-123abc".to_owned().try_into().unwrap(),
                        first_seen_at: Some(now),
                        bound_since: Some(now),
                        last_used_at: now,
                        active: true,
                        marked_for_move: false,
                        client_kind: None,
                        restored: false,
                    },
                    now,
                )
                .unwrap();
            state
                .model
                .start(
                    &id,
                    RequestStart {
                        id: 1,
                        at: now,
                        transport: Transport::Http,
                    },
                )
                .unwrap();
        }
        let guard = RequestGuard {
            runtime: runtime.clone(),
            id: 1,
            session: id.clone(),
            move_at: None,
        };
        let pacing = guard.pacing();
        assert_eq!(
            runtime
                .detail(&id)
                .unwrap()
                .session
                .pacing_wait
                .unwrap()
                .waiting,
            1
        );
        drop(guard);
        drop(pacing);
        let detail = runtime.detail(&id).unwrap();
        assert_eq!(detail.session.inflight.requests, 0);
        assert!(detail.session.pacing_wait.is_none());
        assert_eq!(detail.recent_requests.len(), 1);
        assert_eq!(
            detail.recent_requests[0].outcome,
            RequestOutcome::ClientDrop
        );
        runtime.freeze();
        let revisions = runtime.revisions();
        runtime.pace(1, Some(now));
        runtime.account_event(
            ActivityKind::Login,
            Provider::Codex,
            Some("cx-123abc"),
            None,
            None,
        );
        assert_eq!(runtime.revisions(), revisions);
    }
}

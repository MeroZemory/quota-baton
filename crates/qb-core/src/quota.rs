//! Active usage observations. Polls share the relay credential gates and client.
mod parse;
mod profile;

use crate::{
    account_runtime::AccountRuntime,
    accounts::{Account, AccountStore, Health, HealthReason, Identity, Provider, UserState},
    config::Config,
    observability::Observability,
    relay::credentials::TokenGates,
};
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Semaphore, time::Instant};

#[cfg(test)]
pub(crate) fn parse_snapshot_for_test(
    provider: Provider,
    body: &[u8],
    now: DateTime<Utc>,
) -> Option<serde_json::Value> {
    parse::snapshot(provider, body, now)
}

#[cfg(test)]
pub(crate) fn is_complete_snapshot_for_test(provider: Provider, body: &[u8]) -> bool {
    parse::is_complete_snapshot(provider, body)
}

const MAX_BODY: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum PollError {
    TooSoon { retry_after: u64 },
    Excluded,
    NotFound,
    Unavailable,
}

#[derive(Default)]
struct Clock {
    next: Option<Instant>,
    failure_until: Option<Instant>,
    last: Option<Instant>,
    failures: u32,
    running: bool,
    disabled_until: Option<Instant>,
    profile: profile::ProfileClock,
    placement_last: Option<Instant>,
    placement_polled_through: Option<u64>,
}

pub struct UsagePoller {
    store: Arc<AccountStore>,
    runtime: Arc<AccountRuntime>,
    obs: Arc<Observability>,
    client: reqwest::Client,
    tokens: Mutex<TokenGates>,
    bases: [String; 2],
    min_interval: u64,
    placement_min_interval: u64,
    clocks: Mutex<BTreeMap<String, Clock>>,
    tray_until: Mutex<Option<Instant>>,
    event_streams: AtomicUsize,
    semaphore: Semaphore,
    profile_semaphore: Semaphore,
    bindings: Mutex<Option<Weak<crate::bindings::BindingTable>>>,
}

pub(crate) struct EventGuard(Weak<UsagePoller>);
impl Drop for EventGuard {
    fn drop(&mut self) {
        if let Some(poller) = self.0.upgrade() {
            poller.event_streams.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

struct Reservation<'a> {
    poller: &'a UsagePoller,
    id: String,
    succeeded: bool,
    unsupported: bool,
    started: bool,
    placement: bool,
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut clocks = self.poller.clocks.lock().unwrap_or_else(|e| e.into_inner());
        let clock = clocks.entry(self.id.clone()).or_default();
        clock.running = false;
        if !self.started {
            return;
        }
        clock.failures = if self.succeeded {
            0
        } else {
            clock.failures.saturating_add(1)
        };
        let delay = interval(
            self.poller.min_interval,
            clock.failures,
            rand::random::<f64>(),
        );
        let next = Instant::now() + Duration::from_secs_f64(delay);
        clock.failure_until = (!self.succeeded).then_some(next);
        if !self.placement {
            clock.next = Some(next);
        }
        if self.unsupported {
            clock.disabled_until = Some(Instant::now() + Duration::from_secs(6 * 3600));
        }
    }
}

fn interval(min: u64, failures: u32, jitter: f64) -> f64 {
    (min as f64 * 2f64.powi(failures.min(16) as i32) * (0.9 + jitter * 0.2))
        .max(min as f64)
        .min(3600.0)
}

impl UsagePoller {
    pub(crate) fn new(
        store: Arc<AccountStore>,
        runtime: Arc<AccountRuntime>,
        obs: Arc<Observability>,
        client: reqwest::Client,
        tokens: TokenGates,
        config: &Config,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            runtime,
            obs,
            client,
            tokens: Mutex::new(tokens),
            bases: [
                config
                    .upstream
                    .claude_usage_base
                    .trim_end_matches('/')
                    .into(),
                config
                    .upstream
                    .codex_usage_base
                    .trim_end_matches('/')
                    .into(),
            ],
            min_interval: config.quota.poll_min_interval_secs.max(1),
            placement_min_interval: config.routing.placement_poll_min_interval_secs,
            clocks: Mutex::default(),
            tray_until: Mutex::default(),
            event_streams: AtomicUsize::new(0),
            semaphore: Semaphore::new(2),
            profile_semaphore: Semaphore::new(1),
            bindings: Mutex::new(None),
        })
    }
    pub(crate) fn set_bindings(&self, bindings: &Arc<crate::bindings::BindingTable>) {
        *self.bindings.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(bindings));
    }
    fn binding_table(&self) -> Option<Arc<crate::bindings::BindingTable>> {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
    }
    fn placement_due(&self, id: &str, now: DateTime<Utc>) -> Option<u64> {
        let table = self.binding_table()?;
        // This is the merged per-window snapshot used by AccountRuntime too.
        // A fresh outer timestamp cannot acknowledge a stale/missing window.
        if let Some(snapshot) = self.obs.quota_snapshot(id) {
            table.note_usage_observation(id, &snapshot);
        }
        let through = self
            .clocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .and_then(|clock| clock.placement_polled_through);
        table
            .due_placement_poll(id, now, through)
            .map(|due| due.placement_seq)
    }
    #[cfg(debug_assertions)]
    pub(crate) fn set_tokens(&self, tokens: TokenGates) {
        *self.tokens.lock().unwrap_or_else(|e| e.into_inner()) = tokens;
    }
    pub fn mark_tray_open(&self) {
        *self.tray_until.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Instant::now() + Duration::from_secs(120));
    }
    fn tray_open(&self) -> bool {
        if self.event_streams.load(Ordering::Relaxed) > 0 {
            return true;
        }
        self.tray_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| until > Instant::now())
    }
    pub(crate) fn open_events(self: &Arc<Self>) -> EventGuard {
        self.mark_tray_open();
        self.event_streams.fetch_add(1, Ordering::Relaxed);
        EventGuard(Arc::downgrade(self))
    }
    fn reserve(&self, id: &str) -> Result<Reservation<'_>, PollError> {
        self.reserve_reason(id, false)
    }
    fn reserve_reason(&self, id: &str, placement: bool) -> Result<Reservation<'_>, PollError> {
        self.reserve_reason_at(id, placement, Instant::now())
    }
    fn reserve_reason_at(
        &self,
        id: &str,
        placement: bool,
        now: Instant,
    ) -> Result<Reservation<'_>, PollError> {
        let mut clocks = self.clocks.lock().unwrap_or_else(|e| e.into_inner());
        let clock = clocks.entry(id.into()).or_default();
        if placement && let Some(last) = clock.placement_last {
            let interval = Duration::from_secs(self.placement_min_interval);
            let elapsed = now.saturating_duration_since(last);
            if elapsed < interval {
                return Err(PollError::TooSoon {
                    retry_after: interval.saturating_sub(elapsed).as_secs().saturating_add(1),
                });
            }
        }
        let next = if placement {
            clock
                .disabled_until
                .into_iter()
                .chain(clock.failure_until)
                .max()
        } else {
            clock
                .next
                .into_iter()
                .chain(clock.failure_until)
                .chain(clock.disabled_until)
                .max()
        };
        if clock.running || next.is_some_and(|next| next > now) {
            return Err(PollError::TooSoon {
                retry_after: next.map_or(self.min_interval, |next| {
                    next.saturating_duration_since(now)
                        .as_secs()
                        .saturating_add(1)
                }),
            });
        }
        clock.running = true;
        Ok(Reservation {
            poller: self,
            id: id.into(),
            succeeded: false,
            unsupported: false,
            started: false,
            placement,
        })
    }
    async fn load(&self, id: &str) -> Result<Account, PollError> {
        let store = self.store.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || store.load(&id))
            .await
            .map_err(|_| PollError::Unavailable)?
            .map_err(|error| match error {
                crate::accounts::StoreError::NotFound => PollError::NotFound,
                _ => PollError::Unavailable,
            })
    }
    /// Reconcile linked credentials under the core's lease before binding listeners.
    pub async fn startup_reload(&self) -> Result<(), PollError> {
        let store = self.store.clone();
        let accounts = tokio::task::spawn_blocking(move || store.list())
            .await
            .map_err(|_| PollError::Unavailable)?
            .map_err(|_| PollError::Unavailable)?;
        let gates = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for account in accounts {
            self.queue_profile(&account);
            if crate::relay::credentials::is_linked(&account) {
                gates
                    .reload_linked(self.store.clone(), self.obs.clone(), account.id, "startup")
                    .await
                    .map_err(|_| PollError::Unavailable)?;
            }
        }
        Ok(())
    }

    async fn poll_after_401(
        &self,
        gates: &TokenGates,
        account: Account,
        tokens: &crate::accounts::Tokens,
        body: &[u8],
        attempt: u8,
    ) -> Result<Option<(Account, crate::accounts::Tokens)>, PollError> {
        if !crate::relay::credentials::is_linked(&account)
            && crate::relay::poll_revoked(account.provider, body)
            && gates
                .is_current(self.store.clone(), &account, tokens)
                .await
                .map_err(|_| PollError::Unavailable)?
        {
            gates
                .revoke_if_current(self.store.clone(), &account, tokens)
                .await
                .map_err(|_| PollError::Unavailable)?;
            return Ok(None);
        }
        if attempt == 0 {
            let gates_task = gates.clone();
            let store = self.store.clone();
            let obs = self.obs.clone();
            let failed_account = account.clone();
            let failed_tokens = crate::accounts::Tokens {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                id_token: tokens.id_token.clone(),
            };
            // The refresh and token-conditional failure transition both
            // survive cancellation of the three-second request hook.
            if let Some((updated, refreshed)) = tokio::spawn(async move {
                let retry = gates_task
                    .after_401(store.clone(), obs, failed_account.clone(), &failed_tokens)
                    .await?;
                if retry.is_none() {
                    gates_task
                        .cool_unavailable_auth(store, &failed_account, &failed_tokens)
                        .await?;
                }
                Ok::<_, ()>(retry)
            })
            .await
            .map_err(|_| PollError::Unavailable)?
            .map_err(|_| PollError::Unavailable)?
            {
                return Ok(Some((updated, refreshed)));
            }
        } else {
            gates
                .auth_failed(
                    self.store.clone(),
                    self.obs.clone(),
                    account.clone(),
                    tokens,
                )
                .await
                .map_err(|_| PollError::Unavailable)?;
        }
        Ok(None)
    }

    pub async fn poll(&self, id: &str) -> Result<Account, PollError> {
        self.poll_reason(id, None).await
    }
    async fn poll_reason(&self, id: &str, placement: Option<u64>) -> Result<Account, PollError> {
        self.poll_reason_observed(id, placement, false)
            .await
            .map(|(account, _)| account)
    }
    pub(crate) async fn poll_ws_quota(
        &self,
        id: &str,
    ) -> Result<(Account, Option<crate::account_runtime::ObservedUsagePoll>), PollError> {
        self.poll_reason_observed(id, None, true).await
    }
    async fn poll_reason_observed(
        &self,
        id: &str,
        placement: Option<u64>,
        capture_receipt: bool,
    ) -> Result<(Account, Option<crate::account_runtime::ObservedUsagePoll>), PollError> {
        let account = self.load(id).await?;
        if account.user_state == UserState::Disabled
            || (matches!(account.health, Health::NeedsRelogin { .. })
                && !crate::relay::credentials::is_linked(&account))
        {
            return Err(PollError::Excluded);
        }
        let base = &self.bases[usize::from(account.provider == Provider::Codex)];
        // Test builds require an explicit synthetic usage base; legacy relay tests
        // must never dial a production endpoint from their new row-14 hook.
        #[cfg(any(test, all(feature = "testkit", debug_assertions)))]
        if !base.starts_with("http://127.0.0.1:") {
            return Err(PollError::Unavailable);
        }
        let mut reservation = if placement.is_some() {
            self.reserve_reason(id, true)?
        } else {
            self.reserve(id)?
        };
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| PollError::Unavailable)?;
        let gates = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if crate::relay::credentials::is_linked(&account) {
            gates
                .reload_linked(
                    self.store.clone(),
                    self.obs.clone(),
                    id.into(),
                    "before_poll",
                )
                .await
                .map_err(|_| PollError::Unavailable)?;
        }
        let (mut account, mut tokens) = gates
            .before_request(self.store.clone(), self.obs.clone(), id.into())
            .await
            .map_err(|_| PollError::Unavailable)?;
        // User/admin changes while queued take precedence over polling.
        if account.user_state == UserState::Disabled
            || matches!(account.health, Health::NeedsRelogin { .. })
        {
            return Err(PollError::Excluded);
        }
        let probe = self
            .runtime
            .begin_probe(&account)
            .map_err(|_| PollError::Unavailable)?;
        if placement.is_some() && self.placement_due(id, Utc::now()).is_none() {
            // Headers arriving while semaphore/token gates were queued cancel it.
            return Err(PollError::Excluded);
        }
        if let Some(through) = placement {
            let mut clocks = self.clocks.lock().unwrap_or_else(|e| e.into_inner());
            let clock = clocks.get_mut(id).expect("reserved clock");
            clock.placement_last = Some(Instant::now());
            clock.placement_polled_through = Some(through);
        }
        if placement.is_some() {
            self.obs.record_placement_poll(id);
        }
        reservation.started = true;
        self.clocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
            .expect("reserved clock")
            .last = Some(Instant::now());
        for attempt in 0..2 {
            let observation_table = self.binding_table();
            let observation_asof = observation_table
                .as_ref()
                .map(|table| table.begin_usage_observation_for(id));
            if attempt == 0
                && placement.is_some()
                && let (Some(table), Some(asof)) = (&observation_table, observation_asof)
            {
                table.note_placement_poll_started(id, asof.sequence);
            }
            let path = if account.provider == Provider::Codex {
                "/backend-api/wham/usage"
            } else {
                "/api/oauth/usage"
            };
            let mut request = self
                .client
                .get(format!("{base}{path}"))
                .timeout(Duration::from_secs(30))
                .bearer_auth(&tokens.access_token)
                .header("Accept", "application/json")
                .header(
                    "User-Agent",
                    concat!("quota-baton/", env!("CARGO_PKG_VERSION")),
                );
            if let Identity::Codex { account_id, .. } = &account.identity {
                request = request.header("ChatGPT-Account-Id", account_id);
            }
            if account.provider == Provider::Claude {
                request = request.header("anthropic-beta", "oauth-2025-04-20");
            }
            let wait_origin = self.runtime.capture_poll_origin(id).await;
            let outside_origin = self.runtime.capture_outside_poll_origin(id);
            let mut response = request.send().await.map_err(|_| PollError::Unavailable)?;
            let status = response.status();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| PollError::Unavailable)? {
                if body.len() + chunk.len() > MAX_BODY {
                    return Err(PollError::Unavailable);
                }
                body.extend_from_slice(&chunk);
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                if let Some((updated, refreshed)) = self
                    .poll_after_401(&gates, account, &tokens, &body, attempt)
                    .await?
                {
                    account = updated;
                    tokens = refreshed;
                    continue;
                }
                return Err(PollError::Unavailable);
            }
            self.obs.capture_poll(account.provider, &body);
            if account.provider == Provider::Claude && status == reqwest::StatusCode::NOT_FOUND {
                reservation.unsupported = true;
                tracing::warn!(
                    account = id,
                    "Claude usage endpoint unavailable; polling disabled for six hours"
                );
                return Err(PollError::Unavailable);
            }
            if !status.is_success() {
                return Err(PollError::Unavailable);
            }
            let Some(snapshot) = parse::snapshot(account.provider, &body, Utc::now()) else {
                if account.provider == Provider::Claude {
                    reservation.unsupported = true;
                    tracing::warn!(
                        account = id,
                        "Claude usage shape unavailable; polling disabled for six hours"
                    );
                }
                return Err(PollError::Unavailable);
            };
            let replace_windows = parse::is_complete_snapshot(account.provider, &body);
            if let (Some(table), Some(asof)) = (&observation_table, observation_asof) {
                if replace_windows {
                    table.replace_usage_windows_asof_token(id, &snapshot, asof);
                } else {
                    table.note_usage_observation_asof_token(id, &snapshot, asof);
                }
            }
            let reported = snapshot.clone();
            let snapshot = self
                .obs
                .observe_poll(id, snapshot, replace_windows)
                .ok_or(PollError::Unavailable)?;
            if probe.is_some() {
                self.runtime
                    .poll_probe_success(id)
                    .await
                    .map_err(|_| PollError::Unavailable)?;
            }
            let (account, receipt) =
                if capture_receipt && account.provider == Provider::Codex && replace_windows {
                    self.runtime
                        .apply_polled_quota_snapshot_observed(
                            id,
                            snapshot,
                            &reported,
                            wait_origin,
                            outside_origin,
                        )
                        .await
                        .map_err(|_| PollError::Unavailable)?
                } else {
                    let account = self
                        .runtime
                        .apply_polled_quota_snapshot(
                            id,
                            snapshot,
                            &reported,
                            wait_origin,
                            outside_origin,
                        )
                        .await
                        .map_err(|_| PollError::Unavailable)?;
                    (account, None)
                };
            reservation.succeeded = true;
            return Ok((account, receipt));
        }
        Err(PollError::Unavailable)
    }
    fn scheduled(&self, account: &Account) -> bool {
        if account.user_state == UserState::Disabled
            || matches!(account.health, Health::NeedsRelogin { .. })
        {
            return false;
        }
        let hourly = matches!(
            account.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ) && self
            .clocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&account.id)
            .and_then(|c| c.last)
            .is_none_or(|last| Instant::now().duration_since(last) >= Duration::from_secs(3600));
        let stale = self
            .obs
            .quota_snapshot(&account.id)
            .and_then(|s| {
                s["observed_at"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            })
            .is_none_or(|at| (Utc::now() - at.with_timezone(&Utc)).num_seconds() > 600);
        let candidate = account.user_state == UserState::Active
            && (account.eligible(None) || matches!(account.health, Health::Cooling { .. }));
        hourly || (stale && (candidate || self.tray_open()))
    }
    pub async fn tick(&self) {
        let store = self.store.clone();
        let Ok(Ok(accounts)) = tokio::task::spawn_blocking(move || store.list()).await else {
            // A transient read failure must not erase daily profile deadlines.
            return;
        };
        {
            let ids: std::collections::BTreeSet<_> =
                accounts.iter().map(|a| a.id.as_str()).collect();
            self.clocks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|id, clock| {
                    clock.running || clock.profile.running || ids.contains(id.as_str())
                });
        }
        let mut pending = futures_util::stream::FuturesUnordered::new();
        for account in accounts {
            let placement = self.placement_due(&account.id, Utc::now());
            let usage = self.scheduled(&account);
            let profile = self.profile_due(&account);
            if usage || placement.is_some() || profile {
                pending.push(async move {
                    if placement.is_some() {
                        if matches!(
                            self.poll_reason(&account.id, placement).await,
                            Err(PollError::TooSoon { .. })
                        ) && usage
                        {
                            let _ = self.poll(&account.id).await;
                        }
                    } else if usage {
                        let _ = self.poll(&account.id).await;
                    }
                    if profile {
                        let _ = self.fetch_profile(&account.id).await;
                    }
                });
            }
        }
        use futures_util::StreamExt;
        while pending.next().await.is_some() {}
    }
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(poller) = weak.upgrade() else {
                    return;
                };
                poller.tick().await;
                drop(poller);
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        })
    }
    pub async fn before_unavailable(&self, provider: Provider) {
        let store = self.store.clone();
        let work = async {
            let accounts = tokio::task::spawn_blocking(move || store.list())
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            let mut pending = futures_util::stream::FuturesUnordered::new();
            for account in accounts {
                if account.provider == provider
                    && account.user_state == UserState::Active
                    && !matches!(account.health, Health::NeedsRelogin { .. })
                {
                    pending.push(async move {
                        let _ = self.poll(&account.id).await;
                    });
                }
            }
            use futures_util::StreamExt;
            while pending.next().await.is_some() {}
        };
        let _ = tokio::time::timeout(Duration::from_secs(3), work).await;
    }
}

#[cfg(all(feature = "shape-capture", debug_assertions))]
pub(crate) fn poll_shape(provider: Provider, body: &[u8]) -> serde_json::Value {
    parse::shape(provider, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::{StoreError, Tokens},
        debug_pin::DebugPins,
        relay::RelayState,
    };
    use qb_testkit::{ScriptedResponse, ScriptedUpstream};
    use serde_json::json;
    #[cfg(all(feature = "shape-capture", debug_assertions))]
    #[test]
    fn poll_capture_persists_only_keys_and_allowlisted_usage_values() {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/usage-capture-tests");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let obs = Observability::new(
            root.path(),
            Some(crate::observability::CaptureOptions {
                dir: root.path().into(),
                limit: 1,
            }),
        )
        .unwrap();
        obs.capture_poll(Provider::Codex, br#"{"rate_limit":{"primary_window":{"used_percent":10,"opaque":"synthetic-secret"}},"access_token":"synthetic-secret","email":"synthetic@example.invalid"}"#);
        obs.flush().unwrap();
        let capture = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("shape-")
            })
            .unwrap();
        let text = std::fs::read_to_string(capture).unwrap();
        assert!(text.contains("rate_limit.primary_window.used_percent"));
        assert!(text.contains("\"poll\""));
        assert!(!text.contains("synthetic-secret"));
        assert!(!text.contains("synthetic@example.invalid"));
    }
    struct Fixture {
        _root: tempfile::TempDir,
        fake: ScriptedUpstream,
        relay: Arc<RelayState>,
        ids: Vec<String>,
    }
    impl Fixture {
        async fn new(count: usize) -> Self {
            Self::with_provider(count, Provider::Codex).await
        }
        async fn with_provider(count: usize, provider: Provider) -> Self {
            let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(".sandbox/usage-unit-tests");
            std::fs::create_dir_all(&base).unwrap();
            let root = tempfile::tempdir_in(base).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            let store = loop {
                match AccountStore::acquire(root.path()) {
                    Ok(store) => break Arc::new(store),
                    Err(StoreError::LeaseBusy) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("synthetic lease: {error}"),
                }
            };
            let mut ids = Vec::new();
            for n in 0..count {
                let mut a = crate::accounts::health::tests::fixture();
                a.provider = provider;
                a.identity = match provider {
                    Provider::Codex => Identity::Codex {
                        user_id: None,
                        account_id: format!("synthetic-usage-{n}"),
                        email: format!("usage-{n}@example.invalid"),
                    },
                    Provider::Claude => Identity::Claude {
                        account_uuid: format!("synthetic-claude-{n}"),
                        organization_uuid: Some("synthetic-organization".into()),
                        email: format!("usage-{n}@example.invalid"),
                    },
                };
                a.id = a.identity.id();
                a.token_meta.expires_at = Some(Utc::now() + chrono::Duration::days(1));
                let tokens = Tokens {
                    access_token: "synthetic-usage-access".into(),
                    refresh_token: "synthetic-usage-refresh".into(),
                    id_token: None,
                };
                ids.push(store.save(&a, &tokens, None).unwrap().id);
            }
            let fake = ScriptedUpstream::start().await.unwrap();
            let mut config = Config::default();
            config.upstream.codex_usage_base = fake.url.clone();
            config.upstream.claude_usage_base = fake.url.clone();
            let obs = Arc::new(Observability::new(root.path(), None).unwrap());
            let relay = Arc::new(
                RelayState::new(store, &config, obs, Arc::new(DebugPins::default())).unwrap(),
            );
            Self {
                _root: root,
                fake,
                relay,
                ids,
            }
        }
        fn account(&self, n: usize) -> Account {
            self.relay.quota.store.load(&self.ids[n]).unwrap()
        }
        fn usage() -> ScriptedResponse {
            ScriptedResponse::json(
                reqwest::StatusCode::OK,
                r#"{"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":3600}}}"#,
            )
        }
    }

    #[tokio::test]
    async fn outage_poll_preserves_named_bucket_without_inventing_duration() {
        for slot in ["primary", "secondary"] {
            for numeric_only in [false, true] {
                let f = Fixture::new(1).await;
                let until = DateTime::from_timestamp(
                    (Utc::now() + chrono::Duration::hours(1)).timestamp(),
                    0,
                )
                .unwrap();
                let mut rate = json!({});
                rate[format!("{slot}_window")] = json!({"reset_at": until.timestamp()});
                if numeric_only {
                    rate[format!("{slot}_window")]["used_percent"] = json!(100);
                } else {
                    rate["limit_reached"] = json!(true);
                    rate["active_limit"] = json!(slot);
                }
                f.fake.state.push(ScriptedResponse::json(
                    reqwest::StatusCode::OK,
                    json!({"rate_limit": rate}).to_string(),
                ));
                f.relay.quota.poll(&f.ids[0]).await.unwrap();
                assert!(
                    matches!(f.account(0).health,
                    Health::Cooling {reason: HealthReason::Quota, until: actual, ref scope}
                    if actual == until && scope == "account"),
                    "{slot} numeric={numeric_only}"
                );
                let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
                assert_eq!(snapshot["windows"].as_array().unwrap().len(), 1);
                assert_eq!(snapshot["windows"][0]["name"], slot);
            }
        }
    }

    #[tokio::test]
    async fn outage_poll_rejects_explicit_invalid_duration_placeholders() {
        for duration in [json!(0), json!(null), json!("opaque"), json!(-1)] {
            let f = Fixture::new(1).await;
            f.fake.state.push(ScriptedResponse::json(
                reqwest::StatusCode::OK,
                json!({"plan_type": "plus", "rate_limit": {
                    "primary_window": {"limit_window_seconds": duration, "used_percent": 100,
                        "reset_at": (Utc::now() + chrono::Duration::hours(1)).timestamp()}
                }})
                .to_string(),
            ));
            f.relay.quota.poll(&f.ids[0]).await.unwrap();
            assert!(matches!(f.account(0).health, Health::Ready));
            let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
            assert!(snapshot["windows"].as_array().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn outage_usage_poll_success_does_not_complete_serving_probe() {
        let f = Fixture::new(1).await;
        let id = &f.ids[0];
        f.relay
            .quota
            .store
            .update_metadata(id, |a| {
                a.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: Utc::now() + chrono::Duration::hours(1),
                    scope: "account".into(),
                };
            })
            .unwrap();
        f.relay.accounts.release_quota_hold(id).await.unwrap();
        f.fake.state.push(ScriptedResponse::json(
            reqwest::StatusCode::OK,
            r#"{"plan_type":"pro"}"#,
        ));
        f.relay.quota.poll(id).await.unwrap();
        let account = f.account(0);
        let serving = f
            .relay
            .accounts
            .begin_probe(&account)
            .unwrap()
            .expect("usage GET does not prove serving success");
        assert!(f.relay.accounts.begin_probe(&account).is_err());
        drop(serving);
        assert!(f.relay.accounts.begin_probe(&account).unwrap().is_some());
    }

    #[test]
    fn jitter_backoff_is_bounded_and_increases_until_one_hour() {
        assert_eq!(interval(300, 0, 0.0), 300.0);
        assert_eq!(interval(300, 0, 1.0), 330.0);
        assert_eq!(interval(300, 1, 0.5), 600.0);
        assert_eq!(interval(300, 2, 0.5), 1200.0);
        assert_eq!(interval(300, u32::MAX, 1.0), 3600.0);
    }

    #[tokio::test]
    async fn scheduler_candidates_tray_hourly_and_terminal_exclusions() {
        let f = Fixture::new(1).await;
        let poller = &f.relay.quota;
        let mut a = f.account(0);
        assert!(poller.scheduled(&a));
        a.user_state = UserState::Paused;
        assert!(!poller.scheduled(&a));
        poller.mark_tray_open();
        assert!(poller.scheduled(&a));
        *poller.tray_until.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
        let stream = poller.open_events();
        *poller.tray_until.lock().unwrap() = None;
        assert!(poller.scheduled(&a));
        drop(stream);
        assert!(!poller.scheduled(&a));
        a.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: Utc::now() + chrono::Duration::hours(4),
            scope: "account".into(),
        };
        // A recent passive snapshot does not postpone the hourly cooling poll.
        poller
            .obs
            .observe_poll(
                &a.id,
                json!({"windows":[],"source":"poll","observed_at":Utc::now()}),
                true,
            )
            .unwrap();
        poller.clocks.lock().unwrap().insert(
            a.id.clone(),
            Clock {
                last: Some(Instant::now() - Duration::from_secs(3601)),
                ..Clock::default()
            },
        );
        assert!(poller.scheduled(&a));
        poller.clocks.lock().unwrap().get_mut(&a.id).unwrap().last = Some(Instant::now());
        assert!(!poller.scheduled(&a));
        a.user_state = UserState::Disabled;
        poller.mark_tray_open();
        assert!(!poller.scheduled(&a));
        a.user_state = UserState::Active;
        a.health = Health::NeedsRelogin {
            reason: "synthetic".into(),
            since: Utc::now(),
        };
        assert!(!poller.scheduled(&a));
    }

    #[tokio::test]
    async fn reservations_handle_cancel_backoff_and_successful_minimum_interval() {
        let f = Fixture::new(1).await;
        let poller = &f.relay.quota;
        let mut reservation = poller.reserve(&f.ids[0]).unwrap();
        assert!(matches!(
            poller.reserve(&f.ids[0]),
            Err(PollError::TooSoon { .. })
        ));
        reservation.started = true;
        drop(reservation);
        {
            let clocks = poller.clocks.lock().unwrap();
            let clock = &clocks[&f.ids[0]];
            assert!(!clock.running);
            assert_eq!(clock.failures, 1);
            assert_eq!(clock.failure_until, clock.next);
            let delay = clock.next.unwrap().duration_since(Instant::now()).as_secs();
            assert!((539..=660).contains(&delay));
        }
        assert!(matches!(
            poller.reserve(&f.ids[0]),
            Err(PollError::TooSoon { .. })
        ));
        {
            // Regular minimum and shared failure backoff are independent clocks.
            // Expire both explicitly instead of waiting for their real deadline.
            let expired = Instant::now() - Duration::from_secs(1);
            let mut clocks = poller.clocks.lock().unwrap();
            let clock = clocks.get_mut(&f.ids[0]).unwrap();
            clock.next = Some(expired);
            clock.failure_until = Some(expired);
        }
        f.fake.state.push(Fixture::usage());
        poller.poll(&f.ids[0]).await.unwrap();
        assert!(matches!(
            poller.poll(&f.ids[0]).await,
            Err(PollError::TooSoon { .. })
        ));
        assert_eq!(f.fake.state.requests().len(), 1);
        assert_eq!(poller.clocks.lock().unwrap()[&f.ids[0]].failures, 0);
    }

    #[tokio::test]
    async fn queued_cancellation_does_not_advance_unsent_account_clocks() {
        let f = Fixture::new(3).await;
        for _ in 0..3 {
            let mut response = Fixture::usage();
            response.chunks[0].0 = Duration::from_secs(5);
            f.fake.state.push(response);
        }
        f.relay.quota.before_unavailable(Provider::Codex).await;
        let requests = f.fake.state.requests();
        assert_eq!(requests.len(), 2);
        let clocks = f.relay.quota.clocks.lock().unwrap();
        let unsent: Vec<_> = clocks
            .values()
            .filter(|clock| clock.last.is_none())
            .collect();
        assert_eq!(unsent.len(), 1);
        assert!(!unsent[0].running);
        assert_eq!(unsent[0].failures, 0);
        assert!(unsent[0].next.is_none());
    }

    #[tokio::test]
    async fn scheduler_runs_at_most_two_polls_and_skips_fresh_accounts() {
        let f = Fixture::new(5).await;
        for _ in 0..5 {
            let mut response = Fixture::usage();
            response.chunks[0].0 = Duration::from_millis(300);
            f.fake.state.push(response);
        }
        let poller = f.relay.quota.clone();
        let task = tokio::spawn(async move {
            poller.tick().await;
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while f.fake.state.requests().len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(f.fake.state.requests().len(), 2);
        assert_eq!(f.relay.quota.semaphore.available_permits(), 0);
        task.await.unwrap();
        assert_eq!(f.fake.state.requests().len(), 5);
        // Force the minimum clock open: freshness still suppresses a scheduler poll.
        for clock in f.relay.quota.clocks.lock().unwrap().values_mut() {
            clock.next = None;
        }
        f.relay.quota.tick().await;
        assert_eq!(f.fake.state.requests().len(), 5);
    }
    #[tokio::test]
    async fn d20_placement_reservation_bypasses_minimum_but_not_throttle_or_running() {
        let f = Fixture::new(1).await;
        let poller = &f.relay.quota;
        let id = &f.ids[0];
        let now = Instant::now();
        poller.clocks.lock().unwrap().insert(
            id.clone(),
            Clock {
                next: Some(now + Duration::from_secs(3600)),
                ..Clock::default()
            },
        );
        assert!(matches!(poller.reserve(id), Err(PollError::TooSoon { .. })));
        let reservation = poller.reserve_reason_at(id, true, now).unwrap();
        assert!(matches!(
            poller.reserve_reason_at(id, true, now),
            Err(PollError::TooSoon { .. })
        ));
        drop(reservation);
        poller
            .clocks
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .placement_last = Some(now - Duration::from_secs(299));
        assert!(matches!(
            poller.reserve_reason_at(id, true, now),
            Err(PollError::TooSoon { .. })
        ));
        poller
            .clocks
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .placement_last = Some(now - Duration::from_secs(300));
        drop(poller.reserve_reason_at(id, true, now).unwrap());
        let mut clocks = poller.clocks.lock().unwrap();
        let clock = clocks.get_mut(id).unwrap();
        clock.failures = 1;
        clock.failure_until = Some(now + Duration::from_secs(300));
        drop(clocks);
        assert!(matches!(
            poller.reserve_reason_at(id, true, now),
            Err(PollError::TooSoon { .. })
        ));
        assert!(
            f.fake.state.requests().is_empty(),
            "reservation checks never send"
        );
    }
    #[tokio::test]
    async fn d20_scheduler_reconciles_partial_headers_and_deduplicates_by_watermark() {
        let f = Fixture::new(1).await;
        let poller = &f.relay.quota;
        let table = &f.relay.bindings;
        poller.set_bindings(table);
        let now = Utc::now();
        let placed = now - chrono::Duration::seconds(121);
        let snapshot = json!({"source":"headers","observed_at":placed - chrono::Duration::seconds(1),
            "windows":[
                {"name":"primary","used_pct":0.,"reset_at":now + chrono::Duration::hours(1),
                 "observed_at":placed - chrono::Duration::seconds(1)},
                {"name":"secondary","used_pct":0.,"reset_at":now + chrono::Duration::hours(24),
                 "observed_at":placed - chrono::Duration::seconds(1)}]});
        let candidate = crate::bindings::Candidate {
            account: f.account(0),
            quota: Some(snapshot.clone()),
            inflight: 0,
        };
        let decision = table.choose_with_hold_at(
            Provider::Codex,
            Some("codex:poll-evidence"),
            None,
            None,
            &[candidate],
            None,
            10,
            true,
            false,
            placed,
        );
        assert!(matches!(
            decision,
            crate::bindings::Decision::Selected { .. }
        ));
        drop(decision);
        assert!(poller.placement_due(&f.ids[0], now).is_some());
        let placed_seq = poller.placement_due(&f.ids[0], now).unwrap();
        let asof = table.begin_usage_observation();
        let mut partial = snapshot.clone();
        partial["observed_at"] = json!(now);
        partial["windows"][0]["observed_at"] = json!(now);
        table.note_usage_observation_asof(
            &f.ids[0],
            &json!({"windows":[partial["windows"][0].clone()]}),
            asof,
        );
        poller.obs.observe_poll(&f.ids[0], partial, true).unwrap();
        assert_eq!(
            poller.placement_due(&f.ids[0], now),
            Some(placed_seq),
            "weekly evidence remains old"
        );
        poller
            .clocks
            .lock()
            .unwrap()
            .entry(f.ids[0].clone())
            .or_default()
            .placement_polled_through = Some(placed_seq);
        assert!(
            poller.placement_due(&f.ids[0], now).is_none(),
            "one trigger per placement batch"
        );
        let mut fresh = snapshot;
        fresh["observed_at"] = json!(now + chrono::Duration::seconds(1));
        for w in fresh["windows"].as_array_mut().unwrap() {
            w["observed_at"] = json!(now + chrono::Duration::seconds(1));
        }
        table.note_usage_observation_asof(&f.ids[0], &fresh, asof);
        poller.obs.observe_poll(&f.ids[0], fresh, true).unwrap();
        assert!(poller.placement_due(&f.ids[0], now).is_none());
        assert!(
            table.due_placement_polls(now).is_empty(),
            "fresh windows retire both charges"
        );
        assert!(
            f.fake.state.requests().is_empty(),
            "synthetic time/evidence only"
        );
    }
    #[tokio::test]
    async fn d20_placement_success_does_not_spend_regular_poll_budget() {
        let f = Fixture::new(1).await;
        let poller = &f.relay.quota;
        let id = &f.ids[0];
        let now = Instant::now();
        let regular_next = now + Duration::from_secs(3600);
        poller.clocks.lock().unwrap().insert(
            id.clone(),
            Clock {
                next: Some(regular_next),
                ..Clock::default()
            },
        );
        let mut reservation = poller.reserve_reason_at(id, true, now).unwrap();
        reservation.started = true;
        reservation.succeeded = true;
        drop(reservation);
        assert_eq!(poller.clocks.lock().unwrap()[id].next, Some(regular_next));
        assert!(f.fake.state.requests().is_empty());
    }
    #[tokio::test]
    async fn r8_refused_placement_poll_does_not_skip_due_regular_tick() {
        let f = Fixture::new(1).await;
        let poller = &f.relay.quota;
        poller.set_bindings(&f.relay.bindings);
        let id = &f.ids[0];
        let placed = Utc::now() - chrono::Duration::seconds(121);
        let candidate = crate::bindings::Candidate {
            account: f.account(0),
            quota: None,
            inflight: 0,
        };
        drop(f.relay.bindings.choose_with_hold_at(
            Provider::Codex,
            Some("codex:refused-placement-poll"),
            None,
            None,
            &[candidate],
            None,
            10,
            true,
            false,
            placed,
        ));
        assert!(poller.placement_due(id, Utc::now()).is_some());
        poller.clocks.lock().unwrap().insert(
            id.clone(),
            Clock {
                placement_last: Some(Instant::now()),
                ..Clock::default()
            },
        );
        assert!(poller.scheduled(&f.account(0)));
        assert!(matches!(
            poller.reserve_reason(id, true),
            Err(PollError::TooSoon { .. })
        ));
        f.fake.state.push(Fixture::usage());
        poller.tick().await;
        let requests = f.fake.state.requests();
        assert_eq!(
            requests.len(),
            1,
            "regular usage poll must run in this very tick"
        );
        assert_eq!(requests[0].method, "GET");
        let clocks = poller.clocks.lock().unwrap();
        let clock = &clocks[id];
        assert!(clock.last.is_some());
        assert!(clock.next.is_some());
        assert!(
            clock.placement_polled_through.is_none(),
            "refused placement poll spends no placement trigger"
        );
    }
    mod ws_poll_receipt_tests {
        include!("quota/ws_poll_receipt_tests.rs");
    }
    #[cfg(windows)]
    #[path = "profile_linked_tests.rs"]
    mod profile_linked_tests;

    include!("quota/profile_tests.rs");
}

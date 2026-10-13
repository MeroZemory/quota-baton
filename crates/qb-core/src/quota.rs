//! Active usage observations. Polls share the relay credential gates and client.
mod parse;

use crate::{
    account_runtime::AccountRuntime,
    accounts::{Account, AccountStore, Health, HealthReason, Identity, Provider, UserState},
    config::Config,
    observability::Observability,
    relay::credentials::TokenGates,
};
use chrono::Utc;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Semaphore, time::Instant};

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
    last: Option<Instant>,
    failures: u32,
    running: bool,
    disabled_until: Option<Instant>,
}

pub struct UsagePoller {
    store: Arc<AccountStore>,
    runtime: Arc<AccountRuntime>,
    obs: Arc<Observability>,
    client: reqwest::Client,
    tokens: Mutex<TokenGates>,
    bases: [String; 2],
    min_interval: u64,
    clocks: Mutex<BTreeMap<String, Clock>>,
    tray_until: Mutex<Option<Instant>>,
    event_streams: AtomicUsize,
    semaphore: Semaphore,
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
        clock.next = Some(Instant::now() + Duration::from_secs_f64(delay));
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
            clocks: Mutex::default(),
            tray_until: Mutex::default(),
            event_streams: AtomicUsize::new(0),
            semaphore: Semaphore::new(2),
        })
    }
    #[cfg(all(feature = "testkit", debug_assertions))]
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
        let now = Instant::now();
        let mut clocks = self.clocks.lock().unwrap_or_else(|e| e.into_inner());
        let clock = clocks.entry(id.into()).or_default();
        let next = clock.next.into_iter().chain(clock.disabled_until).max();
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
            if crate::relay::credentials::is_linked(&account) {
                gates
                    .reload_linked(self.store.clone(), self.obs.clone(), account.id, "startup")
                    .await
                    .map_err(|_| PollError::Unavailable)?;
            }
        }
        Ok(())
    }

    pub async fn poll(&self, id: &str) -> Result<Account, PollError> {
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
        let mut reservation = self.reserve(id)?;
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
        reservation.started = true;
        self.clocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
            .expect("reserved clock")
            .last = Some(Instant::now());
        for attempt in 0..2 {
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
                if !crate::relay::credentials::is_linked(&account)
                    && crate::relay::poll_revoked(account.provider, &body)
                    && gates
                        .is_current(self.store.clone(), &account, &tokens)
                        .await
                        .map_err(|_| PollError::Unavailable)?
                {
                    gates
                        .revoke_if_current(self.store.clone(), &account, &tokens)
                        .await
                        .map_err(|_| PollError::Unavailable)?;
                    return Err(PollError::Unavailable);
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
                        account = updated;
                        tokens = refreshed;
                        continue;
                    }
                } else {
                    gates
                        .auth_failed(
                            self.store.clone(),
                            self.obs.clone(),
                            account.clone(),
                            &tokens,
                        )
                        .await
                        .map_err(|_| PollError::Unavailable)?;
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
            let snapshot = self
                .obs
                .observe_poll(id, snapshot)
                .ok_or(PollError::Unavailable)?;
            if probe.is_some() {
                self.runtime
                    .probe_success(id)
                    .await
                    .map_err(|_| PollError::Unavailable)?;
            }
            let account = self
                .runtime
                .apply_quota_snapshot(id, snapshot, None)
                .await
                .map_err(|_| PollError::Unavailable)?;
            reservation.succeeded = true;
            return Ok(account);
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
        let accounts = tokio::task::spawn_blocking(move || store.list())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let mut pending = futures_util::stream::FuturesUnordered::new();
        for account in accounts {
            if self.scheduled(&account) {
                pending.push(async move {
                    let _ = self.poll(&account.id).await;
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
            .join("../../.sandbox/usage-capture-tests");
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
            let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/usage-unit-tests");
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
                a.identity = Identity::Codex {
                    user_id: None,
                    account_id: format!("synthetic-usage-{n}"),
                    email: format!("usage-{n}@example.invalid"),
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
            let delay = clock.next.unwrap().duration_since(Instant::now()).as_secs();
            assert!((539..=660).contains(&delay));
        }
        poller
            .clocks
            .lock()
            .unwrap()
            .get_mut(&f.ids[0])
            .unwrap()
            .next = None;
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
}

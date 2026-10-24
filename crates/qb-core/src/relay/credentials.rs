use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use tokio::sync::{Mutex, Notify};

use crate::{
    account_runtime::AccountRuntime,
    accounts::{
        Account, AccountStore, Health, HealthReason, HealthSignal, ImportMode, Provider, Source,
        StoreError, Tokens, linked,
    },
    observability::Observability,
};

/// Safe admin/CLI response: credential values never cross this boundary.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ManualRefreshResult {
    pub outcome: ManualRefreshOutcome,
    pub token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_refresh_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualRefreshOutcome {
    Success,
}

/// Allowlisted terminal codes; never provider messages or response bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualRefreshTerminal {
    RefreshTokenInvalidated,
    RefreshTokenReused,
    RefreshTokenExpired,
    RefreshTokenRevoked,
    InvalidGrant,
    #[serde(rename = "token_endpoint_401")]
    TokenEndpoint401,
}

impl ManualRefreshTerminal {
    fn from_code(code: &str) -> Self {
        match code {
            "refresh_token_invalidated" => Self::RefreshTokenInvalidated,
            "refresh_token_reused" => Self::RefreshTokenReused,
            "refresh_token_expired" => Self::RefreshTokenExpired,
            "refresh_token_revoked" => Self::RefreshTokenRevoked,
            "invalid_grant" => Self::InvalidGrant,
            _ => Self::TokenEndpoint401,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ManualRefreshError {
    #[error("CPA가 갱신하는 계정입니다")]
    Linked,
    #[error("드레인 중인 계정은 갱신할 수 없습니다")]
    Drain,
    #[error("다시 로그인해야 하는 계정입니다")]
    NeedsRelogin,
    #[error("이전 갱신 결과가 불확실합니다. 강제 갱신이 필요합니다")]
    Uncertain,
    #[error("계정을 찾을 수 없습니다")]
    NotFound,
    #[error("지금은 계정을 갱신할 수 없습니다. 잠시 후 다시 시도하세요")]
    Unavailable,
    #[error("갱신 자격 증명이 만료되거나 무효화되었습니다. 다시 로그인하세요")]
    Terminal { outcome: ManualRefreshTerminal },
}

impl From<()> for ManualRefreshError {
    fn from(_: ()) -> Self {
        Self::Unavailable
    }
}

fn manual_allowed(account: &Account) -> Result<(), ManualRefreshError> {
    if is_linked(account) {
        Err(ManualRefreshError::Linked)
    } else if account.drain {
        Err(ManualRefreshError::Drain)
    } else if matches!(account.health, Health::NeedsRelogin { .. }) {
        Err(ManualRefreshError::NeedsRelogin)
    } else {
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct TokenGates {
    gates: Arc<Mutex<BTreeMap<String, Arc<Mutex<ReloadClock>>>>>,
    lifecycle: Arc<RefreshLifecycle>,
    client: reqwest::Client,
    runtime: Arc<AccountRuntime>,
    ahead_access: Arc<StdMutex<BTreeMap<String, (Account, Tokens)>>>,
    ahead_changed: Arc<Notify>,
    claude_endpoint: String,
    codex_endpoint: String,
    takeover_dependencies: Option<crate::accounts::takeover::Dependencies>,
    takeover_admission: Arc<Mutex<()>>,
}

#[derive(Default)]
struct RefreshLifecycle {
    state: StdMutex<RefreshActivityState>,
    changed: Notify,
}

#[derive(Default)]
struct RefreshActivityState {
    stopped: bool,
    active: usize,
}

struct RefreshActivity(Arc<RefreshLifecycle>);

impl Drop for RefreshActivity {
    fn drop(&mut self) {
        self.0.state.lock().expect("refresh lifecycle").active -= 1;
        self.0.changed.notify_waiters();
    }
}

impl RefreshLifecycle {
    fn admit(self: &Arc<Self>) -> Option<RefreshActivity> {
        let mut state = self.state.lock().expect("refresh lifecycle");
        if state.stopped {
            return None;
        }
        state.active += 1;
        Some(RefreshActivity(self.clone()))
    }

    // A persistence retry inherits an already admitted refresh, including during shutdown.
    fn track_write(self: &Arc<Self>) -> RefreshActivity {
        self.state.lock().expect("refresh lifecycle").active += 1;
        RefreshActivity(self.clone())
    }

    fn stopped(&self) -> bool {
        self.state.lock().expect("refresh lifecycle").stopped
    }

    async fn wait(&self, idle: bool) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock().expect("refresh lifecycle");
                if (idle && state.active == 0) || (!idle && state.stopped) {
                    return;
                }
            }
            changed.await;
        }
    }
}

#[derive(Default)]
struct ReloadClock {
    before_expiry: Option<Instant>,
    stale_event: Option<Instant>,
    pending: Option<Rotation>,
    refresh_failure: Option<(zeroize::Zeroizing<String>, Instant)>,
    uncertain_retry: Option<(zeroize::Zeroizing<String>, tokio::time::Instant)>,
    backoff_failures: u32,
    unknown_failures: u32,
}

struct Rotation {
    account: Account,
    tokens: Tokens,
    failed_token: zeroize::Zeroizing<String>,
    store_error_in_memory: bool,
    takeover_operation: Option<String>,
    // A retry may outlive its originating task. Keep the exact box admission
    // through pending storage and the final credential write.
    box_request: Option<crate::boxes::BoxRequest>,
}

#[derive(Default)]
struct RefreshContext {
    manual_baseline: Option<crate::accounts::TokenMeta>,
    takeover_operation: Option<String>,
}
impl ReloadClock {
    fn admit(last: &mut Option<Instant>, now: Instant, interval: Duration) -> bool {
        if last.is_some_and(|last| now.duration_since(last) < interval) {
            return false;
        }
        *last = Some(now);
        true
    }
    fn before_expiry(&mut self) -> bool {
        Self::admit(
            &mut self.before_expiry,
            Instant::now(),
            Duration::from_secs(60),
        )
    }
    fn stale_event(&mut self) -> bool {
        Self::admit(
            &mut self.stale_event,
            Instant::now(),
            Duration::from_secs(600),
        )
    }
}

impl TokenGates {
    pub fn new(client: reqwest::Client, runtime: Arc<AccountRuntime>) -> Self {
        Self {
            gates: Arc::default(),
            lifecycle: Arc::default(),
            client,
            runtime,
            ahead_access: Arc::default(),
            ahead_changed: Arc::default(),
            claude_endpoint: "https://platform.claude.com/v1/oauth/token".into(),
            codex_endpoint: "https://auth.openai.com/oauth/token".into(),
            takeover_dependencies: None,
            takeover_admission: Arc::default(),
        }
    }

    /// Owns the WHOLE admitted transaction after caller cancellation. No
    /// production fallback exists when the explicit adapter is not installed.
    pub(crate) async fn takeover(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        request: crate::accounts::takeover::TakeoverRequest,
        dependencies: crate::accounts::takeover::Dependencies,
    ) -> Result<serde_json::Value, crate::accounts::takeover::TakeoverError> {
        use crate::accounts::takeover::{self, Preparation, ReceiptState, TakeoverError};
        #[cfg(any(test, feature = "testkit"))]
        if !crate::accounts::takeover::synthetic_endpoint(dependencies.profile.endpoint()) {
            return Err(TakeoverError::Invalid);
        }
        let activity = self.lifecycle.admit().ok_or(TakeoverError::Unavailable)?;
        let mut this = self.clone();
        this.takeover_dependencies = Some(dependencies.clone());
        crate::boxes::spawn(async move {
            let _activity = activity;
            // Serialize source snapshots as well as new identities. The account
            // gate below is still shared with every D17 token consumer.
            let _admission = if request.phase == takeover::Phase::Commit {
                Some(this.takeover_admission.lock().await)
            } else {
                None
            };
            // prepare is read-only for inspect. Profile GET never refreshes.
            let preparation =
                takeover::prepare(store.clone(), dependencies.clone(), request).await?;
            let journal = match preparation {
                Preparation::Inspect(value) => return Ok(value),
                Preparation::Commit(journal) => *journal,
            };
            if journal.result.is_some() {
                return Ok(takeover::result_value(&journal));
            }
            let gate = this.gate(&journal.account.id).await;
            let guard = gate.lock_owned().await;
            if guard.pending.is_some() || this.lifecycle.stopped() {
                return Err(TakeoverError::Unavailable);
            }
            let install_store = store.clone();
            let install_dependencies = dependencies.clone();
            // The child blocking task retains lifecycle activity if this
            // detached parent is ever aborted, as do existing D17 read tasks.
            let write_activity = this.lifecycle.track_write();
            let journal = crate::boxes::spawn_blocking(move || {
                let _activity = write_activity;
                let _guard = guard;
                takeover::install(&install_store, &install_dependencies, journal)
            })
            .await
            .map_err(|_| TakeoverError::Unavailable)??;
            let receipt = store
                .takeover_receipt(&journal.account.id)?
                .ok_or(TakeoverError::Invalid)?;
            if receipt.operation != journal.operation {
                return Err(TakeoverError::Conflict);
            }
            match receipt.state {
                ReceiptState::Ready => {
                    let _ = this
                        .refresh_inner(
                            store.clone(),
                            obs,
                            journal.account.id.clone(),
                            None,
                            "takeover",
                            RefreshContext {
                                manual_baseline: None,
                                takeover_operation: Some(journal.operation.clone()),
                            },
                        )
                        .await;
                }
                ReceiptState::Started => return Err(TakeoverError::Uncertain),
                ReceiptState::Installed => return Err(TakeoverError::Unavailable),
                ReceiptState::Finished { .. } | ReceiptState::Superseded => {}
            }
            // A durable receipt, never uncertainty=false, determines completion.
            let finalize_activity = this.lifecycle.track_write();
            crate::boxes::spawn_blocking(move || {
                let _activity = finalize_activity;
                takeover::finalize(&store, journal)
            })
            .await
            .map_err(|_| TakeoverError::Unavailable)?
        })
        .await
        .map_err(|_| TakeoverError::Unavailable)?
    }

    #[cfg(debug_assertions)]
    pub fn with_token_endpoint(mut self, provider: Provider, url: String) -> Self {
        let parsed = reqwest::Url::parse(&url).expect("synthetic token URL");
        assert!(
            parsed.scheme() == "http"
                && parsed.host_str() == Some("127.0.0.1")
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.fragment().is_none()
                && !matches!(parsed.port_or_known_default(), Some(10100 | 10101))
        );
        match provider {
            Provider::Claude => self.claude_endpoint = url,
            Provider::Codex => self.codex_endpoint = url,
        }
        self
    }

    pub async fn is_current(
        &self,
        store: Arc<AccountStore>,
        account: &Account,
        tokens: &Tokens,
    ) -> Result<bool, ()> {
        let guard = self.gate(&account.id).await.lock_owned().await;
        if let Some(pending) = &guard.pending {
            return Ok(pending.tokens.access_token == tokens.access_token);
        }
        let id = account.id.clone();
        let failed = zeroize::Zeroizing::new(tokens.access_token.clone());
        crate::boxes::spawn_blocking(move || {
            let _guard = guard;
            snapshot(&store, &id).map(|(_, current)| current.access_token == *failed)
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn selection_reload(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        account: Account,
    ) -> Result<Account, ()> {
        let gate = self.gate(&account.id).await;
        let guard = gate.lock_owned().await;
        crate::boxes::spawn_blocking(move || {
            let mut clock = guard;
            if store.takeover_fenced(&account.id).map_err(|_| ())? {
                return Err(());
            }
            let (current, _) = snapshot(&store, &account.id)?;
            if !clock.before_expiry() {
                return Ok(current);
            }
            reload(&store, &obs, current, "selection", &mut clock)
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn revoke_if_current(
        &self,
        store: Arc<AccountStore>,
        account: &Account,
        tokens: &Tokens,
    ) -> Result<Account, ()> {
        let expected = account.clone();
        let failed = zeroize::Zeroizing::new(tokens.access_token.clone());
        let gate = self.gate(&account.id).await;
        let guard = gate.lock_owned().await;
        crate::boxes::spawn_blocking(move || {
            if let Some(pending) = &guard.pending
                && pending.tokens.access_token != *failed
            {
                return Ok(pending.account.clone());
            }
            let _guard = guard;
            let (current, current_tokens) = snapshot(&store, &expected.id)?;
            if is_linked(&current) {
                // Defensive fallback for callers classifying a linked 401 as revoked.
                return match linked::reload_and_store(&store, &current) {
                    Ok(updated) => Ok(updated),
                    Err(linked::LinkError::Store(StoreError::GenerationConflict { .. })) => {
                        snapshot(&store, &current.id).map(|(account, _)| account)
                    }
                    Err(_) if current_tokens.access_token != *failed => Ok(current),
                    Err(_) => transition(
                        &store,
                        &current,
                        &failed,
                        HealthSignal::LinkStale {
                            retry_at: chrono::Utc::now() + chrono::Duration::minutes(5),
                        },
                    ),
                };
            }
            transition(
                &store,
                &expected,
                &failed,
                HealthSignal::Revoked {
                    reason: "token_revoked".into(),
                },
            )
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn cool_unavailable_auth(
        &self,
        store: Arc<AccountStore>,
        account: &Account,
        tokens: &Tokens,
    ) -> Result<(), ()> {
        let guard = self.gate(&account.id).await.lock_owned().await;
        if guard
            .pending
            .as_ref()
            .is_some_and(|p| p.tokens.access_token != tokens.access_token)
        {
            return Ok(());
        }
        let id = account.id.clone();
        let failed = zeroize::Zeroizing::new(tokens.access_token.clone());
        crate::boxes::spawn_blocking(move || {
            let _guard = guard;
            store
                .update_metadata_for_token(&id, &failed, |account| {
                    if account.eligible(None) && !is_linked(account) {
                        account.apply_health_signal(
                            HealthSignal::Cooling {
                                reason: HealthReason::Auth,
                                until: chrono::Utc::now() + chrono::Duration::minutes(5),
                                scope: "account".into(),
                            },
                            chrono::Utc::now(),
                        );
                    }
                })
                .map(|_| ())
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())?
    }

    #[cfg(test)]
    #[allow(dead_code)] // Also compiled by the external credential-layer test harness.
    pub(super) async fn block_selection_for_test(&self, id: &str) -> impl Send {
        self.gate(id).await.lock_owned().await
    }

    async fn gate(&self, id: &str) -> Arc<Mutex<ReloadClock>> {
        self.gates
            .lock()
            .await
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    /// Startup and pre-poll reloads are serialized but never admission-throttled.
    pub(crate) async fn reload_linked(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        id: String,
        trigger: &str,
    ) -> Result<Account, ()> {
        let gate = self.gate(&id).await;
        let guard = gate.lock_owned().await;
        let trigger = trigger.to_owned();
        crate::boxes::spawn_blocking(move || {
            let mut clock = guard;
            let (current, _) = snapshot(&store, &id)?;
            if !is_linked(&current) {
                return Ok(current);
            }
            reload(&store, &obs, current, &trigger, &mut clock)
        })
        .await
        .map_err(|_| ())?
    }

    /// Returns a durable rotation when due; cancellation never aborts token consumption.
    pub async fn refresh_ahead(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        id: String,
    ) -> Result<Option<(Account, Tokens)>, ()> {
        let Some(activity) = self.lifecycle.admit() else {
            return Ok(None);
        };
        let this = self.clone();
        crate::boxes::spawn(async move {
            let _activity = activity;
            this.refresh_or_reload(store, obs, id, None, "refresh_ahead")
                .await
        })
        .await
        .map_err(|_| ())?
    }

    /// Shares the detached, write-ahead refresh engine with ahead and current-401.
    pub(crate) async fn manual_refresh(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        id: String,
        force: bool,
    ) -> Result<ManualRefreshResult, ManualRefreshError> {
        let activity = self
            .lifecycle
            .admit()
            .ok_or(ManualRefreshError::Unavailable)?;
        let read_store = store.clone();
        let read_id = id.clone();
        // Capture access BEFORE waiting for the gate: a concurrent rotation is
        // our result too, even though manual refresh has no due-time restriction.
        let (activity, failed, baseline) = crate::boxes::spawn_blocking(move || {
            // The blocking credential read outlives a cancelled requesting future.
            // Keep it tracked until its result is dropped or handed to refresh.
            let activity = activity;
            let account = read_store.load(&read_id).map_err(|error| match error {
                StoreError::NotFound => ManualRefreshError::NotFound,
                _ => ManualRefreshError::Unavailable,
            })?;
            manual_allowed(&account)?;
            let (current, tokens) = snapshot(&read_store, &read_id)?;
            let failed = zeroize::Zeroizing::new(tokens.access_token.clone());
            Ok::<_, ManualRefreshError>((activity, failed, current.token_meta))
        })
        .await
        .map_err(|_| ManualRefreshError::Unavailable)??;
        let this = self.clone();
        crate::boxes::spawn(async move {
            let _activity = activity;
            let (account, _) = this
                .refresh_inner(
                    store,
                    obs,
                    id,
                    Some(failed),
                    if force { "manual_force" } else { "manual" },
                    RefreshContext {
                        manual_baseline: Some(baseline),
                        takeover_operation: None,
                    },
                )
                .await?
                .ok_or(ManualRefreshError::Unavailable)?;
            Ok(ManualRefreshResult {
                outcome: ManualRefreshOutcome::Success,
                token_expires_at: account.token_meta.expires_at,
                last_refresh_at: account.token_meta.last_refresh_at,
            })
        })
        .await
        .map_err(|_| ManualRefreshError::Unavailable)?
    }

    /// Stops new refreshes and waits for detached token consumption and writes.
    /// On timeout or an unsaved rotation the caller must retain the lease and retry.
    pub async fn shutdown_refreshes(&self) -> Result<(), ()> {
        self.lifecycle
            .state
            .lock()
            .expect("refresh lifecycle")
            .stopped = true;
        self.lifecycle.changed.notify_waiters();
        tokio::time::timeout(Duration::from_secs(30), self.lifecycle.wait(true))
            .await
            .map_err(|_| ())?;
        let gates: Vec<_> = self.gates.lock().await.values().cloned().collect();
        let mut failed = false;
        for gate in gates {
            let mut clock = gate.lock().await;
            if let Some(rotation) = &clock.pending {
                match persist_async(self.runtime.store.clone(), rotation).await {
                    Ok(Some((account, _))) => {
                        self.runtime.clear_memory_health(&account);
                        clock.pending = None;
                    }
                    Ok(None) => {
                        tracing::info!(account = %rotation.account.id, "Deleted account rotation superseded");
                        clock.pending = None;
                    }
                    Err(()) => failed = true,
                }
            }
        }
        if failed { Err(()) } else { Ok(()) }
    }

    /// Shutdown wakes this scheduler; aborting it also remains safe.
    pub fn start_refresh_scheduler(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        crate::boxes::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(15));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {},
                    _ = this.lifecycle.wait(false) => break,
                }
                if this.lifecycle.stopped() {
                    break;
                }
                let read_store = store.clone();
                if let Ok(Ok(accounts)) =
                    crate::boxes::spawn_blocking(move || read_store.list()).await
                {
                    for account in accounts {
                        if !is_linked(&account)
                            && !matches!(account.health, Health::NeedsRelogin { .. })
                            && !account.drain
                        {
                            let _ = this
                                .refresh_ahead(store.clone(), obs.clone(), account.id)
                                .await;
                        }
                    }
                }
            }
        })
    }

    pub async fn before_request(
        &self,
        store: Arc<AccountStore>,
        observability: Arc<Observability>,
        id: String,
    ) -> Result<(Account, Tokens), ()> {
        let gate = self.gate(&id).await;
        let guard = loop {
            let changed = self.ahead_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Ok(guard) = gate.clone().try_lock_owned() {
                break guard;
            }
            {
                let serving = self.ahead_access.lock().expect("ahead access");
                if let Some((account, tokens)) = serving.get(&id)
                    && access_valid(account, chrono::Utc::now())
                {
                    return Ok((account.clone(), copy_tokens(tokens)));
                }
            }
            // Wake when valid access is published, or when the gate is released.
            tokio::select! {
                guard = gate.clone().lock_owned() => break guard,
                _ = &mut changed => {},
            }
        };
        let read_store = store.clone();
        let read_obs = observability.clone();
        let read_id = id.clone();
        let result = crate::boxes::spawn_blocking(move || {
            let store = read_store;
            let observability = read_obs;
            let id = read_id;
            let mut clock = guard;
            if clock.pending.is_some() {
                return Err(());
            }
            let (mut account, tokens) = snapshot(&store, &id)?;
            if is_linked(&account)
                && (clock.before_expiry.is_none()
                    || account.token_meta.expires_at.is_some_and(|expiry| {
                        expiry <= chrono::Utc::now() + chrono::Duration::minutes(10)
                    }))
                && clock.before_expiry()
            {
                account = reload(&store, &observability, account, "before_expiry", &mut clock)?;
            }
            if is_linked(&account) {
                snapshot(&store, &account.id)
            } else {
                Ok((account, tokens))
            }
        })
        .await
        .map_err(|_| ())??;
        if !is_linked(&result.0) && ahead_due(&result.0, chrono::Utc::now()) {
            let this = self.clone();
            crate::boxes::spawn(async move {
                let _ = this.refresh_ahead(store, observability, id).await;
            });
        }
        Ok(result)
    }

    pub async fn after_401(
        &self,
        store: Arc<AccountStore>,
        observability: Arc<Observability>,
        account: Account,
        failed: &Tokens,
    ) -> Result<Option<(Account, Tokens)>, ()> {
        let Some(activity) = self.lifecycle.admit() else {
            return Ok(None);
        };
        let failed = zeroize::Zeroizing::new(failed.access_token.clone());
        let this = self.clone();
        // Dropping the requesting future does not abort the single-use refresh.
        crate::boxes::spawn(async move {
            let _activity = activity;
            this.refresh_or_reload(
                store,
                observability,
                account.id,
                Some(failed),
                "upstream_401",
            )
            .await
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn auth_failed(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        account: Account,
        failed: &Tokens,
    ) -> Result<(), ()> {
        let gate = self.gate(&account.id).await;
        let guard = gate.lock_owned().await;
        let failed = zeroize::Zeroizing::new(failed.access_token.clone());
        crate::boxes::spawn_blocking(move || {
            let mut clock = guard;
            if clock
                .pending
                .as_ref()
                .is_some_and(|p| p.tokens.access_token != *failed)
            {
                return Ok(());
            }
            for _ in 0..3 {
                let (current, tokens) = snapshot(&store, &account.id)?;
                if is_linked(&current) {
                    // Retry budget is exhausted: persist CPA rotation, never send again.
                    let trigger = if tokens.access_token != *failed {
                        "stale_token_401"
                    } else {
                        "upstream_401"
                    };
                    reload(&store, &obs, current, trigger, &mut clock)?;
                    return Ok(());
                }
                if tokens.access_token != *failed {
                    return Ok(());
                }
                let mut applied = false;
                store
                    .update_metadata(&current.id, |a| {
                        if a.generation == current.generation {
                            a.apply_health_signal(
                                HealthSignal::AuthAfterRefresh,
                                chrono::Utc::now(),
                            );
                            applied = true;
                        }
                    })
                    .map_err(|_| ())?;
                if applied {
                    return Ok(());
                }
            }
            Err(())
        })
        .await
        .map_err(|_| ())?
    }

    async fn refresh_or_reload(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        id: String,
        failed: Option<zeroize::Zeroizing<String>>,
        trigger: &str,
    ) -> Result<Option<(Account, Tokens)>, ()> {
        self.refresh_inner(store, obs, id, failed, trigger, RefreshContext::default())
            .await
            .map_err(|_| ())
    }

    async fn refresh_inner(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        id: String,
        failed: Option<zeroize::Zeroizing<String>>,
        trigger: &str,
        context: RefreshContext,
    ) -> Result<Option<(Account, Tokens)>, ManualRefreshError> {
        let RefreshContext {
            manual_baseline,
            takeover_operation,
        } = context;
        let manual = matches!(trigger, "manual" | "manual_force" | "takeover");
        let gate = self.gate(&id).await;
        let mut clock = gate.clone().lock_owned().await;
        if self.lifecycle.stopped() || clock.pending.is_some() {
            return Ok(None);
        }
        let status_store = store.clone();
        let status_id = id.clone();
        let operation = takeover_operation.clone();
        let dependencies = self.takeover_dependencies.clone();
        crate::boxes::spawn_blocking(move || {
            use crate::accounts::takeover::ReceiptState;
            let receipt = status_store.takeover_receipt(&status_id).map_err(|_| ())?;
            if let Some(operation) = operation {
                let receipt = receipt.ok_or(ManualRefreshError::Unavailable)?;
                if receipt.operation != operation || receipt.state != ReceiptState::Ready {
                    return Err(ManualRefreshError::Uncertain);
                }
                let journal = status_store
                    .read_takeover_journal(&operation)
                    .map_err(|_| ())?
                    .ok_or(ManualRefreshError::Unavailable)?;
                if !journal.retired {
                    return Err(ManualRefreshError::Unavailable);
                }
                let deps = dependencies.ok_or(ManualRefreshError::Unavailable)?;
                if deps.source.owner(journal.provider, &journal.source)
                    != crate::accounts::takeover::Owner::Stopped
                {
                    return Err(ManualRefreshError::Unavailable);
                }
                deps.source
                    .retire(
                        journal.provider,
                        &journal.source,
                        &journal.destination,
                        journal.keep_source,
                    )
                    .map_err(|_| ManualRefreshError::Unavailable)?;
            } else if receipt.is_some_and(|r| r.fenced()) {
                return Err(ManualRefreshError::Uncertain);
            }
            Ok(())
        })
        .await
        .map_err(|_| ())??;
        if failed.is_none() && takeover_operation.is_none() {
            let read_store = store.clone();
            let read_id = id.clone();
            let metadata = crate::boxes::spawn_blocking(move || read_store.load(&read_id))
                .await
                .map_err(|_| ())?
                .map_err(|_| ())?;
            if is_linked(&metadata)
                || metadata.drain
                || matches!(metadata.health, Health::NeedsRelogin { .. })
                || !ahead_due(&metadata, chrono::Utc::now())
            {
                return Ok(None);
            }
        }
        let read_store = store.clone();
        let read_operation = takeover_operation.clone();
        let (current, tokens) = crate::boxes::spawn_blocking(move || {
            if let Some(operation) = read_operation {
                let current = read_store.load(&id).map_err(|_| ())?;
                let tokens = read_store
                    .takeover_tokens(&current, &operation)
                    .map_err(|_| ())?;
                Ok((current, tokens))
            } else {
                snapshot(&read_store, &id)
            }
        })
        .await
        .map_err(|_| ())??;
        if manual {
            manual_allowed(&current)?;
        }
        if is_linked(&current) {
            let Some(failed) = failed else {
                return Ok(None);
            };
            let trigger = if tokens.access_token != *failed {
                "stale_token_401"
            } else {
                "upstream_401"
            };
            return crate::boxes::spawn_blocking(move || {
                let mut clock = clock;
                let updated = reload(&store, &obs, current, trigger, &mut clock)?;
                let (updated, tokens) = snapshot(&store, &updated.id)?;
                Ok((tokens.access_token != *failed).then_some((updated, tokens)))
            })
            .await
            .map_err(|_| ())?
            .map_err(|_: ()| ManualRefreshError::Unavailable);
        }
        assert!(!is_linked(&current), "linked accounts cannot refresh");
        let ahead = failed.is_none() && takeover_operation.is_none();
        let failed = failed.unwrap_or_else(|| zeroize::Zeroizing::new(tokens.access_token.clone()));
        if tokens.access_token != *failed {
            return Ok(Some((current, tokens)));
        }
        if manual_baseline.as_ref().is_some_and(|baseline| {
            !current.token_meta.refresh_uncertain
                && current.token_meta.last_refresh_at != baseline.last_refresh_at
        }) {
            // A provider may keep its access-token value on a successful refresh.
            // Share that durable completion too, rather than consuming the RT twice.
            return Ok(Some((current, tokens)));
        }
        let due = ahead_due(&current, chrono::Utc::now());
        if manual && current.token_meta.refresh_uncertain && trigger != "manual_force" && !due {
            return Err(ManualRefreshError::Uncertain);
        }
        // Account drain stops proactive/manual work; C6 current-token poll 401
        // remains allowed. Core lifecycle stop excludes every refresh trigger.
        if (ahead && (current.drain || !due))
            || matches!(current.health, Health::NeedsRelogin { .. })
        {
            return Ok(None);
        }
        if clock
            .refresh_failure
            .as_ref()
            .is_some_and(|(token, retry_at)| **token == *failed && Instant::now() < *retry_at)
        {
            return Ok(None);
        }
        if (ahead || manual)
            && current.token_meta.refresh_uncertain
            && clock
                .uncertain_retry
                .as_ref()
                .is_some_and(|(token, retry_at)| {
                    **token == *failed && tokio::time::Instant::now() < *retry_at
                })
        {
            return Ok(None);
        }
        let serving = if (ahead || manual)
            && takeover_operation.is_none()
            && access_valid(&current, chrono::Utc::now())
        {
            self.ahead_access
                .lock()
                .expect("ahead access")
                .insert(current.id.clone(), (current.clone(), copy_tokens(&tokens)));
            self.ahead_changed.notify_waiters();
            Some(AheadAccess {
                entries: self.ahead_access.clone(),
                id: current.id.clone(),
            })
        } else {
            None
        };
        // Write-ahead must finish before consuming the refresh token.
        let prior_uncertain = current.token_meta.refresh_uncertain;
        let write_store = store.clone();
        let mut uncertain = current.clone();
        uncertain.token_meta.refresh_uncertain = true;
        let old_tokens = copy_tokens(&tokens);
        let write_operation = takeover_operation.clone();
        let current = match crate::boxes::spawn_blocking(move || {
            if let Some(operation) = write_operation {
                use crate::accounts::takeover::{Receipt, ReceiptState};
                let before = Receipt {
                    operation: operation.clone(),
                    state: ReceiptState::Ready,
                };
                let after = Receipt {
                    operation,
                    state: ReceiptState::Started,
                };
                write_store.save_takeover_receipt(
                    &uncertain,
                    &old_tokens,
                    Some(uncertain.generation),
                    Some(&before),
                    &after,
                )
            } else {
                save_refresh_credentials(&write_store, &uncertain, &old_tokens)
            }
        })
        .await
        .map_err(|_| ())?
        {
            Ok(current) => current,
            Err(StoreError::GenerationConflict { .. }) => return Ok(None),
            Err(_) => {
                clock.unknown_failures = 0;
                self.runtime.set_refresh_attention(&current.id, true);
                obs.record_refresh(
                    &current.id,
                    trigger,
                    "store_error_writeahead",
                    current.token_meta.expires_at,
                );
                obs.core_warning(&current.id, "refresh_store_error_writeahead");
                if (!ahead && !manual) || !access_valid(&current, chrono::Utc::now()) {
                    let mut cooled = current.clone();
                    cooled.apply_health_signal(HealthSignal::StoreFailure, chrono::Utc::now());
                    obs.record_health(&current.id, &current.health, &cooled.health);
                    self.runtime.emit_account(&cooled);
                }
                clock.refresh_failure = Some((failed, Instant::now() + Duration::from_secs(2)));
                return Ok(None);
            }
        };
        if self.lifecycle.stopped() {
            if let Some(operation) = &takeover_operation {
                let _ = store.finish_takeover_attempt(
                    &current.id,
                    operation,
                    crate::accounts::takeover::Verification {
                        ok: false,
                        token_expires_at: current.token_meta.expires_at,
                    },
                    None,
                );
            } else {
                restore_uncertainty(store.clone(), &current, &tokens, prior_uncertain).await;
            }
            return Ok(None);
        }
        // Space proactive uncertain retries from the send boundary. A new
        // current-token 401 bypasses this interval, but sets the next one too.
        clock.uncertain_retry = Some((
            zeroize::Zeroizing::new(failed.to_string()),
            tokio::time::Instant::now() + Duration::from_secs(120),
        ));
        let reply = tokio::time::timeout(
            Duration::from_secs(30),
            self.request_refresh(&current, &tokens),
        )
        .await;
        drop(serving);
        let mut reply = match reply {
            Ok(Ok(reply)) => reply,
            failure => {
                let error = match failure {
                    Ok(Err(error)) => error,
                    Err(_) => RefreshError::Uncertain,
                    Ok(Ok(_)) => unreachable!(),
                };
                obs.record_refresh(&current.id, trigger, &error.outcome(), None);
                if let Some(operation) = &takeover_operation {
                    // Only NotSent or terminal results are safe, durable endings.
                    // Sent transient/transport errors retain Started and its fence.
                    let terminal = match &error {
                        RefreshError::Terminal(reason) => Some(reason.clone()),
                        _ => None,
                    };
                    if matches!(&error, RefreshError::NotSent) || terminal.is_some() {
                        store
                            .finish_takeover_attempt(
                                &current.id,
                                operation,
                                crate::accounts::takeover::Verification {
                                    ok: false,
                                    token_expires_at: current.token_meta.expires_at,
                                },
                                terminal,
                            )
                            .map_err(|_| ())?;
                        return Ok(None);
                    }
                    obs.core_warning(&current.id, "takeover_refresh_uncertain");
                    return Err(ManualRefreshError::Uncertain);
                }
                if !matches!(error, RefreshError::Uncertain) {
                    clock.uncertain_retry = None;
                    // A request never sent cannot settle uncertainty from an earlier attempt.
                    restore_uncertainty(
                        store.clone(),
                        &current,
                        &tokens,
                        prior_uncertain && matches!(error, RefreshError::NotSent),
                    )
                    .await;
                }
                if let RefreshError::Terminal(reason) = error {
                    let outcome = ManualRefreshTerminal::from_code(&reason);
                    clock.unknown_failures = 0;
                    self.runtime.set_refresh_attention(&current.id, false);
                    crate::boxes::spawn_blocking(move || {
                        transition(
                            &store,
                            &current,
                            &failed,
                            HealthSignal::TerminalRefresh { reason },
                        )
                    })
                    .await
                    .map_err(|_| ())??;
                    return if manual {
                        Err(ManualRefreshError::Terminal { outcome })
                    } else {
                        Ok(None)
                    };
                }
                if matches!(error, RefreshError::Unknown4xx(_)) {
                    clock.unknown_failures = clock.unknown_failures.saturating_add(1);
                    self.runtime
                        .set_refresh_attention(&current.id, clock.unknown_failures >= 3);
                    if clock.unknown_failures == 3 {
                        obs.core_warning(&current.id, "refresh_unknown_4xx");
                    }
                } else {
                    clock.unknown_failures = 0;
                    self.runtime.set_refresh_attention(&current.id, false);
                }
                clock.backoff_failures = clock.backoff_failures.saturating_add(1);
                // Expiry/current401 gates uncertainty; the separate interval
                // only spaces proactive attempts after entering that window.
                clock.refresh_failure = if matches!(error, RefreshError::Uncertain) {
                    None
                } else {
                    Some((
                        failed,
                        Instant::now()
                            + Duration::from_secs((1u64 << clock.backoff_failures.min(8)).min(300)),
                    ))
                };
                return Ok(None);
            }
        };
        clock.refresh_failure = None;
        clock.uncertain_retry = None;
        clock.backoff_failures = 0;
        clock.unknown_failures = 0;
        self.runtime.set_refresh_attention(&current.id, false);
        let mut rotated = current.clone();
        let refreshed_at = chrono::Utc::now();
        rotated.token_meta.last_refresh_at = Some(refreshed_at);
        rotated.token_meta.expires_at = reply
            .expires_in
            .and_then(|seconds| {
                chrono::Duration::try_seconds(i64::try_from(seconds).ok()?)
                    .and_then(|duration| refreshed_at.checked_add_signed(duration))
            })
            .or(current.token_meta.expires_at);
        rotated.token_meta.refresh_uncertain = false;
        let rotation = Rotation {
            account: rotated,
            tokens: Tokens {
                access_token: std::mem::take(&mut reply.access_token),
                refresh_token: reply
                    .refresh_token
                    .take()
                    .unwrap_or_else(|| tokens.refresh_token.clone()),
                id_token: reply.id_token.take().or_else(|| tokens.id_token.clone()),
            },
            failed_token: failed,
            store_error_in_memory: false,
            takeover_operation: takeover_operation.clone(),
            box_request: crate::boxes::current_request(),
        };
        let result = persist_async(store.clone(), &rotation).await;
        match result {
            Ok(Some((updated, tokens))) => {
                self.runtime.clear_memory_health(&updated);
                obs.record_refresh(
                    &current.id,
                    trigger,
                    "success",
                    updated.token_meta.expires_at,
                );
                Ok(Some((updated, tokens)))
            }
            Ok(None) => {
                obs.record_refresh(&current.id, trigger, "superseded", None);
                Ok(None)
            }
            Err(_) => {
                let mut rotation = rotation;
                rotation
                    .account
                    .apply_health_signal(HealthSignal::StoreFailure, chrono::Utc::now());
                let warning_store = store.clone();
                let expected = current.clone();
                let rejected = rotation.failed_token.clone();
                let warning = crate::boxes::spawn_blocking(move || {
                    transition(
                        &warning_store,
                        &expected,
                        &rejected,
                        HealthSignal::StoreFailure,
                    )
                })
                .await;
                rotation.store_error_in_memory = !matches!(warning, Ok(Ok(ref account)) if matches!(account.health, Health::Cooling { reason: HealthReason::StoreError, .. }));
                if rotation.store_error_in_memory {
                    obs.record_health(&current.id, &current.health, &rotation.account.health);
                    self.runtime.emit_account(&rotation.account);
                }
                obs.record_refresh(
                    &current.id,
                    trigger,
                    "store_error",
                    rotation.account.token_meta.expires_at,
                );
                obs.core_warning(&current.id, "refresh_store_error");
                tracing::error!(account = %current.id, "Rotated credential persistence failed; retrying account write");
                clock.pending = Some(rotation);
                let this = self.clone();
                let activity = self.lifecycle.track_write();
                crate::boxes::spawn(async move {
                    let _activity = activity;
                    this.retry_rotation(store, obs, gate).await;
                });
                Ok(None)
            }
        }
    }

    async fn retry_rotation(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        gate: Arc<Mutex<ReloadClock>>,
    ) {
        let mut seconds = 1;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(seconds)) => {},
                _ = self.lifecycle.wait(false) => {},
            }
            let mut clock = gate.lock().await;
            let Some(rotation) = &clock.pending else {
                return;
            };
            match persist_async(store.clone(), rotation).await {
                Ok(Some((account, _))) => {
                    if rotation.store_error_in_memory && rotation.account.health != account.health {
                        obs.record_health(&account.id, &rotation.account.health, &account.health);
                    }
                    self.runtime.clear_memory_health(&account);
                    clock.pending = None;
                    return;
                }
                Ok(None) => {
                    obs.record_refresh(&rotation.account.id, "persist_retry", "superseded", None);
                    clock.pending = None;
                    return;
                }
                Err(()) => {}
            }
            if self.lifecycle.stopped() {
                // The durable old record remains refresh_uncertain. Never retry
                // indefinitely or discard the pending rotation while still running.
                obs.core_warning(&rotation.account.id, "refresh_shutdown_store_error");
                return;
            }
            seconds = (seconds * 2).min(60);
        }
    }

    async fn request_refresh(
        &self,
        account: &Account,
        tokens: &Tokens,
    ) -> Result<RefreshReply, RefreshError> {
        assert!(!is_linked(account), "linked accounts cannot refresh");
        let (url, client_id, scope) = match account.provider {
            Provider::Claude => (
                &self.claude_endpoint,
                "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
            ),
            Provider::Codex => (
                &self.codex_endpoint,
                "app_EMoamEEZ73f0CkXaXp7hrann",
                "openid profile email",
            ),
        };
        #[cfg(any(test, feature = "testkit"))]
        {
            let safe = reqwest::Url::parse(url).is_ok_and(|url| {
                url.scheme() == "http"
                    && url.host_str().is_some_and(|host| {
                        host.trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                    })
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()
                    && !matches!(url.port_or_known_default(), Some(10100 | 10101))
            });
            if !safe {
                return Err(RefreshError::NotSent);
            }
        }
        let body = [
            ("grant_type", "refresh_token"),
            ("refresh_token", tokens.refresh_token.as_str()),
            ("client_id", client_id),
            ("scope", scope),
        ];
        let request = self.client.post(url).header(
            "user-agent",
            concat!("quota-baton/", env!("CARGO_PKG_VERSION")),
        );
        let request = match account.provider {
            Provider::Claude => request.json(&std::collections::BTreeMap::from(body)),
            Provider::Codex => request.form(&body),
        };
        let mut response = request.send().await.map_err(|error| {
            if error.is_connect() {
                RefreshError::NotSent
            } else {
                RefreshError::Uncertain
            }
        })?;
        let status = response.status();
        if status.as_u16() == 401 {
            return Err(RefreshError::Terminal("token_endpoint_401".into()));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(RefreshError::Backoff(status.as_u16()));
        }
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            if status.is_client_error() {
                RefreshError::Unknown4xx(status.as_u16())
            } else {
                RefreshError::Uncertain
            }
        })? {
            if bytes.len().saturating_add(chunk.len()) > 65536 {
                return Err(if status.is_client_error() {
                    RefreshError::Unknown4xx(status.as_u16())
                } else {
                    RefreshError::Uncertain
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            let code = value["error"]
                .as_str()
                .or_else(|| value["error"]["code"].as_str())
                .or_else(|| value["error"]["type"].as_str())
                .or_else(|| value["code"].as_str());
            return Err(match code {
                Some(
                    code @ ("refresh_token_invalidated"
                    | "refresh_token_reused"
                    | "refresh_token_expired"
                    | "refresh_token_revoked"
                    | "invalid_grant"),
                ) => RefreshError::Terminal(code.to_owned()),
                _ if status.is_client_error() => RefreshError::Unknown4xx(status.as_u16()),
                _ => RefreshError::Backoff(status.as_u16()),
            });
        }
        let reply: RefreshReply =
            serde_json::from_slice(&bytes).map_err(|_| RefreshError::Uncertain)?;
        if reply.access_token.is_empty()
            || reply
                .refresh_token
                .as_ref()
                .is_some_and(|value| value.is_empty())
        {
            return Err(RefreshError::Uncertain);
        }
        Ok(reply)
    }
}

enum RefreshError {
    Terminal(String),
    Uncertain,
    NotSent,
    Backoff(u16),
    Unknown4xx(u16),
}

impl RefreshError {
    fn outcome(&self) -> String {
        match self {
            Self::Terminal(code) => code.clone(),
            Self::Uncertain => "transport_uncertain".into(),
            Self::NotSent => "transport_connect".into(),
            Self::Backoff(status) | Self::Unknown4xx(status) => format!("http_{status}"),
        }
    }
}

// Stable jitter delays the 20% boundary by at most 2% of lifetime.
fn refresh_point(account: &Account) -> Option<chrono::DateTime<chrono::Utc>> {
    let expires = account.token_meta.expires_at?;
    let refreshed = account.token_meta.last_refresh_at?;
    let lifetime = (expires - refreshed).num_seconds().max(0);
    let jitter = account.id.bytes().fold(0u64, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u64::from(byte))
    }) % 201;
    Some(
        expires - chrono::Duration::seconds(lifetime / 5)
            + chrono::Duration::seconds(lifetime * jitter as i64 / 10000),
    )
}

fn refresh_due(account: &Account, now: chrono::DateTime<chrono::Utc>) -> bool {
    refresh_point(account)
        .or(account.token_meta.expires_at)
        .is_some_and(|point| now >= point)
}

fn access_valid(account: &Account, now: chrono::DateTime<chrono::Utc>) -> bool {
    account
        .token_meta
        .expires_at
        .is_none_or(|expiry| now < expiry)
}
fn ahead_due(account: &Account, now: chrono::DateTime<chrono::Utc>) -> bool {
    if account.token_meta.refresh_uncertain {
        account
            .token_meta
            .expires_at
            .is_some_and(|expiry| now >= expiry - chrono::Duration::minutes(10))
    } else {
        refresh_due(account, now)
    }
}
struct AheadAccess {
    entries: Arc<StdMutex<BTreeMap<String, (Account, Tokens)>>>,
    id: String,
}
impl Drop for AheadAccess {
    fn drop(&mut self) {
        self.entries.lock().expect("ahead access").remove(&self.id);
    }
}
// Callers construct credential metadata from the current account. A native
// NewerLinkedToken health transition is deliberately still a FullSave, with its
// ordinary revocation semantics. No other restricted-save failure is retried here.
fn save_refresh_credentials(
    store: &AccountStore,
    account: &Account,
    tokens: &Tokens,
) -> Result<Account, StoreError> {
    match store.save_credentials(account, tokens, account.generation) {
        Err(StoreError::CredentialHealthChange) => {
            store.save(account, tokens, Some(account.generation))
        }
        result => result,
    }
}

async fn restore_uncertainty(
    store: Arc<AccountStore>,
    account: &Account,
    tokens: &Tokens,
    uncertain: bool,
) {
    let account = account.clone();
    let tokens = copy_tokens(tokens);
    let _ = crate::boxes::spawn_blocking(move || {
        let mut cleared = account;
        cleared.token_meta.refresh_uncertain = uncertain;
        save_refresh_credentials(&store, &cleared, &tokens)
    })
    .await;
}

#[derive(serde::Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
struct RefreshReply {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

fn copy_tokens(tokens: &Tokens) -> Tokens {
    Tokens {
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        id_token: tokens.id_token.clone(),
    }
}

async fn persist_async(
    store: Arc<AccountStore>,
    rotation: &Rotation,
) -> Result<Option<(Account, Tokens)>, ()> {
    let account = rotation.account.clone();
    let tokens = copy_tokens(&rotation.tokens);
    let failed = zeroize::Zeroizing::new(rotation.failed_token.to_string());
    let operation = rotation.takeover_operation.clone();
    let box_request = rotation.box_request.clone();
    crate::boxes::spawn_blocking(move || {
        let _box_request = box_request;
        for _ in 0..3 {
            let mut current = match store.load(&account.id) {
                Ok(current) => current,
                Err(StoreError::NotFound) => return Ok(None),
                Err(_) => return Err(()),
            };
            if let Some(operation) = &operation {
                use crate::accounts::takeover::ReceiptState;
                let receipt = store.takeover_receipt(&account.id).map_err(|_| ())?;
                if current.source != account.source
                    || receipt.as_ref().is_none_or(|r| r.operation != *operation)
                {
                    return Ok(None);
                }
                match receipt.expect("checked receipt").state {
                    ReceiptState::Started => {}
                    ReceiptState::Finished { .. } => {
                        return snapshot(&store, &account.id).map(Some);
                    }
                    _ => return Ok(None),
                }
            }
            let read_tokens = if let Some(operation) = &operation {
                store.takeover_tokens(&current, operation)
            } else {
                store.tokens(&current)
            };
            let stored_tokens = match read_tokens {
                Ok(tokens) => tokens,
                Err(StoreError::GenerationConflict { .. }) => continue,
                Err(StoreError::NotFound) => return Ok(None),
                Err(_) => {
                    if let Some(operation) = &operation
                        && store
                            .takeover_receipt(&account.id)
                            .map_err(|_| ())?
                            .is_none_or(|r| r.operation != *operation || !r.fenced())
                    {
                        return Ok(None);
                    }
                    return Err(());
                }
            };
            if stored_tokens.access_token != *failed {
                return Ok(Some((current, stored_tokens)));
            }
            current.token_meta = account.token_meta.clone();
            if matches!(
                current.health,
                Health::Cooling {
                    reason: HealthReason::StoreError,
                    ..
                }
            ) {
                current.health = Health::Ready;
            }
            let saved = if let Some(operation) = &operation {
                use crate::accounts::takeover::{Receipt, ReceiptState, Verification};
                let before = Receipt {
                    operation: operation.clone(),
                    state: ReceiptState::Started,
                };
                let after = Receipt {
                    operation: operation.clone(),
                    state: ReceiptState::Finished {
                        result: Verification {
                            ok: true,
                            token_expires_at: current.token_meta.expires_at,
                        },
                    },
                };
                store.save_takeover_receipt(
                    &current,
                    &tokens,
                    Some(current.generation),
                    Some(&before),
                    &after,
                )
            } else {
                save_refresh_credentials(&store, &current, &tokens)
            };
            match saved {
                Ok(updated) => return Ok(Some((updated, tokens))),
                Err(StoreError::GenerationConflict { .. }) => continue,
                Err(_) => {
                    if let Some(operation) = &operation
                        && store
                            .takeover_receipt(&account.id)
                            .map_err(|_| ())?
                            .is_none_or(|r| r.operation != *operation || !r.fenced())
                    {
                        return Ok(None);
                    }
                    return Err(());
                }
            }
        }
        Err(())
    })
    .await
    .map_err(|_| ())?
}

fn transition(
    store: &AccountStore,
    expected: &Account,
    failed: &str,
    signal: HealthSignal,
) -> Result<Account, ()> {
    let updated = store
        .update_metadata_for_token(&expected.id, failed, |account| {
            // Adoption may retain the exact access token while changing its owner.
            // A late linked reload must not reintroduce CPA-only cooling afterward.
            if matches!(&signal, HealthSignal::LinkStale { .. }) && !is_linked(account) {
                return;
            }
            account.apply_health_signal(signal, chrono::Utc::now());
        })
        .map_err(|_| ())?;
    Ok(updated)
}

pub(crate) fn is_linked(account: &Account) -> bool {
    matches!(
        account.source,
        Source::Cpa {
            mode: ImportMode::Linked,
            ..
        }
    )
}

// load와 tokens 사이의 다른 writer 갱신은 고정 횟수만 다시 읽는다.
fn snapshot(store: &AccountStore, id: &str) -> Result<(Account, Tokens), ()> {
    for _ in 0..3 {
        let account = store.load(id).map_err(|_| ())?;
        match store.tokens(&account) {
            Ok(tokens) => return Ok((account, tokens)),
            Err(StoreError::GenerationConflict { .. }) => continue,
            Err(_) => return Err(()),
        }
    }
    Err(())
}

fn reload(
    store: &AccountStore,
    obs: &Observability,
    account: Account,
    trigger: &str,
    clock: &mut ReloadClock,
) -> Result<Account, ()> {
    let rejected =
        zeroize::Zeroizing::new(store.tokens(&account).map_err(|_| ())?.access_token.clone());
    match linked::reload_and_store(store, &account) {
        Ok(updated) => {
            obs.record_reload(
                &account.id,
                trigger,
                "updated",
                updated.token_meta.expires_at,
            );
            Ok(updated)
        }
        Err(error) => {
            let expired = account
                .token_meta
                .expires_at
                .is_some_and(|expiry| expiry <= chrono::Utc::now());
            let rejected_401 = trigger == "upstream_401";
            let outcome = match error {
                linked::LinkError::Stale if !expired && !rejected_401 => "unchanged",
                linked::LinkError::Missing if !expired && !rejected_401 => "link_missing",
                linked::LinkError::Stale | linked::LinkError::Missing => "link_stale",
                linked::LinkError::InvalidData => "link_invalid",
                linked::LinkError::Store(StoreError::GenerationConflict { .. }) => {
                    "generation_conflict"
                }
                _ => "link_error",
            };
            if outcome != "link_stale" || clock.stale_event() {
                obs.record_reload(&account.id, trigger, outcome, account.token_meta.expires_at);
            }
            // reload 실패에도 실패한 원문이나 파일 경로는 로그에 넣지 않는다.
            if matches!(account.health, Health::NeedsRelogin { .. })
                || (trigger != "stale_token_401"
                    && (trigger == "upstream_401"
                        || account
                            .token_meta
                            .expires_at
                            .is_some_and(|expiry| expiry <= chrono::Utc::now())))
            {
                transition(
                    store,
                    &account,
                    &rejected,
                    HealthSignal::LinkStale {
                        retry_at: chrono::Utc::now() + chrono::Duration::minutes(5),
                    },
                )
            } else {
                Ok(account)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_linked_failure_cannot_cool_same_token_after_ownership_change() {
        use crate::accounts::{Identity, ImportMode, SourceMatch};
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r2a-ownership-race");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let store = AccountStore::acquire(root.path()).unwrap();
        let identity = Identity::Codex {
            account_id: "synthetic-ownership-race".into(),
            user_id: None,
            email: "race@example.invalid".into(),
        };
        let account: Account = serde_json::from_value(serde_json::json!({
            "schema":1,"id":identity.id(),"provider":"codex","label":"fixture",
            "email":"race@example.invalid","identity":identity,
            "source":{"kind":"cpa","mode":"linked","dir":root.path().join("absent-cpa"),
                "match":SourceMatch::from(&identity)},
            "user_state":"active","health":{"kind":"ready"},"generation":0,
            "token_meta":{"refresh_uncertain":false}
        }))
        .unwrap();
        let tokens = Tokens {
            access_token: "synthetic-same-access".into(),
            refresh_token: "synthetic-refresh".into(),
            id_token: None,
        };
        let old = store.save(&account, &tokens, None).unwrap();
        let mut adopted = old.clone();
        if let Source::Cpa { mode, .. } = &mut adopted.source {
            *mode = ImportMode::Adopted;
        }
        let adopted = store.save(&adopted, &tokens, Some(old.generation)).unwrap();
        // Deterministic interleaving: linked reader captured old; adoption committed;
        // its late missing-file result now tries the same-token health write.
        let current = transition(
            &store,
            &old,
            &tokens.access_token,
            HealthSignal::LinkStale {
                retry_at: chrono::Utc::now() + chrono::Duration::minutes(5),
            },
        )
        .unwrap();
        assert_eq!(current, adopted);
        assert!(!is_linked(&current));
        assert_eq!(current.health, Health::Ready);
        assert_eq!(
            store.tokens(&current).unwrap().access_token,
            tokens.access_token
        );
        // Actual owned-token authentication failures must still take effect.
        let current = transition(
            &store,
            &current,
            &tokens.access_token,
            HealthSignal::AuthAfterRefresh,
        )
        .unwrap();
        assert!(matches!(
            current.health,
            Health::Cooling {
                reason: HealthReason::Auth,
                ..
            }
        ));
    }

    #[test]
    fn reload_and_stale_intervals_have_independent_boundaries() {
        let mut clock = ReloadClock::default();
        let now = Instant::now();
        assert!(ReloadClock::admit(
            &mut clock.before_expiry,
            now,
            Duration::from_secs(60)
        ));
        assert!(!ReloadClock::admit(
            &mut clock.before_expiry,
            now + Duration::from_secs(59),
            Duration::from_secs(60)
        ));
        assert!(ReloadClock::admit(
            &mut clock.before_expiry,
            now + Duration::from_secs(60),
            Duration::from_secs(60)
        ));
        assert!(ReloadClock::admit(
            &mut clock.stale_event,
            now,
            Duration::from_secs(600)
        ));
        assert!(!ReloadClock::admit(
            &mut clock.stale_event,
            now + Duration::from_secs(599),
            Duration::from_secs(600)
        ));
        assert!(ReloadClock::admit(
            &mut clock.stale_event,
            now + Duration::from_secs(600),
            Duration::from_secs(600)
        ));
    }

    #[tokio::test]
    async fn r4_unavailable_auth_cools_model_eligible_account_and_guards_token() {
        use crate::accounts::{Identity, TokenMeta, UserState};
        let sandbox = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/hardening-fixtures");
        std::fs::create_dir_all(&sandbox).unwrap();
        let root = tempfile::tempdir_in(sandbox).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let store = loop {
            match AccountStore::acquire(root.path()) {
                Ok(store) => break Arc::new(store),
                Err(StoreError::LeaseBusy) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("synthetic fixture lease: {error}"),
            }
        };
        let now = chrono::Utc::now();
        let identity = Identity::Codex {
            account_id: "synthetic-auth-hardening".into(),
            user_id: None,
            email: "auth@example.invalid".into(),
        };
        let account = Account {
            schema: 1,
            id: identity.id(),
            provider: Provider::Codex,
            label: "fixture".into(),
            email: "auth@example.invalid".into(),
            plan: None,
            identity,
            source: Source::Oauth,
            user_state: UserState::Active,
            health: Health::Cooling {
                reason: HealthReason::Permission,
                until: now + chrono::Duration::hours(1),
                scope: "model:X".into(),
            },
            auth_failures: 0,
            payment_failures: 0,
            payment_until: None,
            auth_probe_pending: false,
            quota_cooling: None,
            drain: false,
            allow_credits: false,
            quota_credit_releasable: false,
            generation: 0,
            token_meta: TokenMeta::default(),
        };
        let tokens = Tokens {
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            id_token: None,
        };
        let account = store.save(&account, &tokens, None).unwrap();
        let obs = Arc::new(Observability::new(root.path(), None).unwrap());
        let runtime = AccountRuntime::new(store.clone(), obs, 1.0);
        let gates = TokenGates::new(reqwest::Client::new(), runtime);
        assert!(account.eligible(None));
        let stale = Tokens {
            access_token: "synthetic-stale".into(),
            refresh_token: "synthetic-refresh".into(),
            id_token: None,
        };
        gates
            .cool_unavailable_auth(store.clone(), &account, &stale)
            .await
            .unwrap();
        assert_eq!(store.load(&account.id).unwrap().health, account.health);
        gates
            .cool_unavailable_auth(store.clone(), &account, &tokens)
            .await
            .unwrap();
        let cooled = store.load(&account.id).unwrap();
        assert!(!cooled.eligible(None));
        assert!(!cooled.eligible(Some("Y")));
        assert!(
            matches!(cooled.health, Health::Cooling { scope, until, .. } if scope == "account" && until >= now + chrono::Duration::hours(1))
        );
        assert_eq!(cooled.auth_failures, 0);

        // A shorter model cooldown becomes the normal five-minute auth cooldown;
        // a longer one above retains its existing deadline through the reducer.
        let account = store
            .update_metadata(&account.id, |account| {
                account.health = Health::Cooling {
                    reason: HealthReason::Permission,
                    until: chrono::Utc::now() + chrono::Duration::minutes(1),
                    scope: "model:X".into(),
                };
            })
            .unwrap();
        gates
            .cool_unavailable_auth(store.clone(), &account, &tokens)
            .await
            .unwrap();
        let cooled = store.load(&account.id).unwrap();
        assert!(
            matches!(cooled.health, Health::Cooling { reason: HealthReason::Auth, scope, .. } if scope == "account")
        );
        assert!(cooled.auth_probe_pending);
        assert_eq!(cooled.auth_failures, 0);
    }
}

#[cfg(all(test, windows))]
#[path = "takeover_engine_tests.rs"]
mod takeover_engine_tests;

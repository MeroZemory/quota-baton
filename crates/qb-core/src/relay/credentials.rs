use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::Mutex;

use crate::{
    account_runtime::AccountRuntime,
    accounts::{
        Account, AccountStore, Health, HealthReason, HealthSignal, ImportMode, Provider, Source,
        StoreError, Tokens, linked,
    },
    observability::Observability,
};

#[derive(Clone)]
pub(super) struct TokenGates {
    gates: Arc<Mutex<BTreeMap<String, Arc<Mutex<ReloadClock>>>>>,
    client: reqwest::Client,
    runtime: Arc<AccountRuntime>,
    claude_endpoint: String,
    codex_endpoint: String,
}

#[derive(Default)]
struct ReloadClock {
    before_expiry: Option<Instant>,
    stale_event: Option<Instant>,
    pending: Option<Rotation>,
    refresh_failure: Option<(zeroize::Zeroizing<String>, Instant)>,
}

struct Rotation {
    account: Account,
    tokens: Tokens,
    failed_token: zeroize::Zeroizing<String>,
    store_error_in_memory: bool,
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
            client,
            runtime,
            claude_endpoint: "https://platform.claude.com/v1/oauth/token".into(),
            codex_endpoint: "https://auth.openai.com/oauth/token".into(),
        }
    }

    #[cfg(all(feature = "testkit", debug_assertions))]
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
        let id = account.id.clone();
        let failed = zeroize::Zeroizing::new(tokens.access_token.clone());
        tokio::task::spawn_blocking(move || {
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
        tokio::task::spawn_blocking(move || {
            let mut clock = guard;
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
        tokio::task::spawn_blocking(move || {
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
        let id = account.id.clone();
        let failed = zeroize::Zeroizing::new(tokens.access_token.clone());
        tokio::task::spawn_blocking(move || {
            store
                .update_metadata_for_token(&id, &failed, |account| {
                    if account.eligible(None) {
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

    pub async fn before_request(
        &self,
        store: Arc<AccountStore>,
        observability: Arc<Observability>,
        id: String,
    ) -> Result<(Account, Tokens), ()> {
        let gate = self.gate(&id).await;
        // blocking 작업이 요청 취소 후에도 계속되므로 lock 소유권을 작업 안으로 옮긴다.
        let guard = gate.lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let mut clock = guard;
            if clock.pending.is_some() {
                return Err(());
            }
            let (mut account, _) = snapshot(&store, &id)?;
            if is_linked(&account)
                && account.token_meta.expires_at.is_some_and(|expiry| {
                    expiry <= chrono::Utc::now() + chrono::Duration::minutes(10)
                })
                && clock.before_expiry()
            {
                account = reload(&store, &observability, account, "before_expiry", &mut clock)?;
            }
            snapshot(&store, &account.id)
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn after_401(
        &self,
        store: Arc<AccountStore>,
        observability: Arc<Observability>,
        account: Account,
        failed: &Tokens,
    ) -> Result<Option<(Account, Tokens)>, ()> {
        let failed = zeroize::Zeroizing::new(failed.access_token.clone());
        let this = self.clone();
        // Dropping the requesting future does not abort the single-use refresh.
        tokio::spawn(async move {
            this.refresh_or_reload(store, observability, account.id, failed)
                .await
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn auth_failed(
        &self,
        store: Arc<AccountStore>,
        _obs: Arc<Observability>,
        account: Account,
        failed: &Tokens,
    ) -> Result<(), ()> {
        let gate = self.gate(&account.id).await;
        let guard = gate.lock_owned().await;
        let failed = zeroize::Zeroizing::new(failed.access_token.clone());
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            for _ in 0..3 {
                let (current, tokens) = snapshot(&store, &account.id)?;
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
        failed: zeroize::Zeroizing<String>,
    ) -> Result<Option<(Account, Tokens)>, ()> {
        let gate = self.gate(&id).await;
        let mut clock = gate.clone().lock_owned().await;
        if clock.pending.is_some() {
            return Ok(None);
        }
        let read_store = store.clone();
        let (current, tokens) = tokio::task::spawn_blocking(move || snapshot(&read_store, &id))
            .await
            .map_err(|_| ())??;
        if tokens.access_token != *failed {
            return Ok(Some((current, tokens)));
        }
        if matches!(current.health, Health::NeedsRelogin { .. }) {
            return Ok(None);
        }
        if is_linked(&current) {
            return tokio::task::spawn_blocking(move || {
                let mut clock = clock;
                let updated = reload(&store, &obs, current, "upstream_401", &mut clock)?;
                let (updated, tokens) = snapshot(&store, &updated.id)?;
                Ok((tokens.access_token != *failed).then_some((updated, tokens)))
            })
            .await
            .map_err(|_| ())?;
        }
        if clock
            .refresh_failure
            .as_ref()
            .is_some_and(|(token, retry_at)| **token == *failed && Instant::now() < *retry_at)
        {
            return Ok(None);
        }
        // Write-ahead must finish before consuming the refresh token.
        let write_store = store.clone();
        let mut uncertain = current.clone();
        uncertain.token_meta.refresh_uncertain = true;
        let old_tokens = copy_tokens(&tokens);
        let current = match tokio::task::spawn_blocking(move || {
            write_store.save(&uncertain, &old_tokens, Some(uncertain.generation))
        })
        .await
        .map_err(|_| ())?
        {
            Ok(current) => current,
            Err(StoreError::GenerationConflict { .. }) => return Ok(None),
            Err(_) => return Err(()),
        };
        let reply = tokio::time::timeout(
            Duration::from_secs(30),
            self.request_refresh(&current, &tokens),
        )
        .await;
        let reply = match reply {
            Ok(Ok(reply)) => reply,
            Ok(Err(Some(reason))) => {
                tokio::task::spawn_blocking(move || {
                    transition(
                        &store,
                        &current,
                        &failed,
                        HealthSignal::TerminalRefresh { reason },
                    )
                })
                .await
                .map_err(|_| ())??;
                return Ok(None);
            }
            // Uncertain and transient errors keep the access token and health.
            _ => {
                // A possibly consumed single-use token is never immediately retried
                // by a concurrent waiter. Retry no earlier than refresh-ahead.
                let retry_secs = current
                    .token_meta
                    .expires_at
                    .zip(current.token_meta.last_refresh_at)
                    .map(|(expires, refreshed)| {
                        (expires - (expires - refreshed) / 5 - chrono::Utc::now()).num_seconds()
                    })
                    .unwrap_or(60)
                    .clamp(60, 86400) as u64;
                clock.refresh_failure =
                    Some((failed, Instant::now() + Duration::from_secs(retry_secs)));
                return Ok(None);
            }
        };
        clock.refresh_failure = None;
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
                access_token: reply.access_token,
                refresh_token: reply
                    .refresh_token
                    .unwrap_or_else(|| tokens.refresh_token.clone()),
                id_token: reply.id_token.or_else(|| tokens.id_token.clone()),
            },
            failed_token: failed,
            store_error_in_memory: false,
        };
        let result = persist_async(store.clone(), &rotation).await;
        match result {
            Ok((updated, tokens)) => Ok(Some((updated, tokens))),
            Err(_) => {
                let mut rotation = rotation;
                rotation
                    .account
                    .apply_health_signal(HealthSignal::StoreFailure, chrono::Utc::now());
                let warning_store = store.clone();
                let expected = current.clone();
                let rejected = rotation.failed_token.clone();
                let warning = tokio::task::spawn_blocking(move || {
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
                tracing::error!(account = %current.id, "Rotated credential persistence failed; retrying account write");
                clock.pending = Some(rotation);
                let this = self.clone();
                tokio::spawn(async move {
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
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            let mut clock = gate.lock().await;
            let Some(rotation) = &clock.pending else {
                return;
            };
            if let Ok((account, _)) = persist_async(store.clone(), rotation).await {
                if rotation.store_error_in_memory && rotation.account.health != account.health {
                    obs.record_health(&account.id, &rotation.account.health, &account.health);
                }
                self.runtime.clear_memory_health(&account);
                clock.pending = None;
                return;
            }
            seconds = (seconds * 2).min(60);
        }
    }

    async fn request_refresh(
        &self,
        account: &Account,
        tokens: &Tokens,
    ) -> Result<RefreshReply, Option<String>> {
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
        let request = match account.provider { Provider::Claude => request.json(&serde_json::json!({"grant_type":"refresh_token","refresh_token":tokens.refresh_token,"client_id":client_id,"scope":scope})), Provider::Codex => request.form(&body) };
        let mut response = request.send().await.map_err(|_| None)?;
        let status = response.status();
        if status.as_u16() == 401 {
            return Err(Some("token_endpoint_401".into()));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(None);
        }
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        while let Some(chunk) = response.chunk().await.map_err(|_| None)? {
            if bytes.len().saturating_add(chunk.len()) > 65536 {
                return Err(None);
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| None)?;
            let code = value["error"]
                .as_str()
                .or_else(|| value["error"]["code"].as_str())
                .or_else(|| value["code"].as_str());
            return Err(code
                .filter(|code| {
                    matches!(
                        *code,
                        "refresh_token_invalidated"
                            | "refresh_token_reused"
                            | "refresh_token_expired"
                            | "refresh_token_revoked"
                            | "invalid_grant"
                    )
                })
                .map(str::to_owned));
        }
        let reply: RefreshReply = serde_json::from_slice(&bytes).map_err(|_| None)?;
        if reply.access_token.is_empty()
            || reply.refresh_token.as_ref().is_some_and(String::is_empty)
        {
            return Err(None);
        }
        Ok(reply)
    }
}

#[derive(serde::Deserialize)]
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
) -> Result<(Account, Tokens), ()> {
    let account = rotation.account.clone();
    let tokens = copy_tokens(&rotation.tokens);
    let failed = zeroize::Zeroizing::new(rotation.failed_token.to_string());
    tokio::task::spawn_blocking(move || {
        for _ in 0..3 {
            let (mut current, stored_tokens) = snapshot(&store, &account.id)?;
            if stored_tokens.access_token != *failed {
                return Ok((current, stored_tokens));
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
            match store.save(&current, &tokens, Some(current.generation)) {
                Ok(updated) => return Ok((updated, tokens)),
                Err(StoreError::GenerationConflict { .. }) => continue,
                Err(_) => return Err(()),
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
            account.apply_health_signal(signal, chrono::Utc::now());
        })
        .map_err(|_| ())?;
    Ok(updated)
}

fn is_linked(account: &Account) -> bool {
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
            let outcome = match error {
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
            if trigger == "upstream_401"
                || account
                    .token_meta
                    .expires_at
                    .is_some_and(|expiry| expiry <= chrono::Utc::now())
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
            .join("../../.sandbox/hardening-fixtures");
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
            auth_probe_pending: false,
            quota_cooling: None,
            drain: false,
            allow_credits: false,
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

//! Read-only preview and a single, durably fenced POST. Never enters retry loops.
use super::*;
use crate::reset_credits::{self as reset, Error, Preview, model};
use qb_proto::{reset_credit_action::*, reset_credits::*};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const DETAILS: &str = "/backend-api/wham/rate-limit-reset-credits";
const PREVIEW_SECONDS: i64 = 60;

// Written BEFORE dispatch. A crash/cancel/timeout can only leave Unknown, never
// a permission to repeat. No access/refresh token or provider body is persisted.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fence {
    version: u8,
    identity: String,
    grant: String,
    request: String,
    outcome: ResetCreditOutcome,
}

#[derive(Serialize)]
struct RedemptionRequest<'a> {
    redeem_request_id: &'a str,
    credit_id: &'a str,
}

impl UsagePoller {
    pub(crate) async fn close_reset_credits(&self) {
        self.reset_credits.close().await;
    }
    pub(crate) fn reset_credit_cached(&self, id: &str, provider: Provider) -> ResetCreditInventory {
        if provider != Provider::Codex {
            return reset::inventory(ResetCreditState::Unsupported);
        }
        let Ok(entry) = self.reset_credits.entry(id) else {
            return ResetCreditInventory::default();
        };
        let Ok(entry) = entry.try_lock() else {
            return ResetCreditInventory::default();
        };
        let mut value = entry.inventory.clone().unwrap_or_default();
        let fresh = value
            .observed_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| {
                let age = Utc::now().signed_duration_since(at);
                age >= chrono::Duration::zero()
                    && age < chrono::Duration::seconds(model::INVENTORY_TTL_SECONDS)
            });
        if value.state == ResetCreditState::Fresh && !fresh {
            value.state = ResetCreditState::Stale;
            value.refusal = Some(ResetCreditRefusal::Stale);
        } else if value.state == ResetCreditState::Fresh {
            value.refusal = model::choose(&value, Utc::now()).err();
        }
        value
    }

    pub(super) fn observe_reset_credit_summary(&self, id: &str, provider: Provider, body: &[u8]) {
        if provider != Provider::Codex {
            return;
        }
        let Ok(entry) = self.reset_credits.entry(id) else {
            return;
        };
        let Ok(mut entry) = entry.try_lock() else {
            return;
        };
        let summary = serde_json::from_slice(body)
            .ok()
            .and_then(|value| model::parse_summary(&value, Utc::now()).ok());
        match (entry.inventory.as_mut(), summary) {
            (Some(old), Some(new))
                if old.available_count == new.available_count
                    && new.state == ResetCreditState::Fresh
                    && model::choose(old, Utc::now()).is_ok() => {}
            (_, Some(new)) if new.state == ResetCreditState::Fresh => entry.inventory = Some(new),
            (Some(old), _) => {
                old.state = ResetCreditState::Stale;
                old.refusal = Some(ResetCreditRefusal::Stale);
            }
            (None, Some(new)) => entry.inventory = Some(new),
            (None, None) => {}
        }
    }

    pub(super) fn stale_reset_credit_inventory(&self, id: &str) {
        if let Ok(entry) = self.reset_credits.entry(id)
            && let Ok(mut entry) = entry.try_lock()
            && let Some(value) = &mut entry.inventory
        {
            value.state = ResetCreditState::Stale;
            value.refusal = Some(ResetCreditRefusal::Stale);
        }
    }

    async fn reset_credentials(
        &self,
        id: &str,
    ) -> Result<(Account, crate::accounts::Tokens), Error> {
        if self.store.writes_stopped() {
            return Err(Error::Unavailable);
        }
        let original = self.load(id).await.map_err(|_| Error::Unavailable)?;
        if original.provider != Provider::Codex {
            return Err(Error::Unsupported);
        }
        if !reset_account_allowed(&original) {
            return Err(Error::Ineligible);
        }
        let gates = self.tokens.lock().map_err(|_| Error::Unavailable)?.clone();
        if crate::relay::credentials::is_linked(&original) {
            gates
                .reload_linked(
                    self.store.clone(),
                    self.obs.clone(),
                    id.into(),
                    "before_poll",
                )
                .await
                .map_err(|_| Error::Unavailable)?;
        }
        let (account, tokens) = gates
            .before_request(self.store.clone(), self.obs.clone(), id.into())
            .await
            .map_err(|_| Error::Unavailable)?;
        if !reset_account_allowed(&account) {
            return Err(Error::Ineligible);
        }
        Ok((account, tokens))
    }

    fn reset_request(
        &self,
        account: &Account,
        tokens: &crate::accounts::Tokens,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let endpoint = format!("{}{path}", self.bases[1]);
        let url = reqwest::Url::parse(&endpoint).map_err(|_| Error::Unavailable)?;
        // Test builds cannot reach a production redemption host, even with an
        // accidentally loaded real config. The shared HTTP client never redirects.
        if !reset_endpoint_allowed(&url) {
            return Err(Error::Unavailable);
        }
        let Identity::Codex { account_id, .. } = &account.identity else {
            return Err(Error::Unsupported);
        };
        // Only the mutating transport overrides reqwest's default protocol-NACK
        // policy. Ordinary relay/poll traffic keeps its incumbent client.
        let client = if method == reqwest::Method::POST {
            redemption_client()?
        } else {
            self.client.clone()
        };
        Ok(client
            .request(method, url)
            .bearer_auth(&tokens.access_token)
            .header("ChatGPT-Account-Id", account_id)
            .header("Accept", "application/json")
            .header(
                "User-Agent",
                concat!("quota-baton/", env!("CARGO_PKG_VERSION")),
            )
            .timeout(Duration::from_secs(20)))
    }

    async fn reset_details(
        &self,
        account: &Account,
        tokens: &crate::accounts::Tokens,
    ) -> Result<ResetCreditInventory, Error> {
        let response = self
            .reset_request(account, tokens, reqwest::Method::GET, DETAILS)?
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        let value = reset_body(response).await?;
        let mut inventory =
            model::parse_details(&value, Utc::now()).map_err(|_| Error::Unavailable)?;
        inventory.grants.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(inventory)
    }

    async fn reset_windows(
        &self,
        account: &Account,
        tokens: &crate::accounts::Tokens,
    ) -> Result<Value, Error> {
        let response = self
            .reset_request(
                account,
                tokens,
                reqwest::Method::GET,
                "/backend-api/wham/usage",
            )?
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        let body = reset_body(response).await?;
        let bytes = serde_json::to_vec(&body).map_err(|_| Error::Unavailable)?;
        let snapshot = super::parse::snapshot(Provider::Codex, &bytes, Utc::now())
            .ok_or(Error::Unavailable)?;
        // Bind the affected window set, not utilization/receipt timestamps:
        // ordinary traffic cannot change what the user agreed to reset.
        let mut names: Vec<_> = snapshot["windows"]
            .as_array()
            .ok_or(Error::Unavailable)?
            .iter()
            .map(|window| window["name"].clone())
            .collect();
        names.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        Ok(Value::Array(names))
    }

    pub(crate) async fn refresh_reset_credits(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<ResetCreditInventory, Error> {
        let admitted = self.reset_credits.admit()?;
        let poller = self.clone();
        let id = id.to_owned();
        crate::boxes::spawn(async move {
            let _admitted = admitted;
            poller.refresh_reset_credits_inner(&id).await
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    async fn refresh_reset_credits_inner(&self, id: &str) -> Result<ResetCreditInventory, Error> {
        let entry = self.reset_credits.entry(id)?;
        let mut entry = entry.try_lock().map_err(|_| Error::Busy)?;
        let _process_lock = self.reset_process_lock(id).await?;
        entry.preview = None;
        let result = async {
            let (account, tokens) = self.reset_credentials(id).await?;
            let inventory = self.reset_details(&account, &tokens).await?;
            let fence = self.reset_fence(id, None).await?;
            if let Some(mut fence) = fence {
                if fence.identity != account.identity.canonical_key() {
                    return Err(Error::Unknown);
                }
                if fence.outcome == ResetCreditOutcome::Unknown {
                    // Available/absent is not proof that an uncertain request did
                    // not apply. Only this exact grant's redeemed status resolves it.
                    if inventory
                        .grants
                        .iter()
                        .any(|g| g.id == fence.grant && g.status == ResetCreditStatus::Redeemed)
                    {
                        fence.outcome = ResetCreditOutcome::AlreadyRedeemed;
                        self.reset_fence(id, Some(fence)).await?;
                    } else {
                        entry.inventory = Some(inventory);
                        return Err(Error::Unknown);
                    }
                }
            }
            Ok(inventory)
        }
        .await;
        match result {
            Ok(value) => {
                entry.inventory = Some(value.clone());
                Ok(value)
            }
            Err(error) => {
                if let Some(value) = &mut entry.inventory {
                    value.state = ResetCreditState::Stale;
                    value.refusal = Some(ResetCreditRefusal::Stale);
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn preview_reset_credit(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<ResetCreditPreview, Error> {
        let admitted = self.reset_credits.admit()?;
        let poller = self.clone();
        let id = id.to_owned();
        crate::boxes::spawn(async move {
            let _admitted = admitted;
            poller.preview_reset_credit_inner(&id).await
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    async fn preview_reset_credit_inner(&self, id: &str) -> Result<ResetCreditPreview, Error> {
        let entry = self.reset_credits.entry(id)?;
        let mut entry = entry.try_lock().map_err(|_| Error::Busy)?;
        let _process_lock = self.reset_process_lock(id).await?;
        entry.preview = None;
        let (account, tokens) = self.reset_credentials(id).await?;
        if self.reset_fence(id, None).await?.is_some_and(|f| {
            f.outcome == ResetCreditOutcome::Unknown
                || f.identity != account.identity.canonical_key()
        }) {
            return Err(Error::Unknown);
        }
        let inventory = match self.reset_details(&account, &tokens).await {
            Ok(value) => value,
            Err(error) => {
                if let Some(value) = &mut entry.inventory {
                    value.state = ResetCreditState::Stale;
                    value.refusal = Some(ResetCreditRefusal::Stale);
                }
                return Err(error);
            }
        };
        entry.inventory = Some(inventory.clone());
        let grant = model::choose(&inventory, Utc::now())
            .map_err(|_| Error::Ineligible)?
            .clone();
        let windows = self.reset_windows(&account, &tokens).await?;
        let current = self.load(id).await.map_err(|_| Error::Unavailable)?;
        if !same_confirmation_account(&current, &account) {
            return Err(Error::Stale);
        }
        let wire = ResetCreditPreview {
            account: id.into(),
            label: account.label.clone(),
            email: account.email.clone(),
            preview_token: format!("{:032x}", rand::random::<u128>()),
            expires_at: (Utc::now() + chrono::Duration::seconds(PREVIEW_SECONDS)).to_rfc3339(),
            grant,
            inventory,
            effect: "eligible_codex_limits".into(),
        };
        entry.preview = Some(Preview {
            wire: wire.clone(),
            account,
            windows,
        });
        Ok(wire)
    }

    pub(crate) async fn commit_reset_credit(
        self: &Arc<Self>,
        id: String,
        request: ResetCreditCommitRequest,
    ) -> Result<ResetCreditResult, Error> {
        // Retain ownership of the per-account lock and terminal persistence even
        // if the HTTP caller disappears after dispatch. Never resubmit the POST.
        let admitted = self.reset_credits.admit()?;
        let poller = self.clone();
        crate::boxes::spawn(async move {
            let _admitted = admitted;
            poller.commit_reset_credit_inner(id, request).await
        })
        .await
        .map_err(|_| Error::Unknown)?
    }

    async fn commit_reset_credit_inner(
        &self,
        id: String,
        request: ResetCreditCommitRequest,
    ) -> Result<ResetCreditResult, Error> {
        if !request.confirm {
            return Err(Error::Stale);
        }
        let entry = self.reset_credits.entry(&id)?;
        let mut entry = entry.try_lock().map_err(|_| Error::Busy)?;
        let _process_lock = self.reset_process_lock(&id).await?;
        // A confirmation is spent before any await, including stale/refused ones.
        let preview = entry.preview.take().ok_or(Error::Stale)?;
        let valid_until =
            DateTime::parse_from_rfc3339(&preview.wire.expires_at).map_err(|_| Error::Stale)?;
        if request.preview_token != preview.wire.preview_token
            || Utc::now() >= valid_until
            || request.preview_token.len() != 32
            || preview.wire.account != id
        {
            return Err(Error::Stale);
        }
        let (account, tokens) = self.reset_credentials(&id).await?;
        if !same_confirmation_account(&account, &preview.account) {
            return Err(Error::Stale);
        }
        if self.reset_fence(&id, None).await?.is_some_and(|f| {
            f.outcome == ResetCreditOutcome::Unknown
                || f.identity != account.identity.canonical_key()
        }) {
            return Err(Error::Unknown);
        }
        let inventory = self.reset_details(&account, &tokens).await?;
        let grant = model::choose(&inventory, Utc::now()).map_err(|_| Error::Stale)?;
        if grant != &preview.wire.grant
            || inventory.available_count != preview.wire.inventory.available_count
            || inventory.grants != preview.wire.inventory.grants
            || self.reset_windows(&account, &tokens).await? != preview.windows
            || Utc::now() >= valid_until
            || !same_confirmation_account(
                &self.load(&id).await.map_err(|_| Error::Unavailable)?,
                &account,
            )
            || self.store.writes_stopped()
        {
            return Err(Error::Stale);
        }
        let mut fence = Fence {
            version: 1,
            identity: account.identity.canonical_key(),
            grant: grant.id.clone(),
            request: format!("{:032x}", rand::random::<u128>()),
            outcome: ResetCreditOutcome::Unknown,
        };
        let request = self
            .reset_request(
                &account,
                &tokens,
                reqwest::Method::POST,
                &format!("{DETAILS}/consume"),
            )?
            .json(&RedemptionRequest {
                redeem_request_id: &fence.request,
                credit_id: &fence.grant,
            });
        self.reset_fence(&id, Some(fence.clone())).await?;
        // If takeover stopped this store while the durable write ran, leave
        // Unknown and refuse to send. The process lock spans both boundaries.
        if self.store.writes_stopped() {
            return Err(Error::Unknown);
        }
        let current = self.load(&id).await.map_err(|_| Error::Unknown)?;
        if !same_confirmation_account(&current, &account)
            || !reset_account_allowed(&current)
            || Utc::now() >= valid_until
            || model::choose(&inventory, Utc::now()).ok() != Some(&preview.wire.grant)
        {
            fence.outcome = ResetCreditOutcome::NothingToReset;
            self.reset_fence(&id, Some(fence)).await?;
            return Err(Error::Stale);
        }
        let outcome = match request.send().await {
            Ok(response) => match reset_body(response).await {
                Ok(value) => serde_json::from_value::<Redemption>(value)
                    .map(|reply| reply.code)
                    .unwrap_or(ResetCreditOutcome::Unknown),
                Err(_) => ResetCreditOutcome::Unknown,
            },
            Err(_) => ResetCreditOutcome::Unknown,
        };
        fence.outcome = outcome;
        // If terminal persistence fails the durable pre-send Unknown remains.
        if self.reset_fence(&id, Some(fence)).await.is_err() {
            return Err(Error::Unknown);
        }
        let refreshed = self.reset_details(&account, &tokens).await;
        let mut refresh_failed = refreshed.is_err();
        let mut inventory = refreshed.unwrap_or(inventory);
        if outcome == ResetCreditOutcome::Unknown || refresh_failed {
            inventory.state = ResetCreditState::Stale;
            inventory.refusal = Some(ResetCreditRefusal::Stale);
        }
        entry.inventory = Some(inventory.clone());
        if matches!(
            outcome,
            ResetCreditOutcome::Reset | ResetCreditOutcome::AlreadyRedeemed
        ) {
            refresh_failed |= self.reset_success_usage(&account).await.is_err();
        }
        Ok(ResetCreditResult {
            account: id,
            outcome,
            inventory,
            refresh_failed,
        })
    }

    async fn reset_success_usage(&self, account: &Account) -> Result<(), Error> {
        // A confirmed reset gets one fresh read independent of the ordinary
        // auto/manual 5-minute budget. This is a GET, never a POST retry.
        let (current, tokens) = self.reset_credentials(&account.id).await?;
        if current.identity != account.identity || current.source != account.source {
            return Err(Error::Unavailable);
        }
        let account = &current;
        let table = self.binding_table();
        let asof = table
            .as_ref()
            .map(|table| table.begin_usage_observation_for(&account.id));
        let wait = self.runtime.capture_poll_origin(&account.id).await;
        let outside = self.runtime.capture_outside_poll_origin(&account.id);
        let started_at = Utc::now();
        let response = self
            .reset_request(
                account,
                &tokens,
                reqwest::Method::GET,
                "/backend-api/wham/usage",
            )?
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        let body = reset_body(response).await?;
        let bytes = serde_json::to_vec(&body).map_err(|_| Error::Unavailable)?;
        let snapshot = super::parse::snapshot(Provider::Codex, &bytes, Utc::now())
            .ok_or(Error::Unavailable)?;
        let complete = super::parse::is_complete_snapshot(Provider::Codex, &bytes);
        let (_, _, reconciled) = self
            .runtime
            .apply_polled_usage_report(
                crate::account_runtime::PollUsageReport {
                    account: account.clone(),
                    access_token: zeroize::Zeroizing::new(tokens.access_token.clone()),
                    started_at,
                    snapshot,
                    complete,
                },
                wait,
                outside,
                false,
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        let snapshot = reconciled.ok_or(Error::Unavailable)?;
        if let (Some(table), Some(asof)) = (table, asof) {
            if complete {
                table.replace_usage_windows_asof_token(&account.id, &snapshot, asof);
            } else {
                table.note_usage_observation_asof_token(&account.id, &snapshot, asof);
            }
        }
        Ok(())
    }

    async fn reset_process_lock(&self, id: &str) -> Result<std::fs::File, Error> {
        let directory = self.store.home().join("state/reset-credits");
        let name = format!("{:x}.lock", Sha256::digest(id.as_bytes()));
        crate::boxes::spawn_blocking(move || {
            crate::platform::ensure_private_dir_all(&directory).map_err(|_| Error::Unavailable)?;
            let file = crate::platform::open_reset_credit_lock(&directory.join(name))
                .map_err(|_| Error::Unavailable)?;
            fs2::FileExt::try_lock_exclusive(&file).map_err(|_| Error::Busy)?;
            Ok(file)
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    async fn reset_fence(&self, id: &str, update: Option<Fence>) -> Result<Option<Fence>, Error> {
        #[cfg(test)]
        if update
            .as_ref()
            .is_some_and(|value| value.outcome != ResetCreditOutcome::Unknown)
            && self
                .reset_credits
                .fail_terminal_write
                .swap(false, Ordering::AcqRel)
        {
            return Err(Error::Unavailable);
        }
        let home = self.store.home().to_path_buf();
        let id = id.to_owned();
        crate::boxes::spawn_blocking(move || {
            let directory = home.join("state/reset-credits");
            let file = directory.join(format!("{:x}.json", Sha256::digest(id.as_bytes())));
            if let Some(value) = update {
                crate::platform::ensure_private_dir_all(&directory)
                    .map_err(|_| Error::Unavailable)?;
                let bytes = serde_json::to_vec(&value).map_err(|_| Error::Unavailable)?;
                if bytes.len() > 16 * 1024 {
                    return Err(Error::Unavailable);
                }
                crate::platform::atomic_write_binding(&file, &bytes)
                    .map_err(|_| Error::Unavailable)?;
                Ok(Some(value))
            } else {
                match crate::platform::read_private_bytes(&file, 16 * 1024) {
                    Ok(bytes) if bytes.len() <= 16 * 1024 => {
                        let value: Fence =
                            serde_json::from_slice(&bytes).map_err(|_| Error::Unknown)?;
                        if value.version != 1 {
                            return Err(Error::Unknown);
                        }
                        Ok(Some(value))
                    }
                    Ok(_) => Err(Error::Unknown),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(_) => Err(Error::Unknown),
                }
            }
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}

fn redemption_client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
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
        .map_err(|_| Error::Unavailable)
}

#[derive(Deserialize)]
struct Redemption {
    code: ResetCreditOutcome,
    #[serde(default, rename = "windows_reset")]
    _windows_reset: u64,
}

fn same_confirmation_account(a: &Account, b: &Account) -> bool {
    // Generations also change on descriptive observations. Compare the actual
    // account/permission facts, not ordinary usage or last-observed metadata.
    a.id == b.id
        && a.provider == b.provider
        && a.identity == b.identity
        && a.source == b.source
        && a.label == b.label
        && a.email == b.email
        && a.plan == b.plan
        && a.user_state == b.user_state
        && a.drain == b.drain
        && a.health == b.health
        && a.auth_failures == b.auth_failures
        && a.auth_probe_pending == b.auth_probe_pending
}

fn reset_account_allowed(account: &Account) -> bool {
    account.provider == Provider::Codex
        && matches!(account.user_state, UserState::Active | UserState::Paused)
        && matches!(account.health, Health::Ready | Health::Cooling { .. })
}

async fn reset_body(mut response: reqwest::Response) -> Result<Value, Error> {
    if !response.status().is_success() {
        return Err(Error::Unavailable);
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        return Err(Error::Unavailable);
    }
    let mut body = zeroize::Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
        if chunk.len() > MAX_BODY.saturating_sub(body.len()) {
            return Err(Error::Unavailable);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| Error::Unavailable)
}

#[cfg(any(test, feature = "testkit"))]
fn reset_endpoint_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        })
}

#[cfg(not(any(test, feature = "testkit")))]
fn reset_endpoint_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("chatgpt.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
}

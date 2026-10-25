//! Read-only Claude plan observations on the existing scheduler, never a relay hook.
use super::*;
use crate::oauth::claude_profile;

const DAY: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
pub(super) struct ProfileClock {
    pub(super) next: Option<Instant>,
    pub(super) forced: bool,
    pub(super) running: bool,
}
impl ProfileClock {
    fn due(&self) -> bool {
        !self.running && (self.forced || self.next.is_none_or(|at| at <= Instant::now()))
    }
}

struct ProfileReservation<'a> {
    poller: &'a UsagePoller,
    id: String,
}
impl Drop for ProfileReservation<'_> {
    fn drop(&mut self) {
        if let Some(clock) = self
            .poller
            .clocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.id)
        {
            clock.profile.running = false;
        }
    }
}

fn allowed(account: &Account) -> bool {
    // Profile metadata covers every Claude user state, including disabled.
    // Serving/usage eligibility remains separate; unusable auth is excluded.
    account.provider == Provider::Claude && !matches!(account.health, Health::NeedsRelogin { .. })
}

impl UsagePoller {
    /// Explicit successful import/adopt hook. Caller supplies a current stored
    /// account, not arbitrary input IDs. Coalesces events; a mid-flight event is
    /// retained for the next tick. Metadata saves/token rotations never call it.
    pub(crate) fn queue_profile(&self, account: &Account) {
        if account.provider == Provider::Claude {
            self.clocks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(account.id.clone())
                .or_default()
                .profile
                .forced = true;
        }
    }

    pub(super) fn profile_due(&self, account: &Account) -> bool {
        allowed(account)
            && self
                .clocks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&account.id)
                .is_none_or(|clock| clock.profile.due())
    }

    pub(super) async fn fetch_profile(&self, id: &str) -> Result<Account, PollError> {
        let account = self.load(id).await?;
        if !allowed(&account) {
            return Err(PollError::Excluded);
        }
        let endpoint = format!("{}/api/oauth/profile", self.bases[0]);
        #[cfg(any(test, feature = "testkit"))]
        if !claude_profile::synthetic_endpoint(&endpoint) {
            return Err(PollError::Unavailable);
        }
        // Do not queue profiles behind a slow request and prolong the usage
        // scheduler tick. A busy slot leaves the account due for the next tick;
        // it consumes no attempt. Profiles never occupy usage poll slots.
        let _permit = self
            .profile_semaphore
            .try_acquire()
            .map_err(|_| PollError::Unavailable)?;
        {
            let mut clocks = self.clocks.lock().unwrap_or_else(|e| e.into_inner());
            let clock = &mut clocks.entry(id.into()).or_default().profile;
            if !clock.due() {
                return Err(PollError::TooSoon {
                    retry_after: DAY.as_secs(),
                });
            }
            clock.running = true;
            clock.forced = false;
            // All ordinary attempts count: token-read errors, 401/5xx, unknown
            // shape, timeout and cancellation. Only explicit lifecycle hooks
            // can bypass this deadline; the C9 retry stays within this attempt.
            clock.next = Some(Instant::now() + DAY);
        }
        let _reservation = ProfileReservation {
            poller: self,
            id: id.into(),
        };
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
        if !allowed(&account) {
            return Err(PollError::Excluded);
        }
        for attempt in 0..2 {
            let mut response = self
                .client
                .get(&endpoint)
                .timeout(Duration::from_secs(30))
                .bearer_auth(&tokens.access_token)
                .header("Accept", "application/json")
                .header(
                    "User-Agent",
                    concat!("quota-baton/", env!("CARGO_PKG_VERSION")),
                )
                .header("anthropic-beta", "oauth-2025-04-20")
                .send()
                .await
                .map_err(|_| PollError::Unavailable)?;
            let status = response.status();
            let mut body = zeroize::Zeroizing::new(Vec::new());
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
            if !status.is_success() {
                return Err(PollError::Unavailable);
            }
            let profile: serde_json::Value =
                serde_json::from_slice(&body).map_err(|_| PollError::Unavailable)?;
            // Refuse unrelated/malformed identity shapes; never attach another
            // organization's plan to this account. Unknown tiers on a valid
            // matching identity intentionally clear an old plan to None.
            let Identity::Claude {
                account_uuid,
                organization_uuid,
                ..
            } = &account.identity
            else {
                return Err(PollError::Excluded);
            };
            if profile["account"]["uuid"].as_str() != Some(account_uuid.as_str())
                || organization_uuid
                    .as_deref()
                    .is_some_and(|id| profile["organization"]["uuid"].as_str() != Some(id))
            {
                return Err(PollError::Unavailable);
            }
            let plan = claude_profile::plan(&profile).map(str::to_owned);
            let store = self.store.clone();
            let failed = zeroize::Zeroizing::new(tokens.access_token.clone());
            return crate::boxes::spawn_blocking(move || {
                // This API holds the existing lease/transaction and merges into
                // current metadata. A late response cannot overwrite a refresh,
                // adoption or unrelated admin edit made since request dispatch.
                store.update_metadata_for_token(&account.id, &failed, |current| {
                    if current.generation == account.generation
                        && current.source == account.source
                        && current.identity == account.identity
                        && allowed(current)
                    {
                        current.plan = plan;
                    }
                })
            })
            .await
            .map_err(|_| PollError::Unavailable)?
            .map_err(|_| PollError::Unavailable);
        }
        Err(PollError::Unavailable)
    }
}

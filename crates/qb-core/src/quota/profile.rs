//! Claude plan observations using the existing credential and metadata owners.
use super::*;
use crate::oauth::claude_profile;

const DAY: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
pub(super) struct ProfileClock {
    pub(super) next: Option<Instant>,
    pub(super) forced: bool,
    pub(super) running: bool,
    recovery_next: Option<Instant>,
    flight: Option<tokio::sync::watch::Receiver<Option<Result<Account, PollError>>>>,
    #[cfg(test)]
    pub(super) test_before_permit: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    #[cfg(test)]
    pub(super) test_follower_joined: Option<Arc<tokio::sync::Notify>>,
}
impl ProfileClock {
    fn due(&self, recovery: bool) -> bool {
        !self.running
            && (self.forced
                || self.next.is_none_or(|at| at <= Instant::now())
                || (recovery && self.recovery_next.is_none_or(|at| at <= Instant::now())))
    }
}

struct ProfileReservation<'a> {
    poller: &'a UsagePoller,
    id: String,
    result: tokio::sync::watch::Sender<Option<Result<Account, PollError>>>,
}
impl Drop for ProfileReservation<'_> {
    fn drop(&mut self) {
        // Cancellation wakes every coalesced caller with an explicit failure.
        self.result.send_if_modified(|result| {
            if result.is_none() {
                *result = Some(Err(PollError::Unavailable));
                true
            } else {
                false
            }
        });
        if let Some(clock) = self
            .poller
            .clocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.id)
        {
            clock.profile.running = false;
            clock.profile.flight = None;
        }
    }
}

fn allowed(account: &Account) -> bool {
    // Profile metadata covers every Claude user state, including disabled.
    // Serving/usage eligibility remains separate; unusable auth is excluded.
    account.provider == Provider::Claude && !matches!(account.health, Health::NeedsRelogin { .. })
}

fn recovering(account: &Account) -> bool {
    account.provider == Provider::Claude
        && (account.health == Health::PlanWithoutClaudeCode
            || account.plan.as_deref() == Some("free"))
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
                .is_none_or(|clock| clock.profile.due(recovering(account)))
    }

    pub(super) async fn fetch_profile(&self, id: &str) -> Result<Account, PollError> {
        self.fetch_profile_mode(id, false).await
    }

    /// An actual permission denial (or usage attempt) requires fresh evidence.
    /// Same-account callers share the in-flight observation, including failure.
    pub(crate) async fn refresh_permission_plan(&self, id: &str) -> Result<Account, PollError> {
        self.fetch_profile_mode(id, true).await
    }

    async fn fetch_profile_mode(&self, id: &str, force: bool) -> Result<Account, PollError> {
        let account = self.load(id).await?;
        if !allowed(&account) {
            return Err(PollError::Excluded);
        }
        // Ordinary background attempts keep the old non-queuing behavior and
        // daily failure deadline. Forced work reserves BEFORE waiting for the
        // global slot so concurrent callers cannot enqueue duplicate requests.
        let permit = if force {
            None
        } else {
            Some(
                self.profile_semaphore
                    .try_acquire()
                    .map_err(|_| PollError::Unavailable)?,
            )
        };
        let (result, follower) = {
            let mut clocks = self.clocks.lock().unwrap_or_else(|e| e.into_inner());
            let clock = &mut clocks.entry(id.into()).or_default().profile;
            if clock.running {
                (None, clock.flight.clone())
            } else {
                if !force && !clock.due(recovering(&account)) {
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
                // An untimed entitlement block still needs positive recovery
                // observations while ineligible and the tray is closed.
                clock.recovery_next = Some(Instant::now() + Duration::from_secs(self.min_interval));
                let (sender, receiver) = tokio::sync::watch::channel(None);
                clock.flight = Some(receiver);
                (Some(sender), None)
            }
        };
        if let Some(mut follower) = follower {
            // A forced leader can publish its flight before acquiring the slot.
            // Joining it must never retain the slot that leader needs.
            drop(permit);
            #[cfg(test)]
            if let Some(joined) = self
                .clocks
                .lock()
                .unwrap()
                .get_mut(id)
                .and_then(|clock| clock.profile.test_follower_joined.take())
            {
                joined.notify_one();
            }
            loop {
                if let Some(result) = follower.borrow_and_update().clone() {
                    return result;
                }
                follower
                    .changed()
                    .await
                    .map_err(|_| PollError::Unavailable)?;
            }
        }
        let reservation = ProfileReservation {
            poller: self,
            id: id.into(),
            result: result.ok_or(PollError::Unavailable)?,
        };
        #[cfg(test)]
        {
            let gate = self
                .clocks
                .lock()
                .unwrap()
                .get_mut(id)
                .and_then(|clock| clock.profile.test_before_permit.take());
            if let Some(gate) = gate {
                gate.0.notify_one();
                gate.1.notified().await;
            }
        }
        let _permit = match permit {
            Some(permit) => permit,
            None => self
                .profile_semaphore
                .acquire()
                .await
                .map_err(|_| PollError::Unavailable)?,
        };
        let result = self.fetch_profile_request(id).await;
        reservation.result.send_replace(Some(result.clone()));
        result
    }

    async fn fetch_profile_request(&self, id: &str) -> Result<Account, PollError> {
        let account = self.load(id).await?;
        if !allowed(&account) {
            return Err(PollError::Excluded);
        }
        let endpoint = format!("{}/api/oauth/profile", self.bases[0]);
        #[cfg(any(test, feature = "testkit"))]
        if !claude_profile::synthetic_endpoint(&endpoint) {
            return Err(PollError::Unavailable);
        }
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
        // Reload first so a newer linked token can recover. An unchanged token
        // rejected by the usage request must retain its native retry cooldown.
        if matches!(&account.health, Health::Cooling {
            reason: HealthReason::LinkStale, until, ..
        } if *until > Utc::now())
        {
            return Err(PollError::Unavailable);
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
            let obs = self.obs.clone();
            let failed = zeroize::Zeroizing::new(tokens.access_token.clone());
            return crate::boxes::spawn_blocking(move || {
                // This API holds the existing lease/transaction and merges into
                // current metadata. A late response cannot overwrite a refresh,
                // adoption or unrelated admin edit made since request dispatch.
                let mut previous_change = None;
                let updated = store.update_metadata_for_token(&account.id, &failed, |current| {
                    if current.generation == account.generation
                        && current.source == account.source
                        && current.identity == account.identity
                        && allowed(current)
                    {
                        let previous = current.plan.clone();
                        current.apply_health_signal(
                            crate::accounts::HealthSignal::PlanObservation { plan },
                            Utc::now(),
                        );
                        if previous != current.plan {
                            previous_change = Some(previous);
                        }
                    }
                })?;
                // Persistence and the exact-token/source/generation fence have
                // succeeded. Emit within this blocking task so cancellation of
                // the requesting future cannot lose a committed plan change.
                if let Some(previous) = previous_change {
                    obs.record_plan_change(&updated, previous.as_deref());
                }
                Ok::<_, crate::accounts::StoreError>(updated)
            })
            .await
            .map_err(|_| PollError::Unavailable)?
            .map_err(|_| PollError::Unavailable);
        }
        Err(PollError::Unavailable)
    }
}

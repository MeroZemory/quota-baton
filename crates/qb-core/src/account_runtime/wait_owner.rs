//! Process-local attribution only. No wait selection or dispatch is enabled here.

#[cfg(test)]
mod http_tests;

use super::{
    AccountRuntime, HealthSignal,
    wait_proof::{AcceptPermit, ReportOrigin, ReportSource, SealedHttpReport, WaitProofs},
};
use crate::accounts::{
    Account, AccountStore, BeforeAccountMutation, Health, HealthReason, ReportMutation,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::sync::{Arc, Weak};

const CAPACITY: usize = 4096;
const MAX_MODEL_BYTES: usize = 256;

/// Plain bounded classifier data; retained evidence is not admission authority.
pub(super) struct ShortQuotaProof {
    reset_at: DateTime<Utc>,
    model: Option<String>,
    observed_at: DateTime<Utc>,
    earliest_reset_at: DateTime<Utc>,
    started_at: DateTime<Utc>,
    scope: String,
    allow_credits: bool,
    quota_credit_releasable: bool,
}

impl ShortQuotaProof {
    fn valid_at(&self, now: DateTime<Utc>) -> bool {
        self.reset_at > now
            && self.earliest_reset_at > now
            && self.observed_at <= now
            && self.started_at <= now
            && now.signed_duration_since(self.observed_at) <= Duration::seconds(300)
            && self
                .model
                .as_ref()
                .is_none_or(|model| model.len() <= MAX_MODEL_BYTES)
    }

    fn matches_current(&self, account: &Account, now: DateTime<Utc>) -> bool {
        self.valid_at(now)
            && account.user_state == crate::accounts::UserState::Active
            && !account.drain
            && !account.auth_probe_pending
            && account.auth_failures == 0
            && account.payment_until.is_none_or(|until| until <= now)
            && account.allow_credits == self.allow_credits
            && account.quota_credit_releasable == self.quota_credit_releasable
            && matches!(&account.health, Health::Cooling {
                reason: HealthReason::Quota, until, scope,
            } if *until == self.reset_at && *scope == self.scope)
            && account.quota_cooling.as_ref().is_some_and(|hold| {
                hold.window == "5h"
                    && hold.reset_at == self.reset_at
                    && hold.started_at == self.started_at
                    && hold.started_at <= now
            })
    }
}

pub(super) enum ReportPublication {
    Immediate,
    DeferredHttp429,
}

/// Successful, executed header application, never a published wait ticket.
/// All fields are private, compact and non-serializable; Drop cancels minting.
pub(crate) struct PendingHttp429 {
    owner: Weak<WaitOwner>,
    id: String,
    sealed: SealedHttpReport,
    proof: ShortQuotaProof,
}

pub(super) struct WaitOwner {
    proofs: WaitProofs<ShortQuotaProof>,
}

impl WaitOwner {
    pub(super) fn new() -> Self {
        Self {
            proofs: WaitProofs::new(CAPACITY),
        }
    }

    /// Caller holds the store transaction. The hook owns neither Store nor Runtime.
    pub(super) fn attach_unlocked(self: &Arc<Self>, store: &AccountStore) {
        if store.writes_stopped() {
            self.close();
        }
        let weak = Arc::downgrade(self);
        store.set_pre_mutation_hook_unlocked(Arc::new(move |mutation| {
            let Some(owner) = weak.upgrade() else { return };
            match mutation {
                BeforeAccountMutation::Stop => owner.close(),
                BeforeAccountMutation::Retire { id } => {
                    let _ = owner.proofs.retire(id);
                }
                BeforeAccountMutation::Metadata { id, .. }
                | BeforeAccountMutation::HealthExpiry { id }
                | BeforeAccountMutation::Settings { id }
                | BeforeAccountMutation::FullSave { id } => owner.invalidate(id),
            }
        }));
    }

    pub(super) fn close(&self) {
        let _ = self.proofs.close();
    }

    fn invalidate(&self, id: &str) {
        let _ = self.proofs.invalidate(id);
    }

    pub(super) fn accept(
        self: &Arc<Self>,
        id: &str,
        origin: ReportOrigin,
    ) -> Option<ReportApplication> {
        let permit = self.proofs.accept(id, origin).ok()?;
        Some(ReportApplication {
            owner: Arc::downgrade(self),
            id: id.to_owned(),
            permit: Some(permit),
        })
    }

    /// Caller holds the Store transaction. Continue revalidates; the adapter
    /// still MUST own-rotate before the native signal (also for no signal).
    fn continue_http(
        self: &Arc<Self>,
        id: &str,
        pending: Option<PendingHttp429>,
        known_reset: Option<DateTime<Utc>>,
        signal: Option<&HealthSignal>,
        body_qualified: bool,
    ) -> Option<(ReportApplication, ShortQuotaProof)> {
        let continued = pending.and_then(|pending| {
            let owner = pending.owner.upgrade()?;
            let compatible_signal = match signal {
                None => true,
                Some(HealthSignal::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope,
                }) => *until == pending.proof.reset_at && *scope == pending.proof.scope,
                Some(_) => false,
            };
            if !Arc::ptr_eq(self, &owner)
                || pending.id != id
                || !body_qualified
                || known_reset != Some(pending.proof.reset_at)
                || !compatible_signal
                || !pending.proof.valid_at(Utc::now())
            {
                return None;
            }
            let permit = self.proofs.continue_http(pending.sealed).ok()?;
            Some((
                ReportApplication {
                    owner: Arc::downgrade(self),
                    id: pending.id,
                    permit: Some(permit),
                },
                pending.proof,
            ))
        });
        if continued.is_none() {
            // EVERY actual quota result cuts, even stale/unknown/same-health
            // reports with no Store signal. Never veto native C5 on failure.
            self.invalidate(id);
        }
        continued
    }
}

/// Store type erasure only carries this operation's accepted permit. Independent
/// expiry/metadata/Stop hooks always revoke it through the normal weak hook.
pub(super) struct ReportApplication {
    owner: Weak<WaitOwner>,
    id: String,
    permit: Option<AcceptPermit>,
}

impl ReportMutation for ReportApplication {
    fn before_store_mutation(&mut self, id: &str) {
        let permit = self.permit.take();
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        if id != self.id {
            owner.invalidate(id);
            owner.invalidate(&self.id);
            return;
        }
        self.permit = permit.and_then(|permit| owner.proofs.invalidate_accepted(permit).ok());
        if self.permit.is_none() {
            owner.invalidate(id);
        }
    }
}

impl ReportApplication {
    pub(super) fn finish(mut self, candidate: Option<ShortQuotaProof>) {
        let Some(proof) = candidate else { return };
        // Persistence may have crossed the deadline. Refuse only optional proof.
        if !proof.valid_at(Utc::now()) {
            return;
        }
        if let Some(owner) = self.owner.upgrade()
            && let Some(permit) = self.permit.take()
        {
            let _ = owner.proofs.mint(permit, proof);
        }
    }

    pub(super) fn seal_http(
        mut self,
        candidate: Option<ShortQuotaProof>,
        account: &Account,
    ) -> Option<PendingHttp429> {
        let proof = candidate?;
        if !proof.matches_current(account, Utc::now()) {
            return None;
        }
        let Health::Cooling {
            reason: HealthReason::Quota,
            until,
            scope,
        } = &account.health
        else {
            return None;
        };
        if *until != proof.reset_at
            || !(scope == "account"
                || proof
                    .model
                    .as_ref()
                    .is_some_and(|model| scope == &format!("model:{model}")))
        {
            return None;
        }
        let owner = self.owner.upgrade()?;
        let sealed = owner.proofs.seal_http(self.permit.take()?).ok()?;
        Some(PendingHttp429 {
            owner: self.owner,
            id: self.id,
            sealed,
            proof,
        })
    }
}

pub(super) fn classify(
    before: &Account,
    after: &Account,
    raw: &Value,
    model: Option<&str>,
    applied_at: DateTime<Utc>,
) -> Option<ShortQuotaProof> {
    if model.is_some_and(|model| model.len() > MAX_MODEL_BYTES) {
        return None;
    }
    let reset_at = if before.quota_cooling != after.quota_cooling {
        crate::bindings::quota_wait::newly_applied_short_quota_reset(
            before, after, raw, model, applied_at,
        )?
    } else {
        // Even an existing hold cannot borrow raw attribution from cached merge
        // or release filtering that produced a different native transition.
        let mut expected = before.clone();
        expected.apply_health_signal(
            crate::accounts::HealthSignal::UsageSnapshot {
                snapshot: raw.clone(),
                model: model.map(str::to_owned),
                projected_allowance: 0.0,
            },
            applied_at,
        );
        if &expected != after {
            return None;
        }
        crate::bindings::quota_wait::short_quota_reset(after, Some(raw), model, applied_at)?
    };
    Some(ShortQuotaProof {
        reset_at,
        model: model.map(str::to_owned),
        observed_at: raw["observed_at"].as_str()?.parse().ok()?,
        // The classifier verified every raw timestamp. Preserve the earliest
        // expiry as well as the 300s age bound without retaining arbitrary JSON.
        earliest_reset_at: raw["windows"]
            .as_array()?
            .iter()
            .map(|window| window["reset_at"].as_str()?.parse().ok())
            .collect::<Option<Vec<DateTime<Utc>>>>()?
            .into_iter()
            .min()?,
        started_at: after.quota_cooling.as_ref()?.started_at,
        scope: match &after.health {
            Health::Cooling { scope, .. } => scope.clone(),
            _ => return None,
        },
        allow_credits: after.allow_credits,
        quota_credit_releasable: after.quota_credit_releasable,
    })
}

impl AccountRuntime {
    /// Caller has classified/reconciled a real Quota outcome and consumes its
    /// signal here exactly once. Transport completeness supplies no authority.
    pub(crate) async fn finish_http_429_report(
        &self,
        id: &str,
        pending: Option<PendingHttp429>,
        known_reset: Option<DateTime<Utc>>,
        signal: Option<HealthSignal>,
        body_qualified: bool,
    ) -> Result<(), ()> {
        let store = self.store.clone();
        let owner = self.wait_owner.clone();
        let id = id.to_owned();
        let has_signal = signal.is_some();
        let result = tokio::task::spawn_blocking(move || {
            let _gate = match store.transaction() {
                Ok(gate) => gate,
                Err(_) => {
                    owner.close();
                    // Old no-signal handling only invalidated optional proof;
                    // it did not expose Store-gate failure as a native 502.
                    return if has_signal { Err(()) } else { Ok(()) };
                }
            };
            if store.writes_stopped() {
                owner.close();
            }
            let (mut application, candidate) = match owner.continue_http(
                &id,
                pending,
                known_reset,
                signal.as_ref(),
                body_qualified,
            ) {
                Some((application, proof)) => (Some(application), Some(proof)),
                None => (None, None),
            };
            match signal {
                Some(signal) => {
                    // Same native transition/cause as transition(), without a
                    // nested Store acquisition. Optional failure never skips it.
                    let cause = match application.as_mut() {
                        Some(application) => {
                            crate::accounts::MetadataCause::UsageReport(application)
                        }
                        None => crate::accounts::MetadataCause::Mutation(
                            if matches!(
                                &signal,
                                HealthSignal::Cooling {
                                    reason: HealthReason::Quota,
                                    ..
                                }
                            ) {
                                crate::accounts::MetadataMutation::QuotaRejected
                            } else {
                                crate::accounts::MetadataMutation::Ordinary
                            },
                        ),
                    };
                    let mut ran = false;
                    let mut unchanged = false;
                    store
                        .update_metadata_with_cause_unlocked(&id, cause, |account| {
                            ran = true;
                            let before = candidate.as_ref().map(|_| account.clone());
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
                            let applied_at = Utc::now();
                            account.apply_health_signal(signal, applied_at);
                            // Compare the full native Account inside the actual
                            // reducer closure, before any persistence generation.
                            unchanged = before.as_ref() == Some(&*account)
                                && candidate.as_ref().is_some_and(|proof| {
                                    proof.matches_current(account, applied_at)
                                });
                        })
                        .map_err(|_| ())?;
                    if ran
                        && unchanged
                        && !store.writes_stopped()
                        && let Some(application) = application
                    {
                        application.finish(candidate);
                    }
                }
                None => {
                    if let Some(mut application) = application.take() {
                        // Reconciliation already applied C5. Still make the
                        // final quota cut; do not fabricate a metadata write.
                        application.before_store_mutation(&id);
                        let current_matches = candidate.as_ref().is_some_and(|proof| {
                            store.read_account_unlocked(&id).is_ok_and(|account| {
                                account.id == id && proof.matches_current(&account, Utc::now())
                            })
                        });
                        if !store.writes_stopped() && current_matches {
                            application.finish(candidate);
                        } else {
                            // Missing/malformed/stale metadata declines only
                            // proof. No native signal means no new data error.
                            owner.invalidate(&id);
                        }
                    }
                }
            }
            Ok::<_, ()>(())
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                self.wait_owner.close();
                if has_signal { Err(()) } else { Ok(()) }
            }
        }
    }

    pub(crate) async fn capture_http_origin(&self, id: &str) -> Option<ReportOrigin> {
        self.capture_report_origin(id, ReportSource::HttpAttempt)
            .await
    }

    pub(crate) async fn capture_poll_origin(&self, id: &str) -> Option<ReportOrigin> {
        self.capture_report_origin(id, ReportSource::UsagePoll)
            .await
    }

    /// Called only before a physical send, before the final native admission.
    async fn capture_report_origin(&self, id: &str, source: ReportSource) -> Option<ReportOrigin> {
        if id.is_empty() || id.len() > 128 {
            return None;
        }
        let id = id.to_owned();
        let store = self.store.clone();
        let owner = self.wait_owner.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = store.transaction().ok()?;
            if store.writes_stopped() {
                return None;
            }
            let current = store.read_account_unlocked(&id).ok()?;
            if current.id != id {
                return None;
            }
            owner.proofs.capture(&id, source).ok()
        })
        .await
        .ok()
        .flatten()
    }

    /// A receipt with no usable report still consumes its pre-send watermark.
    /// No synthetic raw report, receipt-time origin, or proof is constructed.
    pub(crate) async fn discard_report_origin(&self, id: &str, origin: Option<ReportOrigin>) {
        let Some(origin) = origin else { return };
        if id.is_empty() || id.len() > 128 {
            return;
        }
        let id = id.to_owned();
        let store = self.store.clone();
        let owner = self.wait_owner.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let Ok(_gate) = store.transaction() else {
                owner.close();
                return;
            };
            if store.writes_stopped() {
                owner.close();
                return;
            }
            // A stale/unproven receipt may conservatively revoke newer proof,
            // just like ordinary native apply; it can never mint or recapture.
            if owner.accept(&id, origin).is_none() {
                owner.invalidate(&id);
            }
        })
        .await;
    }

    /// Every actual quota rejection must call this, including an unknown reset
    /// with no native Store write. Registry failure never vetoes the native C5.
    pub(crate) async fn invalidate_quota_rejection(&self, id: &str) {
        if id.is_empty() || id.len() > 128 {
            return;
        }
        let id = id.to_owned();
        let store = self.store.clone();
        let owner = self.wait_owner.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let Ok(_gate) = store.transaction() else {
                owner.close();
                return;
            };
            owner.invalidate(&id);
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Health, HealthReason, QuotaCooling};
    use serde_json::json;

    struct Fixture {
        runtime: Arc<AccountRuntime>,
        id: String,
        home: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_account(crate::accounts::health::tests::fixture())
        }

        fn with_account(account: Account) -> Self {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/r13-part-c/owner-fixtures");
            std::fs::create_dir_all(&base).unwrap();
            let home = tempfile::tempdir_in(base).unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            {
                let _gate = store.transaction().unwrap();
                Self::write(home.path(), &account);
            }
            let obs =
                Arc::new(crate::observability::Observability::new(home.path(), None).unwrap());
            let runtime = AccountRuntime::new(store, obs, 0.0);
            Self {
                runtime,
                id: account.id,
                home,
            }
        }

        // Synthetic opaque ciphertext is never decrypted by metadata operations.
        fn write(home: &std::path::Path, account: &Account) {
            let mut record = serde_json::to_value(account).unwrap();
            record["secret_dpapi_b64"] = json!("synthetic-unused-ciphertext");
            std::fs::write(
                home.join("accounts").join(format!("{}.json", account.id)),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
        }

        fn raw() -> Value {
            let at = Utc::now() - Duration::seconds(10);
            json!({"observed_at":at,"windows":[
                {"name":"5h","used_pct":100.0,"observed_at":at,"reset_at":at+Duration::minutes(10)},
                {"name":"7d","used_pct":20.0,"observed_at":at,"reset_at":at+Duration::days(1)}
            ]})
        }

        async fn apply(&self, raw: Value, origin: Option<ReportOrigin>) -> Account {
            self.runtime
                .apply_quota_snapshot_reported_origin(&self.id, raw.clone(), &raw, None, origin)
                .await
                .unwrap()
        }

        fn proof(&self) -> Option<DateTime<Utc>> {
            let _gate = self.runtime.store.transaction().unwrap();
            self.runtime
                .wait_owner
                .proofs
                .acquire(&self.id)
                .ok()
                .map(|(_, proof)| proof.reset_at)
        }

        fn ticket(&self) -> super::super::wait_proof::WaitTicket {
            let _gate = self.runtime.store.transaction().unwrap();
            self.runtime.wait_owner.proofs.acquire(&self.id).unwrap().0
        }

        fn admits(&self, ticket: &super::super::wait_proof::WaitTicket) -> bool {
            let _gate = self.runtime.store.transaction().unwrap();
            self.runtime.wait_owner.proofs.admit(ticket).is_ok()
        }

        async fn proven(&self) -> Account {
            let origin = self.runtime.capture_http_origin(&self.id).await.unwrap();
            let account = self.apply(Self::raw(), Some(origin)).await;
            assert!(
                self.proof().is_some(),
                "actual introducing reducer must mint"
            );
            account
        }

        fn headers(
            provider: crate::accounts::Provider,
            reset: DateTime<Utc>,
        ) -> axum::http::HeaderMap {
            let mut headers = axum::http::HeaderMap::new();
            let weekly = reset + Duration::days(1);
            let values = match provider {
                crate::accounts::Provider::Codex => vec![
                    ("x-codex-primary-used-percent", "100".to_owned()),
                    ("x-codex-primary-window-minutes", "300".to_owned()),
                    ("x-codex-primary-reset-at", reset.timestamp().to_string()),
                    ("x-codex-secondary-used-percent", "20".to_owned()),
                    ("x-codex-secondary-window-minutes", "10080".to_owned()),
                    ("x-codex-secondary-reset-at", weekly.timestamp().to_string()),
                ],
                crate::accounts::Provider::Claude => vec![
                    ("anthropic-ratelimit-unified-5h-utilization", "1".to_owned()),
                    (
                        "anthropic-ratelimit-unified-5h-status",
                        "rejected".to_owned(),
                    ),
                    (
                        "anthropic-ratelimit-unified-5h-reset",
                        reset.timestamp().to_string(),
                    ),
                    (
                        "anthropic-ratelimit-unified-7d-utilization",
                        "0.2".to_owned(),
                    ),
                    (
                        "anthropic-ratelimit-unified-7d-status",
                        "allowed".to_owned(),
                    ),
                    (
                        "anthropic-ratelimit-unified-7d-reset",
                        weekly.timestamp().to_string(),
                    ),
                ],
            };
            for (name, value) in values {
                headers.insert(name, value.parse().unwrap());
            }
            headers
        }

        async fn observe_pending(
            &self,
            headers: &axum::http::HeaderMap,
        ) -> (super::super::QuotaObservation, Option<PendingHttp429>) {
            let provider = self.runtime.store.load(&self.id).unwrap().provider;
            let origin = self.runtime.capture_http_origin(&self.id).await;
            let bindings = crate::bindings::BindingTable::new(
                self.home.path(),
                crate::config::RoutingConfig::default(),
            )
            .unwrap();
            let asof = bindings.begin_usage_observation_for(&self.id);
            self.runtime
                .observe_http_429_headers_pending(
                    &self.id,
                    provider,
                    headers,
                    None,
                    (&bindings, asof),
                    origin,
                )
                .await
        }

        async fn pending(&self) -> (PendingHttp429, DateTime<Utc>) {
            let reset =
                DateTime::from_timestamp((Utc::now() + Duration::minutes(10)).timestamp(), 0)
                    .unwrap();
            let provider = self.runtime.store.load(&self.id).unwrap().provider;
            let (observation, pending) =
                self.observe_pending(&Self::headers(provider, reset)).await;
            assert_eq!(observation.quota_until, Some(reset));
            assert!(self.proof().is_none(), "429 headers must never mint early");
            (
                pending.expect("successful qualifying header application must seal"),
                reset,
            )
        }

        fn quota_signal(reset: DateTime<Utc>) -> HealthSignal {
            HealthSignal::Cooling {
                reason: HealthReason::Quota,
                until: reset,
                scope: "account".to_owned(),
            }
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_headers_apply_immediately_final_no_signal_mints_and_cuts_interposed_origin()
     {
        let f = Fixture::new();
        let mut events = f.runtime.subscribe();
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        assert!(
            matches!(&before.health, Health::Cooling { reason: HealthReason::Quota, until, .. } if *until == reset)
        );
        assert_eq!(before.generation, 1);
        assert!(f.runtime.obs.quota_snapshot(&f.id).is_some());
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| event["event"] == "quota_updated")
        );
        let interposed = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        f.runtime
            .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
            .await
            .unwrap();
        assert_eq!(f.proof(), Some(reset));
        assert_eq!(
            f.runtime.store.load(&f.id).unwrap(),
            before,
            "no invented metadata update"
        );
        let _gate = f.runtime.store.transaction().unwrap();
        assert!(
            f.runtime.wait_owner.accept(&f.id, interposed).is_none(),
            "no-signal finish must make the final quota cut"
        );
    }

    #[tokio::test]
    async fn n5a_http_owner_newer_partial_prevents_older_body_mint() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let newer = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let mut partial = f.runtime.obs.quota_snapshot(&f.id).unwrap();
        partial["windows"] = json!([partial["windows"][1].clone()]);
        f.apply(partial, Some(newer)).await;
        f.runtime
            .finish_http_429_report(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                true,
            )
            .await
            .unwrap();
        assert!(f.proof().is_none());
        assert!(matches!(
            f.runtime.store.load(&f.id).unwrap().health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn n5a_http_owner_newer_empty_headers_prevent_older_body_mint() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let (observation, newer) = f.observe_pending(&axum::http::HeaderMap::new()).await;
        assert!(observation.quota_until.is_none());
        assert!(newer.is_none());
        f.runtime
            .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
            .await
            .unwrap();
        assert!(f.proof().is_none());
    }

    #[tokio::test]
    async fn n5a_http_owner_release_and_failed_release_write_prevent_mint() {
        for fail_write in [false, true] {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            if fail_write {
                let mut account = f.runtime.store.load(&f.id).unwrap();
                account.generation = u64::MAX;
                let _gate = f.runtime.store.transaction().unwrap();
                Fixture::write(f.home.path(), &account);
            }
            assert_eq!(
                f.runtime.release_quota_hold(&f.id).await.is_err(),
                fail_write
            );
            let after_release = f.runtime.store.load(&f.id).unwrap();
            f.runtime
                .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
                .await
                .unwrap();
            assert!(f.proof().is_none());
            assert_eq!(f.runtime.store.load(&f.id).unwrap(), after_release);
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_stop_prevents_mint_and_preserves_stopped_native_ok() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        f.runtime.store.stop_writes();
        f.runtime
            .finish_http_429_report(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset + Duration::minutes(1))),
                true,
            )
            .await
            .unwrap();
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
    }

    #[tokio::test]
    async fn n5a_http_owner_unknown_same_health_rejection_revokes_without_write() {
        let f = Fixture::new();
        let before = f.proven().await;
        let ticket = f.ticket();
        f.runtime
            .finish_http_429_report(&f.id, None, None, None, false)
            .await
            .unwrap();
        assert!(!f.admits(&ticket));
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
    }

    #[tokio::test]
    async fn n5a_http_owner_matching_known_codex_body_changes_native_account_and_declines() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        assert!(
            before.quota_credit_releasable,
            "real numeric header reducer precondition"
        );
        let mut expected = before.clone();
        expected.apply_health_signal(
            crate::accounts::HealthSignal::Cooling {
                reason: HealthReason::Quota,
                until: reset,
                scope: "account".into(),
            },
            Utc::now(),
        );
        assert_ne!(
            before, expected,
            "same health tuple is not whole Account equality"
        );
        f.runtime
            .finish_http_429_report(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                true,
            )
            .await
            .unwrap();
        expected.generation += 1;
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), expected);
        assert!(!expected.quota_credit_releasable);
        assert!(
            f.proof().is_none(),
            "preserve C5; do not weaken full Account no-op to a health tuple"
        );
    }

    #[tokio::test]
    async fn n5a_http_owner_unchanged_claude_body_can_mint_after_native_noop() {
        let mut account = crate::accounts::health::tests::fixture();
        account.identity = crate::accounts::Identity::Claude {
            account_uuid: "00000000-0000-4000-8000-000000000065".into(),
            organization_uuid: None,
            email: "synthetic@example.invalid".into(),
        };
        account.id = account.identity.id();
        account.provider = crate::accounts::Provider::Claude;
        let f = Fixture::with_account(account);
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        assert!(!before.quota_credit_releasable);
        f.runtime
            .finish_http_429_report(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                true,
            )
            .await
            .unwrap();
        assert_eq!(f.proof(), Some(reset));
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
    }

    #[tokio::test]
    async fn n5a_http_owner_reset_scope_transport_and_nonquota_conflicts_decline_but_apply_native()
    {
        for case in 0..5 {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            let (known, signal, qualified) = match case {
                0 => (Some(reset + Duration::seconds(1)), None, true),
                1 => (None, None, true),
                2 => (Some(reset), None, false),
                3 => (
                    Some(reset),
                    Some(HealthSignal::Cooling {
                        reason: HealthReason::Quota,
                        until: reset,
                        scope: "model:synthetic-other".into(),
                    }),
                    true,
                ),
                _ => (Some(reset), Some(HealthSignal::Ready), true),
            };
            let before = f.runtime.store.load(&f.id).unwrap();
            let mut expected = before.clone();
            if case == 3 {
                expected.apply_health_signal(
                    crate::accounts::HealthSignal::Cooling {
                        reason: HealthReason::Quota,
                        until: reset,
                        scope: "model:synthetic-other".into(),
                    },
                    Utc::now(),
                );
            } else if case == 4 {
                expected.apply_health_signal(crate::accounts::HealthSignal::Ready, Utc::now());
            }
            if expected != before {
                expected.generation += 1;
            }
            f.runtime
                .finish_http_429_report(&f.id, Some(pending), known, signal, qualified)
                .await
                .unwrap();
            assert!(f.proof().is_none(), "case {case}");
            assert_eq!(
                f.runtime.store.load(&f.id).unwrap(),
                expected,
                "case {case} native behavior"
            );
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_header_failure_is_swallowed_without_pending_or_early_proof() {
        let mut account = crate::accounts::health::tests::fixture();
        account.generation = u64::MAX;
        let f = Fixture::with_account(account.clone());
        let reset = Utc::now() + Duration::minutes(10);
        let (observation, pending) = f
            .observe_pending(&Fixture::headers(account.provider, reset))
            .await;
        assert!(observation.quota_until.is_none());
        assert!(pending.is_none());
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), account);
    }

    #[tokio::test]
    async fn n5a_http_owner_body_persistence_failure_propagates_without_mint() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let mut account = f.runtime.store.load(&f.id).unwrap();
        account.generation = u64::MAX;
        {
            let _gate = f.runtime.store.transaction().unwrap();
            Fixture::write(f.home.path(), &account);
        }
        assert!(
            f.runtime
                .finish_http_429_report(
                    &f.id,
                    Some(pending),
                    Some(reset),
                    Some(Fixture::quota_signal(reset)),
                    true
                )
                .await
                .is_err()
        );
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), account);
    }

    #[tokio::test]
    async fn n5a_http_owner_no_signal_missing_or_malformed_metadata_declines_without_native_error()
    {
        for missing in [false, true] {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            let path = f
                .home
                .path()
                .join("accounts")
                .join(format!("{}.json", f.id));
            {
                let _gate = f.runtime.store.transaction().unwrap();
                if missing {
                    std::fs::remove_file(&path).unwrap();
                } else {
                    std::fs::write(&path, b"{malformed synthetic metadata").unwrap();
                }
            }
            f.runtime
                .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
                .await
                .unwrap();
            assert!(f.proof().is_none());
            assert_eq!(path.exists(), !missing, "no metadata recreation/write");
            if !missing {
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    b"{malformed synthetic metadata"
                );
            }
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_no_signal_current_metadata_applicability_is_rechecked() {
        for case in 0..7 {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            let mut changed = f.runtime.store.load(&f.id).unwrap();
            match case {
                0 => changed.drain = true,
                1 => changed.user_state = crate::accounts::UserState::Paused,
                2 => changed.auth_failures = 1,
                3 => changed.payment_until = Some(reset),
                4 => changed.allow_credits = !changed.allow_credits,
                5 => changed.quota_cooling.as_mut().unwrap().window = "7d".into(),
                _ => {
                    if let Health::Cooling { scope, .. } = &mut changed.health {
                        *scope = "model:synthetic-other".into();
                    }
                }
            }
            // Simulate current metadata disagreement without relying on the
            // prehook. The optional no-signal check must independently refuse.
            {
                let _gate = f.runtime.store.transaction().unwrap();
                Fixture::write(f.home.path(), &changed);
            }
            f.runtime
                .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
                .await
                .unwrap();
            assert!(f.proof().is_none(), "case {case}");
            assert_eq!(
                f.runtime.store.load(&f.id).unwrap(),
                changed,
                "case {case} no native mutation"
            );
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_store_gate_failure_preserves_signal_dependent_native_precedence() {
        for has_signal in [false, true] {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _gate = f.runtime.store.transaction().unwrap();
                    panic!("synthetic Store gate poison");
                }))
                .is_err()
            );
            let result = f
                .runtime
                .finish_http_429_report(
                    &f.id,
                    Some(pending),
                    Some(reset),
                    has_signal.then(|| Fixture::quota_signal(reset)),
                    true,
                )
                .await;
            assert_eq!(
                result.is_err(),
                has_signal,
                "no signal has no native body write failure"
            );
            assert!(matches!(
                f.runtime.wait_owner.proofs.acquire(&f.id),
                Err(super::super::wait_proof::FenceError::Closed)
            ));
        }
    }

    #[test]
    fn n5a_http_owner_compact_candidate_preserves_raw_age_future_and_every_window_reset() {
        let at = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
        let mut proof = ShortQuotaProof {
            reset_at: at + Duration::minutes(10),
            model: None,
            observed_at: at,
            earliest_reset_at: at + Duration::minutes(20),
            started_at: at,
            scope: "account".into(),
            allow_credits: false,
            quota_credit_releasable: false,
        };
        assert!(proof.valid_at(at + Duration::seconds(300)));
        assert!(!proof.valid_at(at + Duration::seconds(300) + Duration::nanoseconds(1)));
        assert!(!proof.valid_at(at - Duration::nanoseconds(1)));
        proof.started_at = at + Duration::seconds(10);
        assert!(!proof.valid_at(proof.started_at - Duration::nanoseconds(1)));
        assert!(proof.valid_at(proof.started_at));
        proof.started_at = at;
        proof.earliest_reset_at = at + Duration::seconds(30);
        assert!(proof.valid_at(at + Duration::seconds(29)));
        assert!(!proof.valid_at(at + Duration::seconds(30)));
        assert!(
            proof.reset_at > at + Duration::seconds(30),
            "nonlimiting window may expire first"
        );
    }

    #[tokio::test]
    async fn n5a_owner_introducing_report_uses_real_delayed_reducer_without_backdating() {
        let f = Fixture::new();
        let raw = Fixture::raw();
        let observed: DateTime<Utc> = raw["observed_at"].as_str().unwrap().parse().unwrap();
        let origin = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let held = f.apply(raw, Some(origin)).await;
        let evidence = held.quota_cooling.unwrap();
        assert!(observed < evidence.started_at);
        assert_eq!(f.proof(), Some(evidence.reset_at));
        let persisted = f.runtime.store.load(&f.id).unwrap();
        assert_eq!(persisted.generation, 1);
        assert_eq!(
            persisted.quota_cooling.unwrap().started_at,
            evidence.started_at
        );
    }

    #[tokio::test]
    async fn n5a_owner_newer_partial_blocks_older_complete_through_actual_store() {
        let f = Fixture::new();
        let full = Fixture::raw();
        let a = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let b = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let mut partial = full.clone();
        partial["windows"] = json!([full["windows"][1].clone()]);
        f.apply(partial, Some(b)).await;
        assert!(f.proof().is_none());
        let held = f.apply(full, Some(a)).await;
        assert!(matches!(
            held.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        assert!(
            f.proof().is_none(),
            "older report still applies native C5 but cannot mint"
        );
    }

    #[tokio::test]
    async fn n5a_owner_empty_http_receipt_consumes_origin_before_older_complete() {
        let f = Fixture::new();
        let a = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let b = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let bindings = crate::bindings::BindingTable::new(
            f.home.path(),
            crate::config::RoutingConfig::default(),
        )
        .unwrap();
        let asof = bindings.begin_usage_observation_for(&f.id);
        f.runtime
            .observe_headers_asof_origin(
                &f.id,
                crate::accounts::Provider::Codex,
                &axum::http::HeaderMap::new(),
                None,
                (&bindings, asof),
                Some(b),
            )
            .await;
        let held = f.apply(Fixture::raw(), Some(a)).await;
        assert!(held.quota_cooling.is_some());
        assert!(
            f.proof().is_none(),
            "empty headers must advance accepted ordering"
        );
    }

    #[tokio::test]
    async fn n5a_owner_older_native_apply_conservatively_revokes_newer_proof() {
        let f = Fixture::new();
        let raw = Fixture::raw();
        let a = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let b = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let held = f.apply(raw.clone(), Some(b)).await;
        let ticket = f.ticket();
        let later = f.apply(raw, Some(a)).await;
        assert_eq!(later.health, held.health);
        assert!(!f.admits(&ticket));
        assert!(
            f.proof().is_none(),
            "ordinary stale apply is intentionally conservative"
        );
    }

    #[tokio::test]
    async fn n5a_owner_cached_merge_and_public_apply_cannot_borrow_raw_origin() {
        let f = Fixture::new();
        f.proven().await;
        let raw = Fixture::raw();
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let partial =
            json!({"observed_at":raw["observed_at"],"windows":[raw["windows"][0].clone()]});
        f.runtime
            .apply_quota_snapshot_reported_origin(&f.id, raw.clone(), &partial, None, Some(origin))
            .await
            .unwrap();
        assert!(f.proof().is_none(), "cache-complete is not raw-complete");
        f.runtime
            .apply_quota_snapshot(&f.id, raw, None)
            .await
            .unwrap();
        assert!(f.proof().is_none(), "public apply never fabricates origin");
    }

    #[tokio::test]
    async fn n5a_owner_failed_release_write_revokes_live_ticket_and_inflight_origin() {
        let f = Fixture::new();
        let mut account = f.proven().await;
        let ticket = f.ticket();
        let inflight = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        // Deterministic native persistence failure, after the real Release hook:
        // write_metadata_record cannot increment MAX generation. No ACL timing.
        account.generation = u64::MAX;
        {
            let _gate = f.runtime.store.transaction().unwrap();
            Fixture::write(f.home.path(), &account);
        }
        assert!(f.runtime.release_quota_hold(&f.id).await.is_err());
        assert!(!f.admits(&ticket));
        assert!(f.proof().is_none());
        assert!(matches!(
            f.runtime.store.load(&f.id).unwrap().health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        let raw = Fixture::raw();
        let _ = f
            .runtime
            .apply_quota_snapshot_reported_origin(&f.id, raw.clone(), &raw, None, Some(inflight))
            .await;
        assert!(f.proof().is_none());
    }

    #[tokio::test]
    async fn n5a_owner_stop_closes_capture_and_stopped_ok_cannot_mint() {
        let f = Fixture::new();
        f.proven().await;
        let ticket = f.ticket();
        let inflight = f.runtime.capture_http_origin(&f.id).await.unwrap();
        f.runtime.store.stop_writes();
        assert!(f.runtime.capture_http_origin(&f.id).await.is_none());
        assert!(f.runtime.capture_poll_origin(&f.id).await.is_none());
        let prior = f.runtime.store.load(&f.id).unwrap();
        let stopped_ok = f.apply(Fixture::raw(), Some(inflight)).await;
        assert_eq!(stopped_ok, prior);
        assert!(!f.admits(&ticket));
        assert!(f.proof().is_none());
        f.runtime.store.stop_writes();
        assert!(f.runtime.capture_http_origin(&f.id).await.is_none());
    }

    #[tokio::test]
    async fn n5a_owner_automatic_expiry_revokes_the_reports_own_rotation() {
        let f = Fixture::new();
        let mut account = crate::accounts::health::tests::fixture();
        let until = Utc::now() - Duration::seconds(1);
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until,
            scope: "account".into(),
        };
        account.quota_cooling = Some(QuotaCooling {
            window: "5h".into(),
            reset_at: until,
            started_at: until - Duration::minutes(5),
        });
        // Seed after constructor baseline (which normally persists expiry).
        // Capture reads current metadata without performing that publication.
        {
            let _gate = f.runtime.store.transaction().unwrap();
            Fixture::write(f.home.path(), &account);
        }
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let held = f.apply(Fixture::raw(), Some(origin)).await;
        assert!(held.quota_cooling.is_some());
        assert!(
            f.proof().is_none(),
            "independent actual expiry cannot inherit own-report permit"
        );
    }

    #[tokio::test]
    async fn n5a_owner_unknown_rejection_without_store_write_revokes() {
        let f = Fixture::new();
        let before = f.proven().await;
        let ticket = f.ticket();
        f.runtime.invalidate_quota_rejection(&f.id).await;
        assert!(!f.admits(&ticket));
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
    }

    #[tokio::test]
    async fn n5a_owner_oversized_model_refuses_only_optional_proof() {
        let f = Fixture::new();
        let origin = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let raw = Fixture::raw();
        let model = "m".repeat(MAX_MODEL_BYTES + 1);
        let held = f
            .runtime
            .apply_quota_snapshot_reported_origin(
                &f.id,
                raw.clone(),
                &raw,
                Some(&model),
                Some(origin),
            )
            .await
            .unwrap();
        assert!(held.quota_cooling.is_some());
        assert!(f.proof().is_none());
    }
}

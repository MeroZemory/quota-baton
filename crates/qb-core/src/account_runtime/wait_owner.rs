//! Process-local attribution only. No wait selection or dispatch is enabled here.

use super::{
    AccountRuntime,
    wait_proof::{AcceptPermit, ReportOrigin, ReportSource, WaitProofs},
};
use crate::accounts::{Account, AccountStore, BeforeAccountMutation, ReportMutation};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::sync::{Arc, Weak};

const CAPACITY: usize = 4096;
const MAX_MODEL_BYTES: usize = 256;

/// Plain bounded classifier data; retained evidence is not admission authority.
pub(super) struct ShortQuotaProof {
    reset_at: DateTime<Utc>,
    model: Option<String>,
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
    }
}

impl ReportApplication {
    pub(super) fn finish(mut self, candidate: Option<ShortQuotaProof>) {
        let Some(proof) = candidate else { return };
        // Persistence may have crossed the deadline. Refuse only optional proof.
        if proof.reset_at <= Utc::now()
            || proof
                .model
                .as_ref()
                .is_some_and(|model| model.len() > MAX_MODEL_BYTES)
        {
            return;
        }
        if let Some(owner) = self.owner.upgrade()
            && let Some(permit) = self.permit.take()
        {
            let _ = owner.proofs.mint(permit, proof);
        }
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
    })
}

impl AccountRuntime {
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
    use chrono::Duration;
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

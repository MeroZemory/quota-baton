//! Staged child of account_runtime::wait_owner; runtime/Store units, not HTTP acceptance.
//! Proof enters only through capture_poll_origin + the actual reported reducer.

use super::{AccountRuntime, AdmittedQuotaWait};
use crate::{
    accounts::{Account, AccountStore, Health, HealthReason, Identity, Provider, Tokens},
    bindings::{BindingTable, Candidate, Decision, HttpWaitRequest, LaunchRequest},
    config::RoutingConfig,
    issue_operation::IssueOperation,
    observability::Observability,
    routing::{Mode, Settings, session_id},
};
use chrono::{DateTime, Duration, Utc};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, mpsc},
    time::Duration as StdDuration,
};
use tokio::time::Instant;

const KEY: &str = "codex:synthetic-runtime-wait";
const LAUNCH: &str = "launch:synthetic-runtime-wait";
const MODEL: &str = "synthetic-model-1";
const OTHER_MODEL: &str = "synthetic-model-2";
const SAFETY_TIMEOUT: StdDuration = StdDuration::from_secs(10);

struct Fixture {
    runtime: Arc<AccountRuntime>,
    table: Arc<BindingTable>,
    id: String,
    model: Option<&'static str>,
    // All native owners and their synthetic files are dropped before the directory.
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_credentials("synthetic-access", None)
    }

    fn with_credentials(access: &str, expires_at: Option<DateTime<Utc>>) -> Self {
        Self::build(access, expires_at, false)
    }

    fn managed() -> Self {
        Self::build("synthetic-access", None, true)
    }

    fn build(access: &str, expires_at: Option<DateTime<Utc>>, managed: bool) -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r13-part-c/wait-runtime-tests/fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::Builder::new()
            .prefix("synthetic-runtime-")
            .tempdir_in(base)
            .unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let mut account = crate::accounts::health::tests::fixture();
        account.token_meta.expires_at = expires_at;
        let account = store.save(&account, &Self::tokens(access), None).unwrap();
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let runtime = AccountRuntime::new(store, obs, 0.0);
        let table = BindingTable::new(home.path(), RoutingConfig::default()).unwrap();
        let candidate = Candidate {
            account: account.clone(),
            quota: None,
            inflight: 0,
        };
        if managed {
            table
                .place_launch(
                    LaunchRequest {
                        anchor_key: LAUNCH,
                        resume_key: None,
                        model: Some(MODEL),
                        model_is_explicit: true,
                        account_pref: Some(&account.id),
                        allow_move_for_model: false,
                    },
                    &[candidate],
                    &BTreeMap::from([(
                        account.id.clone(),
                        BTreeSet::from([MODEL.to_owned(), OTHER_MODEL.to_owned()]),
                    )]),
                    Utc::now(),
                )
                .unwrap();
            table
                .resolve_launch(Some(LAUNCH), Some(KEY), Utc::now())
                .unwrap();
            assert!(!table.existing_placement(Some(KEY), &account.id));
            // Native launch placement is provisional until real dispatch accounting.
            // This unit performs that native transition; it does not claim HTTP I/O.
            let lease = table.begin_request(Some(KEY), &account.id);
            lease.dispatched();
            drop(lease);
        } else {
            match table.choose(
                Provider::Codex,
                Some(KEY),
                None,
                None,
                &[candidate],
                None,
                0,
            ) {
                Decision::Selected {
                    account: selected,
                    lease,
                    ..
                } => {
                    assert_eq!(selected, account.id);
                    lease.dispatched();
                    drop(lease);
                }
                other => panic!("native synthetic initial binding: {other:?}"),
            }
        }
        assert!(table.existing_placement(Some(KEY), &account.id));
        Self {
            runtime,
            table,
            id: account.id,
            model: managed.then_some(MODEL),
            home,
        }
    }

    fn tokens(access: &str) -> Tokens {
        Tokens {
            access_token: access.to_owned(),
            refresh_token: "synthetic-refresh".into(),
            id_token: None,
        }
    }

    fn operation() -> IssueOperation {
        IssueOperation::new(Instant::now() + StdDuration::from_secs(90))
    }

    fn request<'a>(&'a self, operation: &'a IssueOperation) -> HttpWaitRequest<'a> {
        HttpWaitRequest {
            provider: Provider::Codex,
            key: Some(KEY),
            model: self.model,
            expected_source: Some(&self.id),
            operation,
        }
    }

    fn admit(&self, operation: &IssueOperation) -> Option<AdmittedQuotaWait> {
        self.runtime
            .current_http_quota_wait(&self.table, self.request(operation))
            .expect("native runtime wait inspection must remain available")
    }

    fn is_current(&self, operation: &IssueOperation, receipt: &AdmittedQuotaWait) -> bool {
        self.runtime
            .current_http_quota_wait_is_current(&self.table, self.request(operation), receipt)
            .expect("native runtime wait revalidation must remain available")
    }

    fn account(&self) -> Account {
        self.runtime.store.load(&self.id).unwrap()
    }

    fn raw(reset: DateTime<Utc>) -> Value {
        let at = Utc::now();
        json!({"source":"poll","observed_at":at,"windows":[
            {"name":"5h","used_pct":100.0,"observed_at":at,"reset_at":reset},
            {"name":"7d","used_pct":20.0,"observed_at":at,"reset_at":at+Duration::days(1)}
        ]})
    }

    async fn prove(&self) -> DateTime<Utc> {
        // Capture before constructing the report, not at its acceptance boundary.
        let origin = self.runtime.capture_poll_origin(&self.id).await.unwrap();
        let reset = Utc::now() + Duration::seconds(45);
        let raw = Self::raw(reset);
        let snapshot = self
            .runtime
            .obs
            .observe_poll(&self.id, raw.clone(), true)
            .unwrap();
        let account = self
            .runtime
            .apply_quota_snapshot_reported_origin(&self.id, snapshot, &raw, None, Some(origin))
            .await
            .unwrap();
        assert!(matches!(account.health, Health::Cooling {
            reason: HealthReason::Quota, until, ..
        } if until == reset));
        assert!(self.has_current_proof(), "actual poll report must qualify");
        reset
    }

    fn has_current_proof(&self) -> bool {
        let _gate = self.runtime.store.transaction().unwrap();
        let account = self.runtime.store.read_account_unlocked(&self.id).unwrap();
        self.runtime
            .wait_owner
            .acquire_current_unlocked(&account, None, Utc::now())
            .is_some()
    }

    fn policy(&self, seconds: u64) {
        self.table
            .set_routing_session(
                &session_id(KEY),
                Some(Settings {
                    mode: Mode::Strict,
                    reset_wait_secs: seconds,
                }),
                None,
                false,
            )
            .unwrap();
    }

    // Fixture fault injection changes only our saved synthetic record, under the
    // native transaction. Preserve the real encrypted token bytes and metadata
    // except for the explicit fault; no owner authority helper is called.
    fn edit_record(&self, update: impl FnOnce(&mut Value)) {
        let _gate = self.runtime.store.transaction().unwrap();
        let path = self
            .home
            .path()
            .join("accounts")
            .join(format!("{}.json", self.id));
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        update(&mut record);
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    }
}

#[tokio::test]
async fn n5a_runtime_wait_requires_report_origin_and_preserves_native_state() {
    let f = Fixture::new();
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    assert!(f.admit(&operation).is_none());
    // A complete saved/report-only hold is not a causal proof.
    let raw = Fixture::raw(Utc::now() + Duration::seconds(45));
    f.runtime
        .apply_quota_snapshot_reported(&f.id, raw.clone(), &raw, None)
        .await
        .unwrap();
    assert!(!f.has_current_proof());
    assert!(f.admit(&operation).is_none());

    let reset = f.prove().await;
    let account = f.account();
    let view = f.table.routing_view();
    let receipt = f
        .admit(&operation)
        .expect("actual current poll proof must admit WAIT");
    assert_eq!(receipt.account(), f.id);
    assert_eq!(receipt.key(), KEY);
    assert_eq!(receipt.model(), None);
    assert_eq!(receipt.reset_at(), reset);
    assert_eq!(receipt.deadline(), operation.deadline());
    assert!(receipt.wake() > Instant::now());
    assert!(receipt.wake() < receipt.deadline());
    assert!(f.is_current(&operation, &receipt));
    assert!(!f.table.account_dispatch_eligible(
        &Candidate {
            account: account.clone(),
            quota: None,
            inflight: 0
        },
        None
    ));
    assert_eq!(f.account(), account);
    assert_eq!(f.table.current(Some(KEY)).as_deref(), Some(f.id.as_str()));
    assert_eq!(f.table.pending_count(&f.id), 0);
    assert_eq!(f.table.routing_view(), view);
}

#[tokio::test]
async fn n5a_runtime_wait_rejects_wrong_identity_and_changed_managed_model() {
    let f = Fixture::new();
    f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    let receipt = f.admit(&operation).unwrap();
    for case in ["source", "provider", "key", "missing-key"] {
        let mut request = f.request(&operation);
        match case {
            "source" => request.expected_source = Some("synthetic-other-source"),
            "provider" => request.provider = Provider::Claude,
            "key" => request.key = Some("codex:synthetic-unbound"),
            "missing-key" => request.key = None,
            _ => unreachable!(),
        }
        assert!(
            f.runtime
                .current_http_quota_wait(&f.table, request)
                .unwrap()
                .is_none(),
            "{case}"
        );
    }
    assert!(f.is_current(&operation, &receipt));
    let mut known_request = f.request(&operation);
    known_request.model = Some(OTHER_MODEL);
    let known_receipt = f
        .runtime
        .current_http_quota_wait(&f.table, known_request)
        .unwrap()
        .expect("full account poll covers a request with a known model");
    assert_eq!(known_receipt.model(), Some(OTHER_MODEL));
    assert!(!f.is_current(&operation, &known_receipt));
    let mut known_request = f.request(&operation);
    known_request.model = Some(OTHER_MODEL);
    assert!(
        !f.runtime
            .current_http_quota_wait_is_current(&f.table, known_request, &receipt)
            .unwrap(),
        "account applicability never retargets an existing request receipt"
    );
    let managed = Fixture::managed();
    managed.prove().await;
    let managed_receipt = managed
        .admit(&operation)
        .expect("full account poll covers the exact current managed model");
    assert_eq!(managed_receipt.model(), Some(MODEL));
    assert!(managed.is_current(&operation, &managed_receipt));
    managed
        .table
        .record_launch_model(KEY, &managed.id, OTHER_MODEL, Utc::now())
        .unwrap();
    assert!(
        managed.has_current_proof(),
        "model publication does not invent a new quota proof"
    );
    assert!(
        managed.admit(&operation).is_none(),
        "stale model request must refuse"
    );
    let mut request = managed.request(&operation);
    request.model = Some(OTHER_MODEL);
    let changed = managed
        .runtime
        .current_http_quota_wait(&managed.table, request)
        .unwrap()
        .expect("account poll supports a fresh receipt for the new current model");
    assert_eq!(changed.model(), Some(OTHER_MODEL));
    let mut request = managed.request(&operation);
    request.model = Some(OTHER_MODEL);
    assert!(
        !managed
            .runtime
            .current_http_quota_wait_is_current(&managed.table, request, &managed_receipt)
            .unwrap(),
        "old managed receipt must not cross model publication"
    );
    let mut unknown = managed.request(&operation);
    unknown.model = None;
    assert!(
        managed
            .runtime
            .current_http_quota_wait(&managed.table, unknown)
            .unwrap()
            .is_none(),
        "missing managed model cannot bypass the binding model check"
    );
}

#[tokio::test]
async fn n5a_runtime_wait_rejects_actual_binding_source_change() {
    let f = Fixture::new();
    f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    let receipt = f.admit(&operation).unwrap();
    let mut other = crate::accounts::health::tests::fixture();
    other.identity = Identity::Codex {
        user_id: None,
        account_id: "synthetic-other-source".into(),
        email: "synthetic-other@example.invalid".into(),
    };
    other.id = other.identity.id();
    other.email = "synthetic-other@example.invalid".into();
    let other = f
        .runtime
        .store
        .save(&other, &Fixture::tokens("synthetic-other-access"), None)
        .unwrap();
    let selected = f.table.choose(
        Provider::Codex,
        Some(KEY),
        None,
        None,
        &[Candidate {
            account: other.clone(),
            quota: None,
            inflight: 0,
        }],
        Some(&other.id),
        0,
    );
    match selected {
        Decision::Selected { account, lease, .. } => {
            assert_eq!(account, other.id);
            lease.dispatched();
            drop(lease);
        }
        other => panic!("native synthetic source move must select: {other:?}"),
    }
    assert!(
        f.has_current_proof(),
        "the old account's proof remains unrelated to the new binding"
    );
    assert_eq!(
        f.table.current(Some(KEY)).as_deref(),
        Some(other.id.as_str())
    );
    assert!(f.admit(&operation).is_none());
    assert!(!f.is_current(&operation, &receipt));
    let mut request = f.request(&operation);
    request.expected_source = Some(&other.id);
    assert!(
        f.runtime
            .current_http_quota_wait(&f.table, request)
            .unwrap()
            .is_none(),
        "new bound account has no qualifying report proof"
    );
}

#[tokio::test]
async fn n5a_runtime_wait_uses_native_policy_and_preserves_same_receipt_on_unrelated_edit() {
    let f = Fixture::new();
    f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    f.policy(60);
    let receipt = f.admit(&operation).unwrap();
    let clocks = (receipt.reset_at(), receipt.wake(), receipt.deadline());
    f.table
        .set_routing_session(
            &session_id("codex:synthetic-unrelated"),
            Some(Settings {
                mode: Mode::Strict,
                reset_wait_secs: 30,
            }),
            None,
            false,
        )
        .unwrap();
    assert!(f.table.policies.plan(Some(KEY)).revision > receipt.policy_revision());
    assert!(
        f.is_current(&operation, &receipt),
        "unrelated global revision must not restart or veto WAIT"
    );
    assert_eq!(
        (receipt.reset_at(), receipt.wake(), receipt.deadline()),
        clocks
    );

    f.table.policies.begin_recovery();
    assert!(f.admit(&operation).is_none());
    assert!(!f.is_current(&operation, &receipt));
    f.table.policies.finish_recovery();
    assert!(f.is_current(&operation, &receipt));
    f.policy(10);
    assert!(
        f.admit(&operation).is_none(),
        "current reset exceeds native ten-second policy"
    );
    assert!(!f.is_current(&operation, &receipt));
    f.policy(0);
    assert!(f.admit(&operation).is_none());
    assert!(!f.is_current(&operation, &receipt));
    assert!(
        f.has_current_proof(),
        "policy rejection must not be disguised as missing proof"
    );
    f.table.policies.freeze();
    assert!(f.admit(&operation).is_none());
}

#[tokio::test]
async fn n5a_runtime_wait_requires_current_real_saved_credentials() {
    for case in ["empty", "expired", "corrupt"] {
        let f = Fixture::with_credentials(
            if case == "empty" {
                ""
            } else {
                "synthetic-access"
            },
            (case == "expired").then(|| Utc::now() - Duration::hours(1)),
        );
        f.prove().await;
        if case == "corrupt" {
            f.edit_record(|record| {
                record["secret_dpapi_b64"] = json!("synthetic-invalid-ciphertext")
            });
        }
        assert!(
            f.has_current_proof(),
            "{case}: credential gate must have a real proof to reject"
        );
        let operation = Fixture::operation();
        let _caller = operation.caller_guard();
        assert!(
            f.admit(&operation).is_none(),
            "{case}: current credentials must refuse WAIT"
        );
        assert_eq!(f.table.pending_count(&f.id), 0);
    }
}

#[tokio::test]
async fn n5a_runtime_wait_checks_probe_volatile_release_pending_and_stop() {
    let f = Fixture::new();
    f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    let receipt = f.admit(&operation).unwrap();
    let mut probe_account = f.account();
    probe_account.auth_failures = 1; // Probe's real admission condition, not a durable edit.
    let probe = f.runtime.begin_probe(&probe_account).unwrap().unwrap();
    assert!(f.has_current_proof());
    assert!(f.admit(&operation).is_none());
    assert!(!f.is_current(&operation, &receipt));
    drop(probe);
    assert!(f.is_current(&operation, &receipt));

    let account = f.account();
    let mut blocked = account.clone();
    blocked.health = Health::Cooling {
        reason: HealthReason::StoreError,
        until: Utc::now() + Duration::minutes(1),
        scope: "account".into(),
    };
    f.runtime.emit_account(&blocked);
    assert!(f.has_current_proof());
    assert!(f.admit(&operation).is_none());
    assert!(!f.is_current(&operation, &receipt));
    f.runtime.clear_memory_health(&account);
    assert!(f.is_current(&operation, &receipt));

    // Isolate the actual runtime pending-marker gate without revoking proof by
    // a separate durable Release. This fixture state is never a proof source.
    {
        let _gate = f.runtime.store.transaction().unwrap();
        f.runtime.released_quota.lock().unwrap().insert(
            f.id.clone(),
            super::super::ReleasedQuota {
                at: Utc::now(),
                pending: true,
                signals: Default::default(),
                signal_times: Default::default(),
                window_statuses: Default::default(),
            },
        );
    }
    assert!(f.has_current_proof());
    assert!(f.admit(&operation).is_none());
    assert!(!f.is_current(&operation, &receipt));
    {
        let _gate = f.runtime.store.transaction().unwrap();
        f.runtime
            .released_quota
            .lock()
            .unwrap()
            .get_mut(&f.id)
            .unwrap()
            .pending = false;
    }
    assert!(f.is_current(&operation, &receipt));
    f.runtime.store.stop_writes();
    assert!(f.admit(&operation).is_none());
    assert!(!f.is_current(&operation, &receipt));
    assert!(f.runtime.capture_poll_origin(&f.id).await.is_none());
}

#[tokio::test]
async fn n5a_runtime_wait_keeps_original_deadline_and_caller_after_issue_admission() {
    let f = Fixture::new();
    f.prove().await;
    let original = Fixture::operation();
    let caller = original.caller_guard();
    let receipt = f.admit(&original).unwrap();
    for deadline in [Instant::now(), Instant::now() + StdDuration::from_secs(5)] {
        let short = IssueOperation::new(deadline);
        let _short_caller = short.caller_guard();
        assert!(
            f.admit(&short).is_none(),
            "reset must precede the actual caller's original deadline"
        );
        assert!(!f.is_current(&short, &receipt));
    }
    assert_eq!(receipt.deadline(), original.deadline());
    assert!(f.is_current(&original, &receipt));
    original.admit().unwrap();
    drop(caller);
    assert!(
        original.check().is_ok(),
        "native issue completion remains admitted"
    );
    assert!(!original.can_reply());
    assert!(f.admit(&original).is_none());
    assert!(!f.is_current(&original, &receipt));
}

#[tokio::test]
async fn n5a_runtime_wait_failed_native_release_revokes_receipt_before_failed_write() {
    let f = Fixture::new();
    let reset = f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    let receipt = f.admit(&operation).unwrap();
    let inflight = f.runtime.capture_poll_origin(&f.id).await.unwrap();
    // Generation overflow is deterministic and occurs after the actual Release
    // pre-mutation hook, before native metadata persistence. Keep real tokens.
    f.edit_record(|record| record["generation"] = json!(u64::MAX));
    assert!(f.is_current(&operation, &receipt));
    assert!(f.admit(&operation).is_some());
    let changed = f.runtime.wait_owner.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    assert!(f.runtime.release_quota_hold(&f.id).await.is_err());
    assert!(
        changed.as_mut().now_or_never().is_some(),
        "failed native Release must wake receipt revalidation"
    );
    let account = f.account();
    assert_eq!(account.generation, u64::MAX);
    assert!(matches!(account.health, Health::Cooling {
        reason: HealthReason::Quota, until, ..
    } if until == reset));
    assert!(
        f.runtime.store.tokens(&account).is_ok(),
        "failure must not be a credential rejection"
    );
    assert!(!f.has_current_proof());
    assert!(!f.is_current(&operation, &receipt));
    assert!(
        f.admit(&operation).is_none(),
        "failed Release cannot reuse unchanged saved Cooling as authority"
    );
    let raw = Fixture::raw(reset);
    let _ = f
        .runtime
        .apply_quota_snapshot_reported_origin(&f.id, raw.clone(), &raw, None, Some(inflight))
        .await;
    assert!(!f.has_current_proof());
    assert!(f.admit(&operation).is_none());
}

#[tokio::test]
async fn n5a_runtime_wait_real_store_barrier_orders_failed_release_before_admission() {
    let f = Arc::new(Fixture::new());
    f.prove().await;
    let operation = Fixture::operation();
    let _caller = operation.caller_guard();
    let receipt = f.admit(&operation).unwrap();
    f.edit_record(|record| record["generation"] = json!(u64::MAX));
    assert!(f.is_current(&operation, &receipt));
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);

    // Establish actual Store ownership BEFORE starting the competing reader.
    // No scoped-thread Drop/join can block the test's timeout failure path.
    let gate = f.runtime.store.transaction().unwrap();
    let worker = {
        let runtime = f.runtime.clone();
        let table = f.table.clone();
        let operation = operation.clone();
        let id = f.id.clone();
        let fixture_owner = f.clone();
        std::thread::spawn(move || {
            // Keep project-local synthetic files alive even if the bounded
            // receiver times out while this owned thread is still blocked.
            let _fixture_owner = fixture_owner;
            if started_tx.send(()).is_err() {
                return;
            }
            let result = runtime.current_http_quota_wait(
                &table,
                crate::bindings::HttpWaitRequest {
                    provider: Provider::Codex,
                    key: Some(KEY),
                    model: None,
                    expected_source: Some(&id),
                    operation: &operation,
                },
            );
            // Receipt is dropped after native gates, before reporting. Capacity
            // one means this sole result cannot block waiting for the receiver.
            let _ = finished_tx.send(result.map(|receipt| receipt.is_some()));
        })
    };
    started_rx
        .recv_timeout(SAFETY_TIMEOUT)
        .expect("admission caller must start");
    // A started message does not assert the reader reached a particular lock.
    // The held Store transaction excludes its inspection in either scheduling.
    let result = f.runtime.store.update_metadata_with_cause_unlocked(
        &f.id,
        crate::accounts::MetadataCause::Mutation(crate::accounts::MetadataMutation::Release),
        |account| {
            account.health = Health::Ready;
            account.quota_cooling = None;
            account.quota_credit_releasable = false;
        },
    );
    assert!(result.is_err());
    assert!(matches!(
        f.runtime.store.read_account_unlocked(&f.id).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    drop(gate);
    let admitted = finished_rx
        .recv_timeout(SAFETY_TIMEOUT)
        .expect("runtime admission must finish after Store release")
        .expect("native runtime inspection must remain available");
    // No unconditional join: successful channel receive proves the native API
    // returned; a blocked thread is detached on failure instead of hanging CI.
    drop(worker);
    assert!(
        !admitted,
        "actual Store barrier must expose failed Release invalidation to admission"
    );
    assert!(!f.is_current(&operation, &receipt));
    assert!(!f.has_current_proof());
}

mod renewal_tests;

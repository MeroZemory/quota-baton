//! Synthetic-only takeover transaction and crash recovery regressions.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[tokio::test]
async fn r10_new_takeover_leaves_label_unset() {
    let f = Fixture::new();
    let plan = f.plan().await;
    assert!(plan.account.label.is_empty());
    let result = install(&f.store, &f.deps, plan).unwrap();
    assert!(f.store.load(&result.account.id).unwrap().label.is_empty());
}

#[tokio::test]
async fn r6_installed_retry_rechecks_owner_before_any_retirement_side_effect() {
    let f = Fixture::new();
    let plan = f.plan().await;
    let installed = f.store.publish_takeover(&plan).unwrap();
    f.source.running.store(true, Ordering::SeqCst);
    assert!(matches!(
        install(&f.store, &f.deps, plan.clone()),
        Err(TakeoverError::Conflict)
    ));
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
    assert!(
        !f.store
            .read_takeover_journal(&plan.operation)
            .unwrap()
            .unwrap()
            .retired
    );
    assert_eq!(
        f.store.load(&installed.id).unwrap().generation,
        installed.generation
    );
    f.source.running.store(false, Ordering::SeqCst);
    let resumed = install(&f.store, &f.deps, plan).unwrap();
    assert_eq!(resumed.account.id, installed.id);
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 1);
}

struct FakeSource {
    proof: SourceProof,
    bytes: Vec<u8>,
    now: DateTime<Utc>,
    retired: AtomicBool,
    mutations: AtomicUsize,
    running: AtomicBool,
    unknown: AtomicBool,
    fail_retire: AtomicBool,
}
impl SourceAccess for FakeSource {
    fn snapshot(&self, _: Provider, _: &std::path::Path) -> Result<SourceSnapshot, TakeoverError> {
        if self.retired.load(Ordering::SeqCst) {
            return Err(TakeoverError::Unavailable);
        }
        Ok(SourceSnapshot {
            proof: self.proof.clone(),
            bytes: zeroize::Zeroizing::new(self.bytes.clone()),
        })
    }
    fn owner(&self, _: Provider, _: &SourceProof) -> Owner {
        if self.unknown.load(Ordering::SeqCst) {
            Owner::Unknown
        } else if self.running.load(Ordering::SeqCst) {
            Owner::Running
        } else {
            Owner::Stopped
        }
    }
    fn retire(
        &self,
        provider: Provider,
        proof: &SourceProof,
        _: &std::path::Path,
        keep: bool,
    ) -> Result<(), TakeoverError> {
        if self.fail_retire.load(Ordering::SeqCst)
            || proof != &self.proof
            || self.owner(provider, proof) != Owner::Stopped
        {
            return Err(TakeoverError::Conflict);
        }
        if !keep && !self.retired.swap(true, Ordering::SeqCst) {
            self.mutations.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn now(&self) -> DateTime<Utc> {
        self.now
    }
}
struct FakeProfile {
    calls: AtomicUsize,
}
impl ProfileResolver for FakeProfile {
    fn endpoint(&self) -> &str {
        "http://127.0.0.1:23456/api/oauth/profile"
    }
    fn claude<'a>(&'a self, _: &'a str) -> AsyncResult<'a, ProfileIdentity> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ProfileIdentity {
                identity: Identity::Claude {
                    account_uuid: "synthetic-user".into(),
                    organization_uuid: Some("synthetic-org".into()),
                    email: "takeover@example.invalid".into(),
                },
                email: "takeover@example.invalid".into(),
                plan: Some("pro".into()),
            })
        })
    }
}
struct Fixture {
    store: Arc<AccountStore>,
    deps: Dependencies,
    source: Arc<FakeSource>,
    profile: Arc<FakeProfile>,
    _root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/staging/r6-core/test-homes");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(std::fs::canonicalize(base).unwrap()).unwrap();
        let store = Arc::new(AccountStore::acquire(root.path()).unwrap());
        let now = Utc::now();
        let bytes = serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{
            "accessToken":"synthetic-access","refreshToken":"synthetic-refresh",
            "expiresAt":(now+chrono::Duration::hours(2)).timestamp_millis(),"scopes":["user:profile"],
            "subscriptionType":"pro","rateLimitTier":null}})).unwrap();
        let source = Arc::new(FakeSource {
            proof: SourceProof {
                path: root.path().join("source/.credentials.json"),
                volume: 1,
                index: 42,
                modified: now - chrono::Duration::minutes(5),
                sha256: digest(&bytes),
            },
            bytes,
            now,
            retired: AtomicBool::new(false),
            mutations: AtomicUsize::new(0),
            running: AtomicBool::new(false),
            unknown: AtomicBool::new(false),
            fail_retire: AtomicBool::new(false),
        });
        let profile = Arc::new(FakeProfile {
            calls: AtomicUsize::new(0),
        });
        let deps = Dependencies {
            source: source.clone(),
            profile: profile.clone(),
        };
        Self {
            store,
            deps,
            source,
            profile,
            _root: root,
        }
    }
    fn request(&self, phase: Phase, inspect_digest: Option<String>) -> TakeoverRequest {
        TakeoverRequest {
            provider: Provider::Claude,
            path: self.source.proof.path.clone(),
            phase,
            inspect_digest,
            replace: false,
            keep_source: false,
        }
    }
    async fn plan(&self) -> Journal {
        let Preparation::Inspect(value) = prepare(
            self.store.clone(),
            self.deps.clone(),
            self.request(Phase::Inspect, None),
        )
        .await
        .unwrap() else {
            panic!("inspect result")
        };
        let digest = value["inspect_digest"].as_str().unwrap().to_owned();
        let Preparation::Commit(journal) = prepare(
            self.store.clone(),
            self.deps.clone(),
            self.request(Phase::Commit, Some(digest)),
        )
        .await
        .unwrap() else {
            panic!("commit result")
        };
        *journal
    }
}

#[tokio::test]
async fn r6_native_claude_inspect_is_readonly_and_uses_injected_profile_with_millisecond_expiry() {
    let f = Fixture::new();
    let names = || {
        std::fs::read_dir(f.store.home().join("accounts"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    };
    let before = names();
    let plan = f.plan().await;
    assert_eq!(names(), before);
    assert_eq!(f.profile.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        plan.account.token_meta.expires_at,
        DateTime::<Utc>::from_timestamp_millis(
            (f.source.now + chrono::Duration::hours(2)).timestamp_millis(),
        )
    );
    assert!(f.store.takeover_journals().unwrap().is_empty());
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r6_ready_started_and_atomic_success_receipt_fence_and_recover_without_source_token_reload()
{
    let f = Fixture::new();
    let journal = f.plan().await;
    let installed = install(&f.store, &f.deps, journal).unwrap();
    let account = f.store.load(&installed.account.id).unwrap();
    assert!(f.store.tokens(&account).is_err());
    assert!(f.store.takeover_fenced_ids().unwrap().contains(&account.id));
    let tokens = f
        .store
        .takeover_tokens(&account, &installed.operation)
        .unwrap();
    assert!(
        f.store
            .save(&account, &tokens, Some(account.generation))
            .is_err()
    );
    assert!(f.store.remove_login_record_unlocked(&account.id).is_err());
    assert!(
        f.store
            .begin_login_migration(&account.id, "cx-000000", &account.identity)
            .is_err()
    );
    let generation = account.generation;
    let replay = install(&f.store, &f.deps, installed.clone()).unwrap();
    assert_eq!(f.store.load(&account.id).unwrap().generation, generation);
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 1);
    let ready = f.store.takeover_receipt(&account.id).unwrap().unwrap();
    assert_eq!(ready.state, ReceiptState::Ready);
    let started = Receipt {
        operation: ready.operation.clone(),
        state: ReceiptState::Started,
    };
    let mut current = account;
    current.token_meta.refresh_uncertain = true;
    let current = f
        .store
        .save_takeover_receipt(
            &current,
            &tokens,
            Some(current.generation),
            Some(&ready),
            &started,
        )
        .unwrap();
    assert!(f.store.tokens(&current).is_err());
    assert!(matches!(
        finalize(&f.store, replay.clone()),
        Err(TakeoverError::Uncertain)
    ));
    let mut rotated = current;
    rotated.token_meta.refresh_uncertain = false;
    rotated.token_meta.last_refresh_at = Some(f.source.now);
    let new_tokens = Tokens {
        access_token: tokens.access_token.clone(),
        refresh_token: "synthetic-rotated".into(),
        id_token: None,
    };
    let result = Verification {
        ok: true,
        token_expires_at: rotated.token_meta.expires_at,
    };
    let finished = Receipt {
        operation: started.operation.clone(),
        state: ReceiptState::Finished {
            result: result.clone(),
        },
    };
    let rotated = f
        .store
        .save_takeover_receipt(
            &rotated,
            &new_tokens,
            Some(rotated.generation),
            Some(&started),
            &finished,
        )
        .unwrap();
    // Simulates rotated account save succeeded, journal finalization did not.
    assert!(
        f.store
            .read_takeover_journal(&installed.operation)
            .unwrap()
            .unwrap()
            .result
            .is_none()
    );
    let value = finalize(&f.store, replay).unwrap();
    assert_eq!(value["refresh"]["ok"], true);
    assert_eq!(
        f.store.tokens(&rotated).unwrap().refresh_token,
        "synthetic-rotated"
    );
    assert!(
        f.store
            .read_takeover_journal(&installed.operation)
            .unwrap()
            .unwrap()
            .protected_tokens
            .is_none()
    );
    let metadata = f
        .store
        .update_metadata(&rotated.id, |a| a.label = "new display".into())
        .unwrap();
    let saved = f
        .store
        .save(&metadata, &new_tokens, Some(metadata.generation))
        .unwrap();
    assert_eq!(f.store.takeover_receipt(&saved.id).unwrap(), Some(finished));
    assert!(
        serde_json::to_value(&saved)
            .unwrap()
            .get("takeover_receipt")
            .is_none()
    );
}

#[tokio::test]
async fn r6_owner_and_digest_races_refuse_before_any_journal_or_retirement() {
    let f = Fixture::new();
    let request = f.request(Phase::Commit, Some("not-the-inspected-digest".into()));
    assert!(
        prepare(f.store.clone(), f.deps.clone(), request)
            .await
            .is_err()
    );
    f.source.running.store(true, Ordering::SeqCst);
    let Preparation::Inspect(value) = prepare(
        f.store.clone(),
        f.deps.clone(),
        f.request(Phase::Inspect, None),
    )
    .await
    .unwrap() else {
        panic!("inspect")
    };
    assert_eq!(value["owner_alive"], true);
    assert!(
        prepare(
            f.store.clone(),
            f.deps.clone(),
            f.request(
                Phase::Commit,
                Some(value["inspect_digest"].as_str().unwrap().into())
            )
        )
        .await
        .is_err()
    );
    assert!(f.store.takeover_journals().unwrap().is_empty());
    assert!(f.store.list().unwrap().is_empty());
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
}

#[test]
fn r6_receipt_transitions_are_operation_bound_and_unknown_is_not_finished() {
    let ready = Receipt {
        operation: "a".repeat(64),
        state: ReceiptState::Ready,
    };
    let started = Receipt {
        operation: ready.operation.clone(),
        state: ReceiptState::Started,
    };
    assert!(ready.permits(&started));
    assert!(started.fenced());
    assert!(!started.permits(&ready));
    assert!(!ready.permits(&Receipt {
        operation: "b".repeat(64),
        state: ReceiptState::Started
    }));
}

#[tokio::test]
async fn r6_retirement_failure_keeps_installed_fence_and_resumes_without_reinstall() {
    let f = Fixture::new();
    let plan = f.plan().await;
    f.source.fail_retire.store(true, Ordering::SeqCst);
    assert!(install(&f.store, &f.deps, plan.clone()).is_err());
    let account = f.store.load(&plan.account.id).unwrap();
    let receipt = f.store.takeover_receipt(&account.id).unwrap().unwrap();
    assert_eq!(receipt.state, ReceiptState::Installed);
    assert!(f.store.tokens(&account).is_err());
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
    let generation = account.generation;
    f.source.fail_retire.store(false, Ordering::SeqCst);
    install(&f.store, &f.deps, plan).unwrap();
    // Only Installed -> Ready adds a generation, never candidate reinstallation.
    assert_eq!(
        f.store.load(&account.id).unwrap().generation,
        generation + 1
    );
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn r6_rename_before_journal_save_recovers_exact_source_without_reinstall() {
    let f = Fixture::new();
    let mut plan = f.plan().await;
    plan.account = f.store.publish_takeover(&plan).unwrap();
    f.deps
        .source
        .retire(plan.provider, &plan.source, &plan.destination, false)
        .unwrap();
    // Deliberately leave durable journal.retired=false, simulating process death.
    let resumed = install(&f.store, &f.deps, plan.clone()).unwrap();
    assert!(resumed.retired);
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.store.load(&plan.account.id).unwrap().generation,
        plan.account.generation + 1
    );
}

#[tokio::test]
async fn r6_started_receipt_survives_restart_without_source_or_endpoint_access() {
    let f = Fixture::new();
    let journal = install(&f.store, &f.deps, f.plan().await).unwrap();
    let mut account = f.store.load(&journal.account.id).unwrap();
    let tokens = f
        .store
        .takeover_tokens(&account, &journal.operation)
        .unwrap();
    let before = f.store.takeover_receipt(&account.id).unwrap().unwrap();
    account.token_meta.refresh_uncertain = true;
    f.store
        .save_takeover_receipt(
            &account,
            &tokens,
            Some(account.generation),
            Some(&before),
            &Receipt {
                operation: journal.operation.clone(),
                state: ReceiptState::Started,
            },
        )
        .unwrap();
    f.store.release_lease();
    let reopened = AccountStore::acquire(f._root.path()).unwrap();
    let current = reopened.load(&account.id).unwrap();
    assert!(current.token_meta.refresh_uncertain);
    assert!(reopened.tokens(&current).is_err());
    assert!(matches!(
        finalize(&reopened, journal),
        Err(TakeoverError::Uncertain)
    ));
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn r6_completed_restart_finishes_journal_then_replays_after_account_deletion() {
    let f = Fixture::new();
    let journal = install(&f.store, &f.deps, f.plan().await).unwrap();
    let mut account = f.store.load(&journal.account.id).unwrap();
    let tokens = f
        .store
        .takeover_tokens(&account, &journal.operation)
        .unwrap();
    let before = f.store.takeover_receipt(&account.id).unwrap().unwrap();
    account.token_meta.refresh_uncertain = true;
    f.store
        .save_takeover_receipt(
            &account,
            &tokens,
            Some(account.generation),
            Some(&before),
            &Receipt {
                operation: journal.operation.clone(),
                state: ReceiptState::Started,
            },
        )
        .unwrap();
    f.store
        .finish_takeover_attempt(
            &account.id,
            &journal.operation,
            Verification {
                ok: false,
                token_expires_at: account.token_meta.expires_at,
            },
            None,
        )
        .unwrap();
    f.store.release_lease();
    let reopened = AccountStore::acquire(f._root.path()).unwrap();
    let completed = reopened
        .read_takeover_journal(&journal.operation)
        .unwrap()
        .unwrap();
    assert!(completed.result.is_some());
    assert!(completed.protected_tokens.is_none());
    {
        let _guard = reopened.transaction().unwrap();
        reopened.remove_login_record_unlocked(&account.id).unwrap();
    }
    assert_eq!(
        finalize(&reopened, completed).unwrap()["refresh"]["ok"],
        false
    );
    assert!(reopened.list().unwrap().is_empty());
}

#[tokio::test]
async fn r6_replace_recovers_terminal_auth_but_preserves_identity_policy_payment_and_label() {
    let f = Fixture::new();
    let mut account = f.plan().await.account;
    account.source = super::super::Source::Oauth;
    account.token_meta.expires_at = Some(f.source.now - chrono::Duration::hours(1));
    account.health = super::super::Health::NeedsRelogin {
        reason: "invalid_grant".into(),
        since: f.source.now,
    };
    account.auth_failures = 3;
    account.user_state = super::super::UserState::Paused;
    account.allow_credits = true;
    account.label = "keep this label".into();
    account.payment_failures = 2;
    account.payment_until = Some(f.source.now + chrono::Duration::hours(4));
    let old_tokens = Tokens {
        access_token: "synthetic-old".into(),
        refresh_token: "synthetic-old-rt".into(),
        id_token: None,
    };
    let old = f.store.save(&account, &old_tokens, None).unwrap();
    let mut request = f.request(Phase::Inspect, None);
    request.replace = true;
    let Preparation::Inspect(value) = prepare(f.store.clone(), f.deps.clone(), request.clone())
        .await
        .unwrap()
    else {
        panic!("inspect")
    };
    request.phase = Phase::Commit;
    request.inspect_digest = Some(value["inspect_digest"].as_str().unwrap().into());
    let Preparation::Commit(plan) = prepare(f.store.clone(), f.deps.clone(), request)
        .await
        .unwrap()
    else {
        panic!("commit")
    };
    let installed = install(&f.store, &f.deps, *plan).unwrap();
    let current = f.store.load(&installed.account.id).unwrap();
    assert_eq!(current.id, old.id);
    assert_eq!(current.identity, old.identity);
    assert_eq!(current.label, old.label);
    assert_eq!(current.user_state, old.user_state);
    assert!(current.allow_credits);
    assert_eq!(current.auth_failures, 0);
    assert_eq!(current.payment_failures, 2);
    assert_eq!(current.payment_until, old.payment_until);
    assert!(!matches!(
        current.health,
        super::super::Health::NeedsRelogin { .. }
    ));
}

#[tokio::test]
async fn r6_owner_unknown_inspects_readonly_and_commit_refuses() {
    let f = Fixture::new();
    f.source.unknown.store(true, Ordering::SeqCst);
    let Preparation::Inspect(value) = prepare(
        f.store.clone(),
        f.deps.clone(),
        f.request(Phase::Inspect, None),
    )
    .await
    .unwrap() else {
        panic!("inspect")
    };
    assert_eq!(value["owner_alive"], true);
    let result = prepare(
        f.store.clone(),
        f.deps.clone(),
        f.request(
            Phase::Commit,
            Some(value["inspect_digest"].as_str().unwrap().into()),
        ),
    )
    .await;
    assert!(matches!(result, Err(TakeoverError::Conflict)));
    assert!(f.store.takeover_journals().unwrap().is_empty());
    assert!(f.store.list().unwrap().is_empty());
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r6_api_key_native_inputs_refuse_without_profile_calls() {
    let f = Fixture::new();
    for (provider, bytes) in [
        (
            Provider::Codex,
            br#"{"auth_mode":"apikey","OPENAI_API_KEY":"synthetic"}"#.as_slice(),
        ),
        (Provider::Claude, br#"{"apiKey":"synthetic"}"#.as_slice()),
    ] {
        assert!(matches!(
            identify(provider, bytes, f.profile.as_ref()).await,
            Err(TakeoverError::ApiKey)
        ));
    }
    assert_eq!(f.profile.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r6_known_codex_users_with_same_email_and_workspace_never_match() {
    let f = Fixture::new();
    let mut account = f.plan().await.account;
    account.provider = Provider::Codex;
    account.identity = Identity::Codex {
        account_id: "workspace".into(),
        user_id: Some("first".into()),
        email: "same@example.invalid".into(),
    };
    account.id = account.identity.id();
    let incoming = Identity::Codex {
        account_id: "workspace".into(),
        user_id: Some("second".into()),
        email: "same@example.invalid".into(),
    };
    assert!(!same_person(&f.store, &account, &incoming).unwrap());
    assert_ne!(account.id, incoming.id());
}

#[tokio::test]
async fn r6_not_sent_receipt_and_private_field_survive_metadata_without_pretending_success() {
    let f = Fixture::new();
    let journal = install(&f.store, &f.deps, f.plan().await).unwrap();
    let mut account = f.store.load(&journal.account.id).unwrap();
    let tokens = f
        .store
        .takeover_tokens(&account, &journal.operation)
        .unwrap();
    let ready = f.store.takeover_receipt(&account.id).unwrap().unwrap();
    let started = Receipt {
        operation: journal.operation.clone(),
        state: ReceiptState::Started,
    };
    account.token_meta.refresh_uncertain = true;
    let account = f
        .store
        .save_takeover_receipt(
            &account,
            &tokens,
            Some(account.generation),
            Some(&ready),
            &started,
        )
        .unwrap();
    let account = f
        .store
        .finish_takeover_attempt(
            &account.id,
            &journal.operation,
            Verification {
                ok: false,
                token_expires_at: account.token_meta.expires_at,
            },
            None,
        )
        .unwrap();
    assert!(!account.token_meta.refresh_uncertain);
    assert!(!f.store.takeover_fenced(&account.id).unwrap());
    assert_eq!(finalize(&f.store, journal).unwrap()["refresh"]["ok"], false);
}

#[tokio::test]
async fn r6_unpublished_intent_after_metadata_advance_gets_fresh_digest_and_keeps_old_cas() {
    let f = Fixture::new();
    let mut account = f.plan().await.account;
    account.source = super::super::Source::Oauth;
    account.token_meta.expires_at = Some(f.source.now - chrono::Duration::hours(1));
    let tokens = Tokens {
        access_token: "synthetic-old".into(),
        refresh_token: "synthetic-old-refresh".into(),
        id_token: None,
    };
    let account = f.store.save(&account, &tokens, None).unwrap();
    let make_plan = || async {
        let mut request = f.request(Phase::Inspect, None);
        request.replace = true;
        let Preparation::Inspect(value) = prepare(f.store.clone(), f.deps.clone(), request.clone())
            .await
            .unwrap()
        else {
            panic!("inspect")
        };
        request.phase = Phase::Commit;
        request.inspect_digest = Some(value["inspect_digest"].as_str().unwrap().into());
        let Preparation::Commit(journal) = prepare(f.store.clone(), f.deps.clone(), request)
            .await
            .unwrap()
        else {
            panic!("plan")
        };
        *journal
    };
    let old = make_plan().await;
    // The exact crash boundary: prepared journal exists, account has no marker.
    f.store.write_takeover_journal(&old).unwrap();
    assert!(f.store.takeover_receipt(&account.id).unwrap().is_none());
    f.store.release_lease();
    let reopened = AccountStore::acquire(f._root.path()).unwrap();
    reopened
        .update_metadata(&account.id, |a| a.label = "updated policy".into())
        .unwrap();
    reopened.release_lease();
    // Use a newly acquired lease for the real post-restart inspection/commit.
    let store = Arc::new(AccountStore::acquire(f._root.path()).unwrap());
    let mut request = f.request(Phase::Inspect, None);
    request.replace = true;
    let Preparation::Inspect(value) = prepare(store.clone(), f.deps.clone(), request.clone())
        .await
        .unwrap()
    else {
        panic!("inspect")
    };
    assert_ne!(value["inspect_digest"], old.inspect_digest);
    request.phase = Phase::Commit;
    request.inspect_digest = Some(value["inspect_digest"].as_str().unwrap().into());
    let Preparation::Commit(new) = prepare(store.clone(), f.deps.clone(), request)
        .await
        .unwrap()
    else {
        panic!("plan")
    };
    assert!(matches!(
        install(&store, &f.deps, old.clone()),
        Err(TakeoverError::Conflict)
    ));
    let installed = install(&store, &f.deps, *new).unwrap();
    assert_eq!(store.load(&account.id).unwrap().label, "updated policy");
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .read_takeover_journal(&old.operation)
            .unwrap()
            .unwrap()
            .expected,
        old.expected
    );
    assert_ne!(old.operation, installed.operation);
}

#[tokio::test]
async fn r6_login_supersedes_installed_without_retirement_or_replay_rename_claim() {
    let f = Fixture::new();
    let plan = f.plan().await;
    let old = f.store.publish_takeover(&plan).unwrap();
    let mut login = old.clone();
    login.source = super::super::Source::Oauth;
    login.token_meta.expires_at = Some(f.source.now + chrono::Duration::minutes(30));
    let tokens = Tokens {
        access_token: "synthetic-new-login".into(),
        refresh_token: "synthetic-new-family".into(),
        id_token: None,
    };
    let saved = super::super::login::save(&f.store, &login, &tokens, false)
        .unwrap()
        .account;
    assert_eq!(saved.id, old.id);
    assert_eq!(
        f.store.takeover_receipt(&saved.id).unwrap().unwrap().state,
        ReceiptState::Superseded
    );
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
    let replay = install(&f.store, &f.deps, plan).unwrap();
    let result = finalize(&f.store, replay).unwrap();
    assert!(result["renamed_to"].is_null());
    assert_eq!(result["refresh"]["ok"], false);
    assert_eq!(
        f.store.tokens(&saved).unwrap().refresh_token,
        "synthetic-new-family"
    );
    f.store.release_lease();
    let reopened = AccountStore::acquire(f._root.path()).unwrap();
    assert!(!reopened.takeover_fenced(&saved.id).unwrap());
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
}

struct HeldRetirement {
    source: Arc<FakeSource>,
    entered: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
}
impl SourceAccess for HeldRetirement {
    fn snapshot(
        &self,
        p: Provider,
        path: &std::path::Path,
    ) -> Result<SourceSnapshot, TakeoverError> {
        self.source.snapshot(p, path)
    }
    fn owner(&self, p: Provider, proof: &SourceProof) -> Owner {
        self.source.owner(p, proof)
    }
    fn now(&self) -> DateTime<Utc> {
        self.source.now()
    }
    fn retire(
        &self,
        p: Provider,
        proof: &SourceProof,
        dest: &std::path::Path,
        keep: bool,
    ) -> Result<(), TakeoverError> {
        self.entered.wait();
        self.release.wait();
        self.source.retire(p, proof, dest, keep)
    }
}

#[tokio::test]
async fn r6_retirement_transaction_orders_concurrent_fresh_login_and_records_actual_rename() {
    let f = Fixture::new();
    let plan = f.plan().await;
    let account = f.store.publish_takeover(&plan).unwrap();
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let deps = Dependencies {
        source: Arc::new(HeldRetirement {
            source: f.source.clone(),
            entered: entered.clone(),
            release: release.clone(),
        }),
        profile: f.profile.clone(),
    };
    let (store, j) = (f.store.clone(), plan.clone());
    let install_task = tokio::task::spawn_blocking(move || install(&store, &deps, j));
    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .unwrap();
    let mut login = account;
    login.source = super::super::Source::Oauth;
    let tokens = Tokens {
        access_token: "synthetic-fresh-login".into(),
        refresh_token: "synthetic-fresh-family".into(),
        id_token: None,
    };
    let (attempted, received) = std::sync::mpsc::channel();
    let store = f.store.clone();
    let mut login_task = tokio::task::spawn_blocking(move || {
        attempted.send(()).unwrap();
        super::super::login::save(&store, &login, &tokens, false)
    });
    received
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    // A concurrent login cannot pass the held retirement store transaction.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut login_task)
            .await
            .is_err()
    );
    release.wait();
    let retired = install_task.await.unwrap().unwrap();
    let saved = login_task.await.unwrap().unwrap().account;
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 1);
    let result = finalize(&f.store, retired).unwrap();
    assert_eq!(
        result["renamed_to"],
        plan.destination.to_string_lossy().as_ref()
    );
    assert_eq!(result["refresh"]["ok"], false);
    assert_eq!(
        f.store.tokens(&saved).unwrap().refresh_token,
        "synthetic-fresh-family"
    );
    f.store.release_lease();
    let reopened = AccountStore::acquire(f._root.path()).unwrap();
    let journal = reopened
        .read_takeover_journal(&plan.operation)
        .unwrap()
        .unwrap();
    assert!(journal.retired && journal.superseded);
}

#[tokio::test]
async fn r6_fresh_login_reconciles_native_rename_before_failed_journal_write_without_renaming_again()
 {
    use std::os::windows::fs::OpenOptionsExt;
    let f = Fixture::new();
    let mut plan = f.plan().await;
    std::fs::create_dir_all(plan.source.path.parent().unwrap()).unwrap();
    std::fs::write(&plan.source.path, &f.source.bytes).unwrap();
    let native = super::super::takeover_native::NativeSource;
    plan.source = native
        .snapshot(plan.provider, &plan.source.path)
        .unwrap()
        .proof;
    plan.account.source = super::super::Source::Takeover {
        from: plan.source.path.clone(),
        at: f.source.now,
    };
    let old = f.store.publish_takeover(&plan).unwrap();
    plan.retirement_started = true;
    f.store.write_takeover_journal(&plan).unwrap();
    let journal_path = f
        .store
        .home()
        .join("accounts")
        .join(format!(".takeover-{}.journal", plan.operation));
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(journal_path)
        .unwrap();
    native
        .retire(plan.provider, &plan.source, &plan.destination, false)
        .unwrap();
    let mut completed_rename = plan.clone();
    completed_rename.retired = true;
    assert!(f.store.write_takeover_journal(&completed_rename).is_err());
    // Keep the same failure through both pre-save refinement and post-save
    // compaction: only the fresh-family/Superseded account write can succeed.
    let mut login = f.store.load(&old.id).unwrap();
    login.source = super::super::Source::Oauth;
    let tokens = Tokens {
        access_token: "synthetic-fresh".into(),
        refresh_token: "synthetic-fresh-family".into(),
        id_token: None,
    };
    let saved = super::super::login::save(&f.store, &login, &tokens, false)
        .unwrap()
        .account;
    assert_eq!(
        f.store.takeover_receipt(&saved.id).unwrap().unwrap().state,
        ReceiptState::Superseded
    );
    let blocked_journal = f
        .store
        .read_takeover_journal(&plan.operation)
        .unwrap()
        .unwrap();
    assert!(blocked_journal.result.is_none() && !blocked_journal.retired);
    assert_eq!(
        f.store.tokens(&saved).unwrap().refresh_token,
        "synthetic-fresh-family"
    );
    drop(held);
    let request = f.request(Phase::Commit, Some(plan.inspect_digest.clone()));
    let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
    let obs = Arc::new(crate::observability::Observability::new(f._root.path(), None).unwrap());
    let gates = crate::relay::credentials::TokenGates::new(
        reqwest::Client::new(),
        crate::account_runtime::AccountRuntime::new(f.store.clone(), obs.clone(), 1.0),
    )
    .with_token_endpoint(plan.provider, fake.url.clone());
    let result = gates
        .takeover(
            f.store.clone(),
            obs.clone(),
            request.clone(),
            f.deps.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        result["renamed_to"],
        plan.destination.to_string_lossy().as_ref()
    );
    assert_eq!(result["refresh"]["ok"], false);
    assert!(
        super::super::takeover_native::retirement_evidence(&plan.source, &plan.destination)
            .unwrap()
    );
    assert!(fake.state.requests().is_empty());
    gates.shutdown_refreshes().await.unwrap();
    f.store.release_lease();
    let reopened = Arc::new(AccountStore::acquire(f._root.path()).unwrap());
    let gates = crate::relay::credentials::TokenGates::new(
        reqwest::Client::new(),
        crate::account_runtime::AccountRuntime::new(reopened.clone(), obs.clone(), 1.0),
    )
    .with_token_endpoint(plan.provider, fake.url.clone());
    assert_eq!(
        gates
            .takeover(reopened.clone(), obs, request, f.deps.clone())
            .await
            .unwrap(),
        result
    );
    assert!(fake.state.requests().is_empty());
    gates.shutdown_refreshes().await.unwrap();
    let journal = reopened
        .read_takeover_journal(&plan.operation)
        .unwrap()
        .unwrap();
    assert!(journal.superseded && journal.retired);
    assert!(!plan.source.path.exists());
    assert!(plan.destination.is_file());
    assert_eq!(
        finalize(&reopened, journal).unwrap()["renamed_to"],
        plan.destination.to_string_lossy().as_ref()
    );
    assert_eq!(
        reopened
            .tokens(&reopened.load(&saved.id).unwrap())
            .unwrap()
            .refresh_token,
        "synthetic-fresh-family"
    );
}

#[tokio::test]
async fn r6_missing_uncertain_retirement_source_does_not_block_fresh_login_or_replay_tombstone() {
    let f = Fixture::new();
    let mut plan = f.plan().await;
    let old = f.store.publish_takeover(&plan).unwrap();
    plan.retirement_started = true;
    f.store.write_takeover_journal(&plan).unwrap();
    let mut login = old;
    login.source = super::super::Source::Oauth;
    let tokens = Tokens {
        access_token: "synthetic-fresh".into(),
        refresh_token: "synthetic-fresh-family".into(),
        id_token: None,
    };
    let saved = super::super::login::save(&f.store, &login, &tokens, false)
        .unwrap()
        .account;
    let journal = f
        .store
        .read_takeover_journal(&plan.operation)
        .unwrap()
        .unwrap();
    assert!(journal.superseded && journal.retirement_started && !journal.retired);
    assert!(finalize(&f.store, journal).unwrap()["renamed_to"].is_null());
    assert_eq!(
        f.store.tokens(&saved).unwrap().refresh_token,
        "synthetic-fresh-family"
    );
    let replay = install(&f.store, &f.deps, plan).unwrap();
    assert_eq!(finalize(&f.store, replay).unwrap()["refresh"]["ok"], false);
    assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r6_failed_supersession_journal_compaction_preserves_replay_restart_delete_and_replace_tombstones()
 {
    use std::os::windows::fs::OpenOptionsExt;
    for action in ["replay", "restart", "delete", "replace"] {
        let f = Fixture::new();
        let plan = f.plan().await;
        let old = f.store.publish_takeover(&plan).unwrap();
        let path = f
            .store
            .home()
            .join("accounts")
            .join(format!(".takeover-{}.journal", plan.operation));
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(path)
            .unwrap();
        let mut login = old;
        login.source = super::super::Source::Oauth;
        login.token_meta.expires_at = Some(f.source.now + chrono::Duration::minutes(10));
        let tokens = Tokens {
            access_token: "synthetic-fresh".into(),
            refresh_token: "synthetic-fresh-family".into(),
            id_token: None,
        };
        let saved = super::super::login::save(&f.store, &login, &tokens, false)
            .unwrap()
            .account;
        assert_eq!(
            f.store.takeover_receipt(&saved.id).unwrap().unwrap().state,
            ReceiptState::Superseded
        );
        assert!(
            f.store
                .read_takeover_journal(&plan.operation)
                .unwrap()
                .unwrap()
                .result
                .is_none()
        );
        let mut next = plan.clone();
        next.inspect_digest = digest(b"synthetic-new-source-operation");
        next.operation = digest(next.inspect_digest.as_bytes());
        next.source.path = f._root.path().join("different-source/.credentials.json");
        next.source.index += 1;
        next.source.sha256 = digest(b"synthetic-different-source");
        next.destination = next
            .source
            .path
            .with_file_name(".credentials.json.taken-over-by-quota-baton-20261019");
        next.account = saved.clone();
        next.account.source = super::super::Source::Takeover {
            from: next.source.path.clone(),
            at: f.source.now,
        };
        next.account.token_meta.expires_at = Some(f.source.now + chrono::Duration::hours(3));
        next.expected = Some(saved.generation);
        next.protected_tokens = Some(
            f.store
                .protect_takeover_tokens(&next.account.id, &tokens)
                .unwrap(),
        );
        match action {
            "replay" => assert!(install(&f.store, &f.deps, plan.clone()).is_err()),
            "delete" => {
                let _guard = f.store.transaction().unwrap();
                assert!(f.store.remove_login_record_unlocked(&saved.id).is_err());
            }
            "replace" => assert!(f.store.publish_takeover(&next).is_err()),
            _ => {}
        }
        assert_eq!(
            f.store
                .tokens(&f.store.load(&saved.id).unwrap())
                .unwrap()
                .refresh_token,
            "synthetic-fresh-family"
        );
        drop(held);
        match action {
            "replay" => {
                let j = install(&f.store, &f.deps, plan.clone()).unwrap();
                assert_eq!(finalize(&f.store, j).unwrap()["refresh"]["ok"], false);
            }
            "delete" => {
                let _guard = f.store.transaction().unwrap();
                f.store.remove_login_record_unlocked(&saved.id).unwrap();
            }
            "replace" => {
                f.store.publish_takeover(&next).unwrap();
            }
            "restart" => {
                f.store.release_lease();
                let reopened = AccountStore::acquire(f._root.path()).unwrap();
                assert!(!reopened.takeover_fenced(&saved.id).unwrap());
            }
            _ => unreachable!(),
        }
        let terminal = f
            .store
            .read_takeover_journal(&plan.operation)
            .unwrap()
            .unwrap();
        assert!(
            terminal.superseded && terminal.result.is_some() && terminal.protected_tokens.is_none()
        );
        assert_eq!(f.source.mutations.load(Ordering::SeqCst), 0);
    }
}

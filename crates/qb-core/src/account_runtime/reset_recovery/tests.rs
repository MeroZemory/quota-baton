use super::*;
use crate::accounts::{HealthSignal, Identity, MetadataCause, MetadataMutation, Source, Tokens};

struct Fixture {
    runtime: Arc<AccountRuntime>,
    id: String,
    home: tempfile::TempDir,
}

impl Fixture {
    fn new(provider: Provider) -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r28-reset-recovery-tests");
        std::fs::create_dir_all(&base).unwrap();
        let home = qb_testkit::fixture::home_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let mut account = crate::accounts::health::tests::fixture();
        account.provider = provider;
        if provider == Provider::Claude {
            account.identity = Identity::Claude {
                account_uuid: "synthetic-r28-reset-recovery".into(),
                organization_uuid: Some("synthetic-organization".into()),
                email: "synthetic@example.invalid".into(),
            };
            account.id = account.identity.id();
            account.plan = Some("pro".into());
        }
        let now = Utc::now();
        account.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot: json!({"windows":[{"name":"5h","used_pct":100,
                "reset_at":now + Duration::hours(1),"observed_at":now - Duration::seconds(20)}]}),
                model: None,
                projected_allowance: 0.0,
            },
            now - Duration::seconds(20),
        );
        assert!(matches!(
            account.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        assert!(account.quota_cooling.is_some());
        let account = store
            .save(
                &account,
                &Tokens {
                    access_token: "synthetic-r28-reset-access".into(),
                    refresh_token: "synthetic-r28-reset-refresh".into(),
                    id_token: None,
                },
                None,
            )
            .unwrap();
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let runtime = AccountRuntime::new(store, obs, 0.0);
        Self {
            runtime,
            id: account.id,
            home,
        }
    }

    fn account(&self) -> Account {
        self.runtime.store.load(&self.id).unwrap()
    }

    async fn observation(&self) -> ResetQuotaObservation {
        let expected = self.account();
        let wait_origin = self.runtime.capture_poll_origin(&self.id).await;
        assert!(
            wait_origin.is_some(),
            "actual native capture must supply authority"
        );
        let observed = Utc::now();
        let reset = expected.quota_cooling.as_ref().unwrap().reset_at;
        let reported = json!({"observed_at":observed,"source":"poll","windows":[
            {"name":"5h","used_pct":0,"observed_at":observed,"reset_at":reset,"status":null},
            {"name":"7d","used_pct":12,"observed_at":observed,
                "reset_at":reset + Duration::days(1),"status":null}
        ]});
        ResetQuotaObservation {
            expected,
            cleared: BTreeSet::from(["5h".into()]),
            snapshot: reported.clone(),
            reported,
            wait_origin,
            outside_origin: self.runtime.capture_outside_poll_origin(&self.id),
            started_at: observed - Duration::milliseconds(1),
            complete: true,
        }
    }

    async fn apply(&self, mut observation: ResetQuotaObservation) -> Account {
        // The real Claude caller publishes the actual GET through this same
        // native cache observer before handing the canonical snapshot to runtime.
        observation.snapshot = self
            .runtime
            .obs
            .observe_poll(&self.id, observation.snapshot, observation.complete)
            .unwrap();
        self.runtime
            .apply_reset_quota_snapshot(observation)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn unchanged_reset_timestamp_recovers_native_pool_and_revokes_pre_release_origins() {
    for used in [0, 17] {
        let f = Fixture::new(Provider::Claude);
        let mut observation = f.observation().await;
        let before = observation.expected.clone();
        observation.reported["windows"][0]["used_pct"] = json!(used);
        observation.snapshot = observation.reported.clone();
        let later_origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        assert!(!f.runtime.eligible(&f.id, None).await);
        let after = f.apply(observation).await;
        assert_eq!(after.health, Health::Ready);
        assert!(after.quota_cooling.is_none());
        assert!(!after.quota_credit_releasable);
        assert!(f.runtime.eligible(&f.id, None).await);
        assert_eq!(after.user_state, before.user_state);
        assert_eq!(after.drain, before.drain);
        assert_eq!(after.allow_credits, before.allow_credits);
        assert!(
            f.runtime.wait_owner.accept(&f.id, later_origin).is_none(),
            "native Release revokes old origins"
        );
        let actual = f.runtime.obs.quota_snapshot(&f.id).unwrap();
        assert_eq!(actual["windows"][0]["used_pct"], used as f64);
        assert_eq!(
            actual["windows"][0]["reset_at"],
            json!(before.quota_cooling.unwrap().reset_at)
        );
    }
}

#[tokio::test]
async fn already_paused_or_draining_choices_survive_an_authorized_recovery() {
    let f = Fixture::new(Provider::Claude);
    f.runtime
        .store
        .update_metadata(&f.id, |account| {
            account.user_state = UserState::Paused;
            account.drain = true;
            account.allow_credits = true;
        })
        .unwrap();
    let observation = f.observation().await;
    let after = f.apply(observation).await;
    assert_eq!(after.health, Health::Ready);
    assert!(after.quota_cooling.is_none());
    assert_eq!(after.user_state, UserState::Paused);
    assert!(after.drain);
    assert!(after.allow_credits);
    assert!(
        !f.runtime.eligible(&f.id, None).await,
        "recovery never enables a paused account"
    );
}

#[tokio::test]
async fn remaining_exhausted_window_is_reduced_again_and_still_blocks_the_pool() {
    let f = Fixture::new(Provider::Claude);
    let mut observation = f.observation().await;
    observation.reported["windows"][1]["used_pct"] = json!(100);
    observation.snapshot = observation.reported.clone();
    let after = f.apply(observation).await;
    assert!(matches!(
        after.health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    assert_eq!(after.quota_cooling.unwrap().window, "7d");
    assert!(!f.runtime.eligible(&f.id, None).await);
    let actual = f.runtime.obs.quota_snapshot(&f.id).unwrap();
    assert_eq!(actual["windows"][0]["used_pct"], 0.0);
    assert_eq!(actual["windows"][1]["used_pct"], 100.0);
}

#[tokio::test]
async fn equal_value_quota_rejection_invalidates_pre_post_origin_without_generation_change() {
    let f = Fixture::new(Provider::Claude);
    let observation = f.observation().await;
    let before = f.account();
    {
        let _guard = f.runtime.store.transaction().unwrap();
        let updated = f
            .runtime
            .store
            .update_metadata_with_cause_unlocked(
                &f.id,
                MetadataCause::Mutation(MetadataMutation::QuotaRejected),
                |_| {},
            )
            .unwrap();
        assert_eq!(
            updated, before,
            "rejection hook runs even for unchanged metadata/generation"
        );
    }
    let after = f.apply(observation).await;
    assert_eq!(after.health, before.health);
    assert_eq!(after.quota_cooling, before.quota_cooling);
    assert_eq!(after.generation, before.generation);
    assert!(!f.runtime.eligible(&f.id, None).await);
    assert_eq!(
        f.runtime.obs.quota_snapshot(&f.id).unwrap()["windows"][0]["used_pct"],
        0.0,
        "actual facts still publish without release authority"
    );
}

#[tokio::test]
async fn missing_or_foreign_origin_cannot_recover_native_cooling() {
    for foreign in [false, true] {
        let f = Fixture::new(Provider::Claude);
        let mut observation = f.observation().await;
        let other = foreign.then(|| Fixture::new(Provider::Claude));
        observation.wait_origin = if let Some(other) = &other {
            let origin = other.runtime.capture_poll_origin(&other.id).await;
            assert!(origin.is_some(), "real foreign-registry origin");
            origin
        } else {
            None
        };
        let before = f.account();
        let after = f.apply(observation).await;
        assert_eq!(after.health, before.health);
        assert_eq!(after.quota_cooling, before.quota_cooling);
        assert!(!f.runtime.eligible(&f.id, None).await);
    }
}

#[tokio::test]
async fn incomplete_stale_unknown_rejected_or_uncleared_cause_never_authorizes_release() {
    for invalid in [
        "partial",
        "missing",
        "unknown",
        "stale",
        "before_get",
        "future",
        "rejected",
        "uncleared",
        "duplicate",
        "exhausted",
    ] {
        let f = Fixture::new(Provider::Claude);
        let mut observation = f.observation().await;
        match invalid {
            "partial" => observation.complete = false,
            "missing" => {
                observation.reported["windows"]
                    .as_array_mut()
                    .unwrap()
                    .remove(0);
            }
            "unknown" => observation.reported["windows"][0]["used_pct"] = Value::Null,
            "stale" => {
                observation.reported["windows"][0]["observed_at"] = json!(
                    observation
                        .expected
                        .quota_cooling
                        .as_ref()
                        .unwrap()
                        .started_at
                )
            }
            "before_get" => {
                observation.reported["windows"][0]["observed_at"] = json!(observation.started_at)
            }
            "future" => {
                observation.reported["windows"][0]["observed_at"] =
                    json!(Utc::now() + Duration::hours(1))
            }
            "rejected" => observation.reported["windows"][0]["status"] = json!("rejected"),
            "uncleared" => observation.cleared = BTreeSet::from(["7d".into()]),
            "duplicate" => {
                let duplicate = observation.reported["windows"][0].clone();
                observation.reported["windows"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
            }
            "exhausted" => observation.reported["windows"][0]["used_pct"] = json!(100),
            _ => unreachable!(),
        }
        observation.snapshot = observation.reported.clone();
        let after = f.apply(observation).await;
        assert!(
            matches!(
                after.health,
                Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
            ),
            "{invalid}"
        );
        assert!(!f.runtime.eligible(&f.id, None).await, "{invalid}");
    }
}

#[tokio::test]
async fn changed_hold_auth_payment_or_user_choice_after_pre_post_capture_is_preserved() {
    for change in [
        "hold", "auth", "payment", "revoked", "disabled", "paused", "drain",
    ] {
        let f = Fixture::new(Provider::Claude);
        let observation = f.observation().await;
        f.runtime
            .store
            .update_metadata(&f.id, |account| match change {
                "hold" => {
                    account.health = Health::Cooling {
                        reason: HealthReason::Quota,
                        until: Utc::now() + Duration::days(2),
                        scope: "model:unrelated".into(),
                    };
                    account.quota_cooling = None;
                }
                "auth" => {
                    account.health = Health::Cooling {
                        reason: HealthReason::Auth,
                        until: Utc::now() + Duration::hours(1),
                        scope: "account".into(),
                    }
                }
                "payment" => {
                    account.health = Health::Cooling {
                        reason: HealthReason::Payment,
                        until: Utc::now() + Duration::hours(1),
                        scope: "account".into(),
                    };
                    account.payment_until = Some(Utc::now() + Duration::hours(1));
                }
                "revoked" => {
                    account.health = Health::NeedsRelogin {
                        reason: "synthetic".into(),
                        since: Utc::now(),
                    }
                }
                "disabled" => account.user_state = UserState::Disabled,
                "paused" => account.user_state = UserState::Paused,
                "drain" => account.drain = true,
                _ => unreachable!(),
            })
            .unwrap();
        let before = f.account();
        let after = f.apply(observation).await;
        assert_eq!(after.health, before.health, "{change}");
        assert_eq!(after.quota_cooling, before.quota_cooling, "{change}");
        assert_eq!(after.user_state, before.user_state);
        assert_eq!(after.drain, before.drain);
        assert_eq!(after.payment_until, before.payment_until);
    }
}

#[tokio::test]
async fn accepted_origin_does_not_replace_exact_expected_account_facts() {
    for mismatch in [
        "generation",
        "identity",
        "source",
        "token",
        "health",
        "cause",
    ] {
        let f = Fixture::new(Provider::Claude);
        let mut observation = f.observation().await;
        match mismatch {
            "generation" => observation.expected.generation += 1,
            "identity" => {
                let Identity::Claude { account_uuid, .. } = &mut observation.expected.identity
                else {
                    unreachable!()
                };
                *account_uuid = "foreign-account".into();
            }
            "source" => {
                observation.expected.source = Source::Takeover {
                    from: f.home.path().join("synthetic"),
                    at: Utc::now(),
                }
            }
            "token" => {
                observation.expected.token_meta.refresh_uncertain =
                    !observation.expected.token_meta.refresh_uncertain
            }
            "health" => observation.expected.health = Health::Ready,
            "cause" => {
                observation
                    .expected
                    .quota_cooling
                    .as_mut()
                    .unwrap()
                    .started_at -= Duration::seconds(1)
            }
            _ => unreachable!(),
        }
        let before = f.account();
        let after = f.apply(observation).await;
        assert_eq!(after.health, before.health, "{mismatch}");
        assert_eq!(after.quota_cooling, before.quota_cooling, "{mismatch}");
        assert!(!f.runtime.eligible(&f.id, None).await);
    }
}

#[tokio::test]
async fn codex_gets_only_ordinary_snapshot_reduction_without_reset_release() {
    let f = Fixture::new(Provider::Codex);
    let observation = f.observation().await;
    let before = f.account();
    let after = f.apply(observation).await;
    assert_eq!(after.health, before.health);
    assert_eq!(after.quota_cooling, before.quota_cooling);
    assert_eq!(
        after.quota_credit_releasable,
        before.quota_credit_releasable
    );
    assert!(!f.runtime.eligible(&f.id, None).await);
}

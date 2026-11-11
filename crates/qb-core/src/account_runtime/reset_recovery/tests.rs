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
        runtime.obs.observe_poll(&account.id, json!({"source":"poll","observed_at":now - Duration::seconds(20),"windows":[
            {"name":"5h","used_pct":100,"reset_at":now + Duration::hours(1),"observed_at":now - Duration::seconds(20),"status":null},
            {"name":"7d","used_pct":12,"reset_at":now + Duration::hours(1) + Duration::days(1),"observed_at":now - Duration::seconds(20),"status":null}
        ]}), true).unwrap();
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
        let started_at = Utc::now();
        let expected = self.account();
        let wait_origin = self.runtime.capture_poll_origin(&self.id).await;
        assert!(
            wait_origin.is_some(),
            "actual native capture must supply authority"
        );
        let observed = Utc::now();
        let cached = self.runtime.obs.quota_snapshot(&self.id).unwrap();
        let reset_for = |name| {
            cached["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|window| window["name"] == name)
                .unwrap()["reset_at"]
                .clone()
        };
        let reported = json!({"observed_at":observed,"source":"poll","windows":[
            {"name":"5h","used_pct":0,"observed_at":observed,"reset_at":reset_for("5h"),"status":null},
            {"name":"7d","used_pct":12,"observed_at":observed,
                "reset_at":reset_for("7d"),"status":null}
        ]});
        ResetQuotaObservation {
            expected,
            cleared: BTreeSet::from(["5h".into()]),
            snapshot: reported.clone(),
            reported,
            wait_origin,
            outside_origin: self.runtime.capture_outside_poll_origin(&self.id),
            started_at,
            complete: true,
        }
    }

    async fn apply(&self, observation: ResetQuotaObservation) -> Account {
        // One raw observation; the native transaction owns the cache merge.
        self.runtime
            .apply_reset_quota_snapshot(observation)
            .await
            .unwrap()
    }

    fn rejected_header_cache(&self, window: &str, claim: &str) -> DateTime<Utc> {
        let cached = self.runtime.obs.quota_snapshot(&self.id).unwrap();
        let reset = cached["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == window)
            .unwrap()["reset_at"]
            .as_str()
            .unwrap()
            .parse::<DateTime<Utc>>()
            .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            "rejected".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-representative-claim",
            claim.parse().unwrap(),
        );
        headers.insert(
            axum::http::HeaderName::from_bytes(
                format!("anthropic-ratelimit-unified-{window}-status").as_bytes(),
            )
            .unwrap(),
            "rejected".parse().unwrap(),
        );
        headers.insert(
            axum::http::HeaderName::from_bytes(
                format!("anthropic-ratelimit-unified-{window}-reset").as_bytes(),
            )
            .unwrap(),
            reset.timestamp().to_string().parse().unwrap(),
        );
        self.runtime
            .obs
            .observe_headers_reported(&self.id, Provider::Claude, &headers)
            .unwrap();
        reset
    }

    async fn rejected_header(&self, window: &str, claim: &str) {
        let reset = self.rejected_header_cache(window, claim);
        self.runtime
            .transition(
                &self.id,
                super::super::HealthSignal::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                },
            )
            .await
            .unwrap();
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

#[tokio::test]
async fn partial_reset_get_cannot_drop_an_uncleared_exhausted_live_window_or_measure_capacity() {
    for malformed in [false, true] {
        let f = Fixture::new(Provider::Claude);
        let mut before = f.runtime.obs.quota_snapshot(&f.id).unwrap();
        before["windows"][1]["used_pct"] = json!(100);
        f.runtime
            .obs
            .observe_poll(&f.id, before.clone(), true)
            .unwrap();
        f.runtime
            .store
            .update_metadata(&f.id, |account| {
                account.apply_health_signal(
                    HealthSignal::UsageSnapshot {
                        snapshot: before.clone(),
                        model: None,
                        projected_allowance: 0.0,
                    },
                    Utc::now() - Duration::seconds(5),
                );
            })
            .unwrap();
        assert_eq!(f.account().quota_cooling.unwrap().window, "7d");
        let mut observation = f.observation().await;
        observation.cleared = BTreeSet::from(["7d".into()]);
        observation.reported["windows"][1]["used_pct"] = json!(0);
        observation.reported["windows"][1]["reset_at"] = json!(
            observation
                .expected
                .quota_cooling
                .as_ref()
                .unwrap()
                .reset_at
        );
        observation.reported["windows"][0]["reset_at"] = before["windows"][0]["reset_at"].clone();
        if malformed {
            observation.reported["windows"][0]["used_pct"] = Value::Null;
            observation.reported["windows"][0]["observed_at"] = Value::Null;
        } else {
            observation.reported["windows"]
                .as_array_mut()
                .unwrap()
                .remove(0);
        }
        observation.snapshot = observation.reported.clone();
        assert!(
            observation.complete,
            "real Claude parser's insufficient completeness hint"
        );
        let after = f.apply(observation).await;
        assert!(matches!(
            after.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        assert!(!f.runtime.eligible(&f.id, None).await);
        let canonical = f.runtime.obs.quota_snapshot(&f.id).unwrap();
        let windows = canonical["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(
            windows.iter().find(|w| w["name"] == "5h").unwrap()["used_pct"],
            100.0
        );
        assert!(f.runtime.obs.placement_capacity_facts(&f.id).is_none());
    }
}

#[tokio::test]
async fn old_header_rejection_on_acknowledged_scope_retires_to_unknown_and_recovers() {
    let f = Fixture::new(Provider::Claude);
    f.rejected_header("5h", "five_hour").await;
    let observation = f.observation().await;
    let after = f.apply(observation).await;
    assert_eq!(after.health, Health::Ready);
    assert!(f.runtime.eligible(&f.id, None).await);
    let canonical = f.runtime.obs.quota_snapshot(&f.id).unwrap();
    assert!(canonical["windows"][0]["status"].is_null());
    assert!(canonical["signals"]["unified_status"].is_null());
    assert!(canonical["signals"]["representative_claim"].is_null());
    assert_eq!(canonical["windows"][0]["used_pct"], 0.0);
    assert_eq!(canonical["windows"][1]["used_pct"], 12.0);
    assert_eq!(
        canonical["windows"].as_array().unwrap().len(),
        2,
        "Ultra absence needs no invented four-slot inventory"
    );
}

#[tokio::test]
async fn newer_or_unrelated_rejections_and_facts_only_refresh_keep_the_native_hold() {
    for variant in [
        "newer",
        "unrelated",
        "facts_only",
        "overlap",
        "overlap_facts",
        "equal_clock",
        "equal_clock_facts",
    ] {
        let f = Fixture::new(Provider::Claude);
        let overlap = matches!(
            variant,
            "overlap" | "overlap_facts" | "equal_clock" | "equal_clock_facts"
        );
        let observation = if overlap {
            let mut observation = f.observation().await;
            // Publish H through the actual header cache path, deliberately
            // leaving its Store mutation pending: the pre-POST origin is valid.
            f.rejected_header_cache("5h", "five_hour");
            let header = f.runtime.obs.quota_snapshot(&f.id).unwrap();
            let header_at = header["observed_at"]
                .as_str()
                .unwrap()
                .parse::<DateTime<Utc>>()
                .unwrap();
            let equal_clock = matches!(variant, "equal_clock" | "equal_clock_facts");
            let read_at = if equal_clock { header_at } else { Utc::now() };
            assert!(
                observation.started_at < header_at
                    && if equal_clock {
                        header_at == read_at
                    } else {
                        header_at < read_at
                    },
                "{variant}: S < H <= R"
            );
            assert_eq!(
                f.account(),
                observation.expected,
                "no native header mutation has run"
            );
            observation.reported["observed_at"] = json!(read_at);
            for window in observation.reported["windows"].as_array_mut().unwrap() {
                window["observed_at"] = json!(read_at);
            }
            observation.snapshot = observation.reported.clone();
            if matches!(variant, "overlap_facts" | "equal_clock_facts") {
                observation.cleared.clear();
            }
            observation
        } else if variant == "newer" {
            let observation = f.observation().await;
            f.rejected_header("5h", "five_hour").await;
            observation
        } else {
            f.rejected_header(
                if variant == "unrelated" { "7d" } else { "5h" },
                if variant == "unrelated" {
                    "seven_day"
                } else {
                    "five_hour"
                },
            )
            .await;
            let mut observation = f.observation().await;
            if variant == "facts_only" {
                observation.cleared.clear();
            }
            observation
        };
        let before = f.account();
        let cache_before = f.runtime.obs.quota_snapshot(&f.id).unwrap();
        let forecast_before = f
            .runtime
            .obs
            .dashboard()
            .usage_forecast(&f.id, Utc::now(), None);
        let mut events = f.runtime.events.subscribe();
        let after = f.apply(observation).await;
        assert!(
            matches!(
                after.health,
                Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
            ),
            "{variant}"
        );
        assert!(!f.runtime.eligible(&f.id, None).await, "{variant}");
        if variant == "newer" || overlap {
            assert_eq!(after.health, before.health, "{variant}");
        }
        let canonical = f.runtime.obs.quota_snapshot(&f.id).unwrap();
        let name = if variant == "unrelated" { "7d" } else { "5h" };
        assert_eq!(
            canonical["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["name"] == name)
                .unwrap()["status"],
            "rejected",
            "{variant}: retained window rejection"
        );
        assert_eq!(
            canonical["signals"]["unified_status"], "rejected",
            "{variant}"
        );
        if overlap {
            assert_eq!(
                canonical, cache_before,
                "{variant}: R did not publish over H"
            );
            assert!(
                f.runtime.obs.placement_capacity_facts(&f.id).is_none(),
                "{variant}: no capacity publication"
            );
            let forecast = f
                .runtime
                .obs
                .dashboard()
                .usage_forecast(&f.id, Utc::now(), None);
            assert_eq!(
                forecast
                    .windows
                    .iter()
                    .find(|w| w.name == "5h")
                    .unwrap()
                    .observed_at,
                forecast_before
                    .windows
                    .iter()
                    .find(|w| w.name == "5h")
                    .unwrap()
                    .observed_at,
                "{variant}: no raw R forecast measurement"
            );
            while let Ok(event) = events.try_recv() {
                assert_ne!(
                    event["event"], "quota_updated",
                    "{variant}: no raw R quota publication"
                );
            }
        }
    }
}

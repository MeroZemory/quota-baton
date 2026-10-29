use super::*;

fn private_fixture() -> tempfile::TempDir {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    tempfile::Builder::new()
        .prefix("qb-forecast-log-")
        .tempdir_in(executable.parent().unwrap())
        .unwrap()
}
use serde_json::json;

const START: i64 = 1_800_000_000;
fn at(t: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(t, 0).unwrap()
}
fn stamp(t: i64) -> String {
    at(t).to_rfc3339()
}
fn account(id: u32, t: i64, short: Option<f64>, long: Option<f64>) -> AdminAccount {
    serde_json::from_value(json!({
        "id":format!("cx-{id:06x}"), "provider":"codex", "generation":1,
        "label":"private-label-must-not-be-recorded", "email":"private-email@example.invalid",
        "plan":"plus", "user_state":"active", "source":{"kind":"oauth"},
        "health":{"kind":"ready"}, "outside_used":false, "pool_eligible":true,
        "takeover_pending":false,"quota_cooling":null,"quota_credit_releasable":false,
        "observed_at":stamp(t),"quota_notice":null,"credits_in_use":false,
        "usage_forecast":{"as_of":stamp(t),"observed_at":null,"windows":[],"earliest_window":null,"credit":null,"truncated":false},
        "has_alternative":false,"last_refresh_at":null,"probe_pending":false,
        "pacing":{"backoff_until":null,"step":0,"release_interval_ms":0,"waiting":0},
        "windows":[
            {"name":"5h","used_pct":short,"reset_at":stamp(t+18000),"observed_at":stamp(t),"status":"allowed"},
            {"name":"7d","used_pct":long,"reset_at":stamp(t+604800),"observed_at":stamp(t),"status":"allowed"}
        ],
        "credits":null,"last_used_at":null,"bound_sessions":0,"token_expires_at":null,
        "drain":false,"allow_credits":false,"refresh_attention":false,"active_sessions":0,"spread_threshold":3
    })).unwrap()
}
fn pair(t: i64) -> Vec<AdminAccount> {
    vec![
        account(1, t, Some(80.), Some(20.)),
        account(2, t, Some(10.), Some(40.)),
    ]
}
fn records(log: &ForecastLog) -> Vec<Record> {
    let worker = log.worker.lock().unwrap();
    worker
        .segments()
        .unwrap()
        .iter()
        .flat_map(|(_, p)| {
            fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}
fn codex_outcomes(log: &ForecastLog) -> Vec<(i64, Option<f64>, Option<Unavailable>)> {
    records(log)
        .into_iter()
        .filter_map(|r| match r.event {
            Event::Outcome {
                provider,
                horizon_seconds,
                realised_r,
                unavailable,
                ..
            } if provider == "codex" => Some((horizon_seconds, realised_r, unavailable)),
            _ => None,
        })
        .collect()
}

#[test]
fn n7_origins_log_min_of_all_windows_no_model_and_no_private_metadata() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    assert!(log.due(at(START)));
    log.sample(at(START), &pair(START)).unwrap();
    assert!(!log.due(at(START + 59)));
    assert!(log.due(at(START + 60)));
    let entries = records(&log);
    let origins: Vec<_> = entries
        .iter()
        .filter_map(|r| match &r.event {
            Event::Origin { origin } => Some(origin),
            _ => None,
        })
        .collect();
    assert_eq!(origins.len(), 2);
    let origin = origins.into_iter().find(|o| o.provider == "codex").unwrap();
    assert_eq!(origin.cohort.len(), 2);
    assert!((origin.remaining.unwrap() - 0.8).abs() < 1e-12); // .2 + .6, not weekly .8 + .6.
    assert!(origin.forecast_1h.is_none() && origin.forecast_3h.is_none());
    assert_eq!(origin.cohort[0].windows.len(), 2);
    let wire = serde_json::to_string(&entries).unwrap();
    assert!(!wire.contains("private-label") && !wire.contains("private-email"));
    assert!(wire.contains("no_pooled_model"));
    log.sample(at(START + 60), &pair(START + 60)).unwrap();
    assert_eq!(log.status().unwrap().origins, 2); // no per-minute duplicate origin.
}
#[test]
fn n7_settles_both_horizons_on_frozen_cohort_despite_automatic_health_change() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    log.sample(at(START), &pair(START)).unwrap();
    for delta in [3600, 10800] {
        let mut current = pair(START + delta);
        for a in &mut current {
            a.pool_eligible = false;
            a.health = AdminHealth::Cooling {
                reason: qb_proto::admin_account::AdminHealthReason::Quota,
                until: stamp(START + delta + 60),
                scope: "account".into(),
                probe_pending: None,
            };
        }
        log.sample(at(START + delta), &current).unwrap();
    }
    let outcomes = codex_outcomes(&log);
    assert!(
        outcomes.iter().any(|(h, r, e)| *h == 3600
            && r.is_some_and(|r| (r - 0.8).abs() < 1e-12)
            && e.is_none())
    );
    assert!(
        outcomes.iter().any(|(h, r, e)| *h == 10800
            && r.is_some_and(|r| (r - 0.8).abs() < 1e-12)
            && e.is_none())
    );
    assert_eq!(log.status().unwrap().excluded_user_change, 0);
    assert_eq!(log.status().unwrap().origins, 6); // 0h, 1h, 3h; no fabricated 2h origin.
}
#[test]
fn n7_pause_resume_between_samples_excludes_and_counts_even_when_membership_returns() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    log.sample(at(START), &pair(START)).unwrap();
    log.account_state(
        "cx-000001",
        "codex",
        AdminUserState::Paused,
        false,
        false,
        None,
    )
    .unwrap();
    log.account_state(
        "cx-000001",
        "codex",
        AdminUserState::Active,
        false,
        false,
        None,
    )
    .unwrap();
    log.sample(at(START + 3600), &pair(START + 3600)).unwrap();
    assert_eq!(
        codex_outcomes(&log),
        vec![(3600, None, Some(Unavailable::UserChanged))]
    );
    assert_eq!(log.status().unwrap().excluded_user_change, 1);
}
#[test]
fn n7_pending_survives_orderly_takeover_without_exclusion() {
    let home = private_fixture();
    {
        let log = ForecastLog::open(home.path()).unwrap();
        log.sample(at(START), &pair(START)).unwrap();
        log.close().unwrap();
    }
    let log = ForecastLog::open(home.path()).unwrap();
    assert_eq!(log.worker.lock().unwrap().checkpoint.pending.len(), 2);
    log.sample(at(START + 3600), &pair(START + 3600)).unwrap();
    let outcomes = codex_outcomes(&log);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].0, 3600);
    assert!(outcomes[0].1.is_some_and(|v| (v - 0.8).abs() < 1e-12));
    assert_eq!(outcomes[0].2, None);
    assert_eq!(log.status().unwrap().excluded_restart, 0);
}
#[test]
fn n7_durable_outbox_recovery_does_not_duplicate_committed_records() {
    let home = private_fixture();
    {
        let log = ForecastLog::open(home.path()).unwrap();
        log.sample(at(START), &pair(START)).unwrap();
        let record = records(&log).into_iter().next().unwrap();
        let mut worker = log.worker.lock().unwrap();
        worker.checkpoint.outbox = vec![record];
        worker.save().unwrap();
    }
    let log = ForecastLog::open(home.path()).unwrap();
    assert_eq!(records(&log).len(), 2);
    assert!(log.worker.lock().unwrap().checkpoint.outbox.is_empty());
}
#[test]
fn n7_missing_measurements_are_unavailable_never_zero() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    log.sample(at(START), &pair(START)).unwrap();
    let mut next = pair(START + 3600);
    next[0].windows[0].used_pct = None;
    log.sample(at(START + 3600), &next).unwrap();
    assert_eq!(
        codex_outcomes(&log),
        vec![(3600, None, Some(Unavailable::InvalidWindow))]
    );
}
#[test]
fn n7_malformed_identity_retries_but_storage_cap_stops_with_durable_explicit_loss() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    let mut bad = pair(START);
    bad[0].id = "personal-identity@example.invalid".into();
    assert!(log.sample(at(START), &bad).is_err());
    assert!(!log.status().unwrap().stopped);
    assert_eq!(log.status().unwrap().losses, 1);
    drop(log);
    let log = ForecastLog::open(home.path()).unwrap();
    assert!(!log.due(at(START)));
    assert!(log.due(at(START + 60)));
    let other = private_fixture();
    let small = ForecastLog::open(other.path()).unwrap();
    small.worker.lock().unwrap().log_cap = 1;
    assert!(small.sample(at(START), &pair(START)).is_err());
    let checkpoint: Checkpoint = serde_json::from_slice(
        &fs::read(other.path().join("state/forecast-log/checkpoint.json")).unwrap(),
    )
    .unwrap();
    assert!(checkpoint.status.stopped);
    assert_eq!(checkpoint.status.losses, 1);
    assert_eq!(checkpoint.outbox.len(), 2); // original evidence retained, not silently dropped.
}
#[test]
fn n7_corrupt_checkpoint_and_torn_journal_are_rejected_without_fresh_fallback() {
    let home = private_fixture();
    let root = home.path().join("state/forecast-log");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("checkpoint.json"), b"broken").unwrap();
    assert!(ForecastLog::open(home.path()).is_err());
    assert_eq!(fs::read(root.join("checkpoint.json")).unwrap(), b"broken");
    let other = private_fixture();
    {
        let log = ForecastLog::open(other.path()).unwrap();
        log.sample(at(START), &pair(START)).unwrap();
    }
    let path = other
        .path()
        .join(format!("state/forecast-log/day-{}.jsonl", START / DAY));
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    assert!(ForecastLog::open(other.path()).is_err());
}
#[test]
fn n7_one_account_is_logged_but_never_scored_as_a_pool() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    let one = vec![account(1, START, Some(0.), Some(0.))];
    log.sample(at(START), &one).unwrap();
    let later = vec![account(1, START + 3600, Some(0.), Some(0.))];
    log.sample(at(START + 3600), &later).unwrap();
    assert_eq!(
        codex_outcomes(&log),
        vec![(3600, None, Some(Unavailable::TooFewAccounts))]
    );
}

#[test]
fn n7_native_claude_named_window_is_recorded_and_participates_in_the_minimum() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    let mut accounts = pair(START);
    for a in &mut accounts {
        a.provider = AdminProvider::Claude;
        a.plan = Some("max_20x".into());
        a.id = a.id.replacen("cx-", "cl-", 1);
    }
    let mut named = accounts[0].windows[1].clone();
    named.name = "7d_oi".into();
    named.used_pct = Some(90.);
    accounts[0].windows.push(named);
    log.sample(at(START), &accounts).unwrap();
    let origin = records(&log)
        .into_iter()
        .find_map(|r| match r.event {
            Event::Origin { origin } if origin.provider == "claude" => Some(origin),
            _ => None,
        })
        .unwrap();
    assert_eq!(origin.cohort[0].windows.len(), 3);
    assert!((origin.remaining.unwrap() - 0.7).abs() < 1e-12);
}

#[test]
fn n7_crash_loses_pause_resume_revision_but_unclean_marker_prevents_false_eligibility() {
    let home = private_fixture();
    {
        let log = ForecastLog::open(home.path()).unwrap();
        log.sample(at(START), &pair(START)).unwrap();
        log.account_state(
            "cx-000001",
            "codex",
            AdminUserState::Paused,
            false,
            false,
            None,
        )
        .unwrap();
        log.account_state(
            "cx-000001",
            "codex",
            AdminUserState::Active,
            false,
            false,
            None,
        )
        .unwrap();
        // Drop without close simulates a crash before those edits are flushed.
    }
    let log = ForecastLog::open(home.path()).unwrap();
    log.sample(at(START + 3600), &pair(START + 3600)).unwrap();
    assert_eq!(
        codex_outcomes(&log),
        vec![(3600, None, Some(Unavailable::RestartGap))]
    );
    assert_eq!(log.status().unwrap().excluded_restart, 2);
}
#[test]
fn n7_clean_close_flushes_latest_user_revision_and_disables_all_future_work() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    log.sample(at(START), &pair(START)).unwrap();
    log.account_state(
        "cx-000001",
        "codex",
        AdminUserState::Paused,
        false,
        false,
        None,
    )
    .unwrap();
    log.account_state(
        "cx-000001",
        "codex",
        AdminUserState::Active,
        false,
        false,
        None,
    )
    .unwrap();
    log.close().unwrap();
    let path = home.path().join("state/forecast-log/checkpoint.json");
    let before = fs::read(&path).unwrap();
    let checkpoint: Checkpoint = serde_json::from_slice(&before).unwrap();
    assert!(checkpoint.clean);
    assert!(!log.due(at(START + 3600)));
    assert!(log.sample(at(START + 3600), &pair(START + 3600)).is_err());
    assert!(
        log.account_state(
            "cx-000001",
            "codex",
            AdminUserState::Paused,
            false,
            false,
            None,
        )
        .is_err()
    );
    log.close().unwrap();
    assert_eq!(fs::read(path).unwrap(), before);
    drop(log);
    let log = ForecastLog::open(home.path()).unwrap();
    log.sample(at(START + 3600), &pair(START + 3600)).unwrap();
    assert_eq!(
        codex_outcomes(&log),
        vec![(3600, None, Some(Unavailable::UserChanged))]
    );
    assert_eq!(log.status().unwrap().excluded_user_change, 1);
}

#[test]
fn n7_target_observation_grace_is_120s_and_clean_restart_does_not_backfill() {
    for lateness in [120, 121, 3600] {
        let home = private_fixture();
        {
            let log = ForecastLog::open(home.path()).unwrap();
            log.sample(at(START), &pair(START)).unwrap();
            log.close().unwrap();
        }
        let log = ForecastLog::open(home.path()).unwrap();
        let sampled = START + 3600 + lateness;
        log.sample(at(sampled), &pair(sampled)).unwrap();
        let outcome = records(&log)
            .into_iter()
            .find_map(|r| match r.event {
                Event::Outcome {
                    provider,
                    origin_at,
                    horizon_seconds,
                    due_at,
                    sampled_at,
                    realised_r,
                    unavailable,
                    ..
                } if provider == "codex" && origin_at == START && horizon_seconds == 3600 => {
                    Some((due_at, sampled_at, realised_r, unavailable))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(outcome.0, START + 3600);
        assert_eq!(outcome.1, sampled);
        if lateness <= 120 {
            assert!(outcome.2.is_some_and(|r| (r - 0.8).abs() < 1e-12));
            assert_eq!(outcome.3, None);
        } else {
            assert_eq!(outcome.2, None);
            assert_eq!(outcome.3, Some(Unavailable::TargetObservationLate));
            assert!(log.status().unwrap().unavailable > 0);
        }
        assert!(!log.status().unwrap().stopped);
    }
}
#[test]
fn n7_missing_origin_window_censors_outcome_instead_of_inflating_r() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    log.sample(at(START), &pair(START)).unwrap();
    let sampled = START + 3600;
    let mut current = pair(sampled);
    // Original minimum was the 5h window (.2). Its disappearance would turn
    // this account into .8 if the later inventory were silently substituted.
    current[0].windows.retain(|w| w.name != "5h");
    log.sample(at(sampled), &current).unwrap();
    assert_eq!(
        codex_outcomes(&log),
        vec![(3600, None, Some(Unavailable::MissingOriginWindow))]
    );
    let observations = records(&log)
        .into_iter()
        .find_map(|r| match r.event {
            Event::Outcome {
                provider,
                observations,
                ..
            } if provider == "codex" => Some(observations),
            _ => None,
        })
        .unwrap();
    assert_eq!(observations[0].windows.len(), 1); // no manufactured history/window.
    assert!(!log.status().unwrap().stopped);
}
#[test]
fn n7_observer_snapshot_race_discards_counts_and_naturally_retries_without_stop() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    log.sample(at(START), &pair(START)).unwrap();
    let sequence = log.worker.lock().unwrap().checkpoint.sequence;
    log.account_state(
        "cx-000001",
        "codex",
        AdminUserState::Paused,
        false,
        false,
        None,
    )
    .unwrap();
    let due = START + 3600;
    // The DTO was assembled before the store observer saw pause.
    log.sample(at(due), &pair(due)).unwrap();
    let status = log.status().unwrap();
    assert_eq!(status.discarded_observer_races, 1);
    assert!(!status.stopped);
    assert_eq!(status.origins, 2);
    assert_eq!(status.outcomes, 0);
    assert_eq!(status.losses, 0);
    let checkpoint: Checkpoint = serde_json::from_slice(
        &fs::read(home.path().join("state/forecast-log/checkpoint.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(checkpoint.sequence, sequence);
    assert_eq!(checkpoint.last_sample, Some(START));
    assert_eq!(
        checkpoint.users.accounts["cx-000001"].state,
        AdminUserState::Paused
    );
    assert_eq!(
        log.users.lock().unwrap().accounts["cx-000001"].state,
        AdminUserState::Paused
    );
    assert!(checkpoint.pending.iter().all(|origin| !origin.done_1h));
    assert!(!log.due(at(due + 59)));
    assert!(log.due(at(due + 60)));
    let mut fresh = pair(due + 60);
    fresh[0].user_state = AdminUserState::Paused;
    fresh[0].pool_eligible = false;
    log.sample(at(due + 60), &fresh).unwrap();
    assert_eq!(log.status().unwrap().discarded_observer_races, 1);
    assert!(!log.status().unwrap().stopped);
    assert_eq!(
        codex_outcomes(&log),
        vec![(3600, None, Some(Unavailable::UserChanged))]
    );
    assert_eq!(
        log.worker.lock().unwrap().checkpoint.last_sample,
        Some(due + 60)
    );
    assert_eq!(log.status().unwrap().origins, 4);
}

/// Actual native producer integration. No DTO seeding, listeners, provider poller
/// or refresh scheduler: only the existing memory guard's immediate first tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n7_native_store_observer_periodic_sampler_and_orderly_reopen_preserve_pending() {
    use crate::{
        accounts::{AccountStore, Identity, Provider, Tokens, UserState},
        config::Config,
        debug_pin::DebugPins,
        observability::Observability,
        relay::RelayState,
    };
    use std::{sync::Arc, time::Duration};

    let fixture = private_fixture();
    let home = fixture.path().join("native-home");
    crate::platform::prepare_observation_directory(&home).unwrap();
    let mut config = Config::default();
    config.secrets.linux_backend = crate::config::LinuxSecretBackend::File0600;
    config.routing_replay.enabled = true;
    config.upstream.codex_base = "http://127.0.0.1:9".into();
    config.upstream.anthropic_base = "http://127.0.0.1:9".into();
    // Native replacement creates 0600 on Unix; no process umask dependency.
    crate::platform::atomic_write_account(
        &home,
        &home.join("config.toml"),
        toml::to_string(&config).unwrap().as_bytes(),
    )
    .unwrap();
    let config = Config::load(&home).unwrap();
    crate::platform::test_assert_snapshot_owner_only(&home.join("config.toml"));
    let store = Arc::new(AccountStore::acquire(&home).unwrap());
    let mut native_ids = Vec::new();
    for n in 0..2 {
        let mut native = crate::accounts::health::tests::fixture();
        native.identity = Identity::Codex {
            user_id: None,
            account_id: format!("synthetic-n7-native-{n}"),
            email: format!("synthetic-n7-{n}@example.invalid"),
        };
        native.id = native.identity.id();
        native.email = format!("synthetic-n7-{n}@example.invalid");
        native.label = format!("Synthetic N7 {n}");
        native.plan = Some("plus".into());
        native.token_meta.expires_at = Some(Utc::now() + chrono::Duration::days(365));
        let saved = store
            .save(
                &native,
                &Tokens {
                    access_token: format!("synthetic-n7-access-{n}"),
                    refresh_token: format!("synthetic-n7-refresh-{n}"),
                    id_token: None,
                },
                None,
            )
            .unwrap();
        native_ids.push(saved.id);
    }
    let obs = Arc::new(Observability::new(&home, None).unwrap());
    let relay = RelayState::new(
        store.clone(),
        &config,
        obs.clone(),
        Arc::new(DebugPins::default()),
    )
    .unwrap();
    let log = obs
        .forecast_log()
        .expect("actual Relay startup must publish N7 logger");
    assert_eq!(log.status().unwrap().origins, 0);
    // Feed actual local response-header observations through the existing native
    // quota parser and persisted health reducer. This performs no HTTP request.
    for (n, id) in native_ids.iter().enumerate() {
        let mut headers = axum::http::HeaderMap::new();
        for (key, value) in [
            (
                "x-codex-primary-used-percent",
                if n == 0 { "80" } else { "10" },
            ),
            (
                "x-codex-secondary-used-percent",
                if n == 0 { "20" } else { "40" },
            ),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-primary-reset-after-seconds", "18000"),
            ("x-codex-secondary-reset-after-seconds", "604800"),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
        let snapshot = obs
            .observe_headers_snapshot(id, Provider::Codex, &headers)
            .unwrap();
        relay
            .accounts
            .apply_quota_snapshot(id, snapshot, None)
            .await
            .unwrap();
    }
    let guard = relay.start_memory_guard();
    let sampler = relay.start_forecast_log_sampler();
    let sampled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let log = log.clone();
            let status = tokio::task::spawn_blocking(move || log.status())
                .await
                .map_err(|_| ())?
                .map_err(|_| ())?;
            if status.stopped {
                return Err(());
            }
            if status.origins >= 2 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    // Reap both tasks before assertions, including the timeout/failure path.
    guard.abort();
    if let Some(sampler) = &sampler {
        sampler.abort();
    }
    let guard_joined = guard.await;
    let sampler_joined = match sampler {
        Some(sampler) => Some(sampler.await),
        None => None,
    };
    assert!(matches!(guard_joined, Err(error) if error.is_cancelled()));
    assert!(matches!(sampler_joined, Some(Err(error)) if error.is_cancelled()));
    assert!(
        matches!(sampled, Ok(Ok(()))),
        "actual first periodic sample must complete within bound"
    );
    let origin = records(&log)
        .into_iter()
        .find_map(|record| match record.event {
            Event::Origin { origin } if origin.provider == "codex" => Some(origin),
            _ => None,
        })
        .expect("actual sampler must append native Codex origin");
    assert_eq!(origin.cohort.len(), 2);
    let mut logged_ids: Vec<_> = origin.cohort.iter().map(|input| input.id.clone()).collect();
    logged_ids.sort();
    native_ids.sort();
    assert_eq!(logged_ids, native_ids);
    assert!(origin.remaining.is_some_and(|r| (r - 0.8).abs() < 1e-12));
    assert!(
        origin
            .cohort
            .iter()
            .all(|input| input.native_usable && input.windows.len() == 2)
    );
    assert!(origin.forecast_1h.is_none() && origin.forecast_3h.is_none());
    // No direct logger observation: these public store transactions must reach
    // the actual AccountRuntime -> Observability -> ForecastLog callback chain.
    let revision = log.users.lock().unwrap().revision("codex");
    store
        .update_settings(&native_ids[0], None, None, |account| {
            account.user_state = UserState::Paused
        })
        .unwrap();
    store
        .update_settings(&native_ids[0], None, None, |account| {
            account.user_state = UserState::Active
        })
        .unwrap();
    assert_eq!(log.users.lock().unwrap().revision("codex"), revision + 2);
    assert_eq!(
        store.load(&native_ids[0]).unwrap().user_state,
        UserState::Active
    );
    relay.shutdown_refreshes().await.unwrap();
    let close_store = store.clone();
    let close_bindings = relay.bindings.clone();
    let close_obs = obs.clone();
    tokio::task::spawn_blocking(move || {
        close_store.stop_writes();
        close_bindings.freeze().unwrap();
        close_obs.flush().unwrap();
        close_obs.shutdown_replay();
        close_store.release_lease();
    })
    .await
    .unwrap();
    let checkpoint: Checkpoint =
        serde_json::from_slice(&fs::read(home.join("state/forecast-log/checkpoint.json")).unwrap())
            .unwrap();
    assert!(checkpoint.clean);
    assert_eq!(checkpoint.pending.len(), 2);
    assert_eq!(checkpoint.users.revision("codex"), revision + 2);
    assert!(
        checkpoint
            .pending
            .iter()
            .all(|origin| !origin.done_1h && !origin.done_3h)
    );
    assert!(!log.due(Utc::now()));
    drop(relay);
    drop(store);
    drop(obs);
    drop(log);

    let restored_store = Arc::new(AccountStore::acquire(&home).unwrap());
    let restored_obs = Arc::new(Observability::new(&home, None).unwrap());
    let restored = RelayState::new(
        restored_store.clone(),
        &config,
        restored_obs.clone(),
        Arc::new(DebugPins::default()),
    )
    .unwrap();
    let restored_log = restored_obs.forecast_log().unwrap();
    {
        let worker = restored_log.worker.lock().unwrap();
        assert_eq!(worker.checkpoint.pending.len(), checkpoint.pending.len());
        assert!(
            worker
                .checkpoint
                .pending
                .iter()
                .all(|origin| !origin.restart_gap)
        );
        assert_eq!(worker.checkpoint.users.revision("codex"), revision + 2);
        let pending = worker
            .checkpoint
            .pending
            .iter()
            .find(|origin| origin.provider == "codex")
            .unwrap();
        assert_eq!(pending.at, origin.at);
        assert_eq!(pending.remaining, origin.remaining);
        assert_eq!(
            pending
                .cohort
                .iter()
                .map(|input| &input.id)
                .collect::<Vec<_>>(),
            origin
                .cohort
                .iter()
                .map(|input| &input.id)
                .collect::<Vec<_>>()
        );
    }
    let finish_bindings = restored.bindings.clone();
    tokio::task::spawn_blocking(move || {
        restored_store.stop_writes();
        finish_bindings.freeze().unwrap();
        restored_obs.flush().unwrap();
        restored_obs.shutdown_replay();
        restored_store.release_lease();
    })
    .await
    .unwrap();
    assert!(!restored_log.due(Utc::now()));
}

#[test]
fn n7_allow_credits_on_noncohort_account_excludes_the_frozen_cohort() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    let mut initial = pair(START);
    let mut third = account(3, START, Some(100.), Some(20.));
    third.pool_eligible = false;
    third.quota_credit_releasable = true;
    third.health = AdminHealth::Cooling {
        reason: qb_proto::admin_account::AdminHealthReason::Quota,
        until: stamp(START + 18000),
        scope: "account".into(),
        probe_pending: None,
    };
    initial.push(third);
    log.sample(at(START), &initial).unwrap();
    let original = records(&log)
        .into_iter()
        .find_map(|record| match record.event {
            Event::Origin { origin } if origin.provider == "codex" => Some(origin),
            _ => None,
        })
        .unwrap();
    let frozen_ids: Vec<_> = original.cohort.iter().map(|a| a.id.clone()).collect();
    assert_eq!(frozen_ids, vec!["cx-000001", "cx-000002"]);
    assert!(original.cohort.iter().all(|a| !a.allow_credits));
    let revision = log.users.lock().unwrap().revision("codex");
    // Native health tests cover credit release. Exercise the public account
    // observer for this user setting, then the resulting ready DTO snapshot.
    let mut native = crate::accounts::health::tests::fixture();
    native.id = "cx-000003".into();
    native.provider = crate::accounts::Provider::Codex;
    native.user_state = crate::accounts::UserState::Active;
    native.drain = false;
    native.allow_credits = true;
    log.observe_account(&native).unwrap();
    assert_eq!(log.users.lock().unwrap().revision("codex"), revision + 1);
    assert!(log.users.lock().unwrap().accounts["cx-000003"].allow_credits);
    let due = START + 3600;
    let mut current = pair(due);
    let mut third = account(3, due, Some(100.), Some(20.));
    third.allow_credits = true;
    current.push(third);
    assert!(usable(&current[2]));
    log.sample(at(due), &current).unwrap();
    assert_eq!(
        codex_outcomes(&log),
        vec![(3600, None, Some(Unavailable::UserChanged))]
    );
    let outcome_ids = records(&log)
        .into_iter()
        .find_map(|record| match record.event {
            Event::Outcome {
                provider,
                origin_at,
                cohort,
                ..
            } if provider == "codex" && origin_at == START => Some(cohort),
            _ => None,
        })
        .unwrap();
    assert_eq!(outcome_ids, frozen_ids);
    assert_eq!(log.status().unwrap().excluded_user_change, 1);
    assert_eq!(log.status().unwrap().discarded_observer_races, 0);
    assert!(!log.status().unwrap().stopped);
    let next = records(&log)
        .into_iter()
        .find_map(|record| match record.event {
            Event::Origin { origin } if origin.provider == "codex" && origin.at == due => {
                Some(origin)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(next.cohort.len(), 3);
    assert!(
        next.cohort
            .iter()
            .any(|a| a.id == "cx-000003" && a.allow_credits)
    );
}

#[test]
fn n7_native_generation_witness_distinguishes_closed_gaps_from_live_updates() {
    use crate::{
        accounts::{AccountStore, Identity, Tokens, UserState},
        config::{Config, LinuxSecretBackend},
    };
    use std::sync::Arc;

    for scenario in [
        "offline_pause_resume",
        "matching_generation",
        "live_metadata",
        "missing_witness",
    ] {
        let fixture = private_fixture();
        let home = fixture.path().join("continuity-home");
        crate::platform::prepare_observation_directory(&home).unwrap();
        let mut config = Config::default();
        config.secrets.linux_backend = LinuxSecretBackend::File0600;
        crate::platform::atomic_write_account(
            &home,
            &home.join("config.toml"),
            toml::to_string(&config).unwrap().as_bytes(),
        )
        .unwrap();
        let store = AccountStore::acquire(&home).unwrap();
        let log = Arc::new(ForecastLog::open(&home).unwrap());
        let mut native_accounts = Vec::new();
        for index in 0..2 {
            let mut account = crate::accounts::health::tests::fixture();
            account.identity = Identity::Codex {
                user_id: None,
                account_id: format!("synthetic-n7-continuity-{index}"),
                email: format!("synthetic-n7-continuity-{index}@example.invalid"),
            };
            account.id = account.identity.id();
            account.email = format!("synthetic-n7-continuity-{index}@example.invalid");
            account.plan = Some("plus".into());
            let account = store
                .save(
                    &account,
                    &Tokens {
                        access_token: format!("synthetic-n7-continuity-access-{index}"),
                        refresh_token: format!("synthetic-n7-continuity-refresh-{index}"),
                        id_token: None,
                    },
                    None,
                )
                .unwrap();
            log.observe_account(&account).unwrap();
            native_accounts.push(account);
        }
        let mut initial = pair(START);
        for (view, native) in initial.iter_mut().zip(&native_accounts) {
            view.id = native.id.clone();
            view.generation = native.generation;
        }
        log.sample(at(START), &initial).unwrap();
        let revision = log.users.lock().unwrap().revision("codex");
        let id = native_accounts[0].id.clone();
        let saved_generation = native_accounts[0].generation;
        if scenario == "live_metadata" {
            let observer = log.clone();
            store.set_observer(Arc::new(move |account, _| {
                observer.observe_account(account).unwrap();
            }));
            // An automatic metadata counter changes generation, not user facts.
            let updated = store
                .update_metadata(&id, |account| {
                    account.auth_failures = account.auth_failures.saturating_add(1);
                })
                .unwrap();
            assert_eq!(updated.generation, saved_generation + 1);
            assert_eq!(log.users.lock().unwrap().revision("codex"), revision);
            assert_eq!(log.users.lock().unwrap().losses, 0);
            assert_eq!(
                log.users.lock().unwrap().generations[&id],
                updated.generation
            );
            // Repeated observations in this owner never reclassify the change.
            log.observe_account(&updated).unwrap();
            assert_eq!(log.users.lock().unwrap().losses, 0);
        }
        store.stop_writes();
        log.close().unwrap();
        let path = home.join("state/forecast-log/checkpoint.json");
        let checkpoint: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(checkpoint["users"].get("session_seen").is_none());
        drop(store);
        drop(log);

        if scenario == "offline_pause_resume" {
            // Same native Store API as the stopped-core CLI fallback, without
            // a live logger/observer. Final Active facts hide both transitions.
            let offline = AccountStore::acquire(&home).unwrap();
            let paused = offline
                .update_metadata(&id, |account| account.user_state = UserState::Paused)
                .unwrap();
            let resumed = offline
                .update_metadata(&id, |account| account.user_state = UserState::Active)
                .unwrap();
            assert_eq!(paused.generation, saved_generation + 1);
            assert_eq!(resumed.generation, saved_generation + 2);
            assert_eq!(resumed.user_state, native_accounts[0].user_state);
            assert_eq!(resumed.drain, native_accounts[0].drain);
            assert_eq!(resumed.allow_credits, native_accounts[0].allow_credits);
            drop(offline);
        } else if scenario == "missing_witness" {
            // Legacy absence is unknown, not generation zero or a fresh baseline.
            let mut legacy: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            legacy["users"]
                .as_object_mut()
                .unwrap()
                .remove("generations");
            fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        }
        let store = AccountStore::acquire(&home).unwrap();
        let log = ForecastLog::open(&home).unwrap();
        assert!(log.users.lock().unwrap().session_seen.is_empty());
        let mut current = pair(START + 3600);
        for (view, native) in current.iter_mut().zip(&native_accounts) {
            let actual = store.load(&native.id).unwrap();
            log.observe_account(&actual).unwrap();
            view.id = actual.id;
            view.generation = actual.generation;
        }
        let expected_gaps = match scenario {
            "offline_pause_resume" => 1,
            "missing_witness" => 2,
            _ => 0,
        };
        assert_eq!(log.users.lock().unwrap().losses, expected_gaps);
        assert_eq!(log.users.lock().unwrap().revision("codex"), revision);
        log.sample(at(START + 3600), &current).unwrap();
        let outcomes = codex_outcomes(&log);
        if expected_gaps > 0 {
            assert_eq!(outcomes, vec![(3600, None, Some(Unavailable::ObserverGap))]);
            assert!(log.status().unwrap().excluded_observer_gap > 0);
        } else {
            assert_eq!(outcomes.len(), 1);
            assert!(
                outcomes[0]
                    .1
                    .is_some_and(|value| (value - 0.8).abs() < 1e-12)
            );
            assert_eq!(outcomes[0].2, None);
            assert_eq!(log.status().unwrap().excluded_observer_gap, 0);
        }
        assert_eq!(log.status().unwrap().excluded_user_change, 0);
        assert!(!log.status().unwrap().stopped);
        // Removal bounds both the durable witness map and owner-local seen set.
        log.sample(at(START + 3660), &[]).unwrap();
        {
            let users = log.users.lock().unwrap();
            assert!(users.accounts.is_empty());
            assert!(users.generations.is_empty());
            assert!(users.session_seen.is_empty());
        }
        store.stop_writes();
        log.close().unwrap();
    }
}

#[test]
fn n7_native_window_names_are_exact_and_numeric_durations_are_bounded_u64() {
    for name in [
        "primary",
        "secondary",
        "7d_oi",
        "7d_sonnet",
        "5h",
        "7d",
        "1s",
        "18446744073709551615s",
        "18446744073709551615m",
    ] {
        assert!(window_name(name), "native name rejected: {name}");
    }
    for name in [
        "Primary",
        "primaryx",
        " secondary",
        "0s",
        "000m",
        "-1h",
        "+1d",
        "1.5h",
        "1w",
        "18446744073709551616s",
        "000000000000000000001s",
        "1\u{e9}",
    ] {
        assert!(!window_name(name), "non-native name accepted: {name}");
    }
}

#[test]
fn n7_diagnostics_are_static_typed_and_do_not_match_arbitrary_error_substrings() {
    for (code, reason) in [
        (
            "forecast_log_window_name_invalid",
            FailureReason::WindowName,
        ),
        (
            "forecast_log_duplicate_window",
            FailureReason::DuplicateWindow,
        ),
        ("forecast_log_forecast_invalid", FailureReason::Forecast),
        ("forecast_log_timestamp_invalid", FailureReason::Timestamp),
        ("forecast_log_invalid_identity", FailureReason::Identity),
        (
            "forecast_log_duplicate_account",
            FailureReason::DuplicateAccount,
        ),
        ("forecast_log_log_cap", FailureReason::Bounds),
        ("forecast_log_clock_rollback", FailureReason::Clock),
        ("forecast_log_sequence_invalid", FailureReason::Sequence),
        ("forecast_log_checkpoint_write", FailureReason::Storage),
        ("forecast_log_closed", FailureReason::Closed),
        ("forecast_log_stopped", FailureReason::Stopped),
        ("forecast_log_observer_poisoned", FailureReason::Poisoned),
        ("forecast_log_task_join", FailureReason::TaskJoin),
        (
            "forecast_log_snapshot_unavailable",
            FailureReason::SnapshotUnavailable,
        ),
    ] {
        let error = anyhow::anyhow!(code).context("private-error-detail");
        let failure = diagnostic(&error, FailurePhase::Sample);
        assert_eq!(
            failure,
            FailureDiagnostic {
                phase: FailurePhase::Sample,
                reason
            }
        );
        let wire = serde_json::to_string(&failure).unwrap();
        assert!(!wire.contains("private-error-detail") && !wire.contains("forecast_log_"));
        assert_eq!(
            serde_json::from_str::<FailureDiagnostic>(&wire).unwrap(),
            failure
        );
        assert_eq!(
            wire,
            format!(
                "{{\"phase\":\"sample\",\"reason\":\"{}\"}}",
                reason.as_str()
            )
        );
    }
    let unknown = anyhow::anyhow!("private path forecast_log_window_name_invalid suffix");
    assert_eq!(
        diagnostic(&unknown, FailurePhase::Open).reason,
        FailureReason::Unknown
    );
    let io = anyhow::Error::new(std::io::Error::other("private path/body"));
    assert_eq!(
        diagnostic(&io, FailurePhase::Archive).reason,
        FailureReason::Storage
    );
    let original = FailureDiagnostic {
        phase: FailurePhase::Sample,
        reason: FailureReason::WindowName,
    };
    let chained = io.context(original);
    assert_eq!(diagnostic(&chained, FailurePhase::Close), original);
    let mut legacy = serde_json::to_value(Checkpoint::default()).unwrap();
    legacy.as_object_mut().unwrap().remove("last_failure");
    assert!(
        serde_json::from_value::<Checkpoint>(legacy)
            .unwrap()
            .last_failure
            .is_none()
    );
}

#[test]
fn n7_invalid_sample_retries_persists_first_reason_and_later_good_sample_settles() {
    for reason in [
        FailureReason::WindowName,
        FailureReason::DuplicateWindow,
        FailureReason::DuplicateAccount,
        FailureReason::Timestamp,
        FailureReason::Forecast,
    ] {
        let home = private_fixture();
        let log = ForecastLog::open(home.path()).unwrap();
        let mut accounts = pair(START);
        match reason {
            FailureReason::WindowName => {
                accounts[0].windows[0].name = "private-invalid-window".into()
            }
            FailureReason::DuplicateWindow => accounts[0].windows[1].name = "5h".into(),
            FailureReason::DuplicateAccount => accounts[1] = accounts[0].clone(),
            FailureReason::Timestamp => {
                accounts[0].windows[0].observed_at = Some("private-invalid-time".into())
            }
            FailureReason::Forecast => {
                accounts[0].usage_forecast.earliest_window = Some("private-invalid-forecast".into())
            }
            _ => unreachable!(),
        }
        let error = log.sample(at(START), &accounts).unwrap_err();
        let failure = diagnostic(&error, FailurePhase::Sample);
        assert_eq!(
            failure,
            FailureDiagnostic {
                phase: FailurePhase::Sample,
                reason
            }
        );
        let path = home.path().join("state/forecast-log/checkpoint.json");
        let checkpoint: Checkpoint = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(checkpoint.last_failure, Some(failure));
        assert!(!checkpoint.status.stopped);
        assert_eq!(checkpoint.status.losses, 1);
        assert_eq!(checkpoint.status.unavailable, 1);
        assert_eq!(checkpoint.retry_at, Some(START + 60));
        assert_eq!(checkpoint.sequence, 0);
        assert_eq!(checkpoint.status.origins, 0);
        assert!(checkpoint.last_sample.is_none() && checkpoint.last_origin_hour.is_none());
        assert!(checkpoint.pending.is_empty() && checkpoint.outbox.is_empty());
        assert!(records(&log).is_empty());
        // Actual observer callbacks remain accepted while the sampler waits.
        let mut native = crate::accounts::health::tests::fixture();
        native.id = accounts[0].id.clone();
        if reason == FailureReason::DuplicateAccount {
            native.id = "cx-000001".into();
        }
        log.observe_account(&native).unwrap();
        log.close().unwrap();
        drop(log);
        let reopened = ForecastLog::open(home.path()).unwrap();
        assert!(!reopened.due(at(START + 59)));
        assert!(reopened.due(at(START + 60)));
        assert_eq!(reopened.last_failure().unwrap(), Some(failure));
        // A different invalid sample must not replace the first reason or add records.
        let mut bad = pair(START + 60);
        bad[0].id = "invalid".into();
        assert!(reopened.sample(at(START + 60), &bad).is_err());
        assert_eq!(reopened.last_failure().unwrap(), Some(failure));
        assert!(records(&reopened).is_empty());
        let good_at = START + 120;
        reopened.sample(at(good_at), &pair(good_at)).unwrap();
        assert_eq!(reopened.worker.lock().unwrap().checkpoint.retry_at, None);
        assert_eq!(
            reopened.worker.lock().unwrap().checkpoint.last_sample,
            Some(good_at)
        );
        assert_eq!(records(&reopened).len(), 2);
        reopened.close().unwrap();
        drop(reopened);
        let resumed = ForecastLog::open(home.path()).unwrap();
        resumed
            .sample(at(good_at + 3600), &pair(good_at + 3600))
            .unwrap();
        let outcomes = codex_outcomes(&resumed);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].1.is_some_and(|r| (r - 0.8).abs() < 1e-12));
        assert_eq!(outcomes[0].2, None);
        assert!(!resumed.status().unwrap().stopped);
        assert_eq!(resumed.last_failure().unwrap(), Some(failure));
        resumed.close().unwrap();
    }
}

#[tokio::test]
async fn n7_native_parser_slot_names_reach_actual_runtime_views_and_log_without_invented_durations()
{
    use crate::{
        account_runtime::AccountRuntime,
        accounts::{AccountStore, Identity, Provider, Tokens},
        bindings::BindingTable,
        config::{Config, LinuxSecretBackend},
        observability::Observability,
    };
    use std::sync::Arc;

    for native_slots in [false, true] {
        let fixture = private_fixture();
        let home = fixture.path().join("parser-home");
        crate::platform::prepare_observation_directory(&home).unwrap();
        let mut config = Config::default();
        config.secrets.linux_backend = LinuxSecretBackend::File0600;
        crate::platform::atomic_write_account(
            &home,
            &home.join("config.toml"),
            toml::to_string(&config).unwrap().as_bytes(),
        )
        .unwrap();
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        obs.configure_forecast(true);
        let runtime = AccountRuntime::new(store.clone(), obs.clone(), 0.0);
        let bindings = BindingTable::new(&home, config.routing).unwrap();
        let log = Arc::new(ForecastLog::open(&home).unwrap());
        let now = Utc::now();
        for (index, plan) in ["pro", "plus", "free", "plus"].into_iter().enumerate() {
            let mut native = crate::accounts::health::tests::fixture();
            native.identity = Identity::Codex {
                user_id: None,
                account_id: format!("synthetic-n7-parser-{index}"),
                email: format!("synthetic-n7-parser-{index}@example.invalid"),
            };
            native.id = native.identity.id();
            native.plan = Some(plan.into());
            native.token_meta.expires_at = Some(now + chrono::Duration::days(365));
            let saved = store
                .save(
                    &native,
                    &Tokens {
                        access_token: format!("synthetic-n7-parser-access-{index}"),
                        refresh_token: format!("synthetic-n7-parser-refresh-{index}"),
                        id_token: None,
                    },
                    None,
                )
                .unwrap();
            log.observe_account(&saved).unwrap();
            // Actual native parser output, not a constructed DTO or guessed bucket duration.
            let used = [25, 50, 40, 10][index];
            let (minutes, secondary) = if index == 2 {
                (43200, 0)
            } else if index == 3 {
                (300, 10080)
            } else {
                (300, 0)
            };
            let body = if native_slots {
                json!({"plan_type":plan,"rate_limit": {
                    "primary_window": {"used_percent":used,"limit_window_seconds":minutes * 60u64},
                    "secondary_window": {"used_percent":if index == 3 {80} else {0},"limit_window_seconds":secondary * 60u64}
                }})
            } else {
                json!({"rate_limit": {
                    "primary_window": {"used_percent":25}, "secondary_window": {"used_percent":50}
                }})
            };
            let body = serde_json::to_vec(&body).unwrap();
            let parsed =
                crate::quota::parse_snapshot_for_test(Provider::Codex, &body, now).unwrap();
            let snapshot = obs.observe_poll(&saved.id, parsed, true).unwrap();
            runtime
                .apply_quota_snapshot(&saved.id, snapshot, None)
                .await
                .unwrap();
            if native_slots {
                // The native HTTP header parser is the actual producer of 0m.
                // Body parsing deliberately omits explicit duration-zero windows.
                let mut headers = axum::http::HeaderMap::new();
                for (key, value) in [
                    ("x-codex-primary-used-percent", used.to_string()),
                    ("x-codex-primary-window-minutes", minutes.to_string()),
                    (
                        "x-codex-secondary-used-percent",
                        if index == 3 { "80" } else { "0" }.into(),
                    ),
                    ("x-codex-secondary-window-minutes", secondary.to_string()),
                ] {
                    headers.insert(key, value.parse().unwrap());
                }
                let snapshot = obs
                    .observe_headers_snapshot(&saved.id, Provider::Codex, &headers)
                    .unwrap();
                runtime
                    .apply_quota_snapshot(&saved.id, snapshot, None)
                    .await
                    .unwrap();
            }
        }
        // A cached orphan's native quota report is not a stored account and
        // must never become a typed account or a pooled cohort member.
        let mut orphan_headers = axum::http::HeaderMap::new();
        for (key, value) in [
            ("x-codex-primary-used-percent", "20"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-secondary-used-percent", "0"),
            ("x-codex-secondary-window-minutes", "0"),
        ] {
            orphan_headers.insert(key, value.parse().unwrap());
        }
        assert!(
            obs.observe_headers_snapshot("cx-413c3b", Provider::Codex, &orphan_headers)
                .is_some()
        );
        let now = Utc::now();
        let views = runtime.typed_views_with_bindings(&bindings).await.unwrap();
        assert_eq!(views.len(), 4);
        assert!(!views.iter().any(|a| a.id == "cx-413c3b")); // No native account file: never an input.
        for view in &views {
            assert!(view.pool_eligible);
            if native_slots {
                assert!(
                    view.windows
                        .iter()
                        .all(|w| matches!(w.name.as_str(), "5h" | "7d" | "0m" | "43200m"))
                );
                if matches!(view.plan.as_deref(), Some("pro" | "free")) {
                    assert!(view.windows.iter().any(|w| w.name == "0m"));
                }
                if view.plan.as_deref() == Some("free") {
                    assert!(view.windows.iter().any(|w| w.name == "43200m"));
                    assert!(
                        view.usage_forecast
                            .windows
                            .iter()
                            .all(|w| w.window_seconds == 30 * 86400)
                    );
                }
            } else {
                assert_eq!(
                    view.windows
                        .iter()
                        .map(|w| w.name.as_str())
                        .collect::<Vec<_>>(),
                    vec!["primary", "secondary"]
                );
                assert!(view.usage_forecast.windows.is_empty());
            }
        }
        let sampling = log.clone();
        tokio::task::spawn_blocking(move || sampling.sample(now, &views))
            .await
            .unwrap()
            .unwrap();
        let origin = records(&log)
            .into_iter()
            .find_map(|record| match record.event {
                Event::Origin { origin } if origin.provider == "codex" => Some(origin),
                _ => None,
            })
            .unwrap();
        if native_slots {
            assert!(origin.remaining.is_some_and(|r| (r - 2.05).abs() < 1e-12)); // .75 + .5 + .6 + .2.
            assert!(
                origin
                    .cohort
                    .iter()
                    .all(|input| input.windows.iter().all(|w| w.name != "0m"))
            );
            assert!(
                origin
                    .cohort
                    .iter()
                    .any(|input| input.windows.iter().any(|w| w.name == "43200m"))
            );
        } else {
            assert_eq!(origin.remaining, None);
            assert!(
                origin
                    .cohort
                    .iter()
                    .all(|input| input.existing_forecast.windows.is_empty()
                        && input.windows.is_empty()
                        && input.remaining.is_none()
                        && input.unavailable == Some(Unavailable::MissingWindow))
            );
        }
        assert!(origin.forecast_1h.is_none() && origin.forecast_3h.is_none());
        assert_eq!(
            log.worker.lock().unwrap().checkpoint.last_sample,
            Some(now.timestamp())
        );
        assert!(log.worker.lock().unwrap().checkpoint.last_failure.is_none());
        store.stop_writes();
        bindings.freeze().unwrap();
        tokio::task::spawn_blocking(move || {
            log.close().unwrap();
            obs.flush().unwrap();
        })
        .await
        .unwrap();
        store.release_lease();
    }
}

#[test]
fn n7_legacy_empty_stop_archives_exact_bytes_and_starts_only_new_origin() {
    for clean in [false, true] {
        let home = private_fixture();
        let root = home.path().join("state/forecast-log");
        crate::platform::prepare_observation_directory(&root).unwrap();
        let failed = Checkpoint {
            clean,
            status: Status {
                stopped: true,
                losses: 1,
                ..Status::default()
            },
            ..Checkpoint::default()
        };
        let mut legacy = serde_json::to_value(failed).unwrap();
        legacy.as_object_mut().unwrap().remove("last_failure");
        legacy.as_object_mut().unwrap().remove("recovery");
        let original = serde_json::to_vec(&legacy).unwrap();
        crate::platform::atomic_write_observation(&root.join("checkpoint.json"), &original)
            .unwrap();
        let log = ForecastLog::open(home.path()).unwrap();
        let archive = {
            let worker = log.worker.lock().unwrap();
            assert!(!worker.checkpoint.status.stopped);
            assert_eq!(worker.checkpoint.status.losses, 0);
            assert_eq!(worker.checkpoint.sequence, 0);
            assert_eq!(worker.checkpoint.last_sample, None);
            assert_eq!(worker.checkpoint.last_origin_hour, None);
            assert!(worker.checkpoint.pending.is_empty());
            let receipt = worker.checkpoint.recovery.as_ref().unwrap();
            assert_eq!(receipt.prior_losses, 1);
            home.path().join("state").join(&receipt.archive)
        };
        assert_eq!(fs::read(archive.join("checkpoint.json")).unwrap(), original);
        // First production sampling uses now; recovery copies no earlier rows.
        let now = Utc::now();
        log.sample(now, &pair(now.timestamp())).unwrap();
        let fresh = records(&log);
        assert_eq!(fresh.len(), 2);
        assert!(
            fresh
                .iter()
                .all(|record| record.sampled_at == now.timestamp())
        );
        assert_eq!(log.worker.lock().unwrap().checkpoint.sequence, 2);
        log.close().unwrap();
        drop(log);
        let reopened = ForecastLog::open(home.path()).unwrap();
        assert!(!reopened.status().unwrap().stopped);
        assert_eq!(reopened.worker.lock().unwrap().checkpoint.sequence, 2);
        assert_eq!(
            fs::read_dir(home.path().join("state"))
                .unwrap()
                .filter(|entry| entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("forecast-log.failed-"))
                .count(),
            1
        );
        assert_eq!(fs::read(archive.join("checkpoint.json")).unwrap(), original);
        reopened.close().unwrap();
    }
}

#[test]
fn n7_stopped_history_or_coded_failure_is_not_automatically_reset() {
    for with_history in [false, true] {
        let home = private_fixture();
        let root = home.path().join("state/forecast-log");
        let log = ForecastLog::open(home.path()).unwrap();
        if with_history {
            log.sample(at(START), &pair(START)).unwrap();
        }
        log.close().unwrap();
        drop(log);
        let mut checkpoint: Checkpoint =
            serde_json::from_slice(&fs::read(root.join("checkpoint.json")).unwrap()).unwrap();
        checkpoint.status.stopped = true;
        checkpoint.status.losses = 1;
        checkpoint.last_failure = Some(FailureDiagnostic {
            phase: FailurePhase::Sample,
            reason: if with_history {
                FailureReason::WindowName
            } else {
                FailureReason::Forecast
            },
        });
        crate::platform::atomic_write_observation(
            &root.join("checkpoint.json"),
            &serde_json::to_vec(&checkpoint).unwrap(),
        )
        .unwrap();
        let reopened = ForecastLog::open(home.path()).unwrap();
        assert!(reopened.status().unwrap().stopped);
        assert!(!reopened.due(Utc::now()));
        assert!(
            reopened
                .worker
                .lock()
                .unwrap()
                .checkpoint
                .recovery
                .is_none()
        );
        assert_eq!(
            reopened.worker.lock().unwrap().checkpoint.sequence,
            if with_history { 2 } else { 0 }
        );
        assert_eq!(records(&reopened).len(), if with_history { 2 } else { 0 });
        assert_eq!(
            fs::read_dir(home.path().join("state"))
                .unwrap()
                .filter(|entry| entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("forecast-log.failed-"))
                .count(),
            0
        );
        reopened.close().unwrap();
    }
}

#[test]
fn n7_ignored_slots_never_contribute_capacity_or_validate_ignored_payloads() {
    let mut view = account(1, START, Some(20.), Some(40.));
    for name in ["0m", "primary", "secondary", "0m"] {
        let mut ignored = view.windows[0].clone();
        ignored.name = name.into();
        ignored.used_pct = Some(f64::NAN);
        ignored.observed_at = Some("invalid-ignored-observation".into());
        ignored.reset_at = Some("invalid-ignored-reset".into());
        view.windows.push(ignored);
    }
    let measured = input(&view, START).unwrap();
    assert_eq!(measured.windows.len(), 2);
    assert_eq!(measured.remaining, Some(0.6));
    view.windows.drain(..2);
    let missing = input(&view, START).unwrap();
    assert!(missing.windows.is_empty());
    assert_eq!(missing.remaining, None);
    assert_eq!(missing.unavailable, Some(Unavailable::MissingWindow));
}

#[test]
fn n7_preflight_bound_retries_but_late_reconciliation_bound_stops() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    let oversized = vec![account(1, START, Some(20.), Some(40.)); ACCOUNTS + 1];
    let error = log.sample(at(START), &oversized).unwrap_err();
    assert_eq!(
        diagnostic(&error, FailurePhase::Sample).reason,
        FailureReason::Bounds
    );
    assert!(!log.status().unwrap().stopped);
    assert!(log.users.lock().unwrap().accounts.is_empty());
    assert!(records(&log).is_empty());
    // Same reason code, but reconciliation has begun: this is never retried.
    log.users
        .lock()
        .unwrap()
        .revisions
        .insert("codex".into(), u64::MAX);
    let error = log.sample(at(START + 60), &pair(START + 60)).unwrap_err();
    assert_eq!(
        diagnostic(&error, FailurePhase::Sample).reason,
        FailureReason::Bounds
    );
    assert!(log.status().unwrap().stopped);
    assert_eq!(log.status().unwrap().unavailable, 1);
    assert!(records(&log).is_empty());
    log.close().unwrap();
}

#[test]
fn n7_coded_empty_window_name_recovery_preserves_prior_archive_lineage() {
    for clean in [false, true] {
        for prior in [false, true] {
            let home = private_fixture();
            let root = home.path().join("state/forecast-log");
            let log = ForecastLog::open(home.path()).unwrap();
            log.close().unwrap();
            drop(log);
            let path = root.join("checkpoint.json");
            let mut checkpoint: Checkpoint =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let first_archive = if prior {
                checkpoint.status.stopped = true;
                checkpoint.status.losses = 1;
                crate::platform::atomic_write_observation(
                    &path,
                    &serde_json::to_vec(&checkpoint).unwrap(),
                )
                .unwrap();
                let first = ForecastLog::open(home.path()).unwrap();
                first.close().unwrap();
                drop(first);
                checkpoint = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                Some(
                    home.path()
                        .join("state")
                        .join(&checkpoint.recovery.as_ref().unwrap().archive),
                )
            } else {
                None
            };
            let first_bytes = first_archive
                .as_ref()
                .map(|p| fs::read(p.join("checkpoint.json")).unwrap());
            if prior {
                // An uncoded failure with existing lineage remains stopped.
                let mut uncoded = checkpoint.clone();
                uncoded.status.stopped = true;
                uncoded.status.losses = 1;
                crate::platform::atomic_write_observation(
                    &path,
                    &serde_json::to_vec(&uncoded).unwrap(),
                )
                .unwrap();
                let parked = ForecastLog::open(home.path()).unwrap();
                assert!(parked.status().unwrap().stopped);
                assert_eq!(
                    parked
                        .worker
                        .lock()
                        .unwrap()
                        .checkpoint
                        .recovery
                        .as_ref()
                        .unwrap()
                        .archive,
                    checkpoint.recovery.as_ref().unwrap().archive
                );
                parked.close().unwrap();
                drop(parked);
            }
            checkpoint.clean = clean;
            checkpoint.status.stopped = true;
            checkpoint.status.losses = 1;
            checkpoint.last_failure = Some(FailureDiagnostic {
                phase: FailurePhase::Sample,
                reason: FailureReason::WindowName,
            });
            let original = serde_json::to_vec(&checkpoint).unwrap();
            crate::platform::atomic_write_observation(&path, &original).unwrap();
            let resumed = ForecastLog::open(home.path()).unwrap();
            let receipt = resumed
                .worker
                .lock()
                .unwrap()
                .checkpoint
                .recovery
                .clone()
                .unwrap();
            let archive = home.path().join("state").join(&receipt.archive);
            assert_eq!(fs::read(archive.join("checkpoint.json")).unwrap(), original);
            if let Some(first_archive) = first_archive {
                assert_ne!(archive, first_archive);
                assert_eq!(
                    fs::read(first_archive.join("checkpoint.json")).unwrap(),
                    first_bytes.unwrap()
                );
            }
            assert!(!resumed.status().unwrap().stopped);
            assert_eq!(receipt.prior_losses, 1);
            resumed.sample(at(START), &pair(START)).unwrap();
            assert_eq!(records(&resumed).len(), 2);
            resumed.close().unwrap();
        }
    }
}

#[test]
fn n7_not_used_claude_accounts_never_enter_the_paid_cohort_even_with_stale_eligibility() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    let mut views = Vec::new();
    for (id, used) in [
        (1, 20.),
        (2, 40.),
        (3, 99.),
        (4, 99.),
        (5, 99.),
        (6, 99.),
        (7, 99.),
        (8, 99.),
    ] {
        let mut view = account(id, START, Some(used), Some(used));
        view.provider = AdminProvider::Claude;
        view.id = format!("cl-{id:06x}");
        view.plan = Some(if id == 3 || id == 4 { "free" } else { "pro" }.into());
        if id == 3 {
            view.id = "cl-269a6a".into();
        }
        if id == 4 {
            view.id = "cl-b8da25".into();
        }
        if id == 5 {
            view.user_state = AdminUserState::Paused;
        }
        if id == 6 {
            view.user_state = AdminUserState::Disabled;
        }
        if id == 7 {
            view.health = AdminHealth::NeedsRelogin {
                reason: "auth".into(),
                since: stamp(START),
            };
        }
        if id == 8 {
            view.health = AdminHealth::PlanWithoutClaudeCode {};
        }
        view.pool_eligible = true; // Deliberately stale producer bit for every exclusion.
        views.push(view);
    }
    log.sample(at(START), &views).unwrap();
    let origin = records(&log)
        .into_iter()
        .find_map(|record| match record.event {
            Event::Origin { origin } if origin.provider == "claude" => Some(origin),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        origin
            .cohort
            .iter()
            .map(|a| a.id.as_str())
            .collect::<Vec<_>>(),
        vec!["cl-000001", "cl-000002"]
    );
    assert!(origin.remaining.is_some_and(|r| (r - 1.4).abs() < 1e-12));
    log.close().unwrap();
}

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
    log.account_state("cx-000001", "codex", AdminUserState::Paused, false)
        .unwrap();
    log.account_state("cx-000001", "codex", AdminUserState::Active, false)
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
fn n7_malformed_identity_and_storage_cap_stop_with_durable_explicit_loss() {
    let home = private_fixture();
    let log = ForecastLog::open(home.path()).unwrap();
    let mut bad = pair(START);
    bad[0].id = "personal-identity@example.invalid".into();
    assert!(log.sample(at(START), &bad).is_err());
    assert!(log.status().unwrap().stopped);
    assert_eq!(log.status().unwrap().losses, 1);
    drop(log);
    let log = ForecastLog::open(home.path()).unwrap();
    assert!(!log.due(at(START)));
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
        log.account_state("cx-000001", "codex", AdminUserState::Paused, false)
            .unwrap();
        log.account_state("cx-000001", "codex", AdminUserState::Active, false)
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
    log.account_state("cx-000001", "codex", AdminUserState::Paused, false)
        .unwrap();
    log.account_state("cx-000001", "codex", AdminUserState::Active, false)
        .unwrap();
    log.close().unwrap();
    let path = home.path().join("state/forecast-log/checkpoint.json");
    let before = fs::read(&path).unwrap();
    let checkpoint: Checkpoint = serde_json::from_slice(&before).unwrap();
    assert!(checkpoint.clean);
    assert!(!log.due(at(START + 3600)));
    assert!(log.sample(at(START + 3600), &pair(START + 3600)).is_err());
    assert!(
        log.account_state("cx-000001", "codex", AdminUserState::Paused, false)
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
    log.account_state("cx-000001", "codex", AdminUserState::Paused, false)
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

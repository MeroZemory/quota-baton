use super::*;
use crate::accounts::Provider;
use crate::observability::AttemptRecord;
use serde_json::json;

fn epoch() -> DateTime<Utc> {
    "2026-10-30T01:00:00Z".parse().unwrap()
}

#[test]
fn plan_fence_rejects_inflight_old_reports_after_a_round_trip() {
    let mut a = account("plan-race", "pro");
    let mut m = Measurements::default();
    train(&mut m, &a, 3, 100);
    let old = a.clone();
    a.generation += 1;
    a.plan = Some("free".into());
    m.invalidate(&a, epoch() + Duration::seconds(190));
    a.generation += 1;
    a.plan = Some("pro".into());
    m.invalidate(&a, epoch() + Duration::seconds(200));
    // An old returned Account cannot seed after both native plan callbacks.
    m.observe(
        &old,
        &poll(epoch() + Duration::seconds(210), 17.0),
        false,
        epoch() + Duration::seconds(220),
    );
    assert!(m.facts(&a.id).is_none());
    // Nor may an old usage response acquire the new plan by being applied late.
    m.observe(
        &a,
        &poll(epoch() + Duration::seconds(195), 17.0),
        false,
        epoch() + Duration::seconds(220),
    );
    assert!(m.accounts[&a.id].baselines.is_empty());
    m.observe(
        &a,
        &poll(epoch() + Duration::seconds(230), 18.0),
        false,
        epoch() + Duration::seconds(230),
    );
    assert_eq!(m.accounts[&a.id].baselines.len(), 2);
    assert!(m.facts(&a.id).unwrap().windows.is_empty());
}

#[test]
fn slow_windows_need_continuous_polls_not_a_guessed_short_interval() {
    let a = account("slow", "pro");
    let mut m = Measurements::default();
    m.observe(&a, &poll(epoch(), 10.0), false, epoch());
    for i in 1..=12 {
        m.record_request(&request(
            &a.id,
            epoch() + Duration::seconds(i * 60 - 30),
            Some(100),
        ));
        let at = epoch() + Duration::seconds(i * 60);
        let mut snapshot = poll(at, 10.0 + i as f64 / 10.0);
        // A zero-duration slot is not a native gating window.
        snapshot["windows"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"0m","used_pct":null}));
        m.observe(&a, &snapshot, false, at);
    }
    let facts = m.facts(&a.id).unwrap();
    assert!(facts.valid());
    let first = &facts.windows["5h"].samples[0];
    assert_eq!(first.input_tokens, 1000);
    assert_eq!(first.end_at - first.start_at, Duration::minutes(10));
    assert_eq!(sample_count(&m, &a.id), 1);
    // A real polling gap breaks continuity instead of accumulating over it.
    m.record_request(&request(&a.id, epoch() + Duration::minutes(14), Some(100)));
    let at = epoch() + Duration::minutes(20);
    m.observe(&a, &poll(at, 15.0), false, at);
    assert_eq!(sample_count(&m, &a.id), 1);
}

#[test]
fn delayed_first_poll_cannot_seed_inside_a_completed_or_unknown_request() {
    for tokens in [Some(100), None] {
        let a = account("delayed", "pro");
        let mut m = Measurements::default();
        m.record_request(&request(&a.id, epoch() + Duration::seconds(9), tokens));
        // The response arrives at T12 but reports dispatch at T9, before the
        // observed completion at T10. No baseline existed during that request.
        m.observe(
            &a,
            &poll(epoch() + Duration::seconds(9), 10.0),
            false,
            epoch() + Duration::seconds(12),
        );
        assert!(m.accounts[&a.id].baselines.is_empty());
        m.record_request(&request(&a.id, epoch() + Duration::seconds(20), Some(100)));
        m.observe(
            &a,
            &poll(epoch() + Duration::seconds(60), 12.0),
            false,
            epoch() + Duration::seconds(60),
        );
        assert_eq!(sample_count(&m, &a.id), 0);
        m.record_request(&request(&a.id, epoch() + Duration::seconds(90), Some(100)));
        m.observe(
            &a,
            &poll(epoch() + Duration::seconds(120), 14.0),
            false,
            epoch() + Duration::seconds(120),
        );
        assert_eq!(sample_count(&m, &a.id), 1);
    }
}

#[test]
fn native_plan_change_notification_cannot_revive_pre_transition_measurements() {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let base = executable
        .parent()
        .unwrap()
        .join(".sandbox/r21-plan-change");
    std::fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let home = root.path().join("home");
    crate::platform::create_private_directory_new(&home).unwrap();
    let obs = crate::observability::Observability::new(&home, None).unwrap();
    let mut a = account("plan-notification", "pro");
    obs.observe_placement_capacity(&a, &poll(epoch(), 10.0), false, epoch());
    for i in 1..=3 {
        obs.record_request(request(
            &a.id,
            epoch() + Duration::seconds(i * 60 - 30),
            Some(100),
        ));
        let at = epoch() + Duration::seconds(i * 60);
        obs.observe_placement_capacity(&a, &poll(at, 10.0 + i as f64 * 2.0), false, at);
    }
    assert_eq!(
        obs.placement_capacity_facts(&a.id).unwrap().windows["5h"]
            .samples
            .len(),
        3
    );
    a.plan = Some("free".into());
    obs.record_plan_change(&a, Some("pro"));
    assert!(obs.placement_capacity_facts(&a.id).is_none());
    a.plan = Some("pro".into());
    obs.record_plan_change(&a, Some("free"));
    assert!(obs.placement_capacity_facts(&a.id).is_none());
    drop(obs);
}

fn account(id: &str, plan: &str) -> Account {
    let mut account = crate::accounts::health::tests::fixture();
    account.id = id.into();
    account.plan = Some(plan.into());
    account
}

fn poll(at: DateTime<Utc>, used: f64) -> Value {
    json!({"source":"poll", "windows":[
        {"name":"5h", "used_pct":used, "observed_at":at, "reset_at":epoch()+Duration::days(2)},
        {"name":"7d", "used_pct":used, "observed_at":at, "reset_at":epoch()+Duration::days(7)}
    ]})
}

fn request(id: &str, start: DateTime<Utc>, tokens: Option<u64>) -> RequestRecord {
    serde_json::from_value(json!({
        "ts":start, "req":"synthetic", "family":"codex", "route":"responses",
        "transport":"http", "account":id, "session":null, "model":null,
        "originator":null, "ua":null, "status":200, "upstream_status":200,
        "ttfb_ms":1, "dur_ms":1000, "bytes_in":999999, "bytes_out":999999,
        "attempts":1, "outcome":"ok", "cache_usage":{"input_tokens":tokens}
    }))
    .unwrap()
}

fn projected(used: f64) -> BTreeMap<String, f64> {
    BTreeMap::from([("5h".into(), used), ("7d".into(), used)])
}

fn train(m: &mut Measurements, a: &Account, count: usize, tokens: u64) -> Value {
    m.observe(a, &poll(epoch(), 10.0), false, epoch());
    let mut last = poll(epoch(), 10.0);
    for i in 1..=count {
        m.record_request(&request(
            &a.id,
            epoch() + Duration::seconds(i as i64 * 60 - 30),
            Some(tokens),
        ));
        let at = epoch() + Duration::seconds(i as i64 * 60);
        last = poll(at, 10.0 + i as f64 * 2.0);
        m.observe(a, &last, false, at);
    }
    last
}

fn sample_count(m: &Measurements, id: &str) -> usize {
    m.facts(id)
        .unwrap()
        .windows
        .get("5h")
        .map_or(0, |w| w.samples.len())
}

#[test]
fn cold_and_below_three_are_unweighted_then_measured_capacity_is_used() {
    let a = account("a", "free");
    let mut m = Measurements::default();
    assert!(m.facts("a").is_none());
    let q = train(&mut m, &a, 2, 100);
    assert!(
        remaining_tokens(
            &m.facts("a").unwrap(),
            &a,
            &q,
            &projected(50.0),
            epoch() + Duration::minutes(2)
        )
        .is_none()
    );
    m.record_request(&request("a", epoch() + Duration::seconds(150), Some(100)));
    let at = epoch() + Duration::minutes(3);
    let q = poll(at, 16.0);
    m.observe(&a, &q, false, at);
    assert_eq!(
        remaining_tokens(&m.facts("a").unwrap(), &a, &q, &projected(50.0), at),
        Some(2500.0)
    );
}

#[test]
fn free_and_pro_equal_percent_use_only_measured_tokens_and_only_room_is_positive() {
    let mut m = Measurements::default();
    let free = account("free", "free");
    let pro = account("pro", "pro");
    let fq = train(&mut m, &free, 3, 100);
    let pq = train(&mut m, &pro, 3, 1000);
    let at = epoch() + Duration::minutes(3);
    let ff = m.facts("free").unwrap();
    let pf = m.facts("pro").unwrap();
    let f = remaining_tokens(&ff, &free, &fq, &projected(50.0), at).unwrap();
    let p = remaining_tokens(&pf, &pro, &pq, &projected(50.0), at).unwrap();
    assert_eq!(p, f * 10.0);
    assert_eq!(
        remaining_tokens(&pf, &pro, &pq, &projected(100.0), at),
        Some(0.0)
    );
    assert!(f > 0.0); // The existing selector can still choose the only account with room.
    let mut renamed = ff.clone();
    renamed.plan = "pro".into();
    assert_eq!(
        remaining_tokens(&renamed, &pro, &fq, &projected(50.0), at),
        Some(f)
    );
}

#[test]
fn every_projected_window_needs_three_samples_and_non_gating_windows_are_ignored() {
    let mut m = Measurements::default();
    let a = account("a", "prolite");
    let mut q = train(&mut m, &a, 3, 100);
    let at = epoch() + Duration::minutes(3);
    q["windows"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"43200m", "used_pct":null}));
    let mut facts = m.facts("a").unwrap();
    assert!(remaining_tokens(&facts, &a, &q, &projected(50.0), at).is_some());
    facts.windows.get_mut("7d").unwrap().samples.pop();
    assert!(remaining_tokens(&facts, &a, &q, &projected(50.0), at).is_none());
    assert!(remaining_tokens(&facts, &a, &q, &BTreeMap::from([("5h".into(), 50.0)]), at).is_some());
    assert!(remaining_tokens(&facts, &a, &q, &BTreeMap::new(), at).is_none());
}

#[test]
fn minimum_window_capacity_and_median_resist_an_outlier() {
    let mut m = Measurements::default();
    let a = account("a", "promax");
    let q = train(&mut m, &a, 3, 100);
    let mut facts = m.facts("a").unwrap();
    facts.windows.get_mut("5h").unwrap().samples[0].input_tokens = 1000000;
    let p = BTreeMap::from([("5h".into(), 50.0), ("7d".into(), 90.0)]);
    assert_eq!(
        remaining_tokens(&facts, &a, &q, &p, epoch() + Duration::minutes(3)),
        Some(500.0)
    );
}

#[test]
fn reset_retains_samples_but_plan_changes_clear_them_even_if_plan_changes_back() {
    let mut m = Measurements::default();
    let mut a = account("a", "free");
    train(&mut m, &a, 3, 100);
    m.record_request(&request("a", epoch() + Duration::seconds(210), Some(99999)));
    let at = epoch() + Duration::minutes(4);
    let mut reset = poll(at, 1.0);
    for w in reset["windows"].as_array_mut().unwrap() {
        w["reset_at"] = json!(epoch() + Duration::days(10));
    }
    m.observe(&a, &reset, false, at);
    assert_eq!(sample_count(&m, "a"), 3);
    a.plan = Some("pro".into());
    m.observe(&a, &reset, false, at);
    assert_eq!(sample_count(&m, "a"), 0);
    a.plan = Some("free".into());
    m.observe(&a, &reset, false, at);
    assert_eq!(sample_count(&m, "a"), 0);
}

#[test]
fn blocked_missing_usage_and_unknown_traffic_discard_intervals() {
    for case in ["outside", "missing", "unknown", "ambiguous", "details"] {
        let mut m = Measurements::default();
        let a = account("a", "free");
        let b = account("b", "pro");
        m.observe(&a, &poll(epoch(), 10.0), false, epoch());
        m.observe(&b, &poll(epoch(), 10.0), false, epoch());
        let mut r = request("a", epoch() + Duration::seconds(10), Some(100));
        match case {
            "missing" => r.cache_usage = None,
            "unknown" => r.account = None,
            "ambiguous" => {
                r.attempts = 2;
                r.attempt_details = vec![];
            }
            "details" => {
                r.attempts = 2;
                r.attempt_details = vec![
                    AttemptRecord {
                        account: "a".into(),
                        failover_reason: None,
                    },
                    AttemptRecord {
                        account: "b".into(),
                        failover_reason: None,
                    },
                ];
            }
            _ => (),
        }
        m.record_request(&r);
        let at = epoch() + Duration::minutes(1);
        m.observe(&a, &poll(at, 12.0), case == "outside", at);
        assert_eq!(sample_count(&m, "a"), 0, "{case}");
        if matches!(case, "unknown" | "ambiguous" | "details") {
            assert!(m.accounts["b"].baselines.is_empty(), "{case}");
        }
        if matches!(case, "unknown" | "ambiguous") {
            assert!(m.unknown_traffic > 0);
        }
    }
}

#[test]
fn zero_attempts_do_not_add_traffic_or_invalidate_clean_measurement() {
    let mut m = Measurements::default();
    let a = account("a", "free");
    m.observe(&a, &poll(epoch(), 10.0), false, epoch());
    let mut local = request("a", epoch(), None);
    local.attempts = 0;
    m.record_request(&local);
    m.record_request(&request("a", epoch() + Duration::seconds(10), Some(100)));
    let at = epoch() + Duration::minutes(1);
    m.observe(&a, &poll(at, 12.0), false, at);
    assert_eq!(sample_count(&m, "a"), 1);
}

#[test]
fn request_boundaries_and_bad_dates_cannot_seed_or_admit_samples() {
    for (start, duration) in [(0, 1000), (59, 1000), (60, 1000)] {
        let mut m = Measurements::default();
        let a = account("a", "free");
        m.observe(&a, &poll(epoch(), 10.0), false, epoch());
        let mut r = request("a", epoch() + Duration::seconds(start), Some(100));
        r.dur_ms = duration;
        m.record_request(&r);
        let at = epoch() + Duration::minutes(1);
        m.observe(&a, &poll(at, 12.0), false, at);
        assert_eq!(sample_count(&m, "a"), 0);
    }
    for bad in [
        json!("invalid"),
        json!(epoch() + Duration::hours(1)),
        Value::Null,
    ] {
        let mut m = Measurements::default();
        let a = account("a", "free");
        let mut q = poll(epoch(), 10.0);
        q["windows"][0]["observed_at"] = bad;
        m.observe(&a, &q, false, epoch());
        assert!(m.accounts["a"].baselines.is_empty());
    }
}

#[test]
fn sub_percentage_steps_accumulate_but_no_tokens_never_means_zero_tokens() {
    let mut m = Measurements::default();
    let a = account("a", "free");
    m.observe(&a, &poll(epoch(), 10.0), false, epoch());
    for i in 1..=2 {
        m.record_request(&request(
            "a",
            epoch() + Duration::seconds(i * 60 - 30),
            Some(50),
        ));
        let at = epoch() + Duration::seconds(i * 60);
        m.observe(&a, &poll(at, 10.0 + i as f64 * 0.5), false, at);
    }
    let facts = m.facts("a").unwrap();
    let sample = &facts.windows["5h"].samples[0];
    assert_eq!(sample.input_tokens, 100);
    assert_eq!(sample.used_pct_delta, 1.0);
    assert_eq!(sample.start_at, epoch());
    assert_eq!(sample.end_at, epoch() + Duration::minutes(2));
}

#[test]
fn credits_staleness_and_invalid_wire_facts_refuse_weighting() {
    let mut m = Measurements::default();
    let a = account("a", "pro");
    let q = train(&mut m, &a, 3, 100);
    let at = epoch() + Duration::minutes(3);
    let facts = m.facts("a").unwrap();
    assert!(facts.valid());
    let mut credit = q.clone();
    credit["signals"] = json!({"serving_from_credits":true});
    // The native poll signal fences new measurements. Decision interpretation
    // uses the actual paid-serving witness that QuotaFacts records, rather
    // than reading unrecorded signal/profile fields inside the kernel.
    m.observe(&a, &credit, false, at);
    assert!(m.accounts["a"].baselines.is_empty());
    credit["paid_serving_observed_at"] = json!(at);
    assert!(remaining_tokens(&facts, &a, &credit, &projected(50.0), at).is_none());
    assert!(
        remaining_tokens(
            &facts,
            &a,
            &q,
            &projected(50.0),
            at + Duration::seconds(301)
        )
        .is_none()
    );
    let mut invalid = facts.clone();
    invalid.plan = "arbitrary-body-text".into();
    assert!(!invalid.valid());
    invalid = facts.clone();
    invalid.windows.get_mut("5h").unwrap().samples[0].used_pct_delta = f64::NAN;
    assert!(!invalid.valid());
    invalid = facts.clone();
    invalid.windows.get_mut("5h").unwrap().samples[0].input_tokens = 0;
    assert!(!invalid.valid());
    invalid = facts.clone();
    invalid.windows.get_mut("5h").unwrap().samples[0].reset_at = epoch();
    assert!(!invalid.valid());
    let mut wire = serde_json::to_value(&facts).unwrap();
    wire["identity"] = json!("forbidden");
    assert!(serde_json::from_value::<CapacityFacts>(wire).is_err());
    let mut other_provider = a.clone();
    other_provider.provider = Provider::Claude;
    assert!(remaining_tokens(&facts, &other_provider, &q, &projected(50.0), at).is_none());
}

#[test]
fn sample_window_and_account_storage_are_bounded() {
    let mut m = Measurements::default();
    let a = account("a", "pro");
    train(&mut m, &a, 40, 100);
    assert_eq!(sample_count(&m, "a"), MAX_SAMPLES);
    assert!(m.facts("a").unwrap().valid());
    for i in 0..MAX_ACCOUNTS + 2 {
        let a = account(&format!("bounded-{i}"), "pro");
        m.observe(&a, &poll(epoch(), 10.0), false, epoch());
    }
    assert_eq!(m.accounts.len(), MAX_ACCOUNTS);
    let mut huge = poll(epoch(), 10.0);
    huge["windows"] = json!((1..=MAX_WINDOWS+1).map(|i|json!({
        "name":format!("{i}m"),"observed_at":epoch(),"reset_at":epoch()+Duration::days(1),"used_pct":10.0
    })).collect::<Vec<_>>());
    m.observe(&a, &huge, false, epoch());
    assert!(m.accounts["a"].baselines.is_empty());
}

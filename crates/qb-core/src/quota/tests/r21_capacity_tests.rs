use super::*;

#[tokio::test]
async fn r21_native_claude_polls_measure_three_own_intervals_and_publish_capacity() {
    let f = Fixture::with_provider_and_interval(1, Provider::Claude, 1).await;
    let poller = &f.relay.quota;
    let runtime = &f.relay.accounts;
    let id = &f.ids[0];
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    let initial = poller.refresh_permission_plan(id).await.unwrap();
    assert_eq!(initial.plan.as_deref(), Some("max_20x"));
    assert!(initial.eligible(None));
    let resets = [
        Utc::now() + chrono::Duration::hours(5),
        Utc::now() + chrono::Duration::days(7),
    ];
    let mut dispatch_bounds = Vec::new();

    for round in 0..=3 {
        if round > 0 {
            // Completed, manually supplied numeric ledger data, not HTTP
            // serving evidence. Attribute the send through the native owner.
            runtime.note_own_usage_send(id);
            let record: crate::observability::RequestRecord = serde_json::from_value(json!({
                "ts": Utc::now(), "req": format!("synthetic-r21-{round}"),
                "family": "claude", "route": "/v1/messages", "transport": "http",
                "account": id, "session": null, "model": null,
                "originator": null, "ua": null, "status": 200, "upstream_status": 200,
                "ttfb_ms": 0, "dur_ms": 0, "bytes_in": 0, "bytes_out": 0,
                "attempts": 1, "outcome": "ok", "cache_usage": {"input_tokens": 10_000}
            }))
            .unwrap();
            poller.obs.record_request(record);
        }
        // Exercise the real minimum interval, without changing private clocks.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let used = 25 + round;
        f.fake.state.push(ScriptedResponse::json(
            reqwest::StatusCode::OK,
            json!({
                "five_hour": {"utilization": used, "resets_at": resets[0]},
                "seven_day": {"utilization": used, "resets_at": resets[1]},
                "seven_day_opus": null, "seven_day_sonnet": null,
                "seven_day_oauth_apps": null, "seven_day_cowork": null,
                "extra_usage": {"is_enabled": false}
            })
            .to_string(),
        ));
        // UsagePoller forces a real profile refresh after every usage attempt.
        f.fake.state.push(profile_reply("default_claude_max_20x"));
        let before = Utc::now();
        let account = poller.poll(id).await.unwrap();
        let after = Utc::now();
        dispatch_bounds.push((before, after));
        assert_eq!(account.plan.as_deref(), Some("max_20x"));
        assert_eq!(account.health, initial.health);
        assert_eq!(account.quota_cooling, initial.quota_cooling);
        assert_eq!(
            account.quota_credit_releasable,
            initial.quota_credit_releasable
        );
        assert!(account.eligible(None));
        assert!(!runtime.outside_used(id));
        assert!(!runtime.outside_protected(id));

        let snapshot = poller.obs.quota_snapshot(id).unwrap();
        assert_eq!(snapshot["source"], "poll");
        let windows = snapshot["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 2);
        let facts = poller.obs.placement_capacity_facts(id).unwrap();
        assert_eq!(facts.provider, Provider::Claude);
        assert_eq!(facts.plan, "max_20x");
        assert_eq!(facts.windows.len(), if round == 0 { 0 } else { 2 });
        for (name, reset) in ["5h", "7d"].into_iter().zip(resets) {
            let window = windows.iter().find(|w| w["name"] == name).unwrap();
            assert_eq!(window["used_pct"].as_f64(), Some(used as f64));
            assert_eq!(window["reset_at"], json!(reset));
            if round > 0 {
                let samples = &facts.windows[name].samples;
                assert_eq!(samples.len(), round as usize);
                for (index, sample) in samples.iter().enumerate() {
                    assert_eq!(sample.input_tokens, 10_000);
                    assert_eq!(sample.used_pct_delta, 1.0);
                    assert_eq!(sample.reset_at, reset);
                    assert!(sample.start_at >= dispatch_bounds[index].0);
                    assert!(sample.start_at <= dispatch_bounds[index].1);
                    assert!(sample.end_at >= dispatch_bounds[index + 1].0);
                    assert!(sample.end_at <= dispatch_bounds[index + 1].1);
                    assert!(sample.start_at < sample.end_at);
                }
            }
        }
        let views = runtime.typed_views().await.unwrap();
        assert_eq!(views.len(), 1);
        let view = &views[0];
        assert_eq!(&view.id, id);
        assert_eq!(
            view.placement_capacity_tokens,
            if round < 3 { None } else { Some(720_000) }
        );
        assert!(view.pool_eligible);
        assert!(!view.probe_pending);
        assert!(view.quota_notice.is_none());
        assert!(view.quota_cooling.is_none());
        assert!(!view.credits_in_use);
        assert!(!view.outside_used);
    }

    let requests = f.fake.state.requests();
    assert_eq!(
        requests.len(),
        9,
        "one initial profile plus four usage/profile pairs"
    );
    assert_eq!(requests[0].path_and_query, "/api/oauth/profile");
    for pair in requests[1..].chunks_exact(2) {
        assert_eq!(pair[0].method, "GET");
        assert_eq!(pair[0].path_and_query, "/api/oauth/usage");
        assert_eq!(pair[1].method, "GET");
        assert_eq!(pair[1].path_and_query, "/api/oauth/profile");
    }
}

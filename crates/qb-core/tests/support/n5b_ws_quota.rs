//! Future child of m4a_ws; requires the staged WS receipt engine and pump adapter.
//! All accounts/tokens and listeners are synthetic. No direct proof or Cooling mint.
use super::*;
use qb_core::{
    relay::WsQuotaWaitEvent,
    routing::{Mode, Settings},
};
use qb_testkit::websocket::ConnectionEvent;
use std::sync::atomic::Ordering;
use tokio::{
    sync::{broadcast, oneshot},
    time::{Instant, sleep_until, timeout, timeout_at},
};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

const KEY: &str = "codex:synthetic-ws-session";
const PHASE: Duration = Duration::from_secs(2);
const CLEANUP: Duration = Duration::from_secs(3);
const REQUEST_SECS: u64 = 10;
const RESET_SECS: i64 = 6;

type DispatchGate = (oneshot::Receiver<()>, oneshot::Sender<()>);

struct Held {
    h: Harness,
    _usage: ScriptedUpstream,
    client: Client,
    source: String,
    model: &'static str,
    raw: String,
    reset: chrono::DateTime<Utc>,
    reset_not_before: Instant,
    late_horizon: Instant,
    events: broadcast::Receiver<WsQuotaWaitEvent>,
    admitted: WsQuotaWaitEvent,
    before_write: Option<DispatchGate>,
}

fn application_messages(h: &Harness) -> Vec<Message> {
    h.fake
        .state
        .messages()
        .into_iter()
        .filter(|message| matches!(message, Message::Text(_) | Message::Binary(_)))
        .collect()
}

fn assert_unsent(h: &Harness) {
    assert!(
        application_messages(h).is_empty(),
        "held unsent WS create must not reach any upstream"
    );
}

async fn policy(h: &Harness, mode: Mode, wait: u64) {
    let bindings = h.state.bindings.clone();
    timeout(
        PHASE,
        tokio::task::spawn_blocking(move || {
            bindings.set_routing_session(
                &qb_core::routing::session_id(KEY),
                Some(Settings {
                    mode,
                    reset_wait_secs: wait,
                }),
                None,
                false,
            )
        }),
    )
    .await
    .expect("native session policy publication must complete")
    .unwrap()
    .unwrap();
    assert_eq!(
        h.state
            .bindings
            .policies
            .plan(Some(KEY))
            .settings
            .reset_wait_secs,
        wait
    );
}

async fn stage(
    events: &mut broadcast::Receiver<WsQuotaWaitEvent>,
    expected: &str,
    source: &str,
) -> WsQuotaWaitEvent {
    let event = timeout(PHASE, events.recv())
        .await
        .expect("unsent WS create must enter genuine quota wait / reset reselect")
        .expect("WS wait observer closed or lagged");
    assert_eq!(event.stage, expected, "wrong native WS quota wait stage");
    assert_eq!(event.source, source, "wrong native WS quota wait source");
    event
}

fn quiet(events: &mut broadcast::Receiver<WsQuotaWaitEvent>) {
    assert!(
        matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "unexpected repeated admission/reselect or closed WS wait observer"
    );
}

async fn held(model: &'static str, mode: Mode, prewrite: bool) -> Held {
    held_with_poll_interval(model, mode, prewrite, None).await
}

async fn held_with_poll_interval(
    model: &'static str,
    mode: Mode,
    prewrite: bool,
    poll_min_interval: Option<u64>,
) -> Held {
    let usage = timeout(PHASE, ScriptedUpstream::start())
        .await
        .unwrap()
        .unwrap();
    let h = Harness::new_configured(
        1,
        false,
        false,
        None,
        Some((REQUEST_SECS, 5)),
        None,
        |config| {
            config.upstream.codex_usage_base = usage.url.clone();
            config.routing.reset_hold_secs = 60;
            if let Some(interval) = poll_min_interval {
                config.quota.poll_min_interval_secs = interval;
                config.quota.validate().unwrap();
            }
            config.routing.validate().unwrap();
        },
    )
    .await;
    assert!(h.servers.data_addr.ip().is_loopback());
    let (mut client, source) = timeout(PHASE, h.connect()).await.unwrap();
    assert!(matches!(
        h.store.load(&source).unwrap().health,
        Health::Ready
    ));
    assert_eq!(h.state.ws_reserved_for_test(&source), 1);
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "real healthy 101 required"
    );
    assert!(usage.state.requests().is_empty());
    assert_unsent(&h);
    policy(&h, mode, 60).await;
    assert_eq!(
        h.state.bindings.current(Some(KEY)).as_deref(),
        Some(source.as_str())
    );
    let raw = format!("  {{\"type\":\"response.create\",\"model\":\"{model}\",\"input\":[]}} \n");
    let (retained, release) = h.state.pause_ws_ordinary_after_pacing(KEY, raw.as_bytes());
    let before_write =
        prewrite.then(|| h.state.pause_ws_ordinary_before_write(KEY, raw.as_bytes()));
    let mut events = h.state.subscribe_ws_quota_wait();
    let sent_at = Instant::now();
    timeout(PHASE, client.send(Message::Text(raw.clone().into())))
        .await
        .unwrap()
        .unwrap();
    timeout(PHASE, retained)
        .await
        .expect("healthy create must be retained before the actual quota poll")
        .unwrap();
    let retained_at = Instant::now();
    assert_unsent(&h);

    // Match the actual N5a wham producer. Poll remains account-scoped model=None,
    // while the retained native create has a real Some(model) request identity.
    let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + RESET_SECS, 0).unwrap();
    usage.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({"rate_limit": {
            "primary_window":{"used_percent":100, "limit_window_seconds":18000,
                "reset_at":reset.timestamp()},
            "secondary_window":{"used_percent":10, "limit_window_seconds":604800,
                "reset_at":(reset + ChronoDuration::hours(47)).timestamp()},
            "active_limit":"5h"
        }})
        .to_string(),
    ));
    let account = timeout(PHASE, h.state.quota.poll(&source))
        .await
        .expect("real full poll must finish while the unsent-frame gate is held")
        .expect("native poll/reducer must accept the complete wham report");
    assert!(matches!(account.health, Health::Cooling {
        reason: HealthReason::Quota, until, ref scope,
    } if until == reset && scope == "account"));
    let cooling = account.quota_cooling.unwrap();
    assert_eq!(cooling.window, "5h");
    assert_eq!(cooling.reset_at, reset);
    let polls = usage.state.requests();
    assert_eq!(polls.len(), 1);
    assert_eq!(polls[0].method, axum::http::Method::GET);
    assert_eq!(polls[0].path_and_query, "/backend-api/wham/usage");
    assert!(polls[0].body.is_empty());
    assert!(polls[0].headers.get("authorization").is_some());
    assert!(
        polls[0].headers.get("authorization")
            == h.fake.state.requests()[0].headers.get("authorization"),
        "poll and healthy upgrade must use the same synthetic source credential"
    );
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "poll is not a WS/data attempt"
    );
    assert_unsent(&h);
    // Sample monotonic before wall time: this lower bound cannot move R later.
    let sampled = Instant::now();
    let remaining = (reset - Utc::now()).to_std().unwrap();
    let reset_not_before = sampled + remaining;
    let late_horizon = reset_not_before + Duration::from_millis(300);
    release
        .send(())
        .expect("healthy retained-frame setup gate disappeared");
    let admitted = stage(&mut events, "admitted", &source).await;
    assert!(
        reset - Utc::now() > ChronoDuration::seconds(3),
        "close/revocation cleanup must be bounded strictly before reset"
    );
    assert!(admitted.deadline >= sent_at + Duration::from_secs(REQUEST_SECS));
    assert!(
        admitted.deadline <= retained_at + Duration::from_secs(REQUEST_SECS),
        "WS deadline must originate with the retained turn, not after poll/setup"
    );
    assert!(reset_not_before < admitted.deadline);
    assert_unsent(&h);
    quiet(&mut events);
    Held {
        h,
        _usage: usage,
        client,
        source,
        model,
        raw,
        reset,
        reset_not_before,
        late_horizon,
        events,
        admitted,
        before_write,
    }
}

async fn close_frame(client: &mut Client, code: u16) {
    let message = timeout(CLEANUP, client.next())
        .await
        .expect("held WS must deliver its native close promptly")
        .expect("native close frame missing")
        .expect("WS close failed at transport level");
    assert_close(message, code);
    match timeout(PHASE, client.flush()).await.unwrap() {
        Ok(()) | Err(Error::ConnectionClosed | Error::AlreadyClosed) => {}
        Err(error) => panic!("native close acknowledgement failed: {error}"),
    }
}

async fn released(h: &Harness, source: &str) {
    timeout(CLEANUP, async {
        loop {
            let events = h.fake.state.events();
            let all_closed = events.iter().all(|event| match event {
                ConnectionEvent::Opened(id) => events
                    .iter()
                    .any(|event| matches!(event, ConnectionEvent::Closed(closed) if closed == id)),
                _ => true,
            });
            if all_closed
                && h.state.inflight_requests.load(Ordering::Acquire) == 0
                && h.state.accounts.inflight_count(source) == 0
                && h.state.bindings.pending_count(source) == 0
                && h.state.ws_reserved_for_test(source) == 0
                && h.state.budget.used() == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("held WS must release actual upstream/socket/native owners before teardown");
    assert!(
        !h.servers.data_task.is_finished(),
        "relay teardown cannot prove cleanup"
    );
}

async fn unsent_row(f: &Held) {
    let row = r1_create_record(&f.h, f.model).await;
    assert_eq!(row["account"], f.source);
    assert_eq!(row["attempts"], 0);
    assert_eq!(row["attempt_details"], json!([]));
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(row["bytes_out"], 0);
}

async fn before_reset(f: &mut Held) {
    let h = &f.h;
    let reset_not_before = f.reset_not_before;
    let no_early_send = async {
        loop {
            if Instant::now() < reset_not_before {
                assert_unsent(h);
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        biased;
        _ = sleep_until(reset_not_before) => {}
        message = f.client.next() => panic!(
            "unsent quota-held create must not produce any response before reset: {message:?}"
        ),
        _ = no_early_send => unreachable!(),
    }
}

#[tokio::test]
async fn n5b_ws_real_poll_unsent_reset_sends_same_account_exactly_once() {
    let mut f = held("synthetic-poll-ws-reset", Mode::Recover, false).await;
    let done = Message::Text(r#"{"type":"response.completed"}"#.into());
    f.h.fake.state.push_reply([done.clone()]);
    before_reset(&mut f).await;
    let reselect = stage(&mut f.events, "reselect", &f.source).await;
    assert!(
        Utc::now() >= f.reset,
        "quota reselect must follow actual wall reset"
    );
    assert_eq!(reselect.deadline, f.admitted.deadline);
    let received = timeout(PHASE, f.client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received, done);
    assert_eq!(
        application_messages(&f.h),
        [Message::Text(f.raw.clone().into())]
    );
    let source = f.h.store.load(&f.source).unwrap();
    let tokens = f.h.store.tokens(&source).unwrap();
    for request in f.h.fake.state.requests() {
        assert!(
            request.headers["authorization"].to_str().unwrap()
                == format!("Bearer {}", tokens.access_token.as_str()),
            "reset dispatch/redial must retain the actual bound source"
        );
    }
    let row = r1_create_record(&f.h, f.model).await;
    assert_eq!(row["account"], f.source);
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(row["outcome"], "ok");
    sleep_until(f.late_horizon).await;
    assert_eq!(
        application_messages(&f.h),
        [Message::Text(f.raw.clone().into())]
    );
    quiet(&mut f.events);
    timeout(
        PHASE,
        f.client.close(Some(CloseFrame {
            code: 1000.into(),
            reason: "".into(),
        })),
    )
    .await
    .unwrap()
    .unwrap();
    close_frame(&mut f.client, 1000).await;
    released(&f.h, &f.source).await;
}

#[tokio::test]
async fn n5b_ws_real_poll_unsent_close_has_no_late_send_after_reset() {
    let mut f = held("synthetic-poll-ws-close", Mode::Recover, false).await;
    timeout(
        PHASE,
        f.client.close(Some(CloseFrame {
            code: 1000.into(),
            reason: "".into(),
        })),
    )
    .await
    .unwrap()
    .unwrap();
    close_frame(&mut f.client, 1000).await;
    released(&f.h, &f.source).await;
    assert!(
        Utc::now() < f.reset,
        "Close cleanup must precede original reset"
    );
    unsent_row(&f).await;
    sleep_until(f.late_horizon).await;
    assert_unsent(&f.h);
    assert_eq!(
        f.h.fake.state.requests().len(),
        1,
        "no late dial after Close"
    );
    quiet(&mut f.events);
}

#[tokio::test]
async fn n5b_ws_real_poll_unsent_policy_zero_revokes_before_reset() {
    let mut f = held("synthetic-poll-ws-policy", Mode::Strict, false).await;
    let previous = f.h.state.bindings.policies.plan(Some(KEY));
    policy(&f.h, Mode::Strict, 0).await;
    let current = f.h.state.bindings.policies.plan(Some(KEY));
    assert_eq!(current.settings.mode, previous.settings.mode);
    assert_eq!(current.anchor, previous.anchor);
    assert!(current.revision > previous.revision);
    close_frame(&mut f.client, 1000).await;
    released(&f.h, &f.source).await;
    assert!(
        Utc::now() < f.reset,
        "policy revocation must not wait for reset"
    );
    assert_eq!(
        f.h.state.bindings.current(Some(KEY)).as_deref(),
        Some(f.source.as_str())
    );
    unsent_row(&f).await;
    sleep_until(f.late_horizon).await;
    assert_unsent(&f.h);
    assert_eq!(
        f.h.fake.state.requests().len(),
        1,
        "no late dial after policy refusal"
    );
    quiet(&mut f.events);
}

#[tokio::test]
async fn n5b_ws_real_poll_unsent_original_deadline_vetoes_delayed_first_write() {
    let mut f = held("synthetic-poll-ws-deadline", Mode::Recover, true).await;
    before_reset(&mut f).await;
    let reselect = timeout_at(
        f.admitted.deadline,
        stage(&mut f.events, "reselect", &f.source),
    )
    .await
    .expect("post-reset reselect must precede the original turn deadline");
    assert!(Utc::now() >= f.reset);
    assert_eq!(reselect.deadline, f.admitted.deadline);
    let (arrived, release) = f.before_write.take().unwrap();
    timeout(PHASE, arrived)
        .await
        .expect("reset must reach the actual pre-write gate without sending")
        .unwrap();
    assert!(Instant::now() + Duration::from_millis(500) < f.admitted.deadline);
    assert_unsent(&f.h);
    // This timer intentionally crosses the ORIGINAL deadline. It is not used
    // to guess admission and does not grant any fresh operation or wait budget.
    sleep_until(f.admitted.deadline + Duration::from_millis(100)).await;
    let _ = release.send(()); // The original deadline may already have cancelled its slot.
    close_frame(&mut f.client, 1000).await;
    released(&f.h, &f.source).await;
    assert_unsent(&f.h);
    unsent_row(&f).await;
    assert_eq!(
        f.h.fake.state.requests().len(),
        1,
        "no post-deadline redial"
    );
    quiet(&mut f.events);
}

#[tokio::test]
async fn n5b_ws_real_poll_unsent_drain_cancels_before_reset_without_late_send() {
    let mut f = held("synthetic-poll-ws-drain", Mode::Recover, false).await;
    let state = f.h.state.clone();
    // Publication precedes persistence/export. Keep the actual client polling
    // while the whole drain runs; neither future is detached on cancellation.
    let drain = async {
        timeout_at(f.reset_not_before, state.begin_drain())
            .await
            .expect("native drain freeze must finish before the original reset")
            .expect("native drain/freeze must succeed");
    };
    let client_close = async {
        timeout(PHASE, f.h.state.wait_for_drain_start())
            .await
            .expect("native drain admission must close promptly");
        assert!(f.h.state.is_draining());
        close_frame(&mut f.client, 1001).await;
        released(&f.h, &f.source).await;
        assert!(
            Utc::now() < f.reset,
            "drain must cancel the admitted unsent WS wait before reset"
        );
    };
    tokio::join!(drain, client_close);
    unsent_row(&f).await;
    assert_unsent(&f.h);
    quiet(&mut f.events);
    // Drain is native authority. Keep the real listener/fake alive beyond R;
    // dropping the Harness is not evidence that the retained send was cancelled.
    sleep_until(f.late_horizon).await;
    assert_unsent(&f.h);
    assert_eq!(
        f.h.fake.state.requests().len(),
        1,
        "drain must not permit a late WS dial after quota reset"
    );
    quiet(&mut f.events);
    assert!(!f.h.servers.data_task.is_finished());
}

#[tokio::test]
async fn n5b_ws_real_poll_weekly_only_refuses_unsent_wait_without_data() {
    // This case cannot use held(): that helper requires a qualifying admitted
    // event. Preserve the same healthy-upgrade / retained-frame / real-poll order.
    let usage = timeout(PHASE, ScriptedUpstream::start())
        .await
        .unwrap()
        .unwrap();
    let h = Harness::new_configured(
        1,
        false,
        false,
        None,
        Some((REQUEST_SECS, 5)),
        None,
        |config| {
            config.upstream.codex_usage_base = usage.url.clone();
            config.routing.reset_hold_secs = 60;
            config.routing.validate().unwrap();
        },
    )
    .await;
    assert!(h.servers.data_addr.ip().is_loopback());
    let (mut client, source) = timeout(PHASE, h.connect()).await.unwrap();
    assert!(matches!(
        h.store.load(&source).unwrap().health,
        Health::Ready
    ));
    assert_eq!(h.state.ws_reserved_for_test(&source), 1);
    assert_eq!(h.fake.state.requests().len(), 1);
    assert!(usage.state.requests().is_empty());
    assert_unsent(&h);
    policy(&h, Mode::Recover, 60).await;
    assert_eq!(
        h.state.bindings.current(Some(KEY)).as_deref(),
        Some(source.as_str())
    );
    let model = "synthetic-poll-ws-weekly-refusal";
    let raw = format!("  {{\"type\":\"response.create\",\"model\":\"{model}\",\"input\":[]}} \n");
    let (retained, release) = h.state.pause_ws_ordinary_after_pacing(KEY, raw.as_bytes());
    let mut events = h.state.subscribe_ws_quota_wait();
    timeout(PHASE, client.send(Message::Text(raw.into())))
        .await
        .unwrap()
        .unwrap();
    timeout(PHASE, retained)
        .await
        .expect("healthy weekly-refusal frame must reach the retained-frame gate")
        .unwrap();
    assert_unsent(&h);

    let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + RESET_SECS, 0).unwrap();
    usage.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({"rate_limit": {
            // Both complete provider slots, with a genuine 7d-only exhaustion.
            "primary_window":{"used_percent":10, "limit_window_seconds":18000,
                "reset_at":(reset + ChronoDuration::hours(4)).timestamp()},
            "secondary_window":{"used_percent":100, "limit_window_seconds":604800,
                "reset_at":reset.timestamp()},
            "active_limit":"7d"
        }})
        .to_string(),
    ));
    let account = timeout(PHASE, h.state.quota.poll(&source))
        .await
        .expect("weekly-only complete real poll must finish at the setup gate")
        .expect("native poll/reducer must accept the complete weekly-only report");
    assert!(matches!(account.health, Health::Cooling {
        reason: HealthReason::Quota, until, ref scope,
    } if until == reset && scope == "account"));
    let cooling = account.quota_cooling.unwrap();
    assert_eq!(cooling.window, "7d");
    assert_eq!(cooling.reset_at, reset);
    let polls = usage.state.requests();
    assert_eq!(polls.len(), 1);
    assert_eq!(polls[0].method, axum::http::Method::GET);
    assert_eq!(polls[0].path_and_query, "/backend-api/wham/usage");
    assert!(polls[0].body.is_empty());
    assert!(polls[0].headers.get("authorization").is_some());
    assert!(
        polls[0].headers.get("authorization")
            == h.fake.state.requests()[0].headers.get("authorization"),
        "weekly poll must use the same synthetic source credential as upgrade"
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_unsent(&h);
    let sampled = Instant::now();
    let late_horizon =
        sampled + (reset - Utc::now()).to_std().unwrap() + Duration::from_millis(300);
    assert!(
        reset - Utc::now() > ChronoDuration::seconds(3),
        "weekly refusal must have a bounded pre-reset observation interval"
    );
    release
        .send(())
        .expect("weekly-refusal setup gate disappeared");
    tokio::select! {
        biased;
        event = events.recv() => panic!(
            "weekly-only complete poll must not admit WS quota wait: {event:?}"
        ),
        _ = close_frame(&mut client, 1000) => {}
    }
    released(&h, &source).await;
    assert!(
        Utc::now() < reset,
        "weekly-only refusal must use native Close before reset"
    );
    quiet(&mut events);
    let row = r1_create_record(&h, model).await;
    assert_eq!(row["account"], source);
    assert_eq!(row["attempts"], 0);
    assert_eq!(row["attempt_details"], json!([]));
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(row["bytes_out"], 0);
    assert_unsent(&h);
    sleep_until(late_horizon).await;
    assert_unsent(&h);
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "weekly-only refusal must not produce a late data dial after reset"
    );
    quiet(&mut events);
    assert!(!h.state.is_draining());
    assert!(!h.servers.data_task.is_finished());
}

// Second real polls use an opt-in validated 1s fixture interval. TooSoon is
// honored by its native retry_after; no poll clock or origin is manufactured.
async fn newer_real_poll(f: &Held, body: Value) -> qb_core::accounts::Account {
    f._usage
        .state
        .push(ScriptedResponse::json(StatusCode::OK, body.to_string()));
    let account = timeout_at(f.reset_not_before, async {
        loop {
            assert_unsent(&f.h);
            match timeout(PHASE, f.h.state.quota.poll(&f.source))
                .await
                .expect("newer real quota poll must finish within its phase bound")
            {
                Ok(account) => break account,
                Err(qb_core::quota::PollError::TooSoon { retry_after }) => {
                    assert!((1..=2).contains(&retry_after));
                    sleep_until(Instant::now() + Duration::from_secs(retry_after)).await;
                }
                Err(error) => panic!("newer real quota poll failed: {error:?}"),
            }
        }
    })
    .await
    .expect("newer quota observation must be applied before the ORIGINAL reset");
    let polls = f._usage.state.requests();
    assert_eq!(polls.len(), 2, "exactly two real usage GETs required");
    let upgrade = f.h.fake.state.requests();
    assert_eq!(upgrade.len(), 1, "quota polling must not redial the WS");
    for request in polls {
        assert_eq!(request.method, axum::http::Method::GET);
        assert_eq!(request.path_and_query, "/backend-api/wham/usage");
        assert!(request.body.is_empty());
        assert!(request.headers.get("authorization").is_some());
        assert!(
            request.headers.get("authorization") == upgrade[0].headers.get("authorization"),
            "both real polls must use the healthy socket's synthetic source credential"
        );
    }
    assert!(Utc::now() < f.reset);
    account
}

fn still_same_socket(f: &Held) {
    assert_eq!(f.h.fake.state.requests().len(), 1);
    assert_eq!(
        f.h.fake
            .state
            .events()
            .iter()
            .filter(|event| matches!(event, ConnectionEvent::Opened(_)))
            .count(),
        1,
        "early reset must reuse the original actual upgraded socket"
    );
    assert_eq!(
        f.h.state.bindings.current(Some(KEY)).as_deref(),
        Some(f.source.as_str())
    );
}

#[tokio::test]
async fn n5b_ws_real_poll_early_reset_resumes_same_socket_once_before_original_reset() {
    let mut f = held_with_poll_interval(
        "synthetic-poll-ws-early-reset",
        Mode::Recover,
        false,
        Some(1),
    )
    .await;
    let old_hold = f.h.store.load(&f.source).unwrap().quota_cooling.unwrap();
    let done = Message::Text(r#"{"type":"response.completed"}"#.into());
    f.h.fake.state.push_reply([done.clone()]);
    let next_reset = f.reset + ChronoDuration::seconds(600);
    assert!(next_reset - old_hold.reset_at > ChronoDuration::seconds(300));
    let account = newer_real_poll(
        &f,
        json!({"rate_limit": {
            "primary_window":{"used_percent":0, "limit_window_seconds":18000,
                "reset_at":next_reset.timestamp()},
            "secondary_window":{"used_percent":10, "limit_window_seconds":604800,
                "reset_at":(f.reset + ChronoDuration::hours(47)).timestamp()},
            "active_limit":"5h", "limit_reached":false
        }}),
    )
    .await;
    assert!(matches!(account.health, Health::Ready));
    assert!(account.quota_cooling.is_none());
    assert!(matches!(
        f.h.store.load(&f.source).unwrap().health,
        Health::Ready
    ));
    let snapshot = f.h.obs.quota_snapshot(&f.source).unwrap();
    let primary = snapshot["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["name"] == "5h")
        .unwrap();
    let observed: chrono::DateTime<Utc> = primary["observed_at"].as_str().unwrap().parse().unwrap();
    assert!(observed > old_hold.started_at);
    assert_eq!(primary["used_pct"], 0.0);
    assert_eq!(primary["reset_at"], json!(next_reset));

    let received = timeout_at(f.reset_not_before, f.client.next())
        .await
        .expect("newer healthy full poll must resume the retained WS create before original reset")
        .expect("early-reset WS must retain its client socket")
        .expect("early-reset WS transport must stay healthy");
    assert_eq!(
        received, done,
        "newer healthy full poll must resume the retained WS create before original reset"
    );
    assert!(
        Utc::now() < f.reset,
        "original reset cannot mask early-reset recovery"
    );
    let reselect = stage(&mut f.events, "reselect", &f.source).await;
    assert_eq!(reselect.deadline, f.admitted.deadline);
    assert_eq!(
        application_messages(&f.h),
        [Message::Text(f.raw.clone().into())]
    );
    still_same_socket(&f);
    let row = r1_create_record(&f.h, f.model).await;
    assert_eq!(row["account"], f.source);
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(row["outcome"], "ok");
    sleep_until(f.late_horizon).await;
    assert_eq!(
        application_messages(&f.h),
        [Message::Text(f.raw.clone().into())]
    );
    still_same_socket(&f);
    quiet(&mut f.events);
    timeout(
        PHASE,
        f.client.close(Some(CloseFrame {
            code: 1000.into(),
            reason: "".into(),
        })),
    )
    .await
    .unwrap()
    .unwrap();
    close_frame(&mut f.client, 1000).await;
    released(&f.h, &f.source).await;
}

#[tokio::test]
async fn n5b_ws_real_poll_partial_still_cooling_never_sends_early() {
    let mut f = held_with_poll_interval(
        "synthetic-poll-ws-partial-cooling",
        Mode::Recover,
        false,
        Some(1),
    )
    .await;
    let old_hold = f.h.store.load(&f.source).unwrap().quota_cooling.unwrap();
    let old_snapshot = f.h.obs.quota_snapshot(&f.source).unwrap();
    let weekly_before = old_snapshot["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["name"] == "7d")
        .unwrap()
        .clone();
    // A genuine newer partial report: the provider's secondary slot is absent.
    // The native merged display may retain 7d, but raw proof cannot borrow it.
    let account = newer_real_poll(
        &f,
        json!({"rate_limit": {
            "primary_window":{"used_percent":100, "limit_window_seconds":18000,
                "reset_at":f.reset.timestamp()},
            "active_limit":"5h"
        }}),
    )
    .await;
    assert!(matches!(account.health, Health::Cooling {
        reason: HealthReason::Quota, until, ref scope,
    } if until == f.reset && scope == "account"));
    assert_eq!(account.quota_cooling, Some(old_hold.clone()));
    let snapshot = f.h.obs.quota_snapshot(&f.source).unwrap();
    let windows = snapshot["windows"].as_array().unwrap();
    assert_eq!(
        windows.len(),
        2,
        "native partial merge retains the old weekly window"
    );
    assert_eq!(
        windows
            .iter()
            .find(|window| window["name"] == "7d")
            .unwrap(),
        &weekly_before
    );
    let primary = windows
        .iter()
        .find(|window| window["name"] == "5h")
        .unwrap();
    let observed: chrono::DateTime<Utc> = primary["observed_at"].as_str().unwrap().parse().unwrap();
    assert!(observed > old_hold.started_at);
    assert_eq!(primary["used_pct"], 100.0);
    assert_eq!(primary["reset_at"], json!(f.reset));
    close_frame(&mut f.client, 1000).await;
    released(&f.h, &f.source).await;
    assert!(
        Utc::now() < f.reset,
        "partial refusal must complete before original reset"
    );
    unsent_row(&f).await;
    assert_unsent(&f.h);
    still_same_socket(&f);
    // The diagnostic can be emitted before or after final native eligibility.
    // At most one reselect is permitted; a new admission/budget is never valid.
    match f.events.try_recv() {
        Ok(event) => {
            assert_eq!(event.stage, "reselect");
            assert_eq!(event.source, f.source);
            assert_eq!(event.deadline, f.admitted.deadline);
        }
        Err(broadcast::error::TryRecvError::Empty) => {}
        Err(error) => panic!("partial-refusal observer failed: {error:?}"),
    }
    quiet(&mut f.events);
    sleep_until(f.late_horizon).await;
    assert_unsent(&f.h);
    still_same_socket(&f);
    quiet(&mut f.events);
}

//! Test-only child of n5b_ws_quota: actual 101, real poll, authenticated edit.
//! No direct proof constructor, health assignment or new production observer.
use super::super::{Client, Harness, r1_create_record};
use super::{
    Held, KEY, PHASE, application_messages, assert_unsent, close_frame, held, quiet, released,
    unsent_row,
};
use axum::http::StatusCode;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use qb_core::routing::Mode;
use qb_testkit::websocket::{ConnectionEvent, Message};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    sync::oneshot,
    time::{Instant, sleep_until, timeout, timeout_at},
};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

const RETAIN: &str = "WS urgency cancellation must retain original native cache hold";

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(PHASE)
        .build()
        .unwrap()
}

async fn admin(h: &Harness, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
    assert!(h.servers.admin_addr.ip().is_loopback());
    let mut request = http_client()
        .request(method, format!("http://{}{path}", h.servers.admin_addr))
        .bearer_auth("synthetic-admin-key");
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = timeout(PHASE, request.send())
        .await
        .expect("synthetic authenticated admin request must finish")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    timeout(PHASE, response.json()).await.unwrap().unwrap()
}

async fn wait_view(
    h: &Harness,
    source: &str,
    reset: chrono::DateTime<Utc>,
    mode: Mode,
    on: &str,
    waiting: bool,
) {
    let view = admin(
        h,
        reqwest::Method::GET,
        "/admin/v1/sessions?range=all",
        None,
    )
    .await;
    let id = qb_core::dashboard::SessionId::from_key(KEY);
    let row = view["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("actual dashboard must retain the upgraded WS session row");
    assert_eq!(row["account"], source);
    assert_eq!(
        row["routing"]["action_id"],
        qb_core::routing::session_id(KEY)
    );
    assert_eq!(row["routing"]["mode"], json!(mode));
    assert_eq!(row["routing"]["on_5h_limit"], on);
    assert_eq!(row["quota_wait_known"], true);
    if waiting {
        assert_eq!(
            row["quota_wait"]["waiting"], 1,
            "{RETAIN}: real dashboard lifetime"
        );
        let observed =
            chrono::DateTime::parse_from_rfc3339(row["quota_wait"]["reset_at"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            observed.timestamp(),
            reset.timestamp(),
            "original dashboard reset must not renew"
        );
    } else {
        assert!(
            row["quota_wait"].is_null(),
            "completed/cancelled WS wait must release its real dashboard lifetime"
        );
    }
}

async fn move_while_waiting(
    h: &Harness,
    source: &str,
    reset: chrono::DateTime<Utc>,
    mode: Mode,
    checkpoint: Instant,
) {
    wait_view(h, source, reset, mode, "wait", true).await;
    let result = admin(
        h,
        reqwest::Method::POST,
        "/admin/v1/routing",
        Some(json!({
            "kind": "urgent",
            "id": qb_core::routing::session_id(KEY),
            "on_5h_limit": "move",
        })),
    )
    .await;
    assert_eq!(result["updated"], true);
    wait_view(h, source, reset, mode, "move", true).await;
    // A second public projection is sampled at an original-interval checkpoint,
    // while the native/client watcher remains polled by join!, not after an
    // arbitrary sleep used to infer that admission or policy handling happened.
    sleep_until(checkpoint).await;
    timeout_at(
        checkpoint + Duration::from_secs(1),
        wait_view(h, source, reset, mode, "move", true),
    )
    .await
    .expect("dashboard lifetime checkpoint must finish before original reset");
    assert!(
        Utc::now() < reset,
        "checkpoint must not be satisfied by reset cleanup"
    );
}

fn original_socket_alive(h: &Harness, source: &str) {
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "urgency must not dial a replacement WS"
    );
    let events = h.fake.state.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ConnectionEvent::Opened(_)))
            .count(),
        1
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            ConnectionEvent::Closed(_) | ConnectionEvent::Error(_, _)
        )),
        "{RETAIN}: original upgraded upstream must remain alive"
    );
    assert_eq!(h.state.bindings.current(Some(KEY)).as_deref(), Some(source));
}

async fn watch_before_reset(
    h: &Harness,
    client: &mut Client,
    before_write: &mut Option<oneshot::Receiver<()>>,
    until: Instant,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = sleep_until(until) => return,
            at_write = async { before_write.as_mut().unwrap().await }, if before_write.is_some() => {
                panic!("{RETAIN}: physical write gate reached before original reset: {at_write:?}");
            },
            message = client.next() => match message {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                    timeout(PHASE, client.flush()).await.unwrap().unwrap();
                },
                other => panic!("{RETAIN}: no early client terminal/close/data: {other:?}"),
            },
            _ = tick.tick() => {
                assert!(application_messages(h).is_empty(), "{RETAIN}: no early upstream data");
            },
        }
    }
}

async fn close_and_release(f: &mut Held) {
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
async fn n5c_ws_real_poll_midwait_move_keeps_original_hold_then_same_socket_once() {
    for (mode, model) in [
        (Mode::Recover, "synthetic-n5c-ws-recover"),
        (Mode::Rebalance, "synthetic-n5c-ws-rebalance"),
    ] {
        let mut f = held(model, mode, true).await;
        let original_deadline = f.admitted.deadline;
        let (arrived, release) = f.before_write.take().unwrap();
        let mut arrived = Some(arrived);
        let checkpoint = f.reset_not_before - Duration::from_secs(2);
        assert!(
            Instant::now() < checkpoint,
            "synthetic admin/checkpoint setup must fit original reset"
        );
        let done = Message::Text(r#"{"type":"response.completed"}"#.into());
        f.h.fake.state.push_reply([done.clone()]);
        // Observe the real client and physical before-write gate concurrently
        // with the authenticated policy edit and both dashboard projections.
        tokio::join!(
            move_while_waiting(&f.h, &f.source, f.reset, mode, checkpoint),
            watch_before_reset(&f.h, &mut f.client, &mut arrived, f.reset_not_before),
        );
        let mut arrived = arrived.take().unwrap();
        tokio::select! {
            result = timeout_at(original_deadline, &mut arrived) => {
                result.expect("same original WS operation must reach first write").unwrap();
            },
            message = f.client.next() => panic!("{RETAIN}: socket lost before post-reset first write: {message:?}"),
        }
        assert!(
            Utc::now() >= f.reset,
            "{RETAIN}: physical eligibility must follow wall reset"
        );
        assert!(
            Instant::now() < original_deadline,
            "handoff must not replace the original turn deadline"
        );
        original_socket_alive(&f.h, &f.source);
        assert_unsent(&f.h);
        quiet(&mut f.events); // Native remainder must not manufacture optional reset/reselect.
        wait_view(&f.h, &f.source, f.reset, mode, "move", false).await;
        release
            .send(())
            .expect("original WS frame must still own the physical write slot");
        let received = timeout_at(original_deadline, f.client.next())
            .await
            .expect("original deadline must bound the retained WS operation")
            .unwrap()
            .unwrap();
        assert_eq!(received, done);
        assert_eq!(
            application_messages(&f.h),
            [Message::Text(f.raw.clone().into())]
        );
        original_socket_alive(&f.h, &f.source);
        let account = f.h.store.load(&f.source).unwrap();
        let tokens = f.h.store.tokens(&account).unwrap();
        assert!(
            f.h.fake.state.requests()[0].headers["authorization"]
                .to_str()
                .unwrap()
                == format!("Bearer {}", tokens.access_token),
            "same upgraded socket must use the original synthetic source credential"
        );
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
        original_socket_alive(&f.h, &f.source);
        quiet(&mut f.events);
        close_and_release(&mut f).await;
        wait_view(&f.h, &f.source, f.reset, mode, "move", false).await;
    }
}

#[tokio::test]
async fn n5c_ws_real_poll_midwait_move_client_close_cancels_without_late_send() {
    let mut f = held("synthetic-n5c-ws-cancel", Mode::Recover, false).await;
    let checkpoint = f.reset_not_before - Duration::from_secs(3);
    assert!(
        Instant::now() < checkpoint,
        "synthetic cancellation checkpoint must fit original reset"
    );
    let mut no_gate = None;
    tokio::join!(
        move_while_waiting(&f.h, &f.source, f.reset, Mode::Recover, checkpoint),
        watch_before_reset(
            &f.h,
            &mut f.client,
            &mut no_gate,
            checkpoint + Duration::from_secs(1)
        ),
    );
    original_socket_alive(&f.h, &f.source);
    assert_unsent(&f.h);
    quiet(&mut f.events);
    timeout_at(f.reset_not_before, close_and_release(&mut f))
        .await
        .expect("native WS cleanup must complete before the original reset");
    assert!(
        Utc::now() < f.reset,
        "Close must cancel the retained native remainder before reset"
    );
    unsent_row(&f).await;
    wait_view(&f.h, &f.source, f.reset, Mode::Recover, "move", false).await;
    sleep_until(f.late_horizon).await;
    assert_unsent(&f.h);
    assert_eq!(
        f.h.fake.state.requests().len(),
        1,
        "cancelled urgency handoff must never redial"
    );
    quiet(&mut f.events);
    assert!(
        !f.h.servers.data_task.is_finished(),
        "teardown cannot prove cancellation cleanup"
    );
}

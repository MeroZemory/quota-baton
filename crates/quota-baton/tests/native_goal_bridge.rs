//! Standalone coverage for staged, unregistered native ownership code. No native
//! process, credentials, transport, provider policy or core authorization fixture.
#[path = "../src/native_resume/goal.rs"]
mod goal;

use goal::{Action, Controller, Delivery, Error, MAX_FRAME_BYTES, MAX_PENDING, StoppedGoalTicket};
use serde_json::{Value, json};

const PREFIX: &str = "qb-goal-0123456789abcdef0123456789abcdef:";

fn controller() -> Controller {
    Controller::new(PREFIX.to_owned()).unwrap()
}

fn bytes(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

fn ui(c: &mut Controller, id: Value, method: &str, params: Value) {
    let raw = bytes(&json!({"id": id, "method": method, "params": params}));
    match c.ui_frame(&raw).unwrap() {
        Action::ToNative(forwarded) => assert_eq!(forwarded, raw),
        _ => panic!("expected native passthrough"),
    }
}

fn response(c: &mut Controller, id: Value, result: Value) -> Action {
    c.native_frame(&bytes(&json!({"id": id, "result": result})))
        .unwrap()
}

fn notice(c: &mut Controller, method: &str, params: Value) {
    let raw = bytes(&json!({"method": method, "params": params}));
    match c.native_frame(&raw).unwrap() {
        Action::ToUi(forwarded) => assert_eq!(forwarded, raw),
        _ => panic!("expected UI passthrough"),
    }
}

fn snapshot(status: &str) -> Value {
    json!({"threadId": "thread-a", "objective": "user objective", "createdAt": 123,
        "updatedAt": 456, "status": status, "tokenUsage": {"total": 10}})
}

fn select(c: &mut Controller) {
    ui(c, json!(1), "thread/start", json!({}));
    response(c, json!(1), json!({"thread": {"id": "thread-a"}}));
}

fn user_active(c: &mut Controller) {
    select(c);
    ui(
        c,
        json!(2),
        "thread/goal/set",
        json!({"threadId": "thread-a",
        "objective": "user objective", "origin": "user", "status": "active"}),
    );
    // Native can emit its active echo before the correlated response.
    notice(
        c,
        "thread/goal/updated",
        json!({"threadId": "thread-a", "goal": snapshot("active")}),
    );
    assert!(c.stopped_ticket().is_none());
    response(c, json!(2), json!({"goal": snapshot("active")}));
}

fn quota(c: &mut Controller) {
    notice(
        c,
        "thread/goal/updated",
        json!({"threadId": "thread-a", "goal": snapshot("usageLimited")}),
    );
}

fn read_goal(c: &mut Controller, goal: Value) {
    ui(
        c,
        json!(3),
        "thread/goal/get",
        json!({"threadId": "thread-a"}),
    );
    response(c, json!(3), json!({"goal": goal}));
}

fn ready(c: &mut Controller) -> StoppedGoalTicket {
    user_active(c);
    quota(c);
    assert!(
        c.stopped_ticket().is_none(),
        "notification alone is insufficient"
    );
    read_goal(c, snapshot("usageLimited"));
    let ticket = c.stopped_ticket().expect("correlated stopped snapshot");
    assert_eq!(ticket.thread_id(), "thread-a");
    ticket
}

fn command(action: Action) -> Value {
    match action {
        Action::ToNative(raw) | Action::ResumeNative(raw) => serde_json::from_slice(&raw).unwrap(),
        _ => panic!("expected injected native request"),
    }
}

fn error<T>(result: Result<T, Error>) -> Error {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected fixed error"),
    }
}

fn final_read(c: &mut Controller, ticket: StoppedGoalTicket) -> Value {
    let read = command(c.begin_final_read(ticket).unwrap());
    assert_eq!(
        read,
        json!({"id": format!("{PREFIX}1"), "method": "thread/goal/get",
        "params": {"threadId": "thread-a"}})
    );
    assert_eq!(c.delivery(), Delivery::Checking);
    read["id"].clone()
}

#[test]
fn observed_user_goal_stopped_snapshot_final_read_and_status_only_active() {
    let mut c = controller();
    let ticket = ready(&mut c);
    assert!(c.stopped_ticket().is_none(), "ticket is nonrepeatable");
    let id = final_read(&mut c, ticket);
    let active = command(response(
        &mut c,
        id.clone(),
        json!({"goal": snapshot("usageLimited")}),
    ));
    assert_eq!(
        active,
        json!({"id": format!("{PREFIX}2"), "method": "thread/goal/set",
        "params": {"threadId": "thread-a", "status": "active", "origin": "automatic"}})
    );
    assert_eq!(c.delivery(), Delivery::UnknownNoRetry);
    assert!(matches!(
        response(&mut c, id, json!({"goal": snapshot("usageLimited")})),
        Action::Suppress
    ));
    assert!(matches!(
        response(
            &mut c,
            active["id"].clone(),
            json!({"goal": snapshot("active")})
        ),
        Action::Suppress
    ));
    assert_eq!(c.delivery(), Delivery::Acknowledged);
    assert!(matches!(
        response(
            &mut c,
            active["id"].clone(),
            json!({"goal": snapshot("active")})
        ),
        Action::Suppress
    ));
    assert!(c.stopped_ticket().is_none());
}

#[test]
fn no_observed_user_active_or_no_native_quota_never_offers_ticket() {
    for status in ["active", "usageLimited", "paused", "completed"] {
        let mut c = controller();
        select(&mut c);
        quota(&mut c);
        read_goal(&mut c, snapshot(status));
        assert!(c.stopped_ticket().is_none());
    }
    let mut c = controller();
    user_active(&mut c);
    read_goal(&mut c, snapshot("usageLimited"));
    assert!(
        c.stopped_ticket().is_none(),
        "read cannot substitute for later native quota"
    );
    let mut c = controller();
    select(&mut c);
    ui(
        &mut c,
        json!(2),
        "thread/goal/set",
        json!({"threadId": "thread-a",
        "objective": "user objective", "origin": "automatic"}),
    );
    response(&mut c, json!(2), json!({"goal": snapshot("active")}));
    quota(&mut c);
    read_goal(&mut c, snapshot("usageLimited"));
    assert!(
        c.stopped_ticket().is_none(),
        "automatic origin is not user ownership"
    );
}

#[test]
fn live_reads_preserve_identity_but_prequota_inflight_reads_cannot_mint_tickets() {
    let mut c = controller();
    user_active(&mut c);
    read_goal(&mut c, snapshot("active"));
    ui(
        &mut c,
        json!(4),
        "thread/goal/get",
        json!({"threadId":"thread-a"}),
    );
    quota(&mut c);
    response(&mut c, json!(4), json!({"goal":snapshot("usageLimited")}));
    assert!(c.stopped_ticket().is_none());
    read_goal(&mut c, snapshot("usageLimited"));
    assert!(c.stopped_ticket().is_some());
}

#[test]
fn pipelined_reads_do_not_erase_selection_or_user_goal_correlation() {
    let mut c = controller();
    ui(&mut c, json!(1), "thread/start", json!({}));
    ui(&mut c, json!(2), "account/read", json!({}));
    response(&mut c, json!(1), json!({"thread":{"id":"thread-a"}}));
    response(&mut c, json!(2), json!({"account":null}));
    ui(
        &mut c,
        json!(3),
        "thread/goal/set",
        json!({
            "threadId":"thread-a", "objective":"user objective", "origin":"user", "status":"active"
        }),
    );
    ui(&mut c, json!(4), "config/read", json!({}));
    response(&mut c, json!(3), json!({"goal":snapshot("active")}));
    response(&mut c, json!(4), json!({}));
    quota(&mut c);
    read_goal(&mut c, snapshot("usageLimited"));
    assert!(
        c.stopped_ticket().is_some(),
        "pipelined reads are not owner changes"
    );
}

#[test]
fn crossed_thread_selection_replies_keep_only_the_latest_owner() {
    let mut c = controller();
    ui(&mut c, json!(1), "thread/start", json!({}));
    ui(&mut c, json!(2), "thread/start", json!({}));
    response(&mut c, json!(2), json!({"thread":{"id":"thread-b"}}));
    response(&mut c, json!(1), json!({"thread":{"id":"thread-a"}}));
    let mut goal = snapshot("active");
    goal["threadId"] = json!("thread-b");
    ui(
        &mut c,
        json!(3),
        "thread/goal/set",
        json!({
            "threadId":"thread-b", "objective":"user objective", "origin":"user", "status":"active"
        }),
    );
    response(&mut c, json!(3), json!({"goal":goal}));
    goal["status"] = json!("usageLimited");
    notice(
        &mut c,
        "thread/goal/updated",
        json!({"threadId":"thread-b", "goal":goal}),
    );
    ui(
        &mut c,
        json!(4),
        "thread/goal/get",
        json!({"threadId":"thread-b"}),
    );
    response(&mut c, json!(4), json!({"goal":goal}));
    let ticket = c.stopped_ticket().unwrap();
    let read = command(c.begin_final_read(ticket).unwrap());
    assert_eq!(read["params"]["threadId"], "thread-b");
}

#[test]
fn private_final_read_error_cancels_and_active_echo_before_ack_is_correlated() {
    let mut c = controller();
    let ticket = ready(&mut c);
    let id = final_read(&mut c, ticket);
    assert!(matches!(
        c.native_frame(&bytes(&json!({"id":id, "error":{"code":-1}})))
            .unwrap(),
        Action::Suppress
    ));
    assert_eq!(c.delivery(), Delivery::Cancelled);
    assert!(c.stopped_ticket().is_none());

    let mut c = controller();
    let ticket = ready(&mut c);
    let id = final_read(&mut c, ticket);
    let active = command(response(
        &mut c,
        id,
        json!({"goal":snapshot("usageLimited")}),
    ));
    notice(
        &mut c,
        "thread/goal/updated",
        json!({"threadId":"thread-a", "goal":snapshot("active")}),
    );
    assert_eq!(c.delivery(), Delivery::UnknownNoRetry);
    assert!(matches!(
        response(
            &mut c,
            active["id"].clone(),
            json!({"goal":snapshot("active")})
        ),
        Action::Suppress
    ));
    assert_eq!(c.delivery(), Delivery::Acknowledged);
}

#[test]
fn absent_paused_completed_or_changed_snapshot_never_revives() {
    let mut changed = snapshot("usageLimited");
    changed["objective"] = json!("replacement");
    let mut recreated = snapshot("usageLimited");
    recreated["createdAt"] = json!(124);
    let mut wrong_thread = snapshot("usageLimited");
    wrong_thread["threadId"] = json!("thread-b");
    for invalid in [
        Value::Null,
        snapshot("paused"),
        snapshot("completed"),
        changed,
        recreated,
        wrong_thread,
    ] {
        let mut c = controller();
        user_active(&mut c);
        quota(&mut c);
        read_goal(&mut c, invalid);
        assert!(c.stopped_ticket().is_none());
        quota(&mut c);
        read_goal(&mut c, snapshot("usageLimited"));
        assert!(
            c.stopped_ticket().is_none(),
            "later duplicates cannot revive invalidated ownership"
        );
    }
}

#[test]
fn exact_passthrough_preserves_spaces_ids_escapes_and_ui_responses() {
    let mut c = controller();
    for raw in [
        br#" { "id" : "ui-\u0031", "method" : "account/read", "params" : {} } "#.as_slice(),
        br#"{"id":"native-question","result":{"answer":"\u0061"}}"#.as_slice(),
    ] {
        match c.ui_frame(raw).unwrap() {
            Action::ToNative(out) => assert_eq!(out, raw),
            _ => panic!("UI frame was altered"),
        }
    }
    let raw = br#" {"id":"ui-1", "result" : {"account": null}} "#;
    match c.native_frame(raw).unwrap() {
        Action::ToUi(out) => assert_eq!(out, raw),
        _ => panic!("native frame was altered"),
    }
    assert!(matches!(
        response(&mut c, json!(format!("{PREFIX}unknown")), json!({})),
        Action::Suppress
    ));
}

#[test]
fn pending_bound_duplicate_and_private_collision_fail_closed() {
    let mut c = controller();
    for id in 0..MAX_PENDING {
        ui(&mut c, json!(id), "account/read", json!({}));
    }
    assert_eq!(
        error(c.ui_frame(&bytes(
            &json!({"id": MAX_PENDING, "method": "account/read"})
        ))),
        Error::PendingBound
    );
    assert_eq!(
        error(c.ui_frame(br#"{"id":99,"method":"account/read"}"#)),
        Error::Disconnected
    );
    let mut c = controller();
    ui(&mut c, json!(1), "account/read", json!({}));
    // String and integer IDs are distinct; duplicate integer is rejected.
    ui(&mut c, json!("1"), "account/read", json!({}));
    assert_eq!(
        error(c.ui_frame(br#"{"id":1,"method":"account/read"}"#)),
        Error::DuplicateId
    );
    let mut c = controller();
    assert_eq!(
        error(c.ui_frame(&bytes(
            &json!({"id": format!("{PREFIX}1"), "method": "account/read"})
        ))),
        Error::NamespaceCollision
    );
}

#[test]
fn unrelated_pending_request_vetoes_snapshot_and_later_request_revokes_ticket() {
    let mut c = controller();
    user_active(&mut c);
    quota(&mut c);
    ui(&mut c, json!(8), "account/read", json!({}));
    read_goal(&mut c, snapshot("usageLimited"));
    assert!(c.stopped_ticket().is_none());
    response(&mut c, json!(8), json!({}));
    assert!(
        c.stopped_ticket().is_none(),
        "unrelated response cannot mint a ticket"
    );
    read_goal(&mut c, snapshot("usageLimited"));
    let ticket = c.stopped_ticket().unwrap();
    ui(&mut c, json!(9), "account/read", json!({}));
    response(&mut c, json!(9), json!({}));
    assert_eq!(error(c.begin_final_read(ticket)), Error::AttemptConsumed);
    read_goal(&mut c, snapshot("usageLimited"));
    assert!(c.stopped_ticket().is_none());
}

#[test]
fn cancellation_switch_clear_and_unrelated_request_race_final_read() {
    for (method, params) in [
        (
            "thread/goal/set",
            json!({"threadId":"thread-a", "status":"paused", "origin":"user"}),
        ),
        (
            "thread/goal/set",
            json!({"threadId":"thread-a", "status":"completed", "origin":"user"}),
        ),
        (
            "thread/goal/set",
            json!({"threadId":"thread-a", "objective":"replacement", "origin":"user"}),
        ),
        ("thread/goal/clear", json!({"threadId":"thread-a"})),
        ("thread/resume", json!({"threadId":"thread-b"})),
        ("turn/interrupt", json!({"threadId":"thread-a"})),
        ("account/read", json!({})),
    ] {
        let mut c = controller();
        let ticket = ready(&mut c);
        let id = final_read(&mut c, ticket);
        ui(&mut c, json!(10), method, params);
        response(
            &mut c,
            json!(10),
            json!({"goal": snapshot("paused"), "thread":{"id":"thread-b"}}),
        );
        assert!(matches!(
            response(&mut c, id, json!({"goal": snapshot("usageLimited")})),
            Action::Suppress
        ));
        assert_eq!(c.delivery(), Delivery::Cancelled);
        quota(&mut c);
        read_goal(&mut c, snapshot("usageLimited"));
        assert!(c.stopped_ticket().is_none());
    }
}

#[test]
fn user_approval_or_denial_response_vetoes_pending_automatic_dispatch() {
    for result in [json!({"decision":"accept"}), json!({"decision":"decline"})] {
        let mut c = controller();
        let ticket = ready(&mut c);
        let id = final_read(&mut c, ticket);
        let raw = bytes(&json!({"id":"native-approval", "result":result}));
        match c.ui_frame(&raw).unwrap() {
            Action::ToNative(out) => assert_eq!(out, raw),
            _ => panic!("user reply must pass through"),
        }
        assert_eq!(c.delivery(), Delivery::Cancelled);
        assert!(matches!(
            response(&mut c, id, json!({"goal":snapshot("usageLimited")})),
            Action::Suppress
        ));
        assert!(c.stopped_ticket().is_none());
    }
}

#[test]
fn native_pause_clear_replace_and_exact_final_snapshot_are_required() {
    for status in ["paused", "completed", "active"] {
        let mut c = controller();
        let ticket = ready(&mut c);
        let id = final_read(&mut c, ticket);
        notice(
            &mut c,
            "thread/goal/updated",
            json!({"threadId":"thread-a", "goal":snapshot(status)}),
        );
        assert!(matches!(
            response(&mut c, id, json!({"goal":snapshot("usageLimited")})),
            Action::Suppress
        ));
        assert_eq!(c.delivery(), Delivery::Cancelled);
    }
    for method in [
        "thread/goal/cleared",
        "thread/archived",
        "thread/unsubscribed",
        "turn/started",
    ] {
        let mut c = controller();
        let ticket = ready(&mut c);
        let id = final_read(&mut c, ticket);
        notice(&mut c, method, json!({"threadId":"thread-a"}));
        assert!(matches!(
            response(&mut c, id, json!({"goal":snapshot("usageLimited")})),
            Action::Suppress
        ));
        assert_eq!(c.delivery(), Delivery::Cancelled);
    }
    for field in [
        "objective",
        "createdAt",
        "threadId",
        "updatedAt",
        "tokenUsage",
    ] {
        let mut c = controller();
        let ticket = ready(&mut c);
        let id = final_read(&mut c, ticket);
        let mut altered = snapshot("usageLimited");
        altered[field] = json!("changed");
        assert!(matches!(
            response(&mut c, id, json!({"goal":altered})),
            Action::Suppress
        ));
        assert_eq!(c.delivery(), Delivery::Cancelled);
    }
}

#[test]
fn lost_ack_disconnect_rejection_and_duplicate_receipts_never_send_twice() {
    for disconnect in [false, true] {
        let mut c = controller();
        let ticket = ready(&mut c);
        let id = final_read(&mut c, ticket);
        let active = command(response(
            &mut c,
            id.clone(),
            json!({"goal":snapshot("usageLimited")}),
        ));
        assert_eq!(c.delivery(), Delivery::UnknownNoRetry);
        if disconnect {
            c.disconnect();
            assert_eq!(
                error(c.native_frame(&bytes(
                    &json!({"id":active["id"], "result":{"goal":snapshot("active")}})
                ))),
                Error::Disconnected
            );
        } else {
            quota(&mut c);
            read_goal(&mut c, snapshot("usageLimited"));
            assert!(matches!(
                response(&mut c, id, json!({"goal":snapshot("usageLimited")})),
                Action::Suppress
            ));
        }
        assert_eq!(c.delivery(), Delivery::UnknownNoRetry);
        assert!(c.stopped_ticket().is_none());
    }
    let mut c = controller();
    let ticket = ready(&mut c);
    let id = final_read(&mut c, ticket);
    let active = command(response(
        &mut c,
        id,
        json!({"goal":snapshot("usageLimited")}),
    ));
    assert!(matches!(
        c.native_frame(&bytes(&json!({"id":active["id"], "error":{"code":-1}})))
            .unwrap(),
        Action::Suppress
    ));
    assert_eq!(c.delivery(), Delivery::Rejected);
    assert!(c.stopped_ticket().is_none());
    let mut c = controller();
    let ticket = ready(&mut c);
    c.disconnect();
    assert_eq!(error(c.begin_final_read(ticket)), Error::Disconnected);
}

#[test]
fn malformed_and_oversized_frames_have_fixed_errors_and_poison_connection() {
    let oversized = vec![b' '; MAX_FRAME_BYTES + 1];
    for (raw, expected) in [
        (&b""[..], Error::FrameBound),
        (oversized.as_slice(), Error::FrameBound),
        (&b"{secret"[..], Error::InvalidFrame),
        (&b"[]"[..], Error::InvalidFrame),
        (&b"\xff"[..], Error::InvalidFrame),
        (&b"{\"method\":\"x\"}\n"[..], Error::InvalidFrame),
        (&b"{\"method\":\"x\"}\r"[..], Error::InvalidFrame),
        (&br#"{"id":null,"method":"x"}"#[..], Error::InvalidId),
        (&br#"{"id":1.5,"method":"x"}"#[..], Error::InvalidId),
        (
            &br#"{"id":1,"result":{},"error":{}}"#[..],
            Error::InvalidFrame,
        ),
        (&br#"{"method":"x","params":[]}"#[..], Error::InvalidFrame),
    ] {
        for native in [false, true] {
            let mut c = controller();
            let actual = error(if native {
                c.native_frame(raw)
            } else {
                c.ui_frame(raw)
            });
            assert_eq!(actual, expected);
            assert!(actual.to_string().starts_with("native_goal_"));
            assert!(!actual.to_string().contains("secret"));
            assert_eq!(error(c.ui_frame(br#"{"method":"x"}"#)), Error::Disconnected);
        }
    }
    assert_eq!(
        error(Controller::new("short".to_owned())),
        Error::InvalidNamespace
    );
}

#[test]
fn ticket_is_bound_to_its_parent_namespace() {
    let mut c = controller();
    let ticket = ready(&mut c);
    let mut other =
        Controller::new("qb-goal-fedcba9876543210fedcba9876543210:".to_owned()).unwrap();
    let _other_ticket = ready(&mut other);
    assert_eq!(error(other.begin_final_read(ticket)), Error::TicketInvalid);
}

//! Private stopped-goal preflight coverage. No transport or core authorization
//! is supplied by this read; the parent owns authority checks and its timeout.
#[allow(dead_code)] // This isolated target exercises a subset of the production adapter.
#[path = "../src/native_resume/goal.rs"]
mod goal;

use goal::{Action, Controller, Delivery, Error};
use serde_json::{Value, json};

const PREFIX: &str = "qb-goal-0123456789abcdef0123456789abcdef:";
const NEXT: &str = "qb-goal-fedcba9876543210fedcba9876543210:";

fn snapshot(status: &str) -> Value {
    json!({"threadId":"thread-a", "objective":"user objective", "createdAt":123,
        "updatedAt":456, "status":status, "tokenUsage":{"total":10}})
}

fn updated(goal: Value) -> Value {
    json!({"method":"thread/goal/updated", "params":{"threadId":"thread-a", "goal":goal}})
}

fn reply(prefix: &str, serial: u8, goal: Value) -> Value {
    json!({"id":format!("{prefix}{serial}"), "result":{"goal":goal}})
}

fn ui(c: &mut Controller, value: Value) {
    let raw = value.to_string().into_bytes();
    assert!(matches!(c.ui_frame(&raw).unwrap(), Action::ToNative(out) if out == raw));
}

fn native(c: &mut Controller, value: Value) -> Action {
    c.native_frame(value.to_string().as_bytes()).unwrap()
}

fn forwarded(c: &mut Controller, value: Value) {
    let raw = value.to_string().into_bytes();
    assert!(matches!(c.native_frame(&raw).unwrap(), Action::ToUi(out) if out == raw));
}

fn suppressed(c: &mut Controller, value: Value) {
    assert!(matches!(native(c, value), Action::Suppress));
}

fn selected() -> Controller {
    let mut c = Controller::new(PREFIX.into()).unwrap();
    ui(&mut c, json!({"id":1,"method":"thread/start","params":{}}));
    forwarded(
        &mut c,
        json!({"id":1,"result":{"thread":{"id":"thread-a"}}}),
    );
    c
}

fn user_active() -> Controller {
    let mut c = selected();
    ui(
        &mut c,
        json!({"id":2,"method":"thread/goal/set","params":{
        "threadId":"thread-a", "objective":"user objective", "origin":"user", "status":"active"}}),
    );
    forwarded(&mut c, updated(snapshot("active")));
    forwarded(&mut c, json!({"id":2,"result":{"goal":snapshot("active")}}));
    c
}

fn stopped_notice() -> Controller {
    let mut c = user_active();
    forwarded(&mut c, updated(snapshot("usageLimited")));
    assert!(
        c.stopped_ticket().is_none(),
        "a notice alone is not a ticket"
    );
    c
}

#[test]
fn confirmed_ephemeral_cleanup_must_finish_before_stop_read_seals() {
    let mut c = user_active();
    ui(
        &mut c,
        json!({"id":90,"method":"thread/start","params":{"ephemeral":true}}),
    );
    forwarded(
        &mut c,
        json!({"id":90,"result":{"thread":{"id":"title-thread"}}}),
    );
    forwarded(&mut c, updated(snapshot("usageLimited")));
    assert!(
        c.begin_stop_read().unwrap().is_none(),
        "confirmed title cleanup must precede sealing"
    );
    ui(
        &mut c,
        json!({"id":91,"method":"thread/unsubscribe","params":{"threadId":"title-thread"}}),
    );
    assert!(
        c.begin_stop_read().unwrap().is_none(),
        "cleanup RPC still pending"
    );
    forwarded(&mut c, json!({"id":91,"result":{"status":"unsubscribed"}}));
    issue(&mut c, PREFIX);
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    assert!(
        c.stopped_ticket().is_some(),
        "original stopped goal remains eligible after cleanup"
    );
}

#[test]
fn failed_ephemeral_cleanup_cannot_unblock_sealing() {
    let mut c = user_active();
    ui(
        &mut c,
        json!({"id":90,"method":"thread/start","params":{"ephemeral":true}}),
    );
    forwarded(
        &mut c,
        json!({"id":90,"result":{"thread":{"id":"title-thread"}}}),
    );
    forwarded(&mut c, updated(snapshot("usageLimited")));
    ui(
        &mut c,
        json!({"id":91,"method":"thread/unsubscribe","params":{"threadId":"title-thread"}}),
    );
    forwarded(
        &mut c,
        json!({"id":91,"error":{"code":-1,"message":"synthetic"}}),
    );
    assert!(
        c.begin_stop_read().unwrap().is_none(),
        "error is not completed cleanup"
    );
    assert!(c.stopped_ticket().is_none());
}

#[test]
fn ephemeral_cleanup_survives_goal_generation_and_blocks_user_read_shortcut() {
    let mut c = selected();
    ui(
        &mut c,
        json!({"id":90,"method":"thread/start","params":{"ephemeral":true}}),
    );
    ui(
        &mut c,
        json!({"id":2,"method":"thread/goal/set","params":{
        "threadId":"thread-a","objective":"user objective","origin":"user","status":"active"}}),
    );
    // The start belongs to an older goal generation, but its native lifetime
    // still needs cleanup. It cannot supply any foreground-goal authority.
    forwarded(
        &mut c,
        json!({"id":90,"result":{"thread":{"id":"title-thread"}}}),
    );
    forwarded(&mut c, json!({"id":2,"result":{"goal":snapshot("active")}}));
    forwarded(&mut c, updated(snapshot("usageLimited")));
    user_read(&mut c);
    assert!(
        c.stopped_ticket().is_none(),
        "ordinary read must not bypass cleanup"
    );
    assert!(c.begin_stop_read().unwrap().is_none());
    ui(
        &mut c,
        json!({"id":91,"method":"thread/unsubscribe","params":{"threadId":"foreign-thread"}}),
    );
    forwarded(&mut c, json!({"id":91,"result":{"status":"unsubscribed"}}));
    assert!(
        c.begin_stop_read().unwrap().is_none(),
        "foreign cleanup is not evidence"
    );
    ui(
        &mut c,
        json!({"id":92,"method":"thread/unsubscribe","params":{"threadId":"title-thread"}}),
    );
    forwarded(
        &mut c,
        json!({"id":92,"result":{"status":"synthetic-unknown"}}),
    );
    assert!(
        c.begin_stop_read().unwrap().is_none(),
        "unknown result is not evidence"
    );
    ui(
        &mut c,
        json!({"id":93,"method":"thread/unsubscribe","params":{"threadId":"title-thread"}}),
    );
    forwarded(&mut c, json!({"id":93,"result":{"status":"notSubscribed"}}));
    issue(&mut c, PREFIX);
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    assert!(c.stopped_ticket().is_some());
}

#[test]
fn confirmed_ephemeral_lifetimes_are_bounded_and_duplicate_ids_fail_closed() {
    for duplicate in [false, true] {
        let mut c = user_active();
        let count = if duplicate { 1 } else { goal::MAX_PENDING };
        for index in 0..count {
            ui(
                &mut c,
                json!({"id":100+index,"method":"thread/start","params":{"ephemeral":true}}),
            );
            forwarded(
                &mut c,
                json!({"id":100+index,"result":{"thread":{"id":format!("title-{index}")}}}),
            );
        }
        ui(
            &mut c,
            json!({"id":999,"method":"thread/start","params":{"ephemeral":true}}),
        );
        let extra_id = if duplicate {
            "title-0"
        } else {
            "title-overflow"
        };
        let result = c.native_frame(
            json!({"id":999,"result":{"thread":{"id":extra_id}}})
                .to_string()
                .as_bytes(),
        );
        assert!(matches!(
            result,
            Err(Error::InvalidFrame | Error::PendingBound)
        ));
        assert!(
            c.begin_stop_read().is_err(),
            "ambiguous or over-bound lifecycle closes the connection"
        );
    }
}

fn issue(c: &mut Controller, prefix: &str) {
    let action = c.begin_stop_read().unwrap().expect("one stopped preflight");
    let Action::ToNative(raw) = action else {
        panic!("preflight must only read, never activate");
    };
    assert_eq!(
        serde_json::from_slice::<Value>(&raw).unwrap(),
        json!({
        "id":format!("{prefix}3"), "method":"thread/goal/get", "params":{"threadId":"thread-a"}})
    );
    assert_eq!(c.delivery(), Delivery::NotSent);
    assert!(
        c.begin_stop_read().unwrap().is_none(),
        "no repeated issuance"
    );
    assert!(
        c.stopped_ticket().is_none(),
        "an unanswered read grants nothing"
    );
}

fn user_read(c: &mut Controller) {
    ui(
        c,
        json!({"id":30,"method":"thread/goal/get","params":{"threadId":"thread-a"}}),
    );
    forwarded(
        c,
        json!({"id":30,"result":{"goal":snapshot("usageLimited")}}),
    );
}

fn denied(c: &mut Controller) {
    assert!(c.begin_stop_read().unwrap().is_none());
    assert!(c.stopped_ticket().is_none());
}

fn ordinary_forwarding(c: &mut Controller) {
    let raw = br#" {"id":90,"method":"account/read","params":{}} "#;
    assert!(matches!(c.ui_frame(raw).unwrap(), Action::ToNative(out) if out == raw.as_slice()));
    let raw = br#" {"id":90,"result":{"account":null}} "#;
    assert!(matches!(c.native_frame(raw).unwrap(), Action::ToUi(out) if out == raw.as_slice()));
}

#[test]
fn notice_read_response_yields_only_one_ownership_ticket_and_preserves_final_read() {
    let mut c = stopped_notice();
    issue(&mut c, PREFIX);
    assert!(c.take_next_episode_proof().unwrap().is_none());
    // Even a matching serial-1 reply cannot skip its separately authorized call.
    suppressed(&mut c, reply(PREFIX, 1, snapshot("usageLimited")));
    assert!(c.stopped_ticket().is_none());
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    assert_eq!(c.delivery(), Delivery::NotSent);
    assert!(c.take_next_episode_proof().unwrap().is_none());
    assert!(c.begin_stop_read().unwrap().is_none());
    // Duplicate replies are suppressed and cannot recreate a read or ticket.
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    let ticket = c.stopped_ticket().expect("correlated stopped ownership");
    assert_eq!(ticket.thread_id(), "thread-a");
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    denied(&mut c);

    // The pure adapter has no core permit. The real parent must independently
    // qualify Observe/Inspect before this call and Consume before its write.
    let Action::ToNative(raw) = c.begin_final_read(ticket).unwrap() else {
        panic!("the final read remains non-mutating");
    };
    assert_eq!(
        serde_json::from_slice::<Value>(&raw).unwrap(),
        json!({
        "id":format!("{PREFIX}1"), "method":"thread/goal/get", "params":{"threadId":"thread-a"}})
    );
    assert_eq!(c.delivery(), Delivery::Checking);
    let Action::ResumeNative(raw) = native(&mut c, reply(PREFIX, 1, snapshot("usageLimited")))
    else {
        panic!("activation still requires the final read");
    };
    assert_eq!(
        serde_json::from_slice::<Value>(&raw).unwrap(),
        json!({
        "id":format!("{PREFIX}2"), "method":"thread/goal/set",
        "params":{"threadId":"thread-a", "status":"active", "origin":"automatic"}})
    );
    assert_eq!(c.delivery(), Delivery::UnknownNoRetry);
    suppressed(&mut c, reply(PREFIX, 1, snapshot("usageLimited")));
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    denied(&mut c);
}

#[test]
fn callers_must_explicitly_start_preflight_and_the_ui_get_path_is_preserved() {
    let mut c = stopped_notice();
    assert_eq!(c.delivery(), Delivery::NotSent);
    assert!(c.stopped_ticket().is_none());
    user_read(&mut c);
    assert!(
        c.begin_stop_read().unwrap().is_none(),
        "UI read already sealed"
    );
    assert!(c.stopped_ticket().is_some());
    assert!(c.stopped_ticket().is_none());
}

#[test]
fn pending_ui_rpc_blocks_issuance_until_its_correlated_reply() {
    let mut c = stopped_notice();
    ui(&mut c, json!({"id":10,"method":"account/read","params":{}}));
    denied(&mut c);
    forwarded(&mut c, json!({"id":11,"result":{}}));
    denied(&mut c);
    forwarded(&mut c, json!({"id":10,"result":{}}));
    issue(&mut c, PREFIX);
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    assert!(c.stopped_ticket().is_some());
}

#[test]
fn no_notice_foreign_identity_and_untrusted_notifications_cannot_start_read() {
    let mut c = user_active();
    denied(&mut c);
    let mut invalid = vec![
        updated(snapshot("active")),
        updated(snapshot("paused")),
        updated(snapshot("completed")),
        updated(Value::Null),
    ];
    for field in ["threadId", "objective", "createdAt"] {
        let mut goal = snapshot("usageLimited");
        goal[field] = json!("foreign");
        invalid.push(updated(goal));
    }
    let mut foreign = updated(snapshot("usageLimited"));
    foreign["params"]["threadId"] = json!("thread-b");
    invalid.push(foreign);
    let mut request = updated(snapshot("usageLimited"));
    request["id"] = json!("native-request");
    invalid.push(request);
    for version in [json!("1.0"), json!(null), json!(2)] {
        let mut untrusted = updated(snapshot("usageLimited"));
        untrusted["jsonrpc"] = version;
        invalid.push(untrusted);
    }
    let mut wrong_method = updated(snapshot("usageLimited"));
    wrong_method["method"] = json!("ordinary");
    invalid.push(wrong_method);
    for frame in invalid {
        let mut c = user_active();
        forwarded(&mut c, frame);
        denied(&mut c);
    }
    // Explicit JSON-RPC 2.0 is a trusted notification envelope too.
    let mut c = user_active();
    let mut notice = updated(snapshot("usageLimited"));
    notice["jsonrpc"] = json!("2.0");
    forwarded(&mut c, notice);
    issue(&mut c, PREFIX);
}

#[test]
fn selection_and_correlated_user_activation_are_required() {
    for mut c in [Controller::new(PREFIX.into()).unwrap(), selected()] {
        forwarded(&mut c, updated(snapshot("usageLimited")));
        denied(&mut c);
    }
    for method in ["thread/goal/get", "thread/goal/set"] {
        let mut c = selected();
        ui(
            &mut c,
            json!({"id":2,"method":method,"params":{
            "threadId":"thread-a", "objective":"user objective", "status":"active", "origin":"automatic"}}),
        );
        forwarded(&mut c, json!({"id":2,"result":{"goal":snapshot("active")}}));
        forwarded(&mut c, updated(snapshot("usageLimited")));
        denied(&mut c);
    }
    let mut c = selected();
    ui(
        &mut c,
        json!({"id":2,"method":"thread/goal/set","params":{
        "threadId":"thread-a", "objective":"user objective", "origin":"user"}}),
    );
    forwarded(&mut c, updated(snapshot("active")));
    forwarded(&mut c, updated(snapshot("usageLimited")));
    denied(&mut c);
    forwarded(&mut c, json!({"id":2,"result":{"goal":snapshot("active")}}));
    denied(&mut c);
}

#[test]
fn every_ui_frame_after_issuance_vetoes_before_or_after_the_reply() {
    let frames = [
        json!({"id":10,"method":"thread/goal/set","params":{"threadId":"thread-a","status":"paused","origin":"user"}}),
        json!({"id":10,"method":"thread/goal/set","params":{"threadId":"thread-a","objective":"changed","origin":"user"}}),
        json!({"id":10,"method":"thread/goal/clear","params":{"threadId":"thread-a"}}),
        json!({"id":10,"method":"turn/start","params":{"threadId":"thread-a","input":[{"type":"text","text":"user input"}]}}),
        json!({"id":10,"method":"thread/resume","params":{"threadId":"thread-b"}}),
        json!({"id":10,"method":"thread/goal/get","params":{"threadId":"thread-a"}}),
        json!({"id":10,"method":"account/read","params":{}}),
        json!({"id":10,"method":"thread/start","params":{"ephemeral":true}}),
        json!({"id":10,"method":"thread/unsubscribe","params":{"threadId":"title-thread"}}),
        json!({"id":"approval","result":{"decision":"accept"}}),
        json!({"method":"ordinary","params":{}}),
    ];
    for completed in [false, true] {
        for frame in &frames {
            let mut c = stopped_notice();
            issue(&mut c, PREFIX);
            if completed {
                suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
            }
            ui(&mut c, frame.clone());
            // Draining pending UI RPCs cannot repair the sealed generation.
            if frame.get("method").is_some() && frame.get("id").is_some() {
                forwarded(
                    &mut c,
                    json!({"id":10,"result":{"thread":{"id":"thread-b"},"goal":snapshot("paused")}}),
                );
            }
            suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
            denied(&mut c);
            assert_eq!(c.delivery(), Delivery::Cancelled);
            assert!(c.take_next_episode_proof().is_err());
            ordinary_forwarding(&mut c);
        }
    }
}

#[test]
fn ui_after_ticket_revokes_the_unchanged_final_read() {
    let mut c = stopped_notice();
    issue(&mut c, PREFIX);
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    let ticket = c.stopped_ticket().unwrap();
    ui(&mut c, json!({"method":"ordinary","params":{}}));
    assert!(matches!(
        c.begin_final_read(ticket),
        Err(Error::AttemptConsumed)
    ));
    denied(&mut c);
    ordinary_forwarding(&mut c);
}

#[test]
fn native_changes_clear_candidate_and_invalidate_an_issued_snapshot() {
    let mut changes = vec![
        updated(snapshot("active")),
        updated(snapshot("paused")),
        updated(snapshot("completed")),
        updated(Value::Null),
    ];
    for field in ["threadId", "objective", "createdAt"] {
        let mut goal = snapshot("usageLimited");
        goal[field] = json!("changed");
        changes.push(updated(goal));
    }
    for method in [
        "thread/goal/cleared",
        "thread/archived",
        "thread/unsubscribed",
        "turn/started",
    ] {
        changes.push(json!({"method":method,"params":{"threadId":"thread-a"}}));
    }
    for issued in [false, true] {
        for frame in &changes {
            let mut c = stopped_notice();
            if issued {
                issue(&mut c, PREFIX);
            }
            forwarded(&mut c, frame.clone());
            if issued {
                suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
                assert_eq!(c.delivery(), Delivery::Cancelled);
                ordinary_forwarding(&mut c);
            }
            denied(&mut c);
        }
    }
    // Same identity with a changed stopped snapshot also revokes a pending read.
    for field in ["updatedAt", "tokenUsage", "extra"] {
        let mut c = stopped_notice();
        issue(&mut c, PREFIX);
        let mut changed = snapshot("usageLimited");
        changed[field] = json!("changed");
        forwarded(&mut c, updated(changed));
        suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
        assert_eq!(c.delivery(), Delivery::Cancelled);
        denied(&mut c);
    }
}

#[test]
fn mismatched_or_error_response_permanently_vetoes_automatic_work() {
    let mut invalid = vec![
        Value::Null,
        json!({}),
        snapshot("active"),
        snapshot("paused"),
        snapshot("completed"),
    ];
    for field in [
        "threadId",
        "objective",
        "createdAt",
        "updatedAt",
        "tokenUsage",
        "extra",
    ] {
        let mut goal = snapshot("usageLimited");
        goal[field] = json!("changed");
        invalid.push(goal);
    }
    let mut responses: Vec<Value> = invalid
        .into_iter()
        .map(|goal| reply(PREFIX, 3, goal))
        .collect();
    responses.push(json!({"id":format!("{PREFIX}3"),"error":{"code":-1}}));
    responses.push(json!({"id":format!("{PREFIX}3"),"result":{}}));
    for frame in responses {
        let mut c = stopped_notice();
        issue(&mut c, PREFIX);
        suppressed(&mut c, frame);
        assert_eq!(c.delivery(), Delivery::Cancelled);
        suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
        forwarded(&mut c, updated(snapshot("usageLimited")));
        user_read(&mut c);
        denied(&mut c);
        assert!(c.take_next_episode_proof().is_err());
        ordinary_forwarding(&mut c);
    }
}

#[test]
fn private_reply_before_issuance_cannot_be_replayed_as_correlation() {
    for mut c in [
        Controller::new(PREFIX.into()).unwrap(),
        user_active(),
        stopped_notice(),
    ] {
        suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
        assert_eq!(c.delivery(), Delivery::Cancelled);
        forwarded(&mut c, updated(snapshot("usageLimited")));
        suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
        denied(&mut c);
        ordinary_forwarding(&mut c);
    }
}

#[test]
fn cancel_and_disconnect_clear_candidate_and_read_without_retry() {
    for issued in [false, true] {
        let mut c = stopped_notice();
        if issued {
            issue(&mut c, PREFIX);
        }
        c.cancel_resume();
        suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
        denied(&mut c);
        ordinary_forwarding(&mut c);

        let mut c = stopped_notice();
        if issued {
            issue(&mut c, PREFIX);
        }
        c.disconnect();
        assert!(matches!(c.begin_stop_read(), Err(Error::Disconnected)));
        assert!(matches!(
            c.native_frame(
                reply(PREFIX, 3, snapshot("usageLimited"))
                    .to_string()
                    .as_bytes()
            ),
            Err(Error::Disconnected)
        ));
        assert!(c.stopped_ticket().is_none());
    }
}

fn rearmed() -> Controller {
    let mut c = stopped_notice();
    issue(&mut c, PREFIX);
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    let ticket = c.stopped_ticket().unwrap();
    assert!(matches!(
        c.begin_final_read(ticket).unwrap(),
        Action::ToNative(_)
    ));
    assert!(matches!(
        native(&mut c, reply(PREFIX, 1, snapshot("usageLimited"))),
        Action::ResumeNative(_)
    ));
    suppressed(&mut c, reply(PREFIX, 2, snapshot("active")));
    for (method, status) in [
        ("turn/started", "inProgress"),
        ("turn/completed", "completed"),
    ] {
        forwarded(
            &mut c,
            json!({"method":method,"params":{"threadId":"thread-a","turn":{"id":"fresh-turn","status":status}}}),
        );
    }
    let proof = c
        .take_next_episode_proof()
        .unwrap()
        .expect("ACK and fresh progress");
    // Native evidence is consumed here; the parent separately owns core Advance.
    c.rearm(proof, NEXT.into()).unwrap();
    c
}

#[test]
fn rearm_resets_slots_and_retired_serial_three_is_only_a_suppressed_reply() {
    let mut c = rearmed();
    for serial in [1, 2, 3, 3] {
        suppressed(&mut c, reply(PREFIX, serial, snapshot("paused")));
    }
    suppressed(
        &mut c,
        json!({"id":format!("{PREFIX}3"),"error":{"code":-1}}),
    );
    assert_eq!(c.delivery(), Delivery::NotSent);
    denied(&mut c);
    forwarded(&mut c, updated(snapshot("usageLimited")));
    issue(&mut c, NEXT);
    suppressed(&mut c, reply(PREFIX, 3, snapshot("usageLimited")));
    assert!(
        c.stopped_ticket().is_none(),
        "old reply cannot finish the new read"
    );
    suppressed(&mut c, reply(NEXT, 3, snapshot("usageLimited")));
    assert!(c.stopped_ticket().is_some());
    denied(&mut c);
}

#[test]
fn retired_unknown_suffixes_and_private_method_or_ui_collisions_poison_transport() {
    for frame in [
        json!({"id":format!("{PREFIX}unknown"),"result":{}}),
        json!({"id":format!("{PREFIX}4"),"result":{}}),
        json!({"id":format!("{PREFIX}3"),"method":"ordinary","params":{}}),
        json!({"id":format!("{NEXT}3"),"method":"ordinary","params":{}}),
    ] {
        let mut c = rearmed();
        assert!(matches!(
            c.native_frame(frame.to_string().as_bytes()),
            Err(Error::NamespaceCollision)
        ));
        assert!(matches!(c.begin_stop_read(), Err(Error::Disconnected)));
    }
    for prefix in [PREFIX, NEXT] {
        let mut c = rearmed();
        let frame = json!({"id":format!("{prefix}3"),"method":"ordinary","params":{}});
        assert!(matches!(
            c.ui_frame(frame.to_string().as_bytes()),
            Err(Error::NamespaceCollision)
        ));
        assert!(matches!(c.begin_stop_read(), Err(Error::Disconnected)));
    }
}

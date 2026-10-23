//! Pure native-notification projection: no capture, native process or authority.
#![cfg(all(feature = "testkit", debug_assertions))]
#![allow(dead_code)] // Standalone path inclusion also brings the capture API.

#[path = "../src/native_resume/observation.rs"]
mod observation;

use observation::classify_native_notification;
use serde_json::{Value, json};

const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";

fn project(value: Value) -> Option<Value> {
    let frame = serde_json::to_vec(&value).unwrap();
    classify_native_notification(&frame).map(|event| serde_json::to_value(event).unwrap())
}

#[test]
fn core_observation_rejection_exports_only_the_fixed_protocol_reason() {
    use qb_proto::native_reset::Refusal;
    for (reason, expected) in [
        (Refusal::Invalid, "invalid"),
        (Refusal::Unavailable, "unavailable"),
        (Refusal::NotDue, "not_due"),
        (Refusal::Cancelled, "cancelled"),
    ] {
        assert_eq!(
            serde_json::to_value(observation::Event::ObservationRejected { reason }).unwrap(),
            json!({"event":"observation_rejected", "reason":expected})
        );
    }
    assert_eq!(
        serde_json::to_value(observation::Event::ObservationFailed).unwrap(),
        json!({"event":"observation_failed"})
    );
}

fn completed(status: Value) -> Value {
    json!({"method":"turn/completed", "params":{
        "threadId":UUID, "turn":{"id":"private-turn-id", "status":status,
            "items":[{"text":"private-conversation"}],
            "error":{"message":"private-error"}}
    }})
}

#[test]
fn goal_notifications_export_only_known_state_and_canonical_thread() {
    for (status, expected) in [
        ("active", "active"),
        ("paused", "paused"),
        ("usageLimited", "usage_limited"),
    ] {
        assert_eq!(
            project(json!({"method":"thread/goal/updated","params":{
            "threadId":UUID,"goal":{"status":status,"objective":"private-goal",
                "tokens":99,"extra":"private-value"}}})),
            Some(json!({"event":"native_goal_updated","thread_id":UUID,"state":expected}))
        );
    }
    for status in [json!("unknown"), json!(true), Value::Null, json!({})] {
        assert!(
            project(json!({"method":"thread/goal/updated","params":{
            "threadId":UUID,"goal":{"status":status}}}))
            .is_none()
        );
    }
    assert!(
        project(json!({"method":"thread/goal/updated","params":{
        "threadId":"private-invalid-id","goal":{"status":"paused"}}}))
        .is_none()
    );
}

#[test]
fn native_thread_started_serializes_only_fixed_event_and_canonical_uuid() {
    for id in [UUID, "0123456789abcdef0123456789abcdef"] {
        let value = json!({"method":"thread/started", "params":{"thread":{
            "id":id, "preview":"private-conversation", "cwd":"private-path",
            "goal":"private-goal", "credentials":"private-credentials"
        }}});
        assert_eq!(
            project(value),
            Some(json!({"event":"native_thread_started", "thread_id":id}))
        );
    }
}

#[test]
fn native_turn_completed_maps_only_known_terminal_statuses_to_boolean() {
    for (status, successful) in [
        ("completed", true),
        ("failed", false),
        ("interrupted", false),
    ] {
        assert_eq!(
            project(completed(json!(status))),
            Some(json!({"event":"native_turn_completed", "thread_id":UUID,
                "successful":successful}))
        );
    }
    for status in [
        json!("inProgress"),
        json!("unknown"),
        json!("Completed"),
        json!(""),
        Value::Null,
        json!(true),
        json!(1),
        json!({}),
        json!([]),
    ] {
        assert!(project(completed(status)).is_none());
    }
}

#[test]
fn requests_responses_wrong_methods_and_invalid_shapes_are_ignored() {
    for value in [
        Value::Null,
        json!([]),
        json!(7),
        json!("private"),
        json!({}),
        json!({"method":"thread/started"}),
        json!({"method":"thread/started", "params":null}),
        json!({"method":"thread/started", "params":{"thread":UUID}}),
        json!({"method":"thread/started", "params":{"thread":{"id":7}}}),
        json!({"method":"thread/started", "params":{"threadId":UUID}}),
        json!({"method":"turn/completed", "params":{"threadId":UUID}}),
        json!({"method":"turn/completed", "params":{"thread":{"id":UUID},
            "turn":{"status":"completed"}}}),
        json!({"method":"turn/completed", "params":{"threadId":UUID, "turn":[]}}),
        json!({"method":"turn/completed", "params":{"threadId":UUID, "turn":{}}}),
        json!({"method":"turn/started", "params":{"threadId":UUID,
            "turn":{"status":"completed"}}}),
        json!({"id":1, "result":{"thread":{"id":UUID}}}),
    ] {
        assert!(project(value).is_none());
    }
    for id in [Value::Null, json!(0), json!("private-request-id")] {
        let mut value = completed(json!("completed"));
        value["id"] = id;
        assert!(project(value).is_none());
    }
    for version in [Value::Null, json!(2), json!("1.0")] {
        let mut value = completed(json!("completed"));
        value["jsonrpc"] = version;
        assert!(project(value).is_none());
    }
    let mut standard = completed(json!("completed"));
    standard["jsonrpc"] = json!("2.0");
    assert!(project(standard).is_some());
}

#[test]
fn invalid_ids_never_escape_either_notification_path() {
    for id in [
        Value::Null,
        json!(7),
        json!(""),
        json!("private-goal"),
        json!("Bearer private-credentials"),
        json!("01234567-89AB-CDEF-0123-456789ABCDEF"),
        json!("01234567-89ab-cdef-0123-456789abcdeg"),
        json!("012345678-9ab-cdef-0123-456789abcdef"),
        json!(format!("{UUID}\n")),
    ] {
        assert!(
            project(json!({"method":"thread/started", "params":{
            "thread":{"id":id.clone()}}}))
            .is_none()
        );
        let mut turn = completed(json!("completed"));
        turn["params"]["threadId"] = id;
        assert!(project(turn).is_none());
    }
}

#[test]
fn malformed_trailing_invalid_utf8_and_deep_json_are_ignored() {
    for frame in [
        &b""[..],
        &b"{"[..],
        &b"\xff"[..],
        &b"{\"method\":\"thread/started\",\"params\":"[..],
    ] {
        assert!(classify_native_notification(frame).is_none());
    }
    let mut trailing = serde_json::to_vec(&completed(json!("completed"))).unwrap();
    trailing.extend_from_slice(b" {}");
    assert!(classify_native_notification(&trailing).is_none());
    let deep = format!("{}0{}", "[".repeat(256), "]".repeat(256));
    assert!(classify_native_notification(deep.as_bytes()).is_none());
}

#[test]
fn exact_one_mib_frame_is_accepted_but_one_byte_more_is_ignored() {
    let mut frame = serde_json::to_vec(&completed(json!("completed"))).unwrap();
    frame.resize(1024 * 1024, b' ');
    assert!(classify_native_notification(&frame).is_some());
    frame.push(b' ');
    assert!(classify_native_notification(&frame).is_none());
}

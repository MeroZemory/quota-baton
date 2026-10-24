use qb_proto::native_reset::{Command, Event, Reply, Request, VERSION};
use serde_json::json;

#[test]
fn requests_round_trip_without_accepting_goal_text_or_caller_authority() {
    for command in [
        json!({"type":"observe", "thread_id":"synthetic-thread"}),
        json!({"type":"consume", "episode":7}),
        json!({"type":"inspect", "episode":7}),
        json!({"type":"advance", "episode":7}),
        json!({"type":"cancel", "episode":7}),
    ] {
        let wire = json!({"version":VERSION,"request_id":1,"command":command});
        let request: Request = serde_json::from_value(wire.clone()).unwrap();
        assert!(request.valid_shape());
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
    }
    for wire in [
        json!({"version":VERSION,"request_id":1,"command":{"type":"observe", "thread_id":"a", "objective":"synthetic"}}),
        json!({"version":VERSION,"request_id":1,"command":{"type":"consume", "episode":7,"reset_unix_ms":123}}),
        json!({"version":VERSION,"request_id":1,"command":{"type":"advance", "episode":7,"next_episode":8}}),
        json!({"version":VERSION,"request_id":1,"command":{"type":"resume","episode":7}}),
        json!({"version":VERSION,"request_id":1,"command":{"type":"cancel","episode":7},"authorized":true}),
    ] {
        assert!(serde_json::from_value::<Request>(wire).is_err());
    }
}

#[test]
fn invalid_request_envelopes_are_never_shape_valid() {
    for request in [
        Request {
            version: VERSION + 1,
            request_id: 1,
            command: Command::Consume { episode: 1 },
        },
        Request {
            version: VERSION,
            request_id: 0,
            command: Command::Consume { episode: 1 },
        },
        Request {
            version: VERSION,
            request_id: 1,
            command: Command::Cancel { episode: 0 },
        },
        Request {
            version: VERSION,
            request_id: 1,
            command: Command::Advance { episode: 0 },
        },
        Request {
            version: VERSION,
            request_id: 1,
            command: Command::Observe {
                thread_id: "a".repeat(257),
            },
        },
        Request {
            version: VERSION,
            request_id: 1,
            command: Command::Observe {
                thread_id: "a\nb".into(),
            },
        },
    ] {
        assert!(!request.valid_shape());
    }
}

#[test]
fn hints_and_other_request_or_episode_grants_never_admit_activation() {
    for event in [
        json!({"type":"waiting", "episode":7,"reset_unix_ms":123}),
        json!({"type":"due", "episode":7}),
        json!({"type":"ready", "episode":7}),
        json!({"type":"pending", "episode":7}),
        json!({"type":"advanced", "episode":7}),
        json!({"type":"cancelled", "episode":7}),
        json!({"type":"refused", "reason":"not_due"}),
    ] {
        let wire = json!({"version":VERSION,"request_id":1,"event":event});
        let reply: Reply = serde_json::from_value(wire.clone()).unwrap();
        assert!(reply.valid_shape());
        assert!(!reply.grants(1, 7));
        assert_eq!(serde_json::to_value(&reply).unwrap(), wire);
    }
    let reply = Reply {
        version: VERSION,
        request_id: Some(1),
        event: Event::Granted { episode: 7 },
    };
    assert!(reply.grants(1, 7));
    assert!(!reply.grants(2, 7));
    assert!(!reply.grants(1, 8));
    assert!(!reply.grants(0, 7));
}

#[test]
fn advanced_is_only_a_correlated_prior_episode_receipt() {
    for request_id in [None, Some(0), Some(1)] {
        for episode in [0, 7] {
            let reply = Reply {
                version: VERSION,
                request_id,
                event: Event::Advanced { episode },
            };
            assert_eq!(reply.valid_shape(), request_id == Some(1) && episode == 7);
            assert!(!reply.grants(1, 7));
            assert!(!reply.grants(1, 8));
        }
    }
    assert!(serde_json::from_value::<Reply>(json!({"version":VERSION,"request_id":1,"event":{"type":"advanced","episode":7,"next_episode":8}})).is_err());
}

#[test]
fn pending_is_correlated_nonzero_and_never_an_activation_grant() {
    for request_id in [None, Some(0), Some(1)] {
        for episode in [0, 7] {
            let reply = Reply {
                version: VERSION,
                request_id,
                event: Event::Pending { episode },
            };
            assert_eq!(reply.valid_shape(), request_id == Some(1) && episode == 7);
            assert!(!reply.grants(1, 7));
        }
    }
    let old = Reply {
        version: VERSION - 1,
        request_id: Some(1),
        event: Event::Ready { episode: 7 },
    };
    assert!(!old.valid_shape());
}

#[test]
fn uncorrelated_grants_unknown_events_and_future_versions_are_refused() {
    let reply = Reply {
        version: VERSION,
        request_id: None,
        event: Event::Granted { episode: 7 },
    };
    assert!(!reply.valid_shape());
    assert!(!reply.grants(1, 7));
    let reply = Reply {
        version: VERSION + 1,
        request_id: Some(1),
        event: Event::Granted { episode: 7 },
    };
    assert!(!reply.grants(1, 7));
    assert!(
        serde_json::from_value::<Reply>(
            json!({"version":VERSION,"request_id":1,"event":{"type":"resume_now","episode":7}})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<Reply>(json!({"version":VERSION,"request_id":1,"event":{"type":"granted","episode":7,"account":"synthetic"}})).is_err());
}

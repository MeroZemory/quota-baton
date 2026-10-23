//! Staged harness observer tests. No native process, runtime or authority.
#![cfg(all(feature = "testkit", debug_assertions))]

#[path = "../src/native_resume/observation.rs"]
mod observation;

use std::{fs, path::Path};

use observation::{Capture, Error, Event, ThreadId, emit};

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic observer operation failed"),
    }
}

#[test]
fn capture_is_explicit_new_sandbox_only_once_and_finish_joins_final_summary() {
    let sandbox = require(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(".sandbox")
            .canonicalize(),
    );
    let directory = require(
        tempfile::Builder::new()
            .prefix("native-observer-")
            .tempdir_in(&sandbox),
    );
    let path = directory.path().join("events.jsonl");
    emit(Event::Entry);
    assert!(!path.exists());

    // Validation errors create no destination and do not consume the one slot.
    for invalid in [
        directory.path().join("events.txt"),
        directory.path().join("absent").join("events.jsonl"),
        directory.path().join("CON.jsonl"),
        directory.path().join("events:stream.jsonl"),
        Path::new("relative-observer.jsonl").to_path_buf(),
        sandbox.parent().unwrap().join(format!(
            "{}.jsonl",
            directory.path().file_name().unwrap().to_str().unwrap()
        )),
    ] {
        assert!(matches!(Capture::start(&invalid), Err(Error::InvalidPath)));
    }
    assert_eq!(require(fs::read_dir(directory.path())).count(), 0);
    let existing = directory.path().join("existing.jsonl");
    require(fs::write(&existing, b"preserve-existing"));
    assert!(matches!(Capture::start(&existing), Err(Error::Open)));
    assert_eq!(require(fs::read(&existing)), b"preserve-existing");

    let capture = require(Capture::start(&path));
    assert!(!capture.lost());
    // Invalid/secret-shaped IDs cannot become an Event and emit no record.
    for invalid in [
        "Bearer synthetic-private-token",
        "https://127.0.0.1/secret-token",
        "objective: do private work",
        "qb-goal-0123456789abcdef0123456789abcdef:",
        "01234567-89ab-cdef-0123-456789abcdeg",
    ] {
        assert!(matches!(
            ThreadId::parse(invalid),
            Err(Error::InvalidThread)
        ));
    }
    emit(Event::Entry);
    emit(Event::StoppedThread {
        thread_id: require(ThreadId::parse("01234567-89ab-cdef-0123-456789abcdef")),
    });
    emit(Event::Cleanup);
    assert!(!capture.lost());
    require(capture.finish());

    // Successful consuming finish includes final flush/join, not just summary.
    let bytes = require(fs::read(&path));
    assert!(bytes.len() <= observation::MAX_OUTPUT_BYTES);
    let rows: Vec<serde_json::Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|row| !row.is_empty())
        .map(|row| require(serde_json::from_slice(row)))
        .collect();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["event"], "entry");
    assert_eq!(rows[1]["thread_id"], "01234567-89ab-cdef-0123-456789abcdef");
    assert_eq!(rows[2]["event"], "cleanup");
    assert_eq!(
        rows[3],
        serde_json::json!({"event":"summary","loss":false,"received":3,"written":3})
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("private"));
    emit(Event::ActivationWritten);
    assert_eq!(require(fs::read(&path)), bytes);
    let second = directory.path().join("second.jsonl");
    assert!(matches!(
        Capture::start(&second),
        Err(Error::AlreadyStarted)
    ));
    assert!(!second.exists());
}

#[test]
fn fixed_event_schema_has_only_typed_ids_and_booleans() {
    let events = [
        Event::Entry,
        Event::ChildrenSpawned,
        Event::EndpointAccepted,
        Event::ParentEntered,
        Event::Cleanup,
        Event::StoppedThread {
            thread_id: require(ThreadId::parse("0123456789abcdef0123456789abcdef")),
        },
        Event::Observed { episode: u64::MAX },
        Event::Inspected {
            episode: 7,
            ready: false,
        },
        Event::Consumed {
            episode: 7,
            granted: false,
        },
        Event::ActivationWritten,
        Event::ActivationAcknowledged,
    ];
    for event in events {
        let line = require(serde_json::to_vec(&event));
        assert!(line.len() < 256);
        let value: serde_json::Value = require(serde_json::from_slice(&line));
        assert!(value["event"].is_string());
        for key in value.as_object().unwrap().keys() {
            assert!(matches!(
                key.as_str(),
                "event" | "thread_id" | "episode" | "ready" | "granted"
            ));
        }
    }
}

#[test]
fn only_canonical_lowercase_uuid32_or_uuid36_is_representable() {
    for valid in [
        "00000000000000000000000000000000",
        "0123456789abcdef0123456789abcdef",
        "01234567-89ab-cdef-0123-456789abcdef",
    ] {
        let thread = require(ThreadId::parse(valid));
        assert_eq!(require(serde_json::to_value(thread)), valid);
    }
    for invalid in [
        "",
        "01234567-89AB-CDEF-0123-456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "012345678-9ab-cdef-0123-456789abcdef",
        "01234567-89ab-cdef-0123-456789abcde\n",
        "01234567-89ab-cdef-0123-456789abcde\"",
        "sk-synthetic-never-observe-token",
    ] {
        assert!(matches!(
            ThreadId::parse(invalid),
            Err(Error::InvalidThread)
        ));
    }
}

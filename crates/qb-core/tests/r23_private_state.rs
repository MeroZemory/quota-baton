use qb_core::{
    bindings::BindingTable,
    config::RoutingConfig,
    routing::{PolicyStore, Project, Settings},
};
use std::{fs, path::Path};

#[cfg(unix)]
#[path = "r23_private_state/unix.rs"]
mod native;
#[cfg(windows)]
#[path = "r23_private_state/windows.rs"]
mod native;

fn home() -> tempfile::TempDir {
    // A physical project-local parent avoids testing the disposable OS temp
    // namespace or an implementation-target junction as the data home.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r23-private-state");
    fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(fs::canonicalize(root).unwrap()).unwrap();
    native::prepare_home(home.path());
    native::assert_private(home.path());
    home
}

#[test]
fn policy_and_binding_constructors_persist_without_precreated_state() {
    let policy_home = home();
    assert!(!policy_home.path().join("state").exists());
    let policy = PolicyStore::new(policy_home.path(), Settings::default()).unwrap();
    policy
        .put_project(
            None,
            Project {
                name: "synthetic-private".into(),
                settings: Settings::default(),
            },
        )
        .unwrap();
    assert!(
        policy_home
            .path()
            .join("state/routing-policies.json")
            .is_file()
    );
    native::assert_private(&policy_home.path().join("state"));

    let binding_home = home();
    assert!(!binding_home.path().join("state").exists());
    let table = BindingTable::new(binding_home.path(), RoutingConfig::default()).unwrap();
    let snapshot = table.export().unwrap();
    table.import(snapshot.clone()).unwrap();
    table.flush().unwrap();
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(binding_home.path().join("state/bindings.json")).unwrap())
            .unwrap();
    assert_eq!(stored, snapshot);
    native::assert_private(&binding_home.path().join("state"));
}

#[test]
fn nested_data_creation_and_existing_private_paths_are_idempotent() {
    let home = home();
    let leaf = home.path().join("state/forecast-log/epochs");
    qb_core::ensure_private_dir_all(&leaf).unwrap();
    qb_core::ensure_private_dir_all(&leaf).unwrap();
    for path in [
        home.path().join("state"),
        home.path().join("state/forecast-log"),
        leaf,
    ] {
        native::assert_private(&path);
    }
}

#[test]
fn exclusive_private_leaf_never_adopts_existing_entries() {
    let home = home();
    let leaf = home.path().join("epoch");
    qb_core::create_private_directory_new(&leaf).unwrap();
    native::assert_private(&leaf);
    assert_eq!(
        qb_core::create_private_directory_new(&leaf)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    let record = home.path().join("record");
    fs::write(&record, b"unchanged").unwrap();
    assert_eq!(
        qb_core::create_private_directory_new(&record)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&record).unwrap(), b"unchanged");
    assert!(qb_core::create_private_directory_new(&home.path().join("missing/leaf")).is_err());
    assert!(!home.path().join("missing").exists());
}

#[test]
fn real_observation_and_epoch_writers_create_private_data_directories() {
    use qb_core::observability::replay_recorder::{BuildIdentity, Provenance, Recorder, Stamp};
    let home = home();
    let observer = qb_core::observability::Observability::new(home.path(), None).unwrap();
    native::assert_private(&home.path().join("state"));
    native::assert_private(&home.path().join("ledger"));
    let tape = home.path().join("state/tape");
    for offset in 0..2 {
        let (recorder, archive) = Recorder::enable_opt_in(
            &tape,
            BuildIdentity::default(),
            Provenance::Synthetic,
            Stamp {
                utc_ns: 1_790_000_000_000_000_000 + offset,
                monotonic_ns: offset as u64,
            },
        )
        .unwrap();
        native::assert_private(&tape);
        for entry in fs::read_dir(&tape).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                native::assert_private(&entry.path());
            }
        }
        recorder.disable();
        drop(archive);
    }
    drop(observer);
}

#[test]
fn user_created_loose_state_is_refused_without_repair() {
    let home = home();
    let state = home.path().join("state");
    native::create_loose(&state);
    let error = qb_core::ensure_private_dir_all(&state).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("private"));
    assert!(qb_core::ensure_private_dir_all(&state.join("child")).is_err());
    assert!(!state.join("child").exists());
    native::assert_loose(&state);
    let error = PolicyStore::new(home.path(), Settings::default())
        .err()
        .unwrap();
    assert!(error.to_string().contains("private"));
    assert!(BindingTable::new(home.path(), RoutingConfig::default()).is_err());
    native::assert_loose(&state);

    let nested_home = crate::home();
    let private_state = nested_home.path().join("state");
    qb_core::ensure_private_dir_all(&private_state).unwrap();
    let loose_child = private_state.join("child");
    native::create_loose(&loose_child);
    assert!(qb_core::ensure_private_dir_all(&loose_child).is_err());
    assert!(qb_core::ensure_private_dir_all(&loose_child.join("grandchild")).is_err());
    assert!(!loose_child.join("grandchild").exists());
    native::assert_loose(&loose_child);
    native::assert_private(&private_state);
}

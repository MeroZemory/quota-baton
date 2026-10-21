//! Synthetic metadata only; never consult a user's home or configuration.
use super::*;

const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";
const C: &str = "33333333-3333-3333-3333-333333333333";
const D: &str = "44444444-4444-4444-4444-444444444444";

fn home() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r9-compressed-tests");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(fs::canonicalize(root).unwrap()).unwrap()
}

fn name(home: &Path, physical: &str, compressed: bool) -> PathBuf {
    home.join("sessions").join(format!(
        "rollout-test-{physical}.jsonl{}",
        if compressed { ".zst" } else { "" }
    ))
}

fn record(thread: &str, base: Option<&str>, parent: Option<&str>) -> String {
    let value = serde_json::json!({"type":"session_meta","payload":{
        "id":thread,"model_provider":"openai","forked_from_id":parent,
        "history_base":base.map(|id| serde_json::json!({"thread_id":id}))
    }});
    format!("{value}\n")
}

fn write_bytes(home: &Path, physical: &str, bytes: &[u8], compressed: bool) -> PathBuf {
    let path = name(home, physical, compressed);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = if compressed {
        zstd::stream::encode_all(bytes, 1).unwrap()
    } else {
        bytes.to_vec()
    };
    fs::write(&path, bytes).unwrap();
    path
}

fn write(
    home: &Path,
    thread: &str,
    physical: &str,
    base: Option<&str>,
    parent: Option<&str>,
    compressed: bool,
) -> PathBuf {
    write_bytes(
        home,
        physical,
        record(thread, base, parent).as_bytes(),
        compressed,
    )
}

fn corrupt(home: &Path, physical: &str) -> PathBuf {
    let path = name(home, physical, true);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"SYNTHETIC-PRIVATE-NOT-ZSTD").unwrap();
    path
}

fn ids(id: &str) -> BTreeSet<String> {
    BTreeSet::from([id.to_owned()])
}

#[test]
fn compressed_root_is_a_normal_selectable_rollout_without_body_retention() {
    let home = home();
    let mut bytes = record(A, None, None).into_bytes();
    // Invalid UTF-8 after the first line must never be parsed as metadata.
    bytes.extend_from_slice(b"SYNTHETIC-PRIVATE-BODY\xff\n");
    let path = write_bytes(home.path(), D, &bytes, true);
    let found = catalog(home.path()).unwrap();
    assert_eq!(physical_id(&path).as_deref(), Some(D));
    assert_eq!(found.compressed.as_slice(), std::slice::from_ref(&path));
    assert!(found.unreadable.is_empty());
    let chain = found.chain(A).unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].physical, D);
    assert_eq!(chain[0].path, path);
    assert_eq!(chain[0].bytes, fs::metadata(&path).unwrap().len());
    assert!(
        !serde_json::to_string(&chain)
            .unwrap()
            .contains("SYNTHETIC-PRIVATE")
    );
    assert!(!home.path().join("state_5.sqlite").exists());
}

#[test]
fn mixed_plain_compressed_fork_and_physical_history_are_transitive() {
    let home = home();
    write(home.path(), A, D, None, None, true);
    write(home.path(), B, B, Some(D), None, false);
    write(home.path(), C, C, None, Some(B), true);
    let found = catalog(home.path()).unwrap();
    assert_eq!(found.descendants(A), BTreeSet::from([B.into(), C.into()]));
    assert_eq!(
        found.related_ids(&ids(A)),
        BTreeSet::from([A.into(), B.into(), C.into(), D.into()])
    );
    assert_eq!(
        found
            .chain(C)
            .unwrap()
            .iter()
            .map(|r| r.thread.as_str())
            .collect::<Vec<_>>(),
        [A, B, C]
    );
}

#[test]
fn unrelated_corrupt_and_unknown_filename_do_not_abort_catalog_or_chain() {
    let home = home();
    write(home.path(), A, A, None, None, false);
    let bad = corrupt(home.path(), B);
    let unknown = home.path().join("sessions/unknown.jsonl.zst");
    fs::write(&unknown, b"invalid").unwrap();
    let found = catalog(home.path()).unwrap();
    assert_eq!(found.unreadable.len(), 2);
    assert!(
        found
            .unreadable
            .iter()
            .any(|r| r.path == bad && r.physical.as_deref() == Some(B))
    );
    assert!(
        found
            .unreadable
            .iter()
            .any(|r| r.path == unknown && r.physical.is_none())
    );
    assert!(found.ensure_readable_for(&ids(A)).is_ok());
    assert!(found.chain(A).is_ok());
    let refusal = found.ensure_readable_for(&ids(B)).unwrap_err();
    assert_eq!(
        crate::sessions::friendly_error(&refusal).as_deref(),
        Some("관련된 압축 대화 기록을 읽을 수 없습니다. doctor로 확인하세요")
    );
    let error = refusal.to_string();
    assert!(error.contains(B));
    assert!(!error.contains("SYNTHETIC-PRIVATE"));
}

#[test]
fn corrupt_physical_ancestor_and_logical_fork_parent_are_related() {
    for history in [true, false] {
        let home = home();
        corrupt(home.path(), D);
        write(
            home.path(),
            A,
            A,
            history.then_some(D),
            (!history).then_some(D),
            true,
        );
        let found = catalog(home.path()).unwrap();
        let error = found.ensure_readable_for(&ids(A)).unwrap_err().to_string();
        assert!(error.contains(D));
        assert!(found.chain(A).is_err());
    }
}

#[test]
fn caller_row_ids_and_cross_home_graph_can_identify_unreadable_descendants() {
    let root = home();
    let other = home();
    write(root.path(), A, A, None, None, false);
    write(root.path(), B, D, None, Some(A), true);
    corrupt(other.path(), D);
    let first = catalog(root.path()).unwrap();
    let second = catalog(other.path()).unwrap();
    assert!(second.ensure_readable_for(&ids(A)).is_ok());
    let related = first.related_ids(&ids(A));
    assert!(second.ensure_readable_for(&related).is_err());
    // A row can independently establish that D belongs to the moving thread.
    assert!(
        second
            .ensure_readable_for(&BTreeSet::from([A.into(), D.into()]))
            .is_err()
    );
}

#[test]
fn bomb_truncated_nonmetadata_and_invalid_ids_are_safe_read_failures() {
    let home = home();
    let mut oversized = record(A, None, None).trim_end().as_bytes().to_vec();
    oversized.resize(HEADER_LIMIT as usize, b' ');
    oversized.push(b'\n');
    write_bytes(home.path(), A, &oversized, true);
    let truncated = write(home.path(), B, B, None, None, true);
    let bytes = fs::read(&truncated).unwrap();
    fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    let nonmeta = format!(
        "{{\"type\":\"event_msg\",\"payload\":\"SYNTHETIC-PRIVATE\"}}\n{}",
        record(C, None, None)
    );
    write_bytes(home.path(), C, nonmeta.as_bytes(), true);
    write_bytes(
        home.path(),
        D,
        record("invalid", None, None).as_bytes(),
        true,
    );
    let found = catalog(home.path()).unwrap();
    assert!(found.rollouts.is_empty());
    assert_eq!(found.unreadable.len(), 4);
    for id in [A, B, C, D] {
        assert!(found.ensure_readable_for(&ids(id)).is_err());
    }
}

#[test]
fn exact_four_mib_first_line_is_allowed_for_plain_and_compressed() {
    for compressed in [false, true] {
        let home = home();
        let mut bytes = record(A, None, None).trim_end().as_bytes().to_vec();
        bytes.resize(HEADER_LIMIT as usize - 1, b' ');
        bytes.push(b'\n');
        write_bytes(home.path(), A, &bytes, compressed);
        assert!(catalog(home.path()).unwrap().chain(A).is_ok());
    }
}

#[test]
fn missing_newline_and_malformed_reference_are_unreadable() {
    let home = home();
    write_bytes(
        home.path(),
        A,
        record(A, None, None).trim_end().as_bytes(),
        true,
    );
    write_bytes(
        home.path(),
        B,
        record(B, Some("bad-reference"), None).as_bytes(),
        true,
    );
    let found = catalog(home.path()).unwrap();
    assert_eq!(found.unreadable.len(), 2);
    assert!(found.rollouts.is_empty());
}

#[test]
fn complete_header_frame_does_not_require_or_decode_later_frames() {
    let home = home();
    let path = write(home.path(), A, A, None, None, true);
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"SYNTHETIC-CORRUPT-BODY-FRAME");
    fs::write(path, bytes).unwrap();
    assert!(catalog(home.path()).unwrap().chain(A).is_ok());
}

#[cfg(windows)]
#[test]
fn compressed_hardlink_alias_is_retained_and_sqlite_can_select_it() {
    let home = home();
    let path = write(home.path(), A, D, None, None, true);
    let alias = home
        .path()
        .join("archived_sessions")
        .join(path.file_name().unwrap());
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    fs::hard_link(path, alias).unwrap();
    let found = catalog(home.path()).unwrap();
    assert_eq!(found.all().count(), 2);
    assert_eq!(found.aliases.len(), 1);
    assert!(found.conflicts.is_empty());
    let selected = &found.aliases[0].path;
    let db = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL);")
        .unwrap();
    db.execute(
        "INSERT INTO threads VALUES(?1, ?2)",
        rusqlite::params![A, selected.to_str().unwrap()],
    )
    .unwrap();
    assert_eq!(
        &catalog(home.path()).unwrap().active(A).unwrap().path,
        selected
    );
}

#[cfg(windows)]
#[test]
fn compressed_plain_duplicate_physical_id_is_not_a_fileid_alias() {
    let home = home();
    write(home.path(), A, A, None, None, false);
    write(home.path(), A, A, None, None, true);
    write(home.path(), B, B, None, None, true);
    let found = catalog(home.path()).unwrap();
    assert_eq!(found.conflicts, ids(A));
    assert_eq!(found.all().count(), 3);
    assert_eq!(found.threads[A], [A]);
    for error in [
        found.ensure_readable_for(&ids(A)).unwrap_err(),
        found.active(A).err().unwrap(),
        found.chain(A).err().unwrap(),
    ] {
        assert!(error.is::<PhysicalConflict>());
        assert_eq!(
            error.to_string(),
            format!("같은 대화 기록 ID가 서로 다른 파일에 중복되어 있습니다: {A}")
        );
    }
    assert!(found.ensure_readable_for(&ids(B)).is_ok());
    assert_eq!(found.chain(B).unwrap().len(), 1);
}

#[cfg(windows)]
#[test]
fn physical_conflicts_retain_all_names_threads_and_refuse_only_related_selections() {
    for compressed in [false, true] {
        for archived in [false, true] {
            for other_thread in [A, B] {
                let home = home();
                let primary = write(home.path(), A, D, None, None, compressed);
                let duplicate = home
                    .path()
                    .join(if archived {
                        "archived_sessions"
                    } else {
                        "sessions/duplicate"
                    })
                    .join(primary.file_name().unwrap());
                fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
                let bytes = record(other_thread, None, None);
                fs::write(
                    &duplicate,
                    if compressed {
                        zstd::stream::encode_all(bytes.as_bytes(), 1).unwrap()
                    } else {
                        bytes.into_bytes()
                    },
                )
                .unwrap();
                write(home.path(), C, C, None, None, !compressed);
                let found = catalog(home.path()).unwrap();
                assert_eq!(found.conflicts, ids(D));
                assert_eq!(found.all().count(), 3);
                assert!(found.all().any(|r| r.path == primary));
                assert!(found.all().any(|r| r.path == duplicate));
                assert_eq!(found.threads[A], [D]);
                assert_eq!(found.threads[other_thread], [D]);
                for selected in [A, other_thread, D] {
                    assert!(
                        found
                            .ensure_readable_for(&ids(selected))
                            .unwrap_err()
                            .is::<PhysicalConflict>()
                    );
                    assert!(
                        found
                            .active(selected)
                            .err()
                            .unwrap()
                            .is::<PhysicalConflict>()
                    );
                    assert!(
                        found
                            .chain(selected)
                            .err()
                            .unwrap()
                            .is::<PhysicalConflict>()
                    );
                }
                assert!(found.ensure_readable_for(&ids(C)).is_ok());
                assert_eq!(found.chain(C).unwrap().len(), 1);
            }
        }
    }
}

#[cfg(windows)]
#[test]
fn conflicting_copies_preserve_logical_and_physical_parent_edges_in_each_root() {
    for history in [false, true] {
        for archived in [false, true] {
            let home = home();
            write(home.path(), A, A, None, None, false);
            write(home.path(), B, D, None, None, true);
            let duplicate = home
                .path()
                .join(if archived {
                    "archived_sessions"
                } else {
                    "sessions/duplicate"
                })
                .join(format!("rollout-conflict-{D}.jsonl.zst"));
            fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
            fs::write(
                duplicate,
                zstd::stream::encode_all(
                    record(B, history.then_some(A), (!history).then_some(A)).as_bytes(),
                    1,
                )
                .unwrap(),
            )
            .unwrap();
            write(home.path(), C, C, None, None, true);
            let found = catalog(home.path()).unwrap();
            assert_eq!(
                found.related_ids(&ids(A)),
                BTreeSet::from([A.into(), B.into(), D.into()])
            );
            for selected in [A, B, D] {
                assert!(
                    found
                        .ensure_readable_for(&ids(selected))
                        .unwrap_err()
                        .is::<PhysicalConflict>()
                );
                assert!(
                    found
                        .active(selected)
                        .err()
                        .unwrap()
                        .is::<PhysicalConflict>()
                );
                assert!(
                    found
                        .chain(selected)
                        .err()
                        .unwrap()
                        .is::<PhysicalConflict>()
                );
            }
            assert_eq!(found.chain(C).unwrap().len(), 1);
        }
    }
}

#[cfg(windows)]
#[test]
fn cross_home_graph_propagates_only_related_physical_conflicts() {
    for history in [false, true] {
        let root = home();
        let other = home();
        write(root.path(), A, A, None, None, false);
        write(
            root.path(),
            B,
            B,
            history.then_some(D),
            (!history).then_some(D),
            true,
        );
        write(other.path(), C, D, None, None, false);
        write(other.path(), C, D, None, None, true);
        let first = catalog(root.path()).unwrap();
        let second = catalog(other.path()).unwrap();
        assert_eq!(first.chain(A).unwrap().len(), 1);
        assert!(
            second
                .ensure_readable_for(&first.related_ids(&ids(A)))
                .is_ok()
        );
        assert!(
            second
                .ensure_readable_for(&first.related_ids(&ids(B)))
                .unwrap_err()
                .is::<PhysicalConflict>()
        );
    }
}

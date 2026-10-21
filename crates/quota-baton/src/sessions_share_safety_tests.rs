use super::*;

// Snapshot only durable session state. Apply may create coordination locks.
// Include missing paths so a refused operation cannot silently publish rows,
// an index, or a journal where none existed before.
fn share_safety_snapshot(f: &Fixture) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn collect(path: &Path, snapshot: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        if !path.exists() {
            snapshot.push((path.to_owned(), None));
        } else if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                collect(&entry.unwrap().path(), snapshot);
            }
        } else {
            snapshot.push((path.to_owned(), Some(fs::read(path).unwrap())));
        }
    }

    let mut snapshot = Vec::new();
    for home in [&f.source, &f.store.target] {
        for name in ["state_5.sqlite", "goals_1.sqlite", "session_index.jsonl"] {
            collect(&home.join(name), &mut snapshot);
            if name.ends_with(".sqlite") {
                for suffix in ["-wal", "-shm", "-journal"] {
                    collect(&home.join(format!("{name}{suffix}")), &mut snapshot);
                }
            }
        }
        for name in ["sessions", "archived_sessions"] {
            collect(&home.join(name), &mut snapshot);
        }
    }
    collect(&f.store.path(), &mut snapshot);
    for name in ["session-moves", "sessions-stale"] {
        collect(&f.store.home.join("journal").join(name), &mut snapshot);
    }
    snapshot.sort_by(|a, b| a.0.cmp(&b.0));
    snapshot
}

fn share_safety_source(f: &Fixture, compressed: bool) -> PathBuf {
    let original = f.add(A, None, false, "active");
    let source = if compressed {
        let compressed = original.with_extension("jsonl.zst");
        let body = zstd::stream::encode_all(fs::read(&original).unwrap().as_slice(), 1).unwrap();
        // Keep a single selected name, now containing an actual zstd stream.
        fs::rename(&original, &compressed).unwrap();
        fs::write(&compressed, body).unwrap();
        Connection::open(f.source.join("state_5.sqlite"))
            .unwrap()
            .execute(
                "UPDATE threads SET rollout_path=?1 WHERE id=?2",
                rusqlite::params![compressed.to_str().unwrap(), A],
            )
            .unwrap();
        compressed
    } else {
        original
    };
    File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)))
        .unwrap();
    source
}

#[test]
fn share_unimport_refuses_last_rollout_name_even_with_force() {
    for compressed in [false, true] {
        for apply in [false, true] {
            for force in [false, true] {
                let f = Fixture::new();
                let source = share_safety_source(&f, compressed);
                let body = fs::read(&source).unwrap();
                let identity = guard::identity(&source).unwrap();
                assert!(f.import(A, true).unwrap());
                let entry = f.store.load().unwrap().imports[A].clone();
                assert_eq!(entry.files.len(), 1);
                let shared = &entry.files[0].target;
                assert_eq!(entry.files[0].source, source);
                assert!(entry.files[0].owned);
                assert_eq!(guard::hardlink_count(shared).unwrap(), 2);
                fs::remove_file(&source).unwrap();
                assert_eq!(guard::hardlink_count(shared).unwrap(), 1);
                let before = share_safety_snapshot(&f);

                let result = unimport(&f.store, A, apply, force);
                assert!(
                    result.is_err(),
                    "last-name deletion allowed: compressed={compressed}, apply={apply}, force={force}"
                );
                assert_eq!(fs::read(shared).unwrap(), body);
                assert_eq!(guard::identity(shared).unwrap(), identity);
                assert_eq!(guard::hardlink_count(shared).unwrap(), 1);
                assert!(!source.exists());
                assert_eq!(share_safety_snapshot(&f), before);
            }
        }
    }
}

#[test]
fn share_unimport_missing_recorded_source_can_keep_another_hardlink() {
    for compressed in [false, true] {
        for force in [false, true] {
            let f = Fixture::new();
            let source = share_safety_source(&f, compressed);
            assert!(f.import(A, true).unwrap());
            let shared = f.store.load().unwrap().imports[A].files[0].target.clone();
            let survivor = source.with_file_name(format!(
                "surviving-{}",
                source.file_name().unwrap().to_str().unwrap()
            ));
            fs::hard_link(&source, &survivor).unwrap();
            let body = fs::read(&survivor).unwrap();
            let identity = guard::identity(&survivor).unwrap();
            fs::remove_file(&source).unwrap();
            assert_eq!(guard::hardlink_count(&shared).unwrap(), 2);
            let before = share_safety_snapshot(&f);
            let source_before = source_snapshot(&f.source);

            unimport(&f.store, A, false, force).unwrap();
            assert_eq!(share_safety_snapshot(&f), before);
            unimport(&f.store, A, true, force).unwrap();
            assert!(!shared.exists());
            assert!(!source.exists());
            assert_eq!(fs::read(&survivor).unwrap(), body);
            assert_eq!(guard::identity(&survivor).unwrap(), identity);
            assert_eq!(guard::hardlink_count(&survivor).unwrap(), 1);
            assert!(!f.store.load().unwrap().imports.contains_key(A));
            assert_eq!(source_snapshot(&f.source), source_before);
        }
    }
}

#[test]
fn share_import_refuses_target_header_id_disagreeing_with_selected_filename() {
    for compressed in [false, true] {
        for apply in [false, true] {
            let f = Fixture::new();
            let source = f.add(A, None, false, "active");
            let mut target = f
                .store
                .target
                .join("archived_sessions")
                .join(source.file_name().unwrap());
            if compressed {
                target = target.with_extension("jsonl.zst");
            }
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            let header = format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":B,"model_provider":"openai"}})
            );
            let body = if compressed {
                zstd::stream::encode_all(header.as_bytes(), 1).unwrap()
            } else {
                header.into_bytes()
            };
            fs::write(&target, &body).unwrap();
            File::options()
                .write(true)
                .open(&target)
                .unwrap()
                .set_times(
                    fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)),
                )
                .unwrap();
            let source_identity = guard::identity(&source).unwrap();
            let target_identity = guard::identity(&target).unwrap();
            let before = share_safety_snapshot(&f);

            // Fixture::import explicitly sets keep_source=true (share mode).
            let error = f.import(A, apply).unwrap_err();
            assert!(
                error.downcast_ref::<InventoryConflict>().is_some(),
                "expected InventoryConflict: compressed={compressed}, apply={apply}: {error}"
            );
            assert_eq!(share_safety_snapshot(&f), before);
            assert_eq!(fs::read(&target).unwrap(), body);
            assert_eq!(guard::identity(&source).unwrap(), source_identity);
            assert_eq!(guard::identity(&target).unwrap(), target_identity);
            assert_eq!(guard::hardlink_count(&source).unwrap(), 1);
            assert_eq!(guard::hardlink_count(&target).unwrap(), 1);
        }
    }
}

#[test]
fn physical_owner_conflict_in_peer_home_refuses_share_and_move() {
    for keep_source in [false, true] {
        for apply in [false, true] {
            let mut f = Fixture::new();
            let source = f.add(A, None, false, "active");
            let peer = f._root.path().join("peer");
            let alias = peer
                .join("archived_sessions")
                .join(source.file_name().unwrap());
            fs::create_dir_all(alias.parent().unwrap()).unwrap();
            let body = format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":B,"model_provider":"openai"}})
            );
            fs::write(&alias, &body).unwrap();
            f.store.search_homes.push(peer);
            let before = share_safety_snapshot(&f);
            let catalog = discovery::catalog(&f.source).unwrap();
            let error = import_one(
                &f.store,
                &catalog,
                A,
                ImportOptions {
                    apply,
                    skip_existing: false,
                    announce: false,
                    keep_source,
                    clean_stale: false,
                },
            )
            .unwrap_err();
            assert!(error.is::<InventoryConflict>(), "{error}");
            assert_eq!(share_safety_snapshot(&f), before);
            assert_eq!(fs::read_to_string(&alias).unwrap(), body);
        }
    }
}

#[test]
fn journaled_move_restores_a_name_before_removing_its_last_target_link() {
    for compressed in [false, true] {
        let f = Fixture::new();
        let source = share_safety_source(&f, compressed);
        f.import(A, true).unwrap();
        let mut journal = f.store.load().unwrap();
        let record = journal.imports.get_mut(A).unwrap();
        record.keep_source = false;
        record.complete = false;
        record.move_started = true;
        for link in &mut record.files {
            link.moved = true;
        }
        let snapshot = db::source_snapshot(&f.source, &[A.into()]).unwrap();
        record.source_database =
            crate::cutover::protect(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let target = record.files[0].target.clone();
        let identity = guard::identity(&target).unwrap();
        let bytes = fs::read(&target).unwrap();
        f.store.save(&journal).unwrap();
        db::remove_exact(&snapshot).unwrap();
        fs::remove_file(&source).unwrap();
        assert_eq!(guard::hardlink_count(&target).unwrap(), 1);
        unimport(&f.store, A, true, false).unwrap();
        assert!(!target.exists());
        assert_eq!(guard::identity(&source).unwrap(), identity);
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(guard::hardlink_count(&source).unwrap(), 1);
        assert!(!f.store.load().unwrap().imports.contains_key(A));
        assert_eq!(db::threads(&f.source).unwrap().len(), 1);
    }
}

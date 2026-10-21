//! Synthetic only. No real map/home, environment changes, or native Codex.
use super::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";
const C: &str = "33333333-3333-3333-3333-333333333333";

struct Fixture {
    _root: tempfile::TempDir,
    path: PathBuf,
    backup: PathBuf,
    target: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/session-map-resolve-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let fixture = Self {
            path: root.path().join("appdata/herdr/session-homes.json"),
            backup: root.path().join("journal/map-backups"),
            target: root.path().join("synthetic-qb-home"),
            _root: root,
        };
        fs::create_dir(&fixture.target).unwrap();
        fixture.write(&json!({
            "sessions": {A: "synthetic-source", B: "synthetic-other-home"},
            "future": {"keep": [1, true], "padding": "synthetic".repeat(1600)}
        }));
        fixture
    }

    fn write(&self, value: &Value) {
        fs::create_dir_all(self.path.parent().unwrap()).unwrap();
        fs::write(&self.path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(&self.path).unwrap()
    }

    fn value(&self) -> Value {
        serde_json::from_slice(&self.bytes()).unwrap()
    }

    fn plan(&self) -> MapPlan {
        plan(&self.path, A, &self.target, &self.backup).unwrap()
    }

    fn resolved(&self, mut value: Value) -> Value {
        value["sessions"][A] = json!(map_home_value(&self.target).unwrap());
        value
    }
}

// POSIX replace flags intentionally model a noncooperating atomic map writer.
fn foreign_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::DELETE;
    let scratch = tempfile::tempdir_in(path.parent().unwrap())?;
    let staged = scratch.path().join("foreign.json");
    publish(&staged, bytes, || Ok(()))?;
    let file = OpenOptions::new()
        .read(true)
        .access_mode(DELETE.0)
        .share_mode(0)
        .open(&staged)?;
    rename_file(&file, path, 3)
}

struct Hook;

impl Drop for Hook {
    fn drop(&mut self) {
        RESOLUTION_HOOK.with(|hook| *hook.borrow_mut() = None);
    }
}

fn hook(callback: impl FnMut(&str) -> Result<()> + 'static) -> Hook {
    RESOLUTION_HOOK.with(|hook| {
        let mut hook = hook.borrow_mut();
        assert!(hook.is_none());
        *hook = Some(Box::new(callback));
    });
    Hook
}

fn interrupt(plan: &MapPlan, boundary: &'static str, occurrence: usize) {
    let seen = Rc::new(Cell::new(0));
    let hits = seen.clone();
    let last = Rc::new(RefCell::new(None));
    let checkpoint = last.clone();
    let _hook = hook(move |label| {
        *checkpoint.borrow_mut() = Some(label.to_owned());
        if label == boundary {
            hits.set(hits.get() + 1);
            if hits.get() == occurrence {
                return Err(anyhow!(
                    "synthetic resolution interruption at {boundary} #{occurrence}"
                ));
            }
        }
        Ok(())
    });
    let result = apply_resolution(plan);
    let error = result.as_ref().err().map(|error| format!("{error:#}"));
    assert!(
        seen.get() >= occurrence,
        "resolution boundary {boundary} #{occurrence} not reached; last checkpoint: {:?}; actual result: {}",
        last.borrow().as_deref(),
        error.as_deref().unwrap_or("success")
    );
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains(&format!(
                "synthetic resolution interruption at {boundary} #{occurrence}"
            ))),
        "resolution boundary {boundary} #{occurrence} returned an unexpected result: {}",
        error.as_deref().unwrap_or("success")
    );
}

fn reload(bytes: &[u8]) -> MapPlan {
    serde_json::from_slice(bytes).unwrap()
}

#[test]
fn resolution_replans_conflict_and_keeps_the_latest_other_uuids_and_original_delta() {
    let f = Fixture::new();
    let original = f.plan();
    save_backup(&original).unwrap();
    let evidence = fs::read(backup_path(&original).unwrap()).unwrap();
    let mut foreign = f.value();
    foreign["sessions"][A] = json!("synthetic-conflicting-home");
    foreign["sessions"][B] = json!("synthetic-earlier-update");
    foreign_replace(&f.path, &serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(apply(&original).is_err());
    let new = replan_resolution(&original).unwrap();
    assert!(new.is_resolution());
    assert!(new.backup_dir.parent() == Some(original.backup_dir.as_path()));
    assert!(new.change.before.as_deref() == Some("synthetic-conflicting-home"));
    assert!(new.resolution.as_ref().unwrap().change == original.change);
    assert!(
        !new.backup_dir.exists(),
        "replanning must not publish evidence"
    );

    // A second independent writer AFTER replan must also survive application.
    foreign["sessions"][B] = json!("synthetic-latest-update");
    foreign["sessions"][C] = json!("synthetic-new-uuid");
    foreign["future"]["new"] = json!([false, 42]);
    foreign_replace(&f.path, &serde_json::to_vec(&foreign).unwrap()).unwrap();
    let persisted = serde_json::to_vec(&new).unwrap();
    apply_resolution(&reload(&persisted)).unwrap();
    assert!(f.value() == f.resolved(foreign));
    assert!(fs::read(backup_path(&original).unwrap()).unwrap() == evidence);
    assert!(check_backup(&new).unwrap());
    let (intent, complete) = repair_intent(&new).unwrap().unwrap();
    assert!(complete);
    assert!(serde_json::from_str::<Value>(&intent.intended).unwrap() == f.value());
    let encrypted = fs::read_to_string(new.backup_dir.join(REPAIR_INTENT)).unwrap();
    assert!(!encrypted.contains("synthetic-latest-update"));
    assert!(!encrypted.contains("synthetic-new-uuid"));
}

#[test]
fn permanently_unavailable_txf_has_an_explicit_resolution_without_weakening_apply() {
    let f = Fixture::new();
    let original = f.plan();
    let before = f.bytes();
    let txf_calls = Rc::new(Cell::new(0));
    let calls = txf_calls.clone();
    let _hook = hook(move |label| {
        if label == "map_txf_writer" {
            calls.set(calls.get() + 1);
            return Err(anyhow!(TRANSACTION));
        }
        Ok(())
    });
    assert!(apply(&original).is_err());
    assert!(f.bytes() == before);
    assert!(txf_calls.get() == 1);
    let evidence = fs::read(backup_path(&original).unwrap()).unwrap();
    let resolved = replan_resolution(&original).unwrap();
    let persisted = serde_json::to_vec(&resolved).unwrap();
    assert!(
        apply(&resolved).is_err(),
        "ordinary apply cannot take the resolution path"
    );
    assert!(
        restore(&resolved).is_err(),
        "explicit recovery is forward only"
    );
    apply_resolution(&reload(&persisted)).unwrap();
    apply_resolution(&reload(&persisted)).unwrap();
    assert!(
        txf_calls.get() == 1,
        "explicit resolution must never require TxF"
    );
    assert!(f.value() == f.resolved(serde_json::from_slice(&before).unwrap()));
    assert!(fs::read(backup_path(&original).unwrap()).unwrap() == evidence);
}

#[test]
fn deny_share_excludes_atomic_writers_and_readers_until_completion() {
    let f = Fixture::new();
    let p = replan_resolution(&f.plan()).unwrap();
    let mut outsider = f.resolved(f.value());
    outsider["sessions"][B] = json!("synthetic-writer-after-completion");
    let outsider_bytes = serde_json::to_vec(&outsider).unwrap();
    let path = f.path.clone();
    let attempted = Rc::new(Cell::new(0));
    let hits = attempted.clone();
    let bytes = outsider_bytes.clone();
    let installed = hook(move |label| {
        if label.starts_with("resolution_") {
            hits.set(hits.get() + 1);
            assert!(
                foreign_replace(&path, &bytes).is_err(),
                "foreign replacement was admitted at {label}"
            );
            assert!(File::open(&path).is_err(), "reader was admitted at {label}");
            assert!(
                OpenOptions::new().write(true).open(&path).is_err(),
                "writer was admitted at {label}"
            );
        }
        Ok(())
    });
    apply_resolution(&p).unwrap();
    drop(installed);
    assert!(attempted.get() >= 10);
    foreign_replace(&f.path, &outsider_bytes).unwrap();
    let intent_evidence = fs::read(p.backup_dir.join(REPAIR_INTENT)).unwrap();
    let complete_evidence = fs::read(p.backup_dir.join(REPAIR_COMPLETE)).unwrap();
    apply_resolution(&p).unwrap();
    apply_resolution(&p).unwrap();
    assert!(
        f.bytes() == outsider_bytes,
        "completed repairs cannot replay whole maps"
    );
    assert!(fs::read(p.backup_dir.join(REPAIR_INTENT)).unwrap() == intent_evidence);
    assert!(fs::read(p.backup_dir.join(REPAIR_COMPLETE)).unwrap() == complete_evidence);
}

#[test]
fn every_resolution_write_boundary_replays_the_persisted_intent_twice() {
    let boundaries = [
        ("resolution_locked", 1),
        ("resolution_backups", 1),
        ("resolution_intent_staged", 1),
        ("resolution_journal", 1),
        ("resolution_truncated", 1),
        ("resolution_truncate_synced", 1),
        ("resolution_write", 1),
        ("resolution_write", 2),
        ("resolution_write", 3),
        ("resolution_write_synced", 1),
        ("resolution_write_synced", 2),
        ("resolution_write_synced", 3),
        ("resolution_flushed", 1),
        ("resolution_complete_staged", 1),
        ("resolution_completed", 1),
    ];
    for (boundary, occurrence) in boundaries {
        let f = Fixture::new();
        let expected = f.resolved(f.value());
        let original = f.plan();
        save_backup(&original).unwrap();
        let old_evidence = fs::read(backup_path(&original).unwrap()).unwrap();
        let p = replan_resolution(&original).unwrap();
        let caller_journal = serde_json::to_vec(&p).unwrap();
        interrupt(&p, boundary, occurrence);
        let intent_evidence = fs::read(p.backup_dir.join(REPAIR_INTENT)).ok();
        if repair_intent(&p)
            .unwrap()
            .is_some_and(|(_, complete)| !complete)
        {
            assert!(
                verify(&p).is_err(),
                "pending repair is never reported complete"
            );
            let retry = replan_resolution(&reload(&caller_journal)).unwrap();
            assert!(serde_json::to_vec(&retry).unwrap() == caller_journal);
        }
        apply_resolution(&reload(&caller_journal)).unwrap();
        apply_resolution(&reload(&caller_journal)).unwrap();
        verify(&p).unwrap();
        assert!(f.value() == expected);
        assert!(fs::read(backup_path(&original).unwrap()).unwrap() == old_evidence);
        if let Some(evidence) = intent_evidence {
            assert!(fs::read(p.backup_dir.join(REPAIR_INTENT)).unwrap() == evidence);
        }
    }
}

#[test]
fn truncated_and_arbitrarily_short_or_complete_prefixes_repair_from_encrypted_intent() {
    for length in [0, 1, 7, 4095, usize::MAX - 1, usize::MAX] {
        let f = Fixture::new();
        let expected = f.resolved(f.value());
        let p = replan_resolution(&f.plan()).unwrap();
        let caller_journal = serde_json::to_vec(&p).unwrap();
        interrupt(&p, "resolution_journal", 1);
        let (intent, complete) = repair_intent(&p).unwrap().unwrap();
        assert!(!complete);
        let length = match length {
            usize::MAX => intent.intended.len(),
            n if n == usize::MAX - 1 => intent.intended.len() - 1,
            n => n,
        };
        // Model a successful short write, including an empty truncation.
        // fs::write truncates this existing object; it does not replace it.
        fs::write(&f.path, &intent.intended.as_bytes()[..length]).unwrap();
        let file = File::open(&f.path).unwrap();
        assert!(repair_identity(&file).unwrap() == intent.identity);
        drop(file);
        let retry = replan_resolution(&reload(&caller_journal)).unwrap();
        assert!(serde_json::to_vec(&retry).unwrap() == caller_journal);
        apply_resolution(&retry).unwrap();
        apply_resolution(&retry).unwrap();
        assert!(f.value() == expected);
    }
}

#[test]
fn a_replaced_file_is_refused_even_when_its_bytes_match_the_intended_prefix() {
    for complete_bytes in [false, true] {
        let f = Fixture::new();
        let p = replan_resolution(&f.plan()).unwrap();
        interrupt(&p, "resolution_write", 1);
        let (intent, _) = repair_intent(&p).unwrap().unwrap();
        let replacement = if complete_bytes {
            intent.intended.as_bytes().to_vec()
        } else {
            intent.intended.as_bytes()[..7].to_vec()
        };
        foreign_replace(&f.path, &replacement).unwrap();
        let file = File::open(&f.path).unwrap();
        assert!(repair_identity(&file).unwrap() != intent.identity);
        drop(file);
        let evidence = fs::read(p.backup_dir.join(REPAIR_INTENT)).unwrap();
        assert!(apply_resolution(&p).unwrap_err().to_string() == CONFLICT);
        assert!(f.bytes() == replacement);
        assert!(fs::read(p.backup_dir.join(REPAIR_INTENT)).unwrap() == evidence);
        assert!(!p.backup_dir.join(REPAIR_COMPLETE).exists());
        let retry = replan_resolution(&p).unwrap();
        assert!(serde_json::to_vec(&retry).unwrap() == serde_json::to_vec(&p).unwrap());
    }
}

#[test]
fn an_unexpected_same_identity_other_uuid_edit_during_a_crash_is_not_overwritten() {
    let f = Fixture::new();
    let mut outsider = f.value();
    let p = replan_resolution(&f.plan()).unwrap();
    interrupt(&p, "resolution_journal", 1);
    let (intent, _) = repair_intent(&p).unwrap().unwrap();
    outsider["sessions"][B] = json!("synthetic-in-place-outside-change");
    f.write(&outsider);
    let file = File::open(&f.path).unwrap();
    assert!(repair_identity(&file).unwrap() == intent.identity);
    drop(file);
    let before = f.bytes();
    assert!(apply_resolution(&p).unwrap_err().to_string() == CONFLICT);
    assert!(f.bytes() == before);
    assert!(!p.backup_dir.join(REPAIR_COMPLETE).exists());
}

#[test]
fn missing_or_corrupted_repair_evidence_cannot_authorize_map_changes() {
    for damage in [
        "intent-corrupt",
        "intent-missing",
        "complete-corrupt",
        "delta-missing",
    ] {
        let f = Fixture::new();
        let p = replan_resolution(&f.plan()).unwrap();
        interrupt(&p, "resolution_truncated", 1);
        match damage {
            "intent-corrupt" => {
                fs::write(p.backup_dir.join(REPAIR_INTENT), b"invalid DPAPI").unwrap()
            }
            "intent-missing" => fs::remove_file(p.backup_dir.join(REPAIR_INTENT)).unwrap(),
            "complete-corrupt" => {
                fs::write(p.backup_dir.join(REPAIR_COMPLETE), b"invalid DPAPI").unwrap();
            }
            "delta-missing" => fs::remove_file(backup_path(&p).unwrap()).unwrap(),
            _ => unreachable!(),
        }
        let before = f.bytes();
        assert!(apply_resolution(&p).is_err());
        assert!(f.bytes() == before);
    }
}

#[test]
fn a_missing_map_publication_never_replaces_a_racing_other_uuid_writer() {
    let f = Fixture::new();
    fs::remove_file(&f.path).unwrap();
    let original = f.plan();
    let p = replan_resolution(&original).unwrap();
    let outsider = json!({"sessions": {B: "synthetic-racing-creator"}, "future": true});
    let path = f.path.clone();
    let bytes = serde_json::to_vec(&outsider).unwrap();
    let published = Rc::new(Cell::new(false));
    let reached = published.clone();
    let installed = hook(move |label| {
        if label == "resolution_before_publish" {
            foreign_replace(&path, &bytes)?;
            reached.set(true);
        }
        Ok(())
    });
    let result = apply_resolution(&p);
    drop(installed);
    assert!(
        published.get(),
        "racing writer did not publish at resolution_before_publish; actual result: {}",
        result
            .as_ref()
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_else(|| "success".to_owned())
    );
    assert!(
        result.is_err(),
        "no-replace publication must refuse the racing map"
    );
    assert!(f.value() == outsider);
    assert!(!p.backup_dir.join(REPAIR_INTENT).exists());
    let replanned = replan_resolution(&p).unwrap();
    apply_resolution(&replanned).unwrap();
    assert!(f.value() == f.resolved(outsider));
    assert!(check_backup(&original).unwrap());
    assert!(check_backup(&p).unwrap());
}

#[test]
fn a_missing_map_publication_is_idempotent_after_the_publish_boundary() {
    let f = Fixture::new();
    fs::remove_file(&f.path).unwrap();
    let p = replan_resolution(&f.plan()).unwrap();
    let caller_journal = serde_json::to_vec(&p).unwrap();
    interrupt(&p, "resolution_published", 1);
    let before = f.bytes();
    apply_resolution(&reload(&caller_journal)).unwrap();
    apply_resolution(&reload(&caller_journal)).unwrap();
    assert!(f.bytes() == before);
    verify(&p).unwrap();
}

#[test]
fn missing_map_and_backup_ancestors_are_created_without_touching_sibling_paths() {
    let mut f = Fixture::new();
    let unrelated_map = f.path.clone();
    let unrelated_bytes = f.bytes();
    let sibling = f._root.path().join("unrelated-sibling");
    fs::create_dir(&sibling).unwrap();
    let sentinel = sibling.join("keep.txt");
    fs::write(&sentinel, b"synthetic sibling evidence").unwrap();
    f.path = f
        ._root
        .path()
        .join("new-appdata/nested/herdr/session-homes.json");
    let p = replan_resolution(&f.plan()).unwrap();
    assert!(!f.path.parent().unwrap().exists());
    assert!(!f.backup.parent().unwrap().exists());
    apply_resolution(&p).unwrap();
    apply_resolution(&p).unwrap();
    assert!(f.value() == f.resolved(json!({"sessions": {}})));
    assert!(fs::read(&unrelated_map).unwrap() == unrelated_bytes);
    assert!(fs::read(&sentinel).unwrap() == b"synthetic sibling evidence");
    assert!(fs::read_dir(&sibling).unwrap().count() == 1);
    verify(&p).unwrap();
}

#[test]
fn a_file_blocking_a_missing_backup_ancestor_is_preserved_and_refused() {
    let f = Fixture::new();
    let p = replan_resolution(&f.plan()).unwrap();
    let before = f.bytes();
    let blocker = f.backup.parent().unwrap();
    fs::write(blocker, b"synthetic blocking evidence").unwrap();
    assert!(apply_resolution(&p).is_err());
    assert!(f.bytes() == before);
    assert!(fs::read(blocker).unwrap() == b"synthetic blocking evidence");
    assert!(!p.backup_dir.exists());
}

#[test]
fn resolution_of_legacy_extended_intent_keeps_the_original_after_and_backups() {
    let f = Fixture::new();
    let native_after = f.target.to_str().unwrap();
    assert!(native_after.starts_with(r"\\?\"));
    let mut stored = serde_json::to_value(f.plan()).unwrap();
    stored["change"]["after"] = json!(native_after);
    let old = reload(&serde_json::to_vec(&stored).unwrap());
    save_backup(&old).unwrap();
    let evidence = fs::read(backup_path(&old).unwrap()).unwrap();
    let mut latest = f.value();
    latest["sessions"][A] = json!("synthetic-conflicting-home");
    latest["sessions"][B] = json!("synthetic-latest-other-home");
    f.write(&latest);
    let next = replan_resolution(&old).unwrap();
    assert_eq!(next.change.after, native_after);
    assert!(next.resolution.as_ref().unwrap().change == old.change);
    assert_eq!(
        next.change.before.as_deref(),
        Some("synthetic-conflicting-home")
    );
    let persisted = serde_json::to_vec(&next).unwrap();
    let recovered = reload(&persisted);
    apply_resolution(&recovered).unwrap();
    apply_resolution(&reload(&persisted)).unwrap();
    verify(&recovered).unwrap();
    latest["sessions"][A] = json!(native_after);
    assert!(f.value() == latest);
    assert!(check_backup(&recovered).unwrap());
    assert_eq!(fs::read(backup_path(&old).unwrap()).unwrap(), evidence);
}

#[test]
fn old_serialized_plans_cannot_implicitly_opt_in_to_resolution() {
    let f = Fixture::new();
    let original = f.plan();
    let bytes = serde_json::to_vec(&original).unwrap();
    let legacy = reload(&bytes);
    assert!(!legacy.is_resolution());
    assert!(
        serde_json::from_slice::<Value>(&bytes)
            .unwrap()
            .get("resolution")
            .is_none()
    );
    let before = f.bytes();
    assert!(apply_resolution(&legacy).is_err());
    assert!(f.bytes() == before);
    assert!(!f.backup.exists());
}

#[test]
fn a_pretty_printed_intent_over_the_limit_refuses_before_truncation() {
    let f = Fixture::new();
    let mut value = json!({"sessions": {A: "synthetic-source"}, "padding": ""});
    let overhead = serde_json::to_vec(&value).unwrap().len();
    value["padding"] = json!("x".repeat(LIMIT as usize - overhead - 1));
    f.write(&value);
    let p = replan_resolution(&f.plan()).unwrap();
    let before = f.bytes();
    assert!(apply_resolution(&p).is_err());
    assert!(f.bytes() == before);
    assert!(!p.backup_dir.join(REPAIR_INTENT).exists());
}

//! Filename-only production lookup, invoked by the existing writer, never ingress.
use super::labels::{MemoryLabel, claude_project_label};
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const SLUG_BYTES: usize = 1024;
const ROOT_DEPTH: usize = 32;
const WALK_TIME: Duration = Duration::from_millis(25);
const CACHE_TIME: Duration = Duration::from_secs(30);
const CACHE_ENTRIES: usize = 128;
#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    per_level: usize,
    depth: usize,
}
const LIMITS: Limits = Limits {
    entries: 4096,
    per_level: 1024,
    depth: 16,
};
struct Decode {
    basename: Option<String>,
    matched: usize,
    entries: usize,
    exhausted: bool,
    ambiguous: bool,
}
impl Decode {
    fn label(self, slug: &str) -> Option<String> {
        if !self.exhausted && !self.ambiguous && self.matched == slug.len() {
            return self.basename;
        }
        slug.get(self.matched..)
            .filter(|tail| !tail.is_empty())
            .map(str::to_owned)
    }
}

#[derive(Default)]
struct DecodeCache(VecDeque<(String, Option<String>, Instant)>);
impl DecodeCache {
    fn get(&self, slug: &str, now: Instant) -> Option<Option<String>> {
        self.0.iter().find_map(|(key, value, at)| {
            (key == slug
                && now
                    .checked_duration_since(*at)
                    .is_some_and(|age| age < CACHE_TIME))
            .then(|| value.clone())
        })
    }
    fn insert(&mut self, slug: &str, label: Option<String>, now: Instant) {
        if slug.len() > SLUG_BYTES {
            return;
        }
        self.0.retain(|(key, _, _)| key != slug);
        while self.0.len() >= CACHE_ENTRIES {
            self.0.pop_front();
        }
        self.0.push_back((slug.to_owned(), label, now));
    }
}

fn local_drive(letter: u8) -> Option<PathBuf> {
    crate::platform::resolver_local_drive(letter)
}

fn local_scan_root(path: &Path) -> bool {
    crate::platform::resolver_local_scan_root(path, SLUG_BYTES, ROOT_DEPTH)
}

fn encode_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn directory_names(path: &Path) -> io::Result<impl Iterator<Item = io::Result<OsString>> + use<>> {
    Ok(fs::read_dir(path)?.map(|entry| entry.map(|entry| entry.file_name())))
}

/// Walk ordinary directories by their encoded names, never file contents.
/// The longest prefix wins; a literal full name breaks encoding collisions.
/// Every OS operation is preceded by a cooperative deadline check: a local
/// filesystem call already in progress cannot be interrupted here.
fn decode_with<I: Iterator<Item = io::Result<OsString>>>(
    slug: &str,
    limits: Limits,
    root_for_drive: impl FnOnce(u8) -> Option<PathBuf>,
    mut directory: impl FnMut(&Path) -> bool,
    mut names: impl FnMut(&Path) -> io::Result<I>,
    mut expired: impl FnMut() -> bool,
) -> Decode {
    let mut result = Decode {
        basename: None,
        matched: 0,
        entries: 0,
        exhausted: false,
        ambiguous: false,
    };
    let bytes = slug.as_bytes();
    if bytes.len() < 4
        || bytes.len() > SLUG_BYTES
        || !bytes[0].is_ascii_alphabetic()
        || &bytes[1..3] != b"--"
    {
        return result;
    }
    let tail = &slug[3..];
    if tail
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
    {
        return result;
    }
    if expired() || limits.entries == 0 || limits.per_level == 0 || limits.depth == 0 {
        result.exhausted = true;
        return result;
    }
    let Some(root) = root_for_drive(bytes[0]) else {
        return result;
    };
    if !root.is_absolute() {
        return result;
    }
    if expired() {
        result.exhausted = true;
        return result;
    }
    if !directory(&root) {
        return result;
    }
    result.matched = 3;
    let mut parent = root;
    for _ in 0..limits.depth {
        if expired() {
            result.exhausted = true;
            return result;
        }
        let Ok(mut entries) = names(&parent) else {
            return result;
        };
        let remaining = &slug[result.matched..];
        let mut best: Option<(String, usize, bool)> = None;
        let mut ambiguous = false;
        let mut level = 0;
        loop {
            if expired() {
                result.exhausted = true;
                return result;
            }
            let Some(entry) = entries.next() else {
                break;
            };
            // One lookahead distinguishes a complete listing from truncation.
            // Never select an apparent unique match from an incomplete listing.
            if level >= limits.per_level || result.entries >= limits.entries {
                result.exhausted = true;
                return result;
            }
            level += 1;
            result.entries += 1;
            let Ok(name) = entry.and_then(|name| {
                name.into_string()
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
            }) else {
                return result;
            };
            if name.is_empty() || matches!(name.as_str(), "." | "..") {
                continue;
            }
            let encoded = encode_name(&name);
            let Some(rest) = remaining
                .get(..encoded.len())
                .filter(|prefix| prefix.eq_ignore_ascii_case(&encoded))
                .and_then(|_| remaining.get(encoded.len()..))
            else {
                continue;
            };
            if !rest.is_empty() && !rest.starts_with('-') {
                continue;
            }
            if expired() {
                result.exhausted = true;
                return result;
            }
            if !directory(&parent.join(&name)) {
                continue;
            }
            let exact = name == remaining;
            let rank = (encoded.len(), exact);
            if best
                .as_ref()
                .is_none_or(|(_, len, exact)| rank > (*len, *exact))
            {
                best = Some((name, encoded.len(), exact));
                ambiguous = false;
            } else if best
                .as_ref()
                .is_some_and(|(_, len, exact)| rank == (*len, *exact))
            {
                ambiguous = true;
            }
        }
        if ambiguous {
            result.ambiguous = true;
            return result;
        }
        let Some((name, len, _)) = best else {
            return result;
        };
        if expired() {
            result.exhausted = true;
            return result;
        }
        parent.push(&name);
        result.matched += len;
        if result.matched == slug.len() {
            result.basename = Some(name);
            return result;
        }
        // Consume only the verified directory and its one path separator.
        result.matched += 1;
    }
    result.exhausted = true;
    result
}
fn decoded_basename(slug: &str) -> Option<String> {
    if slug.len() > SLUG_BYTES {
        return None;
    }
    static CACHE: OnceLock<Mutex<DecodeCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let now = Instant::now();
    if let Some(label) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(slug, now)
    {
        return label;
    }
    let decoded = decode_with(
        slug,
        LIMITS,
        local_drive,
        |path| ordinary(path, true),
        directory_names,
        || now.elapsed() >= WALK_TIME,
    );
    debug_assert!(decoded.entries <= LIMITS.entries);
    let label = decoded.label(slug);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(slug, label.clone(), Instant::now());
    label
}
fn ordinary(path: &Path, directory: bool) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return false;
    }
    if crate::platform::metadata_is_reparse(&meta) {
        return false;
    }
    if directory {
        meta.is_dir()
    } else {
        meta.is_file()
    }
}
pub(super) fn scan(root: &Path, sid: &str) -> Option<MemoryLabel> {
    scan_with(root, sid, decoded_basename)
}
fn scan_with(
    root: &Path,
    sid: &str,
    mut decode: impl FnMut(&str) -> Option<String>,
) -> Option<MemoryLabel> {
    if sid.is_empty()
        || sid.len() > 128
        || !sid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return None;
    }
    // Reject aliases anywhere in the configured root; no recursive traversal.
    if !local_scan_root(root) {
        return None;
    }
    // Check prefixes from drive to leaf; checking the leaf first could traverse
    // an already-present parent junction before detecting that junction.
    let ancestors: Vec<_> = root.ancestors().collect();
    if ancestors.into_iter().rev().any(|p| !ordinary(p, true)) {
        return None;
    }
    let wanted = format!("{sid}.jsonl");
    let mut budget = 4096usize;
    for directory in fs::read_dir(root).ok()?.take(128).flatten() {
        if !ordinary(&directory.path(), true) {
            continue;
        }
        let slug = directory.file_name().to_string_lossy().into_owned();
        for entry in fs::read_dir(directory.path()).ok()?.take(budget) {
            budget = budget.saturating_sub(1);
            let Ok(file) = entry else {
                continue;
            };
            if file.file_name() == std::ffi::OsStr::new(&wanted) && ordinary(&file.path(), false) {
                return claude_project_label(sid, [(slug.as_str(), wanted.as_str())], &mut decode);
            }
        }
        if budget == 0 {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        let base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r7-filename-tests");
        fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap()
    }
    fn synthetic_decode(slug: &str, root: &Path) -> Decode {
        decode_with(
            slug,
            LIMITS,
            |_| Some(root.to_owned()),
            |candidate| {
                assert!(candidate.starts_with(root));
                ordinary(candidate, true)
            },
            |parent| {
                assert!(parent.starts_with(root));
                directory_names(parent)
            },
            || false,
        )
    }
    fn synthetic_label(slug: &str, root: &Path) -> super::super::labels::LabelView {
        claude_project_label("fixture", [(slug, "fixture.jsonl")], |value| {
            synthetic_decode(value, root).label(value)
        })
        .unwrap()
        .view()
    }
    #[test]
    fn r7_slug_decode_preserves_existing_hyphenated_parent_and_repo_basename() {
        let temp = fixture();
        let base = temp.path().join("synthetic-drive");
        fs::create_dir_all(base.join("parent-with-hyphens/quota-baton-impl")).unwrap();
        let slug = "C--parent-with-hyphens-quota-baton-impl";
        let decoded = synthetic_decode(slug, &base);
        assert!(!decoded.exhausted);
        assert!(!decoded.ambiguous);
        assert!(decoded.entries <= LIMITS.entries);
        assert_eq!(decoded.label(slug).as_deref(), Some("quota-baton-impl"));
        let projects = temp.path().join("projects");
        fs::create_dir_all(projects.join(slug)).unwrap();
        fs::write(projects.join(slug).join("fixture.jsonl"), [0xff, 0xfe]).unwrap();
        let label = scan_with(&projects, "fixture", |value| {
            synthetic_decode(value, &base).label(value)
        })
        .unwrap()
        .view();
        assert_eq!(label.text, "quota-baton-impl");
        assert!(label.best_effort);
        let wire = serde_json::to_string(&label).unwrap();
        assert!(!wire.contains("parent-with-hyphens"));
        assert!(!wire.contains("synthetic-drive"));
        assert!(!wire.contains("C--"));
    }
    #[test]
    fn r16_directory_names_decode_punctuation_spaces_and_mixed_case() {
        let temp = fixture();
        let parent = temp.path().join("_projects/SyntheticOwner");
        for (name, encoded) in [
            ("sample-proj-a", "sample-proj-a"),
            ("under_score", "under-score"),
            ("with.dot", "with-dot"),
            ("two words", "two-words"),
            ("MiXeD-Name", "mixed-name"),
        ] {
            fs::create_dir_all(parent.join(name)).unwrap();
            let slug = format!("c---PROJECTS-syntheticowner-{encoded}");
            let label = synthetic_label(&slug, temp.path());
            assert_eq!(label.text, name);
            assert!(label.best_effort);
        }
    }
    #[test]
    fn r16_full_literal_name_wins_but_colliding_encoded_prefixes_are_not_guessed() {
        let temp = fixture();
        let base = temp.path();
        fs::create_dir_all(base.join("parent/same_name")).unwrap();
        fs::create_dir_all(base.join("parent/same.name")).unwrap();
        let slug = "C--parent-same-name";
        let decoded = synthetic_decode(slug, base);
        assert!(decoded.ambiguous);
        assert_eq!(decoded.label(slug).as_deref(), Some("same-name"));
        fs::create_dir_all(base.join("parent/same-name/child")).unwrap();
        assert_eq!(synthetic_label(slug, base).text, "same-name");
        let slug = "C--parent-same-name-child";
        let decoded = synthetic_decode(slug, base);
        assert!(decoded.ambiguous);
        assert_eq!(decoded.label(slug).as_deref(), Some("same-name-child"));
        fs::create_dir_all(base.join("parent/x-y")).unwrap();
        fs::create_dir_all(base.join("parent/x/y")).unwrap();
        assert_eq!(synthetic_label("C--parent-x-y", base).text, "x-y");
    }
    #[test]
    fn r16_missing_path_keeps_the_entire_unresolved_suffix() {
        let temp = fixture();
        fs::create_dir_all(temp.path().join("parent-with-hyphens")).unwrap();
        let slug = "C--parent-with-hyphens-missing-project-name";
        let label = synthetic_label(slug, temp.path());
        assert_eq!(label.text, "missing-project-name");
        assert!(label.best_effort);
        let slug = "C--unknown-parent-project-name";
        assert_eq!(
            synthetic_label(slug, temp.path()).text,
            "unknown-parent-project-name"
        );
        assert_eq!(
            synthetic_label(slug, &temp.path().join("missing-drive")).text,
            slug
        );
        fs::create_dir_all(temp.path().join("short")).unwrap();
        assert_eq!(
            synthetic_label("C--shorter-project-name", temp.path()).text,
            "shorter-project-name"
        );
    }
    #[test]
    fn r16_slug_walk_caps_total_entries_each_level_depth_and_time() {
        let temp = fixture();
        let root = temp.path();
        let slug = "C--a-b-c-d";
        let decoded = decode_with(
            slug,
            Limits {
                entries: 6,
                ..LIMITS
            },
            |_| Some(root.to_owned()),
            |_| true,
            |_| Ok(std::iter::repeat_with(|| Ok(OsString::from("a")))),
            || false,
        );
        assert_eq!(decoded.entries, 6);
        assert!(decoded.exhausted);
        assert_eq!(decoded.label(slug).as_deref(), Some("a-b-c-d"));
        let decoded = decode_with(
            slug,
            Limits {
                per_level: 2,
                ..LIMITS
            },
            |_| Some(root.to_owned()),
            |_| true,
            |_| {
                Ok(["a", "b", "c"]
                    .into_iter()
                    .map(|name| Ok(OsString::from(name))))
            },
            || false,
        );
        assert_eq!(decoded.entries, 2);
        assert!(decoded.exhausted);
        let slug = "C--a-a-a-a";
        let decoded = decode_with(
            slug,
            Limits { depth: 2, ..LIMITS },
            |_| Some(root.to_owned()),
            |_| true,
            |_| Ok(std::iter::once(Ok(OsString::from("a")))),
            || false,
        );
        assert!(decoded.exhausted);
        assert_eq!(decoded.label(slug).as_deref(), Some("a-a"));
        let decoded = decode_with(
            "C--a",
            LIMITS,
            |_| panic!("expired walk must not select a drive"),
            |_| panic!("expired walk must not read metadata"),
            |_| -> io::Result<std::iter::Empty<io::Result<OsString>>> {
                panic!("expired walk must not enumerate")
            },
            || true,
        );
        assert!(decoded.exhausted);
        assert_eq!(decoded.entries, 0);
        let stop = std::cell::Cell::new(false);
        let slug = "C--parent-leaf-name";
        let decoded = decode_with(
            slug,
            LIMITS,
            |_| Some(root.to_owned()),
            |_| true,
            |path| {
                if path != root {
                    stop.set(true);
                }
                Ok(std::iter::once(Ok(OsString::from("parent"))))
            },
            || stop.get(),
        );
        assert!(decoded.exhausted);
        assert_eq!(decoded.label(slug).as_deref(), Some("leaf-name"));
        // A partial listing cannot prove a unique full match.
        let decoded = decode_with(
            "C--a",
            LIMITS,
            |_| Some(root.to_owned()),
            |_| true,
            |_| {
                Ok([
                    Ok(OsString::from("a")),
                    Err(io::ErrorKind::PermissionDenied.into()),
                ]
                .into_iter())
            },
            || false,
        );
        assert!(decoded.basename.is_none());
        assert_eq!(decoded.label("C--a").as_deref(), Some("a"));
    }
    #[test]
    fn r7_slug_decode_rejects_remote_roots_unsafe_components_and_detected_reparse_prefixes() {
        let temp = fixture();
        let base = temp.path();
        for slug in [
            "--server-share-repo",
            "C--../escape",
            "C--parent\\escape",
            "C--parent:escape",
        ] {
            let decoded = decode_with(
                slug,
                LIMITS,
                |_| panic!("invalid slug must not select a drive"),
                |_| panic!("invalid slug must not read metadata"),
                |_| -> io::Result<std::iter::Empty<io::Result<OsString>>> {
                    panic!("invalid slug must not enumerate")
                },
                || false,
            );
            assert_eq!(decoded.entries, 0);
            assert!(decoded.basename.is_none());
        }
        let decoded = decode_with(
            "Z--remote-repo",
            LIMITS,
            |_| None,
            |_| panic!("remote drive must not read metadata"),
            |_| -> io::Result<std::iter::Empty<io::Result<OsString>>> {
                panic!("remote drive must not enumerate")
            },
            || false,
        );
        assert_eq!(decoded.entries, 0);
        let blocked = base.join("blocked");
        let decoded = decode_with(
            "C--blocked-child",
            LIMITS,
            |_| Some(base.to_owned()),
            |path| path == base,
            |path| {
                assert!(!path.starts_with(&blocked));
                Ok(std::iter::once(Ok(OsString::from("blocked"))))
            },
            || false,
        );
        assert!(decoded.basename.is_none());
        assert!(!local_scan_root(Path::new(r"\\server\share\projects")));
    }
    #[test]
    fn r16_slug_cache_is_bounded_expires_and_keeps_negative_results() {
        let mut cache = DecodeCache::default();
        let now = Instant::now();
        cache.insert("C--missing", None, now);
        assert_eq!(cache.get("C--missing", now), Some(None));
        cache.insert("C--repo", Some("repo-name".into()), now);
        assert_eq!(cache.get("C--repo", now), Some(Some("repo-name".into())));
        assert!(cache.get("C--repo", now + CACHE_TIME).is_none());
        for i in 0..CACHE_ENTRIES + 1 {
            cache.insert(&format!("C--repo-{i}"), Some("repo".into()), now);
        }
        assert_eq!(cache.0.len(), CACHE_ENTRIES);
        assert!(cache.get("C--repo-0", now).is_none());
        assert!(
            cache
                .get(&format!("C--repo-{CACHE_ENTRIES}"), now)
                .is_some()
        );
    }
    #[test]
    fn synthetic_filename_lookup_ignores_contents_and_rejects_traversal() {
        let temp = fixture();
        let root = temp.path().join("projects");
        fs::create_dir_all(root.join("fixture-private-folder")).unwrap();
        // Invalid transcript bytes: opening/parsing content is neither needed nor allowed.
        fs::write(
            root.join("fixture-private-folder/fixture.jsonl"),
            [0xff, 0xfe],
        )
        .unwrap();
        assert_eq!(
            scan(&root, "fixture").unwrap().view().text,
            "fixture-private-folder"
        );
        assert!(scan(&root, "../fixture").is_none());
        assert!(scan(&root, "missing").is_none());
    }
}

//! Filename-only production lookup, invoked by the existing writer, never ingress.
use super::labels::{MemoryLabel, claude_project_label};
use std::{
    fs,
    path::{Path, PathBuf},
};

const SLUG_BYTES: usize = 1024;
const SLUG_PARTS: usize = 64;
const ROOT_DEPTH: usize = 32;
#[derive(Clone, Copy)]
struct Limits {
    queries: usize,
    depth: usize,
    pending: usize,
}
const LIMITS: Limits = Limits {
    queries: 256,
    depth: 16,
    pending: 64,
};
struct Decode {
    basename: Option<String>,
    queries: usize,
    exhausted: bool,
    ambiguous: bool,
}
impl Decode {
    fn resolved(self) -> Option<String> {
        if self.exhausted || self.ambiguous {
            None
        } else {
            self.basename
        }
    }
}

// Only fixed local disks/RAM disks qualify. Never issue path metadata against
// UNC, a mapped network drive, unknown media, or a reparse prefix.
#[cfg(windows)]
fn local_drive(letter: u8) -> Option<PathBuf> {
    use windows::{Win32::Storage::FileSystem::GetDriveTypeW, core::PCWSTR};
    let drive = [
        u16::from(letter.to_ascii_uppercase()),
        u16::from(b':'),
        u16::from(b'\\'),
        0,
    ];
    let kind = unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) };
    matches!(kind, 3 | 6)
        .then(|| PathBuf::from(format!("{}:\\", char::from(letter.to_ascii_uppercase()))))
}
#[cfg(not(windows))]
fn local_drive(_letter: u8) -> Option<PathBuf> {
    None
}

fn local_scan_root(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut parts = path.components();
        let Some(Component::Prefix(prefix)) = parts.next() else {
            return false;
        };
        let letter = match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            _ => return false,
        };
        if !matches!(parts.next(), Some(Component::RootDir)) {
            return false;
        }
        if path.as_os_str().len() > SLUG_BYTES
            || path.components().count() > ROOT_DEPTH
            || parts.any(|part| !matches!(part, Component::Normal(_)))
        {
            return false;
        }
        local_drive(letter).is_some()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// Partition ambiguous '-' separators, pruning solely on directory metadata.
/// Injecting both drive root and metadata predicate keeps tests inside their
/// own synthetic directory. No directory enumeration or content read occurs.
fn decode_with(
    slug: &str,
    limits: Limits,
    root_for_drive: impl FnOnce(u8) -> Option<PathBuf>,
    mut directory: impl FnMut(&Path) -> bool,
) -> Decode {
    let mut result = Decode {
        basename: None,
        queries: 0,
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
    let pieces: Vec<_> = tail.split('-').take(SLUG_PARTS + 1).collect();
    if pieces.len() > SLUG_PARTS {
        result.exhausted = true;
        return result;
    }
    let Some(root) = root_for_drive(bytes[0]) else {
        return result;
    };
    if !root.is_absolute() {
        return result;
    }
    if limits.queries == 0 || limits.pending == 0 {
        result.exhausted = true;
        return result;
    }
    result.queries += 1;
    if !directory(&root) {
        return result;
    }
    let mut pending = vec![(root, 0usize, 0usize)];
    let mut solutions = 0usize;
    while let Some((parent, start, depth)) = pending.pop() {
        if depth >= limits.depth {
            result.exhausted = true;
            continue;
        }
        for end in (start + 1..=pieces.len()).rev() {
            let name = pieces[start..end].join("-");
            if name.is_empty()
                || matches!(name.as_str(), "." | "..")
                || name.encode_utf16().count() > 255
            {
                continue;
            }
            if result.queries >= limits.queries {
                result.exhausted = true;
                return result;
            }
            let candidate = parent.join(&name);
            result.queries += 1;
            // Every prefix must itself be ordinary before any child is tested.
            if !directory(&candidate) {
                continue;
            }
            if end == pieces.len() {
                solutions += 1;
                result.basename = Some(name);
                if solutions > 1 {
                    result.ambiguous = true;
                    result.basename = None;
                    return result;
                }
            } else {
                if pending.len() >= limits.pending {
                    result.exhausted = true;
                    return result;
                }
                pending.push((candidate, end, depth + 1));
            }
        }
    }
    result
}
fn decoded_basename(slug: &str) -> Option<String> {
    let decoded = decode_with(slug, LIMITS, local_drive, |path| ordinary(path, true));
    debug_assert!(decoded.queries <= LIMITS.queries);
    decoded.resolved()
}
fn ordinary(path: &Path, directory: bool) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        }
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
        )
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
        assert!(decoded.queries <= LIMITS.queries);
        assert_eq!(decoded.resolved().as_deref(), Some("quota-baton-impl"));
        let projects = temp.path().join("projects");
        fs::create_dir_all(projects.join(slug)).unwrap();
        fs::write(projects.join(slug).join("fixture.jsonl"), [0xff, 0xfe]).unwrap();
        let label = scan_with(&projects, "fixture", |value| {
            synthetic_decode(value, &base).resolved()
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
    fn r7_ambiguous_or_missing_slug_uses_advisory_fallback_without_claiming_reconstruction() {
        let temp = fixture();
        let base = temp.path().join("synthetic-drive");
        fs::create_dir_all(base.join("parent/x-y")).unwrap();
        fs::create_dir_all(base.join("parent/x/y")).unwrap();
        let decoded = synthetic_decode("C--parent-x-y", &base);
        assert!(decoded.ambiguous);
        assert!(decoded.resolved().is_none());
        let label = claude_project_label("fixture", [("C--parent-x-y", "fixture.jsonl")], |slug| {
            synthetic_decode(slug, &base).resolved()
        })
        .unwrap()
        .view();
        assert_eq!(label.text, "y");
        assert!(label.best_effort);
        let missing = synthetic_decode("C--parent-repo", &base.join("missing"));
        assert_eq!(missing.queries, 1);
        assert!(missing.resolved().is_none());
        let label = claude_project_label(
            "fixture",
            [("C--parent-quota-baton-impl", "fixture.jsonl")],
            |slug| synthetic_decode(slug, &base.join("missing")).resolved(),
        )
        .unwrap()
        .view();
        assert_eq!(label.text, "impl");
        assert!(label.best_effort);
    }
    #[test]
    fn r7_slug_decode_caps_queries_depth_and_candidate_states() {
        let temp = fixture();
        let base = temp.path();
        let mut queries = 0;
        let decoded = decode_with(
            "C--a-b-c-d-e",
            Limits {
                queries: 6,
                ..LIMITS
            },
            |_| Some(base.to_owned()),
            |path| {
                assert!(path.starts_with(base));
                queries += 1;
                true
            },
        );
        assert_eq!(queries, 6);
        assert_eq!(decoded.queries, 6);
        assert!(decoded.exhausted);
        assert!(decoded.resolved().is_none());
        let decoded = decode_with(
            "C--a-b-c-d",
            Limits { depth: 2, ..LIMITS },
            |_| Some(base.to_owned()),
            |path| path == base || !path.file_name().unwrap().to_string_lossy().contains('-'),
        );
        assert!(decoded.exhausted);
        assert!(decoded.resolved().is_none());
        let decoded = decode_with(
            "C--a-b-c-d",
            Limits {
                pending: 1,
                ..LIMITS
            },
            |_| Some(base.to_owned()),
            |_| true,
        );
        assert!(decoded.exhausted);
        assert!(decoded.queries <= LIMITS.queries);
        assert!(decoded.resolved().is_none());
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
            );
            assert_eq!(decoded.queries, 0);
            assert!(decoded.resolved().is_none());
        }
        let decoded = decode_with(
            "Z--remote-repo",
            LIMITS,
            |_| None,
            |_| panic!("remote drive must not read metadata"),
        );
        assert_eq!(decoded.queries, 0);
        assert!(decoded.resolved().is_none());
        let blocked = base.join("blocked");
        let decoded = decode_with(
            "C--blocked-child",
            LIMITS,
            |_| Some(base.to_owned()),
            |path| {
                assert!(!path.starts_with(blocked.join("child")));
                path == base
            },
        );
        assert!(decoded.resolved().is_none());
        assert!(!local_scan_root(Path::new(r"\\server\share\projects")));
    }
    #[test]
    fn synthetic_filename_lookup_ignores_contents_and_rejects_traversal() {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r7-filename-tests");
        fs::create_dir_all(&base).unwrap();
        let temp = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let root = temp.path().join("projects");
        fs::create_dir_all(root.join("fixture-private-folder")).unwrap();
        // Invalid transcript bytes: opening/parsing content is neither needed nor allowed.
        fs::write(
            root.join("fixture-private-folder/fixture.jsonl"),
            [0xff, 0xfe],
        )
        .unwrap();
        assert_eq!(scan(&root, "fixture").unwrap().view().text, "folder");
        assert!(scan(&root, "../fixture").is_none());
        assert!(scan(&root, "missing").is_none());
    }
}

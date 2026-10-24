//! Static guard for the CLI's common/platform boundary.
use std::{fs, path::Path};

fn inspect(directory: &Path, root: &Path, windows_roots: &[String]) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "platform" || relative == "native_resume" {
            continue;
        }
        if path.is_dir() {
            inspect(&path, root, windows_roots);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            // Session descendants and their source-level tests belong to the
            // Windows-only sessions feature. Remaining roots come from the
            // actual Windows module declarations, not an independent allowlist.
            if windows_roots.contains(&relative)
                || relative.starts_with("sessions_")
                || matches!(
                    relative.as_str(),
                    "dedicated.rs" | "account_alias.rs" | "image_home.rs"
                )
            {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            let win_path = ["windows", "::"].concat();
            assert!(
                !compact.contains(&win_path),
                "platform API leaked into {relative}"
            );
            for (marker, macro_call) in [
                (["#[", "cfg"].concat(), false),
                (["cfg", "!("].concat(), true),
            ] {
                for tail in compact.split(marker.as_str()).skip(1) {
                    let expression = tail.split(']').next().unwrap();
                    let expression = if macro_call {
                        expression.split(')').next().unwrap()
                    } else {
                        expression
                    };
                    assert!(
                        !["windows", "unix", "target_os"]
                            .iter()
                            .any(|os| expression.contains(os)),
                        "OS selection leaked into {relative}"
                    );
                }
            }
        }
    }
}

#[test]
fn cli_os_selection_stays_at_the_platform_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let registry = fs::read_to_string(root.join("platform/windows/mod.rs")).unwrap();
    let windows_roots = registry
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("#[path = \"../../")
                .and_then(|path| path.strip_suffix("\"]"))
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert!(!windows_roots.is_empty());
    inspect(&root, &root, &windows_roots);
}

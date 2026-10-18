//! Process-local launcher label. No settings file or argument contains the label.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::path::Path;

const NAME: &str = "x-quota-baton-label";
const MAX_HEADERS: usize = 64 * 1024;

/// Encoding is deliberately ASCII: Node's header APIs reject raw Hangul/emoji.
pub fn folder_value(cwd: &Path) -> Option<String> {
    let folder: String = cwd
        .file_name()?
        .to_str()?
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    if folder.trim().is_empty() || folder == "." || folder == ".." {
        return None;
    }
    Some(format!("qb1:{}", URL_SAFE_NO_PAD.encode(folder.as_bytes())))
}

/// Keep unrelated lines byte-for-byte; replace only the header addressed to us.
/// Oversized inherited state stays the client's responsibility: omit our label
/// rather than truncating or logging somebody else's headers.
pub fn merge_headers(existing: &str, value: &str) -> Option<String> {
    if existing.len() > MAX_HEADERS || !value.is_ascii() || value.contains(['\r', '\n']) {
        return None;
    }
    let mut lines: Vec<_> = existing
        .split('\n')
        .filter(|line| {
            !line
                .split_once(':')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case(NAME))
        })
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut merged = lines.join("\n");
    if !merged.is_empty() {
        merged.push('\n');
    }
    merged.push_str(NAME);
    merged.push_str(": ");
    merged.push_str(value);
    (merged.len() <= MAX_HEADERS).then_some(merged)
}

/// The generated batch launcher delegates only its final child spawn here.
/// Keep the dedicated settings file unchanged and out of per-launch scratch.
/// Client settings can override this environment; the core's filename lookup
/// remains the fallback when no launcher header reaches it.
pub fn run(program: &Path, settings: &Path, args: &[std::ffi::OsString]) -> std::io::Result<i32> {
    if !program.is_absolute() || !settings.is_absolute() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut child = std::process::Command::new(program);
    child.arg("--settings").arg(settings).args(args);
    let headers = std::env::var_os("ANTHROPIC_CUSTOM_HEADERS");
    if let Some(value) = folder_value(&std::env::current_dir()?) {
        let existing = headers
            .as_deref()
            .map(std::ffi::OsStr::to_str)
            .unwrap_or(Some(""));
        if let Some(merged) = existing.and_then(|existing| merge_headers(existing, &value)) {
            child.env("ANTHROPIC_CUSTOM_HEADERS", merged);
        }
    }
    // Rust applies its batch-specific escaping for .cmd/.bat. Do not change
    // this into an interpolated `cmd /c` string or CALL reparse.
    Ok(child.status()?.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_basename_is_ascii_and_round_trips_without_path() {
        let value = folder_value(Path::new(r"C:\synthetic\한글 & 테스트 ! ^ % 🐬")).unwrap();
        assert!(value.is_ascii());
        let decoded = URL_SAFE_NO_PAD
            .decode(value.strip_prefix("qb1:").unwrap())
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "한글 & 테스트 ! ^ % 🐬"
        );
        assert!(folder_value(Path::new(r"C:\")).is_none());
    }
    #[test]
    fn preserves_colons_newlines_and_unrelated_values_replaces_owned_duplicates_only() {
        let original = "X-Other: alpha:beta\r\nX-Quota-Baton-Label: old\r\nX-Aux: retain\nx-quota-baton-label: older";
        assert_eq!(
            merge_headers(original, "qb1:bmV3").unwrap(),
            "X-Other: alpha:beta\r\nX-Aux: retain\nx-quota-baton-label: qb1:bmV3"
        );
        assert_eq!(
            merge_headers("", "qb1:YQ").unwrap(),
            "x-quota-baton-label: qb1:YQ"
        );
        assert!(merge_headers("preserve", "bad\nInjected: yes").is_none());
        assert!(merge_headers(&"a".repeat(MAX_HEADERS + 1), "qb1:YQ").is_none());
    }
}
